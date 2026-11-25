//! 服务器管理面板（kaypanel）的 sidecar 生命周期与内嵌 webview。
//!
//! kaypanel 是一个独立编译的 Go 二进制，作为 ScreenWall 的 sidecar 运行：
//! 只监听回环随机端口，启动后往 stdout 打一行握手 JSON，父进程据此拿到
//! 地址与一次性登录令牌。stdin 是父进程持有的管道，父进程退出或主动关闭
//! 时它读到 EOF 自行退出，不会留下孤儿进程。
//!
//! 界面不是另开应用，而是作为**主窗口里的子 webview** 贴在内容区：
//! 左侧栏点「服务器管理」时把它显示出来并铺满内容区，切到别的页面就隐藏。
//! 这样两套功能共享同一个窗口、同一个任务栏条目、同一个托盘。
//!
//! 之所以不用 iframe：kaypanel 自己带了 `X-Frame-Options: DENY` 与
//! `SameSite=Strict`，iframe 必然白屏。子 webview 是独立的浏览上下文，
//! 不受这两条限制，也不会和主界面的 CSS/JS 互相污染。

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{webview::WebviewBuilder, AppHandle, Manager, State, Webview, WebviewUrl};

/// sidecar 启动后回报的握手信息。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Handshake {
    pub url: String,
    pub token: String,
    pub pid: u32,
}

/// 运行中的面板进程。持有 `stdin` 句柄是刻意为之：
/// 它一旦被 drop（本进程退出或面板被关闭），子进程就会读到 EOF 并退出。
struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    handshake: Handshake,
}

#[derive(Default)]
pub struct Hub {
    running: Mutex<Option<Running>>,
    /// 内嵌 webview 的 label（建过一次就复用）。
    embedded: Mutex<Option<Webview>>,
}

/// 内嵌面板 webview 的 label。
const PANEL_WEBVIEW: &str = "kaypanel-embed";

/// 主窗口里内容区的几何位置由前端上报（避开侧边栏与顶栏）。
///
/// 单位是**前端视口的 CSS 像素**（`getBoundingClientRect()` 的原始值），
/// 不是物理像素、也不是窗口 DIP —— 两者会因为 DPI 缩放和界面缩放
/// （`set_zoom`）而不同。换算在 `to_logical` 里做。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// 前端的视口宽度（CSS 像素）。用来求 CSS→DIP 的换算系数。
    pub viewport_width: f64,
}

impl Bounds {
    /// 把 CSS 像素换算成窗口 DIP。
    ///
    /// `set_zoom` 只改变 WebView 的渲染缩放，窗口本身的 DIP 尺寸不变。
    /// 所以 `窗口 DIP 宽度 ÷ 视口 CSS 宽度` 就是当前实际的换算系数，
    /// 它同时吃掉了 DPI 缩放与界面缩放，不用分别判断。
    fn to_logical(self, window_dip_width: f64) -> (f64, f64, f64, f64) {
        let scale = if self.viewport_width > 1.0 && window_dip_width > 1.0 {
            window_dip_width / self.viewport_width
        } else {
            1.0
        };
        (
            self.x * scale,
            self.y * scale,
            self.width * scale,
            self.height * scale,
        )
    }
}

/// 启动（或复用）面板进程，返回可导航的自动登录 URL。
///
/// 进程已经在跑就直接返回原来的 URL —— 重复点击侧边栏不应该起第二个实例。
pub fn ensure_started(app: &AppHandle, hub: &Hub) -> Result<Handshake, String> {
    let mut guard = hub.running.lock().unwrap();

    // 复用仍在运行的实例。
    if let Some(running) = guard.as_mut() {
        match running.child.try_wait() {
            // 还活着。
            Ok(None) => return Ok(running.handshake.clone()),
            // 已经退出了：清掉，往下走重新拉起。
            Ok(Some(_)) | Err(_) => *guard = None,
        }
    }

    let exe = sidecar_path(app)?;
    let config = data_dir()?.join("kaypanel").join("config.yaml");
    if let Some(parent) = config.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建面板目录失败: {error}"))?;
    }

    let mut child = Command::new(&exe)
        .arg("-sidecar")
        .arg("-config")
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // 日志留在管道里会写满缓冲区卡住子进程，直接丢弃。
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("启动服务器管理面板失败: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "面板进程没有 stdout 管道".to_string())?;

    // 第一行就是握手 JSON。只读一行，之后不再占用管道。
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("读取面板握手信息失败: {error}"))?;

    let handshake: Handshake = serde_json::from_str(line.trim()).map_err(|error| {
        let _ = child.kill();
        let _ = child.wait();
        format!("面板握手信息无法解析: {error}")
    })?;

    let stdin = child.stdin.take();
    tracing::info!(url = %handshake.url, pid = handshake.pid, "服务器管理面板已启动");
    *guard = Some(Running {
        child,
        stdin,
        handshake: handshake.clone(),
    });
    Ok(handshake)
}

