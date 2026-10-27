//! 系统级对话（全局锚点气泡 → AI 对话框）：直接与随包 `abb-agent` 对话的后端。
//!
//! ## 定位
//!
//! 这是**系统级 Owner Bot**：owner 身份、整机全权限，不经任何 IM 渠道。入口是托盘进程里的
//! 全局锚点气泡（`AnchorBubble`）与它打开的 `SystemChatWindow`——**GUI 专属**：ABB 无头运行
//! （`--service`）时不实例化任何窗口，那时与 ABB 对话只能走 IM bot（产品里本就如此）。
//!
//! ## 为什么单独成模块（不塞进 `ui.rs`）
//!
//! 回合装配/历史上界/跨回合串行化是**可独立单测**的逻辑（mock agent 可驱动全链路），
//! 与 Slint 窗口接线解耦；将来若要新增前端（或改由 service 托管）也只动这一个模块。
//!
//! ## 多轮语义
//!
//! [`crate::runtime::oneshot::oneshot_turn`] 每次都是**新 ACP 会话**（用完即拆栈），
//! 进程间也没有可借的常驻句柄。于是「记忆」由本模块自己维护：把最近若干轮历史拼进
//! 每轮的 prompt，历史落盘 `~/.agent-bridge/syschat/sessions/<id>.jsonl`（会话索引
//! `sessions.json`；旧版单文件 `history.jsonl` 首次自动迁入默认会话），跨重启保留。
//!
//! 这**不是**服务端会话态，只是上下文重放——因此有明确上界（[`HISTORY_KEEP_TURNS`]），
//! 免得每轮 token 成本线性增长。历史读写都在 `history.lock` 独占锁内完成（fs2：
//! Unix flock / Windows LockFileEx），防止多线程/多窗口下的丢更新。

use crate::config::Config;
use crate::runtime::acp::{ChunkKind, TurnChunk};
use crate::runtime::harness::{AgentConfig, SteerSupport, SyncTurnOutcome};
use crate::runtime::oneshot::SteerWiring;
use crate::runtime::queue::InboundMsg;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

/// 系统级会话的工作区隔离键（`workspaces/<key>/`）。与任何 bot 隔离键都不同名。
pub const SYSCHAT_KEY: &str = "syschat";

/// 历史保留的最近轮数（一问一答 = 1 轮）。超过后丢弃最旧的。
///
/// 这是喂给模型的上下文上界：不设上界时每轮都全量重放，token 成本随会话线性增长。
pub const HISTORY_KEEP_TURNS: usize = 20;

/// 单条历史的最大字符数（防单条超长粘贴把 prompt 撑爆；按字符截断，见 AGENTS 的 UTF-8 约定）。
const MAX_HISTORY_CHARS: usize = 8000;

/// 单轮回合预算（与团队生成同口径：模型可能跑工具，给足 10 分钟）。
const TURN_BUDGET: Duration = Duration::from_secs(600);

/// 系统级会话的 system 前言。ABB 的 `base_prompt.md` 已提供底座人格，这里只补
/// 「系统级、全权限、直接执行」的定位——不重复 base_prompt 已写的内容。
const SYSTEM_PREAMBLE: &str = "\
[系统级对话] 你正通过 ABB 的全局气泡与用户直接对话（非任何 IM 渠道）。\n\
你运行在用户本机上，拥有整机读写与执行权限。请直接、简洁地回应；\n\
需要操作本机时直接用工具执行，不要只描述步骤。";

/// 一轮对话记录。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    /// "user" | "assistant"
    pub role: String,
    pub text: String,
    /// 本轮的**思考过程**（E2 基本形态）：`agent_thought_chunk` 累积文本。
    /// 空串 = 该轮没有思考（或旧记录）。`#[serde(default)]` 让 E2 之前落的 jsonl
    /// 行照常反序列化（缺字段 ⇒ 空串），**不破坏既有历史文件**。
    #[serde(default)]
    pub thought: String,
    /// Unix 秒（展示/排序用）。
    pub ts: i64,
}

/// 「重新生成」/「复制」/「朗读」的目标轮解析结果（卡 `abb-v11-message-actions`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTarget {
    /// 该助手轮在历史里的下标。
    pub index: usize,
    /// 该轮**正文**（`⧉ 复制` 与 `🔊 朗读` 的内容；不含思考）。
    pub text: String,
    /// `⟳ 重新生成` 要重跑的**用户提示词**：该轮之前最近的一条用户轮。
    /// 该轮之前没有用户轮（异常历史）⇒ `None`，调用方给可见提示而不是发空消息。
    pub prompt: Option<String>,
    /// 上面那条用户轮在历史里的下标（`⟳ 重新生成` 要从这里**丢弃到末尾**再重跑）。
    /// 与 `prompt` 同生共死（`prompt.is_none()` ⇔ 这里为 `None`）。
    pub prompt_index: Option<usize>,
}

/// 解析助手轮动作行的目标（纯函数，单测钉住）。
///
/// `turn` **必须是真实的历史下标**，且 `turns[turn]` 必须真的是助手轮——指到用户轮/越界一律
/// `None`（宁可报「找不到这一轮」也不猜）。
///
/// 为什么**不**接受「负值 = 最后一条助手轮」这类哨兵（原设计如此，独立评审判 Critical）：
/// 哨兵不是身份。一条已收尾但还没换真实下标的动作行留在模型里，等第二轮收尾时模型里就有
/// **两条**同值哨兵，两条都解析到最后一条助手轮 ⇒ 点第一条的「复制/朗读」操作的是**别的轮**
/// 且报成功。所以「刚跑完那一轮」的下标必须在收尾时就算出来（见 [`last_assistant_index`]）。
pub fn action_target(turns: &[Turn], turn: i32) -> Option<ActionTarget> {
    let index = usize::try_from(turn).ok()?;
    if turns.get(index)?.role != "assistant" {
        return None;
    }
    let text = turns[index].text.clone();
    let prompt_index = turns[..index].iter().rposition(|t| t.role == "user");
    let prompt = prompt_index.map(|i| turns[i].text.clone());
    Some(ActionTarget {
        index,
        text,
        prompt,
        prompt_index,
    })
}

/// 刚收尾那一轮在**落盘历史**里的下标（`None` = 定位不到 ⇒ 调用方**不挂动作行**）。
///
/// 为什么定位得到：`turn_core` 的「临界区 2」在回合结局事件（`Reply`/`Stopped`）**之前**就把
/// 助手轮落进历史（`src/syschat.rs` 的 `turn_core` 尾部），而 GUI 是单线程、同一时刻只有一个
/// 在途回合（`busy`）——所以 GUI 处理该事件时，历史末条就是这一轮。
///
/// 为什么要「末条必须是助手轮」这个条件而不是直接取 `len() - 1`：回合失败（Error 事件）或
/// 极端停止（正文与思考都空、`turn_core` 不落盘）时末条会是用户轮，此时**不挂**才对——
/// 挂出来的按钮会指向别的轮，属于「给了假保证」。
pub fn last_assistant_index(turns: &[Turn]) -> Option<i32> {
    match turns.last() {
        Some(t) if t.role == "assistant" => i32::try_from(turns.len() - 1).ok(),
        _ => None,
    }
}

/// 系统级对话的数据目录：`~/.agent-bridge/syschat/`。
pub fn dir() -> PathBuf {
    crate::bridge_dir().join("syschat")
}

/// 会话元数据（`sessions.json` 条目）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    pub id: String,
    /// 展示标题：首条用户消息截断（空会话 = 「新会话」）。
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// 本会话用的供应商名（"" = 跟随全局默认）。**「模型选择」就落在这一项上**。
    pub provider: String,
    /// 已归档（卡 `abb-v11-session-archive`）：归档后从会话列表里隐藏，但**不删任何数据**
    /// （`sessions/<id>.jsonl` 原样保留，取消归档即找回）。
    ///
    /// `#[serde(default)]` 就是**旧 `sessions.json` 的向后兼容**：老文件里根本没有这个键，
    /// 缺字段一律按「未归档」读入。序列化方向不做跳过：让这一位在文件里显式可见，
    /// 便于人工核对（旧版本 ABB 读新文件时 serde 默认忽略未知字段，不会因此读坏）。
    #[serde(default)]
    pub archived: bool,
    /// 本会话最近一轮请求**执行层上报的真实**上下文占用（prompt tokens =
    /// input + cacheRead + cacheWrite，见 `runtime::acp::TokenUsage::context_tokens`）。
    ///
    /// `0` = **还没有一轮报过 usage**（新建会话、改动前的老会话、不上报的适配器）
    /// ——读者必须按「未知」显示，别把它当成「占用为 0」；`context_usage()` 就是这么分的。
    /// `#[serde(default)]` 是旧 `sessions.json` 的向后兼容（缺字段按「未知」读入）。
    #[serde(default)]
    pub context_tokens: u64,
}

/// `sessions.json` 落盘结构。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct SessionState {
    #[serde(default)]
    current: String,
    #[serde(default)]
    sessions: Vec<SessionMeta>,
}

/// 最近活动的**未归档**会话（`updated_at` 最大；并列时取先插入的那条——与 [`SessionStore::list`]
/// 的稳定排序同口径）。
///
/// 抽成自由函数是为了让「current 该落到哪一条」这条判据只有一处实现。
fn newest_visible(state: &SessionState) -> Option<&SessionMeta> {
    state
        .sessions
        .iter()
        .filter(|s| !s.archived)
        .max_by_key(|s| s.updated_at)
}

/// 会话存储（可注入 base 目录，便于单测不碰真实 `~/.agent-bridge`）。
///
/// 结构：
/// - `<base>/sessions.json`：`{current, sessions:[{id,title,created_at,updated_at,provider,archived}]}`
/// - `<base>/sessions/<id>.jsonl`：该会话的逐轮历史
/// - `<base>/history.jsonl`：旧版单会话历史（首次自动迁入默认会话）
struct SessionStore {
    base: PathBuf,
}

impl SessionStore {
    fn production() -> Self {
        Self::new_at(dir())
    }
    fn new_at(base: PathBuf) -> Self {
        Self { base }
    }
    fn state_path(&self) -> PathBuf {
        self.base.join("sessions.json")
    }
    fn history_path(&self, id: &str) -> PathBuf {
        // id 来自 sessions.json / 公共 API；白名单外的 id 退化为一个不可用名字，
        // 绝不越出 sessions/（避免损坏/恶意的 sessions.json 把 append/clear 引到目录外）——评审 N3。
        let safe: String = if is_valid_session_id(id) {
            id.to_string()
        } else {
            "_invalid".to_string()
        };
        self.base.join("sessions").join(format!("{safe}.jsonl"))
    }
    fn legacy_path(&self) -> PathBuf {
        self.base.join("history.jsonl")
    }

    fn load(&self) -> SessionState {
        std::fs::read_to_string(self.state_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, state: &SessionState) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.base)?;
        // 用 uuid 唯一 tmp 名的共享原子写（卡 `abb-proc-sysctl-nb-20261028` NB-3）：
        // 固定 `syschat.json.tmp` 会被 `fs::write`（O_CREAT|O_TRUNC）**跟随符号链接**，
        // 受限会话预放一条链接就能让本函数覆盖工作区外的真实文件。
        // 与 `schedule.rs` 同一修法；0o600 由 `atomic_write_sensitive` 保证。
        let text = serde_json::to_string_pretty(state).unwrap_or_default();
        crate::atomic_write_sensitive(&self.state_path(), &text)
    }

    /// 保证「至少一个会话、current 有效」；改动过返回 true。首次还把旧版单会话历史迁入默认会话。
    fn ensure_current(&self, state: &mut SessionState) -> bool {
        if !state.sessions.is_empty() {
            // 不变量：`current` 必须是**未归档**的既有会话。归档会把会话从列表里藏起来，
            // 若 current 还指着它，用户接下来发的消息就会写进一个列表里看不见的会话
            // （卡 `abb-v11-session-archive`）。这一段同时覆盖「current 为空 / 已不存在」的老情况。
            if state
                .sessions
                .iter()
                .any(|s| s.id == state.current && !s.archived)
            {
                return false;
            }
            match newest_visible(state) {
                Some(s) => state.current = s.id.clone(),
                None => {
                    // 全部会话都已归档 ⇒ 起一条新的空会话。**不去改用户已归档的那些**
                    // （归档是用户的意图，静默取消归档等于替用户反悔）。模型选择沿用刚离开
                    // 那条会话的，别因为归档把用户的模型选择丢掉。
                    let provider = state
                        .sessions
                        .iter()
                        .find(|s| s.id == state.current)
                        .map(|s| s.provider.clone())
                        .unwrap_or_default();
                    let id = new_session_id();
                    let now = crate::chrono_lite::unix_secs() as i64;
                    state.sessions.push(SessionMeta {
                        id: id.clone(),
                        title: "新会话".to_string(),
                        created_at: now,
                        updated_at: now,
                        provider,
                        archived: false,
                        context_tokens: 0,
                    });
                    state.current = id;
                }
            }
            return true;
        }
        let id = new_session_id();
        let now = crate::chrono_lite::unix_secs() as i64;
        state.sessions.push(SessionMeta {
            id: id.clone(),
            title: "新会话".to_string(),
            created_at: now,
            updated_at: now,
            provider: String::new(),
            archived: false,
            context_tokens: 0,
        });
        state.current = id.clone();
        if self.legacy_path().exists() {
            let _ = std::fs::create_dir_all(self.base.join("sessions"));
            let dest = self.history_path(&id);
            if std::fs::rename(self.legacy_path(), &dest).is_err() {
                let _ = std::fs::copy(self.legacy_path(), &dest);
            }
        }
        true
    }

    fn read_history(&self, id: &str) -> Vec<Turn> {
        let Ok(file) = std::fs::File::open(self.history_path(id)) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Turn>(&l).ok())
            .collect()
    }

    fn append_turns(&self, id: &str, turns: &[Turn]) -> std::io::Result<()> {
        if turns.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(self.base.join("sessions"))?;
        let path = self.history_path(id);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        for t in turns {
            writeln!(file, "{}", serde_json::to_string(t).unwrap_or_default())?;
        }
        Ok(())
    }

    /// 取**未归档**会话列表（`updated_at` 倒序）与当前会话 id。
    ///
    /// 归档的会话不出现在这里（卡 `abb-v11-session-archive`）；要列归档的用 [`Self::archived`]。
    fn list(&self) -> (Vec<SessionMeta>, String) {
        let mut state = self.load();
        if self.ensure_current(&mut state) {
            let _ = self.save(&state);
        }
        let mut list: Vec<SessionMeta> = state
            .sessions
            .iter()
            .filter(|s| !s.archived)
            .cloned()
            .collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        (list, state.current)
    }

    /// 已归档会话（`updated_at` 倒序，最近用过的在前）。纯**读**：不修不变量、不落盘。
    fn archived(&self) -> Vec<SessionMeta> {
        let mut list: Vec<SessionMeta> = self
            .load()
            .sessions
            .into_iter()
            .filter(|s| s.archived)
            .collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        list
    }

    /// 置位/清位某会话的 `archived`（**调用方须已持历史锁**）。
    ///
    /// - id 不存在 ⇒ 幂等忽略（与 [`switch_session`] 同口径：不报错、不改任何东西）；
    /// - 值本来就是这个 ⇒ 不落盘（幂等，避免无意义的写盘与 mtime 抖动）；
    /// - 归档**当前**会话 ⇒ `current` 换到最近活动的未归档会话；一条不剩就起一条新的
    ///   （不变量与理由见 [`Self::ensure_current`]）。
    ///
    /// 只动索引里的这一位：`sessions/<id>.jsonl` 与任何历史文件都不碰（卡要求「不删数据」）。
    fn set_archived(&self, id: &str, archived: bool) -> Result<(), String> {
        let mut state = self.load();
        self.ensure_current(&mut state);
        let Some(index) = state.sessions.iter().position(|s| s.id == id) else {
            return Ok(());
        };
        if state.sessions[index].archived == archived {
            return Ok(());
        }
        state.sessions[index].archived = archived;
        // 归档掉当前会话时，current 的落位交给同一套判据（不在这里自己挑，避免两份实现漂移）。
        let _ = self.ensure_current(&mut state);
        self.save(&state)
            .map_err(|e| format!("写入会话索引失败：{e}"))
    }

    fn provider_of(&self, id: &str) -> String {
        self.load()
            .sessions
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| s.provider)
            .unwrap_or_default()
    }

    /// 历史独占锁（路径 = `base/history.lock`，故测试用临时 base 不会碰真实数据目录）。
    fn lock(&self) -> Result<HistoryLock, String> {
        HistoryLock::acquire(&self.base.join("history.lock"))
            .map_err(|e| format!("锁定对话历史失败：{e}"))
    }
}

/// 会话 id 白名单：`[A-Za-z0-9_-]`、非空、≤ 64 字节（与 [`new_session_id`] 生成口径一致）。
fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 新建会话 id：`s<时间戳>-<短随机>`（可排序、不撞；只含 `[A-Za-z0-9_-]`，无路径穿越）。
fn new_session_id() -> String {
    format!(
        "s{}-{}",
        crate::chrono_lite::unix_secs(),
        uuid::Uuid::new_v4().simple()
    )
}

/// 首条用户消息 → 会话标题（截断）。
fn derive_title(text: &str) -> String {
    let t = text.trim().replace('\n', " ");
    let cut: String = t.chars().take(24).collect();
    if cut.is_empty() {
        "新会话".to_string()
    } else {
        cut
    }
}

/// 当前会话 id（必要时创建会话并迁移旧历史）。GUI 发消息前调一次即可。
pub fn current_session() -> String {
    let store = SessionStore::production();
    let _g = store.lock().ok();
    store.list().1
}

/// 会话列表（`updated_at` 倒序）+ 当前会话 id。
///
/// 注意：这是**读**入口，锁为尽力而为（拿不到就按当前快照返回）；真正的写入口
/// （[`new_session`] / [`switch_session`] / [`set_session_provider`] / [`turn`]）一律严格锁。
pub fn session_list() -> (Vec<SessionMeta>, String) {
    let store = SessionStore::production();
    let _g = store.lock().ok();
    store.list()
}

/// 已归档会话列表（`updated_at` 倒序）。纯读入口，锁为尽力而为（同 [`session_list`]）。
pub fn archived_session_list() -> Vec<SessionMeta> {
    let store = SessionStore::production();
    let _g = store.lock().ok();
    store.archived()
}

/// [`archive_current`] 的结果：`archived = false` 表示当前是空会话、按口径**未归档**；
/// `title` 是被处理的会话标题（状态行用）。
pub struct ArchiveOutcome {
    pub archived: bool,
    pub title: String,
}

/// 归档**当前**会话（GUI「🗄 归档…」的收口）。先经过空会话闸门。
///
/// 为什么要这个闸门（owner 2026-10-15 实测）：空会话被归档毫无意义，而且归档后
/// [`SessionStore::ensure_current`] 发现「没有未归档会话」会**立刻新建一条空会话** ⇒
/// 用户连点几次就把归档列表塞满一串永远用不到的「新会话」空壳（实测盘上 25 条会话里 20 条
/// 是这么来的）。所以：当前会话为空 ⇒ **不归档、不新建**，由调用方给一句状态行。
pub fn archive_current() -> Result<ArchiveOutcome, String> {
    let store = SessionStore::production();
    let _g = store.lock()?;
    archive_current_locked(&store)
}

/// [`archive_current`] 的实现：**调用方必须已持历史锁**（单测用临时 base 直接驱动）。
fn archive_current_locked(store: &SessionStore) -> Result<ArchiveOutcome, String> {
    let mut state = store.load();
    store.ensure_current(&mut state);
    let id = state.current.clone();
    let title = state
        .sessions
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.title.clone())
        .unwrap_or_default();
    if is_empty_session(&store.read_history(&id)) {
        return Ok(ArchiveOutcome {
            archived: false,
            title,
        });
    }
    if let Some(i) = state.sessions.iter().position(|s| s.id == id) {
        state.sessions[i].archived = true;
    }
    // current 的落位交给同一套判据（与 `set_archived` 一致，不在这里再挑一次）。
    store.ensure_current(&mut state);
    store
        .save(&state)
        .map_err(|e| format!("写入会话索引失败：{e}"))?;
    Ok(ArchiveOutcome {
        archived: true,
        title,
    })
}

/// 取消归档（找回）。同样只动索引里的这一位，历史文件不碰。
pub fn unarchive_session(id: &str) -> Result<(), String> {
    let store = SessionStore::production();
    let _g = store.lock()?;
    store.set_archived(id, false)
}

/// 会话面板该列的 **(列表, 当前项 id)**（卡 `abb-v11-session-archive`）。
///
/// 普通视图 = 未归档列表 + 真实当前项；归档视图 = 已归档列表 + **空当前项**。
///
/// 为什么归档视图必须清掉当前项：当前会话一定在未归档列表里（[`SessionStore::ensure_current`]
/// 的不变量），归档列表里那一项若还带着「当前」tag，用户就会以为「当前会话已经归档了」。
/// 判据放在这里（而不是写在 `ui.rs` 的回调里）就是为了能单测。
pub fn session_menu_view(
    archive_view: bool,
    visible: (Vec<SessionMeta>, String),
    archived: Vec<SessionMeta>,
) -> (Vec<SessionMeta>, String) {
    if archive_view {
        (archived, String::new())
    } else {
        visible
    }
}

/// 会话面板（`ui/app.slint` 的 `MenuPanel`）在两种视图下的**文案**。
///
/// 为什么抽成纯函数而不是写死在 `.slint` 里：文案要能单测（卡 `abb-v11-session-archive`
/// 要求「列表过滤」可负向验证），且「已归档（N）」的 N 必须与列表同源——写死在 `.slint`
/// 就只能靠人眼比对。`.slint` 只负责把这里的字符串画出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPanelText {
    pub heading: String,
    pub foot: String,
    /// 面板里第一行「额外项」：普通视图 = 归档当前会话；归档视图 = 返回会话列表。
    /// **同一行两义**——判据只在 Rust 一处（`ui.rs::on_session_extra`），`.slint` 不判。
    pub extra_text: String,
    pub extra_tag: String,
    /// 第二行：普通视图 = 进「已归档」；归档视图 = 空（不画——那一行的位置已被「返回」占了）。
    pub extra2_text: String,
    pub extra2_tag: String,
}

/// 见 [`SessionPanelText`]。`n_archived` 是归档条数（两种视图都用它；归档视图的 N 就是列表长度）。
pub fn session_panel_text(archive_view: bool, n_archived: usize) -> SessionPanelText {
    if archive_view {
        SessionPanelText {
            heading: format!("已归档（{n_archived} 条，点一条即找回）"),
            foot: "点一条 = 取消归档并切回继续聊；历史文件一直保留，不做任何删除".to_string(),
            extra_text: "← 返回会话列表".to_string(),
            extra_tag: String::new(),
            extra2_text: String::new(),
            extra2_tag: String::new(),
        }
    } else {
        SessionPanelText {
            heading: "会话（历史保留，可随时切回）".to_string(),
            foot: "点一条即切回该会话继续聊；归档后从列表隐藏、可在「已归档」里找回".to_string(),
            extra_text: "🗄 归档…".to_string(),
            extra_tag: "当前会话".to_string(),
            extra2_text: format!("🗄 已归档（{n_archived}）…"),
            extra2_tag: String::new(),
        }
    }
}

/// 会话是否为空（**「新建会话」去重的唯一判据**，卡 `abb-v11-new-session-dedupe`）。
///
/// 口径：历史里**没有任何带内容的轮**。逐条对齐卡里的边界——
/// - **有附件但无消息**：待发附件只存在于 GUI 内存（`attach-count`），**不落历史** ⇒ 历史里
///   没有轮 ⇒ 空（点了 📎 再点新建会话不该因此多出一个空会话）；
/// - **只有思考无正文**：`turn_core` 在「正文空、思考非空」时**照样落盘**助手轮（见那里的
///   注释：停了也不丢推理）⇒ 已有内容 ⇒ 非空；
/// - 用户轮**一律算有内容**：那是用户真说出口的话（哪怕正文空、只带了附件），不能因为
///   「正文空白」就被「新建会话」当成空会话吞掉。
///
/// 纯函数（不碰 IO/锁）⇒ 单测可穷举边界。
pub fn is_empty_session(turns: &[Turn]) -> bool {
    turns
        .iter()
        .all(|t| t.role != "user" && t.text.trim().is_empty() && t.thought.trim().is_empty())
}

/// 「新建会话」的结果。
///
/// 为什么要 `created`：本卡之后点「新建会话」**不一定**真新建（当前会话为空就复用），状态行
/// 得如实说，不能一律报「已新建会话」（说假话比不说更糟）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub id: String,
    /// `false` = 复用了当前空会话（会话列表没有增长）。
    pub created: bool,
}

/// 新建会话并置为当前；`provider` 为空时沿用当前会话的模型选择。
///
/// **去重**（卡 `abb-v11-new-session-dedupe`）：当前会话为空（[`is_empty_session`]）时**复用**它
/// —— 否则连点几下「新建会话」就会在列表里堆一串空会话。
/// 拿不到历史锁时返回可重试错误——不静默降级。
pub fn new_session(provider: Option<&str>) -> Result<NewSession, String> {
    let store = SessionStore::production();
    let _g = store.lock()?;
    new_session_locked(&store, provider)
}

/// [`new_session`] 的实现：**调用方必须已持历史锁**。
///
/// 单测用临时 base 直接驱动它（`SessionStore::new_at`）——`SessionStore::production()` 会写真实
/// `~/.agent-bridge`，单测不能走那条路。
fn new_session_locked(store: &SessionStore, provider: Option<&str>) -> Result<NewSession, String> {
    let mut state = store.load();
    let index_changed = store.ensure_current(&mut state);
    // 复用一个空会话：**仍要落盘** —— `ensure_current` 可能刚建出它、或刚把旧版单会话历史
    // 迁进来（那时 `state` 只在内存里，不落盘就会「返回了一个索引里没有的 id」）。
    if is_empty_session(&store.read_history(&state.current)) {
        if index_changed {
            store
                .save(&state)
                .map_err(|e| format!("写入会话索引失败：{e}"))?;
        }
        return Ok(NewSession {
            id: state.current.clone(),
            created: false,
        });
    }
    let provider = match provider {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => state
            .sessions
            .iter()
            .find(|s| s.id == state.current)
            .map(|s| s.provider.clone())
            .unwrap_or_default(),
    };
    let id = new_session_id();
    let now = crate::chrono_lite::unix_secs() as i64;
    state.sessions.push(SessionMeta {
        id: id.clone(),
        title: "新会话".to_string(),
        created_at: now,
        updated_at: now,
        provider,
        archived: false,
        context_tokens: 0,
    });
    state.current = id.clone();
    store
        .save(&state)
        .map_err(|e| format!("写入会话索引失败：{e}"))?;
    Ok(NewSession { id, created: true })
}

/// 切到指定会话（不存在则忽略）。
pub fn switch_session(id: &str) -> Result<(), String> {
    let store = SessionStore::production();
    let _g = store.lock()?;
    let mut state = store.load();
    store.ensure_current(&mut state);
    if state.sessions.iter().any(|s| s.id == id) {
        state.current = id.to_string();
        store
            .save(&state)
            .map_err(|e| format!("写入会话索引失败：{e}"))?;
    }
    Ok(())
}

/// 设置某会话使用的供应商（「模型选择」落盘）。
pub fn set_session_provider(id: &str, provider: &str) -> Result<(), String> {
    let store = SessionStore::production();
    let _g = store.lock()?;
    let mut state = store.load();
    store.ensure_current(&mut state);
    if let Some(s) = state.sessions.iter_mut().find(|s| s.id == id) {
        s.provider = provider.to_string();
        store
            .save(&state)
            .map_err(|e| format!("写入会话索引失败：{e}"))?;
    }
    Ok(())
}

/// 气泡位置文件（GUI 专用；后端不读）。
pub fn bubble_pos_path() -> PathBuf {
    dir().join("bubble.json")
}

/// 分割比例文件（GUI 专用；后端不读）：`~/.agent-bridge/split.json`，内容形如
/// `{"ratio":0.68}`。**文件缺失/损坏/越界 ⇒ [`SPLIT_RATIO_DEFAULT`]**（不报错：记不住比例
/// 只影响观感，不该拦着开窗）。
pub fn split_path() -> PathBuf {
    dir().join("split.json")
}

/// 分割比例（落盘形状）。用结构体而不是裸浮点：将来加字段（比如每屏一份比例）不用改格式。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SplitRatio {
    pub ratio: f32,
}

/// 读分割比例；文件缺失/损坏/越界 ⇒ [`SPLIT_RATIO_DEFAULT`]。
pub fn load_split_ratio() -> f32 {
    load_split_ratio_from(&split_path()).unwrap_or(SPLIT_RATIO_DEFAULT)
}

/// 从指定路径读（单测用临时文件；生产走 [`split_path`]）。
pub fn load_split_ratio_from(path: &std::path::Path) -> Option<f32> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: SplitRatio = serde_json::from_str(&text).ok()?;
    // 越界/非有限都当「没有」（`clamp` 会把脏值变成一个看似合法的比例，宁可回默认值）。
    (v.ratio.is_finite() && (0.0..=1.0).contains(&v.ratio)).then_some(v.ratio)
}

/// 写分割比例（失败静默：同 [`save_bubble_pos`]，记不住比例不影响功能）。
pub fn save_split_ratio(ratio: f32) {
    save_split_ratio_to(&split_path(), ratio)
}

/// 写到指定路径（单测用临时文件；生产走 [`save_split_ratio`]）。
pub fn save_split_ratio_to(path: &std::path::Path, ratio: f32) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let v = SplitRatio {
        ratio: if ratio.is_finite() {
            ratio.clamp(0.0, 1.0)
        } else {
            SPLIT_RATIO_DEFAULT
        },
    };
    if let Ok(text) = serde_json::to_string(&v) {
        let _ = std::fs::write(path, text);
    }
}

/// 系统级会话的工作区：`~/.agent-bridge/workspaces/syschat/`。
pub fn workspace() -> PathBuf {
    crate::workspace_dir(SYSCHAT_KEY)
}

/// markdown 里媒体/文件地址解析的**唯一基准**：syschat 工作区。
///
/// agent 进程的 cwd 就是 [`workspace`]，它回复里的相对路径（`./assets/a.png`、
/// `assets/a.mp4`）都是相对工作区；ABB 托盘进程的 cwd 与它**不是同一个**，所以绝不能用
/// 进程 CWD 解析——那是 D7 卡里「相对路径图片看不到图 / 打不开」的直接原因。
/// 图片内联（`ui::load_block_image`）、媒体卡「用播放器打开」与普通「打开」/链接点击
/// （`ui::on_open_media` / `ui::on_open_link`）三处**必须共用本函数**，不得各写一份。
pub fn resolve_media_path(src: &str) -> PathBuf {
    let base = workspace();
    let direct = resolve_media_path_in(&base, src);
    if direct.is_file() {
        return direct;
    }
    // 回退：agent 常把文档写在**子目录**里、图片按「文档所在目录」写相对路径（消息里是
    // `assets/demo.svg`，文件在 `md-demo/assets/demo.svg`）⇒ 在**工作区**里按路径分量后缀
    // 找**唯一**匹配。找不到或有多个候选就不猜（返回原路径，调用方走「打开」卡片/报错）。
    if !src.starts_with("file://") && !std::path::Path::new(src).is_absolute() {
        if let Some(p) = find_by_suffix(&base, src) {
            return p;
        }
    }
    direct
}

/// 在 `base` 子树里找**路径后缀**等于 `rel` 的唯一文件（`rel` 按 `\"` 拼开）。
///
/// 有预算与深度上限（工作区可能很大，这个函数在 UI 线程上跑）：超预算/多个候选 ⇒ `None`。
fn find_by_suffix(base: &std::path::Path, rel: &str) -> Option<PathBuf> {
    let want: Vec<&str> = rel
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if want.is_empty() {
        return None;
    }
    // memo：**同一段历史每次重建行都会问一遍**，而 miss 的成本是遍历工作区子树
    //（owner 反馈的「GUI 打开很慢」里也有这份开销）。miss 也缓存。
    let key = (base.to_path_buf(), rel.to_string());
    if let Some(hit) = SUFFIX_MEMO.with(|m| m.borrow().get(&key).cloned()) {
        return hit;
    }
    let found = find_by_suffix_uncached(base, &want);
    SUFFIX_MEMO.with(|m| {
        let mut g = m.borrow_mut();
        if g.len() > 256 {
            g.clear();
        }
        g.insert(key, found.clone());
    });
    found
}

thread_local! {
    static SUFFIX_MEMO: std::cell::RefCell<
        std::collections::HashMap<(std::path::PathBuf, String), Option<std::path::PathBuf>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

fn find_by_suffix_uncached(base: &std::path::Path, want: &[&str]) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    let mut budget = 20_000usize;
    let mut stack = vec![(base.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if budget == 0 {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let p = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if depth < 6 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let Some(tail) = p.strip_prefix(base).ok() else {
                continue;
            };
            let tail: Vec<String> = tail
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect();
            if tail.len() >= want.len()
                && tail[tail.len() - want.len()..]
                    .iter()
                    .zip(want)
                    .all(|(a, b)| a == b)
            {
                if found.is_some() {
                    return None; // 多个候选 ⇒ 不猜
                }
                found = Some(p);
            }
        }
    }
    found
}

/// [`resolve_media_path`] 的可注入基准版本（供单测；不做 IO、不查存在性）。
///
/// - `file://` URL → 绝对路径（`file:///a/b.png` → `/a/b.png`；`%20` 百分号解码；
///   `file://localhost/a` 的 host 部分剥掉）；
/// - 绝对路径 → 原样；
/// - 其它（相对路径）→ `base.join(src)`。
pub fn resolve_media_path_in(base: &std::path::Path, src: &str) -> PathBuf {
    if let Some(rest) = src.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        return PathBuf::from(percent_decode(rest));
    }
    let p = std::path::Path::new(src);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// `%XX` 百分号解码（`file://` URL 用；非法/不完整转义原样保留）。手写避免为一条 URL 引入依赖。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ─────────────────────────── C 批：待发附件（📎 多选 / ⌘V 粘贴）───────────────────────────

/// 一个已落盘的待发附件。
///
/// `name` 是**工作区内的文件名**（带时间戳前缀、已清洗、已去重），`path` 是它的绝对路径
/// ——路径进 prompt（agent 自己按需读取），二进制绝不进 prompt（见 `docs/design/README.md`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// 落盘后的文件名（`<unix-ts>-<sanitized>[-N]`）。
    pub name: String,
    /// 落盘后的**绝对**路径（进 prompt 用）。
    pub path: PathBuf,
    /// 字节数（用 `fs::copy` 的实际写入量，便于展示/排查）。
    pub bytes: u64,
}

/// 单条文件名（含扩展名）允许的最大字符数：超长按字符截断但**保留扩展名**
///（后缀是 agent 判类型/选解析器的依据，截掉等于换个文件）。
const MAX_ATTACH_NAME_CHARS: usize = 80;

/// 系统对话附件的落盘目录：`~/.agent-bridge/workspaces/syschat/attachments/`。
pub fn attachments_dir() -> PathBuf {
    workspace().join("attachments")
}

/// 拖放（C2）的状态机。
///
/// **为什么单独抽成纯结构**：winit 的拖放事件**没有「一次拖放结束」的显式信号**——成功 drop 后
/// 它逐文件发 `DroppedFile`（`performDragOperation:`），而 `concludeDragOperation:` 不下发任何事件
/// （也就没有对应的取消事件）。所以「高亮何时收起」「同一路径会不会被落地两次」必须由我们自己
/// 定义口径；GUI 事件循环没法单测，把口径抽成纯结构才能用单测钉住。
///
/// 事件口径（winit 0.30.13 macOS 实现 `platform_impl/macos/window_delegate.rs`，逐行实核）：
/// - `draggingEntered:`（`:369`，取 `NSFilenamesPboardType` `:376`）把拖入的全部文件名**各**发一条
///   `HoveredFile`——拖 N 个文件就是 N 条；
/// - `performDragOperation:`（`:398`/`fn` `:399`，同样读 `:405`）逐文件发 `DroppedFile`；
/// - `draggingExited:`（`:426`）发 `HoveredFileCancelled`（拖出窗口/取消）；
/// - `concludeDragOperation:`（`:420`）**什么都不发**；
/// - 该版本**没有** `draggingUpdated:`（`:369`–`:429` 内只有上面四个方法）。
///
/// 上游边界（**源码已实核，行为后果未在本机实测**）：winit 只注册 `NSFilenamesPboardType`
/// （`:666-669`），其它类型取不到即 `return false`（`:376`/`:405`）⇒ 拖放源只提供别的类型时
/// **一个事件都不会有**（浏览器直拖图片可能属于这一类；锁屏期无法做真机拖放验证，待复验）。
/// 多文件拖放的「攒批」状态（C2-B 的根因修法）。
///
/// winit 对**一次**拖放手势会**逐文件**发 `DroppedFile`（`performDragOperation:` 每个文件各一回），
/// 而 UI 的状态行想报的是「这次进来几个」。若每个事件各自落地一次，状态行会被最后一条覆盖成
/// 「已附加 1 个文件」，📎 待发行却显示 N——两处数字对不上。UI 层把同窗内的事件攒进这里，
/// 由一个短单发定时器整批落地：`take()` 给出的就是**这一批**的真实个数。
#[derive(Default)]
pub struct DropBatcher {
    paths: Vec<PathBuf>,
}

impl DropBatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一个落点，返回当前批内已攒到的个数（= 下一句状态行该报的数）。
    pub fn push(&mut self, path: PathBuf) -> usize {
        self.paths.push(path);
        self.paths.len()
    }

    /// 取走整批（批随之清空，下一次手势从 0 重新计数）。
    pub fn take(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.paths)
    }
}

#[derive(Default)]
pub struct DropTracker {
    hovering: bool,
    landed: std::collections::HashSet<PathBuf>,
}

impl DropTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// `HoveredFile`：进入/继续悬停。
    /// 新一轮拖放的判定：悬停开始时若上一轮已结束（`hovering == false`）则清空去重集——
    /// 否则「把同一个文件拖两次」的第二次会被当成重复而静默丢掉。
    pub fn hovered(&mut self) {
        if !self.hovering {
            self.hovering = true;
            self.landed.clear();
        }
    }

    /// `DroppedFile(path)`：返回**是否应当落地**这个路径（本次拖放里首次出现）。
    /// 同时收起高亮并标记本轮结束（供下一次 [`Self::hovered`] 重置去重集）。
    pub fn dropped(&mut self, path: &std::path::Path) -> bool {
        self.hovering = false;
        self.landed.insert(path.to_path_buf())
    }

    /// `HoveredFileCancelled`：拖出/取消 ⇒ 收高亮 + 清去重集。
    ///
    /// 清去重集是**防御性**的（C2 批 A）：现实中每次拖入都会重新 `draggingEntered:`
    /// ⇒ 重新 [`Self::hovered`]，而那里在 `hovering` false→true 时必清。所以这条清空
    /// 在 winit 0.30.13 的事件序下取不到输入（变异掉它，既有用例全绿）。留着是因为代价为零，
    /// 而万一上游真的发来「取消后紧跟同一路径的 drop」（没有新的 hover），不清就会**静默吞掉**
    /// 这次落地。这条契约由 `drop_tracker_cancel_resets` 钉住。
    pub fn cancelled(&mut self) {
        self.hovering = false;
        self.landed.clear();
    }

    /// 当前是否应显示拖入高亮。
    pub fn hovering(&self) -> bool {
        self.hovering
    }
}

/// 把一批本地文件复制进 [`attachments_dir`] 并返回记录。
///
/// 任一文件不可读/落地失败即整体返回 `Err`（带可读原因），**并回滚本次已复制成功的文件**：
/// 调用方（GUI）据此把整批都视为失败，避免「部分附件静默缺失」。取消选择（空输入）是
/// `Ok(vec![])` 而不是错误。
pub fn land_attachments(paths: &[PathBuf]) -> Result<Vec<Attachment>, String> {
    land_attachments_in(&attachments_dir(), paths)
}

/// [`land_attachments`] 的可注入目录版：单测传临时目录，**绝不写真实 `~/.agent-bridge`**
/// （隔离门禁 `tools/check_test_isolation.sh` 会把落点抓出来）。
fn land_attachments_in(
    dir: &std::path::Path,
    paths: &[PathBuf],
) -> Result<Vec<Attachment>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("创建附件目录失败（{}）：{e}", dir.display()))?;
    // 同一批共用一个时间戳前缀：文件名成组、便于一眼看出「这批是一起附的」。
    let ts = crate::chrono_lite::unix_secs();
    let mut landed: Vec<Attachment> = Vec::with_capacity(paths.len());
    for src in paths {
        match land_one(dir, src, ts) {
            Ok(a) => landed.push(a),
            Err(e) => {
                // 回滚：调用方不会展示这批里的任何一个，留着没人引用的副本只会积垃圾。
                for a in &landed {
                    let _ = std::fs::remove_file(&a.path);
                }
                return Err(e);
            }
        }
    }
    Ok(landed)
}

/// 复制单个文件到 `dir`（失败返回可读原因）。
fn land_one(dir: &std::path::Path, src: &std::path::Path, ts: u64) -> Result<Attachment, String> {
    let meta =
        std::fs::metadata(src).map_err(|e| format!("读取附件失败（{}）：{e}", src.display()))?;
    if meta.is_dir() {
        return Err(format!("不支持目录（{}）", src.display()));
    }
    let raw = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment");
    let base = sanitize_attachment_name(raw);
    let target = unique_attachment_path(dir, &base, ts);
    let bytes = match std::fs::copy(src, &target) {
        Ok(n) => n,
        Err(e) => {
            // 半截目标：`copy` 是「先建目标再写」，写到一半失败（ENOSPC/EIO）会留下一个
            // 看起来像附件、内容却不完整的文件——`land_attachments_in` 的回滚只清**成功项**
            // （`landed`），碰不到它，于是它会默默积在附件目录里（C 批 O1）。
            // 这里 best-effort 删掉；删不掉也不叠加错误（已经在错误路径上）。
            //
            // 如实记：这条分支在测试里**无法确定性触发**——std 的 `fs::copy` 先 `open` 源并校验
            // 「regular file」，再 `open` 目标；所有能诱导的失败（缺文件/目录/超长名/不可写目录）
            // 都在目标的 `open` 上就失败 ⇒ 目标要么压根没建、要么是早已存在的那个文件
            // （`unique_attachment_path` 保证不撞名）。「已建、写到一半」只可能来自**写中途失败**
            // （ENOSPC/EIO），造不出来。故此处只保证行为，不声称有判据。
            let _ = std::fs::remove_file(&target);
            return Err(format!(
                "复制附件失败（{} → {}）：{e}",
                src.display(),
                target.display()
            ));
        }
    };
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(base);
    Ok(Attachment {
        name,
        path: target,
        bytes,
    })
}

/// 目标路径 `<ts>-<base>`；同名冲突加 `-2`/`-3`…（含本批内先落盘的那些，因为它们已在盘上）。
fn unique_attachment_path(dir: &std::path::Path, base: &str, ts: u64) -> PathBuf {
    let (stem, ext) = split_attachment_ext(base);
    let mut n: u32 = 1;
    loop {
        let name = if n == 1 {
            format!("{ts}-{stem}{ext}")
        } else {
            format!("{ts}-{stem}-{n}{ext}")
        };
        let p = dir.join(&name);
        if !p.exists() {
            return p;
        }
        n += 1;
        // 上界只是防疯跑（同一秒撞 1000 次同名），真撞上就用 UUID 兜底而不是死循环。
        if n > 1000 {
            return dir.join(format!(
                "{ts}-{stem}-{}{ext}",
                uuid::Uuid::new_v4().simple()
            ));
        }
    }
}

/// 拆「主名 / 扩展名」：扩展名含点、最多 16 字符（`'.' + ≤16`）；隐藏文件（`.gitignore`）
/// 与「点后无内容」不算扩展名。
fn split_attachment_ext(name: &str) -> (String, String) {
    if let Some(i) = name.rfind('.') {
        if i > 0 && i + 1 < name.len() {
            let ext = &name[i..];
            if ext.chars().count() <= 17 {
                return (name[..i].to_string(), ext.to_string());
            }
        }
    }
    (name.to_string(), String::new())
}

/// 文件名清洗：把路径分隔/控制字符/文件系统禁用字符换成 `_`，去首尾空白与前导点，
/// 超长按字符截断且保留扩展名。
///
/// 为什么必须清洗：`file_name()` 虽已是末段，但仍可能含换行（会破坏 prompt 里
/// `- <路径>` 的行结构）与 Windows 非法字符；用户也可能选到超长/隐藏/纯点的名字。
fn sanitize_attachment_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let bad =
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
        out.push(if bad { '_' } else { c });
    }
    let out = out.trim().trim_start_matches('.').to_string();
    let out = if out.chars().all(|c| c == '.') {
        // 空串 / 纯点（`.`、`..`）都不能当文件名。
        "attachment".to_string()
    } else {
        out
    };
    truncate_attachment_name(&out, MAX_ATTACH_NAME_CHARS)
}

/// 超长截断（按字符，UTF-8 安全）且保留扩展名。
fn truncate_attachment_name(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let (stem, ext) = split_attachment_ext(name);
    let ext_len = ext.chars().count();
    // 扩展名自己就把预算占满时不再保留（否则截出来只剩后缀，反而丢信息）。
    if ext_len > 0 && ext_len < max {
        let budget = max - ext_len;
        let stem: String = stem.chars().take(budget).collect();
        format!("{stem}{ext}")
    } else {
        name.chars().take(max).collect()
    }
}

// ─────────────────────────── D 批：待发附件 3+N 折叠（纯逻辑）───────────────────────────

/// 折叠态最多展示的附件数（稿 `.pending`：最多 3 个，其余折叠成 `+N`）。
pub const PENDING_MAX_SHOWN: usize = 3;

/// 某一时刻待发附件在输入框内的可见切分。
///
/// `shown` 个可见 + `hidden` 个折叠；`hidden == 0` 时**不渲染** `+N`（见 [`Self::has_more`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingView {
    pub shown: usize,
    pub hidden: usize,
}

impl PendingView {
    /// 是否有折叠项（`N > 0` ⇒ 渲染 `+N`；`N == 0` 不渲染）。
    pub fn has_more(&self) -> bool {
        self.hidden > 0
    }
}

/// 待发附件行的**展开/折叠状态**（纯逻辑，Slint 侧只读 [`Self::view`] 的结果）。
///
/// 抽成纯结构的原因：Slint 事件循环没法单测，而「前 3 / 余 N、展开后可再收起、按索引移除后
/// 重新分组」是可独立验证的口径（对齐稿 `.pending` / `.more`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingState {
    expanded: bool,
}

impl PendingState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// 点 `+N` 展开 / 再点收起。
    pub fn toggle(&mut self) {
        self.expanded = !self.expanded;
    }

    /// 按当前折叠态给出 `(可见数, 折叠数)`。
    ///
    /// 展开态：全部可见、`hidden == 0`；折叠态：`min(len, PENDING_MAX_SHOWN)` 可见，其余折叠。
    pub fn view(&self, len: usize) -> PendingView {
        let shown = if self.expanded {
            len
        } else {
            len.min(PENDING_MAX_SHOWN)
        };
        PendingView {
            shown,
            hidden: len - shown,
        }
    }
}

/// 按索引从**待发列表**移除一项；返回移除后的新长度。越界是 no-op。
///
/// **只动内存里的待发列表，不删已落盘文件**（C1 口径：落盘副本可能已被已发出的消息引用，
/// 且同一附件可再次附加）。移除后调用方用 [`PendingState::view`] 按新长度重新分组。
pub fn remove_pending_at<T>(list: &mut Vec<T>, idx: usize) -> usize {
    if idx < list.len() {
        list.remove(idx);
    }
    list.len()
}

/// 附件是否按**图片缩略图**（`.pthumb`）渲染；否则按文件 chip（`.pfile`）。
///
/// 只认常见图片扩展名——音视频按稿子的 `.pfile`/类型角标呈现（不内嵌播放器缩略图）。
pub fn is_image_attachment(name: &str) -> bool {
    matches!(
        attachment_ext_lower(name).as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "avif" | "svg"
    )
}

/// 文件 chip / 缩略图角标上的类型文案：扩展名大写（最多 4 字），无扩展名回落 `FILE`。
pub fn attachment_kind_label(name: &str) -> String {
    let ext = attachment_ext_lower(name);
    if ext.is_empty() {
        "FILE".to_string()
    } else {
        ext.to_uppercase().chars().take(4).collect()
    }
}

/// 取小写扩展名（不含点）；无扩展名返回空串。
fn attachment_ext_lower(name: &str) -> String {
    let (_, ext) = split_attachment_ext(name);
    ext.trim_start_matches('.').to_ascii_lowercase()
}

/// 流式气泡的**锚点是否已失效**：模型被整体重载（`load_syschat_into` 里的 `set_vec`）后，
/// 记下的行号起点已不再指向那个流式气泡。
///
/// **为什么必须有这条判据（E1 评审 B1）**：`SysChatEvt::MediaReady` 会在**不受 `busy` 门控**的
/// 情况下触发整段重载历史（异步 http 媒体下载完成后），所以慢的媒体 URL 会让「重载」落在
/// **下一轮流式进行中**。此时陈旧的 `start` 已指向无效区间：若 `start > row_count`，
/// `sync_stream_rows` 删不到行，随后每个 delta 都把累积全文**整段重推**（重复气泡）；
/// 若 `start < row_count` 但区间已串位，Reply 收尾时会删错行（把真实历史行删掉）。
/// 故用「起点是否越出当前行数」作为失效信号，失效则复位为 `usize::MAX`（下一次 delta 会以
/// 累积全文重新锚一条新气泡，自愈）。
///
/// `usize::MAX` = 尚未锚定（正常状态），恒不失效。
pub fn stream_anchor_stale(start: usize, row_count: usize) -> bool {
    start != usize::MAX && start > row_count
}

/// http(s) 媒体的本地缓存文件路（无论是否存在）：`syschat/media-cache/<sha256(url)>`。
/// 用内容哈希做文件名：同 URL 同名、不同 URL 不撞，且无需维护索引。
pub fn media_cache_file(url: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    let hex = format!("{:x}", h.finalize());
    dir().join("media-cache").join(hex)
}

/// Slint 能按扩展名解码的图片后缀（`i-slint-core` 的 `image` 依赖开了 `png`+`jpeg`，
/// 另有 `svg`/`gif` 等自身解码器）。**没扩展名的缓存一律不认**——Slint 靠扩展名选解码器，
/// 这正是「远程图片一直显示不出来」的根因（旧缓存文件名就是纯 hash）。
const MEDIA_CACHE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "ico", "avif",
];

/// 从**文件头**猜图片后缀（下载落盘时用；`None` = 不认识的格式，宁可报错也别写个假扩展名）。
fn sniff_media_ext(bytes: &[u8]) -> Option<&'static str> {
    let b = bytes;
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("png");
    }
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if b.starts_with(b"GIF8") {
        return Some("gif");
    }
    if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some("webp");
    }
    if b.starts_with(b"BM") {
        return Some("bmp");
    }
    if b.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some("ico");
    }
    if b.len() > 12 && &b[4..8] == b"ftyp" && (&b[8..12] == b"avif" || &b[8..12] == b"avis") {
        return Some("avif");
    }
    // SVG 是文本：看开头一小段里有没有 `<svg`
    let head = &b[..b.len().min(512)];
    if head.windows(4).any(|w| w == b"<svg") {
        return Some("svg");
    }
    None
}

/// 已缓存的 http(s) 媒体路径（未缓存返回 None）。
pub fn media_cache_path(url: &str) -> Option<PathBuf> {
    let base = media_cache_file(url);
    let hex = base.file_name()?.to_string_lossy().to_string();
    let parent = base.parent()?;
    // 缓存文件名 = `<sha256>.<ext>`（见 `MEDIA_CACHE_EXTS` 的原因）。目录里按前缀找
    // ——**无扩展名的旧文件直接忽略**（Slint 读不了，留着只会让图片永远不显示）。
    for entry in std::fs::read_dir(parent).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix(&hex) else {
            continue;
        };
        let Some(ext) = rest.strip_prefix('.') else {
            continue;
        };
        if MEDIA_CACHE_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            return Some(entry.path());
        }
    }
    None
}

/// 下载 http(s) 媒体到本地缓存（已在缓存则直接返回）。
/// 供 GUI 后台线程调用；失败返回可读原因（GUI 显示为链接回退，不崩）。
/// `data:` URI（内联 base64 媒体）→ 落进媒体缓存并返回**本地路径**；非 data URI 返回 `None`。
///
/// 为什么必须有这条：agent 常把「自包含 HTML」里的图片/视频写成 `data:image/png;base64,…`/
/// `data:video/mp4;base64,…`（owner 2026-10-16 现场截图：整篇文字骨架 + 「图片打不开」）。
/// 本仓的媒体解析只认**本地路径**与 **http(s)**，data URI 被当成文件名去找 ⇒ 必然找不到。
/// 这里把它解码落盘，之后**完全复用**既有渲染/播放链路（图片块、音视频卡都不改）。
///
/// 缓存键沿用 [`media_cache_file`]（整个 URI 的 sha256 ⇒ payload 变了键就变）；扩展名先按
/// **文件头**嗅探、再退回 mime（`data:image/svg+xml` 这类文本格式嗅不出来，只能靠 mime）。
pub fn data_uri_path(src: &str) -> Option<PathBuf> {
    let rest = src.strip_prefix("data:")?;
    if let Some(hit) = media_cache_path(src) {
        return Some(hit);
    }
    let (meta, payload) = rest.split_once(',')?;
    if !meta.to_ascii_lowercase().contains("base64") {
        // 非 base64（百分号编码）的 data URI 极少见，**不做**：返回 None 让调用方走原路径。
        return None;
    }
    let mime = meta
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let bytes = base64_decode(payload)?;
    if bytes.is_empty() {
        return None;
    }
    let ext = sniff_media_ext(&bytes).or_else(|| ext_from_mime(&mime))?;
    let path = media_cache_file(src).with_extension(ext);
    if path.is_file() {
        return Some(path);
    }
    let parent = path.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(path)
}

/// mime → 扩展名（嗅探不出来时的兜底：svg 是文本、容器格式靠 mime 更稳）。
fn ext_from_mime(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/aac" => "aac",
        "audio/ogg" => "ogg",
        _ => return None,
    })
}

/// 标准字母表 base64 解码（项目原则：零依赖，手写）。容忍换行/空白（data URI 常被折行）。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

pub async fn fetch_media(url: &str) -> Result<PathBuf, String> {
    if let Some(p) = media_cache_path(url) {
        return Ok(p);
    }
    let path = media_cache_file(url);
    let parent = path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "缓存目录非法".to_string())?;
    std::fs::create_dir_all(&parent).map_err(|e| format!("创建缓存目录失败：{e}"))?;
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("下载失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载失败：HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取响应失败：{e}"))?;
    // 按**文件头**定扩展名（Slint 靠扩展名选解码器；没有扩展名就永远加载不出来）。
    let ext = sniff_media_ext(&bytes).ok_or_else(|| "下载回来的不是认识的图片格式".to_string())?;
    let path = path.with_extension(ext);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("写入缓存失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("落盘缓存失败：{e}"))?;
    Ok(path)
}

/// 气泡位置（**逻辑像素**）。持久化在 `bubble.json`。
///
/// 全链路统一逻辑像素：恢复用 `LogicalPosition`、首次默认位按逻辑屏宽算、拖拽保存也
/// 换算成逻辑后再存（见 [`BubblePos::from_physical`]）。混用物理/逻辑会在 Retina(2x)
/// 上让位置每次重启翻倍（评审 R3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BubblePos {
    pub x: i32,
    pub y: i32,
}

impl BubblePos {
    /// 把 winit `WindowEvent::Moved` 的**物理**像素换算成**逻辑**像素后存盘。
    ///
    /// 为什么需要：winit 的 `Moved` 给的是物理像素（macOS 侧显式
    /// `LogicalPosition::to_physical(scale_factor)`），而本类型存的是逻辑像素。
    /// scale_factor ≤ 0（异常/拿不到）时按 1 处理，避免除零产生 inf → 饱和成 i32::MAX。
    pub fn from_physical(x: i32, y: i32, scale_factor: f32) -> Self {
        let s = if scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        Self {
            x: (x as f32 / s).round() as i32,
            y: (y as f32 / s).round() as i32,
        }
    }
}

/// 读气泡位置；文件缺失/损坏返回 None（调用方用默认位）。
pub fn load_bubble_pos() -> Option<BubblePos> {
    let text = std::fs::read_to_string(bubble_pos_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写气泡位置（失败静默：位置记不住不影响功能）。
pub fn save_bubble_pos(pos: BubblePos) {
    if std::fs::create_dir_all(dir()).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string(&pos) {
        let _ = std::fs::write(bubble_pos_path(), text);
    }
}

// ──────────── 标题栏控件条（稿 v10：①新建会话 / ②历史会话 / ③模型 / ④⚙）的落位几何 ────────────
//
// 稿把这四个控件放在**标题栏**里；而 slint-pixel 的 `PixelTitleBar` 是封闭组件（不能塞子元素），
// 所以它们改为在根 Rectangle 里、VerticalLayout **之后**叠一条控件条（后画 = 上层），靠右贴窗口
// 按钮组左侧。本组函数只算「摆在哪、多宽、放不下时换哪一档」，画由 `ui/app.slint` 负责
// ——这样算术可单测、观感可截图核对。

/// 系统对话窗的初始逻辑宽（= `ui/app.slint` 的 `preferred-width`，src 里显式 `set_size` 一次）。
/// 用途：窗口还没 realize（`Window::size()` 给 0）时算计划的兜底宽度。
/// 与 `.slint` 的一致性由 [`tests::syschat_window_size_constants_match_slint`] 守着。
pub const SYSCHAT_PREFERRED_WIDTH: f32 = 520.0;

/// 系统对话窗的**初始**逻辑高（= `.slint` 的 `preferred-height`）：`no-frame` 窗在 macOS 上
/// 吃不到 `preferred-*`（D 批实测开出来是最小尺寸），故开窗前显式 `set_size` 一次。
pub const SYSCHAT_DEFAULT_H: f32 = 560.0;
/// 系统对话窗的**最小**尺寸（= `.slint` 的 `min-width` / `min-height`）：交回系统做缩放下界。
/// **不是**固定尺寸：`min != max` ⇒ Slint 的 winit 后端把窗口标为可缩放
/// （`i-slint-backend-winit` 的 `window_is_resizable`）。
pub const SYSCHAT_MIN_W: f32 = 340.0;
pub const SYSCHAT_MIN_H: f32 = 380.0;

/// **设置窗**的最小尺寸（= `ui/app.slint` 的 `SettingsWindow` `min-width`/`min-height`）。
///
/// 与 [`SYSCHAT_MIN_W`]/[`SYSCHAT_MIN_H`] 同口径但**各管各的窗**：设置窗也有自管缩放
/// （macOS，卡 `abb-v11-resize-maximize-macos` 的同一套，2026-10-15 扩到设置窗），
/// 每拍按它夹取下界；两个窗下界不同 ⇒ `SelfResize` 一个窗口一份、下界随窗给。
/// 跨文件守卫见 `tests::settings_window_min_constants_match_slint`。
pub const SETTINGS_MIN_W: f32 = 880.0;
pub const SETTINGS_MIN_H: f32 = 620.0;

/// 消息区正文块的可用宽相对**窗宽**要扣掉的全部内边距（D2「消息区底部文字重叠」修复用）。
///
/// 构成：根 Rectangle 的 layout padding 4（2×2）+ `col` 的 padding 24（12×2）+ 每个块自己的
/// layout padding 16（std-widgets 的 HorizontalBox/VerticalBox 默认 8×2，见
/// `LESSON_...默认 padding 挤爆行高`）。运行时由 [`crate::ui::apply_syschat_layout`] 写进
/// `SystemChatWindow.convo-text-width`；`.slint` 里各处在这个基准上再扣自己的额外内边距。
pub const CONVO_BLOCK_TEXT_INSET: f32 = 44.0;

/// `convo-text-width` 的下限：窗口被压到极窄时也别把正文宽度算成 0（宽度 0 会让所有文本各占一行，
/// 估高变成天文数字）。
pub const CONVO_TEXT_WIDTH_MIN: f32 = 120.0;

/// 窗口按钮**整区**宽（slint-pixel 0.3.0 `ui/pixel_painter_widget.slint`：按钮组
/// `x: parent.width - 106px; width: 102px`，内居 3×30 + 2×6）。
///
/// **必须按「整区」让位，不能按「当前可见了几颗按钮」算**（D2 复查 2026-10-13 真机实测的教训）：
/// 之前用 `w - 70`（= 2 颗按钮 + 间隙 + 边距）当按钮区左缘，340px 窄窗下 ③⚙ 实测**压在
/// 最小化按钮的浅色按钮底上**——那个浅色底是主题给的 `face`，宽度不受 30px 图标盒约束，
/// 按几何常数精算只会把「差几像素」变成偶发重叠。留整个 106px 区 + `TB_STRIP_GAP` 才与
/// 主题无关地安全。
const TB_BTN_ZONE_W: f32 = 106.0;
/// 控件条右缘与最左侧那颗窗口按钮之间的最小间隔（不许贴脸）。
const TB_STRIP_GAP: f32 = 8.0;
/// 标题文字右缘估算：`PixelTitleBar` 里标题 `x: 40px`，"ABB · 系统对话" 13px 粗体 ≈ 92px。
const TB_TITLE_END: f32 = 132.0;
/// 控件条左缘与标题文字之间的最小间隔。
const TB_TITLE_GAP: f32 = 8.0;
/// 新建会话（气泡图标）方钮宽（稿 v9 `.tb-btn`：26×26）。
///
/// **位置约定（v10）**：新建会话**只有这一个入口**（v10 已把输入区左侧的 `＋` 拆掉），
/// 所以它排在标题栏控件条最左、且是最后才让位的一项（见 `titlebar_plan` 的换档）。
pub const TB_NEW_W: f32 = 26.0;
/// 历史会话（🗂 图标）方钮宽（稿 `.tb-btn` 26×26）。
///
/// 与旧值的区别：v9 之前它是一个 84px 的带文案 PixelMenu；v10 起标题栏里**只有图标**
/// （稿 v8 第 2 条「两个都只要图标、不要文字」），点开由弹窗列会话。
pub const TB_SESSION_W: f32 = 26.0;
/// 窄窗回退位（状态行）里的会话 PixelMenu 固定宽——那里仍是**带文案**的菜单，需要更宽：
/// PixelMenu 内部 `Text { x: 10px; width: parent.width - 36px }` ⇒ 84px 给文案 48px
/// （13px 字号 ≈ 3 个全宽字 = "新会话" 刚好放全）。`ui/app.slint` 的 D2 回退位按此值画。
pub const TB_SESSION_FALLBACK_W: f32 = 84.0;
/// 模型 chip 理想宽：112px 给文案 76px（≈ 11 个半宽字符，"deepseek-chat" 需 13 个 ⇒ 末尾省略）。
/// 参照稿本身：`chipbtn` 是 `max-width:150px` + `text-overflow:ellipsis`（稿里它也被 flex 压到
/// ~117px）——即稿也承认 chip 会省略号，不是「必须全显」。
pub const TB_MODEL_W: f32 = 112.0;
/// 模型 chip 下限：再窄连 6 个半宽字符都放不下，不如让位（见 `titlebar_plan` 的换档）。
const TB_MODEL_W_MIN: f32 = 88.0;
/// ③ ⚙ 方钮（26×26，同稿 `.tb-btn`）。
const TB_SETTINGS_W: f32 = 26.0;
/// 控件条内各项间距（同稿 `.titlebar` 的 `gap: 6px`）。
const TB_ITEM_GAP: f32 = 6.0;

/// 标题文字是否还画得下：整条左缘越过「标题右缘 + 间隔」就说明没压标题。
/// 窄窗时调用方**先牺牲标题**（置空），把位置让给 ①②③④——owner 口径：它们必须与
/// 最小化/最大化/关闭同在标题栏那一行，标题只是装饰。
pub fn titlebar_title_visible(plan: &TitlebarPlan) -> bool {
    plan.x >= TB_TITLE_END + TB_TITLE_GAP
}

/// chip 内文案可用宽 = chip 宽 − 内边距（PixelMenu：左 10px + 右侧箭头区 26px）。
const TB_CHIP_TEXT_INSET: f32 = 36.0;
/// 13px 粗体（PixelMenu 的 `button-text`）下单个**半宽**字符的估宽（全宽字符按 2 倍算）。
/// 取整偏保守（真值约 6.3–6.9），宁可早一个字符加省略号也不要溢出 chip。
const TB_HALF_CHAR_W: f32 = 7.0;

/// 标题栏控件条的落位计划（逻辑像素，相对窗口左上角）。
///
/// 顺序（自左向右，v10 稿 `#win` 的 titlebar 段）：
/// ① 新建会话（气泡图标）/ ② 历史会话（🗂 图标）/ ③ 模型 chip（带文案）/ ④ ⚙设置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TitlebarPlan {
    /// 整条左缘；**< 0 = 空档塞不下任何一项，整条不画**。
    pub x: f32,
    /// ①新建会话方钮宽（0 = 不画；`x >= 0` 时按换档约定它必定 > 0）。
    pub new_w: f32,
    /// ②历史会话方钮宽（0 = 不画，落到状态行回退位）。
    pub session_w: f32,
    /// ③模型 chip 宽（0 = 不画；模型列表为空时调用方也传 `has_model=false`）。
    pub model_w: f32,
    /// ④⚙ 方钮宽（0 = 不画）。
    pub settings_w: f32,
}

/// 算出标题栏控件条该摆在哪、各项多宽（纯函数，口径钉在单测里）。
///
/// 空档 = 窗口宽 − 右侧「窗口按钮组 + 间隔」− 左侧「标题文字 + 间隔」。
/// 换档顺序按「能力可替代性」（v10 重排）：
/// - **新建会话**没有第二入口（v10 已从输入区移除 `＋`）⇒ 最优先保留，连它都放不下才整条不画；
/// - ⚙设置 在托盘菜单里有等价入口 ⇒ 第一个让位；
/// - 模型下拉在状态行回退位里有等价入口（`titlebar-model-w <= 0` 分支）⇒ 其次让位；
/// - 历史会话在状态行回退位里有等价入口（`titlebar-session-w <= 0` 分支）⇒ 再次。
///
/// 具体：①全量（模型理想宽）→ ②压缩模型宽（下限 `TB_MODEL_W_MIN`）→ ③让 ⚙ → ④让模型 →
/// ⑤只留新建会话 → ⑥整条不画（`x = -1`）。
pub fn titlebar_plan(window_w: f32, has_model: bool) -> TitlebarPlan {
    // 按**整区**让位（见 `TB_BTN_ZONE_W` 的注释：按可见按钮数精算在 340px 窄窗下会压到按钮底）。
    let buttons_left = window_w - 4.0 - TB_BTN_ZONE_W;
    let right_limit = buttons_left - TB_STRIP_GAP;
    let left_limit = TB_TITLE_END + TB_TITLE_GAP;
    let avail = right_limit - left_limit;

    let none = TitlebarPlan {
        x: -1.0,
        new_w: 0.0,
        session_w: 0.0,
        model_w: 0.0,
        settings_w: 0.0,
    };
    // 新建会话是唯一入口（v10 已从输入区移除），连它都放不下才整条不画。
    if avail < TB_NEW_W {
        return none;
    }
    let model_room = |w: f32| -> Option<f32> {
        // 用模型上限去夹：下限都放不下就返回 None（调用方换下一档）。
        (w >= TB_MODEL_W_MIN).then(|| w.min(TB_MODEL_W))
    };
    // ① 全量：new + session + model(理想) + ⚙。
    let with_all = avail - TB_NEW_W - TB_SESSION_W - TB_SETTINGS_W - 3.0 * TB_ITEM_GAP;
    if has_model {
        if let Some(model_w) = model_room(with_all) {
            let total = TB_NEW_W + TB_SESSION_W + model_w + TB_SETTINGS_W + 3.0 * TB_ITEM_GAP;
            return TitlebarPlan {
                x: right_limit - total,
                new_w: TB_NEW_W,
                session_w: TB_SESSION_W,
                model_w,
                settings_w: TB_SETTINGS_W,
            };
        }
    }
    // ② 让出 ⚙（托盘菜单里有「设置…」等价入口）；模型仍留。
    let without_settings = avail - TB_NEW_W - TB_SESSION_W - 2.0 * TB_ITEM_GAP;
    if has_model {
        if let Some(model_w) = model_room(without_settings) {
            let total = TB_NEW_W + TB_SESSION_W + model_w + 2.0 * TB_ITEM_GAP;
            return TitlebarPlan {
                x: right_limit - total,
                new_w: TB_NEW_W,
                session_w: TB_SESSION_W,
                model_w,
                settings_w: 0.0,
            };
        }
    }
    // ③ 让出模型（状态行回退位接）；会话仍留。
    let with_gear = TB_NEW_W + TB_SESSION_W + TB_SETTINGS_W + 2.0 * TB_ITEM_GAP;
    if with_gear <= avail {
        return TitlebarPlan {
            x: right_limit - with_gear,
            new_w: TB_NEW_W,
            session_w: TB_SESSION_W,
            model_w: 0.0,
            settings_w: TB_SETTINGS_W,
        };
    }
    // ④ 让出 ⚙ 与模型，只留 new + session（两枚 26px 方钮）。
    let two = TB_NEW_W + TB_SESSION_W + TB_ITEM_GAP;
    if two <= avail {
        return TitlebarPlan {
            x: right_limit - two,
            new_w: TB_NEW_W,
            session_w: TB_SESSION_W,
            model_w: 0.0,
            settings_w: 0.0,
        };
    }
    // ⑤ 只留新建会话（历史会话落到状态行回退位）。min-width 340px 下不会走到这里，防御性保留。
    TitlebarPlan {
        x: right_limit - TB_NEW_W,
        new_w: TB_NEW_W,
        session_w: 0.0,
        model_w: 0.0,
        settings_w: 0.0,
    }
}

/// 把 chip 文案按可用宽截断（放不下时末尾加 `…`）。
///
/// 为什么在 Rust 侧截断而不是靠 Slint 的 `overflow: elide`：`PixelMenu` 的内部 Text 是**上游组件
/// 的私有实现**（没有 elide 绑定），长会话标题（`derive_title` 可到 24 字）会直接画出 chip 外、
/// 压到隔壁控件——正是本轮要修的「互相压住」。全宽/半宽按 `TB_HALF_CHAR_W` 估算：
/// 只是**显示**截断，宁短勿长。
pub fn fit_chip_label(label: &str, chip_w: f32) -> String {
    let budget = chip_w - TB_CHIP_TEXT_INSET - 2.0; // 再留 2px 让描边不顶字
    if budget <= 0.0 {
        return String::new();
    }
    // 连省略号那一格都放不下：宁可空着，也不要截出一个比可用宽还长的「…」
    // （本仓教训：截断类函数的输出必须真的落在预算内，见单测里的逐档估宽断言）。
    if budget < TB_HALF_CHAR_W {
        return String::new();
    }
    let width = |c: char| -> f32 {
        // CJK/全角标点等按 2 个半宽算；其余（含 ASCII、拉丁字母）按 1 个。（`char::width` 需要
        // 额外 crate，这里用「是否是宽字符」的保守判据：CJK 区段 + 全角形式。）
        if is_wide_char(c) {
            TB_HALF_CHAR_W * 2.0
        } else {
            TB_HALF_CHAR_W
        }
    };
    let total: f32 = label.chars().map(width).sum();
    if total <= budget {
        return label.to_string();
    }
    // 省略号自身也占一格；逐字累加到放不下为止。
    let mut out = String::new();
    let mut used = 0.0;
    for c in label.chars() {
        let w = width(c);
        if used + w > budget - TB_HALF_CHAR_W {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// 宽字符判据（CJK 与全角形式区）。只用于**估宽**，不要求覆盖 Unicode 全部宽字符：
/// 漏判只是把某个字符估窄了 7px（可能少截一个字），不会越界画出去——保守方向由
/// `budget` 里的 2px 余量与「宁短勿长」的截断口径兜住。
fn is_wide_char(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F        // 韩文字母
        | 0x2E80..=0x303E      // CJK 部首 / 标点
        | 0x3041..=0x33FF      // 日文假名 / CJK 兼容
        | 0x3400..=0x4DBF      // CJK 扩展 A
        | 0x4E00..=0x9FFF      // CJK 基本区
        | 0xA000..=0xA4CF      // 彝文
        | 0xAC00..=0xD7A3      // 韩文音节
        | 0xF900..=0xFAFF      // CJK 兼容表意
        | 0xFE30..=0xFE6F      // CJK 兼容形式
        | 0xFF00..=0xFF60      // 全角形式
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD) // CJK 扩展 B+
}

/// token 进度条**内条**可用宽（稿 `.meter{width:56px;border:2px}` ⇒ 内容盒 52px）。
/// 内条填宽 = 这个值 × 百分比；`ui/app.slint` 的 `usage-fill` 宽也按这个上限夹取。
pub const USAGE_METER_INNER_W: f32 = 52.0;

/// 自绘面板（`.menu`）宽（稿 `width:250px`）。Rust 算锚点与 `ui/app.slint` 的 `MenuPanel`
/// 必须同值：面板靠右对齐、靠边内移都靠它。
pub const MENU_PANEL_W: f32 = 250.0;
/// 标题栏控件条里那颗钮的绝对 y（`ui/app.slint` 的 `titlebar-strip-x` 那条布局：`y: 6px`）。
/// **改 .slint 那行时这里要同步**（面板锚点算的是「钮下缘 + 6px」）。
pub const TB_BTN_Y: f32 = 6.0;
/// 标题栏钮高（稿 `.tb-btn{height:26px}`）。同上：与 .slint 同步。
pub const TB_BTN_H: f32 = 26.0;

/// 标题栏「历史会话」钮在窗口坐标里的矩形 `(x, w)`；没画（窄窗换档换掉了）返回 None。
///
/// 算术与 `titlebar_plan` 同源：控件条从 `plan.x` 起按 `[新建][会话][模型][⚙]` 排、
/// 间距 `TB_ITEM_GAP`。
pub fn titlebar_session_btn(plan: &TitlebarPlan) -> Option<(f32, f32)> {
    (plan.x >= 0.0 && plan.session_w > 0.0)
        .then_some((plan.x + plan.new_w + TB_ITEM_GAP, plan.session_w))
}

/// 标题栏「模型」chip 在窗口坐标里的矩形 `(x, w)`；没画返回 None（同 [`titlebar_session_btn`]）。
pub fn titlebar_model_btn(plan: &TitlebarPlan) -> Option<(f32, f32)> {
    (plan.x >= 0.0 && plan.model_w > 0.0).then_some((
        plan.x + plan.new_w + TB_ITEM_GAP + plan.session_w + TB_ITEM_GAP,
        plan.model_w,
    ))
}

// ─────────────────── v11：面板与上下文用量（卡 `abb-v11-panels-align`）───────────────────

/// 历史会话面板每项右侧的 tag（稿 `.menu .item .tag`）：当前会话写「当前」，
/// 其余写相对时间（今天/昨天 `HH:MM`，更早写 `M月D日`）。
///
/// `now` 与 `updated_at` 都是 **UNIX 秒**；日历换算走 `chrono_lite` 的本地
/// （UTC+8）口径——与 `chrono_lite::now()` / #74 历史页同一套，不引依赖。
/// `updated_at <= 0`（旧数据缺字段）→ 空 tag，不编造时间。
pub fn session_tag(updated_at: i64, now: u64, is_current: bool) -> String {
    if is_current {
        return "当前".to_string();
    }
    if updated_at <= 0 {
        return String::new();
    }
    const TZ: i64 = 8 * 3600;
    let local = updated_at.saturating_add(TZ).max(0) as u64;
    let today = (now as i64).saturating_add(TZ).max(0) as u64 / 86400;
    let day = local / 86400;
    let (_, mo, d, h, mi, _) = crate::chrono_lite::epoch_to_ymd(local);
    if day == today {
        format!("今天 {h:02}:{mi:02}")
    } else if day + 1 == today {
        format!("昨天 {h:02}:{mi:02}")
    } else {
        format!("{mo}月{d}日")
    }
}

/// token 数的紧凑写法（稿：`12.4K/128K`）：<1000 原样，K/M 各保留一位小数、
/// 整数时去掉 `.0`（`128000` → `128K`，不是 `128.0K`）。
pub fn token_short(n: u64) -> String {
    let trim = |v: f64| -> String {
        let s = format!("{v:.1}");
        if let Some(stripped) = s.strip_suffix(".0") {
            stripped.to_string()
        } else {
            s
        }
    };
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}K", trim(n as f64 / 1000.0))
    } else {
        format!("{}M", trim(n as f64 / 1_000_000.0))
    }
}

/// 上下文占用百分比（0..=100；`limit == 0` 或已超上限时按口径夹取）。
pub fn usage_percent(used: u64, limit: u64) -> f32 {
    if limit == 0 {
        return 0.0;
    }
    ((used as f64 / limit as f64) * 100.0).clamp(0.0, 100.0) as f32
}

/// 进度条档位（稿 v11 `.meter.warn/.hot`）：0 正常（accent）/ 1 ≥70% 橙 / 2 ≥90% 红。
///
/// 阈值写在这里（而不是 .slint）是为了让它可被单测钉住（颜色本身无法断言，档位可以）。
pub fn usage_level(pct: f32) -> i32 {
    if pct >= 90.0 {
        2
    } else if pct >= 70.0 {
        1
    } else {
        0
    }
}

/// 状态栏右侧的上下文用量文案（稿 v11：`上下文 12.4K/128K · 9.7%`）。
///
/// 两个 `None` 是**两种不同的缺**，分别显示成对应的 `—`：
/// - `used = None`：还没有一轮上报过用量（新建会话 / 改动前的老会话 / 不上报的执行层）；
/// - `limit = None`（含 `Some(0)`）：不知道这个模型的窗口多大。
///
/// **任何一格缺失都不许填数**：`0` 是「占用为零」的意思，不是「不知道」
/// （卡 `abb-syschat-e3-tokens-20261013` 验收 4：表里没有的模型要给可读回落，不能显示 0/负值）。
pub fn context_usage_text(used: Option<u64>, limit: Option<u64>) -> String {
    // 0 上限无法算百分比（`usage_percent` 会返回 0，但那会让「上限未知」看起来像「占用 0%」）
    // ⇒ 归一到「不知道」。
    let limit = limit.filter(|l| *l > 0);
    let used_str = used.map(token_short).unwrap_or_else(|| "—".to_string());
    let limit_str = limit.map(token_short).unwrap_or_else(|| "—".to_string());
    let pct = match (used, limit) {
        (Some(u), Some(l)) => format!(" · {:.1}%", usage_percent(u, l)),
        _ => " · —".to_string(),
    };
    format!("上下文 {used_str}/{limit_str}{pct}")
}

/// 状态栏用量条的**悬停提示**（稿：悬停/点击看精确值；卡验收 3）。
///
/// 除了精确 token 数，这里还必须说清**口径**（卡验收 2）：已用是「最近一轮请求的
/// prompt tokens」（= 这一轮 provider 看到的整个上下文），不是「所有往返累加」，
/// 也不是估算。缺哪一项就说缺哪一项——不留一句含糊的「数据来源待定」。
pub fn context_usage_tip(used: Option<u64>, limit: Option<u64>) -> String {
    let limit = limit.filter(|l| *l > 0);
    let how = "已用 = 本会话最近一轮请求的上下文（prompt tokens），由执行层从模型响应上报";
    match (used, limit) {
        (Some(u), Some(l)) => format!(
            "本会话上下文：{u} / {l} tokens（{:.1}%）。{how}——真实计数，不是估算。",
            usage_percent(u, l)
        ),
        (None, Some(l)) => format!(
            "本会话上下文：{l} tokens 上限；还没有收到过用量上报（执行层未上报，或本会话还没跑过回合）。{how}。"
        ),
        (Some(u), None) => format!(
            "本会话上下文：已用 {u} tokens；这个模型的上限未知（内置表里没有它，可在「设置 → 供应商」填 context_window 补上）。"
        ),
        (None, None) => "本会话上下文：用量与上限都未知（执行层未上报用量；内置表里也没有这个模型，可在「设置 → 供应商」填 context_window）。"
            .to_string(),
    }
}

/// 本会话上下文用量的**唯一数据源接缝**。
///
/// E3（卡 `abb-syschat-e3-tokens-20261013`）起这里是**真实值**，不是估算：
/// - 已用 = 本会话最近一轮请求的 prompt tokens（[`SessionMeta::context_tokens`]，由
///   [`turn_core`] 在执行层上报 `_meta.usage` 时写入）；
/// - 上限 = 该会话所选供应商的模型窗口（[`context_window`]：设置可覆盖，否则内置表）。
///
/// 两项**都可能缺**，而且缺与 0 不同义：`used = None` = 还没有一轮报过（新建会话 /
/// 改动前的老会话 / 不上报的执行层）；`limit = None` = 不知道这个模型多大。
/// `session_list` 同为纯读入口，锁按尽力而为处理。
pub fn context_usage() -> (Option<u64>, Option<u64>) {
    let store = SessionStore::production();
    // **纯读：不存在就什么都不建**（目录、锁文件都不建）。GUI 每拍刷新都会走到这里，
    // 而「看一眼用量」在用户机器上不该有副作用；测试隔离门禁也是把它当硬约束的
    // （改动前这条接缝是常量 `(None, None)`，所以从没碰过盘——E3 落地时踩到：
    // 隔离 HOME 里凭空多出 `syschat/history.lock`，门禁直接红）。
    if !store.state_path().exists() {
        return (None, None);
    }
    let _g = store.lock().ok();
    let state = store.load();
    let current = state.sessions.iter().find(|s| s.id == state.current);
    let used = current.map(|s| s.context_tokens).filter(|t| *t > 0);
    let provider = current.map(|s| s.provider.as_str()).unwrap_or_default();
    (used, context_window(provider))
}

/// 本会话所选供应商的上下文窗口（token）。`provider = ""` = 跟随全局默认
/// （与 `agent_config` / [`crate::config::Config::resolve_provider`] 同口径）。
///
/// **不知道就返回 `None`**：供应商没配 / 找不到、或模型名匹配不到内置表——UI 显示 `—`，
/// 不许拿一个编造的数冒充上限。
fn context_window(provider: &str) -> Option<u64> {
    context_window_in(&Config::load().unwrap_or_default(), provider)
}

/// [`context_window`] 的可测内层：配置**注入**进来（不然单测只能去读真实 `config.json`，
/// 而隔离门禁下那是临时 HOME —— 会变成「测了个寂寞」）。
fn context_window_in(cfg: &Config, provider: &str) -> Option<u64> {
    let name = if provider.is_empty() {
        cfg.default_provider.as_str()
    } else {
        provider
    };
    let p = cfg.providers.iter().find(|p| p.name == name)?;
    if p.context_window > 0 {
        return Some(p.context_window);
    }
    builtin_context_window(&p.model)
}

/// 内置「常见模型 → 上下文窗口」兜底表（token）。**匹配不到返回 `None`，不猜。**
///
/// 为什么需要这张表：`providers[].model` 只是一个名字，ACP 协议里**没有**上下文长度
/// 字段（卡 `abb-syschat-e3-tokens-20261013` 的实核结论），而「已用/上限」缺了上限
/// 就只剩半句话。
///
/// 匹配口径：**子串**、取**首个命中**，所以表里**长的键必须排在短的前面**
/// （`gpt-4-turbo` 在 `gpt-4` 之前）。模型名带日期/版本后缀是常态
/// （`claude-3-5-sonnet-20241022`、`deepseek-chat`），精确相等会大面积落空。
///
/// 表里是各家**公开文档的默认窗口**，**不把 beta 长上下文档算进来**（Claude 的 1M、
/// Gemini 的更大档都要显式申请才生效）。数值随上游版本会变 ⇒ 这是**兜底**不是权威：
/// 用户可在「设置 → 供应商」里用 `context_window` 覆盖（`0` = 没设，走本表）。
fn builtin_context_window(model: &str) -> Option<u64> {
    let m = model.to_ascii_lowercase();
    const TABLE: &[(&str, u64)] = &[
        // OpenAI（长键在前：gpt-4-turbo 必须先于 gpt-4o 之外的其他 gpt-4 命中）
        ("gpt-4-turbo", 128_000),
        ("gpt-4o", 128_000),
        ("gpt-4.1", 1_047_576),
        ("gpt-4", 8_192),
        // Anthropic（Claude 3 起默认 200K）
        ("claude-2", 100_000),
        ("claude-3", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-opus-4", 200_000),
        ("claude-haiku-4", 200_000),
        ("claude", 200_000),
        // DeepSeek（V3/R1 公开窗口 64K）
        ("deepseek", 64_000),
        // Google
        ("gemini", 1_048_576),
        // 推理系列（短键，放最后；`o1` 这种两字符键只在这之后才可能误命中）
        ("o3", 200_000),
        ("o4", 200_000),
        ("o1", 200_000),
    ];
    TABLE.iter().find(|(k, _)| m.contains(k)).map(|(_, v)| *v)
}

/// 面板（自绘 `.menu`）的**锚点**（纯函数，单测钉住）：
/// - 横向：面板**右缘与按钮右缘对齐**（同 `SessionIconButton` 之前的 `x: root.width - w`），
///   靠窗边时整体内移（先压上界再压下界，同 [`clamp_bubble_pos`] 的取向）；
/// - 纵向：面板顶边在按钮**下缘下方** `gap`（稿 `.menu{top:30px}`：按钮高 26 + 4px；
///   本仓取 6px，与 `abb-v11-popup-anchor` 的 6–8px 口径一致）。
///
/// 注：标题栏按钮在**窗口顶部**，面板只能向下展开（向上会跑出窗）；
/// 「贴按钮上方」那条口径只适用于输入框上方的 `.pop`（另一张卡）。
pub fn menu_anchor(
    btn_x: i32,
    btn_w: i32,
    btn_bottom: i32,
    panel_w: i32,
    win_w: i32,
) -> (i32, i32) {
    let max_x = win_w - panel_w - 6;
    let mut x = btn_x + btn_w - panel_w;
    if x > max_x {
        x = max_x;
    }
    if x < 6 {
        x = 6;
    }
    (x, btn_bottom + 6)
}

// ─────────── v11：输入区上方的功能面板（`.pop`）锚点（卡 `abb-v11-popup-anchor`）───────────
//
// 稿 v11 第 1 条：「`表情` / `更多` 的面板必须**紧贴对应按钮上方**弹出（此前飘太远）」。
// 旧实现把面板锚在**输入框（`composer`）的顶边**上（`y: composer.absolute-position.y - h - 6`），
// 而按钮在输入框**底部**的功能区里 ⇒ 面板底边离按钮还有「待发附件行 + 文本区」那么高
// （默认布局下 100+ px，这就是 owner 说的「飘太远」）。现在锚到**按钮自己**的矩形上。
//
// 算术留在 Rust（这里）、画在 `.slint`：按钮矩形由 Slint 报给 Rust（`crow` 整体右对齐，功能区
// 左缘 = 输入框右缘 − 发送钮宽，而发送钮宽随文案/字体变，Rust 侧复算不出来），面板的
// `x/y/w/h` 全由 Rust 写回（`emoji-pop-*` / `more-pop-*`）。

/// `.pop` 底边与按钮顶边之间的间距（卡里写 6–8px；取 6 —— 与标题栏 [`menu_anchor`] 同一个数）。
pub const POP_GAP: i32 = 6;
/// `.pop` 与窗口四周的最小留白（靠窗边整体内移时用）。
pub const POP_MARGIN: i32 = 6;
/// 表情面板宽高（原 `ui/app.slint` 里的 300×220 字面量 ⇒ 搬来与锚点算术同源）。
pub const EMOJI_POP_W: i32 = 300;
pub const EMOJI_POP_H: i32 = 220;
/// 「更多（定时）」面板宽（原 `.slint` 里的 250px 字面量）。
pub const MORE_POP_W: i32 = 250;

/// 「更多」面板的高度：**逐字搬自** `ui/app.slint` 原来的表达式
/// `122 + N×26 + (N>0 ? 20 : 0) + (N>1 ? 26 : 0)`（padding 6×2 + 头 20 + 三档 3×26 + 档间距 + 待发行）。
///
/// 搬进 Rust 的理由：`y = 按钮顶 − 6 − 面板高`，面板高必须与锚点**同源**；也让「待发条目多 ⇒
/// 面板变高 ⇒ 仍不出窗」能被单测钉住。搬的是原表达式（含原式对条目间距的**富余**，见
/// `more_pop_h_is_the_slint_expression` 的注释），不改观感。
pub fn more_pop_h(sched_count: i32) -> i32 {
    let n = sched_count.max(0);
    122 + n * 26 + if n > 0 { 20 } else { 0 } + if n > 1 { 26 } else { 0 }
}

/// 面板落位（窗口坐标，逻辑像素；纯函数）。
///
/// 输入：`btn = (x, y, w, h)` = **对应按钮**在窗口坐标下的矩形；`panel = (w, h)`；`win = (w, h)`。
/// 口径（单测逐条钉住）：
/// 1. 横向 `x = btn.x + btn.w - panel.w`：**面板右缘与按钮右缘对齐**（卡里的验收条）；
/// 2. 纵向 `y = btn.y - POP_GAP - panel.h`：**底边贴在按钮顶边上方 6px**（稿「贴着按钮上方」）；
/// 3. 靠窗边**整体内移**（`POP_MARGIN` 留白，先压上界再压下界——同 [`clamp_bubble_pos`] /
///    [`menu_anchor`] 的取向）。窗口比面板还小时最终落在左上角留白处：**任何输入都不出窗**
///    （「不出窗」优先于「贴按钮」）。
pub fn pop_anchor(btn: (i32, i32, i32, i32), panel: (i32, i32), win: (i32, i32)) -> (i32, i32) {
    let (bx, by, bw, _bh) = btn;
    let (pw, ph) = panel;
    let (ww, wh) = win;
    let mut x = bx + bw - pw;
    let max_x = ww - pw - POP_MARGIN;
    if x > max_x {
        x = max_x;
    }
    if x < POP_MARGIN {
        x = POP_MARGIN;
    }
    let mut y = by - POP_GAP - ph;
    let max_y = wh - ph - POP_MARGIN;
    if y > max_y {
        y = max_y;
    }
    if y < POP_MARGIN {
        y = POP_MARGIN;
    }
    (x, y)
}

// ─────────── v11：内容区 ↔ 输入区的分割条（卡 `abb-v11-resizable-split`）───────────
//
// 稿 v11.1 的 `.split`：夹在 `.convo`（消息区）与 `.composer-zone`（输入区）**之间**的一条 10px
// 把手 —— **不是窗口底边的系统缩放**、**不是输入框内部**（v11 初版把位置画错，v11.1 已修正；
// 本卡按修正后的位置实现）。拖动改两区高度比例，比例持久化。
//
// 「比例」的分母是**可分配带**：窗高减去标题栏/状态栏等固定构件与分割条本身。这个值不在这里
// 镜像布局常量，而是由 `ui/app.slint` 用三块（消息区/把手/输入区）的**真实几何**合成后回报
// （`split-band-changed`）——布局改了自动跟上。

/// 分割条（`.split`）高：设计稿 `docs/design/syschat-dialog.html`（v11.1）的 `.split{height:10px}`。
///
/// 注：卡正文按稿写成「8px 拖拉把手」，稿里其实是 10px；以稿为准（`README` 的版本口径：
/// 「最新口径一律以 `syschat-dialog.html` 为准」）。与 `.slint` 的一致性由
/// [`tests::split_handle_h_matches_design`] 直接读稿子钉住。
pub const SPLIT_HANDLE_H: f32 = 10.0;
/// 内容区最小高（再拖也不许把消息区压没）。
pub const SPLIT_CONVO_MIN_H: f32 = 96.0;
/// 输入区最小高。
///
/// 口径：**输入区在无待发附件时的自然高**（分隔线 2 + 上下 padding 12/10 + 输入框 66 +
/// 间距 8 + 功能区 30 = 146）——即「分割条紧贴输入区上沿」这个稿里的默认观感。
/// 无头渲染台直接量这个值（`split_default_puts_divider_on_composer_top`），漂移时红。
pub const SPLIT_COMPOSER_MIN_H: f32 = 146.0;
/// 比例默认值（没有 `split.json` / 文件损坏时）。
///
/// **1.0 是个好用的哨兵**：它被 [`split_clamp_convo_h`] 的上限夹成 `(band − 10 − 146)/band`，
/// 而那个上限的含义正好是「输入区取最小高」—— 也就是稿里的默认观感，且在**任何窗高下**都成立
/// （不必把「默认窗高」这个数字搬进比例）。用户拖过之后落盘的就是真实比例（< 1）。
pub const SPLIT_RATIO_DEFAULT: f32 = 1.0;

/// 把「想要的内容区高」夹进上下限（纯函数）。
///
/// `composer_min_h` = 输入区下限（无待发附件时就是 [`SPLIT_COMPOSER_MIN_H`]；有待发附件行时
/// 再加上那一行的高——由 [`split_composer_min_h`] 算）。**它同时是 `.slint` 的 `min-height`
/// 与这里的上限口径**：两边同一个数，拖动才不会出现「拖到某处布局又把它挤回去」的错位。
///
/// - `band_h <= 0`（窗口还没 realize / 退化输入）⇒ 0：由 `.slint` 的默认值兜底，不写负高；
/// - 上界 `= 可分配带 − 分割条 − 输入区下限`；上界低于下界时**保内容区最小高**（宁可输入区
///   被挤到它的 `min-height`，也不把消息区压成 0 —— 消息区没高度时一行正文都看不见）；
/// - `NaN` ⇒ 下界（脏值不得进布局）；±inf 交给 `clamp` 夹到各自边界。
pub fn split_clamp_convo_h(band_h: f32, want: f32, composer_min_h: f32) -> f32 {
    let usable = band_h - SPLIT_HANDLE_H;
    if usable <= 0.0 {
        return 0.0;
    }
    let lo = SPLIT_CONVO_MIN_H.min(usable);
    let hi = (usable - composer_min_h).max(lo);
    if want.is_nan() {
        return lo;
    }
    want.clamp(lo, hi)
}

/// 输入区下限：无待发附件时的自然高 + 有待发附件时那一行（缩略图/文件 chip 行 68 + 间距 8）。
///
/// 两个消费者必须同口径：`.slint` 的 `composer-min-h`（布局硬约束）与分割条比例的上限
/// （[`split_clamp_convo_h`]）——否则「拖到最上」时布局会把消息区又挤回去。
pub fn split_composer_min_h(attach_count: i32) -> f32 {
    SPLIT_COMPOSER_MIN_H + if attach_count > 0 { 76.0 } else { 0.0 }
}

/// 文本区（`.area`）的**自然下限**高：稿 `.area{min-height:66px}`。
pub const SPLIT_AREA_MIN_H: f32 = 66.0;

/// 文本区高：自然下限 + 「输入区拿到的富余高度」。
///
/// - `composer_h` = 输入区高（[`split_composer_h`] 算出的）；
/// - `composer_natural_h` = 输入区的自然高（= [`split_composer_min_h`]：含待发附件行）。
///
/// 富余（`composer_h − 自然高`）**全部**给文本区：拖动分割条把输入区拉高时，用户看到的是
/// **输入框/文本区变高**，而不是「分隔线移了、中间多一块空白」。两区高与文本区高都由
/// Rust 写进 `.slint`（实测容器上的 `vertical-stretch` 会被忽略、多出来的空间被丢掉）。
pub fn split_area_h(composer_h: f32, composer_natural_h: f32) -> f32 {
    if !composer_h.is_finite() || !composer_natural_h.is_finite() {
        return SPLIT_AREA_MIN_H;
    }
    SPLIT_AREA_MIN_H + (composer_h - composer_natural_h).max(0.0)
}

/// 比例 → 内容区高（比例先夹进 `[0, 1]`，再按上下限夹取）。
pub fn split_convo_h(band_h: f32, ratio: f32, composer_min_h: f32) -> f32 {
    split_clamp_convo_h(band_h, band_h * ratio.clamp(0.0, 1.0), composer_min_h)
}

/// 内容区高 → **输入区高**（两者拼满可分配带：`输入区 = 带 − 分割条 − 内容区`）。
///
/// 显式给出两区高（而不是让布局去分剩余空间）有两个好处：① 布局里没有「剩余给谁」的歧义
/// （不依赖 stretch 在容器上的语义）；② `.slint` 报给 Rust 的「带」不必从这两个值反推 ——
/// 否则就是自指（换窗高时分母永远分不到新高度）。退化/脏输入 ⇒ 0（不写负高）。
pub fn split_composer_h(band_h: f32, convo_h: f32) -> f32 {
    if !band_h.is_finite() || !convo_h.is_finite() {
        return 0.0;
    }
    (band_h - SPLIT_HANDLE_H - convo_h).max(0.0)
}

/// 内容区高 → 比例（落盘口径）。`band_h <= 0` / 非有限输入 ⇒ [`SPLIT_RATIO_DEFAULT`]
/// （绝不把 `NaN`/`inf` 写进 `split.json`）。
pub fn split_ratio(band_h: f32, convo_h: f32) -> f32 {
    // **不能写成 `band_h <= 0.0`**：`!(x > 0.0)` 对 NaN 为 true（NaN 与任何数比较都 false），
    // 而 `NaN <= 0.0` 为 false ⇒ 直接换会让 NaN 漏进后面的计算，写出非法 `split.json`。
    // 这里显式列出 NaN 分支，把「非正 / NaN / convo 非有限」三种情况保持成与改造前逐字等价；
    // `+inf` 仍按原语义**放行**（原式 `!(inf > 0.0)` 为 false）——别顺手把它也拦掉。
    if band_h.is_nan() || band_h <= 0.0 || !convo_h.is_finite() {
        return SPLIT_RATIO_DEFAULT;
    }
    (convo_h / band_h).clamp(0.0, 1.0)
}

/// 分割条拖动/分配状态（纯数据 + 纯算术：不碰 IO、不碰 Slint ⇒ 可单测）。
///
/// 拖动用「**指针相对消息区顶边的 y**」直接定 divider 位置（而不是「按下时的基线 + 逐帧增量」）：
/// 消息区顶边在拖动期间**不动**（它在标题栏/状态行之下，固定），所以这个坐标本身就是「divider
/// 该在哪」，既不会累加取整误差，也不需要在按下时采样当前高度 —— 窗口/输入区尺寸中途变化
/// （待发附件行出现）也不会让它跳。
#[derive(Debug, Clone, Copy)]
pub struct SplitDrag {
    ratio: f32,
    band_h: f32,
    composer_min_h: f32,
}

impl SplitDrag {
    /// 用落盘比例建状态（脏值/缺失 ⇒ 默认比例）。
    pub fn new(ratio: f32) -> Self {
        Self {
            ratio: if ratio.is_finite() {
                ratio.clamp(0.0, 1.0)
            } else {
                SPLIT_RATIO_DEFAULT
            },
            band_h: 0.0,
            composer_min_h: SPLIT_COMPOSER_MIN_H,
        }
    }

    /// 当前比例（落盘用）。
    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    /// 布局报来「可分配带」与「输入区下限」（窗高/DPI/待发附件变化）⇒ 按比例算两区高。
    pub fn on_band(&mut self, band_h: f32, composer_min_h: f32) -> (f32, f32) {
        self.band_h = band_h;
        self.composer_min_h = composer_min_h;
        self.current()
    }

    /// 拖到「指针相对消息区顶边的 y」：返回新的内容区高，并把比例更新为**夹取后**的值
    /// （夹取过的比例会落盘 ⇒ 下次按窗口重新分配时与屏幕上看到的一致）。
    ///
    /// 返回 `(内容区高, 输入区高)`：两区高一起给，调用方一次写进窗口（见 [`split_composer_h`]）。
    /// 脏输入（NaN/inf）不得传播；带还没报来（未布局）时不动。
    pub fn drag_to(&mut self, pointer_rel: f32) -> (f32, f32) {
        if !pointer_rel.is_finite() || self.band_h <= 0.0 {
            return self.current();
        }
        let h = split_clamp_convo_h(self.band_h, pointer_rel, self.composer_min_h);
        self.ratio = split_ratio(self.band_h, h);
        (h, split_composer_h(self.band_h, h))
    }

    /// 当前比例下的两区高 `(内容区, 输入区)`（不改任何状态）。
    pub fn current(&self) -> (f32, f32) {
        let h = split_convo_h(self.band_h, self.ratio, self.composer_min_h);
        (h, split_composer_h(self.band_h, h))
    }
}

// ───────────── v11c（卡 `abb-v11-resize-maximize-macos`）：自管缩放与最大化 ─────────────
//
// 为什么必须有这一套：`no-frame: true` 的窗在 macOS 上**系统不给边角缩放手柄**，而 winit 的
// `drag_resize_window` 在 macOS 直接返回 `Err(NotSupported)`（winit 0.30.13
// `platform_impl/macos/window_delegate.rs:1179`）⇒ Windows/Linux 那套「自绘热区 + 系统缩放循环」
// 在 macOS 上是死的。本模块只放**纯算术**（方向 → 新矩形、最小/上限夹取、最大化↔还原记忆），
// 取指针/写窗口在 `ui.rs` 的 `mac_resize` 里。

/// 8 向缩放方向（`north-east` 这类角 = 两个分量同时为真）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResizeDir {
    pub north: bool,
    pub south: bool,
    pub east: bool,
    pub west: bool,
}

impl ResizeDir {
    /// 解析 `.slint` 热区报来的方向串（与 `slint_pixel` 的 `PixelWindowResize` 同字面量）。
    ///
    /// 用**整串字面量表**而不是按 `-` 拆词：拆词会接受 `north-north` 这类重复分量（它们不是热区
    /// 会发出的形态，接了就等于把上游的拼写错误静默当成一个合法方向）；表里没有的串一律拒。
    pub fn parse(s: &str) -> Option<Self> {
        let (north, south, east, west) = match s {
            "north" => (true, false, false, false),
            "south" => (false, true, false, false),
            "east" => (false, false, true, false),
            "west" => (false, false, false, true),
            "north-east" => (true, false, true, false),
            "north-west" => (true, false, false, true),
            "south-east" => (false, true, true, false),
            "south-west" => (false, true, false, true),
            _ => return None,
        };
        Some(Self {
            north,
            south,
            east,
            west,
        })
    }
}

/// 逻辑像素矩形（窗口外框）。缩放/最大化的全部算术都用它（`ui.rs` 里负责物理↔逻辑换算）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WinRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl WinRect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// 夹取上下限：脏值（NaN）一律不参与；`min > max` 时下限赢（宁可大一点也不把窗口算成 0）。
///
/// 单独抽出来的理由：`f32::clamp` 在 `min > max` / 边界为 NaN 时**会 panic**，而这两个值一个来自
/// 常量（`SYSCHAT_MIN_*`）、一个来自显示器枚举，不能假定它们总是良定义的。
fn clamp_len(v: f32, min: f32, max: f32) -> f32 {
    let lo = if min.is_finite() { min.max(0.0) } else { 0.0 };
    let hi = if max.is_finite() { max } else { f32::INFINITY };
    if v.is_nan() {
        return lo;
    }
    v.max(lo).min(hi.max(lo))
}

/// 一次自管缩放的**不可变锚点**（按下那一刻的指针与窗口矩形）+ 方向。
///
/// 关键：全部用**屏幕坐标**（不是窗口坐标）。窗口自身会被拖着重定位，用窗口坐标会自我引用
/// （`x_new` 又决定下一拍的 `dx`）→ 实测会来回振荡；屏幕坐标下锚点是常量，天然收敛。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResizeSession {
    pub dir: ResizeDir,
    /// 按下时的指针（屏幕坐标，逻辑像素）。
    pub anchor_pointer: (f32, f32),
    /// 按下时的窗口矩形（逻辑像素）。
    pub anchor: WinRect,
}

impl ResizeSession {
    pub fn new(dir: ResizeDir, pointer: (f32, f32), rect: WinRect) -> Self {
        Self {
            dir,
            anchor_pointer: pointer,
            anchor: rect,
        }
    }

    /// 当前指针（屏幕坐标）→ 新的窗口矩形。
    ///
    /// 口径：
    /// - 东/南边不动左上角；西/北边**左上角跟着走**（`x = anchor.right − w`），
    ///   即被拖的那条边始终在指针处；
    /// - 尺寸先夹进 `[min, max]`（`min` = `.slint` 的 340×380；`max` = 显示器整幅大小）；
    ///   夹取发生在**算左上角之前**，所以夹住之后拖动边仍然贴指针、不会和指针脱开；
    /// - 脏输入（NaN/inf）⇒ 原矩形不动（宁可不缩放，也不把窗口写成 0/NaN）。
    pub fn update(&self, pointer: (f32, f32), min: (f32, f32), max: (f32, f32)) -> WinRect {
        let a = self.anchor;
        if !pointer.0.is_finite()
            || !pointer.1.is_finite()
            || !self.anchor_pointer.0.is_finite()
            || !self.anchor_pointer.1.is_finite()
            || !a.x.is_finite()
            || !a.y.is_finite()
            || !a.w.is_finite()
            || !a.h.is_finite()
        {
            return a;
        }
        let dx = pointer.0 - self.anchor_pointer.0;
        let dy = pointer.1 - self.anchor_pointer.1;
        let want_w = if self.dir.east {
            a.w + dx
        } else if self.dir.west {
            a.w - dx
        } else {
            a.w
        };
        let want_h = if self.dir.south {
            a.h + dy
        } else if self.dir.north {
            a.h - dy
        } else {
            a.h
        };
        let w = clamp_len(want_w, min.0, max.0);
        let h = clamp_len(want_h, min.1, max.1);
        WinRect {
            x: if self.dir.west { a.x + a.w - w } else { a.x },
            y: if self.dir.north { a.y + a.h - h } else { a.y },
            w,
            h,
        }
    }
}

/// 逻辑像素下「两个矩形算同一个」的容差。
///
/// 1px 的来源：`set_size`/`set_position` 走 f32 → 物理整数点 → 再读回来的往返（Retina 2x 下
/// 逻辑半像素会被量化），不能要求逐位相等。
///
/// `cfg(any(test, macos))`（同 `ui.rs::round_rect_bands` 的手法）：唯一的生产调用方是 macOS 专有的
/// `ui::toggle_syschat_maximize`（其它平台 `⤢` 仍交系统 `set_maximized`）——不加这个 cfg 的话
/// Windows/Linux 构建会把它当死代码报 `-D warnings`（实核：本机把调用点去掉就报
/// “function `rect_approx_eq` is never used”），而留着 `allow(dead_code)` 又会将来掩盖真死代码。
#[cfg(any(test, target_os = "macos"))]
fn rect_approx_eq(a: WinRect, b: WinRect) -> bool {
    (a.x - b.x).abs() <= 1.0
        && (a.y - b.y).abs() <= 1.0
        && (a.w - b.w).abs() <= 1.0
        && (a.h - b.h).abs() <= 1.0
}

/// 「⤢」= 铺满当前显示器工作区，再点一次还原到**原尺寸与位置**。
///
/// 为什么不直接用系统的 `set_maximized`：macOS 无边框窗上与尺寸绑定/`no-frame` 冲突（D 批已
/// 记「视觉越点越小」），且「最大化」在无边框窗上没有系统语义可依赖。这里就是一个
/// 「记住 → 铺满 → 还原」的状态机，纯数据、可单测。
///
/// `cfg(any(test, macos))` 的理由同 [`rect_approx_eq`]。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[cfg(any(test, target_os = "macos"))]
pub struct MaximizeState {
    /// 有待还原的矩形 ⇔ 至少点过一次最大化。
    saved: Option<WinRect>,
}

#[cfg(any(test, target_os = "macos"))]
impl MaximizeState {
    /// 点一次 ⤢：
    /// - 当前**正**铺在工作区上（且有待还原记忆）⇒ 还原到记住的矩形；
    /// - 否则 ⇒ 记住**当前**矩形并返回工作区。
    ///
    /// 「当前是否已最大化」用「矩形 ≈ 工作区」判，而不是另设一个 bool：用户在最大化状态下拖动了
    /// 窗口（自管缩放或系统移动）后，矩形就不再等于工作区 ⇒ 再点 ⤢ 是**重新最大化**（与 macOS
    /// 绿点行为一致），不会把用户拖出来的那一份当成新的还原目标……注意这里刻意仍用 `current`
    /// 覆盖记忆：macOS 同样是「最大化 → 移动 → 再最大化」以新位置为下一次还原点。
    pub fn toggle(&mut self, current: WinRect, work: WinRect) -> WinRect {
        if self.is_maximized(current, work) {
            self.saved.take().unwrap_or(work)
        } else {
            self.saved = Some(current);
            work
        }
    }

    /// 当前是否处于「已最大化」态（有待还原的记忆 + 窗口矩形≈工作区）。
    fn is_maximized(&self, current: WinRect, work: WinRect) -> bool {
        self.saved.is_some() && rect_approx_eq(current, work)
    }
}

// ─────────────────── D 批：对话框锚定气泡的几何（纯函数）───────────────────

/// 气泡与对话框之间的间距（逻辑像素，稿 `.gap` = 10px）。
pub const BUBBLE_GAP: i32 = 10;

/// 计算对话框应处的左上角（逻辑像素）：把窗口锚在气泡旁，靠屏边自动翻边、并夹进屏幕。
///
/// 输入（全为逻辑像素）：
/// - `bubble` / `bubble_size`：**气泡可视区**左上角与宽高（不是气泡窗口——窗口可能比图标大）；
/// - `window_size`：对话框宽高；
/// - `work`：屏幕工作区 `(x, y, w, h)`。
///
/// 口径（钉在单测里）：
/// 1. **首选左侧**（稿子场景：气泡在右上，窗口翻到左），此时 `bubble.x - (win.x + win.w) == BUBBLE_GAP`；
/// 2. 左侧放不下（`win.x < work.x`）就**翻到右侧**，此时 `win.x - (bubble.x + bubble.w) == BUBBLE_GAP`。
///    取等号（窗口左缘恰在工作区左界）算「放得下」⇒ **不翻**；
/// 3. 两侧都放不下（窗口比可用空间还宽）或右侧仍越界 → 夹进工作区；此时牺牲 10px 间距，
///    **不出屏优先**；
/// 4. 垂直与气泡顶对齐后同样夹进工作区。
///
/// 纯函数：GUI 侧只负责取坐标/尺寸/工作区，翻转与夹取的口径全在这里，可独立单测。
pub fn anchor_window_pos(
    bubble: (i32, i32),
    bubble_size: (i32, i32),
    window_size: (i32, i32),
    work: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (bx, by) = bubble;
    let (bw, _bh) = bubble_size;
    let (ww, wh) = window_size;
    let (wx, wy, ww_area, wh_area) = work;

    // 首选左侧：气泡左缘 = 窗口右缘 + 间距。左侧放得下（含取等号）就不翻。
    let left_x = bx - BUBBLE_GAP - ww;
    let mut x = if left_x >= wx {
        left_x
    } else {
        bx + bw + BUBBLE_GAP // 翻到右侧
    };
    // 右侧也越界（或窗口比工作区还宽）→ 夹进工作区，保证不出屏。
    let max_x = wx + ww_area - ww;
    if x > max_x {
        x = max_x;
    }
    if x < wx {
        x = wx;
    }

    // 垂直：与气泡顶对齐（气泡在哪，窗口就从哪往下展开），再夹进工作区。
    let mut y = by;
    let max_y = wy + wh_area - wh;
    if y > max_y {
        y = max_y;
    }
    if y < wy {
        y = wy;
    }
    (x, y)
}

// ─────────── D4：气泡跟随对话窗四角（卡 `abb-syschat-d4-bubble-follow`，纯函数）───────────

/// 气泡要吸附的**窗口四角**。
///
/// 枚举顺序即「并列时的优先级」：[`nearest_corner`] 取距离最小者，距离相等时取本顺序里靠前的，
/// 这样即使输入完全对称（气泡正中、窗口比屏还大），「四角择优」也是确定性的而非随实现漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BubbleCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl BubbleCorner {
    /// 固定顺序（并列优先级 + 单测遍历用）。
    pub const ALL: [BubbleCorner; 4] = [
        BubbleCorner::TopLeft,
        BubbleCorner::TopRight,
        BubbleCorner::BottomLeft,
        BubbleCorner::BottomRight,
    ];
}

/// 某个角的**气泡目标位**（气泡**可视区**左上角，逻辑像素；全部输入输出同 [`anchor_window_pos`]）。
///
/// 「角外侧 10px」的口径（与 [`anchor_window_pos`] 的间距口径一致 —— 那里就是**横向** 10px 间隔）：
/// - 横向：左角把气泡放到窗口左缘外 [`BUBBLE_GAP`]（`window.x - BUBBLE_GAP - bw`），
///   右角放到窗口右缘外 [`BUBBLE_GAP`]（`window.right + BUBBLE_GAP`）；
/// - 纵向：**与角所在的那条边对齐** —— 上角对齐窗口上缘（`window.y`），下角对齐窗口下缘
///   （`window.bottom - bh`）。
///
/// 为什么纵向是「对齐边」而不是「再往外 10px」：开窗时 [`anchor_window_pos`] 正是把窗口
/// **顶边对齐气泡顶边**、横向留 10px（见其口径 1/2）。若这里改成对角各外推 10px，开窗那一刻
/// 气泡会被重算到「窗口上缘再往上 `bh + 10`」，于是**每次开窗都向上飘一个气泡高**，反复开关
/// 必然飘出屏。现在的定义让 `anchor_window_pos` 的落点成为本函数的不动点（单测
/// `snap_is_fixed_point_of_open_anchor`），开窗不跳、反复开关不漂移。
pub fn corner_bubble_pos(
    corner: BubbleCorner,
    window: (i32, i32, i32, i32),
    bubble_size: (i32, i32),
) -> (i32, i32) {
    let (wx, wy, ww, wh) = window;
    let (bw, bh) = bubble_size;
    let x = match corner {
        BubbleCorner::TopLeft | BubbleCorner::BottomLeft => wx - BUBBLE_GAP - bw,
        BubbleCorner::TopRight | BubbleCorner::BottomRight => wx + ww + BUBBLE_GAP,
    };
    let y = match corner {
        BubbleCorner::TopLeft | BubbleCorner::TopRight => wy,
        BubbleCorner::BottomLeft | BubbleCorner::BottomRight => wy + wh - bh,
    };
    (x, y)
}

/// 四角择优：选**目标位**离气泡当前位置（可视区左上角）最近的角；距离相等时取
/// [`BubbleCorner::ALL`] 里靠前的那个（确定性）。
///
/// 比的是「目标位」而不是「角的顶点」：目标位才是气泡真要落下去的地方，而且比较前先**夹进工作区**
/// （[`clamp_bubble_pos`]）—— 这样「落到 P ⇒ P 到自己的距离为 0 ⇒ 再算仍是 P」恒成立，
/// 即 [`snap_bubble_to_window`] 的**幂等**。这是防「窗动 → 气泡动 → 再判 → 振荡/翻角」的关键性质
/// （单测 `snap_is_idempotent`）。若不先夹取，窗口贴屏幕边时第一次夹取会把气泡挪回窗内区块，
/// 第二次再判就可能翻到另一个角、位置跟着跳。
///
/// 距离用 `i64` 平方欧氏：屏幕逻辑坐标可以到 ±30000 量级，`i32` 平方会溢出成负 → 择优反了。
pub fn nearest_corner(
    window: (i32, i32, i32, i32),
    bubble: (i32, i32),
    bubble_size: (i32, i32),
    work: (i32, i32, i32, i32),
) -> BubbleCorner {
    let mut best = BubbleCorner::TopLeft;
    let mut best_d = i64::MAX;
    for c in BubbleCorner::ALL {
        let p = clamp_bubble_pos(corner_bubble_pos(c, window, bubble_size), bubble_size, work);
        let dx = i64::from(p.0) - i64::from(bubble.0);
        let dy = i64::from(p.1) - i64::from(bubble.1);
        let d = dx * dx + dy * dy;
        // 严格小于：并列时保留 `ALL` 里靠前的那个（确定性）。
        if d < best_d {
            best_d = d;
            best = c;
        }
    }
    best
}

/// 把气泡可视区左上角夹进工作区（**不出屏优先**：夹取时会牺牲 10px 间距，与 [`anchor_window_pos`]
/// 同取向）。夹取顺序与其逐字一致：先压上界再压下界；窗口比屏还大时 `max < lo`，
/// 最终落在 lo（贴住工作区左/上缘），任何输入都不会产出越界坐标。
pub fn clamp_bubble_pos(
    bubble: (i32, i32),
    bubble_size: (i32, i32),
    work: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (bw, bh) = bubble_size;
    let (wx, wy, ww, wh) = work;
    let mut x = bubble.0;
    let max_x = wx + ww - bw;
    if x > max_x {
        x = max_x;
    }
    if x < wx {
        x = wx;
    }
    let mut y = bubble.1;
    let max_y = wy + wh - bh;
    if y > max_y {
        y = max_y;
    }
    if y < wy {
        y = wy;
    }
    (x, y)
}

/// 把气泡吸附到「离它最近的窗口角」的目标位，并夹进工作区。返回 `(选中的角, 气泡可视区左上角)`。
///
/// **单一事实源（卡 `abb-syschat-d4-bubble-follow`）**：连续几何只此一条 —— 「窗口矩形 ⇒ 气泡位置」。
/// 反方向的 [`anchor_window_pos`]（气泡 ⇒ 窗口）只在**开窗那一刻**用一次，之后不再参与；
/// 绝不把本函数的结果再拿去反推窗口，否则就是 `A 动→B 动→A 动` 的死循环。
pub fn snap_bubble_to_window(
    window: (i32, i32, i32, i32),
    bubble: (i32, i32),
    bubble_size: (i32, i32),
    work: (i32, i32, i32, i32),
) -> (BubbleCorner, (i32, i32)) {
    let corner = nearest_corner(window, bubble, bubble_size, work);
    let pos = clamp_bubble_pos(
        corner_bubble_pos(corner, window, bubble_size),
        bubble_size,
        work,
    );
    (corner, pos)
}

/// 历史独占锁：只在「读历史/写一轮」的短临界区里持有，避免并发写坏历史。
/// 用 `try_lock`（非阻塞）：本锁的所有使用者都在同一进程（GUI 主线程的清空、
/// 后台回合的读写），阻塞拿锁会把 Slint 主线程挂住。持不到 = 已有历史读写在进行，
/// 直接返回可重试错误；不静默降级（与 deliver 的降级取向不同：对话宁可报错重试，
/// 也不并发写坏历史）。
struct HistoryLock {
    file: std::fs::File,
}

impl HistoryLock {
    /// `lock_path` 由调用方给（= store 的 base 下的 `history.lock`）：生产走 `~/.agent-bridge/syschat/`，
    /// 测试走临时 base ⇒ 单测不会把锁文件写进隔离 HOME（否则 `tools/check_test_isolation.sh` 会红）。
    fn acquire(lock_path: &std::path::Path) -> std::io::Result<Self> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(lock_path, std::fs::Permissions::from_mode(0o600));
        }
        use fs2::FileExt;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { file }),
            Err(e) => {
                let msg = if e.kind() == std::io::ErrorKind::WouldBlock {
                    "另一个系统对话操作正在进行".to_string()
                } else {
                    e.to_string()
                };
                Err(std::io::Error::new(e.kind(), msg))
            }
        }
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        // 显式限定到 fs2（std 1.89 也提供 File::unlock，混用会让“用了哪套”变得不清楚）。
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// 读**当前会话**的历史（GUI 用）。
pub fn load_history() -> Vec<Turn> {
    let id = current_session();
    SessionStore::production().read_history(&id)
}

/// 清空**当前会话**的历史（对话窗「清空」）。
pub fn clear_history() -> std::io::Result<()> {
    let id = current_session();
    clear_session(&id)
}

/// 清空指定会话的历史。
///
/// ⚠️ **索引必须跟着回到「空会话」形态**（不是只删文件）：`sessions.json` 里的 `title` 是
/// 「首条用户消息」，只删历史文件会在列表里留下一条「有旧标题、没内容」的**幽灵会话**
/// ——名字与内容不一致（`is_empty_session` 判空、列表却还写着旧标题）。
/// 2026-10-15 实测：`hi`/`😀` 两条归档会话就是这么来的（创建后有轮次 ⇒ 标题已落，后来
/// 在对话窗点了「清空」⇒ 文件没了、索引没动）。
pub fn clear_session(id: &str) -> std::io::Result<()> {
    let store = SessionStore::production();
    let _guard = store
        .lock()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    clear_session_locked(&store, id)
}

/// [`clear_session`] 的实现：**调用方必须已持历史锁**（单测用临时 base 直接驱动）。
fn clear_session_locked(store: &SessionStore, id: &str) -> std::io::Result<()> {
    match std::fs::remove_file(store.history_path(id)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // 索引复位：标题回「新会话」、上下文用量归零；`id` 与 `created_at` 不动（调用方/当前
    // 会话还持有这个 id，换 id 会把 current 弄丢）。存储里没这条就什么也不做。
    let mut state = store.load();
    if let Some(s) = state.sessions.iter_mut().find(|s| s.id == id) {
        s.title = "新会话".to_string();
        s.context_tokens = 0;
        store
            .save(&state)
            .map_err(|e| std::io::Error::other(format!("写入会话索引失败：{e}")))?;
    }
    Ok(())
}

/// 丢弃**从 `from` 起（含）到末尾**的全部历史轮，返回丢掉的条数。
///
/// 用途：`⟳ 重新生成` 的口径是「**原来的不要了、重新来**」——先把该轮（及其之后，因为后面
/// 那些是基于旧答复产生的）删掉，再重跑那条用户提示。**不删任何其它会话**。
///
/// 实现：历史是追加式 JSONL ⇒ 截断要**重写文件**（先写 `.tmp` 再 rename，与 `save` 同款原子性）。
/// `from >= len` 时不做任何事；`from == 0` 是**合法**的（重新生成第一轮时就是要从第 0 条
/// 开始丢——随后 `submit` 会把那条用户轮重新追加回来，不是「清空」语义）。
pub fn drop_turns_from(from: usize) -> Result<usize, String> {
    let store = SessionStore::production();
    let _g = store.lock()?;
    drop_turns_from_locked(&store, from)
}

/// [`drop_turns_from`] 的实现：**调用方必须已持历史锁**（单测用临时 base 直接驱动）。
fn drop_turns_from_locked(store: &SessionStore, from: usize) -> Result<usize, String> {
    let mut state = store.load();
    store.ensure_current(&mut state);
    let id = state.current.clone();
    let turns = store.read_history(&id);
    if from >= turns.len() {
        return Ok(0);
    }
    let dropped = turns.len() - from;
    let keep = &turns[..from];
    let path = store.history_path(&id);
    let mut buf = String::new();
    for t in keep {
        buf.push_str(&serde_json::to_string(t).unwrap_or_default());
        buf.push('\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, buf.as_bytes()).map_err(|e| format!("写入历史失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("落盘历史失败：{e}"))?;
    Ok(dropped)
}

/// 截断单条文本到 [`MAX_HISTORY_CHARS`] 字符（按 char，避免切断 UTF-8）。
fn clamp_text(text: &str) -> String {
    if text.chars().count() <= MAX_HISTORY_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_HISTORY_CHARS).collect();
    format!("{cut}…（超出 {MAX_HISTORY_CHARS} 字已截断）")
}

/// 取最近 [`HISTORY_KEEP_TURNS`] 轮（一问一答 = 1 轮）用于拼 prompt。
///
/// 直接按「最近 2×N 条」取即可：历史是严格交替的 user/assistant，
/// 但若上次回合失败只落了 user 轮，交替会被打破——按条数取能容忍这种奇偶，
/// 不会因为少一条 assistant 就把整段历史吞掉。
pub fn recent_history(history: &[Turn]) -> &[Turn] {
    let keep = HISTORY_KEEP_TURNS * 2;
    if history.len() <= keep {
        history
    } else {
        &history[history.len() - keep..]
    }
}

/// 拼本轮 prompt：前言 + 历史 + 当前消息。
///
/// 历史必须以**明确的对话格式**呈现，否则模型会把历史里自己的旧回复当作新指令。
/// 用户文本原样保留（不转义）：这是用户自己的机器、自己的对话。
pub fn build_prompt(history: &[Turn], user_text: &str) -> String {
    let mut s = String::from(SYSTEM_PREAMBLE);
    let recent = recent_history(history);
    if !recent.is_empty() {
        s.push_str("\n\n## 之前的对话（供参考，不要复述）\n");
        for t in recent {
            let who = if t.role == "assistant" {
                "助手"
            } else {
                "用户"
            };
            s.push_str(who);
            s.push('：');
            s.push_str(&clamp_text(&t.text));
            s.push('\n');
        }
    }
    s.push_str("\n## 当前消息\n用户：");
    s.push_str(user_text);
    s
}

/// 拼本轮 prompt，并在末尾附上待发附件的**本地路径**（agent 自行读取，不塞二进制）。
///
/// `atts` 为空时必须与 [`build_prompt`] **逐字节一致**（无附件路径不改变既有语义，
/// 既有测试与已有的对话格式都不动）。
pub fn prompt_with_attachments(history: &[Turn], user_text: &str, atts: &[Attachment]) -> String {
    let mut s = build_prompt(history, user_text);
    if atts.is_empty() {
        return s;
    }
    s.push_str("\n\n## 附件（本地路径，自行读取）\n");
    for a in atts {
        s.push_str("- ");
        s.push_str(&a.path.display().to_string());
        s.push('\n');
    }
    s
}

/// 一轮对话的结局（E1 流式）：「正常完成」携带最终文本；「用户停止」不再带文本
///（已生成部分已随 `ChunkKind` 分片流出，且由 [`turn_core`] 落进历史）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// 回合正常完成。`usage` = 执行层为**本轮**上报的真实 token 计数
    /// （`session/prompt` 响应的 `_meta.usage`）。
    ///
    /// `None` = 执行层没上报（旧适配器 / goose / 取消与失败），**不是 0**：读者必须
    /// 按「未知」处理，不许把缺数据印成 `0/128K`（卡 `abb-syschat-e3-tokens-20261013`）。
    Done {
        text: String,
        usage: Option<crate::runtime::acp::TokenUsage>,
    },
    Stopped,
}

/// 生成中「按发送」的落点（纯逻辑判定，见 [`SendPump::submit`] / [`route_send`]）。
///
/// UI 必须把三者区分开显示（卡 AC：「已插入」vs「已排队」）——尤其 `Steer` 与 `Queue`
/// 对用户是两种不同承诺：前者已进在途回合，后者还没发出去。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRoute {
    /// 当前没有在途回合 ⇒ 直接起一轮。
    Immediate,
    /// 有在途回合且后端支持 steer ⇒ 插入在途回合（不进本地队列）。
    Steer,
    /// 有在途回合但 steer 不支持/能力不明 ⇒ 入本地队列，等本轮结束后依次发。
    Queue,
}

/// 排队中的一轮用户输入（文本 + 该条附带的待发附件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTurn {
    pub text: String,
    pub atts: Vec<Attachment>,
}

/// 发送落点判定（纯函数，单测钉住）。
///
/// 口径（卡 `abb-syschat-d10-insert-interrupt`）：
/// - 无在途回合 ⇒ `Immediate`；
/// - 在途 + `Supported` ⇒ `Steer`；
/// - 在途 + `Unsupported`/`Unknown` ⇒ `Queue`（**能力不明不得猜成支持**——猜错
///   就会把消息发成一个必然失败的 steer，甚至被后端当 `{}` 成功而静默吞掉）。
pub fn route_send(in_flight: bool, support: SteerSupport) -> SendRoute {
    if !in_flight {
        SendRoute::Immediate
    } else if support == SteerSupport::Supported {
        SendRoute::Steer
    } else {
        SendRoute::Queue
    }
}

/// 系统对话回合的**串行泵**（纯逻辑，GUI 与单测共用）。
///
/// 承重不变量：
/// 1. **一次只跑一轮**（`in_flight`）——排队项绝不并行开跑；
/// 2. 在途 + 支持 steer ⇒ 插入（`Steer`，不入队）；否则在途 ⇒ 入队（`Queue`）；
/// 3. 空闲 ⇒ `Immediate`（本轮结束后由 [`finish`](Self::finish) 弹出队首，
///    交给调用方开下一轮）；
/// 4. **打断（⏹）不丢排队项**：`finish()` 是唯一的出队口，打断与自然完成走同一条
///    ——⏹ 只取消在途那一轮，已承诺「排队等本轮结束后发送」的消息照旧依次发。
///    （若要在打断时也清队列，应显式调 [`clear`](Self::clear)，不得让 `finish`
///    的行为随结局分支——那是「执行路径不同结果不同」的隐蔽分支。）
#[derive(Debug, Default)]
pub struct SendPump {
    in_flight: bool,
    support: SteerSupport,
    pending: VecDeque<QueuedTurn>,
}

impl SendPump {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录当前回合 agent 的 steer 能力（由能力 watch 驱动；不明 = 排队）。
    pub fn set_support(&mut self, support: SteerSupport) {
        self.support = support;
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 提交一条用户输入，返回落点。
    ///
    /// 空文本（trim 后）与「纯附件」拒绝 —— 与既有产品口径一致（附件不单独发），
    /// 同时把「输入框在生成中可用」后的防连点兜底：空串永远不会被当成一条消息。
    /// `Queue` 时**已入队**；`Steer`/`Immediate` 时不入队（由调用方直接发出去）。
    pub fn submit(&mut self, text: &str, atts: Vec<Attachment>) -> Option<SendRoute> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let route = route_send(self.in_flight, self.support);
        if route == SendRoute::Queue {
            self.pending.push_back(QueuedTurn {
                text: trimmed.to_string(),
                atts,
            });
        }
        Some(route)
    }

    /// `Steer` 落点失败（注入通道恰好已随上一轮收尾）时的回退：把消息补进队列，
    /// 避免「判定为插入但实际没送出去」的静默丢失。返回排队后的长度。
    pub fn enqueue_fallback(&mut self, text: &str, atts: Vec<Attachment>) -> usize {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            self.pending.push_back(QueuedTurn {
                text: trimmed.to_string(),
                atts,
            });
        }
        self.pending.len()
    }

    /// 标记一轮开始（空闲 ⇒ 在途）。幂等。
    pub fn begin(&mut self) {
        self.in_flight = true;
    }

    /// 一轮结束（自然完成 / 停止 / 失败）：置空闲并**弹出队首**作为下一轮。
    /// 返回 `None` = 队列空、无下一轮。串行化的唯一出处。
    pub fn finish(&mut self) -> Option<QueuedTurn> {
        self.in_flight = false;
        self.pending.pop_front()
    }

    /// 清空排队项（清空会话历史 / 切换会话 / 新建会话时调用）。
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// **强制置空闲**但**不动队列**（停止看门狗专用，owner 2026-10-16）。
    ///
    /// 为什么不能用 `finish()`：它会顺手弹出队首（那是「回合正常收尾、开下一轮」的语义），
    /// 而看门狗的场景是「回合那边**已经不会再有结局事件**」——此时既要把 `in_flight` 复位
    ///（否则之后的发送全被排进队列、永远轮不到），**又不能**丢掉用户已排队的话。
    pub fn abort(&mut self) {
        self.in_flight = false;
    }
}

/// 构造一条系统对话的入站消息（Prompt 文本 + owner 身份 + syschat 标签）。
///
/// 生成中插入的消息与起一轮的正常消息**同形**（harness 侧 `try_native_steer` 会按
/// `prompt_tag` 拼 steer 包装），不新造协议。
pub fn steer_inbound(text: &str) -> InboundMsg {
    InboundMsg {
        id_hex: uuid::Uuid::new_v4().to_string(),
        author_role: crate::config::SenderRole::Owner.as_str().to_string(),
        text: text.to_string(),
        ts_secs: crate::chrono_lite::unix_secs() as i64,
        prompt_tag: "syschat".to_string(),
        bridge_generation: String::new(),
    }
}

/// 流式回合里的一个连续分段：**同类分片并进同一段，换类时开新段**。
///
/// `started` / `last` 是调用方注入的单调秒数（此处不读时钟，便于单测钉住耗时）；
/// `last - started` 即该段耗时——思考块标题「3.2s」的来源。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSegment {
    pub kind: ChunkKind,
    pub text: String,
    pub started: f64,
    pub last: f64,
}

impl StreamSegment {
    /// 本段耗时（秒，非负）。
    pub fn elapsed(&self) -> f64 {
        (self.last - self.started).max(0.0)
    }
}

/// 流式分片累积器（E1 正文 / E2 思考共用同一条 sink）：按到达顺序累积分段，供
/// ① GUI 打字机 ② 「停止」时把已生成部分落历史 ③ 回合结束区分思考与正文 共用。
///
/// 设计取舍：**不做增量解析**，只维护分段文本；markdown 解析留给渲染层
///（`mdblocks::parse`）在每次渲染时做，这样累积器本身可无依赖单测。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamingBubble {
    segments: Vec<StreamSegment>,
    active: bool,
    stopped: bool,
}

impl StreamingBubble {
    /// 新建一个进行中的流式气泡（`active = true`）。
    pub fn start() -> Self {
        Self {
            segments: Vec::new(),
            active: true,
            stopped: false,
        }
    }

    /// 追加一段分片。空段忽略（agent 可以发空 chunk）。
    /// 同类接上一段，异类**新开一段**（段序 = 上屏顺序）。
    /// 返回 `true` 表示内容发生变化（GUI 需重渲染）。
    pub fn push(&mut self, kind: ChunkKind, delta: &str, now: f64) -> bool {
        if delta.is_empty() {
            return false;
        }
        match self.segments.last_mut() {
            Some(seg) if seg.kind == kind => {
                seg.text.push_str(delta);
                seg.last = now;
            }
            _ => self.segments.push(StreamSegment {
                kind,
                text: delta.to_string(),
                started: now,
                last: now,
            }),
        }
        true
    }

    /// ⏹ 停止：保留已生成部分，标记不再活跃（「正在输入」收起、显示「已停止」）。
    pub fn stop(&mut self) {
        self.stopped = true;
        self.active = false;
    }

    /// 收尾去重：回合结束时以 `final_text` 为**唯一权威**重建正文段
    ///（流式拼接与最终文本不一致时——漏片/重发/后端规范化——以最终为准，
    /// 且只渲染这一条，不会再追加第二条）。**思考段原样保留**（最终文本里没有思考）。
    /// 返回 `true` 表示正文发生变化。
    pub fn finish(&mut self, final_text: &str, now: f64) -> bool {
        let before = self.shown();
        self.segments.retain(|s| s.kind != ChunkKind::Text);
        if !final_text.is_empty() {
            self.segments.push(StreamSegment {
                kind: ChunkKind::Text,
                text: final_text.to_string(),
                started: now,
                last: now,
            });
        }
        self.active = false;
        before != self.shown()
    }

    /// 已显示的**正文**（正文段按顺序拼接；思考段不计入）。
    pub fn shown(&self) -> String {
        self.text_of(ChunkKind::Text)
    }

    /// 已生成的**思考过程**（思考段按顺序拼接）。
    pub fn thought(&self) -> String {
        self.text_of(ChunkKind::Thought)
    }

    pub fn has_thought(&self) -> bool {
        self.segments
            .iter()
            .any(|s| s.kind == ChunkKind::Thought && !s.text.is_empty())
    }

    fn text_of(&self, kind: ChunkKind) -> String {
        let mut out = String::new();
        for s in self.segments.iter().filter(|s| s.kind == kind) {
            out.push_str(&s.text);
        }
        out
    }

    /// 分段快照（顺序即到达顺序，供渲染/历史区分思考与正文）。
    pub fn segments(&self) -> &[StreamSegment] {
        &self.segments
    }

    /// 思考累计耗时（秒）。`now` 只用于**仍在进行**的最后一段
    ///（其余段用它们自己的 `last`，否则会把前面几段的耗时按 `now` 重复放大）。
    pub fn thought_secs(&self, now: f64) -> f64 {
        let last = self.segments.len().saturating_sub(1);
        self.segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind == ChunkKind::Thought)
            .map(|(i, s)| {
                if self.active && i == last {
                    (now.max(s.last) - s.started).max(0.0)
                } else {
                    s.elapsed()
                }
            })
            .sum()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }
}

/// 思考耗时文案（稿：`▸ 思考 · 用时 3.2s`）。
///
/// 口径：< 60s 用秒（一位小数）；≥ 60s 用分（一位小数）——长思考按秒读太累。
/// 纯函数（单测钉住格式化与边界）。
pub fn format_elapsed(secs: f64) -> String {
    let secs = secs.max(0.0);
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{:.1}m", secs / 60.0)
    }
}

/// 客户端打字机（E1/E2 共用，纯逻辑）：后端一次吐一大段时，按拍把文本**逐段**推进上屏。
///
/// 为什么需要它（**实测口径**）：ABB 随包执行层 `abb-agent` 在 `MessageEnd` **整条**
/// 发正文 chunk（见 `crates/abb-agent/src/acp.rs` 的注释），一个回合常常只有 1～2 片；
/// 直接渲染就是「一次性蹦出」。打字机维护 `target`（已收到的权威文本）与 `shown`
///（已显示的**字符边界前缀**），保证：
/// ① `shown` 恒为 `target` 的前缀 ⇒ 永不丢字/乱序（多字节字符按 char 推进）；
/// ② 拍速自适应（积压越多每拍推进越多，约**减半**追赶）⇒ 大段也能在十来拍内追平，
///    小段（≤3 字）仍是逐字；
/// ③ 停止/收尾时 `flush` 把已生成的都显示出来 ⇒ 不留「收到了但没上屏」的字。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Typewriter {
    target: String,
    shown: String,
}

impl Typewriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 每拍推进的字符上限（配合 50ms 的流式定时器 ≈ 80 字/秒）。
    pub const STEP_CAP: usize = 4;

    /// 每拍最多推进的字符数：`max(1, pending/2)`，再**封顶 [`Self::STEP_CAP`]**。
    ///
    /// 为什么封顶（owner 2026-10-16「生成时没有打字机效果」）：后端一次可能吐很大一段，
    /// 纯「减半追赶」会一拍显示一半 ⇒ 观感是**整段蹦出来**。封顶后即使积压很大也是每拍
    /// 固定几字（50ms 一拍 ⇒ 约 80 字/秒），既看得见打字机、也不会拖太久。
    pub fn step_for(pending: usize) -> usize {
        (pending / 2).clamp(1, Self::STEP_CAP)
    }

    /// 追加已到手的文本（权威侧，只增不减）。空段忽略。
    pub fn push(&mut self, delta: &str) {
        if !delta.is_empty() {
            self.target.push_str(delta);
        }
    }

    /// 用权威全文替换目标（回合结束去重：以 `final_text` 为准）。
    /// 已显示部分若仍是新目标的前缀则保留，否则清空（不得显示不属于目标的字节）。
    pub fn set_target(&mut self, full: &str) {
        if !full.starts_with(&self.shown) {
            self.shown.clear();
        }
        self.target = full.to_string();
    }

    /// 推进一拍。返回 `true` 表示显示文本发生变化。
    pub fn tick(&mut self) -> bool {
        let pending = self
            .target
            .chars()
            .count()
            .saturating_sub(self.shown.chars().count());
        if pending == 0 {
            return false;
        }
        let step = Self::step_for(pending);
        // `shown` 恒为 `target` 的前缀 ⇒ 字节偏移必落在 char 边界上。
        let take: String = self.target[self.shown.len()..].chars().take(step).collect();
        if take.is_empty() {
            return false;
        }
        self.shown.push_str(&take);
        true
    }

    /// 立即推到目标末尾（停止 / 收尾：不留未显示的已生成文本）。
    /// 返回 `true` 表示发生变化。
    pub fn flush(&mut self) -> bool {
        if self.shown == self.target {
            return false;
        }
        self.shown = self.target.clone();
        true
    }

    pub fn shown(&self) -> &str {
        &self.shown
    }

    /// 已显示到目标末尾（可立即收尾，不必再等下一拍）。
    pub fn done(&self) -> bool {
        self.shown.len() == self.target.len()
    }
}

/// 流式回合的接线（E1）：正文分片 sink + ⏹ 停止用的外部取消令牌 + 生成中插入（本卡）。
///
/// `cancel` 直接作为 `oneshot_turn_streaming` 的 `external_cancel`——
/// **不新造取消机制**（触发走既有的 Timeout 同款 teardown，返回 `Cancelled`）。
struct StreamWiring {
    chunks: tokio::sync::mpsc::UnboundedSender<TurnChunk>,
    cancel: tokio_util::sync::CancellationToken,
    /// 生成中插入接线（本卡）。`None` = 不接（非 GUI 入口 / 旧调用点，行为不变）。
    steer: Option<SteerWiring>,
}

/// 系统级 agent 配置：**指定供应商**（空 = 全局默认）+ owner 全权限档（无沙箱）+ 专用工作区。
pub fn agent_config(cfg: &Config, provider: &str) -> AgentConfig {
    crate::service::oneshot_agent_config_system(cfg, provider)
}

/// 执行层命令是否可用（fail-fast 预检）。
///
/// 为什么需要：harness 把「agent 拉不起」（二进制缺失/initialize 连败）当作**可重试**，
/// 于是缺执行层时结局是「挂满整轮预算得 Timeout」——对话预算是分钟级，用户会以为程序卡死。
/// 这里在发包前先判一次命令可达性，给出可操作的错误（与 `deps` 的 PATH 解析同一口径）。
fn agent_command_available(cmd: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    let p = std::path::Path::new(cmd);
    // 绝对/相对路径（随包 abb-agent 就是绝对路径）：必须真存在；裸名走 PATH 查找。
    if cmd.contains(std::path::MAIN_SEPARATOR) || p.is_absolute() {
        p.is_file()
    } else {
        crate::deps::find_in_path(cmd).is_some()
    }
}

/// 内层回合（E1）：给定已装配好的 agent 配置 / 工作区 / prompt 跑一轮 oneshot，
/// 把正文 delta 经 sink 实时回传，并把外部取消令牌交给 oneshot（⏹ 停止）。
///
/// `stream = None` 即「非流式」入口（原来由 `turn_with` 薄封装承担；E3 起直接调用本
/// 函数，好让**两条路径都在同一处**拿到并落盘 usage）。
///
/// 去重口径：本函数**不去重**，只把「流式拼接」与「最终文本」两条都交给调用方；
/// 以谁为准由渲染方决定（GUI 侧口径：回合结束时以 `final_text` 为准重建气泡，
/// 见 `StreamingBubble::finish`）。
async fn turn_with_stream(
    agent_cfg: AgentConfig,
    workspace: PathBuf,
    prompt: String,
    stream: Option<StreamWiring>,
) -> Result<TurnEnd, String> {
    let msg = InboundMsg {
        id_hex: uuid::Uuid::new_v4().to_string(),
        author_role: crate::config::SenderRole::Owner.as_str().to_string(),
        text: prompt,
        ts_secs: crate::chrono_lite::unix_secs() as i64,
        prompt_tag: "syschat".to_string(),
        bridge_generation: String::new(),
    };
    let (chunk_tx, cancel, steer) = match stream {
        Some(w) => (Some(w.chunks), Some(w.cancel), w.steer),
        None => (None, None, None),
    };
    let (outcome, usage) = crate::runtime::oneshot::oneshot_turn_streaming_steerable(
        agent_cfg,
        Some(workspace.display().to_string()),
        msg,
        TURN_BUDGET,
        cancel,
        chunk_tx,
        steer,
    )
    .await;
    match outcome {
        SyncTurnOutcome::Ok(text) => {
            if text.trim().is_empty() {
                Err("模型返回了空回复。".to_string())
            } else {
                Ok(TurnEnd::Done { text, usage })
            }
        }
        SyncTurnOutcome::Timeout => Err(format!(
            "对话超时（{}s）。可重试或把问题拆小。",
            TURN_BUDGET.as_secs()
        )),
        SyncTurnOutcome::Closed => Err("agent 不可用（会话未建起）。".to_string()),
        SyncTurnOutcome::Failed(reason) => Err(reason),
        // E1：停止不是错误——已生成文本另存，GUI 保留已显示内容并提示「已停止」。
        SyncTurnOutcome::Cancelled => Ok(TurnEnd::Stopped),
    }
}

/// 会话/agent 配置/工作区装配（[`turn_with_attachments`] 与 [`turn_streaming`] 共用）。
///
/// 返回 `(store, session_id, agent_cfg, ws, trimmed_text)`。
fn prepare_turn(
    cfg: &Config,
    user_text: &str,
) -> Result<(SessionStore, String, AgentConfig, PathBuf, String), String> {
    let text = user_text.trim();
    if text.is_empty() {
        return Err("消息为空。".to_string());
    }

    let store = SessionStore::production();
    // 解析当前会话 + 该会话的模型选择（短临界区）。首次建库**立即落盘**——否则临界区
    // 重新 load 时 state 为空、标题更新落空，本轮会落在孤立的 sessions/<id>.jsonl（评审 N1）。
    let (session_id, provider) = {
        let _g = store.lock()?;
        let mut state = store.load();
        if store.ensure_current(&mut state) {
            store
                .save(&state)
                .map_err(|e| format!("写入会话索引失败：{e}"))?;
        }
        let id = state.current.clone();
        let provider = store.provider_of(&id);
        (id, provider)
    };

    let agent_cfg = agent_config(cfg, &provider);
    // fail-fast：缺执行层时不要挂满整轮预算（见 agent_command_available 文档）。
    if !agent_command_available(&agent_cfg.command) {
        return Err(format!(
            "执行层不可用：找不到 `{}`。请确认已安装随包 abb-agent（发行包自带），或在本机 PATH 上安装 ki（回落执行层 `ki acp serve`）。",
            agent_cfg.command
        ));
    }
    let ws = workspace();
    std::fs::create_dir_all(&ws)
        .map_err(|e| format!("创建系统对话工作区失败（{}）：{e}", ws.display()))?;
    Ok((store, session_id, agent_cfg, ws, text.to_string()))
}

/// 跑一轮系统级对话（非流式，保留的非流式入口）：按**当前会话**（与其模型选择）
/// 装配 agent，再交给 [`turn_core`] 跑会话/历史逻辑。模型选择在**本轮开头**解析一次
/// ⇒「切换从下一条消息起生效」。
///
/// GUI 生产路径已统一走流式 [`turn_streaming`]（打字机 + ⏹）；本函数与
/// [`turn_with_attachments`] 保留给无打字机需求的调用方（headless / 将来入口）。
#[allow(dead_code)] // 保留的非流式 API；当前 bin 内无调用点
pub async fn turn(cfg: &Config, user_text: &str) -> Result<String, String> {
    turn_with_attachments(cfg, user_text, &[]).await
}

/// 带待发附件的一轮系统级对话（C 批：📎 / ⌘V，非流式）。
///
/// 附件只以**本地路径**进 prompt（见 [`prompt_with_attachments`]），落盘与去重已在
/// [`land_attachments`] 完成；历史仍只记用户输入的文本——后续回合重放的是对话，不是
/// 附件清单（同一条附件不会因为历史重放而被反复描述）。
pub async fn turn_with_attachments(
    cfg: &Config,
    user_text: &str,
    atts: &[Attachment],
) -> Result<String, String> {
    let (store, session_id, agent_cfg, ws, text) = prepare_turn(cfg, user_text)?;
    match turn_core(&store, &session_id, agent_cfg, ws, &text, atts, None).await? {
        TurnEnd::Done { text, .. } => Ok(text),
        TurnEnd::Stopped => Err("对话已中断。".to_string()),
    }
}

/// 带待发附件的**流式**系统级回合（E1，GUI 打字机 + ⏹ 停止的生产入口）。
///
/// 与 [`turn_with_attachments`] 共用会话/历史逻辑，额外把正文 delta 经 `chunks` 实时
/// 回传；`cancel` 触发时返回 [`TurnEnd::Stopped`]，已生成部分已随分片流出并被落进历史。
///
/// 本卡新增 `steer`：接上后，调用方可在本轮进行中把消息**插入在途回合**。
/// `None` = 不接（行为与改动前逐字节等价）。
pub async fn turn_streaming(
    cfg: &Config,
    user_text: &str,
    atts: &[Attachment],
    chunks: tokio::sync::mpsc::UnboundedSender<TurnChunk>,
    cancel: tokio_util::sync::CancellationToken,
    steer: Option<SteerWiring>,
) -> Result<TurnEnd, String> {
    let (store, session_id, agent_cfg, ws, text) = prepare_turn(cfg, user_text)?;
    turn_core(
        &store,
        &session_id,
        agent_cfg,
        ws,
        &text,
        atts,
        Some(StreamWiring {
            chunks,
            cancel,
            steer,
        }),
    )
    .await
}

/// 可测核心：会话 store / agent 配置 / 工作区都由调用方注入（测试用临时目录 + mock agent，
/// 不碰真实数据目录，满足隔离门禁）。
///
/// 历史锁只包住两段短临界区（读史+落用户轮 / 落助手轮），**回合在锁外跑**：
/// 回合最长 10 分钟，持锁跨回合会阻塞 GUI 主线程的清空/重开（同一个 `history.lock`）。
///
/// `stream = Some` 时接线 E1 流式：delta 一边喂 GUI（`chunks`）、一边在本地累积——
/// 用户「停止」时把已生成文本落进历史（否则重开对话框那段回复就消失了，与「保留已生成文本」相悖）。
async fn turn_core(
    store: &SessionStore,
    session_id: &str,
    agent_cfg: AgentConfig,
    ws: PathBuf,
    user_text: &str,
    atts: &[Attachment],
    stream: Option<StreamWiring>,
) -> Result<TurnEnd, String> {
    let text = user_text.trim();
    // 临界区 1：读该会话历史拼 prompt + 落用户轮 + 刷新标题/时间。先落用户轮是为了：
    // 即使回合失败，用户说过的话也不该丢（下次重开对话框仍能看到自己的提问）。
    let prompt = {
        let _g = store.lock()?;
        let history = store.read_history(session_id);
        let prompt = prompt_with_attachments(&history, text, atts);
        let now = crate::chrono_lite::unix_secs() as i64;
        store
            .append_turns(
                session_id,
                &[Turn {
                    role: "user".to_string(),
                    text: text.to_string(),
                    thought: String::new(),
                    ts: now,
                }],
            )
            .map_err(|e| format!("写入对话历史失败：{e}"))?;
        let mut state = store.load();
        if let Some(s) = state.sessions.iter_mut().find(|s| s.id == session_id) {
            if s.title.is_empty() || s.title == "新会话" {
                s.title = derive_title(text);
            }
            s.updated_at = now;
        }
        let _ = store.save(&state);
        prompt
    };

    // 回合在锁外跑。返回 `(要落历史的助手正文, 要落历史的思考过程, 是否用户停止)`：
    // - 正常完成 → 正文取 `final_text`（去重口径的权威侧），思考取累积器里的思考段；
    // - 用户停止 → 正文与思考都用本地累积的已生成部分（GUI 侧已有同一份，不重复发）。
    let (reply, thought, stopped, usage) = match stream {
        None => {
            // 非流式：cancel 恒 None ⇒ 不会 Stopped。直接走底层（而不是 [`turn_with`]）是
            // 为了**同一处**拿到 usage：两条路径都落「本会话上下文占用」，否则非流式入口
            // 聊出来的会话会永远显示未知（`turn_with` 只是本函数的无 sink 薄封装）。
            match turn_with_stream(agent_cfg, ws, prompt, None).await? {
                TurnEnd::Done { text, usage } => (Some(text), String::new(), false, usage),
                TurnEnd::Stopped => (None, String::new(), true, None),
            }
        }
        Some(w) => {
            // 中间层：sink 的每条分片既累积（停止/思考分离用）又转发给 GUI（`w.chunks`）。
            // 累积器与 GUI 共用同一条 `ChunkKind` 语义——不新造第二条通道。
            let acc = std::sync::Arc::new(std::sync::Mutex::new(StreamingBubble::start()));
            let (inner_tx, mut inner_rx) = tokio::sync::mpsc::unbounded_channel::<TurnChunk>();
            let gui = w.chunks.clone();
            let acc2 = acc.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(chunk) = inner_rx.recv().await {
                    if let Ok(mut b) = acc2.lock() {
                        // 此处不需要耗时（GUI 自己按墙钟算）：`now = 0.0` 只占位。
                        b.push(chunk.kind, &chunk.text, 0.0);
                    }
                    // GUI 侧收端若提前消失（窗口关了），不必中断回合——只是不再累积之外的多余投递。
                    let _ = gui.send(chunk);
                }
            });
            // 本卡：生成中插入（steer）转接。这里做两件事——
            // ① **落用户轮进历史**（插入的消息是用户真发出来的一句话，下一轮重放
            //    必须包含它；否则用户自己的话在历史里凭空消失，上下文对不上）；
            // ② 转给 oneshot 的注入端点（它会 `push_message` 进在途回合 ⇒ harness steer）。
            // 能力不上报到用不上的地方：`cap_tx` 原样透传。
            // 注：注入只发生在能力=`Supported` 时（调用方判定），所以这里转发不会
            // 触发 harness 的 cancel+merge 回退；若真要回退，也不影响本卡口径。
            let steer = w.steer.map(|sw| {
                let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<InboundMsg>();
                let store2 = SessionStore::new_at(store.base.clone());
                let sid = session_id.to_string();
                let mut inject_rx = sw.inject_rx;
                tokio::spawn(async move {
                    while let Some(msg) = inject_rx.recv().await {
                        if let Ok(_g) = store2.lock() {
                            let _ = store2.append_turns(
                                &sid,
                                &[Turn {
                                    role: "user".to_string(),
                                    text: msg.text.clone(),
                                    thought: String::new(),
                                    ts: crate::chrono_lite::unix_secs() as i64,
                                }],
                            );
                        }
                        if s_tx.send(msg).is_err() {
                            break;
                        }
                    }
                });
                SteerWiring {
                    cap_tx: sw.cap_tx,
                    inject_rx: s_rx,
                }
            });
            let res = turn_with_stream(
                agent_cfg,
                ws,
                prompt,
                Some(StreamWiring {
                    chunks: inner_tx,
                    cancel: w.cancel,
                    steer,
                }),
            )
            .await;
            // 回合已收尾 ⇒ sink 已摘除，转发任务随之结束；等它把缓冲分片冲完再决定停止态文本。
            let _ = forwarder.await;
            match res? {
                TurnEnd::Done { text, usage } => {
                    let thought = acc.lock().map(|b| b.thought()).unwrap_or_default();
                    (Some(text), thought, false, usage)
                }
                TurnEnd::Stopped => {
                    let (partial, thought) = acc
                        .lock()
                        .map(|b| (b.shown(), b.thought()))
                        .unwrap_or_default();
                    // 一个分片都没流出（极早停止）：不落空的助手轮；
                    // 只有思考、还没出正文时仍要落（思考也是「已生成部分」）。
                    let partial = (!partial.trim().is_empty()).then_some(partial);
                    // 停止的回合不落 usage：执行层在取消出口根本不带它，
                    // 硬留上一轮的旧数会让「已用」看着像刚降下来。
                    (partial, thought, true, None)
                }
            }
        }
    };

    // 临界区 2：落助手轮（含思考）+ 刷新会话时间（停止时落的是已生成部分）。
    // 正文空但思考非空（停在思考阶段）也要落盘，否则重开对话框思考块就消失了。
    if reply.is_some() || !thought.is_empty() {
        let _g = store.lock()?;
        let now = crate::chrono_lite::unix_secs() as i64;
        store
            .append_turns(
                session_id,
                &[Turn {
                    role: "assistant".to_string(),
                    text: reply.clone().unwrap_or_default(),
                    thought: thought.clone(),
                    ts: now,
                }],
            )
            .map_err(|e| format!("写入对话历史失败：{e}"))?;
        let mut state = store.load();
        if let Some(s) = state.sessions.iter_mut().find(|s| s.id == session_id) {
            s.updated_at = now;
            // E3：本轮上报了真实 usage ⇒ 记「本会话上下文占用」= 该轮 provider 看到的
            // 全部 prompt tokens（`context_tokens()`，含缓存命中部分）。这是**真实值**
            // 而不是估算：来源是执行层从 provider 响应里解出来的计数。
            if let Some(usage) = usage {
                s.context_tokens = usage.context_tokens();
            }
        }
        let _ = store.save(&state);
    }
    Ok(if stopped {
        TurnEnd::Stopped
    } else {
        TurnEnd::Done {
            text: reply.unwrap_or_default(),
            usage,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn turn_of(role: &str, text: &str) -> Turn {
        Turn {
            role: role.to_string(),
            text: text.to_string(),
            thought: String::new(),
            ts: 0,
        }
    }

    // ── C2 拖放状态机（winit 事件没法单测，口径钉在这里）──

    /// 同一轮拖放里，同一路径只落一次（winit `draggingEntered:` 会把全部文件名各发一条
    /// `HoveredFile`，而逐文件又发 `DroppedFile`，不去重就会重复落地）。
    #[test]
    fn drop_tracker_dedups_within_one_drag() {
        let mut t = DropTracker::new();
        t.hovered();
        assert!(t.dropped(std::path::Path::new("/tmp/a.png")), "首次应落地");
        assert!(
            !t.dropped(std::path::Path::new("/tmp/a.png")),
            "同一轮重复不应再落"
        );
        assert!(
            !t.hovering(),
            "drop 后应收起高亮（winit 不发 cancel，由我们收尾）"
        );
    }

    /// **新的一轮**拖放必须把去重集清掉——否则「把同一个文件拖两次」的第二次会被静默丢掉。
    /// 变异点：删掉 `hovered()` 里 `if !self.hovering` 的清空分支，本测即红。
    #[test]
    fn drop_tracker_new_drag_resets_dedupe() {
        let mut t = DropTracker::new();
        t.hovered();
        assert!(t.dropped(std::path::Path::new("/tmp/a.png")));
        t.hovered(); // 第二次拖入同一个文件
        assert!(t.hovering());
        assert!(
            t.dropped(std::path::Path::new("/tmp/a.png")),
            "新一轮应重新落地"
        );
    }

    /// 一次拖入多文件：每个都应落地各自一次。
    #[test]
    fn drop_tracker_multi_file_drop_lands_each_once() {
        let mut t = DropTracker::new();
        t.hovered();
        assert!(t.dropped(std::path::Path::new("/tmp/a.png")));
        assert!(t.dropped(std::path::Path::new("/tmp/b.mov")));
        assert!(t.dropped(std::path::Path::new("/tmp/c.pdf")));
        assert!(!t.dropped(std::path::Path::new("/tmp/a.png")));
    }

    /// C2-B：一次拖放多文件，批计数必须等于文件数（不是 1）。
    ///
    /// 变异点：`take()` 只回最后一个（或 `push` 不累计）⇒ 本测红。
    #[test]
    fn drop_batcher_counts_whole_gesture() {
        let mut b = DropBatcher::new();
        assert_eq!(b.push(std::path::PathBuf::from("/tmp/a.png")), 1);
        assert_eq!(b.push(std::path::PathBuf::from("/tmp/b.mov")), 2);
        assert_eq!(
            b.push(std::path::PathBuf::from("/tmp/c.pdf")),
            3,
            "一批 3 个 ⇒ 状态行报 3"
        );
        let taken = b.take();
        assert_eq!(taken.len(), 3);
        assert_eq!(
            taken,
            vec![
                std::path::PathBuf::from("/tmp/a.png"),
                std::path::PathBuf::from("/tmp/b.mov"),
                std::path::PathBuf::from("/tmp/c.pdf")
            ],
            "批内顺序 = 事件到达顺序"
        );
        assert!(b.take().is_empty(), "取走即清空：下一次手势从 0 重新计数");
        assert_eq!(b.push(std::path::PathBuf::from("/tmp/d.png")), 1);
    }

    /// 拖出/取消：收高亮 + 清去重集（下一轮重新可落）。
    ///
    /// 变异点：删掉 `cancelled()` 里的 `landed.clear()` ⇒ 中间那条断言红。**只有它咬得住**：
    /// `hovered()` 自己在 `hovering` false→true 时也会清（见上一条），所以「取消 → 重新 hover
    /// → drop」这条路删掉清空照样绿（winit 0.30.13 每次拖入都重发 `draggingEntered:`）。
    #[test]
    fn drop_tracker_cancel_resets() {
        let p = std::path::Path::new("/tmp/a.png");
        let mut t = DropTracker::new();
        t.hovered();
        assert!(
            t.dropped(p),
            "第一轮应落地（不先落一次，下面那条断言就不承重）"
        );
        t.cancelled();
        assert!(!t.hovering());
        // 取消后**没有**新的 hover 就直接 drop（winit 0.30.13 不发这种序，属防御）：仍应落地
        assert!(
            t.dropped(p),
            "取消后同一路径不得被当成「本轮已落过」而静默吞掉"
        );
        t.hovered();
        assert!(t.dropped(p));
    }

    #[test]
    fn prompt_includes_preamble_history_and_current() {
        let h = vec![
            turn_of("user", "你好"),
            turn_of("assistant", "你好，有什么可以帮你？"),
        ];
        let p = build_prompt(&h, "帮我看看磁盘");
        assert!(p.contains("[系统级对话]"), "前言缺失：{p}");
        assert!(p.contains("## 之前的对话"));
        assert!(p.contains("用户：你好"));
        assert!(p.contains("助手：你好，有什么可以帮你？"));
        assert!(
            p.trim_end().ends_with("用户：帮我看看磁盘"),
            "当前消息应在末尾：{p}"
        );
    }

    #[test]
    fn prompt_without_history_has_no_history_block() {
        let p = build_prompt(&[], "第一条");
        assert!(!p.contains("## 之前的对话"), "空历史不该出现历史段：{p}");
        assert!(p.trim_end().ends_with("用户：第一条"));
    }

    #[test]
    fn recent_history_keeps_only_recent_turns() {
        let mut h = Vec::new();
        for i in 0..50 {
            h.push(turn_of("user", &format!("u{i}")));
            h.push(turn_of("assistant", &format!("a{i}")));
        }
        let recent = recent_history(&h);
        assert_eq!(recent.len(), HISTORY_KEEP_TURNS * 2);
        // 最旧的那条必须被丢掉、最新的必须在
        assert_eq!(recent.last().unwrap().text, "a49");
        assert!(!recent.iter().any(|t| t.text == "u0"), "最旧轮应被裁掉");
    }

    #[test]
    fn recent_history_short_returns_all() {
        let h = vec![turn_of("user", "only")];
        assert_eq!(recent_history(&h), h.as_slice());
    }

    #[test]
    fn clamp_text_truncates_on_char_boundary() {
        let long = "汉".repeat(MAX_HISTORY_CHARS + 10);
        let clamped = clamp_text(&long);
        assert!(clamped.starts_with(&"汉".repeat(MAX_HISTORY_CHARS)));
        assert!(clamped.contains("已截断"));
        // 短文本原样
        assert_eq!(clamp_text("短"), "短");
    }

    #[test]
    fn turn_serializes_as_jsonl_roundtrip() {
        let t = turn_of("user", "带\"引号\"\n换行");
        let line = serde_json::to_string(&t).unwrap();
        assert!(!line.contains('\n'), "JSONL 单行化：{line}");
        let back: Turn = serde_json::from_str(&line).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn bubble_pos_from_physical_normalizes_retina() {
        // Retina 2x：物理 (200,100) → 逻辑 (100,50)
        assert_eq!(
            BubblePos::from_physical(200, 100, 2.0),
            BubblePos { x: 100, y: 50 }
        );
        // 1x：原样
        assert_eq!(
            BubblePos::from_physical(100, 50, 1.0),
            BubblePos { x: 100, y: 50 }
        );
        // scale ≤ 0（异常）：按 1 处理，不产生 NaN/inf
        assert_eq!(
            BubblePos::from_physical(10, 20, 0.0),
            BubblePos { x: 10, y: 20 }
        );
        // 负坐标（副屏在主屏左侧）也按同一换算
        assert_eq!(
            BubblePos::from_physical(-300, 40, 2.0),
            BubblePos { x: -150, y: 20 }
        );
    }

    #[test]
    fn agent_command_available_checks_path_and_existence() {
        // 空命令
        assert!(!agent_command_available(""));
        // 绝对路径：存在才真
        let me = std::env::current_exe().unwrap();
        assert!(agent_command_available(&me.display().to_string()));
        let ghost =
            std::env::temp_dir().join(format!("abb-no-such-agent-{}", uuid::Uuid::new_v4()));
        assert!(!agent_command_available(&ghost.display().to_string()));
        // 裸名：PATH 里没有的名字必须判不可用
        assert!(!agent_command_available(
            "abb-definitely-not-a-real-binary-xyz"
        ));
    }

    // ── mock 全链路：prompt 装配 → oneshot 回合（不碰真实历史文件）──

    fn mock_agent_cfg(record_file: &std::path::Path) -> crate::runtime::harness::AgentConfig {
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        crate::runtime::harness::AgentConfig {
            command: python3,
            args: vec![script.display().to_string()],
            extra_env: vec![
                ("PATH".to_string(), crate::deps::composed_path()),
                (
                    "MOCK_RECORD_FILE".to_string(),
                    record_file.display().to_string(),
                ),
            ],
            backend: "mock".to_string(),
            session_sandbox: None,
        }
    }

    fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// 同 [`mock_agent_cfg`]，额外注入夹具环境变量（E2：多片思考/正文）。
    fn mock_agent_cfg_with(
        record_file: &std::path::Path,
        extra: &[(&str, &str)],
    ) -> crate::runtime::harness::AgentConfig {
        let mut cfg = mock_agent_cfg(record_file);
        cfg.extra_env
            .extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        cfg
    }

    /// GUI 路径的核心断言：装配好的 prompt 真送到 agent，回程文本原样返回。
    /// （echo 臂：mock 把 prompt 原文 echo 回来，据此证明"历史确实进了 prompt"。）
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn turn_with_sends_prompt_and_returns_reply() {
        let rec =
            std::env::temp_dir().join(format!("abb-syschat-rec-{}.jsonl", uuid::Uuid::new_v4()));
        let ws = std::env::temp_dir().join(format!("abb-syschat-ws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let history = vec![
            turn_of("user", "syschat-history-marker"),
            turn_of("assistant", "收到"),
        ];
        let prompt = build_prompt(&history, "syschat-current-marker");
        let reply = done_text(
            turn_with_stream(mock_agent_cfg(&rec), ws, prompt, None)
                .await
                .expect("mock echo 回合应 Ok"),
        );
        assert!(reply.contains("echo:"), "echo 文本：{reply}");
        assert!(
            reply.contains("syschat-history-marker"),
            "历史应进 prompt 并被 echo：{reply}"
        );
        assert!(
            reply.contains("syschat-current-marker"),
            "当前消息应进 prompt 并被 echo：{reply}"
        );
        let events = read_records(&rec);
        assert!(
            events.iter().any(|e| e["event"] == "prompt"
                && e["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("[系统级对话]")),
            "prompt 记录应带系统前言：{events:?}"
        );
    }

    /// 卡 d9 定时发送「到点真的会发」的全链路（mock 端到端，不碰 GUI）：
    /// 排一条已到点 + 一条未到点 → `take_due` 只吐已到点那条并**同时从存储揭除** → 该文本
    /// 再走手动发送同一条执行链（`turn_with` + mock agent）→ mock 记录里真的看到它。
    ///
    /// 判别力（负向）：把 [`crate::syschat_sched::is_due`] 的比较反成 `t.to_unix() > now` ⇒
    /// due 集合变空（或拿到未到点那条），本测红。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn scheduled_message_fires_through_mock_agent() {
        let dir = std::env::temp_dir().join(format!(
            "abb-syschat-sched-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = crate::syschat_sched::SchedStore::at(dir.join("scheduled.json"));
        let now = crate::schedule::DateTime::now();
        let past = crate::schedule::DateTime::from_unix(now.to_unix() - 60);
        let future = crate::schedule::DateTime::from_unix(now.to_unix() + 3600);
        store
            .add(
                "syschat-scheduled-marker",
                &crate::syschat_sched::format_at(&past),
            )
            .unwrap();
        store
            .add(
                "syschat-future-marker",
                &crate::syschat_sched::format_at(&future),
            )
            .unwrap();

        // 到点那一拍：只拿到已到点的，且已从存储揭除（不会重复发）。
        let due = store.take_due(now);
        assert_eq!(due.len(), 1, "应只触发已到点的那一条：{due:?}");
        assert!(due[0].text.contains("syschat-scheduled-marker"));
        assert!(store.take_due(now).is_empty(), "同一分钟后不得重复触发");
        assert_eq!(store.list()[0].text, "syschat-future-marker");

        // 真正发出的那一段：与手动发送共用同一条内层回合（scheduler 到点后调的就是
        // `submit_sc`，后者内部起的是同一条 `turn_streaming`/`turn_core` 链）。
        let rec =
            std::env::temp_dir().join(format!("abb-syschat-srec-{}.jsonl", uuid::Uuid::new_v4()));
        let ws = std::env::temp_dir().join(format!("abb-syschat-sws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let reply = done_text(
            turn_with_stream(mock_agent_cfg(&rec), ws, due[0].text.clone(), None)
                .await
                .expect("mock echo 回合应 Ok"),
        );
        assert!(
            reply.contains("syschat-scheduled-marker"),
            "定时文本没到 agent（echo 应包含它）：{reply}"
        );
        let events = read_records(&rec);
        assert!(
            events.iter().any(|e| e["event"] == "prompt"
                && e["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("syschat-scheduled-marker")),
            "agent 侧记录里应有定时文本：{events:?}"
        );
    }

    /// E1 流式全链路（隔离夹具）：sink 真的转发正文 delta，回合结局 Ok，
    /// 历史里助手轮与最终文本一致（去重口径的数据侧前提）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn turn_core_streaming_forwards_deltas_and_records_history() {
        let rec =
            std::env::temp_dir().join(format!("abb-syschat-srec-{}.jsonl", uuid::Uuid::new_v4()));
        let (store, base) = tmp_store("stream");
        let mut st = store.load();
        assert!(store.ensure_current(&mut st));
        store.save(&st).unwrap();
        let id = st.current.clone();
        let ws = std::env::temp_dir().join(format!("abb-syschat-sws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TurnChunk>();
        let end = turn_core(
            &store,
            &id,
            mock_agent_cfg(&rec),
            ws,
            "syschat-stream-marker",
            &[],
            Some(StreamWiring {
                chunks: tx,
                cancel: CancellationToken::new(),
                steer: None,
            }),
        )
        .await
        .expect("mock echo 回合应 Done");
        let TurnEnd::Done { text, .. } = end else {
            panic!("未取消的回合应为 Done，实际 {end:?}");
        };
        assert!(text.contains("syschat-stream-marker"), "最终文本: {text}");

        // 回合已收尾 ⇒ sink 已摘除；缓冲区里的分片不会再增长。
        let mut streamed = String::new();
        while let Ok(c) = rx.try_recv() {
            streamed.push_str(&c.text);
        }
        assert!(
            streamed.contains("syschat-stream-marker"),
            "sink 应转发正文 delta: {streamed}"
        );
        // 去重口径前提：mock 单分片场景下流式拼接与最终文本逐字节相等。
        assert_eq!(streamed, text, "流式拼接应等于最终文本");

        // 历史：user + assistant 两条，助手轮 = 最终文本。
        let hist = store.read_history(&id);
        assert_eq!(hist.len(), 2, "历史应含 user + assistant: {hist:?}");
        assert_eq!(hist[1].role, "assistant");
        assert_eq!(hist[1].text, text);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// E2 流式全链路（隔离夹具）：mock 发 3 片思考 + 2 片正文 ⇒ **同一条 sink** 上按到达
    /// 顺序拿到 Thought×3、Text×2；历史里助手轮**分别**存下思考与正文（不丢任一侧）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn turn_core_streams_thought_and_text_into_history() {
        let rec =
            std::env::temp_dir().join(format!("abb-syschat-trec-{}.jsonl", uuid::Uuid::new_v4()));
        let (store, base) = tmp_store("thought");
        let mut st = store.load();
        assert!(store.ensure_current(&mut st));
        store.save(&st).unwrap();
        let id = st.current.clone();
        let ws = std::env::temp_dir().join(format!("abb-syschat-tws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TurnChunk>();
        let end = turn_core(
            &store,
            &id,
            mock_agent_cfg_with(
                &rec,
                &[
                    ("MOCK_THOUGHT_TEXT", "先想A再想B再想C"),
                    ("MOCK_THOUGHT_CHUNKS", "3"),
                    ("MOCK_TEXT_TEXT", "正文一号正文二号"),
                    ("MOCK_TEXT_CHUNKS", "2"),
                ],
            ),
            ws,
            "syschat-thought-marker",
            &[],
            Some(StreamWiring {
                chunks: tx,
                cancel: CancellationToken::new(),
                steer: None,
            }),
        )
        .await
        .expect("mock 思考+正文回合应 Done");
        let TurnEnd::Done { text, .. } = end else {
            panic!("未取消的回合应为 Done，实际 {end:?}");
        };
        assert_eq!(text, "正文一号正文二号", "最终文本只有正文");

        // 同一条 sink：kind 与顺序都必须对（思考片在前，正文片在后）。
        let mut kinds = Vec::new();
        let mut thought = String::new();
        let mut body = String::new();
        while let Ok(c) = rx.try_recv() {
            kinds.push(c.kind);
            match c.kind {
                ChunkKind::Thought => thought.push_str(&c.text),
                ChunkKind::Text => body.push_str(&c.text),
            }
        }
        assert_eq!(thought, "先想A再想B再想C", "思考分片拼接应无损");
        assert_eq!(body, "正文一号正文二号", "正文分片拼接应无损");
        assert_eq!(
            kinds,
            vec![
                ChunkKind::Thought,
                ChunkKind::Thought,
                ChunkKind::Thought,
                ChunkKind::Text,
                ChunkKind::Text,
            ],
            "同一条通道按到达顺序转发，kind 不得串"
        );

        // 历史：助手轮分别存下正文与思考（思考进 `Turn.thought`）。
        let hist = store.read_history(&id);
        assert_eq!(hist.len(), 2, "历史应含 user + assistant: {hist:?}");
        assert_eq!(hist[1].text, "正文一号正文二号");
        assert_eq!(hist[1].thought, "先想A再想B再想C");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── E1/E2 流式累积/分段/去重/停止态（纯逻辑，GUI 与历史共用）──

    #[test]
    fn streaming_bubble_accumulates_in_order() {
        let mut b = StreamingBubble::start();
        assert!(b.push(ChunkKind::Text, "打", 1.0));
        assert!(b.push(ChunkKind::Text, "字", 1.1));
        assert!(!b.push(ChunkKind::Text, "", 1.2), "空 delta 不算变化");
        assert_eq!(b.shown(), "打字");
        assert!(b.is_active());
        assert!(!b.is_stopped());
    }

    /// E2：思考与正文分段互不混入，且段序 = 到达顺序（同类并段、换类开新段）。
    #[test]
    fn streaming_bubble_segments_thought_and_text_by_arrival_order() {
        let mut b = StreamingBubble::start();
        b.push(ChunkKind::Thought, "先想", 0.0);
        b.push(ChunkKind::Thought, "再想", 1.5);
        b.push(ChunkKind::Text, "正文", 2.0);
        b.push(ChunkKind::Thought, "补充", 3.0);
        b.push(ChunkKind::Text, "续写", 4.0);

        assert_eq!(b.thought(), "先想再想补充", "思考段按顺序拼接");
        assert_eq!(b.shown(), "正文续写", "正文不得掺进思考");
        assert!(b.has_thought());

        let kinds: Vec<ChunkKind> = b.segments().iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ChunkKind::Thought,
                ChunkKind::Text,
                ChunkKind::Thought,
                ChunkKind::Text
            ],
            "换类必须开新段，顺序即上屏顺序"
        );
        let texts: Vec<&str> = b.segments().iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["先想再想", "正文", "补充", "续写"]);
    }

    /// 思考耗时累计：在进行的最后一段用 `now`，更早的段用自身 `last`（不得被 now 重复放大）。
    #[test]
    fn streaming_bubble_thought_secs_accumulates() {
        let mut b = StreamingBubble::start();
        b.push(ChunkKind::Thought, "第一段", 0.0);
        b.push(ChunkKind::Thought, "…", 2.0); // 第一段 0.0→2.0
        b.push(ChunkKind::Text, "正文", 3.0);
        b.push(ChunkKind::Thought, "第二段", 10.0); // 进行中
        assert!((b.thought_secs(12.5) - 4.5).abs() < 1e-9, "2.0 + 2.5 = 4.5");
        b.stop();
        assert!(
            (b.thought_secs(99.0) - 2.0).abs() < 1e-9,
            "停止后不再按 now 增长"
        );
    }

    /// 耗时文案：<60s 用秒，≥60s 用分；负值夹到 0。
    #[test]
    fn format_elapsed_seconds_and_minutes() {
        assert_eq!(format_elapsed(0.0), "0.0s");
        assert_eq!(format_elapsed(3.24), "3.2s");
        assert_eq!(format_elapsed(12.0), "12.0s");
        assert_eq!(format_elapsed(59.9), "59.9s");
        assert_eq!(format_elapsed(60.0), "1.0m");
        assert_eq!(format_elapsed(95.0), "1.6m");
        assert_eq!(format_elapsed(-1.0), "0.0s");
    }

    /// E1 评审 B1 的回归：模型被整体重载（行数缩到比锚点小）后，锚点必须判为**失效**，
    /// 否则 `sync_stream_rows` 删不到行，随后每个 delta 会把累积全文整段重推（重复气泡）。
    /// 变异点：把 `>` 改成 `>=`，`stream_anchor_valid_at_exact_boundary` 即红。
    #[test]
    fn stream_anchor_stale_detects_reload_shrink() {
        assert!(
            stream_anchor_stale(10, 3),
            "起点越出当前行数 ⇒ 失效（模型被重载过）"
        );
        assert!(!stream_anchor_stale(usize::MAX, 3), "未锚定不是失效");
        assert!(!stream_anchor_stale(10, 12), "起点仍在行数内 ⇒ 未失效");
    }

    /// 边界：起点**恰等于**行数（空流式气泡刚好在末尾）是**合法**的，不是失效。
    #[test]
    fn stream_anchor_valid_at_exact_boundary() {
        assert!(!stream_anchor_stale(5, 5));
    }

    #[test]
    fn streaming_bubble_finish_identical_is_stable() {
        let mut b = StreamingBubble::start();
        b.push(ChunkKind::Text, "一样", 1.0);
        assert!(!b.finish("一样", 2.0), "文本未变 ⇒ 不必重渲染");
        assert_eq!(b.shown(), "一样");
    }

    /// 去重口径：回合结束时以最终文本为唯一权威**重建正文**（不一致时不拼接、不重复），
    /// 但**思考段原样保留**（最终文本里没有思考，不能顺手删掉）。
    #[test]
    fn streaming_bubble_finish_replaces_text_and_keeps_thought() {
        let mut b = StreamingBubble::start();
        b.push(ChunkKind::Thought, "想了一半", 0.0);
        b.push(ChunkKind::Text, "已流出的一部分", 1.0);
        assert!(b.finish("最终完整文本", 2.0));
        assert_eq!(b.shown(), "最终完整文本", "正文不得保留旧拼接");
        assert_eq!(b.thought(), "想了一半", "思考段必须保留");
        assert!(!b.is_active());
    }

    #[test]
    fn streaming_bubble_stop_keeps_generated_text() {
        let mut b = StreamingBubble::start();
        b.push(ChunkKind::Thought, "想到一半", 0.0);
        b.push(ChunkKind::Text, "生成到一半", 1.0);
        b.stop();
        assert_eq!(b.shown(), "生成到一半", "停止必须保留已生成正文");
        assert_eq!(b.thought(), "想到一半", "停止必须保留已生成思考");
        assert!(b.is_stopped());
        assert!(!b.is_active());
    }

    // ── E2 客户端打字机（后端整条吐文本时的补字口径）──

    /// 逐字推进：`shown` 恒为 `target` 的前缀；推到末尾即 done（推完立即收尾，再拍不变化）。
    #[test]
    fn typewriter_reveals_prefix_then_finishes() {
        let mut tw = Typewriter::new();
        tw.push("打字机");
        assert_eq!(tw.shown(), "", "未拍时不上屏");
        assert!(!tw.done());
        // pending=3 ⇒ step=1（max(1, 3/2) = 1），逐字。
        assert!(tw.tick());
        assert_eq!(tw.shown(), "打");
        assert!(tw.tick());
        assert!(tw.tick());
        assert_eq!(tw.shown(), "打字机");
        assert!(tw.done(), "推完即到收尾点");
        assert!(!tw.tick(), "收尾后不再变化");
    }

    /// 大段追赶：积压越多每拍推进越多，但**每拍封顶 `STEP_CAP`**（否则大段一拍蹦出来、
    /// 看不见打字机——owner 2026-10-16 反馈）。
    #[test]
    fn typewriter_catches_up_big_chunks_within_few_ticks() {
        let mut tw = Typewriter::new();
        tw.push(&"字".repeat(200));
        let mut ticks = 0;
        while !tw.done() && ticks < 100 {
            tw.tick();
            ticks += 1;
        }
        assert!(tw.done());
        assert!(
            ticks <= 60,
            "200 字应在 ≤60 拍内追平（每拍封顶 4 字），实际 {ticks}"
        );
        assert_eq!(tw.shown().chars().count(), 200);
    }

    /// 停止/收尾用 `flush` 一次性显示已生成的全部文本 ⇒ 不丢字。
    #[test]
    fn typewriter_flush_reveals_everything() {
        let mut tw = Typewriter::new();
        tw.push("生成到一半的回复");
        tw.tick();
        assert!(tw.flush());
        assert_eq!(tw.shown(), "生成到一半的回复");
        assert!(tw.done());
        assert!(!tw.flush(), "已追平再 flush 无变化");
    }

    /// 收尾去重：以权威全文 `set_target`；旧显示是前缀则保留，不是则清空（不显示错位字节）。
    #[test]
    fn typewriter_set_target_is_authoritative() {
        let mut tw = Typewriter::new();
        tw.push("前缀");
        tw.tick();
        assert_eq!(tw.shown(), "前");
        tw.set_target("前缀被规范化后的全文");
        assert_eq!(tw.shown(), "前", "旧显示仍为新目标前缀 ⇒ 保留");
        // 完全不搭界的新目标：旧显示必须清空。
        tw.set_target("完全不同的文本");
        assert_eq!(tw.shown(), "", "非前缀必须清零，避免显示不属于目标的字");
    }

    /// 多字节：逐字推进按 char 切，不产生 UTF-8 半个字符（不 panic、不丢字）。
    #[test]
    fn typewriter_multibyte_boundaries() {
        let mut tw = Typewriter::new();
        tw.push("怹🟦好");
        while tw.tick() {}
        assert_eq!(tw.shown(), "怹🟦好");
    }

    // ── A 批：会话 store（用临时 base，不碰真实 ~/.agent-bridge）──

    #[test]
    fn derive_title_truncates_and_falls_back() {
        assert_eq!(derive_title("  hello\nworld  "), "hello world");
        assert_eq!(derive_title("   "), "新会话");
        assert_eq!(derive_title(&"字".repeat(40)).chars().count(), 24);
    }

    fn tmp_store(tag: &str) -> (SessionStore, std::path::PathBuf) {
        let base =
            std::env::temp_dir().join(format!("abb-syschat-store-{tag}-{}", uuid::Uuid::new_v4()));
        (SessionStore::new_at(base.clone()), base)
    }

    /// B1 回归守卫（卡 `abb-proc-sysctl-nb-20261028`，评审 154 finding 5）：
    /// `SessionStore::save` 必须用 uuid 唯一 tmp 名，不能预放符号链接。
    ///
    /// 为什么必须有这条：把 `save` 退回固定 `sessions.json.tmp` + `fs::write` 后，
    /// 完整 `--bin agent-bridge` 套件（1360 条）**仍然全绿**——这个漏洞类的修复
    /// 在 syschat / syschat_sched 上零回归覆盖，只有临时探针能抓到。此用例把它收进仓。
    #[test]
    fn session_store_save_does_not_follow_preseeded_symlink() {
        let (store, base) = tmp_store("symlink-save");
        std::fs::create_dir_all(&base).unwrap();
        let victim = base.join("victim.txt");
        std::fs::write(&victim, "ORIGINAL").unwrap();
        // 旧实现会跟随这条链接（fs::write = O_CREAT|O_TRUNC）
        std::os::unix::fs::symlink(&victim, base.join("sessions.json.tmp")).unwrap();

        let mut st = store.load();
        store.ensure_current(&mut st);
        store.save(&st).unwrap();

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "ORIGINAL",
            "预置的 sessions.json.tmp 符号链接不得被跟随（否则工作区外文件被覆盖）"
        );
        // 真内容要落到位（不是靠“写失败”通过的）
        assert!(
            base.join("sessions.json").exists(),
            "sessions.json 应正常落盘"
        );
        // 顺带钉住 0o600（旧代码显式 set_permissions，现在靠 atomic_write_sensitive）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(base.join("sessions.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "会话文件必须保持 0o600，实得 {mode:o}");
        }
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn session_store_new_list_switch_and_isolation() {
        let (store, base) = tmp_store("iso");
        let mut st = store.load();
        assert!(store.ensure_current(&mut st), "首个会话应被创建");
        assert_eq!(st.sessions.len(), 1);
        let id1 = st.current.clone();
        assert_eq!(st.sessions[0].title, "新会话");
        store.save(&st).unwrap();

        // 追加一轮 → 只落在 id1
        store
            .append_turns(&id1, &[turn_of("user", "hi"), turn_of("assistant", "yo")])
            .unwrap();
        assert_eq!(store.read_history(&id1).len(), 2);

        // 第二个会话：历史互不串
        let id2 = "s2-test".to_string();
        st.sessions.push(SessionMeta {
            id: id2.clone(),
            title: "第二个".into(),
            created_at: 1,
            updated_at: 1,
            provider: "prov-b".into(),
            archived: false,
            context_tokens: 0,
        });
        st.current = id2.clone();
        st.sessions[0].updated_at = 2; // 让 id1 更「新」，校验排序
        store.save(&st).unwrap();
        store
            .append_turns(&id2, &[turn_of("user", "second")])
            .unwrap();
        assert_eq!(store.read_history(&id1).len(), 2, "新会话不得串到旧会话");
        assert_eq!(store.read_history(&id2).len(), 1);

        // 列表：updated_at 倒序 + provider_of
        let (list, cur) = store.list();
        assert_eq!(cur, id2);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, id1, "updated_at 更大的排在前");
        assert_eq!(store.provider_of(&id2), "prov-b");
        assert_eq!(
            store.provider_of("nope"),
            "",
            "未知会话返回空（= 跟全局默认）"
        );

        // 清空只清该会话
        std::fs::remove_file(store.history_path(&id1)).unwrap();
        assert_eq!(store.read_history(&id1).len(), 0);
        assert_eq!(store.read_history(&id2).len(), 1, "清空不得殃及别的会话");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_store_migrates_legacy_history() {
        let (store, base) = tmp_store("mig");
        std::fs::create_dir_all(&base).unwrap();
        let legacy = format!(
            "{}\n{}\n",
            serde_json::to_string(&turn_of("user", "旧")).unwrap(),
            serde_json::to_string(&turn_of("assistant", "回复")).unwrap()
        );
        std::fs::write(store.legacy_path(), legacy).unwrap();
        let mut st = store.load();
        assert!(store.ensure_current(&mut st), "旧库应迁入默认会话");
        assert!(!store.legacy_path().exists(), "旧 history.jsonl 应被移走");
        assert_eq!(store.read_history(&st.current).len(), 2, "旧对话不得丢");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_id_whitelist() {
        assert!(is_valid_session_id("s123-abc_DEF"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("../etc/passwd"));
        assert!(!is_valid_session_id("a/b"));
        assert!(!is_valid_session_id(&"x".repeat(65)));
    }

    fn done_text(end: TurnEnd) -> String {
        match end {
            TurnEnd::Done { text, .. } => text,
            TurnEnd::Stopped => panic!("非取消回合不应是 Stopped"),
        }
    }

    /// `turn_core` 级两会话隔离：按会话读史→拼 prompt→落各会话，互不串。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn turn_core_isolates_sessions() {
        let (store, base) = tmp_store("turn");
        let mut st = store.load();
        store.ensure_current(&mut st);
        let s1 = st.current.clone();
        let s2 = "s2-turn".to_string();
        st.sessions.push(SessionMeta {
            id: s2.clone(),
            title: "b".into(),
            created_at: 1,
            updated_at: 1,
            provider: String::new(),
            archived: false,
            context_tokens: 0,
        });
        store.save(&st).unwrap();
        let ws = std::env::temp_dir().join(format!("abb-sc-turn-ws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let rec =
            |tag: &str| std::env::temp_dir().join(format!("{tag}-{}.jsonl", uuid::Uuid::new_v4()));

        let r1 = done_text(
            turn_core(
                &store,
                &s1,
                mock_agent_cfg(&rec("abb-sc-t1")),
                ws.clone(),
                "probe-one",
                &[],
                None,
            )
            .await
            .unwrap(),
        );
        assert!(r1.contains("probe-one"));
        let r2 = done_text(
            turn_core(
                &store,
                &s2,
                mock_agent_cfg(&rec("abb-sc-t2")),
                ws.clone(),
                "probe-two",
                &[],
                None,
            )
            .await
            .unwrap(),
        );
        assert!(r2.contains("probe-two"));

        let h1 = store.read_history(&s1);
        let h2 = store.read_history(&s2);
        assert_eq!(h1.len(), 2, "会话1 应有一问一答");
        assert_eq!(h2.len(), 2, "会话2 应有一问一答");
        assert!(h1.iter().any(|t| t.text.contains("probe-one")));
        assert!(
            !h1.iter().any(|t| t.text.contains("probe-two")),
            "会话1 不得串到会话2：{h1:?}"
        );
        assert!(h2.iter().any(|t| t.text.contains("probe-two")));
        assert!(
            !h2.iter().any(|t| t.text.contains("probe-one")),
            "会话2 不得串到会话1：{h2:?}"
        );
        // 标题取首条用户消息
        let (list, _) = store.list();
        assert_eq!(
            list.iter().find(|s| s.id == s1).unwrap().title,
            "probe-one",
            "会话标题应取首条用户消息"
        );

        // **读路径承重**：再各问一轮 → prompt 里必须带**自己**的历史、不带对方的（
        // 否则 `read_history` 按固定 id / 错会话拿到历史也能让上面的断言全绿）。
        let r1b = done_text(
            turn_core(
                &store,
                &s1,
                mock_agent_cfg(&rec("abb-sc-t3")),
                ws.clone(),
                "again-one",
                &[],
                None,
            )
            .await
            .unwrap(),
        );
        assert!(
            r1b.contains("probe-one"),
            "s1 第二轮须读到自己第一轮的历史：{r1b}"
        );
        assert!(
            !r1b.contains("probe-two"),
            "s1 第二轮不得读到 s2 的历史：{r1b}"
        );
        let r2b = done_text(
            turn_core(
                &store,
                &s2,
                mock_agent_cfg(&rec("abb-sc-t4")),
                ws.clone(),
                "again-two",
                &[],
                None,
            )
            .await
            .unwrap(),
        );
        assert!(
            r2b.contains("probe-two"),
            "s2 第二轮须读到自己第一轮的历史：{r2b}"
        );
        assert!(
            !r2b.contains("probe-one"),
            "s2 第二轮不得读到 s1 的历史：{r2b}"
        );

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&ws);
    }

    // ── C 批：附件（落盘/清洗/去重 + prompt 注入）──

    /// 临时源目录 + 附件目标目录（全部在系统临时目录，不碰真实 `~/.agent-bridge`）。
    fn tmp_attach_dirs(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "abb-sc-att-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        (src, dst)
    }

    fn write_src(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn land_attachments_copies_with_ts_prefix_and_bytes() {
        let (src, dst) = tmp_attach_dirs("copy");
        let a = write_src(&src, "报告 v2.pdf", "hello-attachment");
        let landed = land_attachments_in(&dst, &[a]).expect("应落地成功");
        assert_eq!(landed.len(), 1);
        let att = &landed[0];
        assert!(att.path.is_absolute(), "进 prompt 的必须是绝对路径");
        assert_eq!(att.path.parent().unwrap(), dst.as_path());
        // 文件名 = <unix-ts>-<sanitized>，且是同一次调用拿到的同一时间戳前缀
        let (ts_str, rest) = att.name.split_once('-').expect("应有时间戳前缀");
        let ts: u64 = ts_str.parse().expect("前缀应为 unix 秒");
        assert!(ts > 0);
        assert_eq!(rest, "报告 v2.pdf", "干净名字应原样保留");
        assert_eq!(att.bytes, "hello-attachment".len() as u64);
        assert_eq!(
            std::fs::read_to_string(&att.path).unwrap(),
            "hello-attachment"
        );
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    #[test]
    fn land_attachments_dedups_same_name_with_dash_n() {
        let (src, dst) = tmp_attach_dirs("dedup");
        // 三个不同目录里的同名文件（内容各异）——模拟「一次多选里挑了几份同名文件」。
        let a = write_src(&src.join("one"), "dup.png", "one");
        let b = write_src(&src.join("two"), "dup.png", "two");
        let c = write_src(&src.join("three"), "dup.png", "three");
        let landed = land_attachments_in(&dst, &[a, b, c]).expect("应落地成功");
        let names: Vec<&str> = landed.iter().map(|a| a.name.as_str()).collect();
        let stripped: Vec<String> = names
            .iter()
            .map(|n| n.split_once('-').unwrap().1.to_string())
            .collect();
        assert_eq!(stripped, vec!["dup.png", "dup-2.png", "dup-3.png"]);
        // 内容各自独立（不得互相覆盖）
        assert_eq!(std::fs::read_to_string(&landed[0].path).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&landed[1].path).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(&landed[2].path).unwrap(), "three");
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    #[test]
    fn land_attachments_sanitizes_name_and_keeps_extension() {
        let (src, dst) = tmp_attach_dirs("sanitize");
        // 换行/冒号/引号在 macOS 上可作文件名，但会被 prompt 的 `- <路径>` 行结构与
        // Windows 落盘弄坏；控制字符同理，统一改成 `_`。
        let a = write_src(&src, "we\nird:na*me\".png", "x");
        let long_stem = "头".repeat(200);
        let b = write_src(&src, &format!("{long_stem}.mov"), "y");
        let landed = land_attachments_in(&dst, &[a, b]).expect("应落地成功");
        let n0 = landed[0].name.split_once('-').unwrap().1;
        assert_eq!(n0, "we_ird_na_me_.png", "非法字符应换成下划线");
        let n1 = landed[1].name.split_once('-').unwrap().1;
        assert!(n1.ends_with(".mov"), "超长截断必须保留扩展名：{n1}");
        assert!(
            n1.chars().count() <= MAX_ATTACH_NAME_CHARS,
            "截断后应在上限内：{n1}（{} 字）",
            n1.chars().count()
        );
        assert!(!n1.contains('\n'));
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    #[test]
    fn sanitize_attachment_name_edge_cases() {
        assert_eq!(sanitize_attachment_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_attachment_name("  hi.txt  "), "hi.txt");
        assert_eq!(sanitize_attachment_name(".hidden"), "hidden");
        assert_eq!(sanitize_attachment_name(".."), "attachment");
        assert_eq!(sanitize_attachment_name(""), "attachment");
        assert_eq!(sanitize_attachment_name("...."), "attachment");
        // 无扩展名的超长名也要截（不得整体保留）
        let long = "a".repeat(MAX_ATTACH_NAME_CHARS + 50);
        assert_eq!(
            sanitize_attachment_name(&long).chars().count(),
            MAX_ATTACH_NAME_CHARS
        );
    }

    #[test]
    fn land_attachments_errors_on_missing_source_and_rolls_back() {
        let (src, dst) = tmp_attach_dirs("rollback");
        let good = write_src(&src, "good.txt", "ok");
        let missing = src.join("no-such-file.bin");
        let err = land_attachments_in(&dst, &[good, missing]).expect_err("缺文件必须报错");
        assert!(err.contains("no-such-file.bin"), "错误应带可读原因：{err}");
        // 回滚：坏文件导致整批失败 ⇒ 之前复制成功的那个不得留下
        let left: Vec<_> = std::fs::read_dir(&dst)
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "整批失败应回滚已落盘文件：{left:?}");
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    #[test]
    fn land_attachments_rejects_directory_and_empty_input_is_ok() {
        let (src, dst) = tmp_attach_dirs("dir");
        let err = land_attachments_in(&dst, std::slice::from_ref(&src)).expect_err("目录必须报错");
        assert!(err.contains("不支持目录"), "{err}");
        assert!(
            land_attachments_in(&dst, &[]).unwrap().is_empty(),
            "取消（空输入）是 Ok 空向量，不是错误"
        );
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
    }

    /// 无附件时必须**逐字节**等于既有 `build_prompt`（C 批不得改变既有语义）。
    #[test]
    fn prompt_without_attachments_is_byte_identical_to_build_prompt() {
        let h = vec![
            turn_of("user", "你好"),
            turn_of("assistant", "你好，有什么可以帮你？"),
        ];
        assert_eq!(
            prompt_with_attachments(&h, "帮我看看磁盘", &[]),
            build_prompt(&h, "帮我看看磁盘")
        );
        assert_eq!(
            prompt_with_attachments(&[], "第一条", &[]),
            build_prompt(&[], "第一条")
        );
    }

    #[test]
    fn prompt_with_attachments_appends_absolute_paths() {
        let atts = vec![
            Attachment {
                name: "1-a.png".into(),
                path: PathBuf::from("/tmp/x/1-a.png"),
                bytes: 1,
            },
            Attachment {
                name: "1-b.mov".into(),
                path: PathBuf::from("/tmp/x/1-b.mov"),
                bytes: 2,
            },
        ];
        let p = prompt_with_attachments(&[], "看这两个", &atts);
        assert!(p.contains("## 附件（本地路径，自行读取）"), "缺附件段：{p}");
        assert!(p.contains("\n- /tmp/x/1-a.png\n"), "缺第一条路径：{p}");
        assert!(p.contains("\n- /tmp/x/1-b.mov\n"), "缺第二条路径：{p}");
        // 附件段必须在用户消息之后（逐行 `- <绝对路径>`）
        assert!(p.find("看这两个").unwrap() < p.find("- /tmp/x/1-a.png").unwrap());
        // 无附件时不得出现该段（负向：把空分支写错就会红）
        assert!(!prompt_with_attachments(&[], "看这两个", &[]).contains("## 附件"));
    }

    /// 全链路：附件路径真的随 prompt 进 agent（mock 把 prompt 原样 echo 回来）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn turn_core_sends_attachment_paths_in_prompt() {
        let (store, base) = tmp_store("att");
        let mut st = store.load();
        store.ensure_current(&mut st);
        let sid = st.current.clone();
        store.save(&st).unwrap();
        let ws = std::env::temp_dir().join(format!("abb-sc-att-ws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let rec = std::env::temp_dir().join(format!("abb-sc-att-{}.jsonl", uuid::Uuid::new_v4()));
        let att = Attachment {
            name: "7-shot.png".into(),
            path: PathBuf::from("/tmp/att-marker/shot.png"),
            bytes: 3,
        };
        let reply = done_text(
            turn_core(
                &store,
                &sid,
                mock_agent_cfg(&rec),
                ws.clone(),
                "看这张",
                std::slice::from_ref(&att),
                None,
            )
            .await
            .unwrap(),
        );
        assert!(
            reply.contains("/tmp/att-marker/shot.png"),
            "附件路径必须进 prompt 并被 echo：{reply}"
        );
        // 历史仍只记用户输入的文本（附件不进历史重放）
        let hist = store.read_history(&sid);
        let user_turn = hist.iter().find(|t| t.role == "user").unwrap();
        assert_eq!(user_turn.text, "看这张");
        assert!(!user_turn.text.contains("shot.png"));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&ws);
    }

    // ── D 批：待发附件 3+N 折叠（纯逻辑）──

    /// 分组口径：折叠态最多 3 个、其余为 `+N`；`N == 0` 不渲染 `+N`。
    #[test]
    fn pending_view_folds_to_three_and_reports_remainder() {
        let s = PendingState::new();
        // 0 个：都不渲染，N=0
        let v0 = s.view(0);
        assert_eq!((v0.shown, v0.hidden), (0, 0));
        assert!(!v0.has_more());
        // 恰好 3 个：全展示，N=0（**不得**渲染 +N）
        let v3 = s.view(3);
        assert_eq!((v3.shown, v3.hidden), (3, 0));
        assert!(!v3.has_more(), "N==0 不得渲染 +N");
        // 5 个：前 3 + 余 2
        let v5 = s.view(5);
        assert_eq!((v5.shown, v5.hidden), (3, 2));
        assert!(v5.has_more());
        // 2 个：全展示，N=0
        assert_eq!((s.view(2).shown, s.view(2).hidden), (2, 0));
    }

    /// 展开看全部、可再收起。
    #[test]
    fn pending_expand_shows_all_then_collapses_back() {
        let mut s = PendingState::new();
        assert!(!s.is_expanded());
        assert_eq!(
            s.view(7),
            PendingView {
                shown: 3,
                hidden: 4
            }
        );
        s.toggle();
        assert!(s.is_expanded());
        assert_eq!(
            s.view(7),
            PendingView {
                shown: 7,
                hidden: 0
            },
            "展开后全部可见（+N 消失）"
        );
        s.toggle();
        assert!(!s.is_expanded());
        assert_eq!(
            s.view(7),
            PendingView {
                shown: 3,
                hidden: 4
            },
            "收起后回到折叠态"
        );
    }

    /// 按索引移除（越界 no-op）后按**新长度**重新分组。
    #[test]
    fn remove_pending_at_regroups_after_removal() {
        let mut list = vec!["a", "b", "c", "d", "e"];
        let s = PendingState::new();
        assert_eq!(remove_pending_at(&mut list, 1), 4);
        assert_eq!(list, vec!["a", "c", "d", "e"]);
        assert_eq!(
            s.view(list.len()),
            PendingView {
                shown: 3,
                hidden: 1
            }
        );
        // 越界 no-op
        assert_eq!(remove_pending_at(&mut list, 99), 4);
        assert_eq!(list.len(), 4);
        // 删到剩 3 个 ⇒ N=0
        remove_pending_at(&mut list, 0);
        assert_eq!(
            s.view(list.len()),
            PendingView {
                shown: 3,
                hidden: 0
            }
        );
        assert!(!s.view(list.len()).has_more());
        // 展开态下移除后仍全部可见
        let mut s2 = PendingState::new();
        s2.toggle();
        let mut l2 = vec![1, 2, 3, 4];
        remove_pending_at(&mut l2, 0);
        assert_eq!(
            s2.view(l2.len()),
            PendingView {
                shown: 3,
                hidden: 0
            }
        );
    }

    /// 图片判定与类型角标。
    #[test]
    fn attachment_kind_detection() {
        assert!(is_image_attachment("123-shot.PNG"), "大小写不敏感");
        assert!(is_image_attachment("a.jpeg"));
        assert!(!is_image_attachment("doc.pdf"));
        assert!(!is_image_attachment("movie.mp4"), "音视频走文件 chip");
        assert!(!is_image_attachment("noext"));
        assert_eq!(attachment_kind_label("123-设计规范.pdf"), "PDF");
        assert_eq!(attachment_kind_label("clip.mp4"), "MP4");
        assert_eq!(attachment_kind_label("noext"), "FILE");
        assert_eq!(attachment_kind_label(".hidden"), "FILE");
    }

    // ── D 批：对话框锚定/翻转几何（纯函数）──

    /// 默认首选左侧，且间距恰为 `BUBBLE_GAP`。
    #[test]
    fn anchor_prefers_left_with_exact_gap() {
        let bubble = (1000, 120);
        let bs = (36, 36);
        let ws = (460, 560);
        let work = (0, 0, 1440, 900);
        let (x, y) = anchor_window_pos(bubble, bs, ws, work);
        assert_eq!(x, 1000 - BUBBLE_GAP - 460);
        assert_eq!(y, 120);
        assert_eq!(bubble.0 - (x + ws.0), BUBBLE_GAP, "间距恰 10px");
    }

    /// 左缘**取等号**（窗口恰贴工作区左界）仍用左侧、不翻；再越一点就翻右侧且间距仍为 10。
    #[test]
    fn anchor_flip_boundary_takes_equality_as_fits() {
        let bs = (36, 36);
        let ws = (460, 560);
        let work = (0, 0, 1440, 900);
        let bx = BUBBLE_GAP + 460; // left_x == 0（取等号）
        let (x, _) = anchor_window_pos((bx, 0), bs, ws, work);
        assert_eq!(x, 0, "取等号算放得下，不翻");
        assert_eq!(bx - (x + ws.0), BUBBLE_GAP);
        // 再左一像素：放不下 ⇒ 翻到右侧
        let (x2, _) = anchor_window_pos((bx - 1, 0), bs, ws, work);
        assert_eq!(x2, (bx - 1) + bs.0 + BUBBLE_GAP);
        assert_eq!(x2 - ((bx - 1) + bs.0), BUBBLE_GAP, "翻边后间距仍 10");
    }

    /// 垂直：与气泡顶对齐，但不得越出工作区上下界。
    #[test]
    fn anchor_clamps_vertically_into_work_area() {
        let bs = (36, 36);
        let ws = (460, 560);
        let work = (0, 0, 1440, 900);
        // 气泡在底部：窗口下缘越界 ⇒ 上移贴底
        let (_, y) = anchor_window_pos((1000, 850), bs, ws, work);
        assert_eq!(y, 900 - 560);
        // 气泡在顶部之外（负 y）⇒ 夹到工作区上界
        let (_, y2) = anchor_window_pos((1000, -30), bs, ws, work);
        assert_eq!(y2, 0);
    }

    /// 两侧都放不下（窗口比可用空间还宽）时，宁可牺牲间距也必须不出屏。
    #[test]
    fn anchor_keeps_window_on_screen_when_both_sides_overflow() {
        let (x, _) = anchor_window_pos((500, 10), (36, 36), (1400, 300), (0, 0, 1440, 900));
        assert_eq!(x, 1440 - 1400);
        assert!(x >= 0 && x + 1400 <= 1440, "必须夹在工作区内：x={x}");
    }

    /// 工作区坐标可带偏移（副屏在主屏左侧时 x 为负）。
    #[test]
    fn anchor_respects_offset_work_area() {
        let work = (-1920, 0, 1920, 1080);
        let (x, y) = anchor_window_pos((-100, 100), (36, 36), (460, 560), work);
        assert_eq!(x, -100 - BUBBLE_GAP - 460, "左侧放得下就用左侧");
        assert_eq!((-100) - (x + 460), BUBBLE_GAP);
        assert_eq!(y, 100);
    }

    // ── D4：气泡吸附窗口四角（卡 abb-syschat-d4-bubble-follow）──

    const BS: (i32, i32) = (36, 36);
    const WORK: (i32, i32, i32, i32) = (0, 0, 1440, 900);

    /// 每个角的目标位：左/右角与窗口**横向**间隔恰为 `BUBBLE_GAP`；上/下角与对应边对齐。
    #[test]
    fn corner_targets_have_exact_gap_and_edge_alignment() {
        let win = (400, 200, 520, 560);
        let (x, y) = corner_bubble_pos(BubbleCorner::TopLeft, win, BS);
        assert_eq!(x, 400 - BUBBLE_GAP - BS.0, "左角在窗口左缘外 10px");
        assert_eq!(y, 200, "上角对齐窗口上缘");
        let (x, y) = corner_bubble_pos(BubbleCorner::TopRight, win, BS);
        assert_eq!(x, 400 + 520 + BUBBLE_GAP, "右角在窗口右缘外 10px");
        assert_eq!(y, 200);
        let (x, y) = corner_bubble_pos(BubbleCorner::BottomLeft, win, BS);
        assert_eq!(x, 400 - BUBBLE_GAP - BS.0);
        assert_eq!(
            y,
            200 + 560 - BS.1,
            "下角对齐窗口下缘（气泡下边齐窗口下边）"
        );
        let (x, y) = corner_bubble_pos(BubbleCorner::BottomRight, win, BS);
        assert_eq!(x, 400 + 520 + BUBBLE_GAP);
        assert_eq!(y, 200 + 560 - BS.1);
    }

    /// 四角择优：气泡贴哪一角，就选哪一角（四个方向各测一次，证不是恒选一个角）。
    #[test]
    fn nearest_corner_picks_each_corner_it_is_closest_to() {
        let win = (400, 200, 520, 560);
        // 目标位：TL=(354,200) TR=(930,200) BL=(354,724) BR=(930,724)
        assert_eq!(
            nearest_corner(win, (354, 200), BS, WORK),
            BubbleCorner::TopLeft
        );
        assert_eq!(
            nearest_corner(win, (930, 200), BS, WORK),
            BubbleCorner::TopRight
        );
        assert_eq!(
            nearest_corner(win, (354, 724), BS, WORK),
            BubbleCorner::BottomLeft
        );
        assert_eq!(
            nearest_corner(win, (930, 724), BS, WORK),
            BubbleCorner::BottomRight
        );
    }

    /// 并列时取 `ALL` 里靠前的角（确定性）：TL 与 TR 目标位的垂直中分线上取 TL；
    /// 竖直方向同理取 TL 而不是 BL。
    ///
    /// 判别力：把 `nearest_corner` 的比较从 `d < best_d` 改成 `d <= best_d` ⇒ 本用例红
    /// （后者取 `ALL` 里靠后的角）。
    #[test]
    fn nearest_corner_breaks_ties_by_declared_order() {
        // 目标位 TL=(-46,0) TR=(110,0)，x=(−46+110)/2 = 32 时两者等距。
        let win = (0, 0, 100, 100);
        assert_eq!(
            nearest_corner(win, (32, 0), BS, WORK),
            BubbleCorner::TopLeft
        );
        // 竖直方向同理：TL=(-46,0) BL=(-46,64)，y=(0+64)/2 = 32 时等距 ⇒ 取更靠前的 TL。
        assert_eq!(
            nearest_corner(win, (-46, 32), BS, WORK),
            BubbleCorner::TopLeft
        );
    }

    /// 吸附落位：确实选最近角，且横向恰为 10px 间距（贴着角、不出屏时）。
    #[test]
    fn snap_places_bubble_outside_the_nearest_corner() {
        let win = (400, 200, 520, 560);
        // 气泡在窗口右下外侧 ⇒ 选 BR。
        let (corner, (x, y)) = snap_bubble_to_window(win, (930, 724), BS, WORK);
        assert_eq!(corner, BubbleCorner::BottomRight);
        assert_eq!(x, 400 + 520 + BUBBLE_GAP);
        assert_eq!(y, 724);
        assert_eq!(x - (400 + 520), BUBBLE_GAP, "间距恰 10px");
        // 气泡在窗口左上外侧 ⇒ 选 TL，间距同样恰 10px。
        let (corner, (x, _)) = snap_bubble_to_window(win, (354, 200), BS, WORK);
        assert_eq!(corner, BubbleCorner::TopLeft);
        assert_eq!((400) - (x + BS.0), BUBBLE_GAP);
    }

    /// **幂等**（防振荡的关键）：对同一窗口/工作区，把吸附结果再喂回去，结果与角都不变。
    ///
    /// 覆盖：贴角正常位、被夹到屏幕角/边、窗口比屏还大。
    /// 判别力：若把 `nearest_corner` 改成按「角顶点」比距离，窗口极窄等退化输入下会翻角 ⇒ 本用例红。
    #[test]
    fn snap_is_idempotent() {
        // 三个元组拼在一起超出了 clippy 的复杂度阈值（`type_complexity`）。这里用**本函数内**
        // 的别名收口：生产签名 `snap_bubble_to_window(window: (i32,i32,i32,i32), …)` 本次不动
        // （那是独立的可读性问题，不该夹在「清 clippy 基线」的提交里顺手改公共 API）。
        type RectI = (i32, i32, i32, i32);
        type PointI = (i32, i32);
        let cases: &[(RectI, PointI, RectI)] = &[
            ((400, 200, 520, 560), (930, 724), WORK),
            ((400, 200, 520, 560), (354, 200), WORK),
            ((0, 0, 520, 560), (-20, -20), WORK),
            ((1000, 800, 520, 560), (1500, 1400), WORK),
            ((0, 0, 2000, 900), (700, 700), WORK), // 窗口比屏宽
            ((-1700, 200, 520, 560), (-1230, 200), (-1920, 0, 1920, 1080)), // 副屏（负原点）
        ];
        for &(win, b, work) in cases {
            let (c1, p1) = snap_bubble_to_window(win, b, BS, work);
            let (c2, p2) = snap_bubble_to_window(win, p1, BS, work);
            // 位置必须是**严格不动点**（防振荡的承重性质）。
            assert_eq!(
                p1, p2,
                "吸附结果必须是不动点：win={win:?} b={b:?} work={work:?}"
            );
            // 夹取先于择优 ⇒ 第二次也不翻角（判别力：把夹取移到 `nearest_corner` 之后 ⇒
            // 「窗口贴屏边」的用例会翻角，本断言红）。
            assert_eq!(c1, c2, "吸附后角不许翻：win={win:?} b={b:?}");
        }
        // 确定性：同一输入重复调用总得到同一结果（分两个变量，避开 clippy::eq_op 的同式比较）。
        for &(win, b, work) in cases {
            let first = snap_bubble_to_window(win, b, BS, work);
            let second = snap_bubble_to_window(win, b, BS, work);
            assert_eq!(first, second, "同一输入必须稳定：win={win:?} b={b:?}");
        }
    }

    /// **开窗位的不动点**（防反复开关漂移）：`anchor_window_pos`（气泡 ⇒ 窗口）算出的窗口，
    /// 再用 `snap_bubble_to_window`（窗口 ⇒ 气泡）反算，气泡留在原地。
    ///
    /// 两个分支都测：首选左侧 + 左缘放不下翻右侧。
    /// 判别力：把 `corner_bubble_pos` 的纵向改成「再外推 10px」（对角偏移）⇒ 本用例红，
    /// 且真机上表现为每次开关对话窗气泡向上飘一个气泡高。
    #[test]
    fn snap_is_fixed_point_of_open_anchor() {
        let ws = (460, 560);
        // 分支 1：窗口在气泡左侧。
        let bubble = (1000, 120);
        let (wx, wy) = anchor_window_pos(bubble, BS, ws, WORK);
        let (_, back) = snap_bubble_to_window((wx, wy, ws.0, ws.1), bubble, BS, WORK);
        assert_eq!(back, bubble, "开窗位不被吸附改动（窗口在左）");
        assert_eq!(
            nearest_corner((wx, wy, ws.0, ws.1), bubble, BS, WORK),
            BubbleCorner::TopRight
        );
        // 分支 2：气泡贴左缘 ⇒ 窗口翻到气泡右侧。
        let bubble2 = (0, 120);
        let (wx2, wy2) = anchor_window_pos(bubble2, BS, ws, WORK);
        assert_eq!(wx2, BS.0 + BUBBLE_GAP, "前提：确实翻了右侧");
        let (_, back2) = snap_bubble_to_window((wx2, wy2, ws.0, ws.1), bubble2, BS, WORK);
        assert_eq!(back2, bubble2, "开窗位不被吸附改动（窗口在右）");
        assert_eq!(
            nearest_corner((wx2, wy2, ws.0, ws.1), bubble2, BS, WORK),
            BubbleCorner::TopLeft
        );
    }

    /// 屏幕夹取：目标位越出工作区时，夹回区内（不出屏优先，牺牲 10px 间距）。
    ///
    /// 判别力：去掉 `snap_bubble_to_window` 里任一 `if x > max_x` / `if x < wx` / 同款 y 分支
    /// ⇒ 对应断言红。
    #[test]
    fn snap_clamps_bubble_into_work_area() {
        let win = (0, 0, 520, 560);
        // 气泡在最右下外侧 ⇒ BR 目标位 (530, 524) 仍在屏内，取原值。
        let (_, (x, y)) = snap_bubble_to_window(win, (530, 524), BS, WORK);
        assert_eq!((x, y), (530, 524));
        // 窗口贴右/下缘时目标位出屏 ⇒ 夹回工作区内。
        let win2 = (1000, 400, 520, 560); // 右缘 1520 > 1440；下缘 960 > 900
        let (_, (x2, y2)) = snap_bubble_to_window(win2, (1510, 950), BS, WORK);
        assert!(x2 <= 1440 - BS.0 && x2 >= 0, "x 必须在屏内：{x2}");
        assert!(y2 <= 900 - BS.1 && y2 >= 0, "y 必须在屏内：{y2}");
        assert_eq!(x2, 1440 - BS.0);
        assert_eq!(y2, 900 - BS.1);
    }

    /// 窗口比屏还大（最大化/超大窗）：仍返回工作区内的坐标，绝不越界，也不 panic。
    #[test]
    fn snap_keeps_bubble_on_screen_when_window_bigger_than_screen() {
        let win = (-300, -300, 3000, 2000);
        for b in [(0, 0), (1440, 900), (700, 700), (-500, -500)] {
            let (_, (x, y)) = snap_bubble_to_window(win, b, BS, WORK);
            assert!(
                x >= WORK.0 && x + BS.0 <= WORK.0 + WORK.2,
                "x 越界：b={b:?} -> {x}"
            );
            assert!(
                y >= WORK.1 && y + BS.1 <= WORK.1 + WORK.3,
                "y 越界：b={b:?} -> {y}"
            );
        }
    }

    /// 工作区带偏移（副屏在主屏左侧）：目标位与夹取都按工作区自身坐标系算。
    #[test]
    fn snap_respects_offset_work_area() {
        let work = (-1920, 0, 1920, 1080);
        let win = (-1700, 200, 520, 560);
        // 气泡在窗口右侧 ⇒ 右角目标位 = -1700+520+10 = -1170，未越界。
        let (corner, (x, y)) = snap_bubble_to_window(win, (-1230, 200), BS, work);
        assert_eq!(corner, BubbleCorner::TopRight);
        assert_eq!(x, -1170);
        assert_eq!(y, 200);
        // 气泡在窗口左侧 ⇒ 左角目标位 = -1700-10-36 = -1746。
        let (corner, (x, _)) = snap_bubble_to_window(win, (-1746, 300), BS, work);
        assert_eq!(corner, BubbleCorner::TopLeft);
        assert_eq!(x, -1746);
    }

    // ─────────── D2：标题栏控件条（稿 ①②③④）的落位 ───────────

    /// 计划里**画出来的项**的总宽（含相邻项之间的 `TB_ITEM_GAP`）；`x < 0`（整条不画）⇒ 0。
    fn plan_total(p: &TitlebarPlan) -> f32 {
        if p.x < 0.0 {
            return 0.0;
        }
        let mut total = 0.0;
        let mut first = true;
        for w in [p.new_w, p.session_w, p.model_w, p.settings_w] {
            if w > 0.0 {
                if !first {
                    total += TB_ITEM_GAP;
                }
                total += w;
                first = false;
            }
        }
        total
    }

    /// 控件条右缘必须停在**最左侧那颗真实窗口按钮**左边 ≥8px，且这条断言真的在盯按钮组几何。
    ///
    /// 判别力（负向，两个方向）：
    /// 1. 把 `TB_STRIP_GAP` / 任一控件宽度改成别的值 ⇒ 期望值用**字面量**算 ⇒ 必红；
    /// 2. 把按钮区按 2 颗算（`TB_BTN_ZONE_W` 缩到 70）⇒ 右缘越过按钮区 ⇒ 必红。
    #[test]
    fn titlebar_plan_reserves_room_for_all_buttons() {
        let plan = titlebar_plan(520.0, true);
        let strip_right = plan.x + plan_total(&plan);
        // 预留的是**整个按钮区**（字面量 106 + 右边距 4），不按「当前可见几颗按钮」精算：
        // 真机 340px 实测过按可见按钮数算会把 ③⚙ 压到最小化按钮的浅色按钮底上。
        let zone_left = 520.0 - 4.0 - 106.0;
        assert!(
            strip_right <= zone_left - 8.0 + 0.01,
            "控件条右缘 {strip_right} 越过按钮区左侧 {zone_left}（间隔应 ≥8）"
        );
        // 该区必须**装得下三颗按钮**（v10 起最大化已打开，见 .slint 的 `show-maximize: true`）：
        // 30×3 + 6×2 + 右距 4 = 106。
        let three = 3.0 * 30.0 + 2.0 * 6.0 + 4.0;
        assert!(
            TB_BTN_ZONE_W >= three,
            "预留区 {TB_BTN_ZONE_W} 装不下三颗按钮（需 ≥{three}）"
        );
        assert!(zone_left >= 0.0 && plan.x >= 0.0);
    }

    /// `show-maximize` 必须与 `TB_BTN_ZONE_W` 的「三颗按钮」预留一致（跨文件不变量；源码扫描，
    /// 同 `ui::tests::app_theme_applies_slint_pixel_soft_preset` 手法）。
    ///
    /// v10（稿 v8 第 3 条）**恢复最大化**：窗口按钮组是 `— ⤢ ✕` 三颗 ⇒ 常量必须按三颗留；
    /// 反过来，若有人把 `.slint` 改回 `show-maximize: false`，这条也会红（提醒同步口径）。
    #[test]
    fn titlebar_plan_button_count_matches_show_maximize() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        let start = raw
            .find("export component SystemChatWindow")
            .expect("ui/app.slint 里找不到 SystemChatWindow");
        let tail = &raw[start..];
        let end = tail
            .find("export component")
            .map(|i| {
                if i == 0 {
                    tail[1..]
                        .find("export component")
                        .map(|j| j + 1)
                        .unwrap_or(tail.len())
                } else {
                    i
                }
            })
            .unwrap_or(tail.len());
        let win = &tail[..end];
        assert!(
            win.contains("PixelTitleBar"),
            "SystemChatWindow 应画自绘标题栏"
        );
        let maximize_on = win.lines().any(|l| {
            let code = l.split("//").next().unwrap_or(l);
            code.contains("show-maximize: true")
        });
        assert!(
            maximize_on,
            "v10 起 SystemChatWindow 必须打开最大化（稿 v8 第 3 条：三钮 — ⤢ ✕）"
        );
        let three = 3.0 * 30.0 + 2.0 * 6.0 + 4.0;
        assert!(
            TB_BTN_ZONE_W >= three,
            "预留区必须按「最大化也打开」的 3 颗按钮留（{TB_BTN_ZONE_W} < {three}）"
        );
    }

    /// **每一个** `PixelTitleBar` 实例都必须把全部窗口控制回调转发到自己的根组件。
    ///
    /// D6 真机 bug（2026-10-14）：`SystemChatWindow` 的标题栏三钮里只接了
    /// `close-window => { root.close-window(); }`，`—`/`⤢` 两条**一条都没接** ——
    /// `PixelTitleBar` 是上游封闭组件，它只回调自己的 `minimize`/`toggle-maximize`，不会自动冒泡；
    /// 于是按钮画得出、按得下（`TouchArea` 的 grab 与按下态都正常），`root.minimize()` 却永远不被调用
    /// —— 用户看到的就是「布局在、点了没反应」。这类缺陷在源码扫描下就是「少了一行」，
    /// 在像素/几何里完全看不出来。
    ///
    /// 扫描面**故意取全文件**（两个窗口的每一条 `PixelTitleBar`）而不是只看系统对话窗：
    /// 修 D6 的过程中就真的顺手删错了一行（删 `minimize` 时删到了设置窗那一条，而当时只在
    /// SystemChatWindow 上盯着），所以覆盖范围必须按「谁用了这个组件」而不是按「谁在出 bug」。
    ///
    /// 判别力（负向）：删掉任一条转发（任一窗口的 `drag-start`/`minimize`/`toggle-maximize`/
    /// `close-window`）⇒ 本用例红。行为侧由
    /// `ui_headless_render_test::titlebar_window_buttons_receive_clicks`
    /// （事件注入到三颗按钮的真实坐标）守着 —— 两条一起才能证明「画出来的按钮 = 能用的按钮」。
    #[test]
    fn every_title_bar_forwards_all_window_callbacks() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        let raw = raw.as_ref(); // Cow → &str（下面要在它上面切块）
        let cb = ["drag-start", "minimize", "toggle-maximize", "close-window"];
        let mut seen = 0usize;
        let mut rest: &str = raw;
        while let Some(i) = rest.find("PixelTitleBar {") {
            let block_src = &rest[i..];
            // 只取这个 PixelTitleBar 块（花括号配对），免得把后面别处的接线算进来。
            let mut depth = 0i32;
            let mut end = block_src.len();
            for (j, c) in block_src.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = j;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let block = &block_src[..end];
            for name in cb {
                let expect = format!("{name} => {{ root.{name}(); }}");
                assert!(
                    block.contains(&expect),
                    "第 {} 个 PixelTitleBar 没转发 `{name}`（应写 `{expect}`）：\n\
                     上游组件不会自动冒泡，漏一条就是一颗「点了没反应」的窗口按钮",
                    seen + 1
                );
            }
            seen += 1;
            rest = &block_src[end..];
        }
        assert_eq!(
            seen, 2,
            "ui/app.slint 里应有 2 条 PixelTitleBar（设置窗 + 系统对话窗）"
        );
    }

    /// 输入区功能区的接线守卫：✂ 截图（卡 `abb-v11-screenshot` 起是真功能）/ 😊 表情 / ⋯ 更多
    /// （卡 d9）都接到 Rust。
    ///
    /// 源码扫描守卫（同 `titlebar_plan_button_count_matches_show_maximize` 的手法）：按下没反应 /
    /// 接了空实现属于「做了个空壳」，外部观测不到，只能把接线钉在源码上。
    ///
    /// 判别力（负向）：把任一接线改成空实现 / 删掉（例如去掉 `root.emoji-opened()`）⇒ 本用例红。
    #[test]
    fn composer_function_bar_wiring() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        // ✂ 截图：接 composer-todo 把采集交给 Rust。**两个名字必须逐字对上**——
        // 一个是 Slint 传的功能名、一个是 Rust 用来分派的常量；漂了就静默落进「占位」分支
        // （按钮看着正常，点了只回一句「尚未实现」）。
        assert!(
            raw.contains("root.composer-todo(\"截图\")"),
            "✂ 截图没接 composer-todo（会变成点了没反应的死按钮）"
        );
        assert!(
            raw.contains(&format!(
                "root.composer-todo(\"{}\")",
                crate::screenshot::ACTION_NAME
            )),
            "Slint 传的功能名与 `screenshot::ACTION_NAME` 不一致 ⇒ 截屏永远落进占位分支"
        );
        // Rust 侧真去调采集（而不是把名字分派完就没了）。
        let ui_src = crate::platform::src_lf(include_str!("ui.rs"));
        assert!(
            ui_src.contains("crate::screenshot::capture()"),
            "✂ 截图的 Rust 分支没调 `screenshot::capture()`"
        );
        // 😊 表情：打开面板 + 通知 Rust 重置/刷新（两者缺一都是空壳）
        assert!(
            raw.contains("root.emoji-open = true;"),
            "😊 表情没打开 emoji 面板"
        );
        assert!(
            raw.contains("root.emoji-opened();"),
            "😊 表情没通知 Rust 初始化面板内容（面板会是空的）"
        );
        // ⋯ 更多：打开菜单 + 通知 Rust 刷新档位/待发
        assert!(raw.contains("root.more-open = true;"), "⋯ 更多没打开菜单");
        assert!(
            raw.contains("root.more-opened();"),
            "⋯ 更多没通知 Rust 刷新档位/待发列表"
        );
        // Rust 侧的全部接线。
        let rust = crate::platform::src_lf(include_str!("ui.rs"));
        for hook in [
            "on_composer_todo",
            "on_emoji_opened",
            "on_emoji_cat",
            "on_emoji_page",
            "on_emoji_pick",
            "on_more_opened",
            "on_sched_preset",
            "on_sched_cancel",
            "on_sched_cancel_all",
        ] {
            assert!(rust.contains(hook), "Rust 侧没注册 {hook} —— 功能发不出来");
        }
        // 表情插入口径：Slint 的 TextInput 无光标 API ⇒ 追加到草稿**末尾**（本卡口径）。
        assert!(
            rust.contains("draft.push_str(e.as_str())"),
            "表情插入必须追加到草稿末尾（TextInput 无光标 API）"
        );
        // 定时发送复用既有主定时器：到点从存储取出并走与手动发送同一个 submit_sc。
        assert!(
            rust.contains("take_due(crate::schedule::DateTime::now())"),
            "定时到点没有接主定时器（take_due）"
        );
    }

    /// 默认窗口尺寸与**最小尺寸**常量必须等于 `ui/app.slint` 的四个布局约束（跨文件不变量）。
    ///
    /// - 窗口还没 realize（`size()` 为 0）时计划按默认宽算，漂移会让首帧按另一个尺寸落位；
    /// - `min-*` 是「自由缩放的下界」，也是「窗口不是固定尺寸」的证据：Slint 的 winit 后端把
    ///   `min` 交给系统（`set_min_inner_size`），若 `min == max` 反而会变成不可缩放
    ///   （`i-slint-backend-winit` 的 `window_is_resizable`）。
    ///
    /// 判别力：改任一常量、或改 `.slint` 里对应的那一行 ⇒ 本用例红。
    #[test]
    fn syschat_window_size_constants_match_slint() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        let head = &raw[raw
            .find("export component SystemChatWindow")
            .expect("ui/app.slint 里找不到 SystemChatWindow")..];
        // 只取该组件头部的约束行（第一个 `in property` 之前就是全部布局约束）。
        let head: String = head
            .lines()
            .take_while(|l| !l.trim_start().starts_with("in property"))
            .collect::<Vec<_>>()
            .join("\n");
        let pick = |key: &str| -> f32 {
            let line = head
                .lines()
                .find(|l| l.trim_start().starts_with(key))
                .unwrap_or_else(|| panic!("SystemChatWindow 缺少 {key}：\n{head}"));
            let code = line.split("//").next().unwrap_or(line).trim();
            code.trim_start_matches(key)
                .trim()
                .trim_end_matches(';')
                .trim_end_matches("px")
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("解析 {key} 失败：{code:?}"))
        };
        assert_eq!(pick("preferred-width:"), SYSCHAT_PREFERRED_WIDTH);
        assert_eq!(pick("preferred-height:"), SYSCHAT_DEFAULT_H);
        assert_eq!(pick("min-width:"), SYSCHAT_MIN_W);
        assert_eq!(pick("min-height:"), SYSCHAT_MIN_H);
        // 最小尺寸必须严格小于默认尺寸，否则窗口会退化成「固定尺寸」（拖不动）。
        // 用**编译期**断言而不是 `assert!`：两个操作数都是 `const`，运行期断言恒为真
        // ⇒ clippy `assertions_on_constants` 会判它没意义；而这条不变式本身是真约束，
        // 要在**它被破坏的那一刻**（改常量的人）报出来，不是等有人跑这条测试。
        const _: () = assert!(SYSCHAT_MIN_W < SYSCHAT_PREFERRED_WIDTH);
        const _: () = assert!(SYSCHAT_MIN_H < SYSCHAT_DEFAULT_H);
    }

    /// **设置窗**的最小尺寸常量必须等于 `ui/app.slint` 的 `SettingsWindow` 两行（跨文件不变量）。
    ///
    /// 为什么也要钉：设置窗的自管缩放（macOS）每拍用 [`SETTINGS_MIN_W`]/[`SETTINGS_MIN_H`]
    /// 夹下半界，而**系统**那条路（Windows/Linux）用的是 `.slint` 的 `min-width`/`min-height`
    /// ⇒ 两处漂了就会出现「同一台机器上，平台不同则最小尺寸不同」这种最难查的差异。
    ///
    /// 判别力：改任一边 ⇒ 本用例红。
    #[test]
    fn settings_window_min_constants_match_slint() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        let head = &raw[raw
            .find("export component SettingsWindow")
            .expect("ui/app.slint 里找不到 SettingsWindow")..];
        let pick = |key: &str| -> f32 {
            let line = head
                .lines()
                .find(|l| l.trim_start().starts_with(key))
                .unwrap_or_else(|| panic!("SettingsWindow 缺少 {key}"));
            let code = line.split("//").next().unwrap_or(line).trim();
            code.trim_start_matches(key)
                .trim()
                .trim_end_matches(';')
                .trim_end_matches("px")
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("解析 {key} 失败：{code:?}"))
        };
        assert_eq!(pick("min-width:"), SETTINGS_MIN_W);
        assert_eq!(pick("min-height:"), SETTINGS_MIN_H);
        // 与「初始尺寸」相比也必须更小，否则窗口退化成固定尺寸（拖不动）。
        let preferred_w = pick("preferred-width:");
        let preferred_h = pick("preferred-height:");
        assert!(
            SETTINGS_MIN_W < preferred_w,
            "min-width 必须 < preferred-width"
        );
        assert!(
            SETTINGS_MIN_H < preferred_h,
            "min-height 必须 < preferred-height"
        );
    }

    /// `SYSCHAT_PREFERRED_WIDTH` 必须等于 `ui/app.slint` 的 `preferred-width`
    /// （跨文件不变量；窗口未 realize 时计划就按它算，漂移会让首帧按另一个宽度落位）。
    /// 四个约束的统一口径见 [`syschat_window_size_constants_match_slint`]。
    #[test]
    fn syschat_preferred_width_matches_slint() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        let start = raw
            .find("export component SystemChatWindow")
            .expect("ui/app.slint 里找不到 SystemChatWindow");
        let line = raw[start..]
            .lines()
            .find(|l| l.trim_start().starts_with("preferred-width:"))
            .expect("SystemChatWindow 应有 preferred-width");
        let code = line.split("//").next().unwrap_or(line).trim();
        let num: f32 = code
            .trim_start_matches("preferred-width:")
            .trim()
            .trim_end_matches(';')
            .trim_end_matches("px")
            .parse()
            .unwrap_or_else(|_| panic!("解析 preferred-width 失败：{code:?}"));
        assert_eq!(
            num, SYSCHAT_PREFERRED_WIDTH,
            "SYSCHAT_PREFERRED_WIDTH 与 ui/app.slint 的 preferred-width 漂移了"
        );
    }

    /// 默认窗宽（`preferred-width`）下四项全上，右缘正好贴「按钮组左侧 − 8」；
    /// 兜底宽度 `SYSCHAT_PREFERRED_WIDTH` 下也必须四项都在（它是同一扇窗）。
    #[test]
    fn titlebar_plan_fits_all_when_wide_enough() {
        let p = titlebar_plan(520.0, true);
        assert_eq!(
            (p.new_w, p.session_w, p.model_w, p.settings_w),
            (TB_NEW_W, TB_SESSION_W, TB_MODEL_W, TB_SETTINGS_W)
        );
        // 左缘必须在标题文字预留（132 + 8）右边。
        assert!(
            p.x >= TB_TITLE_END + TB_TITLE_GAP,
            "x={} 会压到标题文字",
            p.x
        );
        // 右缘正好贴「按钮组左侧 − 8」：期望值用字面量算（不引用 TB_*），
        // 这样改 `TB_STRIP_GAP` / 控件宽度都会被本用例抳住。
        let zone_left = 520.0 - 4.0 - 106.0;
        assert_eq!(p.x + plan_total(&p), zone_left - 8.0);
        // 兜底宽度下也必须四项都在（前端常量已与 .slint 对齐，见上一条守卫）。
        let def = titlebar_plan(SYSCHAT_PREFERRED_WIDTH, true);
        assert_eq!(def.settings_w, TB_SETTINGS_W, "兜底宽度下 ⚙ 也该在");
    }

    /// 窗口变窄时按「能力可替代性」换档：先压模型 → 让 ⚙ → 让模型 → 让历史会话；
    /// **新建会话是唯一入口 ⇒ 永远在**（连它都放不下才整条不画），且任何一档都不许把控件
    /// 摆到标题文字或窗口按钮上（逐档校验不重叠）。
    #[test]
    fn titlebar_plan_degrades_without_overlapping() {
        // 每个宽度上：各档都不得越界，且宽度随窗宽变窄单调不减。
        let mut prev_total = f32::MAX;
        for w in [
            520.0, 460.0, 440.0, 420.0, 400.0, 380.0, 360.0, 340.0, 300.0, 280.0, 200.0, 100.0,
        ] {
            let p = titlebar_plan(w, true);
            let total = plan_total(&p);
            assert!(
                total <= prev_total + 0.01,
                "w={w}: 档位宽度应随窗宽变窄而非变宽（{total} > {prev_total}）"
            );
            prev_total = total;
            let zone_left = w - 4.0 - TB_BTN_ZONE_W;
            if p.x < 0.0 {
                assert_eq!(total, 0.0, "w={w}: x<0 即整条不画");
                // 只有连新建会话（26px）都放不下时才允许整条不画。
                let avail = zone_left - TB_STRIP_GAP - (TB_TITLE_END + TB_TITLE_GAP);
                assert!(avail < TB_NEW_W, "w={w}: 还有 {avail}px 空档却整条不画");
                continue;
            }
            assert!(
                p.x + total <= zone_left - TB_STRIP_GAP + 0.01,
                "w={w}: 压到窗口按钮区"
            );
            assert!(
                p.x >= TB_TITLE_END + TB_TITLE_GAP - 0.01,
                "w={w}: 压到标题文字"
            );
            // 新建会话是唯一入口（v10 已从输入区移除 `＋`）⇒ 只要整条画着就必在。
            assert_eq!(p.new_w, TB_NEW_W, "w={w}: 新建会话不许让位");
            if p.model_w > 0.0 {
                // 字面量上下界（不引用被验常量）：模型 chip 宽必须落在 88..=112
                assert!(
                    (88.0..=112.0).contains(&p.model_w),
                    "w={w}: 模型 chip 宽越界 {}",
                    p.model_w
                );
            }
            // 历史会话图标钮只按整档开合（26px），不随窗宽伸缩。
            assert!(
                p.session_w == 0.0 || p.session_w == 26.0,
                "w={w}: 历史会话钮只按整档开合，实测 {}",
                p.session_w
            );
        }
    }

    /// 换档取舍：
    /// - 380px（空档 122）：只够 new+session+⚙（90）；模型（≥88）放不下 ⇒ 走状态行回退位。
    /// - 340px（最小宽，空档 82）：连 ⚙ 也让掉 ⇒ 只剩 new+session（58）。
    /// - 300px（空档 42）：只剩新会话钮 —— 它是唯一入口，**不**整条让位。
    /// - 280px（空档 22 < 26）：连它都放不下 ⇒ 整条不画（防御性；窗最小宽 340）。
    #[test]
    fn titlebar_plan_keeps_new_session_longest() {
        let p = titlebar_plan(380.0, true);
        assert_eq!(
            (p.new_w, p.session_w, p.model_w, p.settings_w),
            (TB_NEW_W, TB_SESSION_W, 0.0, TB_SETTINGS_W),
            "380px：模型先让位（状态行回退位接），会话与 ⚙ 留"
        );
        let q = titlebar_plan(340.0, true);
        assert_eq!(
            (q.new_w, q.session_w, q.model_w, q.settings_w),
            (TB_NEW_W, TB_SESSION_W, 0.0, 0.0),
            "340px：只剩 new + session"
        );
        let one = titlebar_plan(300.0, true);
        assert_eq!(
            (one.new_w, one.session_w),
            (TB_NEW_W, 0.0),
            "300px：连历史会话都放不下，但新建会话必须在"
        );
        assert!(one.x >= TB_TITLE_END + TB_TITLE_GAP);
        let tiny = titlebar_plan(280.0, true);
        assert!(
            tiny.x < 0.0 && tiny.new_w == 0.0,
            "280px：空档 < 26 ⇒ 整条不画"
        );
    }

    /// 没配任何「供应商 → 模型」（下拉无可选项）时，模型 chip 不占位、**也不留空档**，其余照摆。
    #[test]
    fn titlebar_plan_without_models_leaves_no_gap_for_model_chip() {
        let p = titlebar_plan(SYSCHAT_PREFERRED_WIDTH, false);
        assert_eq!(
            (p.new_w, p.session_w, p.model_w, p.settings_w),
            (TB_NEW_W, TB_SESSION_W, 0.0, TB_SETTINGS_W)
        );
        let zone_left = SYSCHAT_PREFERRED_WIDTH - 4.0 - TB_BTN_ZONE_W;
        // 只有两个间隔（new|session|settings），没有为模型多留那 6px。
        assert_eq!(
            plan_total(&p),
            TB_NEW_W + TB_SESSION_W + TB_SETTINGS_W + 2.0 * TB_ITEM_GAP
        );
        assert_eq!(p.x + plan_total(&p), zone_left - TB_STRIP_GAP);
    }

    /// 文案截断：放得下原样返回；放不下以省略号收尾且**绝不超过 chip 内可用宽**。
    #[test]
    fn fit_chip_label_truncates_within_budget() {
        // 3 个全宽字（新会话）在 84px chip 里放得下（可用 46px ≥ 42）。
        assert_eq!(fit_chip_label("新会话", 84.0), "新会话");
        // 24 字标题（derive_title 上限）必被截断，且带省略号。
        let long = "把架构图做成网页并配上说明文档再打包发给同事";
        let cut = fit_chip_label(long, 84.0);
        assert!(cut.ends_with('…'), "{cut}");
        assert!(cut.chars().count() < long.chars().count());
        // 逐档校验：截断结果的估算宽 ≤ 预算（保守：宁少一个字）。
        for w in [84.0_f32, 112.0, 60.0, 40.0] {
            let s = fit_chip_label(long, w);
            let est: f32 = s
                .chars()
                .map(|c| if is_wide_char(c) { 14.0 } else { 7.0 })
                .sum();
            assert!(
                est <= (w - 36.0).max(0.0),
                "w={w}: 截断后 {s:?} 估宽 {est} 超出可用空档"
            );
        }
        // 连一个字符都放不下：返回空串（上层 chip 宽为 0 时本就不画，这里是防御）。
        assert_eq!(fit_chip_label("新会话", 30.0), "");
        // ASCII："deepseek-chat"（13 半宽 = 91px）在 112px chip（可用 74px）里被截断。
        let m = fit_chip_label("deepseek-chat", 112.0);
        assert!(m.ends_with('…') && m.len() < "deepseek-chat".len(), "{m}");
    }

    /// v11（卡 `abb-v11-panels-align`）：会话面板右 tag 的相对时间口径。
    ///
    /// 判别力：把「昨天」分支的 `day + 1 == today` 改成 `day == today`（退化成都写「今天」）
    /// ⇒ 本用例红；把当前会话分支去掉（不再写「当前」）⇒ 也红。
    #[test]
    fn session_tag_writes_current_today_yesterday_and_old_date() {
        // 2026-10-18 12:00 本地（UTC+8）（= 2026-10-18 04:00 UTC）
        let now = 1_792_296_000u64;
        assert_eq!(
            session_tag(now as i64, now, true),
            "当前",
            "当前会话不写时间"
        );
        // 本地 09:10 → 今天 09:10
        assert_eq!(session_tag(1_792_285_800, now, false), "今天 09:10");
        // 本地前一天 23:12 → 昨天 23:12
        assert_eq!(session_tag(1_792_249_920, now, false), "昨天 23:12");
        // 更早：固定 `M月D日`（不写「今天/昨天」）
        let old = session_tag(1_791_864_000, now, false);
        assert_eq!(old, "10月13日");
        // 旧数据缺字段（<=0）：空 tag，不编造时间
        assert_eq!(session_tag(0, now, false), "");
    }

    /// v11：token 紧凑写法 / 百分比 / 档位 / 文案（含**两种不同的缺**的空档）。
    ///
    /// 判别力：把 [`usage_level`] 的 90 改回 70（两档合并）⇒ 本用例红；
    /// 把 [`context_usage_text`] 里 `limit.filter(|l| *l > 0)` 去掉 ⇒ 本用例红
    /// （0 上限会被印成 `0.0%`，看着像「占用为零」）。
    #[test]
    fn usage_text_percent_and_level_follow_design() {
        assert_eq!(token_short(0), "0");
        assert_eq!(token_short(999), "999");
        assert_eq!(token_short(12_432), "12.4K");
        assert_eq!(token_short(128_000), "128K"); // 整数不带 .0
        assert_eq!(token_short(1_500_000), "1.5M");
        assert_eq!(usage_percent(12_432, 128_000).round(), 10.0);
        assert_eq!(usage_percent(1, 0), 0.0, "无上限时不产生 NaN/inf");
        assert_eq!(usage_percent(999, 100), 100.0, "越限被夹到 100");
        // 阈值：≥70 橙 / ≥90 红（69.9 / 89.9 仍是下一档）
        assert_eq!(usage_level(0.0), 0);
        assert_eq!(usage_level(69.9), 0);
        assert_eq!(usage_level(70.0), 1);
        assert_eq!(usage_level(89.9), 1);
        assert_eq!(usage_level(90.0), 2);
        assert_eq!(usage_level(100.0), 2);
        assert_eq!(
            context_usage_text(Some(12_432), Some(128_000)),
            "上下文 12.4K/128K · 9.7%"
        );
        // 两种「不知道」分别显示成各自的 `—`（E3：缺与 0 不同义）。
        assert_eq!(context_usage_text(None, None), "上下文 —/— · —");
        assert_eq!(
            context_usage_text(None, Some(128_000)),
            "上下文 —/128K · —",
            "上限知道、用量未知：只隐去不知道的那一半"
        );
        assert_eq!(
            context_usage_text(Some(1_200), None),
            "上下文 1.2K/— · —",
            "用量知道、上限未知：同上"
        );
        // 上限为 0（旧配置的「没设」）等同于未知：不许印成 0% ——那会被读成「快满了/空着」。
        assert_eq!(
            context_usage_text(Some(1_200), Some(0)),
            "上下文 1.2K/— · —"
        );
    }

    /// E3（验收 4）：上限来源有优先级——**供应商覆盖 > 内置表**；两处都不知道就是 `None`。
    ///
    /// 判别力（负向）：把 `context_window_in` 里的 `if p.context_window > 0` 分支删掉 ⇒
    /// 第 2 段红（用户的覆盖会被内置表盖回去，等于设置项没生效）。
    #[test]
    fn context_window_prefers_provider_override_then_builtin_table() {
        let mk = |name: &str, model: &str, cw: u64| crate::config::ProviderConfig {
            name: name.into(),
            kind: "openai-chat".into(),
            base_url: String::new(),
            api_key: String::new(),
            model: model.into(),
            thinking: String::new(),
            context_window: cw,
        };
        // `Config` 有私有字段 ⇒ 不能用结构体更新语法，逐项赋值。
        let mut cfg = Config::default();
        cfg.providers = vec![mk("a", "gpt-4o", 0), mk("b", "gpt-4o", 999_999)];
        cfg.default_provider = "a".into();
        // ① 没配（0）⇒ 内置表（**不改配置的老用户行为不变**）。
        assert_eq!(context_window_in(&cfg, "a"), Some(128_000));
        // ② 配了 ⇒ 覆盖内置表（自建/新模型的真窗口只有用户知道）。
        assert_eq!(context_window_in(&cfg, "b"), Some(999_999));
        // ③ `""` = 跟随全局默认（与 `agent_config` / `resolve_provider` 同口径）。
        assert_eq!(context_window_in(&cfg, ""), Some(128_000));
        // ④ 供应商不在配置里 ⇒ None（不猜）。
        assert_eq!(context_window_in(&cfg, "查无此商"), None);

        let mut cfg2 = Config::default();
        cfg2.providers = vec![mk("c", "my-own-llm", 0)];
        cfg2.default_provider = "c".into();
        // ⑤ 表里没有、也没配 ⇒ None（UI 显示 `—`，不编窗口大小）。
        assert_eq!(context_window_in(&cfg2, "c"), None);
    }

    /// E3：内置兜底表的**取值与顺序**。顺序是承重件——匹配取**首个命中**的子串。
    ///
    /// 判别力（负向）：把 `("gpt-4-turbo", 128_000)` 挪到 `("gpt-4", 8_192)` 之后 ⇒
    /// 第 1 段红；把 `contains` 改成 `==` ⇒ 带日期后缀的那两段红。
    #[test]
    fn builtin_context_window_matches_substrings_longest_key_first() {
        assert_eq!(
            builtin_context_window("gpt-4-turbo-2024-04-09"),
            Some(128_000),
            "长键必须先命中，不能被更短的 gpt-4 抢走"
        );
        assert_eq!(builtin_context_window("gpt-4o"), Some(128_000));
        assert_eq!(builtin_context_window("gpt-4.1-mini"), Some(1_047_576));
        assert_eq!(builtin_context_window("gpt-4"), Some(8_192));
        // 带日期/版本后缀是常态：精确相等会大面积落空。
        assert_eq!(
            builtin_context_window("claude-3-5-sonnet-20241022"),
            Some(200_000)
        );
        assert_eq!(builtin_context_window("claude-sonnet-4-5"), Some(200_000));
        assert_eq!(builtin_context_window("deepseek-chat"), Some(64_000));
        // 大小写不敏感。
        assert_eq!(builtin_context_window("Claude-3-Opus"), Some(200_000));
        // 表里没有 ⇒ None（**不猜一个数**）。
        assert_eq!(builtin_context_window("my-own-llm-v1"), None);
        assert_eq!(builtin_context_window(""), None);
    }

    /// E3（验收 2/3）：悬停提示要把**口径**说清，并且带上精确 token 数。
    ///
    /// 判别力（负向）：把 `context_usage_tip` 里 (None, Some) 与 (Some, None) 两臂合并成
    /// 一句含糊的「数据不足」⇒ 本用例红——那正是「让人不知道缺的是哪一半」的写法。
    #[test]
    fn context_usage_tip_states_scope_and_exact_numbers() {
        let both = context_usage_tip(Some(12_432), Some(128_000));
        assert!(both.contains("12432 / 128000"), "要带精确值：{both}");
        assert!(both.contains("9.7%"), "要带百分比：{both}");
        assert!(both.contains("最近一轮请求"), "口径要写在提示里：{both}");
        assert!(both.contains("不是估算"), "真实/估算必须标清：{both}");

        let no_used = context_usage_tip(None, Some(128_000));
        assert!(no_used.contains("128000"), "{no_used}");
        assert!(no_used.contains("还没有收到过用量上报"), "{no_used}");
        assert!(!no_used.contains("0 /"), "缺用量不许印成 0：{no_used}");

        let no_limit = context_usage_tip(Some(12_432), None);
        assert!(no_limit.contains("12432"), "{no_limit}");
        assert!(no_limit.contains("上限未知"), "{no_limit}");
        assert!(
            no_limit.contains("context_window"),
            "要给可操作的出路（设置项名）：{no_limit}"
        );

        let neither = context_usage_tip(None, None);
        assert!(neither.contains("都未知"), "{neither}");
        assert!(!neither.contains("0 /"), "什么都不许印成 0：{neither}");
    }

    /// v11：面板锚点 —— 右缘跟按钮右缘对齐、顶边在按钮下缘 6px、靠边整体内移（不出窗）。
    ///
    /// 判别力：把横向夹取去掉（不压 `max_x`）⇒ 靠右按钮的面板会伸出窗 ⇒ 本用例红。
    #[test]
    fn menu_anchor_right_aligns_and_keeps_panel_inside_window() {
        // 按钮在窗右侧（右缘 400），面板 250 ⇒ 面板 x = 150，y = 32 + 6。
        assert_eq!(menu_anchor(374, 26, 32, 250, 520), (150, 38));
        // 窄窗（340）：右缘对齐会越界 ⇒ 内移到 6px 起（右缘仍在窗内）。
        let (x, y) = menu_anchor(180, 26, 32, 250, 340);
        assert_eq!(x, 6);
        assert_eq!(y, 38);
        assert!(x + 250 <= 340 - 6, "面板不许出窗");
        // 钮贴到窗右缘（右缘超出可用区）：必须被**右上界**夹回来（只有左边夹取挡不住这条）。
        let (x2, _) = menu_anchor(500, 26, 32, 250, 400);
        assert_eq!(x2, 400 - 250 - 6, "钮贴右缘时应被右上界夹回");
        assert!(x2 + 250 <= 400 - 6, "面板不许出窗（右边界）");
        // 窗比面板还窄：依然落在 6px 起点（不许变负）。
        assert_eq!(menu_anchor(0, 26, 32, 250, 200).0, 6);
    }

    /// 宽字符判据的边界：CJK/全角算宽，ASCII/拉丁/希腊算窄（用于估宽方向正确性）。
    #[test]
    fn is_wide_char_covers_cjk_and_fullwidth() {
        for c in ['新', '会', '话', '，', '：', '（', 'あ', '한'] {
            assert!(is_wide_char(c), "{c} 应算全宽");
        }
        for c in ['A', 'z', '0', '-', '…', 'é'] {
            assert!(!is_wide_char(c), "{c} 应算半宽");
        }
    }

    // ── D7：媒体路径解析基准（唯一口径 = syschat 工作区）──

    /// 相对路径必须相对**传入基准**（真实调用点传 `workspace()`），不得相对进程 CWD。
    /// 变异点：把相对分支改成 `PathBuf::from(src)`（即进程 CWD）⇒ 本测红。
    #[test]
    fn resolve_media_path_relative_uses_base_not_cwd() {
        let base = std::path::Path::new("/tmp/abb-ws-probe");
        assert_eq!(
            resolve_media_path_in(base, "./assets/a.png"),
            std::path::PathBuf::from("/tmp/abb-ws-probe/assets/a.png")
        );
        assert_eq!(
            resolve_media_path_in(base, "assets/a.mp4"),
            std::path::PathBuf::from("/tmp/abb-ws-probe/assets/a.mp4")
        );
        // 基准里本来就存在的相对子目录同样适用
        assert_eq!(
            resolve_media_path_in(base, "attachments/1-a.mp3"),
            std::path::PathBuf::from("/tmp/abb-ws-probe/attachments/1-a.mp3")
        );
    }

    /// 绝对路径原样返回，不受基准影响。
    #[test]
    fn resolve_media_path_absolute_is_untouched() {
        let base = std::path::Path::new("/tmp/abb-ws-probe");
        assert_eq!(
            resolve_media_path_in(base, "/Users/x/a.png"),
            std::path::PathBuf::from("/Users/x/a.png")
        );
    }

    /// `file://` URL 按绝对路径处理（含 `%20` 解码与 `localhost` host 剥离）。
    /// 变异点：删掉 `strip_prefix("file://")` 分支 ⇒ `file:///tmp/a.png` 会被当成
    /// 相对路径拼到基准下，本测红。
    /// `data:` URI 支持（owner 2026-10-16：自包含 HTML 里的内联媒体全是死链）。
    ///
    /// 只测**纯函数**部分（base64 解码与 mime→扩展名）——落盘那半段会写真实运行目录，
    /// 不适合放进单测（隔离门禁的纪律）。
    #[test]
    fn data_uri_base64_decoding_and_mime_ext() {
        // 1×1 PNG 的标准 base64（含 `=` padding）。
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";
        let bytes = super::base64_decode(png).expect("应能解码");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG magic 要对");
        // 容忍折行/空白（data URI 常被折段）。
        let folded = format!("{}\n  {}\n", &png[..20], &png[20..]);
        assert_eq!(super::base64_decode(&folded).unwrap(), bytes);
        // 非法字符 ⇒ None（不能让坏数据变成「半张图」）。
        assert!(super::base64_decode("!!!!").is_none());
        // mime → 扩展名（嗅探不出来时的兜底；svg 是文本、容器格式只能靠 mime）。
        assert_eq!(super::ext_from_mime("image/svg+xml"), Some("svg"));
        assert_eq!(super::ext_from_mime("video/mp4"), Some("mp4"));
        assert_eq!(super::ext_from_mime("audio/mpeg"), Some("mp3"));
        assert_eq!(super::ext_from_mime("application/zip"), None);
        // 非 base64 / 非 data 前缀：明确返回 None（调用方走原路径，绝不猜）。
        assert!(super::data_uri_path("https://a.b/x.png").is_none());
        assert!(super::data_uri_path("data:text/plain,hello").is_none());
    }

    #[test]
    fn resolve_media_path_handles_file_url() {
        let base = std::path::Path::new("/tmp/abb-ws-probe");
        assert_eq!(
            resolve_media_path_in(base, "file:///tmp/a.png"),
            std::path::PathBuf::from("/tmp/a.png")
        );
        assert_eq!(
            resolve_media_path_in(base, "file:///tmp/a%20b.png"),
            std::path::PathBuf::from("/tmp/a b.png")
        );
        assert_eq!(
            resolve_media_path_in(base, "file://localhost/tmp/c.mp4"),
            std::path::PathBuf::from("/tmp/c.mp4")
        );
        // 非法转义原样保留，不 panic
        assert_eq!(
            resolve_media_path_in(base, "file:///tmp/a%zz.png"),
            std::path::PathBuf::from("/tmp/a%zz.png")
        );
    }

    // ── 卡 abb-syschat-d10-insert-interrupt：插入/排队/打断（纯逻辑）──

    /// 发送落点判定：空闲 ⇒ 直接发；在途 + 支持 ⇒ 插入；在途 + 不支持/**能力不明** ⇒ 排队。
    /// 变异点：把 `Unknown` 归到 `Steer` 分支（“先猜支持”），`unknown_queues` 必红。
    #[test]
    fn route_send_matrix() {
        assert_eq!(
            route_send(false, SteerSupport::Unknown),
            SendRoute::Immediate
        );
        assert_eq!(
            route_send(false, SteerSupport::Unsupported),
            SendRoute::Immediate
        );
        assert_eq!(route_send(true, SteerSupport::Supported), SendRoute::Steer);
        assert_eq!(
            route_send(true, SteerSupport::Unsupported),
            SendRoute::Queue
        );
        assert_eq!(route_send(true, SteerSupport::Unknown), SendRoute::Queue);
    }

    /// 空消息（含纯空白）不产生回合——输入框在生成中可用后的最低防线。
    #[test]
    fn submit_rejects_empty() {
        let mut p = SendPump::new();
        assert_eq!(p.submit("", vec![]), None);
        assert_eq!(p.submit("   \n\t", vec![]), None);
        assert_eq!(p.pending_len(), 0);
        assert_eq!(p.submit("  真实消息  ", vec![]), Some(SendRoute::Immediate));
    }

    /// 在途 + 不支持 ⇒ 入队且不入 steer；队首顺序 = 提交顺序（串行化的数据侧）。
    #[test]
    fn queue_preserves_fifo_order() {
        let mut p = SendPump::new();
        p.begin();
        p.set_support(SteerSupport::Unsupported);
        assert_eq!(p.submit("一", vec![]), Some(SendRoute::Queue));
        assert_eq!(p.submit("二", vec![]), Some(SendRoute::Queue));
        assert_eq!(p.pending_len(), 2);
        p.begin(); // finish 前再 begin 幂等（两轮之间不会丢态）
        assert_eq!(p.finish().map(|q| q.text), Some("一".to_string()));
        assert_eq!(p.finish().map(|q| q.text), Some("二".to_string()));
        assert_eq!(p.finish(), None);
        // 队列空 + 空闲 ⇒ 下一条回到 Immediate（finish 真的把 in_flight 降了）。
        assert_eq!(p.submit("三", vec![]), Some(SendRoute::Immediate));
    }

    /// **打断口径**：⏹ 只取消在途那一轮，已入队的消息**不因打断而丢**，
    /// 仍由 `finish()` 依次弹出（与自然完成走同一条出队口）。
    /// 变异点：把 `finish` 改成“一打断就 `clear()`”，本测红。
    #[test]
    fn interrupt_keeps_queued_messages() {
        let mut p = SendPump::new();
        p.begin();
        p.submit("排队甲", vec![]);
        p.submit("排队乙", vec![]);
        // 用户按下 ⏹：在途回合被取消，但队列原封不动。
        let next = p.finish().expect("打断后队列首项仍应开跑");
        assert_eq!(next.text, "排队甲");
        assert_eq!(p.pending_len(), 1, "打断不得清队列");
        assert_eq!(p.finish().map(|q| q.text), Some("排队乙".to_string()));
    }

    /// `clear()` 是显式清空（清空会话/切会话时用），与“打断”区分开。
    #[test]
    fn clear_drops_queue_explicitly() {
        let mut p = SendPump::new();
        p.begin();
        p.submit("将被清", vec![]);
        assert_eq!(p.pending_len(), 1);
        p.clear();
        assert_eq!(p.pending_len(), 0);
        assert!(p.finish().is_none());
    }

    /// `Steer` 落点注入失败时的兜底：补进队列，不静默丢用户消息。
    #[test]
    fn steer_fallback_enqueues() {
        let mut p = SendPump::new();
        p.begin();
        p.set_support(SteerSupport::Supported);
        assert_eq!(p.submit("插入", vec![]), Some(SendRoute::Steer));
        assert_eq!(p.pending_len(), 0, "Steer 不进队");
        assert_eq!(p.enqueue_fallback("插入", vec![]), 1);
        assert_eq!(p.finish().map(|q| q.text), Some("插入".to_string()));
    }

    // ── 卡 abb-syschat-d10-insert-interrupt：两条端到端（mock agent）──

    /// 等 mock 记录到指定事件（有界等待，避免固定 sleep 的时序竞态）。
    async fn wait_for_record(path: &std::path::Path, event: &str) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if read_records(path).iter().any(|e| e["event"] == event) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// 等能力 watch 落锤到 `want`（超时返回当前值，供调用方给出可读断言）。
    ///
    /// 为什么用「查值 + sleep」而不 `timeout_at(deadline, rx.changed())`：实测 tokio
    /// 1.53 下，当 watch **发送端已 drop** 时，`changed()` 会立即就绪地反复唤醒；
    /// `timeout_at` 先轮询内层，内层总是 Ready ⇒ 外层 Sleep 永远不会被轮询到，
    /// 超时形同虚设、整个循环变成 100% CPU 的忙等（本卡实测）。以 sleep 为唯一
    /// 时间源就不取决于 inner 的就绪性。
    async fn wait_for_support(
        rx: &mut tokio::sync::watch::Receiver<SteerSupport>,
        want: SteerSupport,
    ) -> SteerSupport {
        for _ in 0..400 {
            let cur = *rx.borrow_and_update();
            if cur == want {
                return cur;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        *rx.borrow()
    }

    /// E3 端到端（卡 `abb-syschat-e3-tokens-20261013`）：执行层上报的 usage 要**一路落到
    /// 会话索引**——状态栏的「已用」是这条链的产物，不是另找地方猜的。
    ///
    /// 口径：`context_tokens = input + cacheRead + cacheWrite`（**prompt 侧**，不含 output）。
    ///
    /// 判别力（负向，实测会红）：① 删掉 `turn_core` 临界区 2 里写 `s.context_tokens` 的
    /// 两行 ⇒ 第 2 段红；② 把 `usage.context_tokens()` 换成 `usage.total` ⇒ 第 2 段红
    /// （2200 会变成 2230）；③ 把 mock 的 `_meta.usage` 关掉 ⇒ 第 1 段红（阴性对照那半）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn turn_records_real_context_tokens_from_agent_usage() {
        let rec =
            std::env::temp_dir().join(format!("abb-syschat-usage-{}.jsonl", uuid::Uuid::new_v4()));
        let (store, _base) = tmp_store("usage-e3");
        let mut st = store.load();
        assert!(store.ensure_current(&mut st));
        store.save(&st).unwrap();
        let id = st.current.clone();
        let ws =
            std::env::temp_dir().join(format!("abb-syschat-usage-ws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let ctx_tokens = |store: &SessionStore, id: &str| {
            store
                .load()
                .sessions
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.context_tokens)
                .unwrap_or_else(|| panic!("会话 {id} 应在索引里"))
        };

        // ① 阴性对照：执行层**不报** usage ⇒ 值保持 0（= 未知），不许编一个数出来。
        turn_core(
            &store,
            &id,
            mock_agent_cfg(&rec),
            ws.clone(),
            "第一句",
            &[],
            None,
        )
        .await
        .expect("第一回合应完成");
        assert_eq!(
            ctx_tokens(&store, &id),
            0,
            "没上报 usage ⇒ 索引里保持 0（未知），不是被猜出来的数"
        );

        // ② 正例：报 1200/30/1000/0/2230 ⇒ 期望 2200（prompt 侧 = input + cacheRead + cacheWrite）。
        let cfg = mock_agent_cfg_with(
            &rec,
            &[(
                "MOCK_USAGE_JSON",
                r#"{"input":1200,"output":30,"cacheRead":1000,"cacheWrite":0,"total":2230}"#,
            )],
        );
        turn_core(&store, &id, cfg, ws, "第二句", &[], None)
            .await
            .expect("第二回合应完成");
        assert_eq!(
            ctx_tokens(&store, &id),
            2200,
            "执行层上报的真实 usage 必须落进会话索引（prompt 侧，不含 output）"
        );
    }

    /// **支持 steer 的路径**：生成中投一条消息 ⇒ 真写进在途回合（mock 收到
    /// `_session/steering`），**不另起一轮**（只 1 条 `session/prompt`），
    /// 且插入的消息进历史（下一轮上下文不丢）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn steer_supported_inserts_into_in_flight_turn() {
        let rec =
            std::env::temp_dir().join(format!("abb-syschat-steer-{}.jsonl", uuid::Uuid::new_v4()));
        let (store, base) = tmp_store("steer");
        let mut st = store.load();
        assert!(store.ensure_current(&mut st));
        store.save(&st).unwrap();
        let id = st.current.clone();
        let ws = std::env::temp_dir().join(format!("abb-syschat-sws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();

        let (chunks_tx, _chunks_rx) = tokio::sync::mpsc::unbounded_channel::<TurnChunk>();
        let (cap_tx, mut cap_rx) = tokio::sync::watch::channel(SteerSupport::Unknown);
        let (inj_tx, inj_rx) = tokio::sync::mpsc::unbounded_channel::<InboundMsg>();
        // 后端声明支持 steer，并故意把 prompt 应答推迟，造出确定的在途窗口。
        let cfg = mock_agent_cfg_with(
            &rec,
            &[("MOCK_STEER", "1"), ("MOCK_PROMPT_DELAY_MS", "4000")],
        );
        let store2 = SessionStore::new_at(base.clone());
        let id2 = id.clone();
        let turn = tokio::spawn(async move {
            turn_core(
                &store2,
                &id2,
                cfg,
                ws,
                "steer-turn-A",
                &[],
                Some(StreamWiring {
                    chunks: chunks_tx,
                    cancel: CancellationToken::new(),
                    steer: Some(SteerWiring {
                        cap_tx,
                        inject_rx: inj_rx,
                    }),
                }),
            )
            .await
        });

        assert_eq!(
            wait_for_support(&mut cap_rx, SteerSupport::Supported).await,
            SteerSupport::Supported,
            "宣称支持的面（MOCK_STEER=1）应落锤为 Supported"
        );
        assert!(wait_for_record(&rec, "prompt").await, "回合应已派发到 mock");
        // 生成中“按发送”：交给插入端点（调用方在能力=Supported 时才这么做）。
        inj_tx
            .send(steer_inbound("steer-probe-B"))
            .expect("注入通道应可写");

        let end = turn.await.expect("回合任务不应 panic");
        let TurnEnd::Done { text, .. } = end.expect("回合应正常完成") else {
            panic!("未取消的回合应为 Done");
        };
        assert!(text.contains("steer-turn-A"));

        assert!(
            wait_for_record(&rec, "steer").await,
            "mock 应收到 _session/steering（插入真写进了在途回合）"
        );
        let events = read_records(&rec);
        let steers: Vec<_> = events.iter().filter(|e| e["event"] == "steer").collect();
        assert_eq!(steers.len(), 1, "应恰好插入一次：{events:?}");
        assert!(
            steers[0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("steer-probe-B"),
            "插入内容应原样到达 agent: {steers:?}"
        );
        let prompts: Vec<_> = events.iter().filter(|e| e["event"] == "prompt").collect();
        assert_eq!(prompts.len(), 1, "插入不得另起一轮：{prompts:?}");

        // 历史：user(A) + user(B,插) + assistant —— 插入的消息不得从历史里丢。
        let hist = SessionStore::new_at(base.clone()).read_history(&id);
        let texts: Vec<&str> = hist.iter().map(|t| t.text.as_str()).collect();
        assert!(
            texts.contains(&"steer-probe-B"),
            "插入的用户轮应落历史: {texts:?}"
        );
        assert_eq!(hist.len(), 3, "A + 插入B + 助手：{hist:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// **不支持 steer 的回落排队**：能力落锤 `Unsupported`，泵判 `Queue`；
    /// 本轮结束后 `finish()` 依次开下一轮（串行化），mock 收到的 prompt 顺序 = 提交顺序。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn unsupported_steer_queues_and_serializes() {
        let rec =
            std::env::temp_dir().join(format!("abb-syschat-queue-{}.jsonl", uuid::Uuid::new_v4()));
        let (store, base) = tmp_store("queue");
        let mut st = store.load();
        assert!(store.ensure_current(&mut st));
        store.save(&st).unwrap();
        let id = st.current.clone();
        let ws = std::env::temp_dir().join(format!("abb-syschat-qws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();

        let (chunks_tx, _chunks_rx) = tokio::sync::mpsc::unbounded_channel::<TurnChunk>();
        let (cap_tx, mut cap_rx) = tokio::sync::watch::channel(SteerSupport::Unknown);
        // 默认 mock 不声明 steering 扩展，且延迟应答以保证“在途”窗口足够长。
        let cfg = mock_agent_cfg_with(&rec, &[("MOCK_PROMPT_DELAY_MS", "2000")]);
        let store2 = SessionStore::new_at(base.clone());
        let id2 = id.clone();
        let turn = tokio::spawn(async move {
            turn_core(
                &store2,
                &id2,
                cfg,
                ws,
                "queue-turn-A",
                &[],
                Some(StreamWiring {
                    chunks: chunks_tx,
                    cancel: CancellationToken::new(),
                    steer: Some(SteerWiring {
                        cap_tx,
                        inject_rx: {
                            // 不支持路径不会用到注入端点；保留接收端防发送端 panic。
                            let (_keep_tx, rx) = tokio::sync::mpsc::unbounded_channel();
                            rx
                        },
                    }),
                }),
            )
            .await
        });

        assert_eq!(
            wait_for_support(&mut cap_rx, SteerSupport::Unsupported).await,
            SteerSupport::Unsupported,
            "默认 mock 未声明 steering ⇒ 能力必须落锤为 Unsupported"
        );
        let mut pump = SendPump::new();
        pump.begin();
        pump.set_support(SteerSupport::Unsupported);
        assert_eq!(
            pump.submit("queue-turn-B", vec![]),
            Some(SendRoute::Queue),
            "不支持 steer 时必须排队"
        );

        let end = turn.await.expect("回合任务不应 panic");
        assert!(matches!(end, Ok(TurnEnd::Done { .. })));

        // 本轮结束 ⇒ 依次开排队项（串行化的唯一出处）。
        let next = pump.finish().expect("队列应有下一轮");
        assert_eq!(next.text, "queue-turn-B");
        let store3 = SessionStore::new_at(base.clone());
        let end2 = turn_core(
            &store3,
            &id,
            mock_agent_cfg(&rec),
            ws2(),
            &next.text,
            &[],
            None,
        )
        .await
        .expect("第二个回合应完成");
        assert!(matches!(end2, TurnEnd::Done { .. }));

        let events = read_records(&rec);
        let prompt_texts: Vec<String> = events
            .iter()
            .filter(|e| e["event"] == "prompt")
            .map(|e| e["text"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(prompt_texts.len(), 2, "应恰好两轮：{prompt_texts:?}");
        assert!(
            prompt_texts[0].contains("queue-turn-A"),
            "第一轮应是 A: {prompt_texts:?}"
        );
        assert!(
            prompt_texts[1].contains("queue-turn-B"),
            "排队项应在 A 结束后发（顺序 = 提交顺序）: {prompt_texts:?}"
        );
        assert!(
            !events.iter().any(|e| e["event"] == "steer"),
            "不支持路径绝不发 steer 线: {events:?}"
        );

        let hist = SessionStore::new_at(base.clone()).read_history(&id);
        let texts: Vec<&str> = hist.iter().map(|t| t.text.as_str()).collect();
        assert!(
            texts.contains(&"queue-turn-B"),
            "排队项最终应进历史: {texts:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 测试用临时工作区目录。
    fn ws2() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("abb-syschat-ws2-{}", uuid::Uuid::new_v4()))
    }

    // ── v11（卡 `abb-v11-new-session-dedupe`）：「新建会话」去重 ──

    /// 空会话判据的边界（每条对应卡里的一个口径）。
    ///
    /// 判别力：把 `is_empty_session` 改成恒 `true`/恒 `false`、或去掉任一条子句 ⇒ 本用例必红。
    #[test]
    fn is_empty_session_boundaries() {
        // ① 全新会话：历史里一条轮都没有（「有附件但无消息」也落在这里：附件不落历史）
        assert!(is_empty_session(&[]), "没有任何轮 ⇒ 空");
        // ② 用户轮（哪怕正文空白，比如只带了附件）⇒ 不算空（不能把它吞掉）
        assert!(!is_empty_session(&[turn_of("user", "")]), "用户轮算有内容");
        assert!(
            !is_empty_session(&[turn_of("user", "嗨")]),
            "用户轮算有内容"
        );
        // ③ 助手正文 ⇒ 非空
        assert!(
            !is_empty_session(&[turn_of("assistant", "你好")]),
            "有正文的助手轮 ⇒ 非空"
        );
        // ④ 只有思考无正文 ⇒ 非空（`turn_core` 照样落盘这条轮）
        let mut think_only = turn_of("assistant", "");
        think_only.thought = "推理过程…".to_string();
        assert!(
            !is_empty_session(&[think_only]),
            "只有思考 ⇒ 非空（停了也不丢推理）"
        );
        // ⑤ 空壳助手轮（正文/思考都空）⇒ 不算内容（防御：理论上不该落盘，见 turn_core 的守卫）
        assert!(
            is_empty_session(&[turn_of("assistant", "  ")]),
            "全空白的助手轮不算内容"
        );
        assert!(
            is_empty_session(&[turn_of("assistant", ""), turn_of("assistant", "\n")]),
            "全是空壳轮 ⇒ 空"
        );
    }

    /// 端到端去重（临时 base，不碰真实数据目录）：
    /// 空会话连点「新建会话」⇒ 只能有一个会话；有内容后再点 ⇒ 才真新建。
    #[test]
    fn new_session_reuses_current_when_empty() {
        let (store, base) = tmp_store("dedupe");
        // 空库首次调用：`ensure_current` 先建出首个（空）会话 ⇒ 本函数复用它，列表只有 1 条。
        let first = new_session_locked(&store, None).unwrap();
        assert_eq!(store.list().0.len(), 1, "空库调用后只应有一个会话");

        // 连点两次：都是复用，且列表长度不增长。
        for _ in 0..2 {
            let again = new_session_locked(&store, None).unwrap();
            assert!(!again.created, "当前会话为空 ⇒ 复用，不新建");
            assert_eq!(again.id, first.id, "复用的就是当前那个空会话");
            assert_eq!(store.list().0.len(), 1, "列表不得增长");
        }

        // 写进一条用户轮 ⇒ 再点就真新建（旧会话保留在列表里）。
        store
            .append_turns(&first.id, &[turn_of("user", "说一句话")])
            .unwrap();
        let third = new_session_locked(&store, None).unwrap();
        assert!(third.created, "有内容后应真新建");
        assert_ne!(third.id, first.id);
        let (list, cur) = store.list();
        assert_eq!(list.len(), 2, "旧会话必须保留（稿 v10 第 7 条）");
        assert_eq!(cur, third.id);
        // 新建的会话自己也是空的 ⇒ 再点仍复用（不堆空会话）。
        let fourth = new_session_locked(&store, None).unwrap();
        assert!(!fourth.created && fourth.id == third.id);
        assert_eq!(store.list().0.len(), 2, "空会话不被堆出来");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 去重不得弄坏「旧版单会话历史迁移」：迁移后默认会话**非空** ⇒ 点新建会话应真新建
    /// （而不是按空会话复用）。
    #[test]
    fn new_session_after_legacy_migration_creates_new() {
        let (store, base) = tmp_store("dedupe-legacy");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            store.legacy_path(),
            format!(
                "{}\n",
                serde_json::to_string(&turn_of("user", "旧话")).unwrap()
            ),
        )
        .unwrap();
        let a = new_session_locked(&store, None).unwrap();
        let (list, _) = store.list();
        assert_eq!(list.len(), 2, "旧会话（含迁移历史）必须保留");
        assert!(store.read_history(&a.id).is_empty(), "新会话不得带上旧历史");
        assert!(
            list.iter().any(|s| store.read_history(&s.id).len() == 1),
            "迁移进来的那一条轮应在某个会话里（没丢）"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── v11（卡 `abb-v11-popup-anchor`）：`.pop` 面板锚点 ──

    /// 落位三条口径逐条钉住（右对齐、上贴、靠边内移），并逐点扫「不出窗」。
    #[test]
    fn pop_anchor_policies() {
        // ① 右缘与按钮右缘对齐 + 底边在按钮顶边上方 6px。（按钮取得够靠右，免得先被窗边夹住。）
        let btn = (300, 500, 24, 24);
        let panel = (300, 220);
        let win = (520, 560);
        let (x, y) = pop_anchor(btn, panel, win);
        assert_eq!(x, btn.0 + btn.2 - panel.0, "面板右缘应与按钮右缘对齐");
        assert_eq!(x + panel.0, btn.0 + btn.2);
        assert_eq!(btn.1 - (y + panel.1), POP_GAP, "底边距按钮顶边应恰为 6px");

        // ② 靠左窗边：右对齐会跑出左边 ⇒ 内移到留白处，仍不出窗。
        let left = pop_anchor((10, 500, 24, 24), panel, win);
        assert_eq!(left.0, POP_MARGIN, "靠左窗边应内移到留白处");
        // ③ 靠右窗边：右对齐会跑出右边 ⇒ 夹回右留白。
        let right = pop_anchor((500, 500, 24, 24), panel, win);
        assert_eq!(right.0 + 300, 520 - POP_MARGIN, "靠右窗边应夹回右留白");
        // ④ 面板高过按钮上方空间（窗口很矮）⇒ 顶边内移（不出窗优先）。
        let tall = pop_anchor((200, 100, 24, 24), panel, (520, 380));
        assert_eq!(tall.1, POP_MARGIN, "上边放不下时顶边夹到留白处");
        // ⑤ 窗口比面板还小：任何输入都不出窗（最终落在左上留白处）。
        assert_eq!(
            pop_anchor((200, 500, 24, 24), panel, (200, 150)),
            (POP_MARGIN, POP_MARGIN)
        );
        // ⑥ 逐点扫：任何按钮位置/窗口尺寸下都落在窗内。
        for bw in [0, 10, 24, 137] {
            for bx in [-40, 0, 6, 200, 500, 900] {
                for by in [-30, 6, 120, 500, 1200] {
                    for (ww, wh) in [(340, 380), (520, 560), (1200, 900)] {
                        let (x, y) = pop_anchor((bx, by, bw, 24), panel, (ww, wh));
                        assert!(
                            x >= POP_MARGIN
                                && y >= POP_MARGIN
                                && x + 300 <= ww - POP_MARGIN
                                && y + 220 <= wh - POP_MARGIN,
                            "出窗了：btn=({bx},{by},{bw}) win=({ww},{wh}) ⇒ ({x},{y})"
                        );
                    }
                }
            }
        }
    }

    /// 「更多」面板高：搬自 `.slint` 原表达式（逐值对表；改任一分支必红）。
    #[test]
    fn more_pop_h_is_the_slint_expression() {
        assert_eq!(more_pop_h(0), 122);
        assert_eq!(more_pop_h(1), 122 + 26 + 20);
        assert_eq!(more_pop_h(2), 122 + 52 + 20 + 26);
        assert_eq!(more_pop_h(5), 122 + 130 + 20 + 26);
        assert_eq!(more_pop_h(-3), 122, "负数不该产生负高");
        assert!(more_pop_h(3) > more_pop_h(0), "条目变多 ⇒ 面板变高");
    }

    // ── v11（卡 `abb-v11-resizable-split`）：分割条比例 ↔ 像素 ──

    /// 比例 ↔ 像素：不碰上下限时是纯换算，碰上下限就夹住。
    #[test]
    fn split_ratio_pixel_roundtrip_and_clamps() {
        let band = 482.0; // 560 高窗的可分配带（560 − 78）
        let cmin = SPLIT_COMPOSER_MIN_H;
        // 中间比例：往返一致（范围 = [96/482, (482−10−146)/482] ≈ [0.199, 0.676]）。
        for r in [0.25_f32, 0.4, 0.5, 0.65] {
            let h = split_convo_h(band, r, cmin);
            assert_eq!(split_ratio(band, h), r, "比例往返不得漂（{r}）");
        }
        // 上限：输入区下限优先（内容区不得把输入区压到下限之下）
        let h_hi = split_convo_h(band, 0.99, cmin);
        assert_eq!(
            h_hi,
            band - SPLIT_HANDLE_H - cmin,
            "比例 0.99 应被输入区下限夹住"
        );
        // 待发附件行出现 ⇒ 输入区下限抬高一行 ⇒ 上限跟着下移（同一口径喂给两处）
        let cmin2 = split_composer_min_h(2);
        assert_eq!(cmin2, cmin + 76.0);
        assert_eq!(
            split_convo_h(band, 0.99, cmin2),
            h_hi - 76.0,
            "有待发附件时上限应下移一行高（消息区让位）"
        );
        assert_eq!(split_composer_min_h(0), cmin, "无待发附件时就是自然高");
        // 默认比例（哨兵 1.0）就是「输入区取下限」那一档
        assert_eq!(split_convo_h(band, SPLIT_RATIO_DEFAULT, cmin), h_hi);
        // 下限：内容区最小高
        assert_eq!(split_convo_h(band, 0.01, cmin), SPLIT_CONVO_MIN_H);
        assert_eq!(
            split_convo_h(band, -3.0, cmin),
            SPLIT_CONVO_MIN_H,
            "脏比例夹回下限"
        );
        assert_eq!(split_convo_h(band, 9.0, cmin), h_hi, "脏比例夹回上限");
        // 窗口还没布局（带 = 0）⇒ 0，不写负高
        assert_eq!(split_convo_h(0.0, 0.6, cmin), 0.0);
        assert_eq!(split_clamp_convo_h(0.0, 100.0, cmin), 0.0);
        // 退化：带比分割条还矮 ⇒ 0
        assert_eq!(split_convo_h(6.0, 0.6, cmin), 0.0);
        // 极窄（两区最小高之和 > 可分配带）⇒ 保内容区最小高
        let usable = 200.0;
        assert_eq!(
            split_clamp_convo_h(usable + SPLIT_HANDLE_H, 999.0, cmin),
            SPLIT_CONVO_MIN_H.min(usable),
            "两区最小高打架时保内容区"
        );
        // NaN / inf：既不 panic 也不把脏值传播出去
        assert_eq!(split_clamp_convo_h(band, f32::NAN, cmin), SPLIT_CONVO_MIN_H);
        assert_eq!(split_clamp_convo_h(band, f32::INFINITY, cmin), h_hi);
        assert_eq!(split_ratio(0.0, 300.0), SPLIT_RATIO_DEFAULT);
        assert_eq!(split_ratio(band, f32::NAN), SPLIT_RATIO_DEFAULT);
    }

    /// 拖动 = 「divider 跟指针」：指针相对消息区顶边的 y 直接就是内容区高（夹取后），
    /// 比例同步成夹取后的值；换窗高后按**比例**重新分配（不是记住像素高）。
    #[test]
    fn split_drag_follows_pointer_and_clamps() {
        let band = 482.0;
        let cmin = SPLIT_COMPOSER_MIN_H;
        let start_ratio = 0.5_f32;
        let mut d = SplitDrag::new(start_ratio);
        let (h0, c0) = d.on_band(band, cmin);
        assert_eq!(h0, split_convo_h(band, start_ratio, cmin));
        // 两区高正好拼满可分配带（分割条夹在中间）：布局里没有「剩余空间给谁」的歧义
        assert_eq!(h0 + SPLIT_HANDLE_H + c0, band, "两区 + 分割条 = 带");
        assert_eq!(c0, split_composer_h(band, h0));

        // 指针在 300（相对消息区顶）⇒ 高就是 300（区间内不加任何额外偏移）
        let (h1, c1) = d.drag_to(300.0);
        assert_eq!(h1, 300.0, "divider 应跟手");
        assert_eq!(h1 + SPLIT_HANDLE_H + c1, band, "两区 + 分割条 = 带");
        assert_eq!(d.ratio(), split_ratio(band, 300.0));

        // 拖到上限/下限：夹住，且比例写回夹取后的值（松手存的就是屏幕上看到的）
        let (h_max, c_min) = d.drag_to(9999.0);
        assert_eq!(h_max, band - SPLIT_HANDLE_H - cmin);
        assert_eq!(c_min, cmin, "输入区正好被夹在下限上");
        assert_eq!(
            d.ratio(),
            split_ratio(band, h_max),
            "比例应同步为夹取后的值"
        );
        assert_eq!(
            d.drag_to(-9999.0).0,
            SPLIT_CONVO_MIN_H,
            "往上拖被内容区下限夹住"
        );
        assert_eq!(d.ratio(), split_ratio(band, SPLIT_CONVO_MIN_H));

        // 回到区间中间：与之前同一个指针位置必须得到同一个高（不累加、不漂）
        assert_eq!(d.drag_to(300.0).0, 300.0);
        // 逐帧来回抖也不会漂（每一步都是绝对定位）
        for y in [320.0, 280.0, 300.0, 260.0, 300.0] {
            d.drag_to(y);
        }
        assert_eq!(d.drag_to(300.0).0, 300.0, "来回拖动不得累积误差");

        // 待发附件行出现（输入区下限抬高）⇒ 同一指针位置得到的高不变，但上限下移
        let cmin2 = split_composer_min_h(1);
        assert_eq!(
            d.on_band(band, cmin2).0,
            split_convo_h(band, d.ratio(), cmin2)
        );
        let (h_max2, c_min2) = d.drag_to(9999.0);
        assert_eq!(h_max2, band - SPLIT_HANDLE_H - cmin2);
        assert_eq!(c_min2, cmin2, "附件行出现后输入区下限也跟着抬高");
        assert_eq!(h_max2 + SPLIT_HANDLE_H + c_min2, band, "两区 + 分割条 = 带");

        // 窗口变大后按**比例**重新分配（不是记住像素高）
        let band2 = 700.0;
        let (h_new, c_new) = d.on_band(band2, cmin);
        assert_eq!(h_new, split_convo_h(band2, d.ratio(), cmin));
        assert_eq!(h_new + SPLIT_HANDLE_H + c_new, band2, "换窗高后仍拼满");
        let back = split_ratio(band2, h_new);
        assert!(
            (back - d.ratio()).abs() < 1e-6,
            "换窗高后比例应保持（{} vs {}）",
            back,
            d.ratio()
        );
    }

    /// 脏输入与「还没布局」：`SplitDrag` 不得把 NaN/inf 放进比例（它会被写进 split.json），
    /// 也不得在带未知时算出高度；`split_composer_h` 在退化输入下给 0（不写负高）。
    #[test]
    fn split_drag_rejects_dirty_input() {
        let mut d = SplitDrag::new(f32::NAN);
        assert_eq!(d.ratio(), SPLIT_RATIO_DEFAULT);
        // 还没报带（band = 0）⇒ 两区高都是 0、比例不动（调用方据此不做任何写窗口动作）
        assert_eq!(d.on_band(0.0, SPLIT_COMPOSER_MIN_H), (0.0, 0.0));
        assert_eq!(d.drag_to(300.0), (0.0, 0.0), "未布局时拖动不得凭空算出高度");
        assert_eq!(d.ratio(), SPLIT_RATIO_DEFAULT);
        d.on_band(482.0, SPLIT_COMPOSER_MIN_H);
        assert!(d.drag_to(f32::NAN).0.is_finite(), "脏指针位置不得传播");
        assert_eq!(d.ratio(), SPLIT_RATIO_DEFAULT);
        // inf 同样当「没有有效指针位置」：空操作（绝不写进比例）
        assert!(d.drag_to(f32::INFINITY).1.is_finite(), "inf 也不得传播");
        assert_eq!(d.ratio(), SPLIT_RATIO_DEFAULT, "脏指针位置不得改比例");
        // 两区高与「带」的关系：退化带 ⇒ 0；内容区超界 ⇒ 输入区不小于 0；NaN ⇒ 0
        assert_eq!(split_composer_h(0.0, 100.0), 0.0);
        assert_eq!(split_composer_h(6.0, 100.0), 0.0, "带比分割条还矮 ⇒ 0");
        assert_eq!(
            split_composer_h(482.0, 9999.0),
            0.0,
            "内容区超界 ⇒ 输入区不小于 0"
        );
        assert_eq!(split_composer_h(f32::NAN, 100.0), 0.0);
        assert_eq!(split_composer_h(482.0, f32::NAN), 0.0);
    }

    /// 比例落盘：写入 ⇒ 读回一致；**文件缺失/损坏/越界 ⇒ 默认值**。
    ///
    /// 生产路径是 `~/.agent-bridge/split.json`（同 `bubble.json` 那一套）；单测走临时文件
    /// （不碰真实数据目录 —— `tools/check_test_isolation.sh` 会红）。
    #[test]
    fn split_ratio_persists_and_falls_back() {
        let dir = std::env::temp_dir().join(format!("abb-split-{}", uuid::Uuid::new_v4()));
        let path = dir.join("split.json");
        // 文件缺失 ⇒ None（调用方用默认值）
        assert_eq!(load_split_ratio_from(&path), None, "缺失时不给值");
        // 写入 ⇒ 读回
        save_split_ratio_to(&path, 0.42);
        assert_eq!(load_split_ratio_from(&path), Some(0.42));
        // 父目录不存在也能写（自建目录）
        let deep = dir.join("a").join("b").join("split.json");
        save_split_ratio_to(&deep, 0.5);
        assert_eq!(load_split_ratio_from(&deep), Some(0.5));
        // 损坏 / 越界 ⇒ 当「没有」（不得把脏值喂给布局）
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_split_ratio_from(&path), None);
        std::fs::write(&path, r#"{"ratio":7.5}"#).unwrap();
        assert_eq!(load_split_ratio_from(&path), None, "越界比例不给值");
        std::fs::write(&path, r#"{"ratio":-1}"#).unwrap();
        assert_eq!(load_split_ratio_from(&path), None);
        // 脏值写盘时被夹回默认值（文件里不出现 NaN）
        save_split_ratio_to(&path, f32::NAN);
        assert_eq!(load_split_ratio_from(&path), Some(SPLIT_RATIO_DEFAULT));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 分割条高度与**设计稿**一致（读 `docs/design/syschat-dialog.html` 的 `.split{…height:Npx…}`）。
    ///
    /// 为什么直接读稿：卡正文写的是 8px，稿里是 10px，而设计目录的版本口径是「最新口径一律以
    /// `syschat-dialog.html` 为准」。漂移时本用例红。
    #[test]
    fn split_handle_h_matches_design() {
        let raw = crate::platform::src_lf(include_str!("../docs/design/syschat-dialog.html"));
        let line = raw
            .lines()
            .find(|l| l.trim_start().starts_with(".split{"))
            .expect("设计稿里找不到 .split 规则");
        let seg = line.split("height:").nth(1).expect(".split 应有 height");
        let num: f32 = seg
            .split(['p', ';'])
            .next()
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("解析 .split 高度失败：{line:?}"));
        assert_eq!(num, SPLIT_HANDLE_H, "分割条高度与设计稿漂移了");
    }

    // ───── v11c（卡 `abb-v11-resize-maximize-macos`）：自管缩放与最大化（纯逻辑）─────

    /// 八向解析：合法的 8 个字面量都认，脏输入一律拒（不猜）。
    #[test]
    fn resize_dir_parses_eight_and_rejects_junk() {
        let ok = [
            ("north", (true, false, false, false)),
            ("south", (false, true, false, false)),
            ("east", (false, false, true, false)),
            ("west", (false, false, false, true)),
            ("north-east", (true, false, true, false)),
            ("north-west", (true, false, false, true)),
            ("south-east", (false, true, true, false)),
            ("south-west", (false, true, false, true)),
        ];
        for (s, want) in ok {
            let d = ResizeDir::parse(s).unwrap_or_else(|| panic!("{s} 应被接受"));
            assert_eq!((d.north, d.south, d.east, d.west), want, "{s} 拆解错");
        }
        for bad in [
            "",
            "-",
            "north-south",
            "east-west",
            "north-north",
            "up",
            "north-",
            "north east",
        ] {
            assert!(ResizeDir::parse(bad).is_none(), "{bad:?} 不该被接受");
        }
    }

    /// 东/南：左上角**不动**，尺寸跟指针。
    #[test]
    fn resize_session_east_south_keep_origin() {
        let a = WinRect::new(100.0, 100.0, 400.0, 600.0);
        let s = ResizeSession::new(ResizeDir::parse("south-east").unwrap(), (500.0, 700.0), a);
        let r = s.update((560.0, 730.0), (340.0, 380.0), (2000.0, 1200.0));
        assert_eq!((r.x, r.y), (100.0, 100.0), "东/南只改尺寸，不动左上角");
        assert_eq!((r.w, r.h), (460.0, 630.0));
    }

    /// 西/北：被拖的那条边**贴指针** ⇒ 左上角跟着走，而右/下边不动。
    #[test]
    fn resize_session_west_north_move_origin() {
        let a = WinRect::new(100.0, 100.0, 400.0, 600.0);
        let s = ResizeSession::new(ResizeDir::parse("north-west").unwrap(), (100.0, 100.0), a);
        let r = s.update((60.0, 70.0), (340.0, 380.0), (2000.0, 1200.0));
        assert_eq!((r.w, r.h), (440.0, 630.0));
        assert_eq!((r.x, r.y), (60.0, 70.0), "左/上边应贴指针");
        assert_eq!(r.x + r.w, 500.0, "右边不动");
        assert_eq!(r.y + r.h, 700.0, "下边不动");
        // 单边：另一轴完全不动
        let e = ResizeSession::new(ResizeDir::parse("east").unwrap(), (500.0, 400.0), a);
        let re = e.update((600.0, 900.0), (340.0, 380.0), (2000.0, 1200.0));
        assert_eq!((re.w, re.h), (500.0, 600.0), "水平边不许改高（反之亦然）");
        assert_eq!((re.x, re.y), (100.0, 100.0));
        let n = ResizeSession::new(ResizeDir::parse("north").unwrap(), (300.0, 100.0), a);
        let rn = n.update((900.0, 40.0), (340.0, 380.0), (2000.0, 1200.0));
        assert_eq!((rn.w, rn.h), (400.0, 660.0));
        assert_eq!(rn.y, 40.0);
        assert_eq!(rn.x, 100.0);
    }

    /// 上下限：夹到 `.slint` 的 340×380 与显示器上限；夹取后右/下边仍不动，
    /// 且**脏上下限（min > max）下限赢**（绝不把窗口算成 0）。
    #[test]
    fn resize_session_clamps_to_min_and_max() {
        let a = WinRect::new(100.0, 100.0, 400.0, 600.0);
        let se = ResizeSession::new(ResizeDir::parse("south-east").unwrap(), (500.0, 700.0), a);
        let r = se.update((-100.0, -100.0), (340.0, 380.0), (2000.0, 1200.0));
        assert_eq!((r.w, r.h), (340.0, 380.0), "往内拖过下限⇒夹到最小尺寸");
        assert_eq!((r.x, r.y), (100.0, 100.0));
        let r2 = se.update((99999.0, 99999.0), (340.0, 380.0), (2000.0, 1200.0));
        assert_eq!((r2.w, r2.h), (2000.0, 1200.0), "往外拖过上限⇒夹到显示器");
        let sw = ResizeSession::new(ResizeDir::parse("west").unwrap(), (100.0, 100.0), a);
        let r3 = sw.update((900.0, 100.0), (340.0, 380.0), (2000.0, 1200.0));
        assert_eq!(r3.w, 340.0, "西边夹到下限");
        assert_eq!(r3.x + r3.w, 500.0, "夹取后右边仍不动");
        let r4 = se.update((500.0, 700.0), (800.0, 800.0), (100.0, 100.0));
        assert_eq!((r4.w, r4.h), (800.0, 800.0), "min > max ⇒ 下限赢");
    }

    /// 脏输入不传播：NaN/inf（指针/锚点任一项）⇒ 原矩形不动。
    #[test]
    fn resize_session_rejects_non_finite() {
        let a = WinRect::new(100.0, 100.0, 400.0, 600.0);
        let s = ResizeSession::new(ResizeDir::parse("south-east").unwrap(), (500.0, 700.0), a);
        for bad in [
            (f32::NAN, 400.0),
            (500.0, f32::NAN),
            (f32::INFINITY, 400.0),
            (500.0, f32::NEG_INFINITY),
        ] {
            assert_eq!(
                s.update(bad, (340.0, 380.0), (2000.0, 1200.0)),
                a,
                "脏指针 {bad:?} 不得改矩形"
            );
        }
        let dirty = ResizeSession::new(ResizeDir::parse("east").unwrap(), (f32::NAN, 0.0), a);
        assert_eq!(
            dirty.update((600.0, 400.0), (340.0, 380.0), (2000.0, 1200.0)),
            a
        );
    }

    /// 最大化 ↔ 还原：记住原矩形 → 铺满工作区 → 再点回到原矩形（且能再来一轮）。
    #[test]
    fn maximize_toggle_fills_work_area_then_restores() {
        let cur = WinRect::new(120.0, 90.0, 520.0, 560.0);
        let work = WinRect::new(0.0, 25.0, 1440.0, 875.0);
        let mut st = MaximizeState::default();
        let maxed = st.toggle(cur, work);
        assert_eq!(maxed, work, "最大化 = 铺满工作区");
        assert!(st.is_maximized(maxed, work), "这之后应该算已最大化");
        let back = st.toggle(maxed, work);
        assert_eq!(back, cur, "再点一次应还原到原尺寸与位置");
        assert!(!st.is_maximized(back, work));
        assert_eq!(st.toggle(back, work), work, "还原之后还能再最大化");
    }

    /// 最大化后又手动改过窗口 ⇒ 再点 `⤢` 是**重新最大化**（与 macOS 绿点同口径），
    /// 下次还原目标是**重新最大化前**那一份。
    #[test]
    fn maximize_after_manual_geometry_change_remaximizes() {
        let cur = WinRect::new(120.0, 90.0, 520.0, 560.0);
        let work = WinRect::new(0.0, 25.0, 1440.0, 875.0);
        let mut st = MaximizeState::default();
        assert_eq!(st.toggle(cur, work), work, "先铺满工作区");
        let dragged = WinRect::new(300.0, 200.0, 700.0, 500.0);
        let again = st.toggle(dragged, work);
        assert_eq!(again, work, "不在工作区上⇒重新最大化");
        assert_eq!(st.toggle(again, work), dragged, "还原目标是拖出来那一份");
    }

    /// 1px 容差：尺寸/位置走 f32 → 物理整数点 → 读回的往返，不能要求逐位相等。
    #[test]
    fn maximize_tolerance_is_one_pixel() {
        let work = WinRect::new(0.0, 25.0, 1440.0, 875.0);
        let mut st = MaximizeState::default();
        let _ = st.toggle(WinRect::new(100.0, 100.0, 520.0, 560.0), work);
        let almost = WinRect::new(0.5, 25.4, 1439.5, 875.0);
        assert!(st.is_maximized(almost, work), "±1px 内应算同一个");
        let off = WinRect::new(0.0, 25.0, 1430.0, 875.0);
        assert!(!st.is_maximized(off, work), "差 10px 不算");
    }
    /// 卡 `abb-v11-message-actions`：动作行的目标解析。
    fn t(role: &str, text: &str) -> Turn {
        Turn {
            role: role.into(),
            text: text.into(),
            thought: String::new(),
            ts: 0,
        }
    }

    #[test]
    fn action_target_resolves_last_assistant_turn_and_its_prompt() {
        let h = vec![
            t("user", "你好"),
            t("assistant", "你好呀"),
            t("user", "说个笑话"),
            t("assistant", "笑话 X"),
        ];
        // 显式下标（恒为真实下标，见 `action_target` 的文档）
        let a = action_target(&h, 3).expect("应解析到最后一条助手轮");
        assert_eq!(a.index, 3);
        assert_eq!(a.text, "笑话 X");
        assert_eq!(
            a.prompt.as_deref(),
            Some("说个笑话"),
            "提示词取该轮之前最近的用户轮"
        );
        // 显式下标
        let b = action_target(&h, 1).expect("应解析到下标 1");
        assert_eq!(b.index, 1);
        assert_eq!(b.text, "你好呀");
        assert_eq!(
            b.prompt.as_deref(),
            Some("你好"),
            "跨过中间轮只取最近的一条用户轮"
        );
    }

    #[test]
    fn action_target_rejects_user_turn_out_of_range_and_empty_history() {
        let h = vec![t("user", "只有用户轮")];
        assert!(action_target(&h, 0).is_none(), "指到用户轮必须拒绝（不猜）");
        assert!(action_target(&h, 7).is_none(), "越界必须拒绝");
        assert!(action_target(&[], 0).is_none(), "空历史 + 下标必须拒绝");
        // 哨兵一律拒绝：**没有**「负值 = 最后一条助手轮」这条约定（评审 Critical 的修法）。
        //
        // ⚠️ 判据必须拿**末条真的是助手轮**的历史：只用上面那条「没有助手轮」的历史是**空判据**
        // ——老哨兵写法在那里也返回 `None`，断言恒绿（实测：把哨兵分支写回去，只有 `h` 的几条
        // 照样通过，`ends_with_assistant` 这条才是唯一会红的）。评审的复现正是这个形态：
        // 历史末条是助手轮时，`-1` 会解析**到最后一条助手轮**，于是两条同值哨兵一起指到同一轮。
        let ends_with_assistant = vec![
            t("user", "一"),
            t("assistant", "答复一"),
            t("user", "二"),
            t("assistant", "答复二"),
        ];
        for sentinel in [-1, -2, i32::MIN] {
            assert!(
                action_target(&h, sentinel).is_none(),
                "哨兵 {sentinel} 必须拒绝（哨兵不是身份：两条同值哨兵会一起解析到同一轮）"
            );
            assert!(
                action_target(&[], sentinel).is_none(),
                "空历史 + 哨兵必须拒绝"
            );
            assert!(
                action_target(&ends_with_assistant, sentinel).is_none(),
                "末条是助手轮时也必须拒绝哨兵 {sentinel}——否则第二条动作行会静默操作第一条的答复"
            );
        }
    }

    #[test]
    fn action_target_without_preceding_user_turn_has_no_prompt() {
        let h = vec![t("assistant", "凭空一句")];
        let a = action_target(&h, 0).expect("助手轮本身应可解析");
        assert_eq!(a.index, 0);
        assert_eq!(a.text, "凭空一句");
        assert!(a.prompt.is_none(), "前面没有用户轮 ⇒ 没有可重跑的提示词");
    }

    /// 收尾轮的下标定位：末条是助手轮 ⇒ 它的下标；否则 `None`（调用方据此**不挂**动作行）。
    ///
    /// 判别力（负向）：去掉 `role == "assistant"` 条件（直接 `len() - 1`）⇒ 末条是用户轮的
    /// 两个用例红——那会让动作行指向**上一轮的答复**（评审 Critical 的同族错误）。
    #[test]
    fn last_assistant_index_requires_an_assistant_tail() {
        let h = vec![
            t("user", "一"),
            t("assistant", "答复一"),
            t("user", "二"),
            t("assistant", "答复二"),
        ];
        assert_eq!(last_assistant_index(&h), Some(3));
        // 刚插入的用户轮还没落助手轮（回合进行中/失败）⇒ 定位不到，不挂动作行
        let mut h2 = h.clone();
        h2.push(t("user", "三"));
        assert_eq!(
            last_assistant_index(&h2),
            None,
            "末条是用户轮 ⇒ 不许硬取 len-1"
        );
        assert_eq!(last_assistant_index(&[]), None, "空历史");
        assert_eq!(
            last_assistant_index(&[t("user", "只有提问")]),
            None,
            "只有用户轮"
        );
        // 与 action_target 串起来：定位到的下标必须能解析出**这一条**答复
        let idx = last_assistant_index(&h).expect("末条是助手轮");
        let a = action_target(&h, idx).expect("应可解析");
        assert_eq!(a.text, "答复二");
        assert_eq!(a.index, 3);
    }

    // ───────── 卡 `abb-v11-session-archive`：历史会话归档（隐藏 + 可找回，不删数据） ─────────

    /// 会话面板的 (列表, 当前项) 判据：归档视图必须换成已归档列表**且当前项清空**。
    ///
    /// 判别力（负向）：把 `session_menu_view` 的归档分支改回 `visible` ⇒ 本条红
    /// ——那正是「当前会话被标成已归档」的写法。
    #[test]
    fn session_menu_view_swaps_list_and_clears_current_in_archive_view() {
        let m = |id: &str| SessionMeta {
            id: id.to_string(),
            title: id.to_string(),
            created_at: 1,
            updated_at: 2,
            provider: String::new(),
            archived: id == "arch",
            context_tokens: 0,
        };
        let visible = (vec![m("live")], "live".to_string());
        let archived = vec![m("arch")];

        let (list, current) = session_menu_view(false, visible.clone(), archived.clone());
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["live"]
        );
        assert_eq!(current, "live", "普通视图要带真实当前项");

        let (list, current) = session_menu_view(true, visible, archived);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["arch"]
        );
        assert!(
            current.is_empty(),
            "归档视图必须清掉当前项（否则归档列表里那一项会被标成「当前」，看起来像「当前会话已归档」）"
        );
    }

    /// 面板两种视图的文案（纯函数）：普通视图给「归档当前会话」+「已归档（N）」，归档视图第一行
    /// 变义成「返回」且不再画第二行。
    ///
    /// 判别力（负向）：把归档视图的 `extra_text` 改回「🗄 归档…」⇒ 末段断言红（同名两义的行
    /// 会让人点错：以为在归档，其实是在返回）。
    #[test]
    fn session_panel_text_normal_and_archive_views() {
        let n = session_panel_text(false, 3);
        assert_eq!(n.heading, "会话（历史保留，可随时切回）");
        assert_eq!(n.extra_text, "🗄 归档…");
        assert_eq!(n.extra_tag, "当前会话");
        assert_eq!(n.extra2_text, "🗄 已归档（3）…", "条数必须来自实参");
        assert!(n.extra2_tag.is_empty());
        assert!(n.foot.contains("已归档"), "footer 要写清去哪儿找回");

        let a = session_panel_text(true, 3);
        assert!(
            a.heading.contains("3 条"),
            "归档视图要显示条数：{}",
            a.heading
        );
        assert_eq!(a.extra_text, "← 返回会话列表");
        assert!(
            !a.extra_text.contains("归档…") && a.extra2_text.is_empty(),
            "归档视图必须把「🗄 归档…」换成「返回」，并且不画第二行"
        );
        assert!(a.foot.contains("不做任何删除"), "footer 要如实说数据不删");
    }

    /// 归档 ⇒ 从可见列表隐藏；取消归档 ⇒ 回到列表；**历史文件原样**（卡要求「不删数据」）。
    #[test]
    fn archive_hides_from_list_unarchive_restores_and_keeps_history_files() {
        let (store, base) = tmp_store("arch-hide");
        let mut st = store.load();
        store.ensure_current(&mut st);
        let a = st.current.clone();
        for (id, title, updated) in [("b", "乙", 20i64), ("c", "丙", 30)] {
            st.sessions.push(SessionMeta {
                id: id.to_string(),
                title: title.to_string(),
                created_at: 1,
                updated_at: updated,
                provider: String::new(),
                archived: false,
                context_tokens: 0,
            });
        }
        store.save(&st).unwrap();
        store
            .append_turns("b", &[turn_of("user", "只有乙有历史")])
            .unwrap();
        let hist = base.join("sessions").join("b.jsonl");
        let before = std::fs::read(&hist).expect("乙的历史必须已落盘");

        store.set_archived("b", true).unwrap();
        store.set_archived("c", true).unwrap();

        let (visible, current) = store.list();
        assert_eq!(
            visible.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec![a.as_str()],
            "归档后可见列表只剩未归档的那条"
        );
        assert_eq!(current, a, "当前会话不是被归档的那条 ⇒ 不变");
        assert_eq!(
            store
                .archived()
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "b"],
            "归档列表按 updated_at 倒序（最近用过的在前）"
        );
        assert!(hist.exists(), "归档不得删除历史文件");
        assert_eq!(
            std::fs::read(&hist).unwrap(),
            before,
            "历史内容必须逐字节原样"
        );

        // 取消归档 ⇒ 回到可见列表
        store.set_archived("c", false).unwrap();
        assert!(store.archived().iter().all(|s| s.id != "c"));
        assert!(store.list().0.iter().any(|s| s.id == "c"));
    }

    /// 归档**当前**会话：current 落回最近活动的未归档会话；全归档 ⇒ 起一条新的空会话
    /// （**不擅自取消归档**用户已归档的会话），并沿用刚离开那条的模型选择。
    #[test]
    fn archiving_current_moves_current_then_all_archived_creates_new() {
        let (store, _base) = tmp_store("arch-current");
        let mut st = store.load();
        st.sessions = vec![
            SessionMeta {
                id: "a".to_string(),
                title: "甲".to_string(),
                created_at: 1,
                updated_at: 10,
                provider: String::new(),
                archived: false,
                context_tokens: 0,
            },
            SessionMeta {
                id: "b".to_string(),
                title: "乙".to_string(),
                created_at: 2,
                updated_at: 20,
                provider: "deepseek".to_string(),
                archived: false,
                context_tokens: 0,
            },
        ];
        st.current = "a".to_string();
        store.save(&st).unwrap();

        store.set_archived("a", true).unwrap();
        let (visible, current) = store.list();
        assert_eq!(
            current, "b",
            "归档当前会话 ⇒ current 落到最近活动的未归档会话"
        );
        assert_eq!(
            visible.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["b"]
        );

        // 再归档 b ⇒ 一条不剩。**不得**为了腾地方把用户的归档反悔掉，只能新起一条。
        store.set_archived("b", true).unwrap();
        let (visible, current) = store.list();
        assert_eq!(
            visible.len(),
            1,
            "全归档后必须有一条新会话，否则消息写进看不见的会话"
        );
        assert!(current != "a" && current != "b", "current 必须是新起的那条");
        assert_eq!(
            visible[0].provider, "deepseek",
            "新会话要沿用刚离开那条的模型选择（别因归档丢掉用户的选择）"
        );
        assert_eq!(
            store.archived().len(),
            2,
            "两条用户会话都还在归档里（不得擅自取消归档）"
        );
    }

    /// 「值本来就是它 / id 不存在」⇒ `set_archived` **不落盘**。
    ///
    /// 判据做成手写格式的索引文件：若它偷偷 `save`，文件会被 serde 重排（pretty），断言即红；
    /// 反向（真的改值）必须落盘——否则上面那条是空判据。
    #[test]
    fn same_value_set_archived_does_not_write_index() {
        let (store, base) = tmp_store("arch-noop");
        std::fs::create_dir_all(&base).unwrap();
        let hand = r#"{"current":"b","sessions":[{"id":"b","title":"乙","created_at":1,"updated_at":2,"provider":"","archived":true}]}"#;
        std::fs::write(base.join("sessions.json"), hand).unwrap();

        store.set_archived("b", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(base.join("sessions.json")).unwrap(),
            hand,
            "同值 set_archived 不得写盘"
        );
        store.set_archived("没有这个 id", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(base.join("sessions.json")).unwrap(),
            hand,
            "id 不存在 ⇒ 幂等忽略（不写盘、不报错）"
        );

        // 阳性对照：改值一定会落盘（重排成 serde pretty ⇒ 与手写格式不同）
        store.set_archived("b", false).unwrap();
        assert_ne!(
            std::fs::read_to_string(base.join("sessions.json")).unwrap(),
            hand,
            "改值必须落盘（否则上面两条断言都验不到东西）"
        );
    }

    /// 旧 `sessions.json`（没有 `archived` 键）按未归档读入；写回时该位显式落盘
    /// （serde 不 skip ⇒ 人工可核对；旧版 ABB 读新文件时忽略未知字段，不会读坏）。
    #[test]
    fn old_sessions_json_without_archived_loads_unarchived() {
        let (store, base) = tmp_store("arch-legacy");
        std::fs::create_dir_all(&base).unwrap();
        let legacy = r#"{"current":"s1","sessions":[{"id":"s1","title":"老会话","created_at":1,"updated_at":2,"provider":""}]}"#;
        std::fs::write(base.join("sessions.json"), legacy).unwrap();

        let (list, current) = store.list();
        assert_eq!(current, "s1");
        assert_eq!(list.len(), 1);
        assert!(!list[0].archived, "缺 archived 字段 ⇒ 未归档（向后兼容）");

        store.set_archived("s1", true).unwrap();
        let txt = std::fs::read_to_string(base.join("sessions.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&txt).expect("索引必须是合法 JSON");
        let arr = v["sessions"].as_array().expect("sessions 必须是数组");
        assert_eq!(
            arr.iter().find(|s| s["id"] == "s1").unwrap()["archived"],
            serde_json::Value::Bool(true),
            "写回必须带 archived 这一位：{txt}"
        );
        assert_eq!(arr.len(), 2, "唯一会话被归档 ⇒ 索引里新起一条：{txt}");
    }
    /// 空会话**不归档**（owner 2026-10-15 实测：25 条会话里 20 条是被这么造出来的空壳）。
    ///
    /// 判据两层：
    /// - 正面：空会话 ⇒ `archived == false` 且**索引一字未动**（不得新建、不得落盘）；
    /// - 反向控制：同一会话一旦有内容，就能真正归档（否则上面那条可能只是「函数恒不归档」）。
    #[test]
    fn archive_current_refuses_empty_session_but_archives_content() {
        let (store, base) = tmp_store("arch-current-empty");
        std::fs::create_dir_all(&base).unwrap();
        let mut st = store.load();
        st.sessions = vec![SessionMeta {
            id: "empty".into(),
            title: "新会话".into(),
            created_at: 1,
            updated_at: 1,
            provider: String::new(),
            archived: false,
            context_tokens: 0,
        }];
        st.current = "empty".into();
        store.save(&st).unwrap();
        let before = std::fs::read_to_string(base.join("sessions.json")).unwrap();

        let out = archive_current_locked(&store).unwrap();
        assert!(!out.archived, "空会话不得被归档");
        assert_eq!(out.title, "新会话");
        assert_eq!(
            std::fs::read_to_string(base.join("sessions.json")).unwrap(),
            before,
            "拒绝归档时索引必须一字未动（不得新建会话、不得落盘）"
        );

        // 反向控制：给它一句内容，就必须能归档
        store
            .append_turns("empty", &[turn_of("user", "你好")])
            .unwrap();
        let out = archive_current_locked(&store).unwrap();
        assert!(out.archived, "有内容的会话必须能归档");
        assert!(
            store.archived().iter().any(|s| s.id == "empty"),
            "被归档的会话应出现在归档列表里"
        );
    }
    /// 「清空」必须把索引也复位成**空会话形态**——否则列表里留下「有旧标题、没内容」的
    /// 幽灵会话（2026-10-15 实测：`hi` / `😀` 两条归档会话就是这么来的——有轮次 ⇒ 标题
    /// 已落进索引；后来在对话窗点「清空」⇒ 历史文件没了、索引没动）。
    #[test]
    fn clear_session_resets_title_so_no_titled_ghost_remains() {
        let (store, base) = tmp_store("clear-ghost");
        std::fs::create_dir_all(&base).unwrap();
        let mut st = store.load();
        st.sessions = vec![SessionMeta {
            id: "s1".into(),
            title: "hi".into(),
            created_at: 1,
            updated_at: 2,
            provider: "p".into(),
            archived: false,
            context_tokens: 1234,
        }];
        st.current = "s1".into();
        store.save(&st).unwrap();
        store.append_turns("s1", &[turn_of("user", "hi")]).unwrap();
        assert_eq!(store.read_history("s1").len(), 1, "前提：清空前有历史");

        clear_session_locked(&store, "s1").unwrap();
        assert!(store.read_history("s1").is_empty(), "历史应被清空");
        let st2 = store.load();
        let s = st2
            .sessions
            .iter()
            .find(|s| s.id == "s1")
            .expect("会话条目必须还在（换 id 会把 current 弄丢）");
        assert_eq!(s.title, "新会话", "标题必须回到空会话形态，否则就是幽灵");
        assert_eq!(s.context_tokens, 0, "上下文用量应归零");
        assert_eq!(s.provider, "p", "模型选择不动");
        assert!(
            is_empty_session(&store.read_history("s1")),
            "清空后必须被判为空会话（不再能被归档）"
        );
        // 幂等：再清一次不报错、不产生新条目
        clear_session_locked(&store, "s1").unwrap();
        assert_eq!(store.load().sessions.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }
    /// 相对路径后缀回退：文档写在子目录、消息里只写 `assets/x.svg` 时要能找到。
    /// 唯一匹配才认；多个候选不猜（宁可走「打开」卡片，也不给错图）。
    #[test]
    fn find_by_suffix_matches_unique_nested_asset() {
        let base = std::env::temp_dir().join(format!("abb-suffix-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("md-demo/assets")).unwrap();
        std::fs::write(base.join("md-demo/assets/demo.svg"), b"x").unwrap();
        assert_eq!(
            find_by_suffix(&base, "assets/demo.svg"),
            Some(base.join("md-demo/assets/demo.svg")),
            "唯一候选应被采用"
        );
        // 逻辑判据用 `_uncached`（`find_by_suffix` 带 memo，同一 key 不会重算）
        let w: Vec<&str> = vec!["assets", "demo.svg"];
        assert_eq!(
            find_by_suffix_uncached(&base, &w),
            Some(base.join("md-demo/assets/demo.svg"))
        );
        // 多个候选 ⇒ 不猜
        std::fs::create_dir_all(base.join("other/assets")).unwrap();
        std::fs::write(base.join("other/assets/demo.svg"), b"x").unwrap();
        assert_eq!(find_by_suffix_uncached(&base, &w), None);
        // 隐藏目录不参与（`.git` 之类不该被当素材源）
        std::fs::create_dir_all(base.join(".hidden/assets")).unwrap();
        std::fs::write(base.join(".hidden/assets/only.svg"), b"x").unwrap();
        assert_eq!(
            find_by_suffix_uncached(&base, &["assets", "only.svg"]),
            None
        );
        // memo：同一 key 第二次直接命中（即使此刻磁盘上已是「多候选」也返回首次结果——
        // 行重建时反复问的就是同一个 key，这里钉住「不重复遍历」）
        assert_eq!(
            find_by_suffix(&base, "assets/demo.svg"),
            Some(base.join("md-demo/assets/demo.svg"))
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
