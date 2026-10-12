//! 系统级对话（全局锚点气泡 / 无头 CLI 共用）：直接与随包 `abb-agent` 对话的后端。
//!
//! ## 为什么单独成模块（不塞进 `ui.rs`）
//!
//! ABB 可以**无头运行**（`--service`，见 AGENTS.md）：系统级对话必须不依赖托盘 GUI
//! 进程才能工作，气泡只是它的一个前端。故回合逻辑收在此处，由两处共用：
//!
//! - GUI：`AnchorBubble` 点击 → `SystemChatWindow` → [`turn`]（跑在 GUI 的后台 tokio 线程）；
//! - CLI：`agent-bridge chat …` → [`run_chat_cli`]（无头/脚本可用，不需任何窗口）。
//!
//! ## 多轮语义
//!
//! [`crate::runtime::oneshot::oneshot_turn`] 每次都是**新 ACP 会话**（用完即拆栈），
//! 进程间也没有可借的常驻句柄。于是「记忆」由本模块自己维护：把最近若干轮历史拼进
//! 每轮的 prompt，历史落盘 `~/.agent-bridge/syschat/history.jsonl`，跨重启保留。
//!
//! 这**不是**服务端会话态，只是上下文重放——因此有明确上界（[`HISTORY_KEEP_TURNS`]），
//! 免得每轮 token 成本线性增长。历史文件也可能被 GUI 与 CLI 同时写，故读写都在
//! `history.lock` 独占锁内完成（fs2：Unix flock / Windows LockFileEx）。

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
[系统级对话] 你正通过 ABB 的全局气泡 / 命令行与用户直接对话（非任何 IM 渠道）。\n\
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

/// 历史文件路径。
pub fn history_path() -> PathBuf {
    dir().join("history.jsonl")
}

/// 气泡位置文件（GUI 专用；后端不读）。
pub fn bubble_pos_path() -> PathBuf {
    dir().join("bubble.json")
}

/// 系统级会话的工作区：`~/.agent-bridge/workspaces/syschat/`。
pub fn workspace() -> PathBuf {
    crate::workspace_dir(SYSCHAT_KEY)
}

/// 气泡位置（逻辑像素）。持久化在 `bubble.json`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BubblePos {
    pub x: i32,
    pub y: i32,
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

/// 历史独占锁：拿锁 → 读改写 → 释放。持有期间 GUI/CLI 的其它回合会排队，
/// 避免「两个回合各自基于同一份旧历史拼 prompt」的丢更新。
struct HistoryLock {
    file: std::fs::File,
}

