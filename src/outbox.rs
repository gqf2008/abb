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
pub async fn flush_pending(msgr: &dyn Messenger, store: &OutboxStore, chat_id: &str) -> usize {
    let items = store.take(chat_id);
    if items.is_empty() {
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
}
