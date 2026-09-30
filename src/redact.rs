//! 面向用户的错误文本卫生：脱敏 → 折行 → 组合子进程 stderr 尾巴。
//!
//! 为什么单独成模块：agent 起不来时，最有诊断价值的一句话往往来自**子进程 stderr**
//! （例如 BUZZ_AGENT_PROVIDER is required）。这条文本会**进 IM 对话**，所以必须先脱敏
//! 再截断 —— 密钥/令牌不能因为「排障方便」被贴到聊天里。
//!
//! 零正则（与全仓 mention 解析同口径）：手写字符扫描，行为可单测、无依赖。

/// 掩码占位符（与 elev::REDACTED 同形）。
pub const REDACTED: &str = "<redacted>";

/// 连续十六进制串达到这个长度即视为密钥（ed25519 seed / token / sha256 都是这个形状）。
///
/// 取 32 而不是 64：宁可**过度**掩码（比如把 40 位 git sha 也掩掉），也不要把短密钥贴出去。
/// 尾巴只用于「启动失败」排障，损失几个 sha 可接受。
const HEX_RUN_MIN: usize = 32;

/// 敏感键名（小写前缀匹配）。命中即把 key=value / key: value 的**值**掩掉。
const SENSITIVE_KEYS: [&str; 6] = ["api_key", "apikey", "secret", "token", "password", "bearer"];

/// 尾巴进提示前的字符上限（消费方还会再截一次，这里先兜住「几百行 stderr」）。
const TAIL_MAX_CHARS: usize = 400;

/// 折成单行：折叠所有空白与控制字符（IM 提示是单行文案，多行会被通道截断或撑坏形态）。
pub fn flatten_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 脱敏：① 长十六进制串整体掩掉；② 敏感键的值掩掉（含 Bearer <token> 这种两词形态）。
pub fn mask_secrets(s: &str) -> String {
    // ① 长十六进制串（逐字符扫描，保持零正则）
    let chars: Vec<char> = s.chars().collect();
    let mut hexed = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_hexdigit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                i += 1;
            }
            if i - start >= HEX_RUN_MIN {
                hexed.push_str(REDACTED);
            } else {
                hexed.extend(chars[start..i].iter());
            }
        } else {
            hexed.push(chars[i]);
            i += 1;
        }
    }
    // ② 敏感键的值
    let mut out = String::with_capacity(hexed.len());
    let mut mask_next = false;
    for (idx, tok) in hexed.split_whitespace().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        if mask_next {
            // 允许 key = value / key : value 这种「键、分隔符、值」三词形态：
            // 纯分隔符原样保留，继续等真正的值（否则会把 = 掩掉、值却留着）。
            if tok.chars().all(|c| c == '=' || c == ':' || c == '>') {
                out.push_str(tok);
            } else {
                out.push_str(REDACTED);
                mask_next = false;
            }
            continue;
        }
        let lower = tok.to_ascii_lowercase();
        match SENSITIVE_KEYS.iter().find(|k| lower.starts_with(**k)) {
            Some(k) => {
                if let Some(pos) = tok.find(['=', ':']) {
                    // 保留 key= / key: 前缀（= 与 : 都是 ASCII，字节边界安全）
                    out.push_str(&tok[..=pos]);
                    out.push_str(REDACTED);
                } else if lower == *k {
                    // 裸键（典型：Bearer <token>）——它自己不是值，下一个词才是
                    out.push_str(tok);
                    mask_next = true;
                } else {
                    out.push_str(REDACTED);
                }
            }
            None => out.push_str(tok),
        }
    }
    out
}

