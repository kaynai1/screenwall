//! Windows 防火墙规则：给局域网直连放行入站 TCP。
//!
//! 规则**只放行 ScreenWall 自己的可执行文件**（`program=`），不是把端口
//! 对整个系统开放。两条落地路径：
//!
//! - perMachine 安装：NSIS 安装钩子（`crates/client/windows/installer-hooks.nsh`）
//!   在装完后直接调用 `netsh` 加规则，卸载时删除；
//! - 兜底：currentUser 安装、规则被用户/组策略删掉，或程序换过安装目录时，
//!   设置页的「允许局域网直连」按钮会以管理员身份重跑本程序的内置子命令
//!   （一次 UAC），再做一次「删除同名规则 + 按当前 exe/端口重建」。
//!
//! 检测与修复都以 `netsh advfirewall firewall` 为准，不依赖 PowerShell，
//! 这样在受限环境里也能工作。

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

/// 规则名。必须与 `windows/installer-hooks.nsh` 里的定义保持一致。
pub const RULE_NAME: &str = "ScreenWall Direct";
/// 自动发现（UDP 广播应答）的规则名。
pub const DISCOVERY_RULE_NAME: &str = "ScreenWall Discovery";
/// 自动发现用的 UDP 端口。
pub const DISCOVERY_PORT: u16 = screenwall_agent::discovery::DISCOVERY_PORT;
/// 以管理员身份重跑自身时用的命令行开关。
pub const HELPER_FLAG: &str = "--screenwall-firewall-rule";
/// 探测/修复等一次 UAC 的最长时间。
const ELEVATION_TIMEOUT: Duration = Duration::from_secs(60);
/// 不弹控制台窗口。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 给界面看的防火墙状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirewallState {
    /// 名称匹配的规则是否存在。
    pub rule_present: bool,
    /// 规则是否覆盖当前可执行文件与监听端口（存在但指向旧路径/旧端口时为 false）。
    pub rule_matches: bool,
    /// 规则名，界面提示用。
    pub rule_name: String,
    /// 当前监听端口。
    pub port: u16,
}

/// 当前状态。只读，不需要管理员权限。
pub fn state(port: u16) -> FirewallState {
    let detail = show_rule(RULE_NAME);
    let present = detail.is_some();
    // 直连要两条规则都齐：TCP 收控制连接，UDP 回答自动发现探测。
    // 缺 UDP 时直连本身能用，但别的机器扫不到，所以也算「未匹配」。
    let matches = detail
        .as_deref()
        .map(|text| rule_covers(text, port))
        .unwrap_or(false)
        && show_rule(DISCOVERY_RULE_NAME)
            .as_deref()
            .map(|text| rule_covers(text, DISCOVERY_PORT))
            .unwrap_or(false);
    FirewallState {
        rule_present: present,
        rule_matches: matches,
        rule_name: RULE_NAME.to_string(),
        port,
    }
}

