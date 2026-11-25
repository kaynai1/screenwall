//! 错误文案整理：把 `anyhow` 的上下文链和底层 IO 噪音收成一句人话。
//!
//! 命令返回值仍然是 `String`（前端 `invoke` 会包一层 `Error:`），
//! 这里只负责挑出最后一段真正能让人看懂的信息。

/// 从 `anyhow::Error` 里取最后一段人类可读信息。
///
/// `anyhow` 的 `{:#}` 会把整条链用 `: ` 拼起来（例如
/// `发送心跳失败: IO error: 远程主机强迫关闭了一个现有的连接`），
/// 界面上只需要最后那一段；中间的 `IO error` / `Connection reset`
/// 这类纯技术层会被跳过。
pub fn user_message(error: &anyhow::Error) -> String {
    let chain: Vec<String> = error.chain().map(|item| item.to_string()).collect();
    for message in chain.iter().rev() {
        let cleaned = strip_noise(message);
        if !cleaned.is_empty() && !is_technical(&cleaned) {
            return cleaned;
        }
    }
    // 全是被过滤的技术描述：退回最外层那句，至少比空字符串强。
    chain
        .first()
        .map(|message| strip_noise(message))
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "操作失败，请稍后重试".to_string())
}

/// 去掉常见的英文技术前缀与多余空白。
fn strip_noise(message: &str) -> String {
    let mut text = message.trim();
    // 只剥掉一层 `Error:` 之类的包装，不破坏中文正文。
    for prefix in ["Error: ", "error: ", "anyhow: "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim();
        }
    }
    text.trim().to_string()
}

/// 纯技术层的描述：界面上没有意义，继续往链的上一层找。
fn is_technical(message: &str) -> bool {
    const MARKERS: [&str; 10] = [
        "os error",
        "IO error",
        "io error",
        "Connection reset",
        "connection reset",
        "timed out",
        "UnexpectedEof",
        "Broken pipe",
        "10054",
        "10060",
    ];
    MARKERS.iter().any(|marker| message.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_last_human_segment() {
        let error = anyhow::anyhow!("远程主机强迫关闭了一个现有的连接").context("发送心跳失败");
        assert_eq!(user_message(&error), "远程主机强迫关闭了一个现有的连接");
    }

    #[test]
    fn skips_technical_middle_segments() {
        let error = anyhow::anyhow!("远程主机强迫关闭了一个现有的连接")
            .context("IO error: 连接被重置")
            .context("无法连接服务器");
        // 最外层是「无法连接服务器」，最后一段是真正的原因。
        assert_eq!(user_message(&error), "远程主机强迫关闭了一个现有的连接");
    }

    #[test]
    fn falls_back_to_outer_message() {
        let error = anyhow::anyhow!("os error 10054");
        assert_eq!(user_message(&error), "os error 10054");
    }

    #[test]
    fn strips_error_prefix() {
        let error = anyhow::anyhow!("Error: 令牌已失效");
        assert_eq!(user_message(&error), "令牌已失效");
    }
}
