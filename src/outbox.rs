//! 待发积压（pending_outbox）落盘 + 键归一化。
//!
//! 历史背景：微信主动推送（定时任务报告）受 context_token 会话活跃度约束——数小时无
//! 用户活动时，微信侧对 bot 主动推送返回 `ret=-2 prepare failed`（token stale），同 token
//! 重试必然再失败。那时把消息落盘缓存（`workspaces/<bot>/pending_outbox.json`），等用户
//! 下一条入站（context_token 刷新）后一次性补发。
//!
//! **补发侧已随内置微信渠道退役**（2026-11-04）：`take`/`requeue`/`flush_pending` 与
//! `Bridge::flush_outbox` 一起删掉了——它们的唯一入口是微信入站消息，渠道搬走后无人调用。
//! 现在只剩**入队**（`add`）与**键归一化**（`resolve_delivery_chat`）：两条入队路径
//! （`Bridge::queue_outbox` / `deliver::fail_text`）都仍以 `is_wechat()` 为门，故本机现状
//! 不会真的入队。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 一条待补发的消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxItem {
    /// 去重键（幂等：同 id 不重复入队，补发失败重入队时防重复）。
    pub id: String,
    /// 目标会话：微信 = ilink_user_id。
    pub chat_id: String,
    /// 完整消息文本（补发时按通道分段逻辑再切，这里存整条）。
    pub text: String,
    /// 入队时间（unix 秒），对账用。
    pub created_at: u64,
    /// 已尝试补发次数（排查用；不用于丢弃——积压消息「不丢失」优先）。
    #[serde(default)]
    pub attempts: u32,
    /// 来源任务 id（对得上 jobs.json；非任务来源可为空）。
    #[serde(default)]
    pub job_id: String,
}

/// 入队键 / 补发键归一化（缺陷 abb-wx-outbox-flush-mismatch-20260917）。
///
/// 背景：`queue_outbox` 入队用的 chat 来自任务注入的 `AGENT_BRIDGE_CHAT_ID`——本机
/// 现场是 **bot key**（`o9cq…imwechat`）；而 `Bridge::flush_outbox` 补发用的 chat 来自
/// 入站消息的 `from_user_id`——**微信平台 id**（`o9cq…@im.wechat`）。`OutboxStore::take`
/// 是逐字相等 partition，两者不相等 → 永远 take 到 0 条、attempts 恒为 0、且此前对空
/// items 静默 return → 积压成永久黑洞。修复：入队与补发共用本函数，把「bot key」
/// 「ABB 频道 UUID」「空值」统一归一化为该 bot 的平台 receive_id，保证键命中。
///
/// 纯函数核心（可测，幂等）：`chat_id` 非空、非 bot key、且不形如 ABB 频道 UUID 时
/// 原样返回（真实平台 id 一律保留）；否则回落到 `platform`（该 bot 的平台 receive_id）。
/// `platform` 为空（配置缺失 / 单测）时按原值返回——绝不把 chat 清空。
pub fn normalize_delivery_chat(chat_id: &str, bot_key: &str, platform: &str) -> String {
    let c = chat_id.trim();
    let suspect = c.is_empty() || c == bot_key || crate::runtime::keys::looks_like_channel_uuid(c);
    if suspect && !platform.is_empty() {
        platform.to_string()
    } else {
        chat_id.to_string()
    }
}

/// 该 bot 的平台 receive_id：微信 = `wx_owner()`（ilink_user_id，即主会话），
/// 其它平台 = `primary_chat_id`。读不到配置或 bot 不存在 → 空串（调用方据此不归一化）。
pub fn platform_receive_id(bot_key: &str) -> String {
    let Ok(c) = crate::config::Config::load() else {
        return String::new();
    };
    let Some(b) = c.bots.iter().find(|b| b.key() == bot_key) else {
        return String::new();
    };
    if b.is_wechat() && !b.wx_owner().is_empty() {
        return b.wx_owner().to_string();
    }
    b.primary_chat_id.clone()
}

/// 生产入口：`normalize_delivery_chat(chat_id, bot_key, platform_receive_id(bot_key))`。
/// 入队侧（`queue_outbox` / `Router::fail_text`）与补发侧（`flush_outbox`）共用。
pub fn resolve_delivery_chat(bot_key: &str, chat_id: &str) -> String {
    normalize_delivery_chat(chat_id, bot_key, &platform_receive_id(bot_key))
}

pub struct OutboxStore {
    path: PathBuf,
    data: Mutex<Vec<OutboxItem>>,
}

impl OutboxStore {
    pub fn new(bot_key: &str) -> OutboxStore {
        // 目标 bot 可能从未跑过 agent（workspace 目录不存在）→ 先建目录，否则落盘静默失败
        let _ = fs::create_dir_all(crate::workspace_dir(bot_key));
        let path = crate::workspace_dir(bot_key).join("pending_outbox.json");
        let data = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        OutboxStore {
            path,
            data: Mutex::new(data),
        }
    }

    pub(crate) fn new_at(path: PathBuf) -> OutboxStore {
        let data = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        OutboxStore {
            path,
            data: Mutex::new(data),
        }
    }

    fn persist(&self, data: &[OutboxItem]) {
        if let Ok(text) = serde_json::to_string_pretty(data) {
            let _ = crate::atomic_write_text(&self.path, &text);
        }
    }