/// 确保规则存在且指向当前程序/端口。
///
/// 先尝试直接执行（程序本身已提权时能成功）；否则以管理员身份重跑自身
/// 的 [`HELPER_FLAG`] 子命令，并轮询到规则生效为止。
pub fn ensure(port: u16) -> Result<FirewallState, String> {
    if state(port).rule_matches {
        return Ok(state(port));
    }
    // 程序恰好以管理员身份运行时，这一步就完成了，不会弹 UAC。
    if upsert(port).is_ok() && state(port).rule_matches {
        return Ok(state(port));
    }

    request_elevated(port)?;
    let deadline = Instant::now() + ELEVATION_TIMEOUT;
    while Instant::now() < deadline {
        let current = state(port);
        if current.rule_matches {
            return Ok(current);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err("等待管理员授权超时，未检测到防火墙规则。请重试，或手动允许 ScreenWall 的入站连接。".into())
}

/// 给设置页读当前状态；只读 netsh，不需要提权。
#[tauri::command]
pub async fn get_direct_firewall_state(
    shared: tauri::State<'_, std::sync::Arc<crate::Shared>>,
) -> Result<FirewallState, String> {
    let port = shared.config.read().await.direct_port;
    tauri::async_runtime::spawn_blocking(move || state(port))
        .await
        .map_err(|error| format!("读取防火墙状态失败: {error}"))
}

/// 设置页的「允许局域网直连」按钮：补规则。必要时弹一次 UAC。
#[tauri::command]
pub async fn allow_direct_firewall(
    shared: tauri::State<'_, std::sync::Arc<crate::Shared>>,
) -> Result<FirewallState, String> {
    let port = shared.config.read().await.direct_port;
    tauri::async_runtime::spawn_blocking(move || ensure(port))
        .await
        .map_err(|error| format!("修复防火墙规则失败: {error}"))?
}

/// 内置子命令：以管理员身份重跑自身时只修规则，不启动界面。
///
/// 返回 `Some(exit_code)` 表示本次进程是防火墙修复子命令，调用方应直接退出。
pub fn run_helper_from_args() -> Option<i32> {
    let port = helper_port(std::env::args())?;
    let code = match upsert(port) {
        Ok(()) => 0,
        Err(error) => {
            tracing::warn!(%error, "防火墙规则修复失败");
            1
        }
    };
    Some(code)
}

/// 从命令行里识别 [`HELPER_FLAG`]；返回 `None` 表示不是修复子命令。
///
/// 端口缺失或非法时退回直连默认端口，避免参数写错就把规则加到 0 端口上。
pub fn helper_port(args: impl IntoIterator<Item = String>) -> Option<u16> {
    let mut iter = args.into_iter();
    // 第一个参数是可执行文件路径。
    iter.next();
    let mut helper = false;
    let mut port = None;
    for arg in iter {
        if arg == HELPER_FLAG {
            helper = true;
            continue;
        }
        if helper && port.is_none() {
            port = arg.parse::<u16>().ok().filter(|value| *value != 0);
        }
    }
    helper.then(|| port.unwrap_or(crate::direct::DEFAULT_PORT))
}

/// 删除同名规则再按当前 exe/端口重建。
///
/// 用「先删后加」而不是「存在就跳过」：升级或换安装目录后，旧的
/// `program=` 路径会失效；同名的多条规则也会让 `netsh show` 的结果变得
/// 难以判断。删除不存在的规则返回非 0，这里刻意忽略。
fn upsert(port: u16) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| format!("定位程序失败: {error}"))?;

    // 直连 TCP + 自动发现 UDP，两条规则同样的处理方式。
    add_rule(&exe, RULE_NAME, "TCP", port)?;
    add_rule(&exe, DISCOVERY_RULE_NAME, "UDP", DISCOVERY_PORT)?;
    Ok(())
}

/// 先删同名规则再按当前 exe/端口重建一条。
fn add_rule(exe: &std::path::Path, name: &str, protocol: &str, port: u16) -> Result<(), String> {
    let mut delete = netsh_command();
    delete
        .args(["advfirewall", "firewall", "delete", "rule"])
        .raw_arg(rule_arg(name));
    let _ = delete.output();

    let local_port = format!("localport={port}");
    let program = format!("program=\"{}\"", exe.display());
    let output = netsh_command()
        .args(["advfirewall", "firewall", "add", "rule"])
        .raw_arg(rule_arg(name))
        .args([
            "dir=in",
            "action=allow",
            &format!("protocol={protocol}"),
            local_port.as_str(),
            "profile=domain,private",
            "enable=yes",
        ])
        .raw_arg(&program)
        .output()
        .map_err(|error| format!("执行 netsh 失败: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("添加防火墙规则 {name} 失败（需要管理员权限）"))
    }
}

/// 读取规则详情（`verbose` 才有 Program / LocalPort 字段）。
///
/// 返回 `None` 表示没有匹配的规则或 netsh 执行失败。
fn show_rule(name: &str) -> Option<String> {
    let output: Output = netsh_command()
        .args(["advfirewall", "firewall", "show", "rule"])
        .raw_arg(rule_arg(name))
        .arg("verbose")
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

/// netsh 的命令行实例：隐藏控制台，stdin 不继承。
fn netsh_command() -> Command {
    let mut cmd = Command::new(netsh_path());
    cmd.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null());
    cmd
}

/// `name="ScreenWall Direct"`。netsh 自己解析命令行（不是标准 argv），
/// 所以用 `raw_arg` 原样带上引号，不依赖 `Command` 的自动转义。
fn rule_arg(name: &str) -> String {
    format!("name=\"{name}\"")
}

fn netsh_path() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("netsh.exe")
}

