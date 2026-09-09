//! 会话持久化 —— ~/.agent-bridge/workspaces/<bot>/sessions.json。
//! 单后端（buzz-agent）单槽 schema（单后端化 P4.2）：
//! {chat_id: {session_id, started, sandbox_mode?}}
//!
//! 历史 schema（load 时一次性折叠迁移，写盘只写新格式）：
//! - 四槽：{chat_id: {claude: {...}, codex: {...}, pi: {...}, buzz: {...}}}
//!   （按后端分槽时代：三后端会话 id 互不通用，共槽切后端必串）
//! - 旧扁平：{chat_id: {backend, session_id, started}}（更早的聊天切后端时代）
//!
//! 折叠规则（buzz 已是唯一执行层，其余后端的会话 id 对 buzz 无续聊价值）：
//! - buzz 槽 / backend=buzz 的旧扁平记录：逐字保留（session_id/started/sandbox_mode）；
//! - claude/codex/pi 槽与其余旧扁平记录：舍弃——不同 agent 的会话 id 互不通用，
//!   留着只会被误当可 resume 的会话；
//! - 数据零丢失：折叠后首次写盘前，原件逐字归档到 sessions.json.legacy.bak
//!   （只归档一次，已有备份绝不覆盖）；归档失败则不写盘（原件不动，下轮重试）。
//!
//! 迁移只跑一次：新格式文件不含老键，load 不再触发折叠。

use crate::config::SandboxMode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

/// 老四槽格式的单后端槽位（session_id + 是否已开过首轮）。仅用于读老文件
/// （SessionStore load 即折叠清空；session_import 迁移车绕行裸读老文件时仍取得到——
/// P4.1 收口前保持其编译与功能不变）。新 schema 不再使用本类型。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Slot {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub started: bool,
    /// #171 会话创建时的权限档位（老 buzz 槽可能携带，折叠时逐字提升）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<SandboxMode>,
}

impl Slot {
    fn is_empty(&self) -> bool {
        self.session_id.is_empty() && !self.started
    }
}

/// 一个 chat 的会话槽位（单后端单槽）：session_id + 是否已开过首轮（决定下轮
/// resume 还是新建）。平铺三键是唯一生效数据、写盘只写它们；legacy_* / claude /
/// codex / pi / buzz 字段仅为读老文件存在（serde 只进不出：skip_serializing），
/// load 折叠后内存中一律清空。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatEntry {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub started: bool,
    /// #171 会话创建时的权限档位（resume 继承创建时档位，沙箱在会话创建时固定）。
    /// 档位变化对旧会话不生效 → 桥提示用户 /new 换新会话；None = 升级迁移（旧会话无记录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<SandboxMode>,
    /// 旧扁平格式 {backend, session_id, started} 的后端标记（聊天切后端时代）——
    /// 仅 load 折叠判定用（serde rename 读盘键名，内存名带 legacy 前缀防误用）。
    #[serde(rename = "backend", default, skip_serializing)]
    pub legacy_backend: Option<String>,
    #[serde(default, skip_serializing)]
    pub claude: Slot,
    #[serde(default, skip_serializing)]
    pub codex: Slot,
    #[serde(default, skip_serializing)]
    pub pi: Slot,
    /// 老四槽的 buzz 槽（曾错误借用 claude 槽——切 buzz 后端时继承 claude 的
    /// started/sid，注入闸误判 resume 跳过历史注入 → 上下文丢失）。折叠时逐字
    /// 提升为单槽。
    #[serde(default, skip_serializing)]
    pub buzz: Slot,
}

pub struct SessionStore {
    path: PathBuf,
    data: Mutex<HashMap<String, ChatEntry>>,
    /// 上次加载时的文件签名 (mtime, size)。CLI/外部改 sessions.json 后按签名热重载
    /// （#23），无需重启 service 即生效——复用 JobStore 的 refresh 模式；size 与 mtime
    /// 双重判定以缓解同 tick 内 mtime 精度不足的漏检（审查 P3-2）。
    loaded_sig: Mutex<Option<(SystemTime, u64)>>,
    /// 老结构折叠待归档：load 折叠后置位，首次写盘前先把原件逐字归档到
    /// sessions.json.legacy.bak 再覆盖（用户数据零丢失）；归档失败则不写盘，
    /// 置位保留到下次写盘重试。
    pending_archive: Mutex<bool>,
}

// #194：手写 Clone——句柄式拷贝（path 复制，内存缓存清空）。
// 用途：bridge 的 vb 会话存储按 chat 缓存并按值返回；文件是唯一事实源，
// 新实例首次使用时 refresh 从盘加载，语义不变。
impl Clone for SessionStore {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            data: Mutex::new(HashMap::new()),
            loaded_sig: Mutex::new(None),
            pending_archive: Mutex::new(false),
        }
    }
}

