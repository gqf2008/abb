//! 会话持久化 —— ~/.agent-bridge/workspaces/<bot>/sessions.json。
//! 会话按后端区分（claude 的 --resume UUID、codex 的 thread_id、pi 的 --session-id
//! UUID——三者互不通用，共用一个槽位切后端必串）：
//! {chat_id: {claude: {session_id, started}, codex: {...}, pi: {...}}}
//! 当前操作哪个后端的槽位由 SessionStore::new(current_backend, bot_key) 选定——即 per-bot 配置的后端。
//! 旧扁平格式 {chat_id: {backend, session_id, started}} load 时自动迁移到对应后端的槽位。

use crate::config::SandboxMode;
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
    /// #171 会话创建时的权限档位（resume 继承创建时档位，codex 沙箱在会话创建时固定）。
    /// 档位变化对旧会话不生效 → 桥提示用户 /new 换新会话；None = 升级迁移（旧会话无记录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<SandboxMode>,
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

// #194：手写 Clone——句柄式拷贝（path/后端复制，内存缓存清空）。
// 用途：bridge 的 vb 会话存储按 chat 缓存并按值返回；文件是唯一事实源，
// 新实例首次使用时 refresh 从盘加载，语义不变。
impl Clone for SessionStore {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            data: Mutex::new(HashMap::new()),
            current_backend: self.current_backend.clone(),
            loaded_sig: Mutex::new(None),
        }
    }
}

impl SessionStore {
    pub fn new(current_backend: &str, bot_key: &str) -> SessionStore {
        let dir = crate::bridge_dir().join("workspaces").join(bot_key);
        let _ = fs::create_dir_all(&dir);
        Self::at(current_backend, dir.join("sessions.json"))
    }

