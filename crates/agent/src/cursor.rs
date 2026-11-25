//! 光标形状合成。
//!
//! DXGI 桌面复制只给桌面画面，硬件光标要单独用 `GetFramePointerShape`
//! 取形状、用 `DXGI_OUTDUPL_FRAME_INFO::PointerPosition` 取位置。这里只做
//! 纯像素运算，不碰 COM，方便单测覆盖三种形状类型与越界裁剪。
//!
//! 三种形状的语义（见 dxgi1_2.h）：
//! - MONOCHROME：1bpp 的 AND 掩码后面跟同尺寸的 XOR 掩码，经典规则
//!   `dst = (dst & and_mask) ^ xor_mask`；
//! - COLOR：32bpp BGRA，按 alpha 混合；
//! - MASKED_COLOR：32bpp BGRA，alpha 只有 0 / 0xFF，0 表示直接替换，
//!   0xFF 表示与目标异或。

/// 光标形状类型，对应 `DXGI_OUTDUPL_POINTER_SHAPE_TYPE`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerShapeKind {
    /// 单色：AND 掩码 + XOR 掩码，各 `pitch * height` 字节。
    Monochrome,
    /// 彩色 BGRA，alpha 参与混合。
    Color,
    /// 带掩码的彩色 BGRA，alpha 只取 0 / 0xFF。
    MaskedColor,
}

/// 从 `GetFramePointerShape` 取到的一块光标位图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerShape {
    pub kind: PointerShapeKind,
    /// 像素宽度（单色也是像素宽度，不是字节宽度）。
    pub width: usize,
    /// 像素高度。
    pub height: usize,
    /// 每行字节数；单色时是每行掩码的字节数。
    pub pitch: usize,
    /// 形状数据；单色时前半段是 AND 掩码，后半段是 XOR 掩码。
    pub pixels: Vec<u8>,
}

impl PointerShape {
    /// 数据长度是否满足该类型的最低要求。
    pub fn is_valid(&self) -> bool {
        if self.width == 0 || self.height == 0 || self.pitch == 0 {
            return false;
        }
        let rows = self.pitch.saturating_mul(self.height);
        let needed = match self.kind {
            PointerShapeKind::Monochrome => rows.saturating_mul(2),
            PointerShapeKind::Color | PointerShapeKind::MaskedColor => rows,
        };
        self.pixels.len() >= needed
    }
}

/// 把光标就地画到紧凑 BGRA 帧上。
///
/// `x` / `y` 是光标左上角在帧内的坐标（DXGI 的 `PointerPosition` 给的就是
/// 左上角，不是热点）。光标可能有一部分在画面外，越界像素直接跳过；
/// 形状数据不完整或尺寸为 0 时不做任何修改。
pub fn composite_cursor(
    frame: &mut [u8],
    frame_w: usize,
    frame_h: usize,
    shape: &PointerShape,
    x: i32,
    y: i32,
) {
    if frame_w == 0 || frame_h == 0 || !shape.is_valid() {
        return;
    }
    let frame_bytes = frame_w.saturating_mul(frame_h).saturating_mul(4);
    if frame.len() < frame_bytes {
        return;
    }

    for sy in 0..shape.height {
        let dy = y as i64 + sy as i64;
        if dy < 0 || dy >= frame_h as i64 {
            continue;
        }
        let row = dy as usize * frame_w;
        for sx in 0..shape.width {
            let dx = x as i64 + sx as i64;
            if dx < 0 || dx >= frame_w as i64 {
                continue;
            }
            let dst = (row + dx as usize) * 4;
            match shape.kind {
                PointerShapeKind::Monochrome => {
                    // 1bpp：字节内高位在左。
                    let mask_row = sy * shape.pitch;
                    if sx / 8 >= shape.pitch {
                        continue;
                    }
                    let byte = mask_row + sx / 8;
                    let bit = 7 - (sx % 8);
                    let and_bit = (shape.pixels[byte] >> bit) & 1;
                    let xor_bit = (shape.pixels[shape.pitch * shape.height + byte] >> bit) & 1;
                    if and_bit == 1 && xor_bit == 0 {
                        // 完全保留目标像素，透明区域不必写回。
                        continue;
                    }
                    let and = if and_bit == 1 { 0xFF } else { 0x00 };
                    let xor = if xor_bit == 1 { 0xFF } else { 0x00 };
                    for channel in 0..3 {
                        frame[dst + channel] = (frame[dst + channel] & and) ^ xor;
                    }
                    frame[dst + 3] = 255;
                }
                PointerShapeKind::Color | PointerShapeKind::MaskedColor => {
                    let src = sy * shape.pitch + sx * 4;
                    if sx * 4 + 4 > shape.pitch || src + 4 > shape.pixels.len() {
                        continue;
                    }
                    let alpha = shape.pixels[src + 3];
                    match shape.kind {
                        PointerShapeKind::MaskedColor if alpha == 0xFF => {
                            for channel in 0..3 {
                                frame[dst + channel] ^= shape.pixels[src + channel];
                            }
                        }
                        PointerShapeKind::MaskedColor if alpha == 0 => {
                            frame[dst..dst + 3].copy_from_slice(&shape.pixels[src..src + 3]);
                        }
                        _ => {
                            // COLOR，以及 MASKED_COLOR 里不该出现的中间 alpha：
                            // 按常规 alpha 混合处理。
                            if alpha == 0 {
                                continue;
                            }
                            if alpha == 255 {
                                frame[dst..dst + 3].copy_from_slice(&shape.pixels[src..src + 3]);
                            } else {
                                let a = alpha as u32;
                                for channel in 0..3 {
                                    let src_c = shape.pixels[src + channel] as u32;
                                    let dst_c = frame[dst + channel] as u32;
                                    frame[dst + channel] =
                                        ((src_c * a + dst_c * (255 - a) + 127) / 255) as u8;
                                }
                            }
                        }
                    }
                    frame[dst + 3] = 255;
                }
            }
        }
    }
}

