//! 内存限流。
//!
//! 包含两种策略：
//!
//! - [`LoginLimiter`]：登录失败计数，连续失败达到阈值后锁定一段时间（针对撞库）。
//! - [`RateLimiter`]：通用按 key 的令牌桶，用于登录/注册请求频率、`/ws/view`
//!   连接频率与会话建立频率。
//!
//! 都是进程内状态，重启后计数丢失，这是有意为之的取舍。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 生产默认值：5 次失败锁定 60 秒。
pub const MAX_FAILURES: u32 = 5;
pub const LOCKOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    last_failure: Instant,
    locked_until: Option<Instant>,
}

/// 登录失败限流器，可安全地被多个请求共享。
pub struct LoginLimiter {
    entries: Mutex<HashMap<(String, String), Entry>>,
    max_failures: u32,
    lockout: Duration,
}

impl LoginLimiter {
    pub fn new(max_failures: u32, lockout: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_failures: max_failures.max(1),
            lockout,
        }
    }

    /// 若该「用户名 + IP」正处于锁定期，返回剩余秒数（向上取整，至少 1）。
    pub fn locked_for(&self, username: &str, ip: &str) -> Option<u64> {
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        prune(&mut entries, now, self.lockout);
        let entry = entries.get(&key(username, ip))?;
        remaining(entry.locked_until?, now)
    }

    /// 记录一次登录失败。若本次失败触发了锁定，返回剩余秒数。
    pub fn record_failure(&self, username: &str, ip: &str) -> Option<u64> {
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        prune(&mut entries, now, self.lockout);
        let entry = entries.entry(key(username, ip)).or_insert(Entry {
            failures: 0,
            last_failure: now,
            locked_until: None,
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.last_failure = now;
        if entry.failures >= self.max_failures {
            let until = now + self.lockout;
            entry.locked_until = Some(until);
            return remaining(until, now);
        }
        None
    }

    /// 登录成功，清零该「用户名 + IP」的失败记录。
    pub fn record_success(&self, username: &str, ip: &str) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        entries.remove(&key(username, ip));
    }
}

fn key(username: &str, ip: &str) -> (String, String) {
    (username.to_string(), ip.to_string())
}

fn remaining(until: Instant, now: Instant) -> Option<u64> {
    let left = until.checked_duration_since(now)?;
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    Some(secs.max(1))
}

/// 丢弃已过锁定期且长时间没有新失败尝试的条目，避免内存无限增长。
fn prune(entries: &mut HashMap<(String, String), Entry>, now: Instant, lockout: Duration) {
    entries.retain(|_, entry| match entry.locked_until {
        Some(until) => until > now,
        None => now.duration_since(entry.last_failure) < lockout,
    });
}

// ---------------------------------------------------------------- 令牌桶

/// 通用按 key 的令牌桶限流器。
///
/// 每个 key（例如「登录 + IP」「账号 ID + 操作」）有独立的桶：容量为 `burst`，
/// 每 `per` 时间补充 `rate` 个令牌。桶空时返回 429 与建议的重试秒数。
/// 速率写 0 时该限流器不生效（永远放行），用于在配置里关闭某条限制。
pub struct RateLimiter {
    entries: Mutex<HashMap<String, Bucket>>,
    rate: u32,
    per: Duration,
    burst: u32,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl RateLimiter {
    /// `rate` 个令牌 / `per` 时间；`burst` 为桶容量（至少 1）。
    pub fn new(rate: u32, per: Duration, burst: u32) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            rate,
            per,
            burst: burst.max(1),
        }
    }

    /// 是否启用（速率为 0 视为关闭）。
    pub fn enabled(&self) -> bool {
        self.rate > 0
    }

    /// 尝试为 `key` 取一个令牌。
    ///
    /// 成功返回 `None`；超限返回需要等待的秒数（向上取整，至少 1）。
    pub fn check(&self, key: &str) -> Option<u64> {
        if !self.enabled() {
            return None;
        }
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // 定期清理长时间未访问的 key，避免内存无限增长。
        if entries.len() > 4096 {
            prune_buckets(&mut entries, now, self.per);
        }
        let refill_per_sec = self.rate as f64 / self.per.as_secs_f64();
        let bucket = entries.entry(key.to_string()).or_insert(Bucket {
            tokens: self.burst as f64,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_sec).min(self.burst as f64);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            None
        } else {
            let missing = 1.0 - bucket.tokens;
            let secs = (missing / refill_per_sec).ceil().max(1.0) as u64;
            Some(secs)
        }
    }

    /// 清空某个 key 的计数（例如需要重新开始计数时）。
    pub fn reset(&self, key: &str) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        entries.remove(key);
    }
}

