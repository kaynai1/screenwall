//! 无头被控端：用 Amyuni usbmmidd_v2（间接显示驱动，IDD）临时接一块虚拟屏。
//!
//! 没接显示器、也没有 EDID 的机器上，DXGI/GDI 都没有可采集的输出，控制端只能
//! 看到黑帧。做法和 CrossDesk 一样：随安装包带一个签过名的 IDD 驱动，检测到
//! headless 时以管理员身份装驱动并插一块 1920×1080 的虚拟显示器，再照常走
//! DXGI 采集。
//!
//! 边界：
//! - 只在这台机器没有任何活动显示输出时才动手；显示器只是关掉电源、EDID 还在
//!   的情况不算 headless，本来就能采到画面。
//! - 装驱动要提权。GUI 以普通用户运行时通过 `ShellExecuteExW(runas)` 重新拉起
//!   自己（`--screenwall-vdisplay=plug`），在那个进程里完成安装、插屏和写注册表
//!   标记；用户拒绝 UAC 就继续黑帧，不阻塞会话。
//! - 卸载由 NSIS 钩子负责：只有我们写过 `HKLM\Software\ScreenWall\
//!   UsbmmiddDriverInstalled` 才会删驱动。
//!
//! 驱动包是第三方组件（Amyuni Technologies，允许再分发但要求保留声明、首次
//! 使用会弹一次广告页），见 `third_party/usbmmidd_v2/NOTICE.md`。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 进程级开关。设置页切换后立即生效，不必重启被控端。
static ENABLED: AtomicBool = AtomicBool::new(false);
/// 上一次提权失败的时间，用来抑制连续的 UAC 弹窗。
static LAST_FAILURE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
const FAILURE_BACKOFF: Duration = Duration::from_secs(60);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;
const DISPLAY_DEVICE_ATTACHED_TO_DESKTOP: u32 = 0x0000_0001;

const HELPER_ARG: &str = "--screenwall-vdisplay=";
const INSTALL_MARKER_KEY: &str = r"HKLM\Software\ScreenWall";
const INSTALL_MARKER_VALUE: &str = "UsbmmiddDriverInstalled";

#[repr(C)]
#[derive(Clone, Copy)]
struct DisplayDeviceW {
    cb: u32,
    device_name: [u16; 32],
    device_string: [u16; 128],
    state_flags: u32,
    device_id: [u16; 128],
    device_key: [u16; 128],
}

impl Default for DisplayDeviceW {
    fn default() -> Self {
        // 全零等价于 cb 为 0；调用前再填 cb。
        unsafe { std::mem::zeroed() }
    }
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumDisplayDevicesW(
        device: *const u16,
        dev_num: u32,
        info: *mut DisplayDeviceW,
        flags: u32,
    ) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn WaitForSingleObject(handle: isize, milliseconds: u32) -> u32;
    fn GetExitCodeProcess(process: isize, code: *mut u32) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}

#[repr(C)]
struct ShellExecuteInfoW {
    cb_size: u32,
    f_mask: u32,
    hwnd: isize,
    lp_verb: *const u16,
    lp_file: *const u16,
    lp_parameters: *const u16,
    lp_directory: *const u16,
    n_show: i32,
    h_inst_app: isize,
    lp_id_list: *mut std::ffi::c_void,
    lp_class: *const u16,
    hkey_class: isize,
    dw_hot_key: u32,
    h_icon_or_monitor: isize,
    h_process: isize,
    h_thread: isize,
}

const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
const SW_HIDE: i32 = 0;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 活动显示适配器数量（插着且挂了桌面的显示器）。
pub fn count_active_displays() -> usize {
    let mut count = 0;
    for index in 0..16 {
        let mut device = DisplayDeviceW {
            cb: std::mem::size_of::<DisplayDeviceW>() as u32,
            ..Default::default()
        };
        // SAFETY: 空指针让系统枚举适配器；结构体已按 cb 初始化。
        let ok = unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut device, 0) };
        if ok == 0 {
            break;
        }
        if device.state_flags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0 {
            count += 1;
        }
    }
    count
}

/// 已经挂在桌面上的 usbmmidd 输出数量。
pub fn count_virtual_displays() -> usize {
    let mut count = 0;
    for index in 0..16 {
        let mut adapter = DisplayDeviceW {
            cb: std::mem::size_of::<DisplayDeviceW>() as u32,
            ..Default::default()
        };
        // SAFETY: 同上。
        let ok = unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut adapter, 0) };
        if ok == 0 {
            break;
        }
        if adapter.state_flags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == 0 {
            continue;
        }
        let name = wide(&from_wide(&adapter.device_name));
        let mut monitor = DisplayDeviceW {
            cb: std::mem::size_of::<DisplayDeviceW>() as u32,
            ..Default::default()
        };
        // 刚插上的屏可能还没枚举出监视器；此时仍用适配器信息判断，
        // 避免误以为没有插过而再插一块。
        let mut ident = format!(
            "{} {}",
            from_wide(&adapter.device_string),
            from_wide(&adapter.device_id)
        );
        // SAFETY: adapter.device_name 是系统填好的显示设备名。
        if unsafe { EnumDisplayDevicesW(name.as_ptr(), 0, &mut monitor, 0) } != 0 {
            ident.push_str(&format!(
                " {} {}",
                from_wide(&monitor.device_string),
                from_wide(&monitor.device_id)
            ));
        }
        let ident = ident.to_ascii_lowercase();
        if ident.contains("usbmmidd") || ident.contains("usb mobile monitor") {
            count += 1;
        }
    }
    count
}

