//! 文本剪贴板读写（Win32），供被控端与主控端共用。
//!
//! 同步策略是「轮询序列号」：`GetClipboardSequenceNumber` 变了才去读文本，
//! 自己刚写进去的内容会记在 `LAST_WRITE` 里，轮询时跳过，避免两端互相回传。

use std::os::raw::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const GMEM_MOVEABLE: u32 = 0x0002;
/// 超过这个长度的文本（字符数）不同步，避免把大段内容塞进控制通道。
pub const MAX_TEXT_CHARS: usize = 64 * 1024;
/// 单个图片的最大字节数（PNG 编码后）。超过则不同步，避免撑爆控制通道。
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// 从剪贴板读到的原始 DIB 上限（32 MiB）。超过直接放弃，防止异常大图把内存吃光。
const MAX_RAW_DIB_BYTES: usize = 32 * 1024 * 1024;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetClipboardSequenceNumber() -> u32;
    fn OpenClipboard(hwnd: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> *mut c_void;
    fn SetClipboardData(format: u32, data: *mut c_void) -> *mut c_void;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(handle: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(handle: *mut c_void) -> i32;
    fn GlobalFree(handle: *mut c_void) -> *mut c_void;
    fn GlobalSize(handle: *mut c_void) -> usize;
}

static LAST_WRITE: std::sync::Mutex<Option<(u32, String)>> = std::sync::Mutex::new(None);
/// 自己最近一次写进剪贴板的图片序列号，供轮询跳过回传。
/// 图片单独记录，避免和文本的 `LAST_WRITE` 互相影响；0 表示本次进程还没写过图片。
static LAST_IMAGE_SEQ: AtomicU32 = AtomicU32::new(0);

/// 当前剪贴板序列号；没有变化就不必读内容。
pub fn sequence() -> u32 {
    // SAFETY: 该 API 无参数、无指针，只返回当前剪贴板序列号。
    unsafe { GetClipboardSequenceNumber() }
}

/// 读剪贴板文本；非文本、空文本或超长文本返回 `None`。
pub fn read_text() -> Option<String> {
    // SAFETY: 只调用 Win32 剪贴板 API；GetClipboardData 返回的句柄归剪贴板所有，
    // 本函数不释放它，GlobalLock 成功后配对 GlobalUnlock，CloseClipboard 与 OpenClipboard 成对。
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return None;
        }
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT);
        let text = if handle.is_null() {
            None
        } else {
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                None
            } else {
                let len = GlobalSize(handle) / 2;
                let slice = std::slice::from_raw_parts(ptr, len);
                let end = slice.iter().position(|value| *value == 0).unwrap_or(len);
                let value = String::from_utf16_lossy(&slice[..end]);
                GlobalUnlock(handle);
                Some(value)
            }
        };
        CloseClipboard();
        text.filter(|value| text_allowed(value))
    }
}