/// 规则详情是否覆盖当前程序和端口。
///
/// netsh 的字段名会随系统语言本地化，所以不解析标签，只找两个 ASCII 值：
/// 端口数字与当前 exe 文件名。两者都出现才认为匹配。
fn rule_covers(text: &str, port: u16) -> bool {
    let lower = text.to_lowercase();
    if !lower.contains(&port.to_string()) {
        return false;
    }
    match std::env::current_exe().ok().and_then(|exe| {
        exe.file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
    }) {
        Some(name) => lower.contains(&name),
        // 拿不到自身路径时只按端口判断，宁可认为已覆盖，避免反复弹 UAC。
        None => true,
    }
}

/// 以管理员身份重跑自身的内置子命令。
fn request_elevated(port: u16) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| format!("定位程序失败: {error}"))?;
    shell_execute_runas(&exe, &format!("{HELPER_FLAG} {port}"), true)
        .map_err(|error| format!("{error}，防火墙规则没有修改。"))
}

/// 用 `ShellExecuteW` 以管理员身份启动一个程序（触发 UAC）。
///
/// `hidden` 为 true 时不显示目标程序窗口，用于防火墙修复子命令；
/// 安装包需要显示自身的进度窗口，传 false。
/// 返回 `Err` 表示用户取消了 UAC，或被系统策略拒绝。
pub fn shell_execute_runas(
    file: &std::path::Path,
    parameters: &str,
    hidden: bool,
) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = file
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let parameters: Vec<u16> = format!("{parameters}\0").encode_utf16().collect();
    let show = if hidden { SW_HIDE } else { SW_SHOWNORMAL };
    // SAFETY: 三个字符串都以 NUL 结尾且在整个调用期间存活；ShellExecuteW
    // 只读取它们，不保留指针。
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            show,
        )
    };
    // ShellExecute 约定：返回值 <= 32 表示失败（含用户取消 UAC）。
    if result as isize <= 32 {
        Err("未获得管理员授权（UAC 被取消或被系统策略拒绝）".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| item.to_string()).collect()
    }

    /// 普通启动不能被误判成修复子命令。
    #[test]
    fn normal_launch_is_not_helper() {
        assert_eq!(helper_port(args(&["screenwall-client.exe"])), None);
        assert_eq!(
            helper_port(args(&["screenwall-client.exe", "--some-other-flag"])),
            None
        );
    }

    /// 带端口时按参数走。
    #[test]
    fn helper_reads_port() {
        assert_eq!(
            helper_port(args(&["screenwall-client.exe", HELPER_FLAG, "47900"])),
            Some(47900)
        );
    }

    /// 端口缺失或非法时退回默认端口，不会去操作 0 端口。
    #[test]
    fn helper_falls_back_to_default_port() {
        assert_eq!(
            helper_port(args(&["screenwall-client.exe", HELPER_FLAG])),
            Some(crate::direct::DEFAULT_PORT)
        );
        assert_eq!(
            helper_port(args(&["screenwall-client.exe", HELPER_FLAG, "0"])),
            Some(crate::direct::DEFAULT_PORT)
        );
        assert_eq!(
            helper_port(args(&["screenwall-client.exe", HELPER_FLAG, "not-a-port"])),
            Some(crate::direct::DEFAULT_PORT)
        );
    }

    /// netsh 自己解析命令行，规则名带空格必须原样带引号。
    #[test]
    fn rule_arg_keeps_quotes() {
        assert_eq!(rule_arg(RULE_NAME), "name=\"ScreenWall Direct\"");
        assert_eq!(
            rule_arg(DISCOVERY_RULE_NAME),
            "name=\"ScreenWall Discovery\""
        );
    }

    /// 规则匹配只看端口与 exe 文件名这两个 ASCII 值，不依赖本地化字段名。
    #[test]
    fn rule_match_uses_port_and_exe_name() {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "screenwall-client.exe".to_string());
        let text = format!(
            "Rule Name: ScreenWall Direct\nProgram: C:\\Program Files\\ScreenWall\\{exe}\nLocalPort: 47900\n"
        );
        assert!(rule_covers(&text, 47900));
        assert!(!rule_covers(&text, 48000));
        assert!(!rule_covers(
            "Rule Name: ScreenWall Direct\nLocalPort: 47900\n",
            47900
        ));
    }
}
