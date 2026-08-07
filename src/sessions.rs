//! 会话持久化 —— ~/.agent-bridge/workspaces/<bot>/sessions.json。
//! 会话按后端区分（claude 的 --resume UUID 与 codex 的 thread_id 互不通用，共用一个槽位切后端必串）：
//! {chat_id: {claude: {session_id, started}, codex: {session_id, started}}}
//! 当前操作哪个后端的槽位由 SessionStore::new(current_backend, bot_key) 选定——即 per-bot 配置的后端。
//! 旧扁平格式 {chat_id: {backend, session_id, started}} load 时自动迁移到对应后端的槽位。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 单个后端的会话槽位：session_id + 是否已开过首轮（决定下轮 --resume 还是新建）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Slot {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub started: bool,
}

/// 一个 chat 的会话：按后端各存一份（缺省=该后端还没会话）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatEntry {
    #[serde(default, skip_serializing_if = "Slot::is_empty")]
    pub claude: Slot,
    #[serde(default, skip_serializing_if = "Slot::is_empty")]
    pub codex: Slot,
}

impl Slot {
    fn is_empty(&self) -> bool {
        self.session_id.is_empty() && !self.started
    }
}

/// 旧扁平格式（聊天切后端时代）：{backend, session_id, started}，仅用于 load 迁移。
#[derive(Deserialize)]
struct LegacyEntry {
    #[serde(default)]
    backend: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    started: bool,
}

pub struct SessionStore {
    path: PathBuf,
    data: Mutex<HashMap<String, ChatEntry>>,
    /// 当前 bot 生效后端（"claude"/"codex"）——决定各方法读写哪个槽位。
    current_backend: String,
}

impl SessionStore {
    pub fn new(current_backend: &str, bot_key: &str) -> SessionStore {
        let dir = crate::bridge_dir().join("workspaces").join(bot_key);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("sessions.json");
        let data = if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|t| Self::parse(&t))
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        SessionStore {
            path,
            data: Mutex::new(data),
            current_backend: current_backend.to_string(),
        }
    }

    /// 解析 sessions.json：先试新格式（按后端分槽），失败再按旧扁平格式迁移。
    fn parse(text: &str) -> Option<HashMap<String, ChatEntry>> {
        // 新格式直接命中（含真实的 claude/codex 键）才算数——serde 对旧扁平格式也会"解析成功"
        // （backend/session_id/started 被当未知键忽略、claude/codex 缺省为空），结果全空、迁移分支走不到。
        // 故只有解析出非空槽位才采纳新格式，否则回退旧格式迁移。
        if let Ok(m) = serde_json::from_str::<HashMap<String, ChatEntry>>(text) {
            let has_data = m
                .values()
                .any(|e| !e.claude.session_id.is_empty() || !e.codex.session_id.is_empty());
            if has_data || m.is_empty() {
                return Some(m);
            }
        }
        // 旧格式：{chat_id: {backend, session_id, started}} → 落到该 backend 的槽位
        let legacy = serde_json::from_str::<HashMap<String, LegacyEntry>>(text).ok()?;
        let mut out = HashMap::new();
        for (chat_id, e) in legacy {
            if e.session_id.is_empty() {
                continue;
            }
            let slot = Slot {
                session_id: e.session_id,
                started: e.started,
            };
            let mut entry = ChatEntry::default();
            if e.backend.eq_ignore_ascii_case("codex") {
                entry.codex = slot;
            } else {
                entry.claude = slot; // 默认/旧值一律归 claude
            }
            out.insert(chat_id, entry);
        }
        Some(out)
    }

    fn save_locked(&self, data: &HashMap<String, ChatEntry>) {
        // 原子写：tmp + rename（崩溃不留半截）
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(text) = serde_json::to_string_pretty(data) {
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, &self.path);
            }
        }
    }

    /// 取当前后端槽位的可变引用（没有则建默认槽位）。
    fn slot_mut<'a>(entry: &'a mut ChatEntry, backend: &str) -> &'a mut Slot {
        if backend.eq_ignore_ascii_case("codex") {
            &mut entry.codex
        } else {
            &mut entry.claude
        }
    }

    /// 返回该 chat 在当前后端的 session_id，没有则新建 UUID。
    pub fn ensure_session(&self, chat_id: &str) -> String {
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(chat_id.to_string()).or_default();
        let slot = Self::slot_mut(entry, &self.current_backend);
        if !slot.session_id.is_empty() {
            return slot.session_id.clone();
        }
        let sid = uuid::Uuid::new_v4().to_string();
        slot.session_id = sid.clone();
        slot.started = false;
        self.save_locked(&data);
        sid
    }

    pub fn mark_started(&self, chat_id: &str) {
        let mut data = self.data.lock().unwrap();
        if let Some(entry) = data.get_mut(chat_id) {
            Self::slot_mut(entry, &self.current_backend).started = true;
            self.save_locked(&data);
        }
    }

    /// 覆盖该 chat 当前后端的 session_id（codex 用：首轮 exec 抓到真实 thread_id 后回存，供后续 resume）。
    pub fn set_session_id(&self, chat_id: &str, session_id: &str) {
        let mut data = self.data.lock().unwrap();
        if let Some(entry) = data.get_mut(chat_id) {
            let slot = Self::slot_mut(entry, &self.current_backend);
            if slot.session_id != session_id {
                slot.session_id = session_id.to_string();
                self.save_locked(&data);
            }
        }
    }

    pub fn is_started(&self, chat_id: &str) -> bool {
        let data = self.data.lock().unwrap();
        data.get(chat_id)
            .map(|e| {
                if self.current_backend.eq_ignore_ascii_case("codex") {
                    e.codex.started
                } else {
                    e.claude.started
                }
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_flat_format() {
        // 旧扁平格式：{chat: {backend, session_id, started}}，且新格式 parse 必先失败才走这里
        let legacy = r#"{"oc_xxx": {"backend": "codex", "session_id": "tid-1", "started": true}}"#;
        let m = SessionStore::parse(legacy).expect("旧格式应可迁移");
        let e = &m["oc_xxx"];
        assert_eq!(e.codex.session_id, "tid-1");
        assert!(e.codex.started);
        assert!(e.claude.session_id.is_empty(), "claude 槽位应为空");
    }

    #[test]
    fn parses_per_backend_format() {
        let new = r#"{"oc_xxx": {"claude": {"session_id": "c-uuid", "started": true}, "codex": {"session_id": "x-tid", "started": false}}}"#;
        let m = SessionStore::parse(new).expect("新格式应解析");
        let e = &m["oc_xxx"];
        assert_eq!(e.claude.session_id, "c-uuid");
        assert_eq!(e.codex.session_id, "x-tid");
    }

    #[test]
    fn backends_are_independent() {
        // 同一 chat 的 claude/codex 槽位互不干扰（切后端不串）
        let new = r#"{"c": {"claude": {"session_id": "claude-uuid", "started": true}}}"#;
        let m = SessionStore::parse(new).unwrap();
        let e = &m["c"];
        assert_eq!(e.claude.session_id, "claude-uuid");
        assert!(e.codex.session_id.is_empty());
    }
}
