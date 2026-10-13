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
pub async fn turn_with(
    agent_cfg: AgentConfig,
    workspace: PathBuf,
    prompt: String,
) -> Result<String, String> {
    let msg = InboundMsg {
        id_hex: uuid::Uuid::new_v4().to_string(),
        author_role: crate::config::SenderRole::Owner.as_str().to_string(),
        text: prompt,
        ts_secs: crate::chrono_lite::unix_secs() as i64,
        prompt_tag: "syschat".to_string(),
    };
    let outcome = crate::runtime::oneshot::oneshot_turn(
        agent_cfg,
        Some(workspace.display().to_string()),
        msg,
        TURN_BUDGET,
        None,
    )
    .await;
    match outcome {
        SyncTurnOutcome::Ok(text) => {
            if text.trim().is_empty() {
                Err("模型返回了空回复。".to_string())
            } else {
                Ok(text)
            }
        }
        SyncTurnOutcome::Timeout => Err(format!(
            "对话超时（{}s）。可重试或把问题拆小。",
            TURN_BUDGET.as_secs()
        )),
        SyncTurnOutcome::Closed => Err("agent 不可用（会话未建起）。".to_string()),
        SyncTurnOutcome::Failed(reason) => Err(reason),
        SyncTurnOutcome::Cancelled => Err("对话已中断。".to_string()),
    }
}

/// 跑一轮系统级对话（生产入口）：按**当前会话**（与其模型选择）装配 agent，再交给
/// [`turn_core`] 跑会话/历史逻辑。模型选择在**本轮开头**解析一次 ⇒「切换从下一条消息起生效」。
pub async fn turn(cfg: &Config, user_text: &str) -> Result<String, String> {
    turn_with_attachments(cfg, user_text, &[]).await
}

/// 带待发附件的一轮系统级对话（C 批：📎 / ⌘V）。
///
/// 附件只以**本地路径**进 prompt（见 [`prompt_with_attachments`]），落盘与去重已在
/// [`land_attachments`] 完成；历史仍只记用户输入的文本——后续回合重放的是对话，不是
/// 附件清单（同一条附件不会因为历史重放而被反复描述）。
pub async fn turn_with_attachments(
    cfg: &Config,
    user_text: &str,
    atts: &[Attachment],
) -> Result<String, String> {
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

    turn_core(&store, &session_id, agent_cfg, ws, text, atts).await
}

/// 可测核心：会话 store / agent 配置 / 工作区都由调用方注入（测试用临时目录 + mock agent，
/// 不碰真实数据目录，满足隔离门禁）。
///
/// 历史锁只包住两段短临界区（读史+落用户轮 / 落助手轮），**回合在锁外跑**：
/// 回合最长 10 分钟，持锁跨回合会阻塞 GUI 主线程的清空/重开（同一个 `history.lock`）。
async fn turn_core(
    store: &SessionStore,
    session_id: &str,
    agent_cfg: AgentConfig,
    ws: PathBuf,
    user_text: &str,
    atts: &[Attachment],
) -> Result<String, String> {
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

    let reply = turn_with(agent_cfg, ws, prompt).await?;

    // 临界区 2：落助手轮 + 刷新会话时间。
    {
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
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_of(role: &str, text: &str) -> Turn {
        Turn {
            role: role.to_string(),
            text: text.to_string(),
            ts: 0,
        }
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

        let r1 = turn_core(
            &store,
            &s1,
            mock_agent_cfg(&rec("abb-sc-t1")),
            ws.clone(),
            "probe-one",
            &[],
        )
        .await
        .unwrap();
        assert!(r1.contains("probe-one"));
        let r2 = turn_core(
            &store,
            &s2,
            mock_agent_cfg(&rec("abb-sc-t2")),
            ws.clone(),
            "probe-two",
            &[],
        )
        .await
        .unwrap();
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
        let r1b = turn_core(
            &store,
            &s1,
            mock_agent_cfg(&rec("abb-sc-t3")),
            ws.clone(),
            "again-one",
            &[],
        )
        .await
        .unwrap();
        assert!(
            r1b.contains("probe-one"),
            "s1 第二轮须读到自己第一轮的历史：{r1b}"
        );
        assert!(
            !r1b.contains("probe-two"),
            "s1 第二轮不得读到 s2 的历史：{r1b}"
        );
        let r2b = turn_core(
            &store,
            &s2,
            mock_agent_cfg(&rec("abb-sc-t4")),
            ws.clone(),
            "again-two",
            &[],
        )
        .await
        .unwrap();
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
        let reply = turn_core(
            &store,
            &sid,
            mock_agent_cfg(&rec),
            ws.clone(),
            "看这张",
            std::slice::from_ref(&att),
        )
        .await
        .unwrap();
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
}