/// 写剪贴板文本；成功后记下序列号，避免轮询把它当成「新变化」回传。
pub fn write_text(text: &str) -> bool {
    if !text_allowed(text) {
        return false;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    // SAFETY: handle 在交给 SetClipboardData 前由本函数负责，失败路径都会 GlobalFree；
    // GlobalLock/GlobalUnlock 成对出现，OpenClipboard/CloseClipboard 也成对出现。
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if handle.is_null() {
            return false;
        }
        let ptr = GlobalLock(handle) as *mut u16;
        if ptr.is_null() {
            GlobalFree(handle);
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        GlobalUnlock(handle);

        if OpenClipboard(std::ptr::null_mut()) == 0 {
            GlobalFree(handle);
            return false;
        }
        EmptyClipboard();
        let stored = SetClipboardData(CF_UNICODETEXT, handle);
        CloseClipboard();
        if stored.is_null() {
            GlobalFree(handle);
            return false;
        }
    }
    let seq = sequence();
    if let Ok(mut last) = LAST_WRITE.lock() {
        *last = Some((seq, text.to_string()));
    }
    true
}

/// 轮询辅助：序列号变化且不是自己刚写入的内容时返回新文本。
pub fn poll_text(last_seq: &mut u32) -> Option<String> {
    let seq = sequence();
    let last_write = LAST_WRITE.lock().ok().and_then(|last| last.clone());
    if !should_emit(*last_seq, seq, last_write.as_ref()) {
        if seq != *last_seq {
            *last_seq = seq;
        }
        return None;
    }
    *last_seq = seq;
    read_text()
}

/// 轮询判定：序列号变了，并且这次变化不是自己刚写进去的内容。
fn should_emit(last_seq: u32, seq: u32, last_write: Option<&(u32, String)>) -> bool {
    if seq == last_seq {
        return false;
    }
    last_write.map(|(written, _)| *written) != Some(seq)
}

/// 需要同步的文本：非空且不超过长度上限。
fn text_allowed(text: &str) -> bool {
    !text.is_empty() && text.chars().count() <= MAX_TEXT_CHARS
}

// ---------------------------------------------------------------------------
// 统一轮询入口
// ---------------------------------------------------------------------------

/// 轮询到的一次剪贴板变化。
#[derive(Debug, Clone, PartialEq)]
pub enum Clip {
    Text(String),
    /// PNG 编码后的图片字节。
    Image(Vec<u8>),
}

/// 序列号变化且不是自己刚写入的内容时返回这次变化。
///
/// 优先返回文本：很多应用（Excel、Word）复制时同时往剪贴板放位图，
/// 如果优先返回图片，用户复制表格会被同步成一张图而不是可编辑文本。
/// 只有在没有文本时才返回图片。
pub fn poll(last_seq: &mut u32) -> Option<Clip> {
    let seq = sequence();
    let text_seq = LAST_WRITE
        .lock()
        .ok()
        .and_then(|last| last.as_ref().map(|(written, _)| *written));
    // 0 是「本进程还没写过图片」的哨兵，不能当成真实序列号参与比较。
    let written_image_seq = image_sequence();
    let own_image_seq = (written_image_seq != 0).then_some(written_image_seq);
    if !should_emit_clip(*last_seq, seq, text_seq, own_image_seq) {
        // 自己刚写入引起的序列号变化也要吞掉，否则下次轮询会再读一遍旧内容。
        if seq != *last_seq {
            *last_seq = seq;
        }
        return None;
    }
    *last_seq = seq;
    if let Some(text) = read_text() {
        return Some(Clip::Text(text));
    }
    if let Some(png) = read_image_png() {
        return Some(Clip::Image(png));
    }
    None
}

/// 是否需要为这次序列号变化产出内容：变了，且不是自己刚写入的文本或图片。
fn should_emit_clip(
    last_seq: u32,
    seq: u32,
    text_seq: Option<u32>,
    image_seq: Option<u32>,
) -> bool {
    seq != last_seq && text_seq != Some(seq) && image_seq != Some(seq)
}

// ---------------------------------------------------------------------------
// 图片剪贴板（CF_DIB <-> PNG）
// ---------------------------------------------------------------------------

/// 读剪贴板里的位图，编码成 PNG 返回；没有位图、格式不支持或超过
/// [`MAX_IMAGE_BYTES`] 时返回 `None`。
pub fn read_image_png() -> Option<Vec<u8>> {
    // SAFETY: 这里只调用 Win32 剪贴板 API。GetClipboardData 返回的句柄归剪贴板所有，
    // 本函数不释放它；GlobalLock 成功后一定在同一分支里配对 GlobalUnlock。
    unsafe {
        if IsClipboardFormatAvailable(CF_DIB) == 0 {
            return None;
        }
        if !open_clipboard_retry() {
            return None;
        }
        let png = {
            let handle = GetClipboardData(CF_DIB);
            if handle.is_null() {
                None
            } else {
                let size = GlobalSize(handle);
                if !(40..=MAX_RAW_DIB_BYTES).contains(&size) {
                    // 空数据或异常大图都不值得同步，宁可放弃。
                    None
                } else {
                    // SAFETY: handle 非空，长度已由 GlobalSize 限制在 32 MiB 内；
                    // 锁只持续到 dib_to_png 返回，期间没有别的线程能改动这块内存。
                    let ptr = GlobalLock(handle) as *const u8;
                    if ptr.is_null() {
                        None
                    } else {
                        let dib = std::slice::from_raw_parts(ptr, size);
                        let png = dib_to_png(dib);
                        // SAFETY: 与上面的 GlobalLock 成对调用，之后不再使用 ptr。
                        GlobalUnlock(handle);
                        png
                    }
                }
            }
        };
        // SAFETY: 与 open_clipboard_retry 成功时打开的剪贴板成对关闭。
        CloseClipboard();
        png
    }
}

/// 把 PNG 字节写入剪贴板（转成 CF_DIB）。成功返回 `true`。
pub fn write_image_png(png_bytes: &[u8]) -> bool {
    let Some(dib) = png_to_dib(png_bytes) else {
        return false;
    };
    // SAFETY: GlobalAlloc 得到的 handle 在交给 SetClipboardData 之前由本函数负责：
    // 任何提前返回的路径都会 GlobalFree；SetClipboardData 成功后所有权移交给剪贴板。
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, dib.len());
        if handle.is_null() {
            return false;
        }
        // SAFETY: handle 刚分配且未交给剪贴板，锁定/写入/解锁都在这段代码内完成。
        let ptr = GlobalLock(handle) as *mut u8;
        if ptr.is_null() {
            GlobalFree(handle);
            return false;
        }
        std::ptr::copy_nonoverlapping(dib.as_ptr(), ptr, dib.len());
        // SAFETY: 与上面的 GlobalLock 成对调用；拷贝已完成，ptr 不再使用。
        GlobalUnlock(handle);

        if !open_clipboard_retry() {
            GlobalFree(handle);
            return false;
        }
        // EmptyClipboard 会连文本一起清掉，这是 Windows 剪贴板的设计，可以接受。
        EmptyClipboard();
        // SAFETY: handle 是 GMEM_MOVEABLE 全局内存，SetClipboardData(CF_DIB) 要求这种句柄。
        let stored = SetClipboardData(CF_DIB, handle);
        // SAFETY: 与 open_clipboard_retry 打开的剪贴板成对关闭。
        CloseClipboard();
        if stored.is_null() {
            // 失败时所有权仍在本地，必须自己释放，否则泄漏。
            GlobalFree(handle);
            return false;
        }
    }
    // 记下序列号，让 poll 跳过自己刚写入的图片，避免两端来回同步。
    LAST_IMAGE_SEQ.store(sequence(), Ordering::Relaxed);
    true
}