/// 在主窗口里显示（必要时先创建）内嵌面板 webview。
///
/// `bounds` 是前端量出来的内容区矩形，单位是 CSS 像素；这里负责换算成窗口 DIP。
pub fn show_embedded(app: &AppHandle, hub: &Hub, bounds: Bounds) -> Result<(), String> {
    let handshake = ensure_started(app, hub)?;

    // 子 webview 挂在 Window 上（不是 WebviewWindow）。
    let window = app
        .get_window("main")
        .ok_or_else(|| "找不到主窗口".to_string())?;
    let window_width = window_dip_width(&window)?;

    let mut guard = hub.embedded.lock().unwrap();
    if let Some(webview) = guard.as_ref() {
        // 已经建好：直接摆好位置显示。
        apply_bounds(webview, bounds, window_width)?;
        webview
            .show()
            .map_err(|error| format!("显示面板失败: {error}"))?;
        return Ok(());
    }

    let auth_url = format!(
        "{}/api/desktop/login?token={}",
        handshake.url, handshake.token
    );
    let url = auth_url
        .parse()
        .map_err(|error| format!("面板地址无效: {error}"))?;

    let webview = create_embedded(app, &window, url, bounds, window_width)?;
    *guard = Some(webview);
    Ok(())
}

fn create_embedded(
    app: &AppHandle,
    window: &tauri::Window,
    url: tauri::Url,
    bounds: Bounds,
    window_width: f64,
) -> Result<Webview, String> {
    // 独立 WebView2 数据目录：主窗口带 `additionalBrowserArgs` 建了共享
    // user data folder 的 environment，另一个 webview 用不同参数复用它
    // 会拿到 `ERROR_INVALID_STATE`（0x8007139F）。独立目录还能把面板的
    // 登录 cookie 与主窗口隔离。
    let data_dir = data_dir()?.join("kaypanel").join("webview2");
    std::fs::create_dir_all(&data_dir).map_err(|error| format!("创建面板数据目录失败: {error}"))?;

    // 必须显式关掉透明：子 webview 默认会按「无背景」合成，
    // 页面里没画到的地方会透出下面主窗口的内容（看起来像穿透到桌面）。
    // 再给个浅色底垫着，首帧也不会闪。
    let builder = WebviewBuilder::new(PANEL_WEBVIEW, WebviewUrl::External(url))
        .data_directory(data_dir)
        .transparent(false)
        .background_color(tauri::window::Color(245, 246, 248, 255));

    let (x, y, width, height) = bounds.to_logical(window_width);
    let (tx, rx) = std::sync::mpsc::channel::<Result<Webview, String>>();
    let window_main = window.clone();
    app.run_on_main_thread(move || {
        let result = window_main
            .add_child(
                builder,
                tauri::LogicalPosition::new(x, y),
                tauri::LogicalSize::new(width, height),
            )
            .map_err(|error| {
                tracing::error!(%error, "创建内嵌面板失败");
                format!("创建内嵌面板失败: {error}")
            });
        let _ = tx.send(result);
    })
    .map_err(|error| format!("投递建窗任务失败: {error}"))?;

    let webview = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .map_err(|_| "创建内嵌面板超时".to_string())??;
    tracing::info!("服务器管理面板已内嵌");
    Ok(webview)
}

fn apply_bounds(webview: &Webview, bounds: Bounds, window_width: f64) -> Result<(), String> {
    let (x, y, width, height) = bounds.to_logical(window_width);
    webview
        .set_position(tauri::LogicalPosition::new(x, y))
        .map_err(|error| format!("调整面板位置失败: {error}"))?;
    webview
        .set_size(tauri::LogicalSize::new(width, height))
        .map_err(|error| format!("调整面板大小失败: {error}"))
}

