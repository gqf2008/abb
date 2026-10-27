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

/// 目录名：`sanitize(chat_id)`；被改写 / 为空 / Windows 保留名 / 与 bot 工作区
/// 结构目录撞名时追加 `-<8hex>`。
pub fn dir_name(chat_id: &str) -> String {
    let sanitized = crate::config::sanitize(chat_id);
    let needs_suffix = sanitized != chat_id
        || sanitized.is_empty()
        || crate::config::is_windows_reserved_component(&sanitized)
        || collides_with_workspace_structure(&sanitized);
    if needs_suffix {
        format!("{sanitized}-{}", short_hash(chat_id))
    } else {
        sanitized
    }
}

/// bot 工作区根的结构目录（见 `wsver::GITIGNORE` / `tidy::STRUCTURE_DIRS`）。
/// 渠道目录与它们同级：撞名的话要么把桥的文件当渠道目录、要么让 tidy/git 误处理，
/// 所以一律加后缀避开（大小写不敏感比较：macOS/Windows 默认如此）。
const WORKSPACE_STRUCTURE_DIRS: &[&str] = &[
    ".git",
    ".pi-sessions",
    ".trash",
    "archive",
    "attachments",
    "history",
    "notes",
    "sessions",
    "summaries",
    "vb",
];

fn collides_with_workspace_structure(name: &str) -> bool {
    WORKSPACE_STRUCTURE_DIRS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
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
    // **先迁移、后建目录**，且迁移必须对"目标已存在"幂等——否则任何先物化渠道目录的
    // 路径（启动巡检 `acp-channel-sync`、job 派发、桥的 workspace_for）都会把旧 vb
    // 目录的存量数据（history/sessions/AGENTS.md）永久孤儿化。
    migrate_legacy_vb_dir(dir, bot_key, chat_id);
    let _ = std::fs::create_dir_all(dir);
    write_chat_json(dir, bot_key, chat_id, chat_type, thread_id);
    crate::agent::ensure_workspace_guide(dir);
}

/// 旧 vb 目录 `workspaces/<bot>/vb/<vb_uuid>/` → 本渠道目录（一次性，幂等）。
///
/// 旧目录不存在即 no-op。目标已存在时**逐条并入**而不是早退：启动巡检会在消息路径
/// 之前物化渠道目录，早退会让存量 history/sessions 再也无人读（`History::open` 已
/// 指向新目录）。定向条目冲突时**保留目标**（不覆盖已有数据）。
fn migrate_legacy_vb_dir(dir: &Path, bot_key: &str, chat_id: &str) {
    let legacy = crate::workspace_dir(bot_key)
        .join("vb")
        .join(crate::virtualbot::vb_uuid(bot_key, chat_id));
    if !legacy.is_dir() {
        return;
    }
    if !dir.exists() {
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(&legacy, dir) {
            Ok(()) => crate::log!(
                "[channel-dir] 旧 vb 工作区已迁移：vb/{} → {}",
                crate::virtualbot::vb_uuid(bot_key, chat_id),
                dir.display()
            ),
            Err(error) => crate::log!(
                "[channel-dir] 旧 vb 工作区迁移失败（保留旧目录 vb/{}）：{error}",
                crate::virtualbot::vb_uuid(bot_key, chat_id)
            ),
        }
        return;
    }
    let (moved, skipped) = merge_missing_entries(&legacy, dir);
    // 只删空目录：旧目录还有搬不进来的东西时保留它（宁留不丢）。
    let removed = std::fs::remove_dir(&legacy).is_ok();
    if moved > 0 || skipped > 0 || removed {
        crate::log!(
            "[channel-dir] 旧 vb 工作区并入渠道目录：vb/{} → {}（搬入 {moved}，已存在跳过 {skipped}，旧目录已清={removed}）",
            crate::virtualbot::vb_uuid(bot_key, chat_id),
            dir.display()
        );
    }
}