/// 光标覆盖区域的像素备份。
///
/// DXGI 只在光标移动时给「无新画面」的帧；为了不重拷 8MB 纹理，把上一帧
/// 被光标盖住的像素存下来，下一帧先还原再画新光标，避免残影。
#[derive(Default)]
pub struct CursorBackup {
    rect: Option<(usize, usize, usize, usize)>,
    pixels: Vec<u8>,
}

impl CursorBackup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.rect = None;
    }

    /// 备份光标即将覆盖的像素。区域完全在画面外时清空备份。
    pub fn save(
        &mut self,
        frame: &[u8],
        frame_w: usize,
        frame_h: usize,
        shape: &PointerShape,
        x: i32,
        y: i32,
    ) {
        self.rect = None;
        if frame_w == 0 || frame_h == 0 || !shape.is_valid() {
            return;
        }
        if frame.len() < frame_w.saturating_mul(frame_h).saturating_mul(4) {
            return;
        }
        // 与 composite_cursor 一致：裁剪到画面内。
        let left = x.max(0) as usize;
        let top = y.max(0) as usize;
        let right = (x as i64 + shape.width as i64).clamp(0, frame_w as i64) as usize;
        let bottom = (y as i64 + shape.height as i64).clamp(0, frame_h as i64) as usize;
        if left >= right || top >= bottom {
            return;
        }
        let w = right - left;
        let h = bottom - top;
        self.pixels.clear();
        self.pixels.reserve(w * h * 4);
        for row in top..bottom {
            let start = (row * frame_w + left) * 4;
            self.pixels.extend_from_slice(&frame[start..start + w * 4]);
        }
        self.rect = Some((left, top, w, h));
    }

    /// 把备份写回画面，撤销上一次画上的光标。返回是否真的还原了。
    pub fn restore(&mut self, frame: &mut [u8], frame_w: usize) -> bool {
        let Some((left, top, w, h)) = self.rect.take() else {
            return false;
        };
        // 备份时的裁剪以当时的画面宽度为准；还原时画面宽度理应一致，
        // 这里再挡一次，避免尺寸变化后越界写。
        if left.saturating_add(w) > frame_w
            || frame.len()
                < frame_w
                    .saturating_mul(top.saturating_add(h))
                    .saturating_mul(4)
        {
            return false;
        }
        if self.pixels.len() < w * h * 4 {
            return false;
        }
        for (index, row) in (top..top + h).enumerate() {
            let dst = (row * frame_w + left) * 4;
            let src = index * w * 4;
            frame[dst..dst + w * 4].copy_from_slice(&self.pixels[src..src + w * 4]);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(w: usize, h: usize, pixel: [u8; 4]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            frame.extend_from_slice(&pixel);
        }
        frame
    }

    fn pixel(frame: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * w + x) * 4;
        [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
    }

    #[test]
    fn monochrome_and_xor_mask() {
        let mut frame = solid_frame(4, 2, [10, 20, 30, 255]);
        // 2x2 光标，pitch = 1 字节。
        // (0,0) AND=1 XOR=0 保留；(1,0) AND=0 XOR=0 涂黑；
        // (0,1) AND=0 XOR=1 涂白；(1,1) AND=1 XOR=1 反色。
        let shape = PointerShape {
            kind: PointerShapeKind::Monochrome,
            width: 2,
            height: 2,
            pitch: 1,
            pixels: vec![0b1000_0000, 0b0100_0000, 0b0000_0000, 0b1100_0000],
        };
        composite_cursor(&mut frame, 4, 2, &shape, 0, 0);

        assert_eq!(pixel(&frame, 4, 0, 0), [10, 20, 30, 255]);
        assert_eq!(pixel(&frame, 4, 1, 0), [0, 0, 0, 255]);
        assert_eq!(pixel(&frame, 4, 0, 1), [255, 255, 255, 255]);
        assert_eq!(pixel(&frame, 4, 1, 1), [245, 235, 225, 255]);
        // 光标范围外的像素不受影响。
        assert_eq!(pixel(&frame, 4, 2, 0), [10, 20, 30, 255]);
    }

    #[test]
    fn color_shape_alpha_blends() {
        let mut frame = solid_frame(1, 1, [100, 100, 100, 255]);
        let shape = PointerShape {
            kind: PointerShapeKind::Color,
            width: 1,
            height: 1,
            pitch: 4,
            pixels: vec![200, 200, 200, 128],
        };
        composite_cursor(&mut frame, 1, 1, &shape, 0, 0);
        // (200 * 128 + 100 * 127 + 127) / 255 = 150
        assert_eq!(pixel(&frame, 1, 0, 0), [150, 150, 150, 255]);
    }

    #[test]
    fn color_shape_honours_pitch_padding() {
        let mut frame = solid_frame(2, 1, [0, 0, 0, 255]);
        // 两个像素，但每行 12 字节，后 4 字节是行尾填充。
        let shape = PointerShape {
            kind: PointerShapeKind::Color,
            width: 2,
            height: 1,
            pitch: 12,
            pixels: vec![1, 2, 3, 255, 4, 5, 6, 255, 9, 9, 9, 9],
        };
        composite_cursor(&mut frame, 2, 1, &shape, 0, 0);
        assert_eq!(pixel(&frame, 2, 0, 0), [1, 2, 3, 255]);
        assert_eq!(pixel(&frame, 2, 1, 0), [4, 5, 6, 255]);
    }

    #[test]
    fn masked_color_replaces_or_xors() {
        let mut frame = solid_frame(2, 1, [10, 20, 30, 255]);
        let second = [40u8, 50, 60, 255];
        frame[4..8].copy_from_slice(&second);
        let shape = PointerShape {
            kind: PointerShapeKind::MaskedColor,
            width: 2,
            height: 1,
            pitch: 8,
            pixels: vec![1, 2, 3, 0, 0x10, 0x20, 0x40, 0xFF],
        };
        composite_cursor(&mut frame, 2, 1, &shape, 0, 0);
        // alpha = 0：直接替换。
        assert_eq!(pixel(&frame, 2, 0, 0), [1, 2, 3, 255]);
        // alpha = 0xFF：与目标异或（40^16=56、50^32=18、60^64=124）。
        assert_eq!(pixel(&frame, 2, 1, 0), [56, 18, 124, 255]);
    }

    #[test]
    fn clipping_skips_pixels_outside_frame() {
        let mut frame = solid_frame(3, 3, [7, 7, 7, 255]);
        let shape = PointerShape {
            kind: PointerShapeKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            pixels: vec![
                1, 1, 1, 255, 2, 2, 2, 255, // 第一行
                3, 3, 3, 255, 4, 4, 4, 255, // 第二行
            ],
        };
        // 左上角在 (-1, -1)：只有形状的 (1,1) 落在画面 (0,0)。
        composite_cursor(&mut frame, 3, 3, &shape, -1, -1);
        assert_eq!(pixel(&frame, 3, 0, 0), [4, 4, 4, 255]);
        assert_eq!(pixel(&frame, 3, 1, 0), [7, 7, 7, 255]);

        // 右下角：只有形状的 (0,0) 落在画面 (2,2)。
        composite_cursor(&mut frame, 3, 3, &shape, 2, 2);
        assert_eq!(pixel(&frame, 3, 2, 2), [1, 1, 1, 255]);
        assert_eq!(pixel(&frame, 3, 1, 2), [7, 7, 7, 255]);

        // 完全在画面外：什么都不改。
        let before = frame.clone();
        composite_cursor(&mut frame, 3, 3, &shape, 10, 10);
        composite_cursor(&mut frame, 3, 3, &shape, -5, -5);
        assert_eq!(frame, before);
    }

    #[test]
    fn backup_restores_pixels_under_cursor() {
        let shape = PointerShape {
            kind: PointerShapeKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            pixels: vec![9; 16],
        };
        let clean = solid_frame(4, 4, [5, 6, 7, 255]);
        let mut frame = clean.clone();
        let mut backup = CursorBackup::new();
        backup.save(&frame, 4, 4, &shape, 1, 1);
        // 在光标区域上乱涂，再还原应当回到原样。
        for value in frame[((4 + 1) * 4)..((4 + 1) * 4 + 8)].iter_mut() {
            *value = 0;
        }
        assert!(backup.restore(&mut frame, 4));
        assert_eq!(frame, clean);
        // 第二次还原没有备份可用了。
        assert!(!backup.restore(&mut frame, 4));
    }

    #[test]
    fn backup_clips_partial_cursor() {
        let shape = PointerShape {
            kind: PointerShapeKind::Color,
            width: 4,
            height: 4,
            pitch: 16,
            pixels: vec![0; 64],
        };
        let clean = solid_frame(3, 3, [1, 2, 3, 255]);
        let mut frame = clean.clone();
        let mut backup = CursorBackup::new();
        // 光标左上角在 (-2, -2)：只有右下 2×2 落在画面内，即 (0,0)/(1,0)/(0,1)/(1,1)。
        backup.save(&frame, 3, 3, &shape, -2, -2);
        frame[0..4].copy_from_slice(&[200, 200, 200, 255]);
        frame[4..8].copy_from_slice(&[201, 201, 201, 255]);
        frame[12..16].copy_from_slice(&[202, 202, 202, 255]);
        frame[16..20].copy_from_slice(&[203, 203, 203, 255]);
        assert!(backup.restore(&mut frame, 3));
        assert_eq!(frame, clean);
    }

    #[test]
    fn backup_outside_frame_is_noop() {
        let shape = PointerShape {
            kind: PointerShapeKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            pixels: vec![0; 16],
        };
        let mut frame = solid_frame(2, 2, [1, 1, 1, 255]);
        let mut backup = CursorBackup::new();
        backup.save(&frame, 2, 2, &shape, 5, 5);
        assert!(!backup.restore(&mut frame, 2));
    }

    #[test]
    fn invalid_shape_is_noop() {
        let mut frame = solid_frame(2, 2, [1, 2, 3, 255]);
        let before = frame.clone();
        // 数据比 pitch * height 短，按损坏处理，不能越界读。
        let short = PointerShape {
            kind: PointerShapeKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            pixels: vec![0; 4],
        };
        composite_cursor(&mut frame, 2, 2, &short, 0, 0);
        assert_eq!(frame, before);

        // 空形状（例如光标隐藏时没有取到形状）同样不画。
        let empty = PointerShape {
            kind: PointerShapeKind::Monochrome,
            width: 0,
            height: 0,
            pitch: 0,
            pixels: Vec::new(),
        };
        composite_cursor(&mut frame, 2, 2, &empty, 0, 0);
        assert_eq!(frame, before);
    }
}