/// 丢弃已回满且长时间未访问的桶。
fn prune_buckets(entries: &mut HashMap<String, Bucket>, now: Instant, per: Duration) {
    let idle = per.max(Duration::from_secs(60));
    entries.retain(|_, bucket| now.saturating_duration_since(bucket.updated) < idle);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_five_failures() {
        let limiter = LoginLimiter::new(MAX_FAILURES, LOCKOUT);
        for attempt in 1..MAX_FAILURES {
            assert_eq!(
                limiter.record_failure("kay", "1.2.3.4"),
                None,
                "第 {attempt} 次不应锁定"
            );
            assert_eq!(limiter.locked_for("kay", "1.2.3.4"), None);
        }
        let remaining = limiter
            .record_failure("kay", "1.2.3.4")
            .expect("第 5 次失败应锁定");
        assert!((1..=60).contains(&remaining));
        assert!(limiter.locked_for("kay", "1.2.3.4").is_some());
    }

    #[test]
    fn success_clears_failures() {
        let limiter = LoginLimiter::new(MAX_FAILURES, LOCKOUT);
        for _ in 0..MAX_FAILURES - 1 {
            limiter.record_failure("kay", "1.2.3.4");
        }
        limiter.record_success("kay", "1.2.3.4");
        // 清零后再失败 4 次也不应锁定。
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(limiter.record_failure("kay", "1.2.3.4"), None);
        }
        assert_eq!(limiter.locked_for("kay", "1.2.3.4"), None);
    }

    #[test]
    fn lockout_expires() {
        let limiter = LoginLimiter::new(2, Duration::from_millis(40));
        assert!(limiter.record_failure("kay", "1.2.3.4").is_none());
        assert!(limiter.record_failure("kay", "1.2.3.4").is_some());
        assert!(limiter.locked_for("kay", "1.2.3.4").is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(limiter.locked_for("kay", "1.2.3.4"), None);
        // 锁定到期后从零开始计数。
        assert!(limiter.record_failure("kay", "1.2.3.4").is_none());
    }

    #[test]
    fn keys_are_scoped_by_username_and_ip() {
        let limiter = LoginLimiter::new(2, LOCKOUT);
        limiter.record_failure("kay", "1.2.3.4");
        assert!(limiter.record_failure("kay", "1.2.3.4").is_some());
        assert_eq!(limiter.locked_for("kay", "5.6.7.8"), None);
        assert_eq!(limiter.locked_for("other", "1.2.3.4"), None);
    }

    #[test]
    fn token_bucket_allows_burst_then_limits() {
        let limiter = RateLimiter::new(60, Duration::from_secs(60), 3);
        for attempt in 1..=3 {
            assert_eq!(limiter.check("ip"), None, "突发内第 {attempt} 次应放行");
        }
        let wait = limiter.check("ip").expect("突发用尽后应限流");
        assert!(wait >= 1, "应返回等待秒数: {wait}");
        // 其他 key 不受影响。
        assert_eq!(limiter.check("other"), None);
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let limiter = RateLimiter::new(1000, Duration::from_secs(1), 1);
        assert_eq!(limiter.check("ip"), None);
        assert!(limiter.check("ip").is_some());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(limiter.check("ip"), None, "窗口过后应恢复");
    }

    #[test]
    fn token_bucket_disabled_when_rate_zero() {
        let limiter = RateLimiter::new(0, Duration::from_secs(60), 1);
        assert!(!limiter.enabled());
        for _ in 0..100 {
            assert_eq!(limiter.check("ip"), None);
        }
    }

    #[test]
    fn token_bucket_reset_clears_key() {
        let limiter = RateLimiter::new(60, Duration::from_secs(60), 1);
        assert_eq!(limiter.check("ip"), None);
        assert!(limiter.check("ip").is_some());
        limiter.reset("ip");
        assert_eq!(limiter.check("ip"), None);
    }
}
