//! 待处理消息持久化 —— 崩溃/重启恢复（in-flight recovery）。
//! 消息进入 agent 处理前落盘 `workspaces/<bot>/pending.json`，处理完删除；
//! service 重启后扫描残留并自动重放（`Bridge::recover_pending`），实现
//! 「重启后自动续跑上次会话」：不丢消息、不丢上下文。
//!
//! 设计要点：
//! - 落盘时机在「/new、停止词」拦截**之后**：控制指令处理极快且语义是即时动作，
//!   重启丢了让用户重发即可；若把停止词落盘，重放时任务已不在跑，会被当普通消息
//!   透传给 agent，违背用户「叫停」意图。
//! - 三阶段时序（阶段 1 起）：`add`（进 agent 前）→ `set_reply`（回复产出后，仅
//!   Reply 臂）→ 发送 → `remove`（仅发送成功）。各窗口恢复语义：
//!   - add 后 set_reply 前崩溃 → 普通重放重跑（W1，at-least-once 现状）；
//!   - set_reply 后发送前崩溃 → 补发回复不重跑（W2 修复）；
//!   - 发送成功后 remove 前崩溃 → 重启**补发一条重复回复**（at-least-once，仅重发
//!     文本，严格优于 W1 的重跑副作用；用户可见双发，接受）；
//!   - Cancelled/Err 臂：先 remove 再发送（无回复可补发；崩溃窗口丢失的只是停止/
//!     错误通知，任务不会重启续跑——违背叫停语义的窗口已堵）。
//! - 附件只存 `AttachmentMeta`（文件已下载到工作区，重启后路径仍可读），不重新下载。

use crate::attachments::AttachmentMeta;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 一条待恢复的消息（重建 `Ev` 所需字段齐全）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingItem {
    pub mid: String,
    pub chat_id: String,
    pub chat_type: String,
    pub thread_id: String,
    /// 已剥 @提及 的文本（重放时 `handle` 再剥一次是幂等安全的）。
    pub text: String,
    /// 被引用消息的内容（文本 + 已下载附件；引用/回复场景）。serde default 兼容旧 pending.json。
    #[serde(default)]
    pub quoted: crate::messenger::QuotedContent,
    pub attachments: Vec<AttachmentMeta>,
    /// 发送者角色（重放时按原角色走受限/全权限 agent 分支）。
    /// serde default 兼容旧 pending.json（无角色时代 → Owner 全权限，与现状一致）。
    #[serde(default)]
    pub role: crate::config::SenderRole,
    /// 入队时间（unix 秒），启动重放按此排序保持原先后顺序。
    pub created_at: u64,
    /// 已产出的最终回复（阶段 1：W2 崩溃窗口补发）。agent 返回后、发送前落盘——
    /// 重启恢复时看到 Some = 「回复已产出但未确认发出」→ 直接补发，**不重跑 agent**
    /// （原语义 remove 在发送前，此窗口崩溃 = 消息丢）。发送成功后 remove 条目。
    /// serde default 兼容旧 pending.json（无此字段 = 未产出回复，恢复时重跑）。
    #[serde(default)]
    pub reply: Option<String>,
}

/// per-bot 的待处理队列（`workspaces/<bot>/pending.json`）。
/// 读写走内部 Mutex 串行化（多 chat 并发落盘/删除互不踩踏）。
pub struct PendingStore {
    path: PathBuf,
    data: Mutex<Vec<PendingItem>>,
}

impl PendingStore {
    pub fn new(bot_key: &str) -> PendingStore {
        let dir = crate::workspace_dir(bot_key);
        let _ = fs::create_dir_all(&dir);
        Self::at(dir.join("pending.json"))
    }

