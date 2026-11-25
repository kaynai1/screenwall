//! 远程电源操作：重启 / 关机。
//!
//! 直接调用系统自带 `shutdown.exe`，延迟 3 秒执行，给 `power.ok` 留出回传时间。
//! 只应由已经通过授权弹窗的控制会话调用。

use anyhow::{Context, Result};
use screenwall_proto::PowerAction;
use std::process::Command;

/// 安排一次重启 / 关机。`shutdown.exe` 成功启动即返回 Ok。
pub fn schedule(action: PowerAction) -> Result<()> {
    Command::new("shutdown")
        .args(args(action))
        .spawn()
        .context("启动 shutdown.exe 失败")?;
    Ok(())
}

/// `shutdown` 的参数；单独拿出来便于单测。
fn args(action: PowerAction) -> [&'static str; 3] {
    match action {
        PowerAction::Restart => ["/r", "/t", "3"],
        PowerAction::Shutdown => ["/s", "/t", "3"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_actions_to_shutdown_args() {
        assert_eq!(args(PowerAction::Restart), ["/r", "/t", "3"]);
        assert_eq!(args(PowerAction::Shutdown), ["/s", "/t", "3"]);
    }
}
