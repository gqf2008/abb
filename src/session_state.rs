//! 会话管控状态（#87）——暂停/恢复的持久化 + 热重载。
//!
//! - 存储：`~/.agent-bridge/session_state.json`
//!   `{"paused": {"<bot_key>": {"<chat_key>": {"since": unix_secs, "by": "操作者"}}}}`
//! - 粒度：会话 key（chat_id，或 `chat_id:thread_id` 话题形态——与 sessions/history
//!   同 key）。判定时对话题 key 回落 chat_id 前缀——暂停整个群 = 群内所有话题一并静音。
//! - 热重载（mtime+size 签名，JobStore/SessionStore 同款）：CLI pause/resume 落盘后
//!   service 无需重启即生效；跨进程无锁安全（原子写整文件）。
//! - 写入：`atomic_write_sensitive`（uuid tmp + rename + 0600 unix），对齐 config.json 档位。
//! - 审计：管控动作追加 `logs/audit.log`（操作者、时间、bot、chat、动作），见 [`audit`]。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 单条暂停记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PauseInfo {
    /// 暂停时刻（unix 秒）。
    #[serde(default)]
    pub since: i64,
    /// 操作者标识（CLI = 来源 bot key / "cli"）。
    #[serde(default)]
    pub by: String,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    paused: HashMap<String, HashMap<String, PauseInfo>>,
}

/// 会话管控状态句柄（生产/测试共用 `at` 注入路径）。
pub struct SessionState {
    path: PathBuf,
    data: Mutex<State>,
    loaded_sig: Mutex<Option<(std::time::SystemTime, u64)>>,
}

impl SessionState {
    pub fn production() -> SessionState {
        Self::at(crate::bridge_dir().join("session_state.json"))
    }

    /// 按指定路径构造（生产/测试共用；测试注入临时路径隔离）。
    pub fn at(path: PathBuf) -> SessionState {
        let data = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let sig = fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        SessionState {
            path,
            data: Mutex::new(data),
            loaded_sig: Mutex::new(sig),
        }
    }

