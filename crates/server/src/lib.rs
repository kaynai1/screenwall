//! ScreenWall 服务器库：HTTP API、WebSocket 会话中转。

pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod metrics;
pub mod ratelimit;
pub mod state;
pub mod tls;
pub mod update;
pub mod ws;

use std::path::PathBuf;
use std::sync::Arc;

use axum::response::Redirect;
use axum::routing::{get, patch, post};
use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

/// 组装完整的 HTTP + WebSocket 路由。
pub fn router(state: Arc<AppState>) -> Router {
    router_with_web(state, None)
}

/// 同 [`router`]，额外把网页端（`web/dist`）挂在 `/app` 下。
///
/// 网页端是纯静态 SPA，所有资源用 `/app/` 前缀；找不到的路径回落到
/// `index.html`，由前端自己处理路由。没有配置目录时不挂载，行为与旧版一致。
pub fn router_with_web(state: Arc<AppState>, web_dir: Option<PathBuf>) -> Router {
    let app = Router::new()
        .route("/healthz", get(api::healthz))
        .route("/api/v1/register", post(api::register))
        .route("/api/v1/login", post(api::login))
        .route("/api/v1/me", get(api::me))
        .route("/api/v1/logout", post(api::logout))
        .route("/api/v1/devices", get(api::devices))
        .route(
            "/api/v1/devices/{device_id}",
            patch(api::patch_device).delete(api::delete_device),
        )
        .route(
            "/api/v1/devices/{device_id}/revoke",
            post(api::revoke_device),
        )
        .route("/api/v1/audits", get(api::audits))
        .route("/api/v1/audits/direct", post(api::report_direct_audit))
        .route(
            "/api/v1/quick",
            get(api::quick_state).post(api::quick_update),
        )
        .route("/api/v1/quick/rotate", post(api::quick_rotate))
        .route("/api/v1/quick/connect", post(api::quick_connect))
        .route("/api/v1/resolve-id", post(api::resolve_local_id))
        .route("/api/v1/feedback", post(api::submit_feedback))
        .route("/api/v1/connect-link", post(api::create_connect_link))
        .route("/api/v1/connect-link/use", post(api::use_connect_link))
        .route("/l/{token}", get(api::link_page))
        .route("/api/v1/metrics", get(api::metrics))
        .route("/api/v1/version", get(update::version))
        .route("/downloads/{file}", get(update::download))
        .route("/metrics", get(api::metrics_legacy))
        .route("/ws/device", get(ws::device_ws))
        .route("/ws/view", get(ws::view_ws))
        .with_state(state);

    match web_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            app.route("/", get(|| async { Redirect::temporary("/app/") }))
                .nest_service(
                    "/app",
                    ServeDir::new(dir).fallback(ServeFile::new(index)),
                )
        }
        None => app,
    }
}
