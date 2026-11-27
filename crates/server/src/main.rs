//! ScreenWall 账号与中转服务器。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;
use clap::{Parser, Subcommand};
use screenwall_server::auth::{hash_password, hash_token, valid_password, valid_username};
use screenwall_server::config::{Config, LimitsConfig, TlsConfig, UpdateConfig};
use screenwall_server::db::Db;
use screenwall_server::router;
use screenwall_server::state::AppState;
use screenwall_server::tls::{self, PeerAddr, TlsListener};
use screenwall_server::update::UpdateState;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "screenwall-server",
    version,
    about = "ScreenWall 账号与中转服务器"
)]
struct Cli {
    /// 配置文件路径；不存在时使用默认值。
    #[arg(long, global = true, default_value = "server.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 启动服务器。
    Run,
    /// 管理账号。
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// 管理令牌。
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// 查看客户端提交的建议与 BUG。
    Feedback {
        #[command(subcommand)]
        action: FeedbackAction,
    },
}

#[derive(Debug, Subcommand)]
enum UserAction {
    /// 创建账号。
    Add {
        username: String,
        /// 直接在命令行给密码（不推荐，会进入 shell 历史）。
        #[arg(long)]
        password: Option<String>,
    },
    /// 列出账号。
    List,
    /// 重置密码并吊销该账号的所有令牌。
    Reset {
        username: String,
        #[arg(long)]
        password: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TokenAction {
    /// 吊销一个令牌。
    Revoke { token: String },
}

#[derive(Debug, Subcommand)]
enum FeedbackAction {
    /// 查看用户提交的建议与 BUG（最新的在前）。
    List {
        /// 最多显示多少条。
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        Command::Run => {
            let tls = TlsConfig::load(&cli.config)?;
            let update = UpdateConfig::load(&cli.config)?;
            let limits = LimitsConfig::load(&cli.config)?;
            run(config, tls, update, limits).await
        }
        Command::User { action } => {
            let db = Db::open(&config.db_path)?;
            user_action(&db, action)
        }
        Command::Token { action } => {
            let db = Db::open(&config.db_path)?;
            token_action(&db, action)
        }
        Command::Feedback { action } => {
            let db = Db::open(&config.db_path)?;
            feedback_action(&db, action)
        }
    }
}

fn feedback_action(db: &Db, action: FeedbackAction) -> Result<()> {
    match action {
        FeedbackAction::List { limit } => {
            let rows = db.list_feedback(limit)?;
            if rows.is_empty() {
                println!("还没有用户反馈");
                return Ok(());
            }
            for row in rows {
                let category = if row.category == "bug" {
                    "BUG"
                } else {
                    "建议"
                };
                let device = if row.device_name.is_empty() {
                    row.device_id.as_str()
                } else {
                    row.device_name.as_str()
                };
                let contact = if row.contact.is_empty() {
                    "-"
                } else {
                    row.contact.as_str()
                };
                println!(
                    "#{} {} [{}] {}（{}）v{}\n  联系方式：{}\n  {}",
                    row.id,
                    format_time_ms(row.created_at),
                    category,
                    printable(&row.username),
                    printable(device),
                    printable(&row.app_version),
                    printable(contact),
                    printable(&row.content).replace('\n', "\n  ")
                );
            }
        }
    }
    Ok(())
}

/// 打印用户提交的内容前清洗控制字符（保留换行和制表符）。
///
/// 反馈内容、联系方式、设备名都来自请求体；不处理的话，ANSI 转义序列可以
/// 清屏、改终端标题，`\r` 还能覆盖掉前面的记录行，让管理员看到伪造的输出。
fn printable(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c == '\n' || c == '\t' || !c.is_control() {
                c
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

/// 把毫秒时间戳格式化为北京时间 `YYYY-MM-DD HH:MM`。
///
/// 只做「固定 +8 小时」的日历换算（Howard Hinnant 的 civil_from_days），
/// 避免为一个 CLI 输出引入整个时间库。
fn format_time_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000) + 8 * 3600;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute) = (rem / 3600, rem % 3600 / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

async fn run(
    config: Config,
    tls: Option<TlsConfig>,
    update: UpdateConfig,
    limits: LimitsConfig,
) -> Result<()> {
    let db = Db::open(&config.db_path)?;
    let mut state = AppState::with_limits(db, config.clone(), limits);
    state.update = UpdateState::from_config(&update);
    let state = Arc::new(state);
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("监听 {} 失败", config.bind))?;

    match tls {
        Some(tls) => {
            let cert = tls.tls_cert.as_ref().context("tls_cert 未配置")?;
            let key = tls.tls_key.as_ref().context("tls_key 未配置")?;
            let server_config = tls::load_server_config(cert, key)?;
            tracing::info!(
                "ScreenWall 服务器已启动: https://{}（WSS，证书 {}，数据库 {}，公开注册 {}）",
                config.bind,
                cert.display(),
                config.db_path.display(),
                if config.allow_registration {
                    "开启"
                } else {
                    "关闭"
                }
            );
            serve(TlsListener::new(listener, server_config), app).await
        }
        None => {
            tracing::info!(
                "ScreenWall 服务器已启动: http://{}（数据库 {}，公开注册 {}）",
                config.bind,
                config.db_path.display(),
                if config.allow_registration {
                    "开启"
                } else {
                    "关闭"
                }
            );
            serve(listener, app).await
        }
    }
}

/// 明文与 TLS 监听共用的启动入口；路由、WS 升级逻辑完全一致。
async fn serve<L>(listener: L, app: axum::Router) -> Result<()>
where
    L: axum::serve::Listener<Addr = SocketAddr> + Send + 'static,
    for<'a> PeerAddr: Connected<IncomingStream<'a, L>>,
{
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<PeerAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("收到 Ctrl+C，正在退出");
    })
    .await?;
    Ok(())
}

