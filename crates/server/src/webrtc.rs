//! 网页端 WebRTC P2P：ICE 服务器清单与 TURN 临时凭据。
//!
//! 浏览器只通过 `POST /api/v1/p2p/ice` 拿到这份清单；被控端拿到的同一份清单
//! 由服务器在转发 `webrtc` offer 时一并附上。TURN 用 coturn 的
//! `use-auth-secret` 模式：用户名是「过期时间戳:账号」，密码是
//! `base64(HMAC-SHA1(secret, 用户名))`，密钥只存在服务器配置里。

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha1::Sha1;

use crate::config::WebrtcConfig;

/// 生成浏览器 / 被控端要用的 `iceServers` 列表。
///
/// 只配了 STUN 也返回一条；`[webrtc] enabled = false` 时调用方不该走到这里。
pub fn ice_servers(config: &WebrtcConfig, user_id: i64) -> Vec<Value> {
    let mut servers = Vec::new();
    if !config.stun.is_empty() {
        servers.push(json!({
            "urls": config.stun,
        }));
    }
    if let (Some(turn), Some(secret)) = (config.turn.as_ref(), config.turn_secret.as_ref()) {
        let (username, credential) = turn_credentials(secret, config.turn_ttl_secs, user_id);
        servers.push(json!({
            "urls": [turn],
            "username": username,
            "credential": credential,
        }));
    }
    servers
}

/// 签发一组 coturn 临时凭据，有效期 `ttl` 秒。
pub fn turn_credentials(secret: &str, ttl: u64, user_id: i64) -> (String, String) {
    let expires = now_secs().saturating_add(ttl.max(60));
    let username = format!("{expires}:{user_id}");
    let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes())
        .expect("HMAC 接受任意长度密钥");
    mac.update(username.as_bytes());
    let credential = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    (username, credential)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_credential_matches_coturn_algorithm() {
        // 用固定 secret 手算一遍：coturn 的规则是 base64(HMAC-SHA1(secret, username))。
        let (username, credential) = turn_credentials("test-secret", 600, 7);
        let (expires, user) = username.split_once(':').unwrap();
        assert_eq!(user, "7");
        assert!(expires.parse::<u64>().unwrap() > now_secs());
        let mut mac = Hmac::<Sha1>::new_from_slice(b"test-secret").unwrap();
        mac.update(username.as_bytes());
        let expect =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert_eq!(credential, expect);
    }

    #[test]
    fn ice_servers_without_turn_only_lists_stun() {
        let config = WebrtcConfig {
            enabled: true,
            stun: vec!["stun:example.com:3478".to_string()],
            turn: None,
            turn_secret: None,
            turn_ttl_secs: 3600,
        };
        let servers = ice_servers(&config, 1);
        assert_eq!(servers.len(), 1);
        assert!(servers[0]["urls"][0].as_str().unwrap().starts_with("stun:"));
    }
}
