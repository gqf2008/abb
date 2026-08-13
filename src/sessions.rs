//! 会话持久化 —— ~/.agent-bridge/workspaces/<bot>/sessions.json。
//! 会话按后端区分（claude 的 --resume UUID、codex 的 thread_id、pi 的 --session-id UUID 互不通用，
//! 共用一个槽位切后端必串）：
//! {chat_id: {claude: {session_id, started}, codex: {session_id, started}, pi: {session_id, started}}}
//! 当前操作哪个后端的槽位由 SessionStore::new(current_backend, bot_key) 选定——即 per-bot 配置的后端。
//! 旧扁平格式 {chat_id: {backend, session_id, started}} load 时自动迁移到对应后端的槽位。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

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
    #[serde(default, skip_serializing_if = "Slot::is_empty")]
    pub pi: Slot,
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
    /// 上次加载时的文件签名 (mtime, size)。CLI/外部改 sessions.json 后按签名热重载
    /// （#23），无需重启 service 即生效——复用 JobStore 的 refresh 模式；size 与 mtime
    /// 双重判定以缓解同 tick 内 mtime 精度不足的漏检（审查 P3-2）。
    loaded_sig: Mutex<Option<(SystemTime, u64)>>,
}

impl SessionStore {
    pub fn new(current_backend: &str, bot_key: &str) -> SessionStore {
        let dir = crate::bridge_dir().join("workspaces").join(bot_key);
        let _ = fs::create_dir_all(&dir);
        Self::at(current_backend, dir.join("sessions.json"))
    }

