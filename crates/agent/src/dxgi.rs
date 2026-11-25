//! DXGI Desktop Duplication 采集。
//!
//! GDI 的 `StretchBlt` 在 Windows 11 上单帧就要约 40ms（DWM 回读），
//! 1080p 最多只能跑 10fps 左右。这里改从 GPU 的桌面复制表面取帧，
//! 1080p 单帧通常 1–3ms；拿到的是 BGRA 像素，直接交给 JPEG 编码。
//!
//! 适用范围：单显示器、桌面会话。多显示器、远程桌面会话、被其他
//! 捕获程序独占时 `DuplicateOutput` 会失败，调用方回退到 GDI。

use std::fmt;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::core::{Error as WindowsError, Interface};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_MORE_DATA, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
    DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

use crate::cursor::{composite_cursor, CursorBackup, PointerShape, PointerShapeKind};
use crate::display::MonitorRect;

/// d3d11.h 里的 D3D11_SDK_VERSION。
const D3D11_SDK_VERSION: u32 = 7;

/// DXGI 采集失败分类。采集层据此决定「立即回退 GDI」还是「再试几帧」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DxgiErrorKind {
    /// 桌面复制已失效（分辨率切换、锁屏、RDP 断开、显示器热插拔），必须重建。
    AccessLost,
    /// 当前环境不支持桌面复制（无头会话、多输出拼接、驱动不支持）。
    Unsupported,
    /// 暂时不可用（复制被其他程序占用、会话切换中），过一会可能恢复。
    NotCurrentlyAvailable,
    /// 其他错误。
    Other,
}

impl DxgiErrorKind {
    fn from_hresult(code: windows::core::HRESULT) -> Self {
        if code == DXGI_ERROR_ACCESS_LOST {
            Self::AccessLost
        } else if code == DXGI_ERROR_UNSUPPORTED {
            Self::Unsupported
        } else if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
            Self::NotCurrentlyAvailable
        } else {
            Self::Other
        }
    }
}

/// 带分类的 DXGI 错误，包进 `anyhow::Error` 后仍可 downcast 回来。
#[derive(Debug)]
pub struct DxgiError {
    kind: DxgiErrorKind,
    message: String,
}

impl DxgiError {
    fn new(kind: DxgiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn from_windows(context: &str, error: WindowsError) -> Self {
        Self::new(
            DxgiErrorKind::from_hresult(error.code()),
            format!("{context}: {error}"),
        )
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self::new(DxgiErrorKind::Unsupported, message)
    }

    pub fn kind(&self) -> DxgiErrorKind {
        self.kind
    }

    /// ACCESS_LOST 之后同一个复制对象不可能恢复，必须立即重建。
    /// 其余错误先观察几帧，避免偶发抖动就掉到慢路径。
    pub fn requires_immediate_fallback(&self) -> bool {
        matches!(self.kind, DxgiErrorKind::AccessLost)
    }
}

impl fmt::Display for DxgiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DxgiError {}

pub struct DxgiCapturer {
    /// 设备要一直持有，桌面复制表面属于它。
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    /// 从映射内存拷出的紧凑 BGRA 帧（stride = width * 4）。
    buffer: Vec<u8>,
    last_blit_us: u64,
    /// 是否把系统光标合成进画面。
    cursor_enabled: bool,
    /// 最近一次取到的光标形状；形状不变时沿用，避免每帧重新取。
    pointer_shape: PointerShape,
    /// `GetFramePointerShape` 的输出缓冲，形状变化时复用这块内存。
    pointer_buf: Vec<u8>,
    /// 光标左上角在输出内的坐标；`None` 表示还没收到过鼠标位置。
    pointer_pos: Option<(i32, i32)>,
    /// 光标当前是否可见（全屏视频、游戏里通常是隐藏的）。
    pointer_visible: bool,
    /// 上一次画光标前被覆盖的像素，用于在只有光标更新的帧上还原。
    cursor_backup: CursorBackup,
    /// 当前画面里已画上的光标位置；位置/形状不变时跳过重画。
    drawn_pointer: Option<(i32, i32)>,
}

impl DxgiCapturer {
    /// `region` 为要采集的显示器矩形；`None` 表示整块虚拟桌面。
    /// 桌面复制按输出（显示器）工作：指定显示器时直接复制对应输出；
    /// 整块虚拟桌面只有单屏时才等价于一个输出，多屏请走 GDI 拼接。
    pub fn new(region: Option<MonitorRect>, cursor: bool) -> Result<Self> {
        // 首次初始化多等几次，覆盖会话切换时旧复制对象还没释放的情况。
        Self::new_with_attempts(region, cursor, 4)
    }

