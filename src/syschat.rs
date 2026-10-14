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
use crate::runtime::harness::{AgentConfig, SyncTurnOutcome};
use crate::runtime::queue::InboundMsg;
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
    /// Unix 秒（展示/排序用）。
    pub ts: i64,
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
}

/// `sessions.json` 落盘结构。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct SessionState {
    #[serde(default)]
    current: String,
    #[serde(default)]
    sessions: Vec<SessionMeta>,
}

/// 会话存储（可注入 base 目录，便于单测不碰真实 `~/.agent-bridge`）。
///
/// 结构：
/// - `<base>/sessions.json`：`{current, sessions:[{id,title,created_at,updated_at,provider}]}`
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
        let tmp = self.state_path().with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(state).unwrap_or_default(),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, self.state_path())?;
        Ok(())
    }

    /// 保证「至少一个会话、current 有效」；改动过返回 true。首次还把旧版单会话历史迁入默认会话。
    fn ensure_current(&self, state: &mut SessionState) -> bool {
        if !state.sessions.is_empty() {
            if state.current.is_empty() || !state.sessions.iter().any(|s| s.id == state.current) {
                state.current = state.sessions[0].id.clone();
                return true;
            }
            return false;
        }
        let id = new_session_id();
        let now = crate::chrono_lite::unix_secs() as i64;
        state.sessions.push(SessionMeta {
            id: id.clone(),
            title: "新会话".to_string(),
            created_at: now,
            updated_at: now,
            provider: String::new(),
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

    /// 取会话列表（`updated_at` 倒序）与当前会话 id。
    fn list(&self) -> (Vec<SessionMeta>, String) {
        let mut state = self.load();
        if self.ensure_current(&mut state) {
            let _ = self.save(&state);
        }
        let mut list = state.sessions.clone();
        list.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        (list, state.current)
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

/// 新建会话并置为当前；`provider` 为空时沿用当前会话的模型选择。
/// 返回新会话 id（拿不到历史锁时返回可重试错误——不静默降级）。
pub fn new_session(provider: Option<&str>) -> Result<String, String> {
    let store = SessionStore::production();
    let _g = store.lock()?;
    let mut state = store.load();
    store.ensure_current(&mut state);
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
    });
    state.current = id.clone();
    store
        .save(&state)
        .map_err(|e| format!("写入会话索引失败：{e}"))?;
    Ok(id)
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
    resolve_media_path_in(&workspace(), src)
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
/// 事件口径（winit 0.30 macOS 实现，`window_delegate.rs:367-429`）：
/// - `draggingEntered:` **一次性**把拖入的全部文件名各发一条 `HoveredFile`；
/// - `performDragOperation:` 逐文件发 `DroppedFile`；
/// - 只有「拖出窗口 / 取消」才发 `HoveredFileCancelled`。
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
    let bytes = std::fs::copy(src, &target).map_err(|e| {
        format!(
            "复制附件失败（{} → {}）：{e}",
            src.display(),
            target.display()
        )
    })?;
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

/// 已缓存的 http(s) 媒体路径（未缓存返回 None）。
pub fn media_cache_path(url: &str) -> Option<PathBuf> {
    let p = media_cache_file(url);
    p.is_file().then_some(p)
}

/// 下载 http(s) 媒体到本地缓存（已在缓存则直接返回）。
/// 供 GUI 后台线程调用；失败返回可读原因（GUI 显示为链接回退，不崩）。
pub async fn fetch_media(url: &str) -> Result<PathBuf, String> {
    let path = media_cache_file(url);
    if path.is_file() {
        return Ok(path);
    }
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

/// 系统对话窗的初始逻辑宽（= `ui/app.slint` 的 `preferred-width`，src 里已显式
/// `set_size(520, 560)`）。用途：窗口还没 realize（`Window::size()` 给 0）时算计划的兜底宽度。
/// 与 `.slint` 的一致性由 [`tests::syschat_preferred_width_matches_slint`] 守着。
pub const SYSCHAT_PREFERRED_WIDTH: f32 = 520.0;

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
pub fn clear_session(id: &str) -> std::io::Result<()> {
    let store = SessionStore::production();
    let _guard = store
        .lock()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    match std::fs::remove_file(store.history_path(id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
///（已生成部分已随 [`StreamEvent`] 逐字流出，且由 [`turn_core`] 落进历史）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Done(String),
    Stopped,
}

/// 流式正文累积器（纯逻辑，E1）：按到达顺序拼接 delta，供
/// ① GUI 打字机 ② 「停止」时把已生成文本落历史 共用。
///
/// 设计取舍：**不做增量解析**，只维护一个字符串；markdown 解析留给渲染层
///（`mdblocks::parse`）在每次渲染时做，这样累积器本身可无依赖单测。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamingBubble {
    shown: String,
    active: bool,
    stopped: bool,
}

impl StreamingBubble {
    /// 新建一个进行中的流式气泡（`active = true`）。
    pub fn start() -> Self {
        Self {
            shown: String::new(),
            active: true,
            stopped: false,
        }
    }

    /// 追加一段正文 delta。空段忽略（agent 可以发空 chunk）。
    /// 返回 `true` 表示显示文本发生变化（GUI 需重渲染）。
    pub fn push(&mut self, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }
        self.shown.push_str(delta);
        true
    }

    /// ⏹ 停止：保留已生成文本，标记不再活跃（「正在输入」收起、显示「已停止」）。
    pub fn stop(&mut self) {
        self.stopped = true;
        self.active = false;
    }

    /// 收尾去重：回合结束时以 `final_text` 为**唯一权威**替换显示文本
    ///（流式拼接与最终文本不一致时——漏片/重发/后端规范化——以最终为准，
    /// 且只渲染这一条，不会再追加第二条）。
    pub fn finish(&mut self, final_text: &str) -> &str {
        self.shown = final_text.to_string();
        self.active = false;
        &self.shown
    }

    pub fn shown(&self) -> &str {
        &self.shown
    }
}

/// 流式回合的接线（E1）：正文分片 sink + ⏹ 停止用的外部取消令牌。
///
/// `cancel` 直接作为 `oneshot_turn_streaming` 的 `external_cancel`——
/// **不新造取消机制**（触发走既有的 Timeout 同款 teardown，返回 `Cancelled`）。
struct StreamWiring {
    chunks: tokio::sync::mpsc::UnboundedSender<TurnChunk>,
    cancel: tokio_util::sync::CancellationToken,
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

/// 可测内层：给定已装配好的 agent 配置 / 工作区 / prompt，跑一轮 oneshot。
///
/// 生产路径见 [`turn`]；测试注入 mock agent（与 `teambuilder` / `runtime::oneshot`
/// 同款 `tests/mock_acp_agent.py` 装置）。
///
/// E1 起本函数是 [`turn_with_stream`] 的无 sink 薄封装（签名/语义不变：既有测试与调用方零改动）。
pub async fn turn_with(
    agent_cfg: AgentConfig,
    workspace: PathBuf,
    prompt: String,
) -> Result<String, String> {
    match turn_with_stream(agent_cfg, workspace, prompt, None).await? {
        TurnEnd::Done(text) => Ok(text),
        TurnEnd::Stopped => Err("对话已中断。".to_string()),
    }
}

/// 流式版内层回合（E1）：与 [`turn_with`] 同一装配，额外把正文 delta 经 sink
/// 实时回传，并把外部取消令牌交给 oneshot（⏹ 停止）。
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
    };
    let (chunk_tx, cancel) = match stream {
        Some(w) => (Some(w.chunks), Some(w.cancel)),
        None => (None, None),
    };
    let outcome = crate::runtime::oneshot::oneshot_turn_streaming(
        agent_cfg,
        Some(workspace.display().to_string()),
        msg,
        TURN_BUDGET,
        cancel,
        chunk_tx,
    )
    .await;
    match outcome {
        SyncTurnOutcome::Ok(text) => {
            if text.trim().is_empty() {
                Err("模型返回了空回复。".to_string())
            } else {
                Ok(TurnEnd::Done(text))
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
            "执行层不可用：找不到 `{}`。请确认已安装随包 abb-agent（发行包自带），或在本机 PATH 上提供 pi-acp。",
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
        TurnEnd::Done(reply) => Ok(reply),
        TurnEnd::Stopped => Err("对话已中断。".to_string()),
    }
}

/// 带待发附件的**流式**系统级回合（E1，GUI 打字机 + ⏹ 停止的生产入口）。
///
/// 与 [`turn_with_attachments`] 共用会话/历史逻辑，额外把正文 delta 经 `chunks` 实时
/// 回传；`cancel` 触发时返回 [`TurnEnd::Stopped`]，已生成部分已随分片流出并被落进历史。
pub async fn turn_streaming(
    cfg: &Config,
    user_text: &str,
    atts: &[Attachment],
    chunks: tokio::sync::mpsc::UnboundedSender<TurnChunk>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<TurnEnd, String> {
    let (store, session_id, agent_cfg, ws, text) = prepare_turn(cfg, user_text)?;
    turn_core(
        &store,
        &session_id,
        agent_cfg,
        ws,
        &text,
        atts,
        Some(StreamWiring { chunks, cancel }),
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

    // 回合在锁外跑。返回 `(要落历史的助手文本, 是否用户停止)`：
    // - 正常完成 → 最终文本就是 `final_text`（去重口径的权威侧）；
    // - 用户停止 → 用本地累积的已生成文本（GUI 侧已有同一份，不重复发）。
    let (reply, stopped) = match stream {
        None => {
            // 非流式：cancel 恒 None ⇒ 不会 Stopped；turn_with 直接给最终文本。
            let reply = turn_with(agent_cfg, ws, prompt).await?;
            (Some(reply), false)
        }
        Some(w) => {
            // 中间层：sink 的每条分片既累积（停止时用）又转发给 GUI（`w.chunks`）。
            let acc = std::sync::Arc::new(std::sync::Mutex::new(StreamingBubble::start()));
            let (inner_tx, mut inner_rx) = tokio::sync::mpsc::unbounded_channel::<TurnChunk>();
            let gui = w.chunks.clone();
            let acc2 = acc.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(chunk) = inner_rx.recv().await {
                    if chunk.kind == ChunkKind::Text {
                        if let Ok(mut b) = acc2.lock() {
                            b.push(&chunk.text);
                        }
                    }
                    // GUI 侧收端若提前消失（窗口关了），不必中断回合——只是不再累积之外的多余投递。
                    let _ = gui.send(chunk);
                }
            });
            let res = turn_with_stream(
                agent_cfg,
                ws,
                prompt,
                Some(StreamWiring {
                    chunks: inner_tx,
                    cancel: w.cancel,
                }),
            )
            .await;
            // 回合已收尾 ⇒ sink 已摘除，转发任务随之结束；等它把缓冲分片冲完再决定停止态文本。
            let _ = forwarder.await;
            match res? {
                TurnEnd::Done(reply) => (Some(reply), false),
                TurnEnd::Stopped => {
                    let partial = acc
                        .lock()
                        .map(|b| b.shown().to_string())
                        .unwrap_or_default();
                    // 一个分片都没流出（极早停止）：不落空的助手轮。
                    let partial = (!partial.trim().is_empty()).then_some(partial);
                    (partial, true)
                }
            }
        }
    };

    // 临界区 2：落助手轮 + 刷新会话时间（停止时落的是已生成部分）。
    if let Some(reply) = &reply {
        let _g = store.lock()?;
        let now = crate::chrono_lite::unix_secs() as i64;
        store
            .append_turns(
                session_id,
                &[Turn {
                    role: "assistant".to_string(),
                    text: reply.clone(),
                    ts: now,
                }],
            )
            .map_err(|e| format!("写入对话历史失败：{e}"))?;
        let mut state = store.load();
        if let Some(s) = state.sessions.iter_mut().find(|s| s.id == session_id) {
            s.updated_at = now;
        }
        let _ = store.save(&state);
    }
    Ok(if stopped {
        TurnEnd::Stopped
    } else {
        TurnEnd::Done(reply.unwrap_or_default())
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

    /// 拖出/取消：收高亮 + 清去重集（下一轮重新可落）。
    #[test]
    fn drop_tracker_cancel_resets() {
        let mut t = DropTracker::new();
        t.hovered();
        t.cancelled();
        assert!(!t.hovering());
        t.hovered();
        assert!(t.dropped(std::path::Path::new("/tmp/a.png")));
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
        let reply = turn_with(mock_agent_cfg(&rec), ws, prompt)
            .await
            .expect("mock echo 回合应 Ok");
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
            }),
        )
        .await
        .expect("mock echo 回合应 Done");
        let TurnEnd::Done(text) = end else {
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

    // ── E1 流式累积/去重/停止态（纯逻辑，GUI 与历史共用）──

    #[test]
    fn streaming_bubble_accumulates_in_order() {
        let mut b = StreamingBubble::start();
        assert!(b.push("打"));
        assert!(b.push("字"));
        assert!(!b.push(""), "空 delta 不算变化");
        assert_eq!(b.shown(), "打字");
        assert!(b.active);
        assert!(!b.stopped);
    }

    /// 去重口径：回合结束时以最终文本为唯一权威替换显示文本（不一致时不拼接、不重复）。
    #[test]
    fn streaming_bubble_finish_replaces_with_final_text() {
        let mut b = StreamingBubble::start();
        b.push("已流出的一部分");
        assert_eq!(b.finish("最终完整文本"), "最终完整文本");
        assert_eq!(b.shown(), "最终完整文本", "不得保留旧拼接");
        assert!(!b.active);
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
        b.push("一样");
        assert_eq!(b.finish("一样"), "一样");
        assert_eq!(b.shown(), "一样");
    }

    #[test]
    fn streaming_bubble_stop_keeps_generated_text() {
        let mut b = StreamingBubble::start();
        b.push("生成到一半");
        b.stop();
        assert_eq!(b.shown(), "生成到一半", "停止必须保留已生成文本");
        assert!(b.stopped);
        assert!(!b.active);
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
            TurnEnd::Done(t) => t,
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
        let cases: &[((i32, i32, i32, i32), (i32, i32), (i32, i32, i32, i32))] = &[
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
        assert_eq!(wx2, 0 + BS.0 + BUBBLE_GAP, "前提：确实翻了右侧");
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

    /// 输入区占位按钮（😊 表情 / ✂ 截图 / ⋯ 更多）必须是「有反馈的占位」，**不能是死按钮**：
    /// 三个图标都要接 `composer-todo(...)`，且 Rust 侧要注册 `on_composer_todo`（写状态行）。
    /// 源码扫描守卫（同 `titlebar_plan_button_count_matches_show_maximize` 的手法）：
    /// 按下没反应属于「做了个空壳」，外部观测不到，只能把接线钉在源码上。
    ///
    /// 判别力（负向）：去掉任一 `composer-todo(...)` 接线（改成空实现 / 删掉）⇒ 本用例红。
    #[test]
    fn composer_placeholder_buttons_wire_todo_callback() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        for label in ["表情", "截图", "更多（含定时发送）"] {
            assert!(
                raw.contains(&format!("root.composer-todo(\"{label}\")")),
                "占位按钮「{label}」没接 composer-todo（会变成点了没反应的死按钮）"
            );
        }
        // Rust 侧的接线（把占位提示写进既有状态行通道）。
        let rust = crate::platform::src_lf(include_str!("ui.rs"));
        assert!(
            rust.contains("on_composer_todo"),
            "Rust 侧没注册 on_composer_todo —— 占位按钮的状态行提示发不出来"
        );
    }

    /// `SYSCHAT_PREFERRED_WIDTH` 必须等于 `ui/app.slint` 的 `preferred-width`
    /// （跨文件不变量；窗口未 realize 时计划就按它算，漂移会让首帧按另一个宽度落位）。
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
}
