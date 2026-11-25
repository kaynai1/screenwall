//! 系统托盘与看门狗。
//!
//! - 托盘菜单：打开主界面 / 允许被控开关 / 退出。
//! - 关闭窗口默认缩到托盘，只有菜单里的「退出」或 `exit_app` 才真正结束进程。
//! - 看门狗：主控/被控连接连续失败超过阈值时，重启两条会话任务（不重启进程），
//!   避免 WebSocket 半死状态下界面一直显示「连接中」。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::{restart_tasks, Shared};

pub const MENU_OPEN: &str = "tray-open";
pub const MENU_TOGGLE: &str = "tray-toggle-control";
pub const MENU_EXIT: &str = "tray-exit";

/// 真正退出：先停掉后台任务，再结束进程。
pub fn exit_app(app: &AppHandle) {
    if let Some(shared) = app.try_state::<Arc<Shared>>() {
        crate::stop_tasks(&shared);
    }
    app.exit(0);
}

/// 显示并聚焦主窗口；窗口被隐藏或最小化时都能拉回来。
pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn toggle_control(app: &AppHandle) {
    let Some(shared) = app.try_state::<Arc<Shared>>() else {
        return;
    };
    let next = !shared.allow_control.load(Ordering::Relaxed);
    shared.allow_control.store(next, Ordering::Relaxed);
    if !next {
        let mut pending = shared.pending_control_requests.lock().unwrap();
        for (_, request) in pending.drain() {
            let _ = request.reply.send(false);
        }
    }
    // 配置要持久化，和设置页的开关保持同一份状态。
    let shared = shared.inner().clone();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut config = shared.config.write().await;
        if config.allow_control != next {
            config.allow_control = next;
            if let Err(error) = config.save() {
                tracing::warn!(%error, "保存托盘开关失败");
            }
        }
        drop(config);
        let _ = app.emit("tray-allow-control", next);
        let _ = app.emit(
            "local-settings",
            serde_json::json!({ "allowControl": next }),
        );
    });
    tracing::info!("托盘切换允许被控: {next}");
}

/// 建托盘。失败只记日志，不阻止程序启动。
pub fn setup(app: &AppHandle<Wry>) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, MENU_OPEN, "打开主界面", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, MENU_TOGGLE, "允许远程控制", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, MENU_EXIT, "退出 ScreenWall", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &toggle, &separator, &quit])?;

    let mut builder = TrayIconBuilder::with_id("screenwall-tray")
        .tooltip("ScreenWall · 远程工作台")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_OPEN => show_main_window(app),
            MENU_TOGGLE => toggle_control(app),
            MENU_EXIT => exit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    // 复用 tauri.conf 里的窗口图标，避免再打包一份图片。
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}

/// 看门狗：每 30 秒检查一次连接健康度，连续失败或长时间未连上就重启会话任务。
///
/// 只重启「主控 + 被控」两条任务，不动群控/文件等用户正在用的会话；
/// 重启本身是幂等的（`restart_tasks` 会先 abort 再 spawn）。
pub async fn watchdog_loop(app: AppHandle, shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut unhealthy = 0u32;
    loop {
        ticker.tick().await;
        let config = shared.config.read().await.clone();
        if config.token.is_empty() || config.server_url.is_empty() {
            unhealthy = 0;
            continue;
        }
        let connected = *shared.connected.read().await;
        if connected {
            unhealthy = 0;
            continue;
        }
        unhealthy += 1;
        // 连续 4 次（约 2 分钟）都没连上，重启任务；仍在失败就继续每 2 分钟重启一次。
        if unhealthy >= 4 {
            tracing::warn!(unhealthy, "连接看门狗触发，重启会话任务");
            let _ = app.emit(
                "watchdog",
                serde_json::json!({ "action": "restart", "reason": "连接持续失败" }),
            );
            restart_tasks(app.clone(), shared.clone());
            unhealthy = 0;
        }
    }
}