    /// 同 [`DxgiCapturer::new`]，但可指定 `DuplicateOutput` 的尝试次数。
    /// 回退期间的周期性探测用 1 次，避免每次探测都卡住抓屏线程数百毫秒。
    pub fn new_with_attempts(
        region: Option<MonitorRect>,
        cursor: bool,
        attempts: u32,
    ) -> Result<Self> {
        let attempts = attempts.max(1);
        let levels = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let mut device = None;
        let mut context = None;
        let mut level = D3D_FEATURE_LEVEL_11_0;
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .context("D3D11CreateDevice 失败")?;
        }
        let device = device.context("D3D11 设备为空")?;
        let context = context.context("D3D11 上下文为空")?;

        let dxgi_device: IDXGIDevice = device.cast().context("获取 IDXGIDevice 失败")?;
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }.context("获取显卡失败")?;

        let mut outputs = Vec::new();
        for index in 0..8u32 {
            match unsafe { adapter.EnumOutputs(index) } {
                Ok(output) => outputs.push(output),
                Err(_) => break,
            }
        }
        let output: IDXGIOutput = match region {
            Some(region) => outputs
                .into_iter()
                .find(|output| {
                    let Ok(desc) = (unsafe { output.GetDesc() }) else {
                        return false;
                    };
                    let rect = desc.DesktopCoordinates;
                    rect.left == region.x
                        && rect.top == region.y
                        && (rect.right - rect.left).max(0) as u32 == region.w
                        && (rect.bottom - rect.top).max(0) as u32 == region.h
                })
                .with_context(|| {
                    format!(
                        "没有找到坐标 ({}, {}) {}×{} 的显示器输出",
                        region.x, region.y, region.w, region.h
                    )
                })?,
            None => {
                // 整块虚拟桌面：单屏可以直接复制，多屏需要拼接，交给 GDI。
                // 显示器全部断开时这里可能一个输出都没有，同样交给 GDI。
                if outputs.len() != 1 {
                    bail!("检测到 {} 个输出，整块桌面拼接需要 GDI", outputs.len());
                }
                outputs.pop().expect("刚检查过只有一个输出")
            }
        };
        let desc = unsafe { output.GetDesc() }.context("获取显示器信息失败")?;
        if !desc.AttachedToDesktop.as_bool() {
            return Err(anyhow::Error::new(DxgiError::unsupported(
                "显示器未连接到桌面",
            )));
        }
        let width = desc.DesktopCoordinates.right - desc.DesktopCoordinates.left;
        let height = desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top;
        if width <= 0 || height <= 0 {
            return Err(anyhow::Error::new(DxgiError::unsupported(format!(
                "显示器尺寸无效: {width}×{height}"
            ))));
        }
        let width = width as u32;
        let height = height as u32;

        let output1: IDXGIOutput1 = output.cast().context("IDXGIOutput1 不可用")?;
        // 同一个输出同时只允许一个桌面复制。切换会话时旧会话可能还在收尾，
        // 稍等重试几次，避免直接掉到慢一档的 GDI 路径。
        let mut last_error: Option<(DxgiErrorKind, String)> = None;
        let mut dupl = None;
        for attempt in 0..attempts {
            match unsafe { output1.DuplicateOutput(&device) } {
                Ok(value) => {
                    dupl = Some(value);
                    break;
                }
                Err(error) => {
                    last_error =
                        Some((DxgiErrorKind::from_hresult(error.code()), error.to_string()));
                    if attempt + 1 < attempts {
                        std::thread::sleep(Duration::from_millis(120));
                    }
                }
            }
        }
        let Some(dupl) = dupl else {
            let (kind, message) =
                last_error.unwrap_or((DxgiErrorKind::Other, "未知错误".to_string()));
            return Err(anyhow::Error::new(DxgiError::new(
                kind,
                format!("DuplicateOutput 失败: {message}"),
            )));
        };