    /// 按指定路径构造（生产/测试共用；会话归纳清理测试注入 temp workspace）。
    pub(crate) fn at(current_backend: &str, path: PathBuf) -> SessionStore {
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

    /// 当前生效后端（bridge 的 vb 会话存储按 bot 生效后端建实例，#194）。
    pub(crate) fn backend(&self) -> &str {
        &self.current_backend
    }

    /// #194：chat 的会话存储——虚拟 Bot 群 → 独立工作区 vb/<uuid>/sessions.json
    ///（含存量迁移），其余 → bot 级。CLI 管理面（reset/show/delete）与桥共用路由。
    pub fn store_for_chat(current_backend: &str, bot_key: &str, chat_id: &str) -> SessionStore {
        let dir = crate::virtualbot::ensure_vb_dir(bot_key, chat_id)
            .unwrap_or_else(|| crate::workspace_dir(bot_key));
        SessionStore::at(current_backend, dir.join("sessions.json"))
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
                ..Default::default()
            };
            let mut entry = ChatEntry::default();
            if e.backend.eq_ignore_ascii_case("codex") {
                entry.codex = slot;
            } else if e.backend.eq_ignore_ascii_case("pi") {
                entry.pi = slot;
            } else if e.backend.eq_ignore_ascii_case("prime-agent") {
                continue; // #92 收敛：prime-agent 后端下线，旧会话 id 不再可续（不迁移）
            } else {
                entry.claude = slot; // 默认/旧值一律归 claude
            }
            out.insert(chat_id, entry);
        }
        Some(out)
    }

    fn save_locked(&self, data: &HashMap<String, ChatEntry>) -> bool {
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
                return true;
            }
        }
        let _ = fs::remove_file(&tmp);
        false
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
        // #171：重建即新会话——清档位记录，下一轮按当前配置重新记录（防旧档位残留导致
        // 新会话误报「档位已变化」）。
        slot.sandbox_mode = None;
        self.save_locked(&data);
        sid
    }

    /// #171 权限档位变化感知；#185 修正语义：#180 起 resume 按**当前解析档位**运行
    /// （全权限档位还会追加 bypass），旧文案「仍按创建时档位」失实、不覆盖记录会
    /// 每条消息刷屏——现改为提示一次并把记录覆盖为本轮档位。
    /// #198：codex 档位变更**自动重建会话**（轮换 sid、started 复位——等价 reset），
    /// 本轮以全新 exec 按新档位运行（resume 继承首轮沙箱，不重建新档位不生效）；
    /// claude 不轮换（每轮旗标即生效）、pi 无沙箱体系。
    ///
    /// 语义：
    /// - 新会话（started=false）：记录当前档位，返回 None（本轮即以当前档位运行）；
    /// - 既有会话无记录（升级迁移）：补记当前档位，返回 None（无从判断是否变化，不误报）；
    /// - 既有会话记录 ≠ 当前：返回 (提示一次, rotated)，记录覆盖为本轮档位
    ///   （rotated=true=codex：调用方置 rebuilt 让桥写 pending 标记，下一条消息
    ///   注入历史一次——上下文接续）。
    pub fn check_sandbox_mode(&self, chat_id: &str, mode: &SandboxMode) -> Option<(String, bool)> {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(chat_id.to_string()).or_default();
        let slot = Self::slot_mut(entry, &self.current_backend);
        if slot.session_id.is_empty() {
            return None; // 无会话（ensure 未建）：不落记录
        }
        if !slot.started {
            if slot.sandbox_mode.as_ref() != Some(mode) {
                slot.sandbox_mode = Some(*mode);
                self.save_locked(&data);
            }
            return None;
        }
        match slot.sandbox_mode {
            None => {
                // 升级迁移：旧会话无档位记录 → 补记当前值，不提示（无从判断是否变化）。
                slot.sandbox_mode = Some(*mode);
                self.save_locked(&data);
                None
            }
            Some(recorded) if recorded != *mode => {
                // #185：提示一次 + 记录即覆盖为本轮档位（提示至多一次、文案与实际一致）。
                // #198：codex 档位变更 → 自动重建会话（等价 reset：轮换 sid、started
                // 复位）——resume 继承首轮沙箱（#196 实测 0.150.1），不重建则新档位
                // 不生效（切到 workspace-write 写不了）。重建后本轮以全新 exec 按新
                // 档位运行；调用方据 rotated 置 rebuilt → 桥写 pending 标记，下一条
                // 消息注入历史一次（上下文接续）。仅 codex：claude 每轮旗标即生效
                //（轮换反而丢上下文）、pi 无沙箱体系。
                let should_rotate = self.current_backend.eq_ignore_ascii_case("codex");
                if should_rotate {
                    slot.session_id = uuid::Uuid::new_v4().to_string();
                    slot.started = false;
                }
                slot.sandbox_mode = Some(*mode);
                self.save_locked(&data);
                let hint = if should_rotate {
                    format!(
                        "⚠️ 权限档位已变化：已自动重建会话（本轮起按「{}」运行，此前「{}」；旧会话上下文将在下一条消息注入接续）。",
                        mode.as_str(),
                        recorded.as_str()
                    )
                } else {
                    format!(
                        "⚠️ 权限档位已变化：本轮起按当前配置「{}」运行（此前记录为「{}」）。",
                        mode.as_str(),
                        recorded.as_str()
                    )
                };
                Some((hint, should_rotate))
            }
            Some(_) => None,
        }
    }

    /// #194：把本 chat 的全部后端槽位搬到目标 store（虚拟 Bot 独立工作区迁移）。
    /// 源删除、目标写入（目标已有该 chat 则不覆盖，防迁移覆盖新数据）。幂等：
    /// 源无条目即 no-op。返回是否搬了东西。
    pub fn extract_chat_to(&self, chat_id: &str, dst: &SessionStore) -> bool {
        // #194 审查 F3：覆盖精确键 + 话题键（`{chat}:thread…`）——话题槽位不迁会
        // 丢上下文连续性。前缀带 ':' 防 oc_1 误吞 oc_12。
        self.refresh();
        let thread_prefix = format!("{chat_id}:");
        let mut data = self.data.lock().unwrap();
        let keys: Vec<String> = data
            .keys()
            .filter(|k| *k == chat_id || k.starts_with(&thread_prefix))
            .cloned()
            .collect();
        if keys.is_empty() {
            return false;
        }
        let mut moved = Vec::new();
        for k in &keys {
            let entry = data.remove(k).unwrap();
            // 三槽全空（无会话、未开首轮）＝没有值得迁移的状态：直接丢弃
            let empty = entry.claude.session_id.is_empty()
                && !entry.claude.started
                && entry.codex.session_id.is_empty()
                && !entry.codex.started
                && entry.pi.session_id.is_empty()
                && !entry.pi.started;
            if !empty {
                moved.push((k.clone(), entry));
            }
        }
        self.save_locked(&data);
        if moved.is_empty() {
            return false;
        }
        dst.refresh();
        let mut ddata = dst.data.lock().unwrap();
        for (k, entry) in moved {
            // 目标已有该键（新数据）→ 不覆盖（保留目标，丢弃源）
            ddata.entry(k).or_insert(entry);
        }
        dst.save_locked(&ddata);
        true
    }

    /// set_session_id 的 CAS 版本：仅当该 chat 当前槽位的 session_id == expected 时回存，
    /// 返回是否真的回存。任务运行中槽位被 /new 或 CLI `session reset` 换走时（槽位已不是
    /// 本次任务启动时的会话），不得把旧任务的会话 id 写进新槽位——否则桥的
    /// mark_started_if 会匹配旧会话，把新会话标成旧会话的 started，下一条 resume
    /// 旧会话、/new 失效（#49 审查：codex 首轮运行中 /new 的交错场景）。
    /// （原无条件覆盖版 set_session_id 已被本方法取代：调用方是 codex 首轮回存——
    /// 用对端自生成的真实会话 id，必须先验证槽位身份再写。）
    pub fn set_session_id_if(&self, chat_id: &str, expected: &str, session_id: &str) -> bool {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let Some(entry) = data.get_mut(chat_id) else {
            return false;
        };
        let slot = Self::slot_mut(entry, &self.current_backend);
        if slot.session_id != expected {
            return false;
        }
        if slot.session_id != session_id {
            slot.session_id = session_id.to_string();
            self.save_locked(&data);
        }
        true
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

    /// 读某 chat 的完整槽位（所有后端）。不存在返回 None。
    /// 会话归纳清理（session_gc）用它取各后端 sid，精确匹配删除对应会话文件。
    pub fn chat_entry(&self, chat_id: &str) -> Option<ChatEntry> {
        self.refresh();
        self.data.lock().unwrap().get(chat_id).cloned()
    }

    /// 枚举全部 chat key（会话归纳候选判定用；与 parse/refresh 同源——sessions.json
    /// 的解析/旧格式迁移只此一处，schema 演进不绕行裸读）。
    pub fn chat_keys(&self) -> Vec<String> {
        self.refresh();
        self.data.lock().unwrap().keys().cloned().collect()
    }

    /// 删除某 chat 的整个槽位（会话归纳清理用：历史与后端会话文件已清，槽位无意义）。
    /// 返回是否真的删除了且**落盘成功**（save 失败返回 false，调用方据此保留会话状态，
    /// 避免陈旧槽位指向已删文件）；下一个 `ensure_with_started` 会自动重建新 UUID。
    pub fn remove_chat(&self, chat_id: &str) -> bool {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        if !data.contains_key(chat_id) {
            return false;
        }
        // 先落盘再改内存：save 失败时内存与磁盘一致（槽位仍在），「保留会话状态」才
        // 是真的（原实现先 remove 再 save——失败后内存已丢、磁盘还在，进程重启后
        // 陈旧槽位复活指向已删文件；审查修复）。
        let mut next = data.clone();
        next.remove(chat_id);
        if self.save_locked(&next) {
            *data = next;
            true
        } else {
            false
        }
    }

    /// 枚举指定后端**全部 chat** 存活槽位的 session_id（#67 /new 清理用）。
    /// 会话目录是 per-bot、槽位是 per-chat——清理会话文件必须知道哪些 id
    /// 仍被别的聊天占用（误删会让别的聊天静默丢上下文）。
    pub fn live_session_ids(&self, backend: &str) -> Vec<String> {
        self.refresh();
        let data = self.data.lock().unwrap();
        let mut ids = Vec::new();
        for e in data.values() {
            let sid = if backend.eq_ignore_ascii_case("codex") {
                &e.codex.session_id
            } else if backend.eq_ignore_ascii_case("pi") {
                &e.pi.session_id
            } else {
                &e.claude.session_id
            };
            if !sid.is_empty() {
                ids.push(sid.clone());
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #194：extract_chat_to——整 chat 槽位搬到目标 store（虚拟 Bot 独立工作区迁移），
    /// 源移除、目标不覆盖已有、幂等。
    #[test]
    fn extract_chat_to_moves_entry_without_overwrite() {
        let dir = std::env::temp_dir().join(format!("abb-sessions-xfer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = SessionStore::new_at("codex", dir.join("bot-sessions.json"));
        let dst = SessionStore::new_at("codex", dir.join("vb-sessions.json"));
        let (sid, _) = src.ensure_with_started("oc_vb");
        assert!(src.mark_started_if("oc_vb", &sid));

        // 目标已有同 chat（新数据）：迁移不得覆盖
        let (dst_sid, _) = dst.ensure_with_started("oc_vb");
        assert!(src.extract_chat_to("oc_vb", &dst));
        let moved = dst.chat_entry("oc_vb").unwrap();
        assert_eq!(
            moved.codex.session_id, dst_sid,
            "目标已有槽位时不得被迁移覆盖"
        );
        assert!(
            src.chat_entry("oc_vb").is_none(),
            "源槽位必须移除（不双写）"
        );

        // 目标为空：整槽位搬入
        let src2 = SessionStore::new_at("codex", dir.join("bot2.json"));
        let (sid2, _) = src2.ensure_with_started("oc_x");
        assert!(src2.mark_started_if("oc_x", &sid2));
        let dst2 = SessionStore::new_at("codex", dir.join("vb2.json"));
        assert!(src2.extract_chat_to("oc_x", &dst2));
        assert_eq!(dst2.chat_entry("oc_x").unwrap().codex.session_id, sid2);
        assert!(src2.chat_entry("oc_x").is_none());
        // 幂等：再搬一次 no-op
        assert!(!src2.extract_chat_to("oc_x", &dst2));
        let _ = std::fs::remove_dir_all(&dir);
    }

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

        // prime-agent 后端已下线（#92 收敛）→ 旧扁平记录不迁移（丢弃）
        let legacy_pa =
            r#"{"oc_q": {"backend": "prime-agent", "session_id": "q-uuid", "started": true}}"#;
        let m3 = SessionStore::parse(legacy_pa).expect("prime-agent 旧格式应可解析");
        assert!(
            !m3.contains_key("oc_q"),
            "prime-agent 下线：旧记录丢弃，不落到任何槽位"
        );
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
    fn live_session_ids_enumerates_all_chats() {
        // #67：/new 清理会话文件按「存活槽位」判定——枚举必须跨全部 chat key
        // 且只取指定后端（误删其它聊天/其它后端的会话都会静默丢上下文）
        let dir = std::env::temp_dir().join(format!("abb-sessions-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let store = SessionStore::new_at("pi", path.clone());
        let a = store.ensure_with_started("oc_a").0;
        let b = store.ensure_with_started("oc_b").0;
        let mut ids = store.live_session_ids("pi");
        ids.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(ids, want, "跨全部 chat 枚举 pi 槽位");
        // 其它后端的槽位不混入
        let text = r#"{"oc_c": {"claude": {"session_id": "c-uuid", "started": true}}}"#;
        std::fs::write(&path, text).unwrap();
        assert!(
            store.live_session_ids("pi").is_empty(),
            "claude 槽位不算 pi 存活会话"
        );
        assert_eq!(store.live_session_ids("claude"), vec!["c-uuid"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_chat_removes_entry_and_persists() {
        // 会话归纳清理（session_gc）：删除整 chat 槽位，落盘可重载；不存在返回 false
        let dir = std::env::temp_dir().join(format!("abb-sessions-rm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let store = SessionStore::new_at("claude", path.clone());
        let sid = store.ensure_session("oc_a");
        store.ensure_session("oc_b");
        // chat_entry 读回全部后端槽位
        let entry = store.chat_entry("oc_a").expect("存在应返回");
        assert_eq!(entry.claude.session_id, sid);
        assert!(store.chat_entry("oc_none").is_none());
        // remove_chat：删一个，另一个不受影响
        assert!(store.remove_chat("oc_a"));
        assert!(store.chat_entry("oc_a").is_none());
        assert!(store.chat_entry("oc_b").is_some());
        assert!(!store.remove_chat("oc_a"), "已删的 chat 再删返回 false");
        // 落盘持久化（新实例重读）
        let store2 = SessionStore::new_at("claude", path.clone());
        assert!(store2.chat_entry("oc_a").is_none());
        assert!(store2.chat_entry("oc_b").is_some());
        // 删除后重建：ensure_with_started 自动生成新 UUID
        let (sid2, started) = store2.ensure_with_started("oc_a");
        assert_ne!(sid2, sid, "删除后重建应换新 UUID");
        assert!(!started);
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

    #[test]
    fn set_session_id_if_cas_guards_slot_identity() {
        // #49 审查：codex 首轮回存必须 CAS——运行中槽位被 /new / CLI reset 换走时，
        // 不得把旧任务 thread 写进新槽位（否则 mark_started_if 匹配旧 thread，
        // 新会话 resume 旧线程、/new 失效）。
        let dir = std::env::temp_dir().join(format!("abb-sessions-cas-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let store = SessionStore::new_at("codex", path.clone());

        // 首轮回存：槽位仍是任务启动时的占位 UUID → CAS 成功
        let placeholder = store.ensure_session("oc_x");
        assert!(store.set_session_id_if("oc_x", &placeholder, "tid-real-1"));
        let (cur, _) = store.ensure_with_started("oc_x");
        assert_eq!(cur, "tid-real-1", "CAS 成功后槽位是真实 thread");

        // 模拟运行中 /new：槽位被换走 → 旧任务（持占位快照）的回存必须被拒
        let fresh = store.reset_session("oc_x");
        assert_ne!(fresh, placeholder);
        assert!(
            !store.set_session_id_if("oc_x", &placeholder, "tid-stale"),
            "槽位已换走，旧任务的回存必须被拒"
        );
        let (cur2, _) = store.ensure_with_started("oc_x");
        assert_eq!(cur2, fresh, "新槽位 UUID 不被旧任务污染");

        // 新会话（fresh）自己的回存正常
        assert!(store.set_session_id_if("oc_x", &fresh, "tid-real-2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandbox_mode_change_detection() {
        // #171 建立感知、#185 修正语义：#180 起 resume 按当前解析档位运行——
        // 档位变化提示一次（文案=本轮起按新档位），记录随即覆盖；同档位不提示；
        // 记录持久化（提示一次后重载也不再提示）。
        let dir = std::env::temp_dir().join(format!("abb-sessions-sb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let store = SessionStore::new_at("codex", path.clone());

        // 新会话（未开过首轮）：记录档位、无提示
        let sid = store.ensure_session("oc_x");
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::ReadOnly),
            None,
            "新会话不提示"
        );
        // 同档位 resume：不提示
        assert!(store.mark_started_if("oc_x", &sid));
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::ReadOnly),
            None,
            "同档位不提示"
        );
        // 档位变化 → 提示一次，文案与实际一致（本轮起按新档位）
        let (hint, rotated) = store
            .check_sandbox_mode("oc_x", &SandboxMode::FullAccess)
            .expect("档位变化应提示");
        assert!(rotated, "codex 档位变更必须轮换会话（#198）");
        assert!(hint.contains("本轮起"), "提示应说明本轮起按新档位：{hint}");
        assert!(
            !hint.contains("仍按创建时"),
            "不得再声称按旧档位运行：{hint}"
        );
        assert!(hint.contains("read-only"), "提示应说明旧档位：{hint}");
        assert!(hint.contains("full-access"), "提示应说明新档位：{hint}");
        // #198：codex 档位变更自动重建会话（轮换 sid、started 复位）——resume 继承
        // 首轮沙箱，不重建则新档位不生效。重建后本轮以全新 exec 按新档位运行。
        let (sid_after, started_after) = store.ensure_with_started("oc_x");
        assert_ne!(sid_after, sid, "档位变更必须轮换会话 sid（#198 自动重建）");
        assert!(!started_after, "重建后 started 复位（本轮全新 exec）");
        // 记录已覆盖为本轮档位：提示至多一次（#185，旧语义「变化持续提示」与
        // #180 的实际运行档位相反且每条消息刷屏）
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::FullAccess),
            None,
            "记录已覆盖：同档位后续不提示"
        );
        // 落盘持久化：重载后记录已是新档位，不再提示
        let store2 = SessionStore::new_at("codex", path.clone());
        assert_eq!(
            store2.check_sandbox_mode("oc_x", &SandboxMode::FullAccess),
            None,
            "重载后记录已覆盖，不再提示"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandbox_mode_migration_records_silently() {
        // #171 升级迁移：旧会话槽位无 sandbox_mode 字段 → 补记当前档位不提示；
        // 之后档位再变化才提示。
        let dir =
            std::env::temp_dir().join(format!("abb-sessions-sb-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        // 旧格式槽位（无 sandbox_mode 字段，started=true 可 resume）
        std::fs::write(
            &path,
            r#"{"oc_x": {"codex": {"session_id": "tid-1", "started": true}}}"#,
        )
        .unwrap();
        let store = SessionStore::new_at("codex", path.clone());
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::WorkspaceWrite),
            None,
            "迁移补记不提示（无从判断是否变化）"
        );
        assert!(
            store
                .check_sandbox_mode("oc_x", &SandboxMode::ReadOnly)
                .is_some(),
            "迁移后档位再变化才提示"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reset_session_clears_sandbox_record() {
        // #171：重建（claude 自愈换 UUID）即新会话——清档位记录，下一轮按当前配置
        // 重新记录，不残留旧档位误报「档位已变化」。
        let dir =
            std::env::temp_dir().join(format!("abb-sessions-sb-reset-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let store = SessionStore::new_at("codex", path.clone());
        let sid = store.ensure_session("oc_x");
        // 新会话首轮前记录初始档位
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::ReadOnly),
            None
        );
        assert!(store.mark_started_if("oc_x", &sid));
        // 已记录旧档位并感知变化
        assert!(store
            .check_sandbox_mode("oc_x", &SandboxMode::FullAccess)
            .is_some());
        // 重建：换新 UUID + 清记录 + started 复位
        let fresh = store.reset_session("oc_x");
        assert_ne!(fresh, sid);
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::FullAccess),
            None,
            "重建后按新档位重新记录，不残留旧档位误报"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #198：claude 会话档位变更**不轮换 sid**——claude 的 permission/guard 每轮
    /// 重建，改档位立即生效，轮换反而丢上下文（与 codex 的沙箱继承机制相反）。
    #[test]
    fn sandbox_change_does_not_rotate_claude_session() {
        let dir =
            std::env::temp_dir().join(format!("abb-sessions-sb-claude-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let store = SessionStore::new_at("claude", path.clone());
        let sid = store.ensure_session("oc_x");
        assert!(store.mark_started_if("oc_x", &sid));
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::ReadOnly),
            None
        );
        assert!(
            store
                .check_sandbox_mode("oc_x", &SandboxMode::FullAccess)
                .is_some(),
            "档位变化应提示"
        );
        let (sid_after, started_after) = store.ensure_with_started("oc_x");
        assert_eq!(sid_after, sid, "claude 会话不得轮换 sid");
        assert!(started_after, "claude 会话 started 保持");
        std::fs::remove_dir_all(&dir).ok();
    }
}
