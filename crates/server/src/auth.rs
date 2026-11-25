//! 密码哈希、令牌与 Authorization 头解析。

use anyhow::{anyhow, Context, Result};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::HeaderMap;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow!("密码哈希失败: {error}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// 32 字节随机令牌，十六进制表示。
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// 从 `Authorization: Bearer <token>` 取令牌。
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    value.strip_prefix("Bearer ").map(str::trim)
}

/// 校验用户名：3–32 字符，字母、数字、下划线、连字符。
pub fn valid_username(name: &str) -> bool {
    let len = name.chars().count();
    (3..=32).contains(&len)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 校验密码：8–128 字符。
pub fn valid_password(password: &str) -> bool {
    (8..=128).contains(&password.chars().count())
}

/// 规范化设备 ID：1–64 字符，字母、数字、下划线、连字符。
pub fn valid_device_id(id: &str) -> bool {
    let len = id.chars().count();
    (1..=64).contains(&len)
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn new_device_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("系统时间异常")
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