        // 桌面复制只给出 GPU 表面，必须拷到可读的 staging 纹理再映射。
        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { device.CreateTexture2D(&texture_desc, None, Some(&mut staging)) }
            .context("创建 staging 纹理失败")?;
        let staging = staging.context("staging 纹理为空")?;

        tracing::info!(width, height, cursor, "DXGI 桌面复制已就绪");

        Ok(Self {
            _device: device,
            context,
            dupl,
            staging,
            width,
            height,
            buffer: Vec::new(),
            last_blit_us: 0,
            cursor_enabled: cursor,
            pointer_shape: PointerShape {
                kind: PointerShapeKind::Color,
                width: 0,
                height: 0,
                pitch: 0,
                pixels: Vec::new(),
            },
            pointer_buf: Vec::new(),
            pointer_pos: None,
            pointer_visible: false,
            cursor_backup: CursorBackup::new(),
            drawn_pointer: None,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn last_blit_us(&self) -> u64 {
        self.last_blit_us
    }

    /// 取最新一帧；`Ok(None)` 表示这段时间桌面没有更新。
    pub fn grab(&mut self, timeout: Duration) -> Result<Option<&[u8]>> {
        let started = std::time::Instant::now();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe {
            self.dupl
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                return Ok(None);
            }
            Err(error) => {
                // ACCESS_LOST 等错误必须保留类型，采集层要靠它判断立即回退。
                return Err(anyhow::Error::new(DxgiError::from_windows(
                    "AcquireNextFrame 失败",
                    error,
                )));
            }
        }

        // 光标位置/形状必须在持有帧的时候取，拿到帧后无论成败都要 ReleaseFrame，
        // 否则下一帧会一直失败。
        let pointer = self.read_pointer(&info);
        let result = self.compose_frame(&info, resource, pointer);
        let release = unsafe { self.dupl.ReleaseFrame() };
        let changed = result?;
        if let Err(error) = release {
            tracing::warn!("ReleaseFrame 失败: {error}");
        }
        if !changed {
            return Ok(None);
        }
        self.last_blit_us = started.elapsed().as_micros() as u64;
        Ok(Some(&self.buffer))
    }

    /// 把本帧画面（可能只有光标变化）合成进 `buffer`，返回画面是否有变化。
    ///
    /// 只有光标移动/变形时 `AccumulatedFrames` 为 0，桌面画面本身没变。
    /// 此时不重拷纹理（8MB，光标每动一下都拷太亏），而是把上一帧被光标
    /// 覆盖的像素用备份还原，再画新位置的光标。
    fn compose_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
        pointer: Option<(i32, i32)>,
    ) -> Result<bool> {
        let mut changed = false;
        // AccumulatedFrames > 0 表示桌面画面本帧有更新；只有光标移动时为 0。
        if info.AccumulatedFrames > 0 {
            // DXGI 给的桌面纹理不含硬件光标，拷出来就是干净画面。
            self.copy_frame(resource)?;
            // 画面整体换新，旧备份和「已画位置」都作废。
            self.cursor_backup.clear();
            self.drawn_pointer = None;
            changed = true;
        }

        match pointer {
            Some((x, y)) => {
                // 位置和形状都没变：上一帧已经画好，不必重画。
                if self.drawn_pointer == Some((x, y)) {
                    return Ok(changed);
                }
                // 还没拿到过完整画面时不能把空缓冲交给缩放逻辑。
                if self.buffer.len() < self.width as usize * self.height as usize * 4 {
                    return Ok(changed);
                }
                // 桌面没变时先把上一帧画上的光标撤掉，否则会留下残影。
                self.cursor_backup
                    .restore(&mut self.buffer, self.width as usize);
                // 复用缓存的光标形状，不在每帧分配新 Vec。
                self.cursor_backup.save(
                    &self.buffer,
                    self.width as usize,
                    self.height as usize,
                    &self.pointer_shape,
                    x,
                    y,
                );
                composite_cursor(
                    &mut self.buffer,
                    self.width as usize,
                    self.height as usize,
                    &self.pointer_shape,
                    x,
                    y,
                );
                self.drawn_pointer = Some((x, y));
                changed = true;
            }
            None => {
                if self
                    .cursor_backup
                    .restore(&mut self.buffer, self.width as usize)
                {
                    // 光标隐藏或离开画面：把上一帧的旧光标擦掉。
                    changed = true;
                }
                self.drawn_pointer = None;
            }
        }
        Ok(changed)
    }