/// 把子进程 stderr 尾巴并进错误文案（已经脱敏 + 折行 + 截断）。
///
/// 无尾巴（没起过进程、或 stderr 一行都没有）时原样返回 base —— 绝不产出
/// 「；agent stderr：」这种前半句悬空的提示。
pub fn error_with_stderr_tail(base: String, tail: Option<&str>) -> String {
    let Some(t) = tail else { return base };
    let flat = flatten_line(t);
    if flat.is_empty() {
        return base;
    }
    let masked = mask_secrets(&flat);
    let short: String = masked.chars().take(TAIL_MAX_CHARS).collect();
    format!("{base}；agent stderr：{short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64 位十六进制（ed25519 seed / 各类 token 的形状）必须被掩掉。
    #[test]
    fn masks_long_hex_run() {
        let s = mask_secrets("register failed seed=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef ok");
        assert!(!s.contains("0123456789abcdef"), "{s}");
        assert!(s.contains(REDACTED), "{s}");
        assert!(s.contains("register failed"), "{s}");
        assert!(s.contains("ok"), "{s}");
    }

    /// 短十六进制（版本串、pid、短 hash）保持原样，别把正常文本吃了。
    #[test]
    fn keeps_short_hex() {
        let s = mask_secrets("commit deadbeef pid=1234 ver=0x1f");
        assert_eq!(s, "commit deadbeef pid=1234 ver=0x1f");
    }

    /// 敏感键的**值**掩掉，键名保留（用户要能看出是缺哪个键）。
    #[test]
    fn masks_sensitive_key_values() {
        for raw in [
            "api_key=sk-live-abcdef",
            "token: abcdefghijklmn",
            "PASSWORD=hunter2",
            "secret = s3cr3t-value",
        ] {
            let s = mask_secrets(raw);
            assert!(s.contains(REDACTED), "raw={raw} got={s}");
            assert!(!s.contains("hunter2"), "{s}");
            assert!(!s.contains("s3cr3t-value"), "{s}");
        }
        assert!(mask_secrets("api_key=sk-live-abcdef").contains("api_key="));
    }

    /// Bearer <token> 是「裸键 + 下一个词」形态，值必须掩掉。
    #[test]
    fn masks_bearer_token_pair() {
        let s = mask_secrets("Authorization: Bearer abcdefghijklmnop");
        assert!(!s.contains("abcdefghijklmnop"), "{s}");
        assert!(s.to_ascii_lowercase().contains("bearer"), "{s}");
    }

    /// 折行：多行/制表/回车全部折成单空格（IM 文案必须单行）。
    #[test]
    fn flatten_removes_newlines_and_tabs() {
        let s = flatten_line("spawn 失败\n  caused by:\tEDR 拦截\r\n");
        assert!(
            !s.contains('\n') && !s.contains('\r') && !s.contains('\t'),
            "{s:?}"
        );
        assert_eq!(s, "spawn 失败 caused by: EDR 拦截");
    }

    /// 组合：无尾巴 → 原文案（不得出现悬空前缀）。
    #[test]
    fn no_tail_keeps_base_untouched() {
        for tail in [None, Some(""), Some("   \n\t ")] {
            let s = error_with_stderr_tail("agent initialize failed: x".to_string(), tail);
            assert_eq!(s, "agent initialize failed: x");
        }
    }

    /// 组合：有尾巴 → 原文案 + 脱敏后的尾巴，且**单行**。
    #[test]
    fn tail_is_appended_masked_and_single_line() {
        let s = error_with_stderr_tail(
            "agent initialize failed: Agent process exited unexpectedly".to_string(),
            Some("config: BUZZ_AGENT_PROVIDER is required\napi_key=sk-secret-value\n"),
        );
        assert!(s.contains("Agent process exited unexpectedly"), "{s}");
        assert!(
            s.contains("BUZZ_AGENT_PROVIDER is required"),
            "真实原因必须带上：{s}"
        );
        assert!(!s.contains("sk-secret-value"), "密钥不得进提示：{s}");
        assert!(!s.contains('\n'), "{s}");
    }

    /// 超长尾巴必须截断（不能把几百行 stderr 塞进一条 IM 回复）。
    #[test]
    fn long_tail_is_truncated() {
        let s = error_with_stderr_tail("base".to_string(), Some(&"x".repeat(5000)));
        assert!(
            s.chars().count() < TAIL_MAX_CHARS + 100,
            "len={}",
            s.chars().count()
        );
    }
}