    /// 入队（幂等：同 id 已存在则跳过）。
    ///
    /// 落盘前**先合并磁盘上的现状**（2026-10-05 审计 #21）：本 store 的内存快照可能已陈旧
    /// —— 长驻的 `Bridge.outbox` 与每次新建的 CLI store（deliver）同时写同一个
    /// `pending_outbox.json`，而 `persist` 是**整文件覆盖** ⇒ 后写者会把先写者刚入队的项
    /// 静默抹掉（微信任务报告/跨会话消息永远发不出去）。合并规则：磁盘 ∪ 本快照，按 id 去重。
    pub fn add(&self, item: OutboxItem) {
        let mut d = self.data.lock().unwrap();
        if let Ok(text) = fs::read_to_string(&self.path) {
            if let Ok(disk) = serde_json::from_str::<Vec<OutboxItem>>(&text) {
                for it in disk {
                    if !d.iter().any(|x| x.id == it.id) {
                        d.push(it);
                    }
                }
            }
        }
        if d.iter().any(|x| x.id == item.id) {
            return;
        }
        d.push(item);
        self.persist(&d);
    }

    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试临时目录：Drop 时自动清理。
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp_store(name: &str) -> (TempDir, OutboxStore) {
        // 测试用临时目录（无 tempfile 依赖，手工建/清）
        let dir = std::env::temp_dir().join(format!(
            "abb-outbox-test-{}-{}",
            name,
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pending_outbox.json");
        (TempDir(dir), OutboxStore::new_at(path))
    }

    fn item(id: &str, chat: &str) -> OutboxItem {
        OutboxItem {
            id: id.to_string(),
            chat_id: chat.to_string(),
            text: "hello".to_string(),
            created_at: 1,
            attempts: 0,
            job_id: String::new(),
        }
    }

    #[test]
    fn add_dedupes_by_id() {
        let (_d, store) = tmp_store("dedupe");
        store.add(item("a", "u1"));
        store.add(item("a", "u1"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn persists_across_reload() {
        let dir = std::env::temp_dir().join(format!(
            "abb-outbox-persist-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pending_outbox.json");
        {
            let store = OutboxStore::new_at(path.clone());
            store.add(item("a", "u1"));
        }
        let store = OutboxStore::new_at(path);
        assert_eq!(store.len(), 1, "重载后应从磁盘恢复积压项");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const WX_PLATFORM: &str = "o9cq806Evm1T9LW4PNihIL11j2cE@im.wechat";
    const WX_BOT_KEY: &str = "o9cq806Evm1T9LW4PNihIL11j2cEimwechat";
    const HIST_UUID: &str = "a1466a78-1111-2222-3333-9f980edf9ee8";

    #[test]
    fn normalize_maps_bot_key_uuid_and_empty_to_platform_id() {
        // 三种「坏」写法（bot key / ABB 频道 UUID / 空）都归一化为平台 receive_id
        for bad in [WX_BOT_KEY, HIST_UUID, ""] {
            assert_eq!(
                normalize_delivery_chat(bad, WX_BOT_KEY, WX_PLATFORM),
                WX_PLATFORM,
                "bad={bad:?} 应归一到平台 receive_id"
            );
        }
        // 真实平台 id 幂等（原样保留）
        assert_eq!(
            normalize_delivery_chat(WX_PLATFORM, WX_BOT_KEY, WX_PLATFORM),
            WX_PLATFORM
        );
        // platform 缺失时不破坏原值（绝不把 chat 清空）
        assert_eq!(
            normalize_delivery_chat(WX_BOT_KEY, WX_BOT_KEY, ""),
            WX_BOT_KEY
        );
        assert_eq!(normalize_delivery_chat("", WX_BOT_KEY, ""), "");
    }

    #[test]
    fn enqueue_key_equals_flush_key() {
        // 入队侧用 bot key、补发侧用平台 id——归一化后必须是同一把键，take 才能命中
        let enq = normalize_delivery_chat(WX_BOT_KEY, WX_BOT_KEY, WX_PLATFORM);
        let flush = normalize_delivery_chat(WX_PLATFORM, WX_BOT_KEY, WX_PLATFORM);
        assert_eq!(enq, flush);
        assert_eq!(enq, WX_PLATFORM);
        // 按归一化键入队确实落盘（补发侧随渠道退役，`store.take` 的命中验证不再适用）
        let (_d, store) = tmp_store("keyeq");
        store.add(item("a", &enq));
        assert_eq!(store.len(), 1);
    }
}

#[cfg(test)]
mod outbox_merge_guard_tests {
    /// 入队必须先合并磁盘现状（否则长驻 store 会把 CLI 刚入队的项整文件覆盖掉）。
    ///
    /// 判别力（2026-10-05 审计 #21）：把 `add` 里那段合并删掉（退回「直接 push + persist」）
    /// ⇒ 第一条断言红。
    #[test]
    fn add_merges_disk_state_before_persisting() {
        let src = include_str!("outbox.rs");
        assert!(
            src.contains("落盘前**先合并磁盘上的现状**"),
            "add 必须先合并磁盘现状再 persist（整文件覆盖会静默抹掉对方入队的项）"
        );
        let merge_read = concat!("if let Ok(text) = fs::read_to_string(&self.pa", "th) {");
        assert!(
            src.contains(merge_read),
            "add 内部必须读一次磁盘：{merge_read}"
        );
    }
}
