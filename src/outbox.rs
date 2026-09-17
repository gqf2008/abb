//! 待发积压（pending_outbox）—— 微信主动推送（定时任务报告）受 context_token 会话活跃度约束：
//! 数小时无用户活动时，微信侧对 bot 主动推送返回 `ret=-2 prepare failed`（token stale），
//! 且同 token 重试必然再失败。此时把消息落盘缓存（`workspaces/<bot>/pending_outbox.json`），
//! 等用户下一条入站（context_token 刷新）后一次性补发，避免任务报告静默丢失。
//! 只在微信通道启用：飞书/钉钉主动推送不受此限制，仍走「失败回落主会话」的既有路径。

use crate::messenger::Messenger;
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
/// 「buzz 频道 UUID」「空值」统一归一化为该 bot 的平台 receive_id，保证键命中。
///
/// 纯函数核心（可测，幂等）：`chat_id` 非空、非 bot key、且不形如 buzz 频道 UUID 时
/// 原样返回（真实平台 id 一律保留）；否则回落到 `platform`（该 bot 的平台 receive_id）。
/// `platform` 为空（配置缺失 / 单测）时按原值返回——绝不把 chat 清空。
pub fn normalize_delivery_chat(chat_id: &str, bot_key: &str, platform: &str) -> String {
    let c = chat_id.trim();
    let suspect = c.is_empty() || c == bot_key || crate::buzz::keys::looks_like_channel_uuid(c);
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

/// 本次补发「零命中」时的提示行（store 非空却没有该 chat 的积压）。
/// 空 store（正常无积压）返回 None——保持安静；非空则返回含 store 总数与本次 chat 的
/// 行，杜绝「入了队却永远 attempts=0 且零日志」这状态（缺陷 abb-wx-outbox-flush-mismatch）。
fn no_match_note(total: usize, chat_id: &str) -> Option<String> {
    if total == 0 {
        None
    } else {
        Some(format!(
            "[outbox] ⚠️ 本次补发零命中：chat={} 但积压 store 共 {} 条（键不匹配或已被并发取出）——检查入队键/补发键是否经 resolve_delivery_chat 归一化一致",
            crate::agent::truncate(chat_id, 16),
            total
        ))
    }
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
    pub fn add(&self, item: OutboxItem) {
        let mut d = self.data.lock().unwrap();
        if d.iter().any(|x| x.id == item.id) {
            return;
        }
        d.push(item);
        self.persist(&d);
    }

    /// 取出某 chat 的全部积压并落盘（补发期间新入队的项不动，留待下次）。
    pub fn take(&self, chat_id: &str) -> Vec<OutboxItem> {
        let mut d = self.data.lock().unwrap();
        let (keep, taken): (Vec<_>, Vec<_>) = d.drain(..).partition(|x| x.chat_id != chat_id);
        *d = keep;
        if !taken.is_empty() {
            self.persist(&d);
        }
        taken
    }

    /// 补发失败后放回（attempts+1，同 id 幂等）。
    pub fn requeue(&self, mut item: OutboxItem) {
        item.attempts += 1;
        self.add(item);
    }

    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 补发某 chat 的积压：逐条 send_text，成功移除、失败 requeue 保留（下次入站再试）。
/// 返回成功条数。调用方需保证 per-chat 串行（与 handle 共用同一把锁），避免并发补发交错。
///
/// 缺陷 abb-wx-outbox-flush-mismatch-20260917：此前对空 items 直接 `return 0`，而
/// `OutboxStore::take` 是逐字相等 partition——入队键（bot key / 频道 UUID）与补发键
/// （平台 receive_id）不一致时永远 take 到 0 条且**一条日志都不打**，积压成永久黑洞。
/// 现在 store 非空却本次零命中时一律 loud 一条（见 [`no_match_note`]）。
pub async fn flush_pending(msgr: &dyn Messenger, store: &OutboxStore, chat_id: &str) -> usize {
    let items = store.take(chat_id);
    if items.is_empty() {
        // take 已把命中项取走；此刻 len 即「零命中」时未动的积压总数
        if let Some(note) = no_match_note(store.len(), chat_id) {
            crate::log!("{note}");
        }
        return 0;
    }
    let mut ok = 0;
    for item in items {
        let job_tag = if item.job_id.is_empty() {
            "-".to_string()
        } else {
            item.job_id[..item.job_id.len().min(8)].to_string()
        };
        match msgr.send_text(&item.chat_id, &item.text).await {
            Ok(()) => {
                ok += 1;
                crate::log!(
                    "[outbox] 补发成功 id={} job={} chat={} 长度={}（attempts={}）",
                    &item.id[..item.id.len().min(8)],
                    job_tag,
                    &item.chat_id[..item.chat_id.len().min(10)],
                    item.text.chars().count(),
                    item.attempts
                );
            }
            Err(e) => {
                crate::log!(
                    "[outbox] ⚠️ 补发失败，保留积压待下次入站重试 id={} job={} chat={}: {e:#}",
                    &item.id[..item.id.len().min(8)],
                    job_tag,
                    &item.chat_id[..item.chat_id.len().min(10)]
                );
                store.requeue(item);
            }
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::sync::atomic::{AtomicBool, Ordering};

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

    /// 测试假 messenger：记录发送，可配置失败。
    struct FakeMsgr {
        sent: Mutex<Vec<(String, String)>>,
        fail: AtomicBool,
    }
    impl FakeMsgr {
        fn new() -> FakeMsgr {
            FakeMsgr {
                sent: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
            }
        }
    }
    #[async_trait::async_trait]
    impl Messenger for FakeMsgr {
        /// 附件不参与本组断言（trait 无默认实现——审查 #254 P2-2）。
        async fn send_attachment(
            &self,
            _chat_id: &str,
            _meta: &crate::attachments::AttachmentMeta,
        ) -> Result<()> {
            Ok(())
        }
        async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
            if self.fail.load(Ordering::Relaxed) {
                anyhow::bail!("模拟发送失败");
            }
            self.sent
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
            Ok(())
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
    fn take_removes_only_target_chat() {
        let (_d, store) = tmp_store("take");
        store.add(item("a", "u1"));
        store.add(item("b", "u2"));
        let taken = store.take("u1");
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].id, "a");
        assert_eq!(store.len(), 1);
        assert_eq!(store.take("u1").len(), 0);
    }

    #[test]
    fn requeue_keeps_id_and_bumps_attempts() {
        let (_d, store) = tmp_store("requeue");
        store.add(item("a", "u1"));
        let taken = store.take("u1");
        assert_eq!(taken.len(), 1);
        store.requeue(taken[0].clone());
        assert_eq!(store.len(), 1);
        let again = store.take("u1");
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].attempts, 1);
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
        assert_eq!(store.len(), 1);
        assert_eq!(store.take("u1")[0].id, "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn flush_sends_and_removes() {
        let (_d, store) = tmp_store("flush");
        store.add(item("a", "u1"));
        store.add(item("b", "u1"));
        let msgr = FakeMsgr::new();
        let ok = flush_pending(&msgr, &store, "u1").await;
        assert_eq!(ok, 2);
        assert_eq!(msgr.sent.lock().unwrap().len(), 2);
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn flush_failure_requeues() {
        let (_d, store) = tmp_store("flush-fail");
        store.add(item("a", "u1"));
        let msgr = FakeMsgr::new();
        msgr.fail.store(true, Ordering::Relaxed);
        let ok = flush_pending(&msgr, &store, "u1").await;
        assert_eq!(ok, 0);
        assert_eq!(store.len(), 1); // 失败项保留
        assert_eq!(store.take("u1")[0].attempts, 1);
    }

    // ── 缺陷 abb-wx-outbox-flush-mismatch-20260917：入队键 == 补发键 ──

    const WX_PLATFORM: &str = "o9cq806Evm1T9LW4PNihIL11j2cE@im.wechat";
    const WX_BOT_KEY: &str = "o9cq806Evm1T9LW4PNihIL11j2cEimwechat";
    const BUZZ_UUID: &str = "a1466a78-1111-2222-3333-9f980edf9ee8";

    #[test]
    fn normalize_maps_bot_key_uuid_and_empty_to_platform_id() {
        // 三种「坏」写法（bot key / buzz 频道 UUID / 空）都归一化为平台 receive_id
        for bad in [WX_BOT_KEY, BUZZ_UUID, ""] {
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
        // 端到端：按归一化键入队，按另一写法归一化后补发取得到
        let (_d, store) = tmp_store("keyeq");
        store.add(item("a", &enq));
        assert_eq!(store.len(), 1);
        assert_eq!(store.take(&flush).len(), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn no_match_note_is_loud_only_when_store_nonempty() {
        assert!(no_match_note(0, "u1").is_none(), "空 store 保持安静");
        let note = no_match_note(3, "u1").expect("非空 store 零命中须提示");
        assert!(
            note.contains("u1") && note.contains('3'),
            "含本次 chat 与 store 总数: {note}"
        );
    }

    #[tokio::test]
    async fn flush_zero_match_is_not_silent_and_keeps_store() {
        // store 非空但键不匹配：不得误删、不得静默（现在会经 no_match_note 打一条日志）
        let (_d, store) = tmp_store("nomatch");
        store.add(item("a", WX_BOT_KEY)); // 坏键（未归一化的 bot key）
        let msgr = FakeMsgr::new();
        let ok = flush_pending(&msgr, &store, WX_PLATFORM).await;
        assert_eq!(ok, 0);
        assert_eq!(store.len(), 1, "未命中不误删");
        assert!(msgr.sent.lock().unwrap().is_empty());
        assert!(
            no_match_note(store.len(), WX_PLATFORM).is_some(),
            "非空 store 零命中必须产生日志行（杜绝静默黑洞）"
        );
    }
}