/// 主窗口的 DIP 宽度。`inner_size()` 是物理像素，除以 DPI 缩放才是 DIP。
fn window_dip_width(window: &tauri::Window) -> Result<f64, String> {
    let physical = window
        .inner_size()
        .map_err(|error| format!("读取窗口尺寸失败: {error}"))?;
    let scale = window.scale_factor().unwrap_or(1.0);
    Ok(f64::from(physical.width) / scale)
}

/// 隐藏内嵌面板（切到别的页面时调用）。进程与 webview 都留着，下次秒开。
pub fn hide_embedded(hub: &Hub) {
    let guard = hub.embedded.lock().unwrap();
    if let Some(webview) = guard.as_ref() {
        let _ = webview.hide();
    }
}

/// 应用退出时收拾面板：先关 webview，再关子进程。
pub fn shutdown(app: &AppHandle, hub: &Hub) {
    if let Some(webview) = hub.embedded.lock().unwrap().take() {
        let _ = webview.close();
    }
    let _ = app;

    let mut guard = hub.running.lock().unwrap();
    if let Some(mut running) = guard.take() {
        // 先关 stdin：正常路径是子进程读到 EOF 自行退出。
        drop(running.stdin.take());
        // 给一点时间走优雅关闭，超时就强杀。
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        loop {
            match running.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                _ => {
                    let _ = running.child.kill();
                    let _ = running.child.wait();
                    return;
                }
            }
        }
    }
}

/// 定位 sidecar 二进制。
///
/// 打包后 Tauri 把资源放在可执行文件同目录（`kaypanel.exe`）；
/// 开发时优先用构建脚本产出的 `third_party/kaypanel/dist/kaypanel.exe`。
fn sidecar_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    if let Ok(dir) = app.path().resource_dir() {
        for name in ["kaypanel.exe", "kaypanel"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["kaypanel.exe", "kaypanel"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }

    // 开发态：从 target/debug（或 release）回溯到仓库根。
    let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/kaypanel/dist/kaypanel.exe");
    if dev.is_file() {
        return Ok(dev);
    }

    Err("找不到服务器管理组件 kaypanel.exe，请先运行 scripts\\build-kaypanel.bat".to_string())
}

fn data_dir() -> Result<std::path::PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .map(|base| base.join("ScreenWall"))
        .ok_or_else(|| "无法定位 APPDATA 目录".to_string())
}

// ---------------------------------------------------------------- 命令

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelStatus {
    /// 组件是否可用（二进制存在）。
    pub available: bool,
    /// 进程是否已经在跑。
    pub running: bool,
    /// 不可用时的原因，给界面显示。
    pub error: Option<String>,
}

#[tauri::command]
pub async fn get_panel_status(
    app: AppHandle,
    hub: State<'_, std::sync::Arc<Hub>>,
) -> Result<PanelStatus, String> {
    let mut guard = hub.running.lock().unwrap();
    let running = match guard.as_mut() {
        Some(item) => match item.child.try_wait() {
            Ok(None) => true,
            _ => {
                *guard = None;
                false
            }
        },
        None => false,
    };
    drop(guard);

    let available = sidecar_path(&app).is_ok();
    Ok(PanelStatus {
        available,
        running,
        error: if available {
            None
        } else {
            sidecar_path(&app).err()
        },
    })
}

/// 在内容区显示面板。`bounds` 由前端按实际布局量出来。
#[tauri::command]
pub async fn show_server_panel(
    app: AppHandle,
    hub: State<'_, std::sync::Arc<Hub>>,
    bounds: Bounds,
) -> Result<(), String> {
    let app = app.clone();
    let hub = hub.inner().clone();
    // 启动子进程会阻塞等握手，放到阻塞线程池里，别卡住异步运行时。
    tauri::async_runtime::spawn_blocking(move || show_embedded(&app, &hub, bounds))
        .await
        .map_err(|error| format!("显示面板任务失败: {error}"))?
}

/// 后台预启动面板进程（不显示）。用户真正点「服务器管理」时 `ensure_started`
/// 直接复用已启动的进程返回握手，省掉首次点进去时 spawn + 读握手的等待。
/// 幂等：进程已在跑则直接返回。
#[tauri::command]
pub async fn prewarm_panel(
    app: AppHandle,
    hub: State<'_, std::sync::Arc<Hub>>,
) -> Result<(), String> {
    let app = app.clone();
    let hub = hub.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = ensure_started(&app, &hub);
    })
    .await
    .map_err(|error| format!("预启动面板任务失败: {error}"))?;
    Ok(())
}