impl HistoryLock {
    fn acquire() -> std::io::Result<Self> {
        std::fs::create_dir_all(dir())?;
        let lock_path = dir().join("history.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600));
        }
        use fs2::FileExt;
        // 拿不到锁不静默降级（与 deliver 的降级取向不同：那是"宁偶发竞态不丢请求"，
        // 这里是对话，宁可报错让用户重试，也不并发写坏历史）。阻塞拿锁即可。
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        // 显式限定到 fs2（std 1.89 也提供 File::unlock，混用会让“用了哪套”变得不清楚）。
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// 读全部历史（坏行跳过，不让一条脏数据废掉整个会话）。
pub fn load_history() -> Vec<Turn> {
    let Ok(file) = std::fs::File::open(history_path()) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Turn>(&l).ok())
        .collect()
}

/// 追加若干轮到历史文件（open append + 逐行 JSON）。
fn append_turns(turns: &[Turn]) -> std::io::Result<()> {
    if turns.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir())?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(history_path(), std::fs::Permissions::from_mode(0o600));
    }
    for t in turns {
        let line = serde_json::to_string(t).unwrap_or_default();
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// 清空历史（`agent-bridge chat --reset` / 对话窗「清空」）。
pub fn clear_history() -> std::io::Result<()> {
    let _guard = HistoryLock::acquire()?;
    match std::fs::remove_file(history_path()) {
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

/// 系统级 agent 配置：全局默认供应商 + owner 全权限档（无沙箱）+ 专用工作区。
pub fn agent_config(cfg: &Config) -> AgentConfig {
    crate::service::oneshot_agent_config_system(cfg)
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

/// 跑一轮系统级对话：读历史 → 拼 prompt → 落用户轮 → 跑回合 → 落助手轮。
///
/// 全程持有历史独占锁（跨 GUI/CLI 进程）。回合本身在锁内跑：宁可让并发回合排队，
/// 也不能让两个回合各自基于同一份旧历史拼 prompt（那会丢更新、乱序）。
pub async fn turn(cfg: &Config, user_text: &str) -> Result<String, String> {
    let text = user_text.trim();
    if text.is_empty() {
        return Err("消息为空。".to_string());
    }
    let agent_cfg = agent_config(cfg);
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

    let _guard = HistoryLock::acquire().map_err(|e| format!("锁定对话历史失败：{e}"))?;
    let history = load_history();
    let prompt = build_prompt(&history, text);
    // 先落用户轮：即使回合失败，用户说过的话也不该丢（下次重启仍能看到自己的提问）。
    append_turns(&[Turn {
        role: "user".to_string(),
        text: text.to_string(),
        ts: crate::chrono_lite::unix_secs() as i64,
    }])
    .map_err(|e| format!("写入对话历史失败：{e}"))?;

    let reply = turn_with(agent_cfg, ws, prompt).await?;

    append_turns(&[Turn {
        role: "assistant".to_string(),
        text: reply.clone(),
        ts: crate::chrono_lite::unix_secs() as i64,
    }])
    .map_err(|e| format!("写入对话历史失败：{e}"))?;
    Ok(reply)
}

/// `agent-bridge chat` 的已解析参数（纯数据，单测直接断言）。
#[derive(Debug, Default, PartialEq, Eq)]
struct ChatArgs {
    reset: bool,
    json: bool,
    help: bool,
    /// 文本（多个位置参数用空格拼接）；None = 未给（走 stdin）。
    text: Option<String>,
}

/// 解析 `chat` 子命令参数。未知 `--` 参数报错（不静默吞：脚本拼错参数却“成功”最坑）。
fn parse_chat_args(args: &[String]) -> Result<ChatArgs, String> {
    let mut out = ChatArgs::default();
    let mut parts: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--reset" => out.reset = true,
            "--json" => out.json = true,
            "--help" | "-h" => out.help = true,
            other if other.starts_with("--") => return Err(format!("未知参数：{other}")),
            other => parts.push(other.to_string()),
        }
    }
    if !parts.is_empty() {
        out.text = Some(parts.join(" "));
    }
    Ok(out)
}

/// `agent-bridge chat` 的入口（无头可用）。返回进程退出码。
///
/// 用法：
///   agent-bridge chat [--reset] [--json] [文本]
///   文本缺省时从 stdin 读（管道/脚本用）；读到空 → 用法错误。
pub fn run_chat_cli(args: &[String]) -> i32 {
    let parsed = match parse_chat_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            print_chat_usage();
            return 2;
        }
    };
    if parsed.help {
        print_chat_usage();
        return 0;
    }

    if parsed.reset {
        match clear_history() {
            Ok(()) => {
                if parsed.json {
                    println!("{{\"reset\":true}}");
                } else {
                    println!("已清空系统对话历史。");
                }
                return 0;
            }
            Err(e) => {
                eprintln!("清空历史失败：{e}");
                return 1;
            }
        }
    }

    let text = match parsed.text {
        Some(t) => t,
        None => {
            use std::io::Read as _;
            let mut buf = String::new();
            if std::io::stdin().read_to_string(&mut buf).is_err() {
                eprintln!("读取 stdin 失败。");
                return 1;
            }
            buf
        }
    };
    if text.trim().is_empty() {
        eprintln!("没有要发送的内容。");
        print_chat_usage();
        return 2;
    }

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读取配置失败：{e:#}");
            return 1;
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("初始化运行时失败：{e}");
            return 1;
        }
    };
    match rt.block_on(turn(&cfg, &text)) {
        Ok(reply) => {
            if parsed.json {
                let v = serde_json::json!({ "reply": reply });
                println!("{v}");
            } else {
                println!("{reply}");
            }
            0
        }
        Err(e) => {
            eprintln!("对话失败：{e}");
            1
        }
    }
}

fn print_chat_usage() {
    eprintln!(
        "用法：agent-bridge chat [--reset] [--json] [文本]\n\
         \n\
         与系统级 AI 助手（ABB 自带的 abb-agent）直接对话，不经任何 IM 渠道。\n\
         不给文本时从 stdin 读取；--reset 清空历史；--json 以 JSON 输出回复。"
    );
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

    // ── CLI 参数解析（纯函数，不碰文件/网络）──

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_chat_args_plain_text_is_joined() {
        let p = parse_chat_args(&args(&["帮我", "看看磁盘"])).unwrap();
        assert!(!p.reset && !p.json && !p.help);
        assert_eq!(p.text.as_deref(), Some("帮我 看看磁盘"));
    }

    #[test]
    fn parse_chat_args_flags() {
        let p = parse_chat_args(&args(&["--reset"])).unwrap();
        assert!(p.reset && !p.json);
        assert_eq!(p.text, None);
        let p = parse_chat_args(&args(&["--json", "hi"])).unwrap();
        assert!(p.json);
        assert_eq!(p.text.as_deref(), Some("hi"));
        assert!(parse_chat_args(&args(&["-h"])).unwrap().help);
    }

    #[test]
    fn parse_chat_args_unknown_flag_is_error() {
        let e = parse_chat_args(&args(&["--bogus"])).expect_err("未知 -- 参数应报错");
        assert!(e.contains("未知参数"), "错误文案：{e}");
        // 单破折号不是长参数，当文本处理（兼容负数/短横线开头的内容）
        let p = parse_chat_args(&args(&["-v"])).unwrap();
        assert_eq!(p.text.as_deref(), Some("-v"));
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

    /// 无头路径的核心断言：装配好的 prompt 真送到 agent，回程文本原样返回。
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
}
