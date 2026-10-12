//! 渠道工作目录：`~/.agent-bridge/workspaces/<bot_key>/<dir_name>/`。
//!
//! owner 决策（2026-10-12，卡 `abb-channel-mcp-config-20261012`）：**每个渠道
//! （群 / 频道 / DM）都有自己的工作目录**，LLM 会话在其中运行，该渠道的配置
//! （`mcp.json`）也放在这里。此前只有「虚拟 Bot 群」有独立目录
//! （`workspaces/<bot>/vb/<fnv_uuid>/`，见 `virtualbot.rs`），普通渠道共用 bot 目录。
//!
//! 目录名用 `sanitize(chat_id)`（可读、可与平台 id 对上看），**不用派生 uuid**：
//! - `(bot_key, chat_id)` 的唯一性由父目录 `<bot_key>/` 提供；
//! - 平台 chat_id 的字符合法性由 `sanitize` 处理；一旦它**改写了**字符串（或结果为空、
//!   或命中 Windows 保留名），追加 `-<8 位哈希>`，避免两个不同 chat_id 落到同一目录
//!   （也躲开大小写不敏感文件系统上的撞名）。
//!
//! 目录内的 `chat.json` 记录**原始** `chat_id`（`sanitize` 不可逆），供反查与排障。

use std::path::{Path, PathBuf};

/// 目录名：`sanitize(chat_id)`；被改写 / 为空 / Windows 保留名时追加 `-<8hex>`。
pub fn dir_name(chat_id: &str) -> String {
    let sanitized = crate::config::sanitize(chat_id);
    let needs_suffix = sanitized != chat_id
        || sanitized.is_empty()
        || crate::config::is_windows_reserved_component(&sanitized);
    if needs_suffix {
        format!("{sanitized}-{}", short_hash(chat_id))
    } else {
        sanitized
    }
}

/// 渠道目录（只解析，不落盘）。
pub fn channel_dir(bot_key: &str, chat_id: &str) -> PathBuf {
    crate::workspace_dir(bot_key).join(dir_name(chat_id))
}

/// 解析并确保渠道目录存在（建目录 → 写 `chat.json` → 写工作区指引，幂等）。
pub fn ensure_channel_dir(
    bot_key: &str,
    chat_id: &str,
    chat_type: &str,
    thread_id: Option<&str>,
) -> PathBuf {
    let dir = channel_dir(bot_key, chat_id);
    ensure_channel_dir_at(&dir, bot_key, chat_id, chat_type, thread_id);
    dir
}

/// 物化实现体（目录已算出）：单测可直接驱动，不依赖真实 `HOME`。
pub(crate) fn ensure_channel_dir_at(
    dir: &Path,
    bot_key: &str,
    chat_id: &str,
    chat_type: &str,
    thread_id: Option<&str>,
) {
    let _ = std::fs::create_dir_all(dir);
    write_chat_json(dir, bot_key, chat_id, chat_type, thread_id);
    crate::agent::ensure_workspace_guide(dir);
}

/// `chat.json` 的内容：**原始** `chat_id` 是核心（目录名可能有损），其余供排障。
fn chat_json(bot_key: &str, chat_id: &str, chat_type: &str, thread_id: Option<&str>) -> String {
    serde_json::json!({
        "chat_id": chat_id,
        "bot_key": bot_key,
        "chat_type": chat_type,
        "thread_id": thread_id,
    })
    .to_string()
}

/// 写 `chat.json`：内容一致就**不写**（本函数在 p2p / 话题的每条消息路径上被调用，
/// 必须廉价）。tmp + rename，避免读方看到半截文件。
fn write_chat_json(
    dir: &Path,
    bot_key: &str,
    chat_id: &str,
    chat_type: &str,
    thread_id: Option<&str>,
) {
    let want = chat_json(bot_key, chat_id, chat_type, thread_id);
    let path = dir.join("chat.json");
    if matches!(std::fs::read_to_string(&path), Ok(have) if have == want) {
        return;
    }
    let tmp = dir.join(".chat.json.tmp");
    if std::fs::write(&tmp, want.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// fnv64 → 8 位小写十六进制。命名空间含 `chat_id`（`bot_key` 已在父目录里，不必重复）。
fn short_hash(chat_id: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("abb-chatdir:{chat_id}").bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:08x}", (h & 0xffff_ffff) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常规平台 id（纯字母数字 + `_`）原样保留——目录名必须能与 chat_id 对上。
    #[test]
    fn dir_name_keeps_plain_chat_id() {
        assert_eq!(
            dir_name("oc_1f097b843c4d12b3bc8b91205cfe4dd8"),
            "oc_1f097b843c4d12b3bc8b91205cfe4dd8"
        );
        assert_eq!(dir_name("cidABC123"), "cidABC123");
        assert_eq!(dir_name("oc-1"), "oc-1");
    }

    /// sanitize 会改写的 id 必须带后缀：否则 `a:b` 与 `a/b` 会落到同一目录。
    #[test]
    fn dir_name_suffixes_when_sanitize_changes() {
        let a = dir_name("oc_1:2");
        let b = dir_name("oc_1/2");
        assert_ne!(a, b, "两个不同 chat_id 不得映射到同一目录");
        assert!(a.starts_with("oc_12-"), "{a}");
        assert!(b.starts_with("oc_12-"), "{b}");
        // 纯空/全被滤掉的 id 也要有稳定名字
        assert!(dir_name("").starts_with('-'));
        assert_eq!(dir_name(""), dir_name(""));
    }

    /// 确定性：同一 chat_id 跨进程/重启稳定。
    #[test]
    fn dir_name_is_deterministic() {
        assert_eq!(dir_name("oc_1:2"), dir_name("oc_1:2"));
        assert_ne!(dir_name("oc_1"), dir_name("oc_2"));
    }

    /// 物化：建目录 + `chat.json` 记原始 id + 幂等（第二次不重写）。
    #[test]
    fn ensure_materializes_dir_and_chat_json() {
        let tmp = std::env::temp_dir().join(format!("abb-chandir-{}", uuid::Uuid::new_v4()));
        let dir = tmp.join(dir_name("oc_1:2"));
        ensure_channel_dir_at(&dir, "bot_a", "oc_1:2", "group", None);
        assert!(dir.is_dir(), "目录必须被创建");
        let raw = std::fs::read_to_string(dir.join("chat.json")).expect("chat.json");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["chat_id"], "oc_1:2",
            "必须记**原始** chat_id（目录名有损）"
        );
        assert_eq!(v["bot_key"], "bot_a");
        assert_eq!(v["chat_type"], "group");
        assert_eq!(v["thread_id"], serde_json::Value::Null);

        // 幂等：内容一致则不重写（mtime 不变即证据）
        let before = std::fs::metadata(dir.join("chat.json"))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        ensure_channel_dir_at(&dir, "bot_a", "oc_1:2", "group", None);
        let after = std::fs::metadata(dir.join("chat.json"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            before, after,
            "内容一致时不得重写 chat.json（每消息路径要廉价）"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 值变化（如话题 id）必须刷新。
    #[test]
    fn ensure_refreshes_chat_json_on_change() {
        let tmp = std::env::temp_dir().join(format!("abb-chandir-{}", uuid::Uuid::new_v4()));
        let dir = tmp.join("oc_x");
        ensure_channel_dir_at(&dir, "bot_a", "oc_x", "group", Some("t1"));
        ensure_channel_dir_at(&dir, "bot_a", "oc_x", "group", Some("t2"));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("chat.json")).unwrap()).unwrap();
        assert_eq!(v["thread_id"], "t2");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