/// 隐藏面板（切到别的页面时调用）。不销毁 webview，下次显示更快。
#[tauri::command]
pub async fn hide_server_panel(hub: State<'_, std::sync::Arc<Hub>>) -> Result<(), String> {
    hide_embedded(hub.inner());
    Ok(())
}

/// 面板随主窗口缩放时同步几何位置。
#[tauri::command]
pub async fn resize_server_panel(
    app: AppHandle,
    hub: State<'_, std::sync::Arc<Hub>>,
    bounds: Bounds,
) -> Result<(), String> {
    let Some(window) = app.get_window("main") else {
        return Ok(());
    };
    let width = window_dip_width(&window)?;
    let guard = hub.embedded.lock().unwrap();
    match guard.as_ref() {
        Some(webview) => apply_bounds(webview, bounds, width),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::Bounds;

    fn bounds(x: f64, y: f64, width: f64, height: f64, viewport: f64) -> Bounds {
        Bounds {
            x,
            y,
            width,
            height,
            viewport_width: viewport,
        }
    }

    #[test]
    fn logical_is_identity_at_scale_one() {
        // 无 DPI 缩放、无界面缩放：CSS 像素就是 DIP。
        let b = bounds(228.0, 56.0, 1052.0, 764.0, 1280.0);
        let (x, y, w, h) = b.to_logical(1280.0);
        assert!((x - 228.0).abs() < 1e-9);
        assert!((y - 56.0).abs() < 1e-9);
        assert!((w - 1052.0).abs() < 1e-9);
        assert!((h - 764.0).abs() < 1e-9);
    }

    #[test]
    fn logical_scales_with_dpi() {
        // 200% DPI：窗口 DIP 宽度是视口 CSS 宽度的一半。
        // 内容区在 CSS 里占 1052px，实际应该占 526 DIP。
        let b = bounds(228.0, 56.0, 1052.0, 764.0, 1280.0);
        let (x, y, w, h) = b.to_logical(640.0);
        assert!((x - 114.0).abs() < 1e-9);
        assert!((y - 28.0).abs() < 1e-9);
        assert!((w - 526.0).abs() < 1e-9);
        assert!((h - 382.0).abs() < 1e-9);
    }

    #[test]
    fn logical_handles_ui_zoom() {
        // 界面缩放 1.25：WebView 把 CSS 像素放大 25%，视口 CSS 宽度变小。
        // 1280 DIP 宽的窗口，在 1.25 缩放下 innerWidth 是 1024 CSS px。
        let b = bounds(182.4, 44.8, 841.6, 611.2, 1024.0);
        let (x, _y, w, _h) = b.to_logical(1280.0);
        assert!((x - 228.0).abs() < 1e-9);
        assert!((w - 1052.0).abs() < 1e-9);
    }

    #[test]
    fn logical_falls_back_when_viewport_unmeasured() {
        // 布局还没完成时 innerWidth 可能是 0；不能因此算出无穷大或 NaN。
        let b = bounds(10.0, 20.0, 300.0, 400.0, 0.0);
        let (x, y, w, h) = b.to_logical(1280.0);
        assert!((x - 10.0).abs() < 1e-9);
        assert!((y - 20.0).abs() < 1e-9);
        assert!((w - 300.0).abs() < 1e-9);
        assert!((h - 400.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod serde_tests {
    use super::Bounds;

    /// 前端传的是 camelCase（`viewportWidth`）。这个字段名如果不匹配，
    /// 面板会以 `invalid args bounds` 静默失败 —— 只有 release 包才暴露，
    /// 所以用测试钉死。
    #[test]
    fn bounds_accepts_camel_case_from_frontend() {
        let json = r#"{"x":228.0,"y":56.0,"width":1052.0,"height":764.0,"viewportWidth":1280.0}"#;
        let parsed: Bounds = serde_json::from_str(json).expect("camelCase 应该能解析");
        assert!((parsed.viewport_width - 1280.0).abs() < 1e-9);
    }

    #[test]
    fn bounds_rejects_missing_viewport_width() {
        // 少了 viewportWidth 必须报错，而不是悄悄按 0 处理（会算出错误的缩放）。
        let json = r#"{"x":0.0,"y":0.0,"width":100.0,"height":100.0}"#;
        assert!(serde_json::from_str::<Bounds>(json).is_err());
    }
}