/// 把 `from` 里**目标不存在**的条目搬进 `to`（不覆盖、不递归深合）。
/// 返回 `(搬入数, 已存在跳过数)`。定向条目冲突时以 `to` 为准（已有数据优先）。
pub(crate) fn merge_missing_entries(from: &Path, to: &Path) -> (usize, usize) {
    let mut moved = 0usize;
    let mut skipped = 0usize;
    let Ok(entries) = std::fs::read_dir(from) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let dest = to.join(entry.file_name());
        if dest.exists() {
            skipped += 1;
            continue;
        }
        if std::fs::rename(entry.path(), &dest).is_ok() {
            moved += 1;
        }
    }
    (moved, skipped)
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
    // 用 uuid 唯一 tmp 名的共享原子写（卡 `abb-symlink-hardening-20261028`）：
    // 固定 `.chat.json.tmp` + `fs::write`（O_CREAT|O_TRUNC）**会跟随符号链接**，
    // 而这个目录就在**工作区内**（`workspace_dir(bot_key).join(dir_name(chat_id))`）——
    // 是同一修复类里可达性最高的一处。受限会话当前无 `ln`/`cp`/`mv` 所以无实操通路，
    // 但这是「白名单没开口」而非「结构上不可能」。
    let _ = crate::atomic_write_text(&path, &want);
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

    /// 保留名/结构目录撞名必须加后缀。
    #[test]
    fn dir_name_avoids_workspace_structure_dirs() {
        for reserved in [
            "history",
            "sessions",
            "archive",
            "attachments",
            "vb",
            "notes",
        ] {
            let got = dir_name(reserved);
            assert_ne!(got, reserved, "{reserved} 不得原样当渠道目录名");
            assert!(got.starts_with(reserved), "{got}");
        }
        assert_ne!(dir_name("HISTORY"), "HISTORY", "大小写不敏感 FS 上也要避开");
    }

    /// 旧 vb 目录 → 渠道目录：目标**已存在**时逐条并入（复现评审 B1：启动巡检
    /// 先物化渠道目录，消息路径的迁移绝不能早退，否则存量数据永久孤儿）。
    #[test]
    fn legacy_vb_dir_is_merged_when_target_already_exists() {
        let bot = format!("abb-test-{}", uuid::Uuid::new_v4());
        let chat = "oc_legacy_review";
        let legacy = crate::workspace_dir(&bot)
            .join("vb")
            .join(crate::virtualbot::vb_uuid(&bot, chat));
        std::fs::create_dir_all(legacy.join("history")).unwrap();
        std::fs::write(legacy.join("history").join("oc_legacy_review.jsonl"), "h").unwrap();
        std::fs::write(legacy.join("legacy-only.txt"), "x").unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(crate::workspace_dir(&bot));

        // 1) 模拟*另一条路径*先物化了渠道目录（启动巡检）
        let dir = channel_dir(&bot, chat);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chat.json"), "{}").unwrap();

        // 2) 消息路径物化 → 必须把旧目录里的东西搬进来
        ensure_channel_dir_at(&dir, &bot, chat, "group", None);
        assert!(
            dir.join("history").join("oc_legacy_review.jsonl").exists(),
            "旧 history 必须并入（否则该群历史对 agent 变空）"
        );
        assert!(
            dir.join("legacy-only.txt").exists(),
            "旧目录其余条目也要并入"
        );
        assert!(!legacy.exists(), "搬空后旧目录应被清掉");
        // 幂等：再跑一次不报错、不丢东西
        ensure_channel_dir_at(&dir, &bot, chat, "group", None);
        assert!(dir.join("history").join("oc_legacy_review.jsonl").exists());
    }

    /// 旧 vb 目录 → 渠道目录：目标尚不存在时整目录改名。
    #[test]
    fn legacy_vb_dir_is_renamed_when_target_missing() {
        let bot = format!("abb-test-{}", uuid::Uuid::new_v4());
        let chat = "oc_legacy_only";
        let legacy = crate::workspace_dir(&bot)
            .join("vb")
            .join(crate::virtualbot::vb_uuid(&bot, chat));
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("sessions.json"), "{}").unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(crate::workspace_dir(&bot));

        let dir = channel_dir(&bot, chat);
        ensure_channel_dir_at(&dir, &bot, chat, "group", None);
        assert!(dir.join("sessions.json").exists(), "旧目录内容必须搬过来");
        assert!(!legacy.exists(), "旧目录应已改名");
    }

    /// 并入时不覆盖目标已有条目（定向冲突以现有数据为准）。
    #[test]
    fn merge_keeps_existing_target_entries() {
        let from = std::env::temp_dir().join(format!("abb-merge-from-{}", uuid::Uuid::new_v4()));
        let to = std::env::temp_dir().join(format!("abb-merge-to-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("same"), "from").unwrap();
        std::fs::write(to.join("same"), "to").unwrap();
        std::fs::write(from.join("only-from"), "x").unwrap();
        let (moved, skipped) = merge_missing_entries(&from, &to);
        assert_eq!((moved, skipped), (1, 1));
        assert_eq!(
            std::fs::read_to_string(to.join("same")).unwrap(),
            "to",
            "冲突不得覆盖目标"
        );
        assert!(to.join("only-from").exists());
        let _ = std::fs::remove_dir_all(&from);
        let _ = std::fs::remove_dir_all(&to);
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

    /// 卡 `abb-symlink-hardening-20261028`：`chat.json` 的原子写不得跟随预置符号链接。
    ///
    /// 这个目录就在**工作区内**（`workspace_dir(bot_key).join(dir_name(chat_id))`），
    /// 是同一漏洞类里可达性最高的一处。旧实现用固定 `.chat.json.tmp` + `fs::write`
    /// （O_CREAT|O_TRUNC，会跟随链接）⇒ 预放 `→ 工作区外文件` 就能让桥下次写 chat.json
    /// 时覆盖它。改用 uuid 唯一 tmp 名后预放不可预测。
    #[cfg(unix)]
    #[test]
    fn chat_json_write_does_not_follow_preseeded_symlink() {
        let tmp = std::env::temp_dir().join(format!("abb-chandir-sym-{}", uuid::Uuid::new_v4()));
        let dir = tmp.join("oc_sym");
        let _ = std::fs::create_dir_all(&dir);
        let victim = tmp.join("victim.txt");
        std::fs::write(&victim, "ORIGINAL").unwrap();
        // 旧实现的固定中间名
        std::os::unix::fs::symlink(&victim, dir.join(".chat.json.tmp")).unwrap();

        ensure_channel_dir_at(&dir, "bot_a", "oc_sym", "group", None);

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "ORIGINAL",
            "预置的 .chat.json.tmp 符号链接不得被跟随，否则工作区外文件被覆盖"
        );
        // 真内容要落到位（不是靠写失败通过的）
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("chat.json")).unwrap()).unwrap();
        assert_eq!(v["chat_id"], "oc_sym", "chat.json 应正常落盘");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
