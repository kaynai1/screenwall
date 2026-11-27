//! SQLite 存储：账号、令牌、设备。

use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// 9 位十进制连接码，首位不为 0（避免被当成数字时丢位）。
fn random_connect_code() -> String {
    let mut rng = rand::thread_rng();
    let value: u32 = rng.gen_range(100_000_000..1_000_000_000);
    value.to_string()
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    username    TEXT NOT NULL UNIQUE,
    pass_hash   TEXT NOT NULL,
    disabled    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tokens (
    token_hash  TEXT PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id   TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    revoked     INTEGER NOT NULL DEFAULT 0,
    -- 'account'（账号登录）或 'quick'（连接码 + 密码换来的受限令牌）
    kind        TEXT NOT NULL DEFAULT 'account',
    -- kind = 'quick' 时只能打开这一台设备
    target_device_id TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_tokens_user ON tokens(user_id, device_id);

CREATE TABLE IF NOT EXISTS devices (
    id           TEXT NOT NULL,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    os           TEXT NOT NULL DEFAULT '',
    version      TEXT NOT NULL DEFAULT '',
    last_seen    INTEGER NOT NULL DEFAULT 0,
    controllable INTEGER NOT NULL DEFAULT 1,
    -- 9 位数字连接码；空字符串表示还没分配
    connect_code TEXT NOT NULL DEFAULT '',
    -- 连接密码的 Argon2 哈希；空字符串表示没有设置
    access_hash  TEXT NOT NULL DEFAULT '',
    -- 是否允许用连接码 + 密码连接本机
    access_enabled INTEGER NOT NULL DEFAULT 0,
    -- 显卡型号；被控端上线时上报，空字符串表示未知
    gpu            TEXT NOT NULL DEFAULT '',
    -- 局域网出口 IPv4；被控端上线时上报，空字符串表示未知
    ip             TEXT NOT NULL DEFAULT '',
    -- 本机识别码（9 位数字，客户端自己生成、固定不变）；被控端上报
    local_id       TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (user_id, id)
);

-- idx_devices_connect_code 不写在这里：老库里 devices 还没有 connect_code 列，
-- 建索引会直接报 "no such column"，导致服务器根本起不来。它统一在 migrate()
-- 里补完列之后再建（新库也一样，因为 SCHEMA 先建表，migrate 随后执行）。

CREATE TABLE IF NOT EXISTS audits (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id              INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id           INTEGER NOT NULL,
    controller_device_id TEXT NOT NULL DEFAULT '',
    controlled_device_id TEXT NOT NULL,
    started_at           INTEGER NOT NULL,
    ended_at             INTEGER,
    duration_ms          INTEGER NOT NULL DEFAULT 0,
    bytes_up             INTEGER NOT NULL DEFAULT 0,
    bytes_down           INTEGER NOT NULL DEFAULT 0,
    -- 'relay'（账号中转）或 'direct'（局域网直连，由客户端上报）
    mode                 TEXT NOT NULL DEFAULT 'relay'
);

CREATE INDEX IF NOT EXISTS idx_audits_user ON audits(user_id, started_at DESC);

-- 一次性连接链接：短期有效、只能消费一次。
-- 只存令牌哈希，明文链接只在签发时返回一次。
CREATE TABLE IF NOT EXISTS connect_links (
    token_hash TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id  TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER
);

CREATE INDEX IF NOT EXISTS idx_connect_links_user ON connect_links(user_id);
"#;

/// 旧库补列。`CREATE TABLE IF NOT EXISTS` 不会给已存在的表加字段，
/// 所以新字段要在这里显式迁移一次；重复执行是安全的。
fn migrate(conn: &Connection) -> Result<()> {
    if !has_column(conn, "audits", "mode")? {
        conn.execute(
            "ALTER TABLE audits ADD COLUMN mode TEXT NOT NULL DEFAULT 'relay'",
            [],
        )?;
    }
    // 连接码 + 密码：账号令牌与受限令牌共用 tokens 表，用 kind 区分。
    // 旧库测试可能只有部分表，逐表判断，缺表就跳过。
    if table_exists(conn, "tokens")? {
        if !has_column(conn, "tokens", "kind")? {
            conn.execute(
                "ALTER TABLE tokens ADD COLUMN kind TEXT NOT NULL DEFAULT 'account'",
                [],
            )?;
        }
        if !has_column(conn, "tokens", "target_device_id")? {
            conn.execute(
                "ALTER TABLE tokens ADD COLUMN target_device_id TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
    }
    if table_exists(conn, "devices")? {
        if !has_column(conn, "devices", "connect_code")? {
            conn.execute(
                "ALTER TABLE devices ADD COLUMN connect_code TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        if !has_column(conn, "devices", "access_hash")? {
            conn.execute(
                "ALTER TABLE devices ADD COLUMN access_hash TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        if !has_column(conn, "devices", "access_enabled")? {
            conn.execute(
                "ALTER TABLE devices ADD COLUMN access_enabled INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has_column(conn, "devices", "gpu")? {
            conn.execute(
                "ALTER TABLE devices ADD COLUMN gpu TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        if !has_column(conn, "devices", "ip")? {
            conn.execute(
                "ALTER TABLE devices ADD COLUMN ip TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        if !has_column(conn, "devices", "local_id")? {
            conn.execute(
                "ALTER TABLE devices ADD COLUMN local_id TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_connect_code
                ON devices(connect_code) WHERE connect_code <> ''",
            [],
        )?;
        // 识别码索引只用来查，不设唯一约束：识别码由各客户端自己随机生成，
        // 万一两台撞上（约 9 亿分之一），唯一索引会让后上线的那台直接登记
        // 失败 —— 那比「查的时候发现重了、拒绝」更糟。查询侧会显式处理重码。
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_devices_local_id
                ON devices(local_id) WHERE local_id <> ''",
            [],
        )?;
    }
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        params![table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub pass_hash: String,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct AuthRow {
    pub user_id: i64,
    pub device_id: String,
    /// `true` 表示这是连接码换来的受限令牌。
    pub quick: bool,
    /// 受限令牌唯一允许打开的设备 ID。
    pub target_device_id: String,
}

#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub os: String,
    pub version: String,
    pub last_seen: i64,
    pub controllable: bool,
    pub gpu: String,
    pub ip: String,
    /// 本机识别码（9 位数字）；被控端上报，空串表示还没上报。
    pub local_id: String,
}

/// 连接码查询结果：设备所属账号、设备本身，以及访问设置。
#[derive(Debug, Clone)]
pub struct QuickDeviceRow {
    pub user_id: i64,
    pub id: String,
    pub name: String,
    pub controllable: bool,
    pub connect_code: String,
    pub access_hash: String,
    pub access_enabled: bool,
}

/// 一次性连接链接命中后的结果：设备所属账号与设备本身。
#[derive(Debug, Clone)]
pub struct ConnectLinkRow {
    pub user_id: i64,
    pub device_id: String,
}

/// 一次远程会话的审计记录。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditRow {
    pub id: i64,
    pub session_id: u32,
    /// 主控端设备的 ID（来自主控连接的令牌）。
    pub controller_device_id: String,
    /// 被控端设备的 ID。
    pub controlled_device_id: String,
    pub started_at: i64,
    /// 会话尚未结束时为 `null`。
    pub ended_at: Option<i64>,
    pub duration_ms: i64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    /// `relay`（账号中转）或 `direct`（局域网直连）。
    pub mode: String,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建 {} 失败", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库 {} 失败", path.display()))?;
        Self::from_connection(conn)
    }

    /// 内存数据库，供测试使用。
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| anyhow!("数据库锁已损坏"))
    }

    pub fn create_user(&self, username: &str, pass_hash: &str) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO users (username, pass_hash, created_at) VALUES (?1, ?2, ?3)",
            params![username, pass_hash, crate::auth::now_ms()],
        )
        .with_context(|| format!("创建用户 {username} 失败"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn find_user(&self, username: &str) -> Result<Option<UserRow>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, username, pass_hash, disabled FROM users WHERE username = ?1",
            params![username],
            |row| {
                Ok(UserRow {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    pass_hash: row.get(2)?,
                    disabled: row.get::<_, i64>(3)? != 0,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn user_by_id(&self, user_id: i64) -> Result<Option<UserRow>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, username, pass_hash, disabled FROM users WHERE id = ?1",
            params![user_id],
            |row| {
                Ok(UserRow {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    pass_hash: row.get(2)?,
                    disabled: row.get::<_, i64>(3)? != 0,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_users(&self) -> Result<Vec<UserRow>> {
        let conn = self.lock()?;
        let mut stmt =
            conn.prepare("SELECT id, username, pass_hash, disabled FROM users ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(UserRow {
                id: row.get(0)?,
                username: row.get(1)?,
                pass_hash: row.get(2)?,
                disabled: row.get::<_, i64>(3)? != 0,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn update_password(&self, user_id: i64, pass_hash: &str) -> Result<()> {
        self.lock()?.execute(
            "UPDATE users SET pass_hash = ?1 WHERE id = ?2",
            params![pass_hash, user_id],
        )?;
        Ok(())
    }

    pub fn create_token(
        &self,
        token_hash: &str,
        user_id: i64,
        device_id: &str,
        expires_at: i64,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO tokens (token_hash, user_id, device_id, created_at, expires_at, revoked)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![
                token_hash,
                user_id,
                device_id,
                crate::auth::now_ms(),
                expires_at
            ],
        )?;
        Ok(())
    }

    /// 写入连接码换来的受限令牌：只能打开 `target_device_id` 这一台设备。
    pub fn create_quick_token(
        &self,
        token_hash: &str,
        user_id: i64,
        target_device_id: &str,
        expires_at: i64,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO tokens
                (token_hash, user_id, device_id, created_at, expires_at, revoked, kind, target_device_id)
             VALUES (?1, ?2, 'quick', ?3, ?4, 0, 'quick', ?5)",
            params![
                token_hash,
                user_id,
                crate::auth::now_ms(),
                expires_at,
                target_device_id
            ],
        )?;
        Ok(())
    }

    /// 返回有效的账号信息；过期、吊销、账号禁用都返回 `None`。
    pub fn validate_token(&self, token_hash: &str) -> Result<Option<AuthRow>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT t.user_id, t.device_id, t.kind, t.target_device_id
             FROM tokens t JOIN users u ON u.id = t.user_id
             WHERE t.token_hash = ?1
               AND t.revoked = 0
               AND t.expires_at > ?2
               AND u.disabled = 0",
            params![token_hash, crate::auth::now_ms()],
            |row| {
                let kind: String = row.get(2)?;
                Ok(AuthRow {
                    user_id: row.get(0)?,
                    device_id: row.get(1)?,
                    quick: kind == "quick",
                    target_device_id: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn revoke_token(&self, token_hash: &str) -> Result<bool> {
        let changed = self.lock()?.execute(
            "UPDATE tokens SET revoked = 1 WHERE token_hash = ?1",
            params![token_hash],
        )?;
        Ok(changed > 0)
    }

    /// 吊销一个设备的全部令牌，返回受影响的行数（包含此前已吊销的令牌）。
    pub fn revoke_device_tokens_count(&self, user_id: i64, device_id: &str) -> Result<usize> {
        let changed = self.lock()?.execute(
            "UPDATE tokens SET revoked = 1 WHERE user_id = ?1 AND device_id = ?2",
            params![user_id, device_id],
        )?;
        Ok(changed)
    }

    pub fn revoke_device_tokens(&self, user_id: i64, device_id: &str) -> Result<()> {
        self.revoke_device_tokens_count(user_id, device_id)?;
        Ok(())
    }

    pub fn revoke_all_user_tokens(&self, user_id: i64) -> Result<()> {
        self.lock()?.execute(
            "UPDATE tokens SET revoked = 1 WHERE user_id = ?1",
            params![user_id],
        )?;
        Ok(())
    }

    pub fn upsert_device(
        &self,
        user_id: i64,
        device_id: &str,
        name: &str,
        os: &str,
        version: &str,
        gpu: &str,
        ip: &str,
        local_id: &str,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO devices
                (id, user_id, name, os, version, last_seen, controllable, gpu, ip, local_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9)
             ON CONFLICT(user_id, id) DO UPDATE SET
                name = excluded.name,
                os = excluded.os,
                version = excluded.version,
                gpu = excluded.gpu,
                ip = excluded.ip,
                -- 识别码是长期固定的：上报空串时保留旧值，不要抹掉。
                local_id = CASE WHEN excluded.local_id = ''
                                THEN devices.local_id
                                ELSE excluded.local_id END,
                last_seen = excluded.last_seen",
            params![
                device_id,
                user_id,
                name,
                os,
                version,
                crate::auth::now_ms(),
                gpu,
                ip,
                local_id
            ],
        )?;
        Ok(())
    }

    /// 确保设备有一个 9 位连接码，返回当前值。
    ///
    /// 已有码直接返回；没有就生成一个全局唯一的码写回去。极端情况下连续撞码
    /// （比如随机源被固定）会重试 20 次后放弃，返回错误。
    pub fn ensure_connect_code(&self, user_id: i64, device_id: &str) -> Result<String> {
        let conn = self.lock()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT connect_code FROM devices WHERE user_id = ?1 AND id = ?2",
                params![user_id, device_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(existing) = existing else {
            return Err(anyhow!("设备不存在"));
        };
        if !existing.is_empty() {
            return Ok(existing);
        }
        for _ in 0..20 {
            let code = random_connect_code();
            let taken: i64 = conn.query_row(
                "SELECT COUNT(*) FROM devices WHERE connect_code = ?1",
                params![&code],
                |row| row.get(0),
            )?;
            if taken > 0 {
                continue;
            }
            conn.execute(
                "UPDATE devices SET connect_code = ?1 WHERE user_id = ?2 AND id = ?3",
                params![&code, user_id, device_id],
            )?;
            return Ok(code);
        }
        Err(anyhow!("生成连接码失败：连续冲突"))
    }

    /// 换一个新的连接码（旧码立即失效）。
    pub fn rotate_connect_code(&self, user_id: i64, device_id: &str) -> Result<String> {
        let conn = self.lock()?;
        for _ in 0..20 {
            let code = random_connect_code();
            let taken: i64 = conn.query_row(
                "SELECT COUNT(*) FROM devices WHERE connect_code = ?1",
                params![&code],
                |row| row.get(0),
            )?;
            if taken > 0 {
                continue;
            }
            let changed = conn.execute(
                "UPDATE devices SET connect_code = ?1 WHERE user_id = ?2 AND id = ?3",
                params![&code, user_id, device_id],
            )?;
            if changed == 0 {
                return Err(anyhow!("设备不存在"));
            }
            return Ok(code);
        }
        Err(anyhow!("生成连接码失败：连续冲突"))
    }

    /// 读取设备的连接码与访问设置。
    pub fn device_access(
        &self,
        user_id: i64,
        device_id: &str,
    ) -> Result<Option<(String, bool, String)>> {
        self.lock()?
            .query_row(
                "SELECT connect_code, access_enabled, access_hash
                 FROM devices WHERE user_id = ?1 AND id = ?2",
                params![user_id, device_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 更新访问密码与开关；`hash = None` 表示保持原密码不变。
    pub fn set_device_access(
        &self,
        user_id: i64,
        device_id: &str,
        hash: Option<&str>,
        enabled: bool,
    ) -> Result<bool> {
        let changed = match hash {
            Some(hash) => self.lock()?.execute(
                "UPDATE devices SET access_hash = ?1, access_enabled = ?2
                 WHERE user_id = ?3 AND id = ?4",
                params![hash, enabled as i64, user_id, device_id],
            )?,
            None => self.lock()?.execute(
                "UPDATE devices SET access_enabled = ?1 WHERE user_id = ?2 AND id = ?3",
                params![enabled as i64, user_id, device_id],
            )?,
        };
        Ok(changed > 0)
    }

    /// 按连接码查设备（跨账号）。
    pub fn device_by_connect_code(&self, code: &str) -> Result<Option<QuickDeviceRow>> {
        self.lock()?
            .query_row(
                "SELECT user_id, id, name, controllable, connect_code, access_hash, access_enabled
                 FROM devices WHERE connect_code = ?1",
                params![code],
                |row| {
                    Ok(QuickDeviceRow {
                        user_id: row.get(0)?,
                        id: row.get(1)?,
                        name: row.get(2)?,
                        controllable: row.get::<_, i64>(3)? != 0,
                        connect_code: row.get(4)?,
                        access_hash: row.get(5)?,
                        access_enabled: row.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 签发一条一次性连接链接，数据库只存令牌哈希。
    pub fn create_connect_link(
        &self,
        user_id: i64,
        device_id: &str,
        token_hash: &str,
        expires_at: i64,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO connect_links
                (token_hash, user_id, device_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                token_hash,
                user_id,
                device_id,
                crate::auth::now_ms(),
                expires_at
            ],
        )?;
        Ok(())
    }

    /// 原子地消费一条链接：不存在、已过期或已使用都返回 `None`。
    ///
    /// 查询与「标记已用」在同一把锁内完成，并发下同一条链接只会被一个请求消费。
    pub fn consume_connect_link(&self, token_hash: &str) -> Result<Option<ConnectLinkRow>> {
        let conn = self.lock()?;
        let row: Option<(i64, String, i64, Option<i64>)> = conn
            .query_row(
                "SELECT user_id, device_id, expires_at, used_at
                 FROM connect_links WHERE token_hash = ?1",
                params![token_hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((user_id, device_id, expires_at, used_at)) = row else {
            return Ok(None);
        };
        if used_at.is_some() || expires_at < crate::auth::now_ms() {
            return Ok(None);
        }
        let changed = conn.execute(
            "UPDATE connect_links SET used_at = ?1
             WHERE token_hash = ?2 AND used_at IS NULL",
            params![crate::auth::now_ms(), token_hash],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(ConnectLinkRow { user_id, device_id }))
    }

    /// 只读地查一条仍然可用的链接（存在、未过期、未消费）。
    ///
    /// 落地页用它判断该显示「唤起客户端」还是「链接已失效」；
    /// 消费前也用它先查目标设备，避免「设备离线」这种可重试的失败把链接烧掉。
    pub fn connect_link_target(&self, token_hash: &str) -> Result<Option<ConnectLinkRow>> {
        let conn = self.lock()?;
        let row: Option<(i64, String, i64, Option<i64>)> = conn
            .query_row(
                "SELECT user_id, device_id, expires_at, used_at
                 FROM connect_links WHERE token_hash = ?1",
                params![token_hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((user_id, device_id, expires_at, used_at)) = row else {
            return Ok(None);
        };
        if used_at.is_some() || expires_at < crate::auth::now_ms() {
            return Ok(None);
        }
        Ok(Some(ConnectLinkRow { user_id, device_id }))
    }

    /// 链接是否仍然可用；只读，不改状态。
    pub fn connect_link_valid(&self, token_hash: &str) -> Result<bool> {
        Ok(self.connect_link_target(token_hash)?.is_some())
    }

    pub fn touch_device(&self, user_id: i64, device_id: &str) -> Result<()> {
        self.lock()?.execute(
            "UPDATE devices SET last_seen = ?1 WHERE user_id = ?2 AND id = ?3",
            params![crate::auth::now_ms(), user_id, device_id],
        )?;
        Ok(())
    }

    pub fn list_devices(&self, user_id: i64) -> Result<Vec<DeviceRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, os, version, last_seen, controllable, gpu, ip, local_id
             FROM devices WHERE user_id = ?1 ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt.query_map(params![user_id], |row| {
            Ok(DeviceRow {
                id: row.get(0)?,
                name: row.get(1)?,
                os: row.get(2)?,
                version: row.get(3)?,
                last_seen: row.get(4)?,
                controllable: row.get::<_, i64>(5)? != 0,
                gpu: row.get(6)?,
                ip: row.get(7)?,
                local_id: row.get(8)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 账号已登记的设备数（用于设备配额）。
    pub fn count_devices(&self, user_id: i64) -> Result<i64> {
        let count = self.lock()?.query_row(
            "SELECT COUNT(*) FROM devices WHERE user_id = ?1",
            params![user_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// 账号总数（用于指标里的 `devices.accounts`）。
    pub fn count_users(&self) -> Result<i64> {
        let count = self
            .lock()?
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn device(&self, user_id: i64, device_id: &str) -> Result<Option<DeviceRow>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, name, os, version, last_seen, controllable, gpu, ip, local_id
             FROM devices WHERE user_id = ?1 AND id = ?2",
            params![user_id, device_id],
            |row| {
                Ok(DeviceRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    os: row.get(2)?,
                    version: row.get(3)?,
                    last_seen: row.get(4)?,
                    controllable: row.get::<_, i64>(5)? != 0,
                    gpu: row.get(6)?,
                    ip: row.get(7)?,
                    local_id: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn rename_device(&self, user_id: i64, device_id: &str, name: &str) -> Result<bool> {
        let changed = self.lock()?.execute(
            "UPDATE devices SET name = ?1 WHERE user_id = ?2 AND id = ?3",
            params![name, user_id, device_id],
        )?;
        Ok(changed > 0)
    }

    pub fn set_controllable(
        &self,
        user_id: i64,
        device_id: &str,
        controllable: bool,
    ) -> Result<bool> {
        let changed = self.lock()?.execute(
            "UPDATE devices SET controllable = ?1 WHERE user_id = ?2 AND id = ?3",
            params![controllable as i64, user_id, device_id],
        )?;
        Ok(changed > 0)
    }

    pub fn delete_device(&self, user_id: i64, device_id: &str) -> Result<bool> {
        let changed = self.lock()?.execute(
            "DELETE FROM devices WHERE user_id = ?1 AND id = ?2",
            params![user_id, device_id],
        )?;
        Ok(changed > 0)
    }

    /// 会话建立时写入开始记录，返回审计行 ID。
    pub fn create_audit(
        &self,
        user_id: i64,
        session_id: u32,
        controller_device_id: &str,
        controlled_device_id: &str,
        started_at: i64,
    ) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO audits
                (user_id, session_id, controller_device_id, controlled_device_id, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                user_id,
                session_id as i64,
                controller_device_id,
                controlled_device_id,
                started_at
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 会话结束时补齐结束时间、时长与字节数。
    pub fn finish_audit(
        &self,
        audit_id: i64,
        ended_at: i64,
        duration_ms: i64,
        bytes_up: u64,
        bytes_down: u64,
    ) -> Result<bool> {
        let changed = self.lock()?.execute(
            "UPDATE audits SET ended_at = ?2, duration_ms = ?3, bytes_up = ?4, bytes_down = ?5
             WHERE id = ?1",
            params![
                audit_id,
                ended_at,
                duration_ms.max(0),
                bytes_up as i64,
                bytes_down as i64
            ],
        )?;
        Ok(changed > 0)
    }

    /// 写入一条已结束的局域网直连审计。
    ///
    /// 直连不经过服务器，服务器没法自己记录，只能由客户端在会话结束后上报。
    /// `session_id` 固定为 0（直连没有服务器会话号）。
    #[allow(clippy::too_many_arguments)]
    pub fn insert_direct_audit(
        &self,
        user_id: i64,
        controller_device_id: &str,
        controlled_device_id: &str,
        started_at: i64,
        ended_at: i64,
        duration_ms: i64,
        bytes_up: u64,
        bytes_down: u64,
    ) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO audits
                (user_id, session_id, controller_device_id, controlled_device_id,
                 started_at, ended_at, duration_ms, bytes_up, bytes_down, mode)
             VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'direct')",
            params![
                user_id,
                controller_device_id,
                controlled_device_id,
                started_at,
                ended_at,
                duration_ms.max(0),
                bytes_up as i64,
                bytes_down as i64
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 分页查询当前账号的审计记录，返回 `(记录, 满足过滤条件的总条数)`。
    ///
    /// `device` 同时匹配主控端与被控端 ID；`None` 或空字符串表示不过滤。
    /// 排序固定为 `started_at DESC, id DESC`，保证分页稳定。
    pub fn audit_page(
        &self,
        user_id: i64,
        limit: i64,
        offset: i64,
        device: Option<&str>,
    ) -> Result<(Vec<AuditRow>, i64)> {
        let conn = self.lock()?;
        let limit = limit.clamp(1, 500);
        let offset = offset.max(0);
        // 空字符串按未传处理。
        let device = device.filter(|value| !value.is_empty());

        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM audits
             WHERE user_id = ?1
               AND (?2 IS NULL OR controller_device_id = ?2 OR controlled_device_id = ?2)",
            params![user_id, device],
            |row| row.get(0),
        )?;

        let mut stmt = conn.prepare(
            "SELECT id, session_id, controller_device_id, controlled_device_id,
                    started_at, ended_at, duration_ms, bytes_up, bytes_down, mode
             FROM audits
             WHERE user_id = ?1
               AND (?2 IS NULL OR controller_device_id = ?2 OR controlled_device_id = ?2)
             ORDER BY started_at DESC, id DESC
             LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(params![user_id, device, limit, offset], |row| {
            Ok(AuditRow {
                id: row.get(0)?,
                session_id: row.get::<_, i64>(1)? as u32,
                controller_device_id: row.get(2)?,
                controlled_device_id: row.get(3)?,
                started_at: row.get(4)?,
                ended_at: row.get(5)?,
                duration_ms: row.get(6)?,
                bytes_up: row.get::<_, i64>(7)?.max(0) as u64,
                bytes_down: row.get::<_, i64>(8)?.max(0) as u64,
                mode: row.get(9)?,
            })
        })?;
        let rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((rows, total))
    }

    /// 最近 `limit` 条审计记录，按开始时间从新到旧。保留给旧调用方使用。
    pub fn recent_audits(&self, user_id: i64, limit: i64) -> Result<Vec<AuditRow>> {
        self.audit_page(user_id, limit, 0, None)
            .map(|(rows, _)| rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    /// 连接码：首次分配后保持稳定，换码后旧码失效；快速令牌只指向目标设备。
    #[test]
    fn connect_code_and_quick_token_roundtrip() {
        let db = db();
        let user = db.create_user("kay", "hash").unwrap();
        db.upsert_device(user, "dev-a", "书房", "Windows", "0.1.0", "GPU", "1.2.3.4", "")
            .unwrap();

        let code = db.ensure_connect_code(user, "dev-a").unwrap();
        assert_eq!(code.len(), 9);
        assert!(!code.starts_with('0'));
        assert_eq!(db.ensure_connect_code(user, "dev-a").unwrap(), code);

        let found = db.device_by_connect_code(&code).unwrap().unwrap();
        assert_eq!(found.id, "dev-a");
        assert_eq!(found.user_id, user);

        let rotated = db.rotate_connect_code(user, "dev-a").unwrap();
        assert_ne!(rotated, code);
        assert!(db.device_by_connect_code(&code).unwrap().is_none());
        assert!(db.device_by_connect_code(&rotated).unwrap().is_some());

        let expires = crate::auth::now_ms() + 60_000;
        db.create_quick_token("hash-1", user, "dev-a", expires)
            .unwrap();
        let auth = db.validate_token("hash-1").unwrap().unwrap();
        assert!(auth.quick);
        assert_eq!(auth.target_device_id, "dev-a");
        assert_eq!(auth.user_id, user);
    }

    #[test]
    fn user_and_token_roundtrip() {
        let db = db();
        let id = db.create_user("kay", "hash").unwrap();
        let user = db.find_user("kay").unwrap().unwrap();
        assert_eq!(user.id, id);

        let expires = crate::auth::now_ms() + 60_000;
        db.create_token("tok-hash", id, "device-1", expires)
            .unwrap();
        let auth = db.validate_token("tok-hash").unwrap().unwrap();
        assert_eq!(auth.user_id, id);
        assert_eq!(auth.device_id, "device-1");
        assert!(db.validate_token("missing").unwrap().is_none());
    }

    #[test]
    fn expired_token_rejected() {
        let db = db();
        let id = db.create_user("kay", "hash").unwrap();
        db.create_token("old", id, "d", crate::auth::now_ms() - 1)
            .unwrap();
        assert!(db.validate_token("old").unwrap().is_none());
    }

    #[test]
    fn revoked_token_rejected() {
        let db = db();
        let id = db.create_user("kay", "hash").unwrap();
        db.create_token("tok", id, "d", crate::auth::now_ms() + 60_000)
            .unwrap();
        assert!(db.revoke_token("tok").unwrap());
        assert!(db.validate_token("tok").unwrap().is_none());
    }

    #[test]
    fn revoke_device_tokens_only_affects_that_device() {
        let db = db();
        let id = db.create_user("kay", "hash").unwrap();
        let expires = crate::auth::now_ms() + 60_000;
        db.create_token("tok-a", id, "dev-a", expires).unwrap();
        db.create_token("tok-b", id, "dev-b", expires).unwrap();

        assert_eq!(db.revoke_device_tokens_count(id, "dev-a").unwrap(), 1);
        assert!(db.validate_token("tok-a").unwrap().is_none());
        let other = db
            .validate_token("tok-b")
            .unwrap()
            .expect("其他设备令牌仍有效");
        assert_eq!(other.device_id, "dev-b");
    }

    #[test]
    fn audit_start_finish_and_recent() {
        let db = db();
        let kay = db.create_user("kay", "hash").unwrap();
        let other = db.create_user("other", "hash").unwrap();
        let started = crate::auth::now_ms();

        let first = db.create_audit(kay, 1, "ctrl", "dev-a", started).unwrap();
        let second = db
            .create_audit(kay, 2, "ctrl", "dev-b", started + 10)
            .unwrap();
        db.create_audit(other, 3, "ctrl2", "dev-c", started + 20)
            .unwrap();

        assert!(db
            .finish_audit(first, started + 500, 500, 123, 456)
            .unwrap());

        let rows = db.recent_audits(kay, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, second, "按开始时间倒序");
        assert_eq!(rows[0].ended_at, None);
        assert_eq!(rows[1].id, first);
        assert_eq!(rows[1].duration_ms, 500);
        assert_eq!(rows[1].bytes_up, 123);
        assert_eq!(rows[1].bytes_down, 456);
        assert_eq!(rows[1].controller_device_id, "ctrl");
        assert_eq!(rows[1].controlled_device_id, "dev-a");

        assert_eq!(db.recent_audits(kay, 1).unwrap().len(), 1);
        assert_eq!(db.recent_audits(other, 10).unwrap().len(), 1);
    }

    /// 直连审计一次写入完整记录，`mode` 标成 `direct`；中转记录仍是 `relay`。
    #[test]
    fn direct_audit_is_recorded_with_mode() {
        let db = db();
        let kay = db.create_user("kay", "hash").unwrap();
        let started = crate::auth::now_ms();

        let relay = db.create_audit(kay, 7, "ctrl", "dev-a", started).unwrap();
        let direct = db
            .insert_direct_audit(
                kay,
                "ctrl",
                "direct:192.168.1.20:47900",
                started + 5,
                started + 1005,
                1000,
                11,
                22,
            )
            .unwrap();
        assert_ne!(relay, direct);

        let rows = db.recent_audits(kay, 10).unwrap();
        assert_eq!(rows.len(), 2);
        // 直连开始得更晚，排在前面。
        let direct_row = rows.iter().find(|row| row.id == direct).unwrap();
        assert_eq!(direct_row.mode, "direct");
        assert_eq!(direct_row.session_id, 0);
        assert_eq!(direct_row.controlled_device_id, "direct:192.168.1.20:47900");
        assert_eq!(direct_row.duration_ms, 1000);
        assert_eq!(direct_row.bytes_up, 11);
        assert_eq!(direct_row.bytes_down, 22);
        assert!(direct_row.ended_at.is_some());

        let relay_row = rows.iter().find(|row| row.id == relay).unwrap();
        assert_eq!(relay_row.mode, "relay");
    }

    /// 0.2.0 时代的库（devices 没有 connect_code 列）能被新版本打开。
    ///
    /// 这是个真实的升级事故：索引原本写在 `SCHEMA` 里，先于 `migrate()` 执行，
    /// 老库上直接报 `no such column: connect_code`，服务器起不来。测试走文件库
    /// 而不是内存库，因为要复现的是「库里已经有旧表」这条路径。
    #[test]
    fn opens_database_created_before_connect_code_column() {
        let path = std::env::temp_dir().join(format!(
            "screenwall-old-schema-{}-{}.db",
            std::process::id(),
            crate::auth::now_ms()
        ));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    username TEXT NOT NULL UNIQUE,
                    pass_hash TEXT NOT NULL,
                    disabled INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE devices (
                    id TEXT NOT NULL,
                    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    name TEXT NOT NULL,
                    os TEXT NOT NULL DEFAULT '',
                    version TEXT NOT NULL DEFAULT '',
                    last_seen INTEGER NOT NULL DEFAULT 0,
                    controllable INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY (user_id, id)
                );
                INSERT INTO users (username, pass_hash, created_at)
                VALUES ('kay', 'hash', 1);
                INSERT INTO devices (id, user_id, name) VALUES ('dev-a', 1, '书房');",
            )
            .unwrap();
        }

        let db = Db::open(&path).expect("老库必须能打开");
        let user = db.find_user("kay").unwrap().expect("用户还在");
        assert_eq!(user.username, "kay");
        // 补列之后连接码能正常分配，说明迁移完整。
        let code = db.ensure_connect_code(user.id, "dev-a").unwrap();
        assert_eq!(code.len(), 9);

        let _ = std::fs::remove_file(&path);
    }

    /// 旧库（没有 mode 列）打开时能补上列，且历史记录默认是 relay。
    #[test]
    fn migrate_adds_mode_to_old_audits_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE audits (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL,
                session_id INTEGER NOT NULL,
                controller_device_id TEXT NOT NULL DEFAULT '',
                controlled_device_id TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                ended_at INTEGER,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                bytes_up INTEGER NOT NULL DEFAULT 0,
                bytes_down INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO audits (user_id, session_id, controlled_device_id, started_at)
            VALUES (1, 1, 'dev-a', 1000);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        // 重复调用必须安全。
        migrate(&conn).unwrap();
        let mode: String = conn
            .query_row("SELECT mode FROM audits LIMIT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "relay");
    }

    /// 造 5 条 kay 的审计 + 1 条 other 的审计，返回 kay 的审计 ID（按创建顺序）。
    fn seed_audits(db: &Db) -> (i64, i64, Vec<i64>) {
        let kay = db.create_user("kay", "hash").unwrap();
        let other = db.create_user("other", "hash").unwrap();
        let base = crate::auth::now_ms();
        let ids = vec![
            db.create_audit(kay, 1, "ctrl-a", "dev-1", base).unwrap(),
            db.create_audit(kay, 2, "ctrl-b", "dev-2", base + 10)
                .unwrap(),
            db.create_audit(kay, 3, "ctrl-a", "dev-2", base + 20)
                .unwrap(),
            db.create_audit(kay, 4, "ctrl-c", "dev-1", base + 30)
                .unwrap(),
            db.create_audit(kay, 5, "ctrl-a", "dev-3", base + 40)
                .unwrap(),
        ];
        db.create_audit(other, 6, "ctrl-a", "dev-1", base + 50)
            .unwrap();
        (kay, other, ids)
    }

    #[test]
    fn audit_page_paginates_without_duplicates_or_gaps() {
        let db = db();
        let (kay, _, ids) = seed_audits(&db);

        // 倒序：ids[4] > ids[3] > ids[2] > ids[1] > ids[0]。
        let (page1, total) = db.audit_page(kay, 2, 0, None).unwrap();
        assert_eq!(total, 5);
        assert_eq!(
            page1.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[4], ids[3]]
        );

        let (page2, total) = db.audit_page(kay, 2, 2, None).unwrap();
        assert_eq!(total, 5);
        assert_eq!(
            page2.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[2], ids[1]]
        );

        let (page3, total) = db.audit_page(kay, 2, 4, None).unwrap();
        assert_eq!(total, 5);
        assert_eq!(
            page3.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[0]]
        );

        // 三页拼起来正好是全部记录，不重复、不遗漏。
        let mut seen: Vec<i64> = page1
            .iter()
            .chain(&page2)
            .chain(&page3)
            .map(|row| row.id)
            .collect();
        seen.sort_unstable();
        let mut expected = ids.clone();
        expected.sort_unstable();
        assert_eq!(seen, expected);

        // offset 超过总数返回空页，total 不受影响。
        let (empty, total) = db.audit_page(kay, 2, 100, None).unwrap();
        assert!(empty.is_empty());
        assert_eq!(total, 5);
    }

    #[test]
    fn audit_page_filters_by_controller_or_controlled_device() {
        let db = db();
        let (kay, other, ids) = seed_audits(&db);

        // 被控端匹配：dev-1 命中 ids[0]、ids[3]。
        let (rows, total) = db.audit_page(kay, 10, 0, Some("dev-1")).unwrap();
        assert_eq!(total, 2);
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[3], ids[0]]
        );

        // 主控端匹配：ctrl-a 命中 ids[0]、ids[2]、ids[4]。
        let (rows, total) = db.audit_page(kay, 10, 0, Some("ctrl-a")).unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[4], ids[2], ids[0]]
        );

        // 过滤 + 分页：total 是过滤后的总数，不是全表总数。
        let (rows, total) = db.audit_page(kay, 1, 1, Some("ctrl-a")).unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[2]]
        );

        // 空字符串等价于不过滤。
        let (rows, total) = db.audit_page(kay, 10, 0, Some("")).unwrap();
        assert_eq!(total, 5);
        assert_eq!(rows.len(), 5);

        // 不存在的设备：空结果、total 为 0。
        let (rows, total) = db.audit_page(kay, 10, 0, Some("missing")).unwrap();
        assert!(rows.is_empty());
        assert_eq!(total, 0);

        // 过滤不会跨账号：other 的 dev-1 记录不会出现在 kay 的结果里。
        let (rows, total) = db.audit_page(other, 10, 0, Some("dev-1")).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn audit_page_orders_ties_by_id_desc() {
        let db = db();
        let kay = db.create_user("kay", "hash").unwrap();
        let base = crate::auth::now_ms();
        let first = db.create_audit(kay, 1, "ctrl", "dev-a", base).unwrap();
        let second = db.create_audit(kay, 2, "ctrl", "dev-a", base).unwrap();

        let (rows, total) = db.audit_page(kay, 10, 0, None).unwrap();
        assert_eq!(total, 2);
        // started_at 相同时按 id 倒序，保证分页稳定。
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![second, first]
        );
    }

    #[test]
    fn device_upsert_and_list() {
        let db = db();
        let id = db.create_user("kay", "hash").unwrap();
        db.upsert_device(id, "d1", "客厅", "Windows 11", "0.1.0", "GPU-A", "10.0.0.1", "")
            .unwrap();
        db.upsert_device(id, "d1", "客厅电脑", "Windows 11", "0.1.0", "GPU-A", "10.0.0.2", "")
            .unwrap();
        db.upsert_device(id, "d2", "书房", "Windows 11", "0.1.0", "GPU-B", "10.0.0.3", "")
            .unwrap();

        let devices = db.list_devices(id).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(
            devices
                .iter()
                .find(|device| device.id == "d1")
                .unwrap()
                .name,
            "客厅电脑"
        );

        assert!(db.set_controllable(id, "d1", false).unwrap());
        assert!(!db.device(id, "d1").unwrap().unwrap().controllable);
        assert!(db.delete_device(id, "d2").unwrap());
        assert_eq!(db.list_devices(id).unwrap().len(), 1);
    }
}