    /// 按指定路径构造（生产/测试共用）。
    fn at(current_backend: &str, path: PathBuf) -> SessionStore {
        let data = if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|t| Self::parse(&t))
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        let sig = fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        SessionStore {
            path,
            data: Mutex::new(data),
            current_backend: current_backend.to_string(),
            loaded_sig: Mutex::new(sig),
        }
    }

    #[cfg(test)]
    fn new_at(current_backend: &str, path: PathBuf) -> SessionStore {
        Self::at(current_backend, path)
    }

    /// 若 sessions.json 的 (mtime, size) 比上次加载新（CLI/外部进程改了），重新读盘。
    /// 每次公开方法前调用，保证「运行中改文件即时生效」。size 与 mtime 双重判定，
    /// 缓解单 mtime 在同 tick 内精度不足的漏检（审查 P3-2）。
    ///
    /// 已知限制（审查 P3-1b）：mtime+size 是最终一致检测，非强一致——本进程在
    /// 「refresh 读盘 → 改内存 → save 写盘」之间若另一进程改盘，本进程 save 会覆盖之
    /// （lost update）。彻底修复需进程间文件锁（advisory lock），且 JobStore 同模式同问题，
    /// 宜独立架构升级；reset 幂等（丢失可重试），实际窗口在毫秒级同步路径内。
    fn refresh(&self) {
        let cur = fs::metadata(&self.path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        let stale = { *self.loaded_sig.lock().unwrap() != cur };
        if !stale {
            return;
        }
        if let Ok(text) = fs::read_to_string(&self.path) {
            if let Some(data) = Self::parse(&text) {
                *self.data.lock().unwrap() = data;
                // 只有解析成功才推进 sig：临时读失败/外部写了坏文件时不吞掉重试机会
                *self.loaded_sig.lock().unwrap() = cur;
            }
        }
    }

    /// 解析 sessions.json：先试新格式（按后端分槽），失败再按旧扁平格式迁移。
    fn parse(text: &str) -> Option<HashMap<String, ChatEntry>> {
        // 新格式直接命中（含真实的 claude/codex 键）才算数——serde 对旧扁平格式也会"解析成功"
        // （backend/session_id/started 被当未知键忽略、claude/codex 缺省为空），结果全空、迁移分支走不到。
        // 故只有解析出非空槽位才采纳新格式，否则回退旧格式迁移。
        if let Ok(m) = serde_json::from_str::<HashMap<String, ChatEntry>>(text) {
            let has_data = m.values().any(|e| {
                !e.claude.session_id.is_empty()
                    || !e.codex.session_id.is_empty()
                    || !e.pi.session_id.is_empty()
            });
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
            } else if e.backend.eq_ignore_ascii_case("pi") {
                entry.pi = slot;
            } else {
                entry.claude = slot; // 默认/旧值一律归 claude
            }
            out.insert(chat_id, entry);
        }
        Some(out)
    }

    fn save_locked(&self, data: &HashMap<String, ChatEntry>) {
        // 原子写：唯一 tmp + rename（崩溃不留半截；唯一 tmp 避免 CLI reset 与 service
        // 写盘并发时互相覆盖同一 tmp 文件）
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
        if let Ok(text) = serde_json::to_string_pretty(data) {
            if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &self.path).is_ok() {
                // 写完即推进 sig，避免下次公开方法再读盘（磁盘==内存）
                *self.loaded_sig.lock().unwrap() = fs::metadata(&self.path)
                    .ok()
                    .and_then(|m| Some((m.modified().ok()?, m.len())));
                return;
            }
        }
        let _ = fs::remove_file(&tmp);
    }

    /// 取当前后端槽位的可变引用（没有则建默认槽位）。
    fn slot_mut<'a>(entry: &'a mut ChatEntry, backend: &str) -> &'a mut Slot {
        if backend.eq_ignore_ascii_case("codex") {
            &mut entry.codex
        } else if backend.eq_ignore_ascii_case("pi") {
            &mut entry.pi
        } else {
            &mut entry.claude
        }
    }

    /// 返回该 chat 在当前后端的 session_id，没有则新建 UUID。
    ///
    /// 生产 bridge 已改用 `ensure_with_started` 合并快照（审查 P3-1a）；此方法保留作
    /// 细粒度公共 API 与测试辅助。
    #[allow(dead_code)]
    pub fn ensure_session(&self, chat_id: &str) -> String {
        self.refresh();
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

    /// 一次锁内原子取 session_id（空则建 UUID）+ started 状态。供 bridge 拿串行锁后
    /// 单次快照，避免 ensure_session 与 is_started 两次 refresh 之间被外部改盘读到
    /// 中间态（审查 P3-1a：service 取到旧 session_id、却读到新 started 的错位）。
    pub fn ensure_with_started(&self, chat_id: &str) -> (String, bool) {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(chat_id.to_string()).or_default();
        let slot = Self::slot_mut(entry, &self.current_backend);
        if slot.session_id.is_empty() {
            let sid = uuid::Uuid::new_v4().to_string();
            slot.session_id = sid.clone();
            slot.started = false;
            self.save_locked(&data);
            (sid, false)
        } else {
            (slot.session_id.clone(), slot.started)
        }
    }

    /// 仅当该 chat 当前槽位的 session_id == expected 时置 started=true（#23 审查修复）：
    /// 任务运行中若被 /new 或 CLI `session reset` 换走（当前槽位已不是本次任务的会话），
    /// 旧任务完成不得把新槽位 mark 成 started——否则下一条会误 resume 一个从未运行的新 UUID。
    /// 返回是否真的标记了（false = 槽位已被换走/不存在）。
    pub fn mark_started_if(&self, chat_id: &str, expected_session_id: &str) -> bool {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let Some(entry) = data.get_mut(chat_id) else {
            return false;
        };
        let slot = Self::slot_mut(entry, &self.current_backend);
        if slot.session_id != expected_session_id {
            return false;
        }
        slot.started = true;
        self.save_locked(&data);
        true
    }

    /// 会话重建：换新 UUID 且复位 started=false，返回新 session_id。
    /// claude 用（#6/#7）：jsonl 残留 already in use / 启动挂起后，旧 UUID 槽位永久不可用，
    /// 必须换新（started=false → 下轮走 --session-id 新 UUID）；resume 槽位也一并复位。
    pub fn reset_session(&self, chat_id: &str) -> String {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(chat_id.to_string()).or_default();
        let slot = Self::slot_mut(entry, &self.current_backend);
        let sid = uuid::Uuid::new_v4().to_string();
        slot.session_id = sid.clone();
        slot.started = false;
        self.save_locked(&data);
        sid
    }

    /// 覆盖该 chat 当前后端的 session_id（codex 用：首轮 exec 抓到真实 thread_id 后回存，供后续 resume）。
    pub fn set_session_id(&self, chat_id: &str, session_id: &str) {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        if let Some(entry) = data.get_mut(chat_id) {
            let slot = Self::slot_mut(entry, &self.current_backend);
            if slot.session_id != session_id {
                slot.session_id = session_id.to_string();
                self.save_locked(&data);
            }
        }
    }

    /// 该 chat 当前后端是否已开过首轮（只读查询）。bridge 走合并快照，此方法保留作
    /// 细粒度查询 API 与测试辅助（审查 P3-1a）。
    #[allow(dead_code)]
    pub fn is_started(&self, chat_id: &str) -> bool {
        self.refresh();
        let data = self.data.lock().unwrap();
        data.get(chat_id)
            .map(|e| {
                if self.current_backend.eq_ignore_ascii_case("codex") {
                    e.codex.started
                } else if self.current_backend.eq_ignore_ascii_case("pi") {
                    e.pi.started
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
        assert!(e.pi.session_id.is_empty(), "pi 槽位应为空");

        // pi 后端的旧扁平记录 → 落到 pi 槽位
        let legacy_pi = r#"{"oc_p": {"backend": "pi", "session_id": "p-uuid", "started": false}}"#;
        let m2 = SessionStore::parse(legacy_pi).expect("pi 旧格式应可迁移");
        assert_eq!(m2["oc_p"].pi.session_id, "p-uuid");
        assert!(m2["oc_p"].claude.session_id.is_empty());
    }

    #[test]
    fn parses_per_backend_format() {
        let new = r#"{"oc_xxx": {"claude": {"session_id": "c-uuid", "started": true}, "codex": {"session_id": "x-tid", "started": false}, "pi": {"session_id": "p-uuid", "started": true}}}"#;
        let m = SessionStore::parse(new).expect("新格式应解析");
        let e = &m["oc_xxx"];
        assert_eq!(e.claude.session_id, "c-uuid");
        assert_eq!(e.codex.session_id, "x-tid");
        assert_eq!(e.pi.session_id, "p-uuid");
        assert!(e.pi.started);
    }

    #[test]
    fn backends_are_independent() {
        // 同一 chat 的 claude/codex/pi 槽位互不干扰（切后端不串）
        let new = r#"{"c": {"claude": {"session_id": "claude-uuid", "started": true}}}"#;
        let m = SessionStore::parse(new).unwrap();
        let e = &m["c"];
        assert_eq!(e.claude.session_id, "claude-uuid");
        assert!(e.codex.session_id.is_empty());
        assert!(e.pi.session_id.is_empty());
    }

    #[test]
    fn pi_slot_isolated_from_claude() {
        // pi 槽位独立：claude 槽位有值不影响 pi 槽位的读写（切后端不串）
        let dir = std::env::temp_dir().join(format!("abb-sessions-pi-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        // 先写一个 claude 槽位有值的文件
        std::fs::write(
            &path,
            r#"{"oc_x": {"claude": {"session_id": "c-uuid", "started": true}}}"#,
        )
        .unwrap();
        let store = SessionStore::new_at("pi", path.clone());
        let (sid, started) = store.ensure_with_started("oc_x");
        assert!(!started, "pi 槽位应是全新会话");
        assert!(!sid.is_empty());
        assert!(store.mark_started_if("oc_x", &sid));
        // claude 槽位不受影响
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("c-uuid"));
        assert!(text.contains(&sid));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reset_session_swaps_uuid_and_clears_started() {
        let dir = std::env::temp_dir().join(format!("abb-sessions-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let store = SessionStore::new_at("claude", path.clone());
        let old = store.ensure_session("oc_x");
        assert!(store.mark_started_if("oc_x", &old)); // 模拟已开过首轮（resume 槽位）
        assert!(store.is_started("oc_x"));
        // 槽位被换走（模拟运行中 reset）→ 不得 mark 新槽位
        let fresh = store.reset_session("oc_x");
        assert_ne!(fresh, old, "reset 应换新 UUID");
        assert!(
            !store.mark_started_if("oc_x", &old),
            "旧任务不得 mark 新槽位"
        );
        assert!(!store.is_started("oc_x"));

        let new = store.reset_session("oc_x");
        assert_ne!(old, new, "换新 UUID 不应复用旧 id");
        assert!(!store.is_started("oc_x"), "reset 后 started 必须复位 false");

        // 落盘可重载且写入的是新 UUID
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&new));
        assert!(text.contains("\"started\": false"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hot_reload_picks_up_external_change() {
        // #23：运行中外部（CLI）改 sessions.json → 下一次操作热重载，无需重启
        let dir =
            std::env::temp_dir().join(format!("abb-sessions-reload-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let store = SessionStore::new_at("claude", path.clone());
        let sid_a = store.ensure_session("oc_a");
        assert!(store.mark_started_if("oc_a", &sid_a));
        assert!(store.is_started("oc_a"));

        // 模拟 CLI 在另一个进程直接覆盖文件（换一个 chat 的会话）
        std::thread::sleep(std::time::Duration::from_millis(20));
        let text = r#"{"oc_b": {"claude": {"session_id": "ext-uuid", "started": true}}}"#;
        std::fs::write(&path, text).unwrap();

        // 下次操作即热重载：oc_a 消失、oc_b 可见
        assert!(!store.is_started("oc_a"), "外部覆盖后应读到新文件");
        assert!(store.is_started("oc_b"));

        // 热重载后的写也要落盘（reset 换新 UUID）
        let sid = store.reset_session("oc_b");
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(disk.contains(&sid));
        assert!(disk.contains("\"started\": false"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