/// 当前剪贴板里是否有可用的文本或位图。
pub fn has_image() -> bool {
    // SAFETY: IsClipboardFormatAvailable 只查询格式，不解引用任何指针。
    unsafe {
        IsClipboardFormatAvailable(CF_DIB) != 0 || IsClipboardFormatAvailable(CF_UNICODETEXT) != 0
    }
}

/// 自己刚写进剪贴板的图片序列号，供轮询跳过回传（未写过图片时为 0）。
pub fn image_sequence() -> u32 {
    LAST_IMAGE_SEQ.load(Ordering::Relaxed)
}

/// `OpenClipboard` 可能被别的进程占用；重试 3 次、每次间隔 30ms 再放弃。
fn open_clipboard_retry() -> bool {
    for attempt in 0..3 {
        // SAFETY: 传入空 HWND 表示把剪贴板关联到当前任务，是允许的用法。
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return true;
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    false
}

/// PNG -> CF_DIB（32bpp、bottom-up）。解码成 RGBA8，RGB 源补 alpha=255。
pub(crate) fn png_to_dib(png_bytes: &[u8]) -> Option<Vec<u8>> {
    if png_bytes.is_empty() || png_bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    // EXPAND 展开调色板/低位深与 tRNS，STRIP_16 统一成 8 位，ALPHA 让透明色也变成真实 alpha。
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = {
        let info = reader.info();
        (info.width as usize, info.height as usize)
    };
    if width == 0 || height == 0 {
        return None;
    }
    let rgba_len = width.checked_mul(height)?.checked_mul(4)?;
    if rgba_len > MAX_RAW_DIB_BYTES {
        return None;
    }
    let buffer_len = reader.output_buffer_size()?;
    let mut frame = vec![0u8; buffer_len];
    let info = reader.next_frame(&mut frame).ok()?;
    let rgba = frame_to_rgba(&frame, &info)?;
    if rgba.len() != rgba_len {
        return None;
    }

    // 32bpp 每行天然 4 字节对齐。
    let stride = width.checked_mul(4)?;
    let pixel_bytes = stride.checked_mul(height)?;
    let mut dib = vec![0u8; 40usize.checked_add(pixel_bytes)?];
    store_u32_le(&mut dib, 0, 40); // biSize = BITMAPINFOHEADER
    store_i32_le(&mut dib, 4, width as i32); // biWidth

    // 正高度 = bottom-up；这是 Windows 程序解析 CF_DIB 时最常见的行序。
    store_i32_le(&mut dib, 8, height as i32);
    store_u16_le(&mut dib, 12, 1); // biPlanes
    store_u16_le(&mut dib, 14, 32); // biBitCount
    store_u32_le(&mut dib, 16, 0); // biCompression = BI_RGB
    store_u32_le(&mut dib, 20, pixel_bytes as u32); // biSizeImage

    // 其余字段（DPI、调色板数量）留 0，让接收方按默认值处理。

    // bottom-up：DIB 第 0 行是图片最底下一行，所以要倒着写。
    for (source_row, source) in rgba.chunks_exact(stride).enumerate() {
        let target_row = height - 1 - source_row;
        let start = 40 + target_row * stride;
        let target = &mut dib[start..start + stride];
        for (pixel, output) in source
            .as_chunks::<4>()
            .0
            .iter()
            .zip(target.as_chunks_mut::<4>().0.iter_mut())
        {
            output[0] = pixel[2]; // B
            output[1] = pixel[1]; // G
            output[2] = pixel[0]; // R
            output[3] = pixel[3]; // A
        }
    }
    Some(dib)
}

/// CF_DIB -> PNG。支持 24bpp（无 alpha）和 32bpp（BGRA），正负高度都行。
pub(crate) fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let bi_size = read_u32_le(dib, 0)? as usize;
    // biSize 小于 40 不是合法的 BITMAPINFOHEADER；同时防止偏移指到缓冲区外。
    if bi_size < 40 || bi_size > dib.len() {
        return None;
    }
    let width = read_i32_le(dib, 4)?;
    let height = read_i32_le(dib, 8)?;
    let bit_count = read_u16_le(dib, 14)?;
    let compression = read_u32_le(dib, 16)?;
    // 只认未压缩的 24/32bpp。BI_BITFIELDS 等格式的掩码各不相同，硬解会得到错色，
    // 宁可不同步也不要把花屏送给对端。
    if compression != 0 || (bit_count != 24 && bit_count != 32) {
        return None;
    }
    if width <= 0 || height == 0 {
        return None;
    }
    let width = width as usize;
    let rows = height.unsigned_abs() as usize;
    let channels = bit_count as usize / 8;
    let row_bytes = width.checked_mul(channels)?;
    let stride = row_bytes.checked_add(3)? / 4 * 4;
    let pixel_bytes = stride.checked_mul(rows)?;
    if pixel_bytes > MAX_RAW_DIB_BYTES {
        return None;
    }
    let end = bi_size.checked_add(pixel_bytes)?;
    if end > dib.len() {
        return None;
    }

    let color = if channels == 4 {
        png::ColorType::Rgba
    } else {
        png::ColorType::Rgb
    };
    let mut pixels = Vec::with_capacity(width.checked_mul(rows)?.checked_mul(channels)?);
    for row in 0..rows {
        // 正高度时第 0 行是最底下一行，需要倒着取；负高度是 top-down，直接顺序取。
        let source_row = if height > 0 { rows - 1 - row } else { row };
        let start = bi_size + source_row * stride;
        for pixel in dib[start..start + row_bytes].chunks_exact(channels) {
            pixels.push(pixel[2]); // R <- B
            pixels.push(pixel[1]); // G
            pixels.push(pixel[0]); // B <- R
            if channels == 4 {
                pixels.push(pixel[3]); // A
            }
        }
    }

    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width as u32, rows as u32);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&pixels).ok()?;
    }
    if png.len() > MAX_IMAGE_BYTES {
        None
    } else {
        Some(png)
    }
}