    /// 从本帧信息里更新光标位置与形状，返回需要画到画面上的左上角坐标。
    ///
    /// 位置每帧都可能变；形状只有 `PointerShapeBufferSize > 0` 时才更新，
    /// 其余帧沿用缓存的形状。取形状失败只记日志，不影响画面本身。
    fn read_pointer(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Option<(i32, i32)> {
        if !self.cursor_enabled {
            return None;
        }
        // LastMouseUpdateTime 非零才表示本帧带了鼠标更新；为零时位置字段无效。
        if info.LastMouseUpdateTime != 0 {
            self.pointer_visible = info.PointerPosition.Visible.as_bool();
            if self.pointer_visible {
                // DXGI 给的是左上角，不是热点。
                self.pointer_pos = Some((
                    info.PointerPosition.Position.x,
                    info.PointerPosition.Position.y,
                ));
            }
        }
        if info.PointerShapeBufferSize > 0 {
            // 形状变了：即使位置没动也要重画。
            self.drawn_pointer = None;
            if let Err(error) = self.fetch_pointer_shape(info.PointerShapeBufferSize) {
                tracing::debug!("读取光标形状失败: {error:#}");
                self.pointer_shape = PointerShape {
                    kind: PointerShapeKind::Color,
                    width: 0,
                    height: 0,
                    pitch: 0,
                    pixels: Vec::new(),
                };
            }
        }
        if !self.pointer_visible {
            return None;
        }
        let pos = self.pointer_pos?;
        self.pointer_shape.is_valid().then_some(pos)
    }

    /// 取光标形状。先问一次所需大小再分配，形状不变时复用缓冲。
    fn fetch_pointer_shape(&mut self, needed: u32) -> Result<()> {
        let needed = needed as usize;
        if self.pointer_buf.len() < needed {
            self.pointer_buf.resize(needed, 0);
        }
        let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut required = 0u32;
        // 安全：pointer_buf 已按 needed 扩容，GetFramePointerShape 只写这块
        // 缓冲与两个栈上输出；调用期间缓冲不会被别处借用。
        let result = unsafe {
            self.dupl.GetFramePointerShape(
                self.pointer_buf.len() as u32,
                self.pointer_buf.as_mut_ptr() as *mut _,
                &mut required,
                &mut shape_info,
            )
        };
        let shape_info = match result {
            Ok(()) => shape_info,
            Err(error) if error.code() == DXGI_ERROR_MORE_DATA => {
                // 缓冲区不够：按返回的所需大小扩一次再取。
                let grown = (required as usize).max(needed);
                self.pointer_buf.resize(grown, 0);
                let mut retry_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                let mut retry_required = 0u32;
                // 安全：同上，缓冲已按 grown 扩容。
                unsafe {
                    self.dupl.GetFramePointerShape(
                        self.pointer_buf.len() as u32,
                        self.pointer_buf.as_mut_ptr() as *mut _,
                        &mut retry_required,
                        &mut retry_info,
                    )
                }
                .context("GetFramePointerShape 扩容后仍失败")?;
                required = retry_required;
                retry_info
            }
            Err(error) => return Err(error).context("GetFramePointerShape 失败"),
        };

        let kind = if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
            PointerShapeKind::Monochrome
        } else if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
            PointerShapeKind::Color
        } else if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
            PointerShapeKind::MaskedColor
        } else {
            bail!("未知的光标形状类型: {}", shape_info.Type);
        };
        // required 是实际写入的字节数；个别驱动成功时不给这个值，按
        // pitch * height 推算（单色要两段掩码）兜底。
        let rows = (shape_info.Pitch as usize).saturating_mul(shape_info.Height as usize);
        let expected = match kind {
            PointerShapeKind::Monochrome => rows.saturating_mul(2),
            PointerShapeKind::Color | PointerShapeKind::MaskedColor => rows,
        };
        let used = (required as usize)
            .max(expected)
            .min(self.pointer_buf.len());
        self.pointer_shape = PointerShape {
            kind,
            width: shape_info.Width as usize,
            height: shape_info.Height as usize,
            pitch: shape_info.Pitch as usize,
            pixels: self.pointer_buf[..used].to_vec(),
        };
        Ok(())
    }

    fn copy_frame(&mut self, resource: Option<IDXGIResource>) -> Result<()> {
        let resource = resource.context("桌面复制没有返回画面资源")?;
        let texture: ID3D11Texture2D = resource.cast().context("画面资源不是 2D 纹理")?;
        unsafe {
            self.context.CopyResource(&self.staging, &texture);
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .context("映射 staging 纹理失败")?;

        let width = self.width as usize;
        let height = self.height as usize;
        let row_bytes = width * 4;
        self.buffer.resize(row_bytes * height, 0);
        let pitch = mapped.RowPitch as usize;
        if pitch < row_bytes {
            unsafe { self.context.Unmap(&self.staging, 0) };
            bail!("纹理行距异常: pitch={pitch} row={row_bytes}");
        }
        unsafe {
            let src = mapped.pData as *const u8;
            let dst = self.buffer.as_mut_ptr();
            for y in 0..height {
                std::ptr::copy_nonoverlapping(
                    src.add(y * pitch),
                    dst.add(y * row_bytes),
                    row_bytes,
                );
            }
            self.context.Unmap(&self.staging, 0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HRESULT;

    #[test]
    fn error_kinds_are_classified() {
        assert_eq!(
            DxgiErrorKind::from_hresult(DXGI_ERROR_ACCESS_LOST),
            DxgiErrorKind::AccessLost
        );
        assert_eq!(
            DxgiErrorKind::from_hresult(DXGI_ERROR_UNSUPPORTED),
            DxgiErrorKind::Unsupported
        );
        assert_eq!(
            DxgiErrorKind::from_hresult(DXGI_ERROR_NOT_CURRENTLY_AVAILABLE),
            DxgiErrorKind::NotCurrentlyAvailable
        );
        assert_eq!(
            DxgiErrorKind::from_hresult(HRESULT(0x8000_4005_u32 as i32)),
            DxgiErrorKind::Other
        );
    }

    #[test]
    fn access_lost_requires_immediate_fallback() {
        let lost = DxgiError::new(DxgiErrorKind::AccessLost, "access lost");
        assert!(lost.requires_immediate_fallback());
        // 其余错误先观察几帧，避免偶发失败就掉到 GDI。
        for kind in [
            DxgiErrorKind::Unsupported,
            DxgiErrorKind::NotCurrentlyAvailable,
            DxgiErrorKind::Other,
        ] {
            assert!(!DxgiError::new(kind, "transient").requires_immediate_fallback());
        }
    }

    #[test]
    fn dxgi_error_survives_anyhow_downcast() {
        let error = anyhow::Error::new(DxgiError::new(DxgiErrorKind::AccessLost, "显示器已断开"));
        let typed = error
            .downcast_ref::<DxgiError>()
            .expect("采集层需要能取回错误分类");
        assert_eq!(typed.kind(), DxgiErrorKind::AccessLost);
        assert!(typed.to_string().contains("显示器已断开"));
    }
}