/// 文件签名 (mtime, size)：refresh/at 同源。
fn file_sig(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    fs::metadata(path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}

impl SessionStore {
    pub fn new(bot_key: &str) -> SessionStore {
        let dir = crate::bridge_dir().join("workspaces").join(bot_key);
        let _ = fs::create_dir_all(&dir);
        Self::at(dir.join("sessions.json"))
    }

    /// 按指定路径构造（生产/测试共用；会话归纳清理测试注入 temp workspace）。
    pub(crate) fn at(path: PathBuf) -> SessionStore {
        let store = SessionStore {
            path,
            data: Mutex::new(HashMap::new()),
            loaded_sig: Mutex::new(None),
            pending_archive: Mutex::new(false),
        };
        store.reload();
        store
    }

    /// #194：chat 的会话存储——虚拟 Bot 群 → 独立工作区 vb/<uuid>/sessions.json
    ///（含存量迁移），其余 → bot 级。CLI 管理面（reset/show/delete）与桥共用路由。
    pub fn store_for_chat(bot_key: &str, chat_id: &str) -> SessionStore {
        let dir = crate::virtualbot::ensure_vb_dir(bot_key, chat_id)
            .unwrap_or_else(|| crate::workspace_dir(bot_key));
        SessionStore::at(dir.join("sessions.json"))
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
        let cur = file_sig(&self.path);
        let stale = { *self.loaded_sig.lock().unwrap() != cur };
        if !stale {
            return;
        }
        self.reload();
    }

    /// 从盘加载（at 首载与 refresh 热重载共用同一路径）：老格式在内存折叠后立即
    /// 归档原件 + 落盘新格式——迁移只跑一次（落盘后的新格式不再触发折叠）。
    /// 只有解析成功才推进 sig：临时读失败/坏文件不吞掉重试机会。
    fn reload(&self) {
        let cur = file_sig(&self.path);
        let Ok(text) = fs::read_to_string(&self.path) else {
            return;
        };
        let Some((data, migrated)) = Self::parse(&text) else {
            return;
        };
        *self.data.lock().unwrap() = data;
        *self.loaded_sig.lock().unwrap() = cur;
        if migrated {
            *self.pending_archive.lock().unwrap() = true;
            let data = self.data.lock().unwrap();
            // 立即落盘新格式（内含原件归档）；失败则 pending_archive 保持置位，
            // 下次写盘重试，盘上原件不动。
            self.save_locked(&data);
        }
    }

    /// 解析 sessions.json：统一读三种 schema（新单槽 / 老四槽 / 旧扁平），返回
    /// （折叠后数据, 是否折叠了老结构）。老键一律只进不出——折叠只认平铺三键。
    fn parse(text: &str) -> Option<(HashMap<String, ChatEntry>, bool)> {
        let raw: HashMap<String, ChatEntry> = serde_json::from_str(text).ok()?;
        let mut migrated = false;
        let mut out = HashMap::with_capacity(raw.len());
        for (chat_id, e) in raw {
            let has_legacy = e.legacy_backend.is_some()
                || !e.claude.is_empty()
                || !e.codex.is_empty()
                || !e.pi.is_empty()
                || !e.buzz.is_empty();
            if !has_legacy {
                out.insert(chat_id, e);
                continue;
            }
            migrated = true;
            let folded = Self::fold_legacy_entry(e);
            // 折叠后无会话状态（仅非 buzz 槽/记录）→ 整条移除（空槽不携带任何状态，
            // 下一条消息 ensure 自动重建全新会话）。
            if !folded.session_id.is_empty() || folded.started {
                out.insert(chat_id, folded);
            }
        }
        Some((out, migrated))
    }

    /// 老条目折叠为单槽：buzz 数据逐字保留，其余后端槽位/记录舍弃（不同 agent 的
    /// 会话 id 互不通用，对 buzz 无续聊价值；原件见 sessions.json.legacy.bak）。
    fn fold_legacy_entry(e: ChatEntry) -> ChatEntry {
        // 旧扁平格式 {backend, session_id, started}：仅 backend=buzz 的记录保留
        if let Some(backend) = &e.legacy_backend {
            if backend.eq_ignore_ascii_case("buzz") || backend.eq_ignore_ascii_case("buzz-agent") {
                return ChatEntry {
                    session_id: e.session_id,
                    started: e.started,
                    sandbox_mode: e.sandbox_mode,
                    ..Default::default()
                };
            }
            return ChatEntry::default();
        }
        // 新格式平铺键与老槽并存（异常形态，如手工合并的文件）：平铺键优先，老槽舍弃
        if !e.session_id.is_empty() || e.started {
            return ChatEntry {
                session_id: e.session_id,
                started: e.started,
                sandbox_mode: e.sandbox_mode,
                ..Default::default()
            };
        }
        // 老四槽格式：buzz 槽逐字提升为单槽，claude/codex/pi 槽舍弃
        ChatEntry {
            session_id: e.buzz.session_id,
            started: e.buzz.started,
            sandbox_mode: e.buzz.sandbox_mode,
            ..Default::default()
        }
    }

    /// 折叠后首次写盘前归档原件：<file>.legacy.bak（sessions.json.legacy.bak）。
    /// 「不存在才建」（tmp + hard_link 原子语意）：并发进程/崩溃重入都不覆盖已有
    /// 备份——保留最老原件。盘上无原件（全新工作区）按已归档放行。归档失败返回
    /// false——save_locked 据此不写盘（绝不无备份覆盖用户数据）。
    fn archive_original(&self) -> bool {
        let bak = self.path.with_extension("json.legacy.bak");
        if bak.exists() {
            return true;
        }
        let Ok(bytes) = fs::read(&self.path) else {
            return true;
        };
        let tmp = self
            .path
            .with_extension(format!("json.legacy.tmp.{}", uuid::Uuid::new_v4()));
        let ok = fs::write(&tmp, bytes).is_ok()
            && match fs::hard_link(&tmp, &bak) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => true,
                Err(_) => false,
            };
        let _ = fs::remove_file(&tmp);
        ok
    }

    fn save_locked(&self, data: &HashMap<String, ChatEntry>) -> bool {
        // 老结构折叠后的首次写盘：先归档原件再覆盖（数据零丢失）；归档失败不写盘，
        // pending 保持置位，下次写盘重试。
        {
            let mut pending = self.pending_archive.lock().unwrap();
            if *pending {
                if !self.archive_original() {
                    crate::log!(
                        "[sessions] ⚠️ 原件归档失败，本次不写盘（保留老格式原件，下轮重试）: {}",
                        self.path.display()
                    );
                    return false;
                }
                *pending = false;
            }
        }
        // 原子写：唯一 tmp + rename（崩溃不留半截；唯一 tmp 避免 CLI reset 与 service
        // 写盘并发时互相覆盖同一 tmp 文件）
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
        if let Ok(text) = serde_json::to_string_pretty(data) {
            if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &self.path).is_ok() {
                // 写完即推进 sig，避免下次公开方法再读盘（磁盘==内存）
                *self.loaded_sig.lock().unwrap() = file_sig(&self.path);
                return true;
            }
        }
        let _ = fs::remove_file(&tmp);
        false
    }

    /// 返回该 chat 的 session_id，没有则新建 UUID。
    ///
    /// 生产 bridge 已改用 `ensure_with_started` 合并快照（审查 P3-1a）；此方法保留作
    /// 细粒度公共 API 与测试辅助。
    #[allow(dead_code)]
    pub fn ensure_session(&self, chat_id: &str) -> String {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(chat_id.to_string()).or_default();
        if !entry.session_id.is_empty() {
            return entry.session_id.clone();
        }
        let sid = uuid::Uuid::new_v4().to_string();
        entry.session_id = sid.clone();
        entry.started = false;
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
        if entry.session_id.is_empty() {
            let sid = uuid::Uuid::new_v4().to_string();
            entry.session_id = sid.clone();
            entry.started = false;
            self.save_locked(&data);
            (sid, false)
        } else {
            (entry.session_id.clone(), entry.started)
        }
    }

    /// 仅当该 chat 槽位的 session_id == expected 时置 started=true（#23 审查修复）：
    /// 任务运行中若被 /new 或 CLI `session reset` 换走（槽位已不是本次任务的会话），
    /// 旧任务完成不得把新槽位 mark 成 started——否则下一条会误 resume 一个从未运行的新 UUID。
    /// 返回是否真的标记了（false = 槽位已被换走/不存在）。
    pub fn mark_started_if(&self, chat_id: &str, expected_session_id: &str) -> bool {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let Some(entry) = data.get_mut(chat_id) else {
            return false;
        };
        if entry.session_id != expected_session_id {
            return false;
        }
        entry.started = true;
        self.save_locked(&data);
        true
    }

    /// 服务启动复位：ACP 单轨（harness）时代 agent 会话不跨进程存活——每次
    /// ABB 重启 = harness 会话归零，盘上持久化的 started/sid 是上个进程的谎言。
    /// 全部槽位清空后，每个 chat 在新进程的首轮走 !resume → 注入闸（marker 失配）
    /// → 历史注入，上下文跨重启衔接（#49 语义）。
    ///
    /// 由 service 在 Bridge 构建后调用（bot 级存储）；vb 会话存储在 sessions_for
    /// 首建实例时同样复位（进程内首次使用 = 服务启动后的首次使用）。
    pub fn reset_slots_for_service_start(&self) {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let mut changed = false;
        for entry in data.values_mut() {
            if !entry.session_id.is_empty() || entry.started {
                entry.session_id.clear();
                entry.started = false;
                changed = true;
            }
        }
        if changed {
            self.save_locked(&data);
        }
    }

    /// 会话重建：换新 UUID 且复位 started=false，返回新 session_id。
    /// 旧 UUID 槽位永久不可用（jsonl 残留/启动挂起等自愈场景）时必须换新
    ///（started=false → 下轮走新 UUID 首轮）；resume 槽位也一并复位。
    pub fn reset_session(&self, chat_id: &str) -> String {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(chat_id.to_string()).or_default();
        let sid = uuid::Uuid::new_v4().to_string();
        entry.session_id = sid.clone();
        entry.started = false;
        // #171：重建即新会话——清档位记录，下一轮按当前配置重新记录（防旧档位残留导致
        // 新会话误报「档位已变化」）。
        entry.sandbox_mode = None;
        self.save_locked(&data);
        sid
    }

    /// #171 权限档位变化感知；#185 修正语义：#180 起 resume 按**当前解析档位**运行
    /// （全权限档位还会追加 bypass），旧文案「仍按创建时档位」失实、不覆盖记录会
    /// 每条消息刷屏——现改为提示一次并把记录覆盖为本轮档位。
    /// `rotate_on_change`：档位变更是否轮换会话（新 sid + started 复位，等价 reset）
    /// ——由调用方按后端语义决定（#198：codex resume 继承首轮沙箱，不轮换新档位
    /// 不生效；claude 每轮旗标即生效，轮换反而丢上下文；buzz 经 session/new `_meta`
    /// 下发档位，是否需轮换由 P4.1 接线时定）。rotated=true 时调用方置 rebuilt
    /// 让桥写 pending 标记，下一条消息注入历史一次（上下文接续）。
    ///
    /// 语义：
    /// - 新会话（started=false）：记录当前档位，返回 None（本轮即以当前档位运行）；
    /// - 既有会话无记录（升级迁移）：补记当前档位，返回 None（无从判断是否变化，不误报）；
    /// - 既有会话记录 ≠ 当前：返回 (提示一次, rotated)，记录覆盖为本轮档位。
    pub fn check_sandbox_mode(
        &self,
        chat_id: &str,
        mode: &SandboxMode,
        rotate_on_change: bool,
    ) -> Option<(String, bool)> {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(chat_id.to_string()).or_default();
        if entry.session_id.is_empty() {
            return None; // 无会话（ensure 未建）：不落记录
        }
        if !entry.started {
            if entry.sandbox_mode.as_ref() != Some(mode) {
                entry.sandbox_mode = Some(*mode);
                self.save_locked(&data);
            }
            return None;
        }
        match entry.sandbox_mode {
            None => {
                // 升级迁移：旧会话无档位记录 → 补记当前值，不提示（无从判断是否变化）。
                entry.sandbox_mode = Some(*mode);
                self.save_locked(&data);
                None
            }
            Some(recorded) if recorded != *mode => {
                // #185：提示一次 + 记录即覆盖为本轮档位（提示至多一次、文案与实际一致）。
                // #198：rotate_on_change → 轮换 sid、started 复位（等价 reset）——
                // resume 继承首轮沙箱的后端（codex）不轮换则新档位不生效；轮换后本轮
                // 以全新会话按新档位运行，调用方据 rotated 置 rebuilt → 桥写 pending
                // 标记，下一条消息注入历史一次（上下文接续）。
                if rotate_on_change {
                    entry.session_id = uuid::Uuid::new_v4().to_string();
                    entry.started = false;
                }
                entry.sandbox_mode = Some(*mode);
                self.save_locked(&data);
                let hint = if rotate_on_change {
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
                Some((hint, rotate_on_change))
            }
            Some(_) => None,
        }
    }

    /// #194：把本 chat 的槽位搬到目标 store（虚拟 Bot 独立工作区迁移）。
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
            // 空槽（无会话、未开首轮）＝没有值得迁移的状态：直接丢弃
            let empty = entry.session_id.is_empty() && !entry.started;
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

    /// set_session_id 的 CAS 版本：仅当该 chat 槽位的 session_id == expected 时回存，
    /// 返回是否真的回存。任务运行中槽位被 /new 或 CLI `session reset` 换走时（槽位已不是
    /// 本次任务启动时的会话），不得把旧任务的会话 id 写进新槽位——否则桥的
    /// mark_started_if 会匹配旧会话，把新会话标成旧会话的 started，下一条 resume
    /// 旧会话、/new 失效（#49 审查：首轮回存与 /new 的交错场景）。
    ///（原无条件覆盖版 set_session_id 已被本方法取代：调用方是首轮回存——
    /// 用对端自生成的真实会话 id，必须先验证槽位身份再写。）
    pub fn set_session_id_if(&self, chat_id: &str, expected: &str, session_id: &str) -> bool {
        self.refresh();
        let mut data = self.data.lock().unwrap();
        let Some(entry) = data.get_mut(chat_id) else {
            return false;
        };
        if entry.session_id != expected {
            return false;
        }
        if entry.session_id != session_id {
            entry.session_id = session_id.to_string();
            self.save_locked(&data);
        }
        true
    }

    /// 该 chat 是否已开过首轮（只读查询）。bridge 走合并快照，此方法保留作
    /// 细粒度查询 API 与测试辅助（审查 P3-1a）。
    #[allow(dead_code)]
    pub fn is_started(&self, chat_id: &str) -> bool {
        self.refresh();
        let data = self.data.lock().unwrap();
        data.get(chat_id).map(|e| e.started).unwrap_or(false)
    }

    /// 读某 chat 的完整槽位。不存在返回 None。
    pub fn chat_entry(&self, chat_id: &str) -> Option<ChatEntry> {
        self.refresh();
        self.data.lock().unwrap().get(chat_id).cloned()
    }

    /// 枚举全部 chat key（会话归纳候选判定用；与 parse/refresh 同源——sessions.json
    /// 的解析/折叠迁移只此一处，schema 演进不绕行裸读）。
    pub fn chat_keys(&self) -> Vec<String> {
        self.refresh();
        self.data.lock().unwrap().keys().cloned().collect()
    }

    /// 删除某 chat 的槽位（会话归纳清理用：历史已清，槽位无意义）。
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

    /// 枚举**全部 chat** 存活槽位的 session_id（tidy 孤儿清理的 live 集，#67）。
    /// 会话文件目录是 per-bot、槽位是 per-chat——清理会话文件必须知道哪些 id
    /// 仍被别的聊天占用（误删会让别的聊天静默丢上下文）。
    pub fn live_session_ids(&self) -> Vec<String> {
        self.refresh();
        let data = self.data.lock().unwrap();
        data.values()
            .filter(|e| !e.session_id.is_empty())
            .map(|e| e.session_id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abb-sessions-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── P4.2 升级回归：老四槽 → 单槽折叠（用户数据零丢失是硬指标）──

    #[test]
    fn folds_four_slot_format_keeping_buzz_verbatim() {
        // 老四槽文件 → load：buzz 槽逐字保留（sid/started/sandbox_mode）；
        // claude/codex/pi 槽舍弃；仅非 buzz 槽的 chat 整条移除
        let dir = temp_dir("fold4");
        let path = dir.join("sessions.json");
        let legacy = r#"{
  "oc_buzz": {"claude": {"session_id": "c-uuid", "started": true}, "codex": {"session_id": "x-tid", "started": true}, "pi": {"session_id": "p-uuid", "started": true}, "buzz": {"session_id": "b-uuid", "started": true, "sandbox_mode": "workspace-write"}},
  "oc_only_legacy": {"claude": {"session_id": "c2-uuid", "started": true}, "pi": {"session_id": "p2-uuid", "started": false}},
  "oc_codex_only": {"codex": {"session_id": "x2-tid", "started": true}}
}"#;
        std::fs::write(&path, legacy).unwrap();

        let store = SessionStore::at(path.clone());
        // buzz 槽逐字提升（含 sandbox_mode）
        let e = store.chat_entry("oc_buzz").expect("buzz 槽保留");
        assert_eq!(e.session_id, "b-uuid");
        assert!(e.started);
        assert_eq!(e.sandbox_mode, Some(SandboxMode::WorkspaceWrite));
        // 仅非 buzz 槽的 chat：会话 id 对 buzz 无意义 → 整条移除
        assert!(store.chat_entry("oc_only_legacy").is_none());
        assert!(store.chat_entry("oc_codex_only").is_none());
        // 折叠即落盘新格式：文件只剩平铺键，无任何后端槽位键
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("b-uuid"));
        for k in [
            "\"claude\"",
            "\"codex\"",
            "\"pi\"",
            "\"buzz\"",
            "\"backend\"",
        ] {
            assert!(!text.contains(k), "新格式不得含老键 {k}: {text}");
        }
        // 原件逐字归档（用户数据零丢失）
        let bak = std::fs::read_to_string(path.with_extension("json.legacy.bak")).unwrap();
        assert_eq!(bak, legacy, "备份必须逐字等于原件");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migration_runs_only_once() {
        // 折叠迁移只跑一次：第二实例 load 新格式不再触发迁移——
        // 备份不被改写（保留最老原件），文件内容稳定
        let dir = temp_dir("once");
        let path = dir.join("sessions.json");
        let legacy = r#"{"oc_x": {"buzz": {"session_id": "b-uuid", "started": true}}}"#;
        std::fs::write(&path, legacy).unwrap();

        let store1 = SessionStore::at(path.clone());
        assert_eq!(store1.chat_entry("oc_x").unwrap().session_id, "b-uuid");
        let new_text = std::fs::read_to_string(&path).unwrap();
        let bak_path = path.with_extension("json.legacy.bak");
        assert_eq!(std::fs::read_to_string(&bak_path).unwrap(), legacy);

        // 第二实例：新格式 load 不触发迁移（无二次改写、备份不动）
        let store2 = SessionStore::at(path.clone());
        assert_eq!(store2.chat_entry("oc_x").unwrap().session_id, "b-uuid");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            new_text,
            "新格式 load 不得改写文件"
        );
        assert_eq!(
            std::fs::read_to_string(&bak_path).unwrap(),
            legacy,
            "备份不得被二次迁移覆盖"
        );

        // 常规写盘（新格式）也不再触碰备份
        store2.reset_session("oc_x");
        assert_eq!(std::fs::read_to_string(&bak_path).unwrap(), legacy);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_format_does_not_trigger_migration() {
        // 新格式 load：不产生备份文件、不改写原文件
        let dir = temp_dir("newfmt");
        let path = dir.join("sessions.json");
        let text =
            r#"{"oc_x": {"session_id": "b-uuid", "started": true, "sandbox_mode": "read-only"}}"#;
        std::fs::write(&path, text).unwrap();
        let store = SessionStore::at(path.clone());
        let e = store.chat_entry("oc_x").unwrap();
        assert_eq!(e.session_id, "b-uuid");
        assert!(e.started);
        assert_eq!(e.sandbox_mode, Some(SandboxMode::ReadOnly));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "原件不动");
        assert!(
            !path.with_extension("json.legacy.bak").exists(),
            "新格式不产生迁移备份"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn folds_legacy_flat_format_by_backend() {
        // 旧扁平格式 {backend, session_id, started}：buzz 记录保留，其余后端舍弃
        let dir = temp_dir("flat");
        let path = dir.join("sessions.json");
        std::fs::write(
            &path,
            r#"{"oc_b": {"backend": "buzz", "session_id": "b-uuid", "started": true}, "oc_c": {"backend": "claude", "session_id": "c-uuid", "started": true}, "oc_x": {"backend": "codex", "session_id": "x-tid", "started": true}, "oc_p": {"backend": "pi", "session_id": "p-uuid", "started": false}, "oc_q": {"backend": "prime-agent", "session_id": "q-uuid", "started": true}}"#,
        )
        .unwrap();
        let store = SessionStore::at(path.clone());
        let e = store.chat_entry("oc_b").expect("buzz 旧扁平记录保留");
        assert_eq!(e.session_id, "b-uuid");
        assert!(e.started);
        for k in ["oc_c", "oc_x", "oc_p", "oc_q"] {
            assert!(store.chat_entry(k).is_none(), "非 buzz 旧扁平记录舍弃: {k}");
        }
        // 原件已归档
        assert!(path.with_extension("json.legacy.bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fold_prefers_flat_keys_when_mixed() {
        // 异常形态：平铺新键与老槽并存（手工合并等）→ 平铺键优先，老槽舍弃
        let dir = temp_dir("mixed");
        let path = dir.join("sessions.json");
        std::fs::write(
            &path,
            r#"{"oc_x": {"session_id": "flat-sid", "started": true, "buzz": {"session_id": "slot-sid", "started": false}}}"#,
        )
        .unwrap();
        let store = SessionStore::at(path.clone());
        let e = store.chat_entry("oc_x").unwrap();
        assert_eq!(e.session_id, "flat-sid", "平铺键优先");
        assert!(e.started);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("slot-sid"), "老槽已清除: {text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn archive_never_overwritten() {
        // 已有备份时归档不得覆盖（保留最老原件）——即使文件再次变成老格式
        let dir = temp_dir("keepbak");
        let path = dir.join("sessions.json");
        std::fs::write(
            &path,
            r#"{"oc_a": {"buzz": {"session_id": "b1", "started": true}}}"#,
        )
        .unwrap();
        let bak_path = path.with_extension("json.legacy.bak");
        std::fs::write(&bak_path, "最老原件").unwrap();
        let store = SessionStore::at(path.clone());
        assert_eq!(store.chat_entry("oc_a").unwrap().session_id, "b1");
        assert_eq!(
            std::fs::read_to_string(&bak_path).unwrap(),
            "最老原件",
            "已有备份不得被覆盖"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hot_reload_folds_legacy_file_swapped_in() {
        // 运行中外部把老格式文件换进来 → 下次操作热重载即折叠 + 归档
        let dir = temp_dir("hotfold");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());
        store.ensure_session("oc_a");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let legacy = r#"{"oc_b": {"buzz": {"session_id": "ext-buzz", "started": true}}}"#;
        std::fs::write(&path, legacy).unwrap();
        // 热重载折叠：oc_a 消失（外部覆盖）、oc_b 以 buzz 槽保留
        assert!(!store.is_started("oc_a"));
        let e = store.chat_entry("oc_b").unwrap();
        assert_eq!(e.session_id, "ext-buzz");
        assert!(e.started);
        // 折叠已落盘 + 原件已归档
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.legacy.bak")).unwrap(),
            legacy
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"buzz\""), "落盘为新格式: {text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 既有行为回归（单槽语义）──

    #[test]
    fn extract_chat_to_moves_entry_without_overwrite() {
        // #194：extract_chat_to——整 chat 槽位搬到目标 store（虚拟 Bot 独立工作区迁移），
        // 源移除、目标不覆盖已有、幂等。
        let dir = temp_dir("xfer");
        let src = SessionStore::at(dir.join("bot-sessions.json"));
        let dst = SessionStore::at(dir.join("vb-sessions.json"));
        let (sid, _) = src.ensure_with_started("oc_vb");
        assert!(src.mark_started_if("oc_vb", &sid));

        // 目标已有同 chat（新数据）：迁移不得覆盖
        let (dst_sid, _) = dst.ensure_with_started("oc_vb");
        assert!(src.extract_chat_to("oc_vb", &dst));
        let moved = dst.chat_entry("oc_vb").unwrap();
        assert_eq!(moved.session_id, dst_sid, "目标已有槽位时不得被迁移覆盖");
        assert!(
            src.chat_entry("oc_vb").is_none(),
            "源槽位必须移除（不双写）"
        );

        // 目标为空：整槽位搬入
        let src2 = SessionStore::at(dir.join("bot2.json"));
        let (sid2, _) = src2.ensure_with_started("oc_x");
        assert!(src2.mark_started_if("oc_x", &sid2));
        let dst2 = SessionStore::at(dir.join("vb2.json"));
        assert!(src2.extract_chat_to("oc_x", &dst2));
        assert_eq!(dst2.chat_entry("oc_x").unwrap().session_id, sid2);
        assert!(src2.chat_entry("oc_x").is_none());
        // 幂等：再搬一次 no-op
        assert!(!src2.extract_chat_to("oc_x", &dst2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn service_start_reset_clears_all_slots() {
        let dir = temp_dir("reset");
        let path = dir.join("sessions.json");
        let sid = {
            let store = SessionStore::at(path.clone());
            let (sid, _) = store.ensure_with_started("oc_x");
            assert!(store.mark_started_if("oc_x", &sid));
            sid
        };
        // 模拟服务重启：新实例复位 → 槽位清空
        let store2 = SessionStore::at(path.clone());
        store2.reset_slots_for_service_start();
        let (sid2, started2) = store2.ensure_with_started("oc_x");
        assert!(!started2, "复位后首轮必须 !resume（注入闸）");
        assert_ne!(sid2, sid, "复位后 sid 必须换新（marker 失配触发注入）");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_session_ids_enumerates_all_chats() {
        // #67：清理会话文件按「存活槽位」判定——枚举必须跨全部 chat key
        let dir = temp_dir("live");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());
        let a = store.ensure_with_started("oc_a").0;
        let b = store.ensure_with_started("oc_b").0;
        let mut ids = store.live_session_ids();
        ids.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(ids, want, "跨全部 chat 枚举存活槽位");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_chat_removes_entry_and_persists() {
        // 会话归纳清理（session_gc）：删除整 chat 槽位，落盘可重载；不存在返回 false
        let dir = temp_dir("rm");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());
        let sid = store.ensure_session("oc_a");
        store.ensure_session("oc_b");
        // chat_entry 读回槽位
        let entry = store.chat_entry("oc_a").expect("存在应返回");
        assert_eq!(entry.session_id, sid);
        assert!(store.chat_entry("oc_none").is_none());
        // remove_chat：删一个，另一个不受影响
        assert!(store.remove_chat("oc_a"));
        assert!(store.chat_entry("oc_a").is_none());
        assert!(store.chat_entry("oc_b").is_some());
        assert!(!store.remove_chat("oc_a"), "已删的 chat 再删返回 false");
        // 落盘持久化（新实例重读）
        let store2 = SessionStore::at(path.clone());
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
        let dir = temp_dir("swap");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());
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
        let dir = temp_dir("reload");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());
        let sid_a = store.ensure_session("oc_a");
        assert!(store.mark_started_if("oc_a", &sid_a));
        assert!(store.is_started("oc_a"));

        // 模拟 CLI 在另一个进程直接覆盖文件（换一个 chat 的会话）
        std::thread::sleep(std::time::Duration::from_millis(20));
        let text = r#"{"oc_b": {"session_id": "ext-uuid", "started": true}}"#;
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
        // #49 审查：首轮回存必须 CAS——运行中槽位被 /new / CLI reset 换走时，
        // 不得把旧任务的会话 id 写进新槽位（否则 mark_started_if 匹配旧会话，
        // 新会话 resume 旧会话、/new 失效）。
        let dir = temp_dir("cas");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());

        // 首轮回存：槽位仍是任务启动时的占位 UUID → CAS 成功
        let placeholder = store.ensure_session("oc_x");
        assert!(store.set_session_id_if("oc_x", &placeholder, "tid-real-1"));
        let (cur, _) = store.ensure_with_started("oc_x");
        assert_eq!(cur, "tid-real-1", "CAS 成功后槽位是真实会话 id");

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
        // #171 建立感知、#185 修正语义：resume 按当前解析档位运行——档位变化提示
        // 一次（文案=本轮起按新档位），记录随即覆盖；同档位不提示；记录持久化。
        // rotate_on_change=true（#198 codex 语义）：变更即轮换 sid、started 复位。
        let dir = temp_dir("sb");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());

        // 新会话（未开过首轮）：记录档位、无提示
        let sid = store.ensure_session("oc_x");
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::ReadOnly, true),
            None,
            "新会话不提示"
        );
        // 同档位 resume：不提示
        assert!(store.mark_started_if("oc_x", &sid));
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::ReadOnly, true),
            None,
            "同档位不提示"
        );
        // 档位变化 → 提示一次，文案与实际一致（本轮起按新档位）
        let (hint, rotated) = store
            .check_sandbox_mode("oc_x", &SandboxMode::FullAccess, true)
            .expect("档位变化应提示");
        assert!(rotated, "rotate_on_change=true 必须轮换会话（#198）");
        assert!(hint.contains("本轮起"), "提示应说明本轮起按新档位：{hint}");
        assert!(
            !hint.contains("仍按创建时"),
            "不得再声称按旧档位运行：{hint}"
        );
        assert!(hint.contains("read-only"), "提示应说明旧档位：{hint}");
        assert!(hint.contains("full-access"), "提示应说明新档位：{hint}");
        // 轮换：sid 换新、started 复位（本轮全新会话按新档位运行）
        let (sid_after, started_after) = store.ensure_with_started("oc_x");
        assert_ne!(sid_after, sid, "档位变更必须轮换会话 sid");
        assert!(!started_after, "重建后 started 复位");
        // 记录已覆盖为本轮档位：提示至多一次（#185）
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::FullAccess, true),
            None,
            "记录已覆盖：同档位后续不提示"
        );
        // 落盘持久化：重载后记录已是新档位，不再提示
        let store2 = SessionStore::at(path.clone());
        assert_eq!(
            store2.check_sandbox_mode("oc_x", &SandboxMode::FullAccess, true),
            None,
            "重载后记录已覆盖，不再提示"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandbox_mode_migration_records_silently() {
        // #171 升级迁移：旧会话槽位无 sandbox_mode 字段 → 补记当前档位不提示；
        // 之后档位再变化才提示。
        let dir = temp_dir("sb-mig");
        let path = dir.join("sessions.json");
        // 无 sandbox_mode 字段的槽位（started=true 可 resume）
        std::fs::write(
            &path,
            r#"{"oc_x": {"session_id": "tid-1", "started": true}}"#,
        )
        .unwrap();
        let store = SessionStore::at(path.clone());
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::WorkspaceWrite, true),
            None,
            "迁移补记不提示（无从判断是否变化）"
        );
        assert!(
            store
                .check_sandbox_mode("oc_x", &SandboxMode::ReadOnly, true)
                .is_some(),
            "迁移后档位再变化才提示"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reset_session_clears_sandbox_record() {
        // #171：重建即新会话——清档位记录，下一轮按当前配置重新记录，
        // 不残留旧档位误报「档位已变化」。
        let dir = temp_dir("sb-reset");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());
        let sid = store.ensure_session("oc_x");
        // 新会话首轮前记录初始档位
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::ReadOnly, true),
            None
        );
        assert!(store.mark_started_if("oc_x", &sid));
        // 已记录旧档位并感知变化
        assert!(store
            .check_sandbox_mode("oc_x", &SandboxMode::FullAccess, true)
            .is_some());
        // 重建：换新 UUID + 清记录 + started 复位
        let fresh = store.reset_session("oc_x");
        assert_ne!(fresh, sid);
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::FullAccess, true),
            None,
            "重建后按新档位重新记录，不残留旧档位误报"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #198 反面对称：rotate_on_change=false（claude 语义——permission/guard 每轮
    /// 重建，改档位立即生效）不轮换 sid，轮换反而丢上下文。
    #[test]
    fn sandbox_change_without_rotation_keeps_session() {
        let dir = temp_dir("sb-norotate");
        let path = dir.join("sessions.json");
        let store = SessionStore::at(path.clone());
        let sid = store.ensure_session("oc_x");
        assert!(store.mark_started_if("oc_x", &sid));
        assert_eq!(
            store.check_sandbox_mode("oc_x", &SandboxMode::ReadOnly, false),
            None
        );
        let (hint, rotated) = store
            .check_sandbox_mode("oc_x", &SandboxMode::FullAccess, false)
            .expect("档位变化应提示");
        assert!(!rotated, "rotate_on_change=false 不轮换");
        assert!(!hint.contains("已自动重建会话"), "文案与行为一致：{hint}");
        let (sid_after, started_after) = store.ensure_with_started("oc_x");
        assert_eq!(sid_after, sid, "不轮换 sid");
        assert!(started_after, "started 保持");
        std::fs::remove_dir_all(&dir).ok();
    }
}