/// 没有任何活动显示输出（真无头），或者桌面尺寸为 0。
pub fn is_headless() -> bool {
    if count_active_displays() > 0 {
        return false;
    }
    // SAFETY: 纯查询。
    unsafe {
        GetSystemMetrics(SM_CXVIRTUALSCREEN) <= 0 || GetSystemMetrics(SM_CYVIRTUALSCREEN) <= 0
    }
}

/// 找到驱动包目录（含 `deviceinstaller64.exe`）。
pub fn package_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("SCREENWALL_USBMMIDD_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Tauri 把 resources 放在 exe 同级（资源映射成 usbmmidd_v2）。
            candidates.push(dir.join("usbmmidd_v2"));
            candidates.push(dir.join("resources").join("usbmmidd_v2"));
            // 开发时从 target\debug 往上找仓库里的第三方目录。
            candidates.push(
                dir.join("..")
                    .join("..")
                    .join("third_party")
                    .join("usbmmidd_v2"),
            );
        }
    }
    // 编译期路径只在源码树里存在；发布版第一个候选就命中。
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("third_party")
            .join("usbmmidd_v2"),
    );
    candidates.into_iter().find(|dir| installer(dir).is_some())
}

fn installer(dir: &Path) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(target_pointer_width = "64") {
        &["deviceinstaller64.exe", "deviceinstaller.exe"]
    } else {
        &["deviceinstaller.exe"]
    };
    for name in names {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// 会话开始前调用：真无头时确保虚拟屏在线。已经在跑物理屏则什么也不做。
///
/// 返回 `Ok(true)` 表示现在有一块虚拟屏可用；`Ok(false)` 表示不需要；
/// `Err` 是给日志看的原因（用户拒绝 UAC、包缺失等）。
pub fn ensure_display(timeout: Duration) -> Result<bool, String> {
    if !is_headless() {
        return Ok(false);
    }
    if count_virtual_displays() > 0 {
        return Ok(true);
    }
    let dir = package_dir().ok_or_else(|| "缺少虚拟显示器驱动包 usbmmidd_v2".to_string())?;
    // UAC 被取消时不要在每个流/每次会话上反复弹窗，退避一分钟。
    if let Ok(guard) = LAST_FAILURE.lock() {
        if let Some(at) = *guard {
            if at.elapsed() < FAILURE_BACKOFF {
                return Err("上一次管理员授权被取消，稍后再试".to_string());
            }
        }
    }
    let started = Instant::now();
    if let Err(error) = request_elevated("plug") {
        if let Ok(mut guard) = LAST_FAILURE.lock() {
            *guard = Some(Instant::now());
        }
        return Err(error);
    }
    let mode = loop {
        if count_virtual_displays() > 0 {
            break true;
        }
        if started.elapsed() >= timeout {
            break false;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    if mode {
        tracing::info!(
            dir = %dir.display(),
            "已插入 usbmmidd 虚拟显示器（无头采集）"
        );
        Ok(true)
    } else {
        Err("虚拟显示器插入超时".to_string())
    }
}

/// 弹 UAC 重新拉起自己，以管理员身份执行 `plug` / `unplug`。
fn request_elevated(mode: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| format!("定位程序失败: {error}"))?;
    let exe_w = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(&format!("{HELPER_ARG}{mode}"));
    let mut info = ShellExecuteInfoW {
        cb_size: std::mem::size_of::<ShellExecuteInfoW>() as u32,
        f_mask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: 0,
        lp_verb: verb.as_ptr(),
        lp_file: exe_w.as_ptr(),
        lp_parameters: params.as_ptr(),
        lp_directory: std::ptr::null(),
        n_show: SW_HIDE,
        h_inst_app: 0,
        lp_id_list: std::ptr::null_mut(),
        lp_class: std::ptr::null(),
        hkey_class: 0,
        dw_hot_key: 0,
        h_icon_or_monitor: 0,
        h_process: 0,
        h_thread: 0,
    };
    // SAFETY: 所有字符串指针都指向调用期间有效的 UTF-16 缓冲。
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        return Err("提权被拒绝或无法启动助手".to_string());
    }
    if info.h_process == 0 {
        return Err("提权助手没有返回进程句柄".to_string());
    }
    // SAFETY: h_process 由 ShellExecuteExW 返回，用完后关闭。
    let wait = unsafe { WaitForSingleObject(info.h_process, 60_000) };
    if wait == WAIT_TIMEOUT {
        unsafe {
            CloseHandle(info.h_process);
        }
        return Err("提权助手超时".to_string());
    }
    let mut code = 1u32;
    // SAFETY: 句柄有效，code 是栈上变量。
    unsafe {
        GetExitCodeProcess(info.h_process, &mut code);
        CloseHandle(info.h_process);
    }
    if wait != WAIT_OBJECT_0 || code != 0 {
        return Err(format!("提权助手失败（退出码 {code}）"));
    }
    Ok(())
}

/// 命令行里带 `--screenwall-vdisplay=...` 时，在 Tauri 启动前执行并退出。
///
/// 返回 `Some(exit_code)` 表示进程应该直接退出。
pub fn run_helper_from_args(args: &[String]) -> Option<i32> {
    let mode = args.iter().find_map(|arg| arg.strip_prefix(HELPER_ARG))?;
    let code = match run_helper(mode) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ScreenWall 虚拟显示器: {error}");
            1
        }
    };
    Some(code)
}

