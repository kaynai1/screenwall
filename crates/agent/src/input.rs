//! 键鼠注入：把协议中的 `InputEvent` 翻译成 Win32 `SendInput`。
//!
//! 与 `capture.rs` 一样直接使用 raw FFI，不引入 windows-sys。
//! 所有坐标均由主控端相对整块虚拟桌面（超级屏）归一化到 0..1，
//! 本模块用 `MOUSEEVENTF_VIRTUALDESK` 把它映射到整个虚拟桌面的
//! 0..65535；虚拟桌面指标异常时退回主屏映射。
//!
//! 注入过程维护当前按下的鼠标键和键盘扫描码，`ReleaseAll`、会话结束、
//! 采集停止时统一释放，避免对端断线后按键卡住。

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use screenwall_proto::{InputEvent, MouseButton, SasMethod};

use crate::display::MonitorRect;

// ---------------------------------------------------------------- 常量

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;

const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_XDOWN: u32 = 0x0080;
const MOUSEEVENTF_XUP: u32 = 0x0100;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;

const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;

const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const KEYEVENTF_SCANCODE: u32 = 0x0008;

/// 单个批次最多处理的事件数，防止异常负载长时间占用输入锁。
const MAX_EVENTS_PER_BATCH: usize = 256;
/// 一条 `Text` 事件最多注入的 UTF-16 code unit 数。
const MAX_TEXT_UNITS: usize = 1024;
/// 单次 `SendInput` 调用的 INPUT 数量上限（文本按此切块）。
const MAX_INPUTS_PER_CALL: usize = 256;
/// X1/X2 键在 mouseData 中的标识。
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

/// UIPI 拦截时 `SendInput` 的 `GetLastError`。
const ERROR_ACCESS_DENIED: i32 = 5;
/// UIPI 状态持续期间的重报间隔，避免每次点击都刷一条协议消息。
const UIPI_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// 降级发送 Ctrl+Alt+Del 用到的虚拟键码。
const VK_CONTROL: u16 = 0x11;
/// Alt（菜单键）。
const VK_MENU: u16 = 0x12;
const VK_DELETE: u16 = 0x2e;

// ---------------------------------------------------------------- FFI

// 64 位布局：INPUT { DWORD type; 4 字节填充; union(32 字节) } = 40 字节。
// MOUSEINPUT 32 字节，KEYBDINPUT 24 字节。
#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeybdInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HardwareInput {
    msg: u32,
    param_l: u16,
    param_h: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InputUnion {
    mouse: MouseInput,
    keyboard: KeybdInput,
    hardware: HardwareInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    kind: u32,
    data: InputUnion,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<MouseInput>() == 32);
    assert!(size_of::<KeybdInput>() == 24);
    assert!(size_of::<Input>() == 40);
};

#[link(name = "user32")]
unsafe extern "system" {
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    fn GetSystemMetrics(index: i32) -> i32;
}

// `sas.dll` 只在需要时加载，避免静态链接在缺少该 DLL 的老系统上直接启动失败。
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> *const c_void;
    fn SetLastError(code: u32);
}

// 读取 SoftwareSASGeneration 策略与当前账户名，用来判断 SendSAS 是否可能成功。
#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: isize,
        sub_key: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        data_size: *mut u32,
    ) -> i32;
    fn GetUserNameW(buffer: *mut u16, size: *mut u32) -> i32;
}

/// `SendSAS` 的签名：`void SendSAS(BOOL AsUser)`，成功与否没有返回值。
type SendSasFn = unsafe extern "system" fn(i32);

/// `HKEY_LOCAL_MACHINE`（32 位值在 64 位上按符号扩展）。
const HKEY_LOCAL_MACHINE: isize = -0x7fff_fffe;
/// `RRF_RT_REG_DWORD`：只接受 REG_DWORD。
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;

/// 虚拟桌面尺寸有效时，绝对坐标按整个虚拟桌面解释。
fn virtual_desk_flag() -> u32 {
    let cx = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let cy = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if cx > 0 && cy > 0 {
        MOUSEEVENTF_VIRTUALDESK
    } else {
        0
    }
}