fn user_action(db: &Db, action: UserAction) -> Result<()> {
    match action {
        UserAction::Add { username, password } => {
            let username = username.trim();
            if !valid_username(username) {
                bail!("用户名需为 3–32 位字母、数字、下划线或连字符");
            }
            if db.find_user(username)?.is_some() {
                bail!("用户 {username} 已存在");
            }
            let password = read_password(password)?;
            if !valid_password(&password) {
                bail!("密码需为 8–128 位");
            }
            let id = db.create_user(username, &hash_password(&password)?)?;
            println!("已创建用户 {username}（id={id}）");
        }
        UserAction::List => {
            let users = db.list_users()?;
            if users.is_empty() {
                println!("还没有账号");
            }
            for user in users {
                println!(
                    "id={}\t{}\t{}",
                    user.id,
                    user.username,
                    if user.disabled { "已禁用" } else { "正常" }
                );
            }
        }
        UserAction::Reset { username, password } => {
            let user = db
                .find_user(username.trim())?
                .with_context(|| format!("用户 {username} 不存在"))?;
            let password = read_password(password)?;
            if !valid_password(&password) {
                bail!("密码需为 8–128 位");
            }
            db.update_password(user.id, &hash_password(&password)?)?;
            db.revoke_all_user_tokens(user.id)?;
            println!("已重置 {username} 的密码，并吊销其全部令牌");
        }
    }
    Ok(())
}

fn token_action(db: &Db, action: TokenAction) -> Result<()> {
    match action {
        TokenAction::Revoke { token } => {
            let token = token.trim();
            if db.revoke_token(&hash_token(token))? {
                println!("令牌已吊销");
            } else {
                println!("没有找到该令牌（可能已吊销）");
            }
        }
    }
    Ok(())
}

fn read_password(provided: Option<String>) -> Result<String> {
    if let Some(password) = provided {
        return Ok(password);
    }
    let password = rpassword::prompt_password("密码: ")?;
    let confirm = rpassword::prompt_password("再次输入: ")?;
    if password != confirm {
        bail!("两次输入的密码不一致");
    }
    Ok(password)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::{format_time_ms, printable};

    #[test]
    fn strips_terminal_control_characters() {
        // ANSI 清屏 + 回车覆盖 + 退格都要替换掉，换行和制表符保留。
        assert_eq!(
            printable("\x1b[2Jhi\r\tthere\nok"),
            "\u{fffd}[2Jhi\u{fffd}\tthere\nok"
        );
        assert_eq!(printable("正常中文"), "正常中文");
    }

    #[test]
    fn formats_epoch_and_known_dates_in_beijing_time() {
        // 1970-01-01 00:00 UTC = 北京时间 08:00。
        assert_eq!(format_time_ms(0), "1970-01-01 08:00");
        // 2024-02-29 03:04 UTC（闰日）= 北京时间 11:04。
        assert_eq!(format_time_ms(1_709_175_840_000), "2024-02-29 11:04");
        // 2026-01-01 00:00 UTC 之前一秒，跨年边界。
        assert_eq!(format_time_ms(1_767_225_599_000), "2026-01-01 07:59");
    }
}