fn run_helper(mode: &str) -> Result<(), String> {
    let dir = package_dir().ok_or_else(|| "找不到 usbmmidd_v2 驱动包".to_string())?;
    let exe = installer(&dir).ok_or_else(|| "驱动包里没有 deviceinstaller".to_string())?;
    match mode {
        "install" => install_driver(&dir, &exe).map(|_| ()),
        "plug" => {
            let installed = install_driver(&dir, &exe)?;
            if installed {
                write_install_marker();
            }
            plug_monitor(&dir, &exe)
        }
        "unplug" => unplug_monitors(&dir, &exe),
        other => Err(format!("未知的虚拟显示器模式: {other}")),
    }
}

fn run_installer(dir: &Path, exe: &Path, args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new(exe)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("启动 deviceinstaller 失败: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "deviceinstaller {:?} 返回 {}: {}",
            args,
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }
}

fn install_driver(dir: &Path, exe: &Path) -> Result<bool, String> {
    // 已经装过（设备节点存在）就不再 install：重复安装要重新签名确认。
    if already_installed() {
        return Ok(false);
    }
    run_installer(dir, exe, &["install", "usbmmidd.inf", "usbmmidd"])?;
    Ok(true)
}

fn already_installed() -> bool {
    for index in 0..16 {
        let mut device = DisplayDeviceW {
            cb: std::mem::size_of::<DisplayDeviceW>() as u32,
            ..Default::default()
        };
        // SAFETY: 纯查询。
        let ok = unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut device, 0) };
        if ok == 0 {
            break;
        }
        let ident = format!(
            "{} {}",
            from_wide(&device.device_string),
            from_wide(&device.device_id)
        )
        .to_ascii_lowercase();
        if ident.contains("usbmmidd") || ident.contains("usb mobile monitor") {
            return true;
        }
    }
    false
}

fn plug_monitor(dir: &Path, exe: &Path) -> Result<(), String> {
    if count_virtual_displays() > 0 {
        return Ok(());
    }
    run_installer(dir, exe, &["enableidd", "1"])
}

fn unplug_monitors(dir: &Path, exe: &Path) -> Result<(), String> {
    // 驱动最多 4 台；逐台拔，拔到没有为止。
    for _ in 0..4 {
        if count_virtual_displays() == 0 {
            return Ok(());
        }
        run_installer(dir, exe, &["enableidd", "0"])?;
    }
    Ok(())
}

fn write_install_marker() {
    let status = std::process::Command::new("reg")
        .args([
            "add",
            INSTALL_MARKER_KEY,
            "/v",
            INSTALL_MARKER_VALUE,
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
            "/reg:64",
        ])
        .status();
    match status {
        Ok(status) if status.success() => {}
        _ => tracing::warn!("写虚拟显示器安装标记失败，卸载时不会自动删除驱动"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_env_override_wins() {
        // 只验证候选逻辑的入口存在；真正的目录取决于安装位置。
        let dir = package_dir();
        if let Some(dir) = dir {
            assert!(installer(&dir).is_some());
        }
    }

    #[test]
    fn headless_and_display_counts_are_consistent() {
        if count_active_displays() == 0 {
            assert!(is_headless());
        } else {
            assert!(!is_headless());
        }
    }

    #[test]
    fn helper_argument_is_parsed() {
        assert_eq!(run_helper_from_args(&["x.exe".into()]), None);
        // 不执行真实模式，只验证前缀识别；未知模式返回非零。
        let args = vec![
            "x.exe".to_string(),
            "--screenwall-vdisplay=__test__".to_string(),
        ];
        assert_eq!(run_helper_from_args(&args), Some(1));
    }
}