/// 把 png crate 解码出的 8 位帧统一展开成 RGBA8，方便后续转 BGRA。
fn frame_to_rgba(frame: &[u8], info: &png::OutputInfo) -> Option<Vec<u8>> {
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let width = info.width as usize;
    let height = info.height as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let channels = info.color_type.samples();
    let row_bytes = width.checked_mul(channels)?;
    if row_bytes > info.line_size {
        return None;
    }
    let frame_len = info.line_size.checked_mul(height)?;
    if frame.len() < frame_len {
        return None;
    }
    let rgba_len = width.checked_mul(height)?.checked_mul(4)?;
    let mut rgba = Vec::with_capacity(rgba_len);
    for row in frame[..frame_len].chunks_exact(info.line_size) {
        let row = &row[..row_bytes];
        match info.color_type {
            png::ColorType::Rgb => {
                for pixel in row.as_chunks::<3>().0 {
                    rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
                }
            }
            png::ColorType::Rgba => rgba.extend_from_slice(row),
            png::ColorType::Grayscale => {
                for &gray in row {
                    rgba.extend_from_slice(&[gray, gray, gray, 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for pixel in row.as_chunks::<2>().0 {
                    rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
                }
            }
            _ => return None,
        }
    }
    if rgba.len() == rgba_len {
        Some(rgba)
    } else {
        None
    }
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let raw: [u8; 4] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn read_i32_le(bytes: &[u8], offset: usize) -> Option<i32> {
    read_u32_le(bytes, offset).map(|value| value as i32)
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let raw: [u8; 2] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u16::from_le_bytes(raw))
}

fn store_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
    if let Some(slot) = offset
        .checked_add(4)
        .and_then(|end| bytes.get_mut(offset..end))
    {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

fn store_i32_le(bytes: &mut [u8], offset: usize, value: i32) {
    store_u32_le(bytes, offset, value as u32);
}

fn store_u16_le(bytes: &mut [u8], offset: usize, value: u16) {
    if let Some(slot) = offset
        .checked_add(2)
        .and_then(|end| bytes.get_mut(offset..end))
    {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_echo_of_own_write() {
        let own = (7u32, "x".to_string());
        assert!(!should_emit(6, 7, Some(&own)));
    }

    #[test]
    fn ignores_unchanged_sequence() {
        assert!(!should_emit(9, 9, None));
    }

    #[test]
    fn reports_external_change() {
        assert!(should_emit(9, 10, None));
        let own = (7u32, "x".to_string());
        assert!(should_emit(7, 10, Some(&own)));
    }

    #[test]
    fn filters_empty_and_oversized_text() {
        assert!(text_allowed("中文"));
        assert!(!text_allowed(""));
        let huge = "x".repeat(MAX_TEXT_CHARS + 1);
        assert!(!text_allowed(&huge));
    }

    fn encode_png(color: png::ColorType, width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, width, height);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("写 PNG 头");
            writer.write_image_data(data).expect("写 PNG 数据");
        }
        png
    }

    fn decode_png(bytes: &[u8]) -> (u32, u32, png::ColorType, Vec<u8>) {
        // 测试里保留 PNG 原本的颜色类型（RGB/RGBA），不要用 ALPHA 强制补 alpha。
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("读 PNG 头");
        let mut frame = vec![0u8; reader.output_buffer_size().expect("PNG 缓冲区大小")];
        let info = reader.next_frame(&mut frame).expect("解码 PNG");
        frame.truncate(info.buffer_size());
        (info.width, info.height, info.color_type, frame)
    }

    #[test]
    fn png_roundtrip_through_dib_keeps_rgba_pixels() {
        let source = vec![
            255, 0, 0, 255, // 红
            0, 255, 0, 128, // 半透明绿
            0, 0, 255, 64, // 低透明度蓝
            10, 20, 30, 40, // 任意值
        ];
        let png = encode_png(png::ColorType::Rgba, 2, 2, &source);
        let dib = png_to_dib(&png).expect("PNG 转 DIB");
        // 32bpp、BI_RGB、正高度（bottom-up）。
        assert_eq!(read_u16_le(&dib, 14), Some(32));
        assert_eq!(read_u32_le(&dib, 16), Some(0));
        assert_eq!(read_i32_le(&dib, 8), Some(2));
        assert_eq!(dib.len(), 40 + 2 * 2 * 4);

        let back = dib_to_png(&dib).expect("DIB 转 PNG");
        let (width, height, color, pixels) = decode_png(&back);
        assert_eq!((width, height), (2, 2));
        assert_eq!(color, png::ColorType::Rgba);
        assert_eq!(pixels, source);
    }

    #[test]
    fn rgb_png_gets_opaque_alpha() {
        let png = encode_png(png::ColorType::Rgb, 1, 1, &[12, 34, 56]);
        let dib = png_to_dib(&png).expect("PNG 转 DIB");
        let (_, _, color, pixels) = decode_png(&dib_to_png(&dib).expect("DIB 转 PNG"));
        assert_eq!(color, png::ColorType::Rgba);
        assert_eq!(pixels, vec![12, 34, 56, 255]);
    }

    #[test]
    fn bottom_up_24bpp_dib_keeps_bgr_order() {
        let mut dib = vec![0u8; 40 + 2 * 8];
        store_u32_le(&mut dib, 0, 40);
        store_i32_le(&mut dib, 4, 2); // width
        store_i32_le(&mut dib, 8, 2); // 正高度 = bottom-up
        store_u16_le(&mut dib, 12, 1);
        store_u16_le(&mut dib, 14, 24);
        // 第一行是图片最底下一行，BGR(10,20,30) 对应 RGB(30,20,10)。
        dib[40..46].copy_from_slice(&[10, 20, 30, 40, 50, 60]);
        // 第二行是图片最上面一行，BGR(70,80,90) 对应 RGB(90,80,70)。
        dib[48..54].copy_from_slice(&[70, 80, 90, 100, 110, 120]);

        let (width, height, color, pixels) = decode_png(&dib_to_png(&dib).expect("DIB 转 PNG"));
        assert_eq!((width, height), (2, 2));
        assert_eq!(color, png::ColorType::Rgb);
        assert_eq!(
            pixels,
            vec![90, 80, 70, 120, 110, 100, 30, 20, 10, 60, 50, 40]
        );
    }

    #[test]
    fn top_down_24bpp_dib_is_not_flipped() {
        let mut dib = vec![0u8; 40 + 8];
        store_u32_le(&mut dib, 0, 40);
        store_i32_le(&mut dib, 4, 2);
        store_i32_le(&mut dib, 8, -1); // 负高度 = top-down
        store_u16_le(&mut dib, 12, 1);
        store_u16_le(&mut dib, 14, 24);
        dib[40..46].copy_from_slice(&[1, 2, 3, 4, 5, 6]);

        let (_, _, _, pixels) = decode_png(&dib_to_png(&dib).expect("DIB 转 PNG"));
        assert_eq!(pixels, vec![3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn rejects_compressed_dib() {
        let png = encode_png(png::ColorType::Rgba, 1, 1, &[1, 2, 3, 4]);
        let mut dib = png_to_dib(&png).expect("PNG 转 DIB");
        store_u32_le(&mut dib, 16, 3); // BI_BITFIELDS
        assert!(dib_to_png(&dib).is_none());
    }

    #[test]
    fn rejects_oversized_dib() {
        // 只有头部没有像素；巨大尺寸应在分配/读取前就被拒绝。
        let mut dib = vec![0u8; 40];
        store_u32_le(&mut dib, 0, 40);
        store_i32_le(&mut dib, 4, 100_000);
        store_i32_le(&mut dib, 8, 100_000);
        store_u16_le(&mut dib, 12, 1);
        store_u16_le(&mut dib, 14, 32);
        assert_eq!(read_u32_le(&dib, 16), Some(0));
        assert!(dib_to_png(&dib).is_none());
    }

    #[test]
    fn poll_ignores_unchanged_sequence() {
        assert!(!should_emit_clip(9, 9, None, None));
    }

    #[test]
    fn poll_ignores_own_text_write() {
        assert!(!should_emit_clip(7, 10, Some(10), None));
    }

    #[test]
    fn poll_ignores_own_image_write() {
        assert!(!should_emit_clip(7, 10, None, Some(10)));
    }

    #[test]
    fn poll_reports_external_change() {
        assert!(should_emit_clip(7, 10, Some(6), Some(5)));
    }

    #[test]
    #[ignore = "依赖真实桌面会话的剪贴板；CI/无桌面服务会话里可能没有剪贴板，故默认跳过"]
    fn real_clipboard_image_roundtrip() {
        let source = vec![255, 128, 0, 255];
        let png = encode_png(png::ColorType::Rgba, 1, 1, &source);
        assert!(write_image_png(&png));
        assert!(has_image());
        let restored = read_image_png().expect("读回位图");
        let (width, height, _, pixels) = decode_png(&restored);
        assert_eq!((width, height), (1, 1));
        assert_eq!(pixels, source);
        assert_eq!(image_sequence(), sequence());
    }
}