    /// 按指定路径构造（生产/测试共用）。
    fn at(path: PathBuf) -> PendingStore {
        let data = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<PendingItem>>(&t).ok())
            .unwrap_or_default();
        PendingStore {
            path,
            data: Mutex::new(data),
        }
    }

    /// 入队一条消息。同 mid 已存在（启动重放再次入队）时先删后插，保持单条。
    pub fn add(&self, item: PendingItem) {
        let mut data = self.data.lock().unwrap();
        data.retain(|p| p.mid != item.mid);
        data.push(item);
        self.save(&data);
    }

    /// 任务已完成，按 mid 移除。
    pub fn remove(&self, mid: &str) {
        let mut data = self.data.lock().unwrap();
        let before = data.len();
        data.retain(|p| p.mid != mid);
        if data.len() != before {
            self.save(&data);
        }
    }

    /// 记录该消息已产出的最终回复（agent 返回后、发送前调用）：条目保留在盘上，
    /// 重启恢复时据此补发而非重跑。已移除（发送成功）时静默忽略。
    pub fn set_reply(&self, mid: &str, reply: &str) {
        let mut data = self.data.lock().unwrap();
        if let Some(p) = data.iter_mut().find(|p| p.mid == mid) {
            p.reply = Some(reply.to_string());
            self.save(&data);
        }
    }

    /// 启动恢复用快照：按入队时间升序（同秒按原顺序）。
    pub fn snapshot(&self) -> Vec<PendingItem> {
        let mut items = self.data.lock().unwrap().clone();
        items.sort_by_key(|p| p.created_at);
        items
    }

    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 原子写盘（tmp + rename），避免崩溃留半截 json。
    fn save(&self, data: &[PendingItem]) {
        if let Ok(text) = serde_json::to_string_pretty(data) {
            let _ = crate::atomic_write_text(&self.path, &text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(mid: &str, at: u64) -> PendingItem {
        PendingItem {
            mid: mid.into(),
            chat_id: "oc_x".into(),
            chat_type: "group".into(),
            thread_id: String::new(),
            text: "hi".into(),
            quoted: crate::messenger::QuotedContent::default(),
            attachments: Vec::new(),
            role: crate::config::SenderRole::Owner,
            created_at: at,
            reply: None,
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("abb-pending-test-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn add_remove_roundtrip_persists() {
        let p = temp_path("roundtrip");
        let store = PendingStore::at(p.clone());
        store.add(item("m1", 1));
        store.add(item("m2", 2));
        assert_eq!(store.len(), 2);

        // 新实例重读（模拟重启）→ 数据仍在
        let reloaded = PendingStore::at(p.clone());
        assert_eq!(reloaded.len(), 2);

        reloaded.remove("m1");
        assert_eq!(reloaded.len(), 1);
        let reloaded2 = PendingStore::at(p.clone());
        assert_eq!(reloaded2.len(), 1);
        assert_eq!(reloaded2.snapshot()[0].mid, "m2");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn add_same_mid_dedupes() {
        let p = temp_path("dedupe");
        let store = PendingStore::at(p.clone());
        store.add(item("m1", 1));
        store.add(item("m1", 2)); // 重放再次入队同 mid → 保持单条
        assert_eq!(store.len(), 1);
        assert_eq!(store.snapshot()[0].created_at, 2);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_reply_persists_and_removed_silently_ignored() {
        // 阶段 1：回复产出后落盘（W2 补发依据）——重读仍带 reply；已移除条目静默忽略。
        let p = temp_path("reply");
        let store = PendingStore::at(p.clone());
        store.add(item("m1", 1));
        store.set_reply("m1", "最终回复");
        // 新实例重读（模拟重启）→ reply 仍在
        let reloaded = PendingStore::at(p.clone());
        assert_eq!(
            reloaded.snapshot()[0].reply.as_deref(),
            Some("最终回复"),
            "reply 落盘往返"
        );
        // 已移除 → 静默忽略不 panic、不新建
        reloaded.remove("m1");
        reloaded.set_reply("m1", "不应落盘");
        assert!(PendingStore::at(p.clone()).is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn snapshot_sorted_by_created_at() {
        let p = temp_path("sort");
        let store = PendingStore::at(p.clone());
        store.add(item("m3", 30));
        store.add(item("m1", 10));
        store.add(item("m2", 20));
        let mids: Vec<String> = store.snapshot().iter().map(|p| p.mid.clone()).collect();
        assert_eq!(mids, ["m1", "m2", "m3"]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn old_json_without_quoted_still_loads() {
        // #25 之前落盘的 pending.json 没有 quoted 字段：serde default 必须兼容，不能崩。
        let p = temp_path("legacy");
        std::fs::write(
            &p,
            r#"[{"mid":"m1","chat_id":"oc_x","chat_type":"group","thread_id":"","text":"旧消息","attachments":[],"created_at":1}]"#,
        )
        .unwrap();
        let store = PendingStore::at(p.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(store.snapshot()[0].mid, "m1");
        assert!(
            store.snapshot()[0].quoted.text.is_empty()
                && store.snapshot()[0].quoted.attachments.is_empty(),
            "旧文件缺 quoted 应默认空"
        );
        // 旧文件无 reply 字段 → 默认 None（恢复时重跑，与阶段 1 前语义一致）
        assert!(
            store.snapshot()[0].reply.is_none(),
            "旧文件缺 reply 应默认 None"
        );
        // 旧文件无 role 字段 → 默认 Owner（重放时走全权限，与现状一致）
        assert_eq!(store.snapshot()[0].role, crate::config::SenderRole::Owner);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remove_unknown_mid_is_noop() {
        let p = temp_path("noop");
        let store = PendingStore::at(p.clone());
        store.add(item("m1", 1));
        store.remove("nope");
        assert_eq!(store.len(), 1);
        let _ = std::fs::remove_file(&p);
    }
}