    /// 热重载：文件 (mtime, size) 比上次加载新（CLI/外部进程改了）→ 重读。
    /// 与 SessionStore::refresh 同款签名判定（mtime+size 双保险，缓解同 tick 精度漏检）。
    fn refresh(&self) {
        let cur = fs::metadata(&self.path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        let stale = { *self.loaded_sig.lock().unwrap() != cur };
        if !stale {
            return;
        }
        if let Ok(text) = fs::read_to_string(&self.path) {
            if let Ok(data) = serde_json::from_str::<State>(&text) {
                *self.data.lock().unwrap() = data;
                // 只有解析成功才推进 sig：临时读失败/外部写了坏文件时不吞掉重试机会
                *self.loaded_sig.lock().unwrap() = cur;
            }
        }
    }

    /// 会话是否暂停。key 精确命中，或话题 key（含 ':'）回落 chat_id 前缀命中。
    pub fn is_paused(&self, bot_key: &str, key: &str) -> bool {
        self.refresh();
        let data = self.data.lock().unwrap();
        if let Some(m) = data.paused.get(bot_key) {
            if m.contains_key(key) {
                return true;
            }
            if let Some(idx) = key.find(':') {
                if m.contains_key(&key[..idx]) {
                    return true;
                }
            }
        }
        false
    }

    /// 暂停会话。已暂停返回 false（幂等，不重复记）。
    pub fn pause(&self, bot_key: &str, key: &str, by: &str) -> bool {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let m = data.paused.entry(bot_key.to_string()).or_default();
        if m.contains_key(key) {
            return false;
        }
        m.insert(
            key.to_string(),
            PauseInfo {
                since: crate::chrono_lite::unix_secs() as i64,
                by: by.to_string(),
            },
        );
        drop(data);
        self.save()
    }

    /// 恢复会话。未暂停返回 false（幂等）。
    pub fn resume(&self, bot_key: &str, key: &str) -> bool {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let Some(m) = data.paused.get_mut(bot_key) else {
            return false;
        };
        if m.remove(key).is_none() {
            return false;
        }
        let empty = m.is_empty();
        drop(data);
        if empty {
            self.remove_empty_bot(bot_key);
        }
        self.save()
    }

    /// 删除会话时清暂停态：精确 key + 该 chat 的全部话题前缀（chat_id: 开头）。
    pub fn remove_chat(&self, bot_key: &str, key: &str) {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let Some(m) = data.paused.get_mut(bot_key) else {
            return;
        };
        let prefix = format!("{key}:");
        let before = m.len();
        m.retain(|k, _| k != key && !k.starts_with(&prefix));
        let changed = m.len() != before;
        let empty = m.is_empty();
        drop(data);
        if empty {
            self.remove_empty_bot(bot_key);
        }
        if changed {
            self.save();
        }
    }

    fn remove_empty_bot(&self, bot_key: &str) {
        let mut data = self.data.lock().unwrap();
        if data
            .paused
            .get(bot_key)
            .map(|m| m.is_empty())
            .unwrap_or(false)
        {
            data.paused.remove(bot_key);
        }
    }

    /// 某 bot 的全部暂停会话（CLI list 用），按 chat key 排序。
    pub fn paused_chats(&self, bot_key: &str) -> Vec<(String, PauseInfo)> {
        self.refresh();
        let data = self.data.lock().unwrap();
        data.paused
            .get(bot_key)
            .map(|m| {
                let mut v: Vec<(String, PauseInfo)> =
                    m.iter().map(|(k, p)| (k.clone(), p.clone())).collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                v
            })
            .unwrap_or_default()
    }

    /// 落盘（原子写敏感文件）。失败只 log（管控是增强能力，不 panic）。
    fn save(&self) -> bool {
        let data = self.data.lock().unwrap();
        let text = match serde_json::to_string_pretty(&*data) {
            Ok(t) => t,
            Err(e) => {
                crate::log!("[session-state] 序列化失败: {e:#}");
                return false;
            }
        };
        drop(data);
        match crate::atomic_write_sensitive(&self.path, &text) {
            Ok(()) => true,
            Err(e) => {
                crate::log!("[session-state] 写盘失败 {}: {e:#}", self.path.display());
                false
            }
        }
    }
}

/// 管控动作审计日志：追加 `logs/audit.log` 一行（操作者、时间、bot、chat、动作）。
/// 写失败只 log 警告（审计是增强能力，不阻塞主链路）。
pub fn audit(action: &str, bot_key: &str, chat: &str, by: &str, detail: &str) {
    let line = format!(
        "[{}] {} bot={} chat={} by={}{}",
        crate::chrono_lite::now(),
        action,
        bot_key,
        chat,
        by,
        if detail.is_empty() {
            String::new()
        } else {
            format!(" detail={detail}")
        }
    );
    crate::log!("[audit] {line}");
    let path = crate::bridge_dir().join("logs").join("audit.log");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let res = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")
        });
    if let Err(e) = res {
        crate::log!("[audit] 写审计日志失败 {}: {e:#}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("abb-sess-state-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn pause_resume_roundtrip_and_persistence() {
        let p = temp_path("rt");
        let s = SessionState::at(p.clone());
        assert!(!s.is_paused("b1", "oc_x"));
        assert!(s.pause("b1", "oc_x", "cli"));
        assert!(s.is_paused("b1", "oc_x"));
        // 重开句柄（模拟 CLI 写入后 service 重读）→ 状态持久
        let s2 = SessionState::at(p.clone());
        assert!(s2.is_paused("b1", "oc_x"));
        assert!(s2.resume("b1", "oc_x"));
        assert!(!s2.is_paused("b1", "oc_x"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pause_is_idempotent_and_resume_unknown_is_false() {
        let p = temp_path("idem");
        let s = SessionState::at(p.clone());
        assert!(s.pause("b1", "oc_x", "cli"));
        assert!(!s.pause("b1", "oc_x", "cli"), "重复暂停不重复记");
        assert!(!s.resume("b1", "oc_never"), "恢复未暂停会话返回 false");
        assert!(s.resume("b1", "oc_x"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn thread_key_falls_back_to_chat_prefix() {
        let p = temp_path("thread");
        let s = SessionState::at(p.clone());
        s.pause("b1", "oc_x", "cli");
        // 群暂停 → 话题消息同样命中
        assert!(s.is_paused("b1", "oc_x:omt_123"));
        // 其它 bot 不受影响
        assert!(!s.is_paused("b2", "oc_x"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remove_chat_clears_thread_prefixed_pauses() {
        let p = temp_path("rm");
        let s = SessionState::at(p.clone());
        s.pause("b1", "oc_x", "cli");
        s.pause("b1", "oc_x:omt_1", "cli");
        s.pause("b1", "oc_y", "cli");
        s.remove_chat("b1", "oc_x");
        assert!(!s.is_paused("b1", "oc_x"));
        assert!(!s.is_paused("b1", "oc_x:omt_1"));
        assert!(s.is_paused("b1", "oc_y"), "其它会话不受影响");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn paused_chats_lists_sorted() {
        let p = temp_path("list");
        let s = SessionState::at(p.clone());
        s.pause("b1", "oc_b", "cli");
        s.pause("b1", "oc_a", "cli");
        let v = s.paused_chats("b1");
        assert_eq!(
            v.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["oc_a", "oc_b"]
        );
        assert!(s.paused_chats("b2").is_empty());
        let _ = std::fs::remove_file(&p);
    }
}