/// 绝对移动：0..65535 覆盖整块虚拟桌面（原点即虚拟桌面原点）。
fn absolute_move_flags() -> u32 {
    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | virtual_desk_flag()
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> Input {
    Input {
        kind: INPUT_MOUSE,
        data: InputUnion {
            mouse: MouseInput {
                dx,
                dy,
                mouse_data: data,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn key_input(vk: u16, scan: u16, flags: u32) -> Input {
    Input {
        kind: INPUT_KEYBOARD,
        data: InputUnion {
            keyboard: KeybdInput {
                vk,
                scan,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

/// `SendInput` 失败原因。UIPI 是唯一需要回报给主控端的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendFailure {
    /// 前台窗口的完整性级别更高，输入被 UIPI 拦截。
    Uipi,
    /// 其它原因（参数、驱动、桌面切换等）。
    Other,
}

/// 只根据错误码分类：只有 `ERROR_ACCESS_DENIED` 能确定是 UIPI。
fn classify_send_error(code: Option<i32>) -> SendFailure {
    if code == Some(ERROR_ACCESS_DENIED) {
        SendFailure::Uipi
    } else {
        SendFailure::Other
    }
}

/// 发送一组 INPUT；全部成功返回 `Ok`，否则记录错误并分类。
fn send_all(inputs: &[Input]) -> Result<(), SendFailure> {
    if inputs.is_empty() {
        return Ok(());
    }
    let count = inputs.len().min(u32::MAX as usize) as u32;
    // 先清零，失败时读到的才是 SendInput 自己设置的状态，而不是上一条残留。
    unsafe { SetLastError(0) };
    let sent = unsafe { SendInput(count, inputs.as_ptr(), size_of::<Input>() as i32) };
    if sent == count {
        return Ok(());
    }
    // 必须在任何日志/格式化之前取错误码，避免被中间调用覆盖。
    let error = std::io::Error::last_os_error();
    let failure = classify_send_error(error.raw_os_error());
    match failure {
        SendFailure::Uipi => tracing::warn!(
            sent,
            expected = count,
            "SendInput 被 UIPI 拦截：前台窗口的完整性级别高于被控端"
        ),
        SendFailure::Other => tracing::warn!(
            sent,
            expected = count,
            error = %error,
            "SendInput 未完全发送"
        ),
    }
    Err(failure)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- 按下状态

/// 当前由本模块按下、尚未释放的鼠标键与键盘扫描码。
struct Pressed {
    /// 顺序固定为 Left/Right/Middle/X1/X2。
    mouse: [bool; 5],
    /// (扫描码, 是否扩展键)。
    keys: Vec<(u16, bool)>,
}

impl Pressed {
    const fn new() -> Self {
        Self {
            mouse: [false; 5],
            keys: Vec::new(),
        }
    }

    fn set_key(&mut self, scan: u16, ext: bool, down: bool) {
        if down {
            if !self.keys.iter().any(|&(s, e)| s == scan && e == ext) {
                self.keys.push((scan, ext));
            }
        } else {
            self.keys.retain(|&(s, e)| !(s == scan && e == ext));
        }
    }
}

static PRESSED: Mutex<Pressed> = Mutex::new(Pressed::new());

/// 输入锁中毒时继续使用内部状态，绝不 panic。
fn pressed() -> MutexGuard<'static, Pressed> {
    PRESSED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------- UIPI 状态

/// 需要上报给主控端的 UIPI 状态变化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UipiReport {
    /// 刚刚检测到（或持续处于）输入被 UIPI 拦截。
    Blocked,
    /// 拦截已解除，输入恢复正常。
    Cleared,
}

#[derive(Default)]
struct UipiState {
    blocked: bool,
    last_report: Option<Instant>,
}

static UIPI: Mutex<UipiState> = Mutex::new(UipiState {
    blocked: false,
    last_report: None,
});

/// 单批注入的结果汇总：本批是否遇到 UIPI、是否至少成功过一次。
#[derive(Default)]
struct InjectTally {
    blocked: bool,
    succeeded: bool,
}

impl InjectTally {
    fn record(&mut self, result: Result<(), SendFailure>) {
        match result {
            Ok(()) => self.succeeded = true,
            Err(SendFailure::Uipi) => self.blocked = true,
            Err(SendFailure::Other) => {}
        }
    }
}

/// 根据本批结果推进 UIPI 状态机；只在状态变化或持续拦截到达重报间隔时返回。
fn advance_uipi(
    state: &mut UipiState,
    blocked: bool,
    succeeded: bool,
    now: Instant,
) -> Option<UipiReport> {
    if blocked {
        let due = !state.blocked
            || state
                .last_report
                .is_none_or(|last| now.duration_since(last) >= UIPI_REPORT_INTERVAL);
        if due {
            state.blocked = true;
            state.last_report = Some(now);
            return Some(UipiReport::Blocked);
        }
        return None;
    }
    // 只有真正成功发送过才敢解除：空批次或全是其它错误时保持原状态。
    if succeeded && state.blocked {
        state.blocked = false;
        state.last_report = None;
        return Some(UipiReport::Cleared);
    }
    None
}

fn uipi_state() -> MutexGuard<'static, UipiState> {
    UIPI.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------- 注入

/// 注入一批输入事件。非法事件静默跳过，绝不 panic。
///
/// `region` 是主控端当前看到的显示器；绝对坐标按这块屏幕映射，
/// 这样切换显示器后点击仍落在画面上的同一相对位置。
///
/// 返回值只在 UIPI 状态变化（或持续拦截到重报间隔）时非空，
/// 由调用方决定是否通过协议通知主控端。
pub fn inject(events: &[InputEvent], region: Option<MonitorRect>) -> Option<UipiReport> {
    if events.is_empty() {
        return None;
    }
    tracing::debug!(count = events.len(), "注入远程输入");
    if events.len() > MAX_EVENTS_PER_BATCH {
        tracing::warn!(
            received = events.len(),
            limit = MAX_EVENTS_PER_BATCH,
            "输入批次超过上限，多余事件已丢弃"
        );
    }

    let mut state = pressed();
    let mut tally = InjectTally::default();
    for event in events.iter().take(MAX_EVENTS_PER_BATCH) {
        apply(event, &mut state, region, &mut tally);
    }
    drop(state);

    advance_uipi(
        &mut uipi_state(),
        tally.blocked,
        tally.succeeded,
        Instant::now(),
    )
}

/// 释放本模块当前按下的全部鼠标键和键盘键，并清空跟踪状态。
pub fn release_all() {
    let mut state = pressed();
    let _ = release_locked(&mut state);
}

fn apply(
    event: &InputEvent,
    state: &mut Pressed,
    region: Option<MonitorRect>,
    tally: &mut InjectTally,
) {
    match event {
        InputEvent::Move { abs, rel } => match (abs, rel) {
            (Some(abs), _) => {
                let (x, y) = abs_to_screen(*abs, region);
                tally.record(send_all(&[mouse_input(x, y, 0, absolute_move_flags())]));
            }
            (None, Some(rel)) => {
                tally.record(send_all(&[mouse_input(
                    rel[0],
                    rel[1],
                    0,
                    MOUSEEVENTF_MOVE,
                )]));
            }
            (None, None) => {}
        },
        InputEvent::Button { button, down, abs } => {
            let (flags, data) = button_flags(*button, *down);
            let mut inputs = Vec::with_capacity(2);
            // 先定位再按下，避免点击位置漂移。
            if *down {
                if let Some(abs) = abs {
                    let (x, y) = abs_to_screen(*abs, region);
                    inputs.push(mouse_input(x, y, 0, absolute_move_flags()));
                }
            }
            inputs.push(mouse_input(0, 0, data, flags));
            let result = send_all(&inputs);
            if result.is_ok() {
                if let Some(slot) = button_slot(*button) {
                    state.mouse[slot] = *down;
                }
            }
            tally.record(result);
        }
        InputEvent::Wheel { delta } => {
            let mut inputs = Vec::with_capacity(2);
            if delta[0] != 0 {
                inputs.push(mouse_input(0, 0, delta[0] as u32, MOUSEEVENTF_HWHEEL));
            }
            // 前端发送 ±120 的整数倍；正值表示向上滚动。
            if delta[1] != 0 {
                inputs.push(mouse_input(0, 0, delta[1] as u32, MOUSEEVENTF_WHEEL));
            }
            if !inputs.is_empty() {
                tally.record(send_all(&inputs));
            }
        }
        InputEvent::Key { scan, down, ext } => {
            if *scan == 0 {
                tracing::debug!("忽略扫描码为 0 的按键事件");
                return;
            }
            let mut flags = KEYEVENTF_SCANCODE;
            if *ext {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !*down {
                flags |= KEYEVENTF_KEYUP;
            }
            let result = send_all(&[key_input(0, *scan, flags)]);
            if result.is_ok() {
                state.set_key(*scan, *ext, *down);
            }
            tally.record(result);
        }
        InputEvent::Text { text } => {
            let mut units: Vec<u16> = text.encode_utf16().collect();
            if units.is_empty() {
                return;
            }
            if units.len() > MAX_TEXT_UNITS {
                tracing::warn!(
                    units = units.len(),
                    limit = MAX_TEXT_UNITS,
                    "文本输入过长，已截断"
                );
                units.truncate(MAX_TEXT_UNITS);
            }
            // 每个 UTF-16 code unit 各发一次按下/抬起；vk 必须为 0。
            let units_per_call = MAX_INPUTS_PER_CALL / 2;
            for chunk in units.chunks(units_per_call) {
                let mut inputs = Vec::with_capacity(chunk.len() * 2);
                for &unit in chunk {
                    inputs.push(key_input(0, unit, KEYEVENTF_UNICODE));
                    inputs.push(key_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
                tally.record(send_all(&inputs));
            }
        }
        InputEvent::ReleaseAll => {
            if let Some(result) = release_locked(state) {
                tally.record(result);
            }
        }
    }
}

/// 释放已跟踪的按键。状态先清空再发送，失败也不会重复释放同一按键。
/// 返回 `None` 表示没有需要释放的按键。
fn release_locked(state: &mut Pressed) -> Option<Result<(), SendFailure>> {
    let mut inputs = Vec::new();

    for (slot, down) in state.mouse.iter_mut().enumerate() {
        if !*down {
            continue;
        }
        *down = false;
        let Some(button) = slot_button(slot) else {
            continue;
        };
        let (flags, data) = button_flags(button, false);
        inputs.push(mouse_input(0, 0, data, flags));
    }

    for &(scan, ext) in &state.keys {
        let mut flags = KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP;
        if ext {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        inputs.push(key_input(0, scan, flags));
    }
    state.keys.clear();

    if inputs.is_empty() {
        None
    } else {
        Some(send_all(&inputs))
    }
}

// ---------------------------------------------------------------- Ctrl+Alt+Del

/// `SendSAS` 不可用的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SasFallback {
    /// 系统没有 `sas.dll`，或该 DLL 未导出 `SendSAS`。
    NoDll,
    /// `SoftwareSASGeneration` 策略不允许当前身份生成 SAS。
    Policy,
}

impl SasFallback {
    fn detail(self) -> &'static str {
        match self {
            Self::NoDll => "被控端没有可用的 sas.dll!SendSAS",
            Self::Policy => {
                "系统策略 SoftwareSASGeneration 未允许当前身份发送 SAS（需要服务/SYSTEM 或策略放行）"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SasPlan {
    Send,
    Fallback(SasFallback),
}

/// 决定是否调用 `SendSAS`。`policy` 是 SoftwareSASGeneration 的值（缺失视为 0）。
fn sas_plan(available: bool, policy: Option<u32>, system: bool) -> SasPlan {
    if !available {
        return SasPlan::Fallback(SasFallback::NoDll);
    }
    // 0 = 谁都不许，1 = 只允许服务，2 = 只允许应用，3 = 两者都允许。
    // 按项目约定，SYSTEM 身份在策略为 0/1/缺失/未知值时仍尝试；其余值按最保守处理。
    let allowed = matches!(policy, Some(2 | 3)) || system;
    if allowed {
        SasPlan::Send
    } else {
        SasPlan::Fallback(SasFallback::Policy)
    }
}

/// 加载 `sas.dll` 并取 `SendSAS` 地址；结果缓存，避免每次请求都 LoadLibrary。
fn send_sas() -> Option<SendSasFn> {
    static SEND_SAS: OnceLock<Option<SendSasFn>> = OnceLock::new();
    *SEND_SAS.get_or_init(|| unsafe {
        let module = LoadLibraryW(wide("sas.dll").as_ptr());
        if module == 0 {
            return None;
        }
        let proc = GetProcAddress(module, c"SendSAS".as_ptr().cast());
        if proc.is_null() {
            return None;
        }
        // SAFETY: 地址来自 sas.dll 的导出表，签名与官方 `void SendSAS(BOOL)` 一致。
        Some(std::mem::transmute::<*const c_void, SendSasFn>(proc))
    })
}

/// 读取 `HKLM\...\Policies\System\SoftwareSASGeneration`。
fn software_sas_generation() -> Option<u32> {
    let sub_key = wide("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System");
    let value = wide("SoftwareSASGeneration");
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    let mut kind = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub_key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut kind,
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (status == 0).then_some(data)
}

/// 当前账户是否 LocalSystem。SYSTEM 身份不需要 SoftwareSASGeneration 放行。
fn current_user_is_system() -> bool {
    let mut buffer = [0u16; 64];
    let mut len = buffer.len() as u32;
    if unsafe { GetUserNameW(buffer.as_mut_ptr(), &mut len) } == 0 {
        return false;
    }
    // 成功时 len 含结尾 NUL。
    let end = (len as usize).saturating_sub(1).min(buffer.len());
    String::from_utf16_lossy(&buffer[..end]).eq_ignore_ascii_case("SYSTEM")
}

/// 降级方案：把 Ctrl、Alt、Delete 作为普通按键注入（无法进入安全桌面）。
fn send_ctrl_alt_del() -> Result<(), SendFailure> {
    send_all(&[
        key_input(VK_CONTROL, 0, 0),
        key_input(VK_MENU, 0, 0),
        key_input(VK_DELETE, 0, KEYEVENTF_EXTENDEDKEY),
        key_input(VK_DELETE, 0, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
        key_input(VK_MENU, 0, KEYEVENTF_KEYUP),
        key_input(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ])
}

/// 处理主控端的 Ctrl+Alt+Del 请求。
///
/// 优先调用 `SendSAS`；策略/权限不允许时回退为普通按键，并把原因一起返回。
/// `SendSAS` 本身没有返回值，因此只能确认「已调用」，无法确认系统是否接受。
pub fn secure_attention(as_user: bool) -> (SasMethod, Option<String>) {
    /// 回退到普通按键，并拼出给主控端的说明。
    fn fallback(reason: &str) -> (SasMethod, Option<String>) {
        let detail = match send_ctrl_alt_del() {
            Ok(()) => format!("{reason}；已回退为普通 Ctrl+Alt+Del 按键（只能作用于普通应用）"),
            Err(SendFailure::Uipi) => {
                format!("{reason}；回退按键也被 UIPI 拦截（前台有提权窗口）")
            }
            Err(SendFailure::Other) => format!("{reason}；回退按键注入失败"),
        };
        tracing::warn!(%detail, "SendSAS 不可用，已回退为普通按键");
        (SasMethod::Keys, Some(detail))
    }

    let plan = sas_plan(
        send_sas().is_some(),
        software_sas_generation(),
        current_user_is_system(),
    );
    match plan {
        SasPlan::Send => {
            let Some(send) = send_sas() else {
                return fallback("sas.dll!SendSAS 在调用前不可用");
            };
            let as_user_flag = if as_user { 1 } else { 0 };
            // SAFETY: 函数指针来自 sas.dll 导出，参数是 BOOL。
            unsafe { send(as_user_flag) };
            tracing::info!(as_user, "已调用 SendSAS 生成安全注意序列");
            (SasMethod::Sas, None)
        }
        SasPlan::Fallback(reason) => fallback(reason.detail()),
    }
}

// ---------------------------------------------------------------- 映射

fn abs_to_screen(abs: [f32; 2], region: Option<MonitorRect>) -> (i32, i32) {
    match region {
        Some(region) if !region.is_empty() => map_abs(region, virtual_desk(), abs),
        _ => (normalize(abs[0]), normalize(abs[1])),
    }
}

/// 当前虚拟桌面的矩形（所有显示器拼成的包围盒）。
fn virtual_desk() -> MonitorRect {
    unsafe {
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w > 0 && h > 0 {
            return MonitorRect {
                x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                w: w as u32,
                h: h as u32,
            };
        }
        MonitorRect {
            x: 0,
            y: 0,
            w: GetSystemMetrics(SM_CXSCREEN).max(1) as u32,
            h: GetSystemMetrics(SM_CYSCREEN).max(1) as u32,
        }
    }
}

/// 把显示器内的 0..1 坐标换算成相对整个虚拟桌面的 0..1，再映射到 0..65535。
fn map_abs(region: MonitorRect, desk: MonitorRect, abs: [f32; 2]) -> (i32, i32) {
    if desk.is_empty() {
        return (normalize(abs[0]), normalize(abs[1]));
    }
    let x = region.x as f64 - desk.x as f64 + clamp_unit(abs[0]) as f64 * region.w as f64;
    let y = region.y as f64 - desk.y as f64 + clamp_unit(abs[1]) as f64 * region.h as f64;
    (
        normalize_ratio(x / desk.w as f64),
        normalize_ratio(y / desk.h as f64),
    )
}

/// 0..1 归一化坐标映射到 0..65535；配合 VIRTUALDESK 时覆盖整个虚拟桌面，
/// 非有限值按 0 处理。虚拟桌面原点就是归一化零点，因此直接乘 65535。
fn normalize(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    (value.clamp(0.0, 1.0) * 65535.0).round() as i32
}

fn normalize_ratio(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    (value.clamp(0.0, 1.0) * 65535.0).round() as i32
}

fn clamp_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn button_flags(button: MouseButton, down: bool) -> (u32, u32) {
    match button {
        MouseButton::Left => (
            if down {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
            0,
        ),
        MouseButton::Right => (
            if down {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
            0,
        ),
        MouseButton::Middle => (
            if down {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
            0,
        ),
        MouseButton::X1 => (
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON1,
        ),
        MouseButton::X2 => (
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON2,
        ),
    }
}

fn button_slot(button: MouseButton) -> Option<usize> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Right => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::X1 => Some(3),
        MouseButton::X2 => Some(4),
    }
}

fn slot_button(slot: usize) -> Option<MouseButton> {
    match slot {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Right),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::X1),
        4 => Some(MouseButton::X2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_coordinates_clamp() {
        assert_eq!(normalize(0.0), 0);
        assert_eq!(normalize(1.0), 65535);
        assert_eq!(normalize(-1.0), 0);
        assert_eq!(normalize(2.0), 65535);
        assert_eq!(normalize(f32::NAN), 0);
        assert_eq!(normalize(f32::INFINITY), 0);
        assert_eq!(normalize(0.5), 32768);
    }

    #[test]
    fn input_layout_is_40_bytes() {
        assert_eq!(size_of::<Input>(), 40);
    }

    #[test]
    fn button_flags_for_x_buttons_use_mouse_data() {
        assert_eq!(button_flags(MouseButton::X1, true), (MOUSEEVENTF_XDOWN, 1));
        assert_eq!(button_flags(MouseButton::X2, false), (MOUSEEVENTF_XUP, 2));
    }

    #[test]
    fn abs_maps_into_selected_display() {
        let desk = MonitorRect {
            x: 0,
            y: 0,
            w: 3840,
            h: 1080,
        };
        let right = MonitorRect {
            x: 1920,
            y: 0,
            w: 1920,
            h: 1080,
        };
        // 右屏中心 = 整块虚拟桌面的 75% 处。
        assert_eq!(map_abs(right, desk, [0.5, 0.5]), (49151, 32768));
        assert_eq!(map_abs(right, desk, [0.0, 0.0]), (32768, 0));
    }

    #[test]
    fn access_denied_is_classified_as_uipi() {
        assert_eq!(
            classify_send_error(Some(ERROR_ACCESS_DENIED)),
            SendFailure::Uipi
        );
        assert_eq!(classify_send_error(Some(0)), SendFailure::Other);
        assert_eq!(classify_send_error(None), SendFailure::Other);
    }

    #[test]
    fn uipi_state_reports_transitions_and_throttles() {
        let t0 = Instant::now();
        let mut state = UipiState::default();
        // 首次拦截立即上报。
        assert_eq!(
            advance_uipi(&mut state, true, false, t0),
            Some(UipiReport::Blocked)
        );
        // 持续拦截在间隔内不重复上报。
        assert_eq!(
            advance_uipi(&mut state, true, false, t0 + Duration::from_secs(1)),
            None
        );
        // 到达重报间隔后再报一次，提醒主控端仍然被拦。
        assert_eq!(
            advance_uipi(
                &mut state,
                true,
                false,
                t0 + UIPI_REPORT_INTERVAL + Duration::from_secs(1)
            ),
            Some(UipiReport::Blocked)
        );
        // 没成功发送过（例如全是其它错误）不能解除。
        assert_eq!(
            advance_uipi(
                &mut state,
                false,
                false,
                t0 + UIPI_REPORT_INTERVAL + Duration::from_secs(2)
            ),
            None
        );
        // 真正发送成功后才上报解除。
        assert_eq!(
            advance_uipi(
                &mut state,
                false,
                true,
                t0 + UIPI_REPORT_INTERVAL + Duration::from_secs(3)
            ),
            Some(UipiReport::Cleared)
        );
        assert!(!state.blocked);
        // 正常输入不会反复上报。
        assert_eq!(
            advance_uipi(
                &mut state,
                false,
                true,
                t0 + UIPI_REPORT_INTERVAL + Duration::from_secs(4)
            ),
            None
        );
    }

    #[test]
    fn sas_plan_requires_dll_and_permission() {
        // 没有 sas.dll 只能降级。
        assert_eq!(
            sas_plan(false, Some(3), true),
            SasPlan::Fallback(SasFallback::NoDll)
        );
        // 策略 2/3 明确允许应用。
        assert_eq!(sas_plan(true, Some(2), false), SasPlan::Send);
        assert_eq!(sas_plan(true, Some(3), false), SasPlan::Send);
        // 策略 1 只服务；SYSTEM 视为服务身份。
        assert_eq!(sas_plan(true, Some(1), true), SasPlan::Send);
        assert_eq!(
            sas_plan(true, Some(1), false),
            SasPlan::Fallback(SasFallback::Policy)
        );
        // 策略 0 / 缺失 / 未知值：只有 SYSTEM 才尝试。
        assert_eq!(
            sas_plan(true, Some(0), false),
            SasPlan::Fallback(SasFallback::Policy)
        );
        assert_eq!(
            sas_plan(true, None, false),
            SasPlan::Fallback(SasFallback::Policy)
        );
        assert_eq!(
            sas_plan(true, Some(9), false),
            SasPlan::Fallback(SasFallback::Policy)
        );
        assert_eq!(sas_plan(true, Some(0), true), SasPlan::Send);
    }

    #[test]
    fn abs_maps_negative_virtual_origin() {
        let desk = MonitorRect {
            x: -1920,
            y: 0,
            w: 3840,
            h: 1080,
        };
        let left = MonitorRect {
            x: -1920,
            y: 0,
            w: 1920,
            h: 1080,
        };
        assert_eq!(map_abs(left, desk, [0.5, 0.5]), (16384, 32768));
        // 非有限值按左上角处理，不产生 NaN。
        assert_eq!(map_abs(left, desk, [f32::NAN, f32::INFINITY]), (0, 0));
    }
}
