//! agent 执行辅助 —— 单后端化（buzz harness 唯一执行层）时代的残留面。
//!
//! P4.1 大删除：CLI spawn 形态（claude -p / codex exec / pi --mode json）整体移除——
//! `Backend` 枚举、`run`/`run_once`/`process_line`、三后端命令构造族、`build_injection`
//! 注入矩阵、`RealAgentRunner` 全部删除（P2 起聊天全量走 buzz harness，P3.2-P3.4
//! 归纳/角色生成/团队生成收口 oneshot_turn，生产代码已无 CLI spawn 调用方）。
//! 本模块保留：
//! - [`AgentRunner`] trait：bridge 92 个编排测试的挡板缝（**仅测试注入**；生产装配
//!   [`SpawnRetiredRunner`]——生产 dispatch 恒走 harness，该路径不可达， stub 如实
//!   报错而非 panic）；
//! - [`buzz_provider_env`] / [`provider_ready`]：harness 句柄装配的供应商 env 与
//!   预检硬闸（service / dispatch 预检消费）；
//! - [`remove_pi_transcripts`] 族：legacy pi 会话文件清理（session_gc / tidy 消费——
//!   buzz fork 不落 .pi-sessions，这些只服务存量数据）；
//! - [`ensure_workspace_guide`]：工作区指引（fork 经 hints 读 cwd AGENTS.md——
//!   job/deliver CLI 的可发现性仍靠它；调用点已迁 service 启动与 buzz dispatch）；
//! - [`generate_role_prompt`]：「✨生成」角色提示词（oneshot_turn，P3.3）；
//! - [`kill_stale_agents`]：legacy agent-pids.json 孤儿进程一次性清理（写端随
//!   run_once 删除，读端保留消化升级前残留——文件清空后永不再触发）。
//!
//! **无超时**：桥是推送模型——等 agent 跑完即回发（harness 侧语义，见 buzz/harness）。

use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;

/// 按**字符**截断（可能含中文，按字节切会落在 UTF-8 中间 panic）。日志/报错预览用。
pub fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// 在每个 bot 的 workspace 里放指引（buzz fork 经 hints 机制读 cwd 的 AGENTS.md）。
/// 关键：告诉 agent 定时/周期需求要用桥注入的 `$ABB_BIN`（本程序绝对路径）调 job CLI 建任务后
/// **立即退出**，别自己写 sleep/while 循环挂着（会一直占着该聊天，期间新消息全部排队）。
/// 版本化（GUIDE_MARKER）：老工作区里无标记的旧模板（写死 `agent-bridge job`、实际在
/// mac/win 的 agent 环境都调不到）自动覆盖升级；已含标记的文件不动（幂等）。
// P4.1：唯一生产调用点（旧 agent::run）已删；P4.4 待把写指引接进 harness dispatch 路径
// （buzz fork 经 hints 读 cwd AGENTS.md，缺指引 = $ABB_BIN job 用法无人告知）。
#[allow(dead_code)]
const GUIDE_MARKER: &str = "abb-guide-v3";

/// 写工作区指引（CLAUDE.md / AGENTS.md 同文）。幂等（marker 判定）。
/// P4.1 起生产无调用点（旧 agent::run 已删）——**P4.4 待办**：接进 harness dispatch
/// 路径（service bot 启动写 bot 级 + buzz dispatch 懒建写 vb 工作区），恢复
/// `$ABB_BIN` job 用法指引。当前仅测试调用。
#[allow(dead_code)]
pub(crate) fn ensure_workspace_guide(workspace: &std::path::Path) {
    let guide = format!(
        "# ABB 工作区（{GUIDE_MARKER}）

你在飞书/微信/钉钉 bot 的工作区里。用户消息从飞书/微信/钉钉转来；你的 stdout 末尾会作为回复发回给用户。

## 定时任务 → 用 $ABB_BIN 建任务，建完即退出

用户说「每天 X 点」「每 N 分钟」「到点提醒」「稍后」「工作日」等周期或延迟需求时，
**用桥注入的 `$ABB_BIN`（本程序绝对路径）调 job CLI 建定时任务，建完就结束**。绝不要自己写
sleep/while 循环去等待——那会一直占着这个聊天，期间用户发来的新消息全部排队收不到回复。

- 加：`\"$ABB_BIN\" job add (--once \"YYYY-MM-DD HH:MM\" | --cron \"分 时 日 月 周\") --prompt \"到点做什么\" [--note \"原句\"] [--to bot_key:chat_id]…`
  - cron 例：每分钟 `* * * * *`；每天 9:30 `30 9 * * *`；工作日 10 点 `0 10 * * 1-5`；每小时 `0 * * * *`
  - `--to` 可重复：任务结果同时投递多个会话（裸 `chat_id` = 本 bot；`bot_key:chat_id` = 跨 bot，如 `feishu:oc_xxx`）
- 列：`\"$ABB_BIN\" job list`
- 删：`\"$ABB_BIN\" job del <id 前缀>`
- 不要用裸命令名 `agent-bridge` / `abb`：macOS 在 .app 内、Windows 在安装目录，都不在 PATH，
  裸调用会 command not found。`ABB_BIN` 由桥 spawn 时注入，保证调的是当前安装的同一个程序。

目标会话与 bot 已由桥通过环境变量注入：`AGENT_BRIDGE_CHAT_ID`、`AGENT_BRIDGE_BOT_KEY`，CLI 会自动取用，无需手填。

## 跨会话投递（需在 ABB 设置里打开「跨会话投递」开关）

用户说「把结果同步到 XX 群 / 发到另一个 bot」等跨会话需求时，用 `$ABB_BIN` 调 deliver CLI 把消息
投递到**其它 bot 的会话**（跨平台路由，例如微信里的指令把结果发到飞书群）。目标 bot key 用设置里
的 bot 名称，目标 chat_id 需用户提供；来源 bot/会话由环境变量注入，无需手填。

- 投：`\"$ABB_BIN\" deliver --bot <目标bot key> --chat <目标chat_id> --text \"内容\" [--file <本地路径>]…`
  - `--file` 可重复：转发附件时带上本地路径元数据，接收端（同机）可按路径读取处理
- 投递是异步的：CLI 只入队，service 侧实际发送；失败会回源到当前会话报错，不会静默丢。
- 开关关闭时 CLI 会直接报错——提示用户先去设置打开，不要反复重试。
- **防循环**：不要把收到的跨会话消息再原样转发回去（同一来源/目标/内容 10 分钟内会被 service 抑制并回源提示）。

## 其它

- 任务完成（产出最终回复）后**立即退出**，不要持续运行或等待。
- 普通问答、查资料、改文件等直接做即可，做完输出结论。
- 你只能读写本工作区；不要假设有公网入站（消息靠桥转）。
"
    );
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let p = workspace.join(name);
        // 已存在但无版本标记（旧模板）→ 覆盖升级；已含标记 → 不动（幂等）。
        // 代价：用户自定义但没加标记的文件也会被覆盖一次——权衡后接受，
        // 旧命令在 mac/win 的 agent 环境里都不可用，宁可升级。
        let need_write = if p.exists() {
            std::fs::read_to_string(&p)
                .map(|t| !t.contains(GUIDE_MARKER))
                .unwrap_or(true)
        } else {
            true
        };
        if need_write {
            let _ = std::fs::write(&p, &guide);
        }
    }
}

/// 一次桥接执行的最终结果。
// P4.1：生产唯一构造方 RealAgentRunner 已删，现仅 MockAgentRunner（测试挡板）构造、
// spawn 回落路径消费——`#[allow(dead_code)]` 抑制「生产不构造」告警，语义留测试缝。
#[allow(dead_code)]
pub enum RunOutcome {
    /// 正常完成，附最终回复文本 + 本次运行结束时的 session_id
    /// （codex 首轮回存真实 thread_id、claude 自愈/换新后都是最终值）。
    /// bridge 用 session_id 做「mark 前校验当前槽位仍是本次会话」——运行中被
    /// /new 或 CLI `session reset` 换走的旧任务，不得把新槽位 mark 成 started（#23 审查修复）。
    /// rebuilt = 会话在对端丢失（codex no rollout found / claude No conversation found）
    /// 后以**同一 sid** 回退全新重建（#54）——bridge 据此写 pending 迁移标记，让
    /// 下一条消息注入历史（重建轮本身无法注入：注入判定发生在 run 之前）。
    Reply {
        reply: String,
        session_id: String,
        rebuilt: bool,
    },
    /// 被用户在聊天里打断（停止词）。无回复；bridge 自行发送停止提示，不 mark_started。
    Cancelled,
}

/// Agent 执行抽象（#23 测试可测性）：bridge 持 `Arc<dyn AgentRunner>`，按 `Messenger`
/// 同款注入模式。**P4.1 起本 trait 仅为测试挡板缝**：生产 dispatch 恒走 buzz harness
///（acp_handles 恒装配，见 service run_bot），Bridge 内 spawn 回落路径只剩测试驱动
/// （MockAgentRunner 驱动「任务运行中」时序的编排断言）；生产装配
/// [`SpawnRetiredRunner`]（如实报错占位）。P4.1 签名删除 `backend` 参数（Backend
/// 枚举已删——单后端世界 runner 不需要知道后端）。
#[async_trait::async_trait]
pub trait AgentRunner: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        prompt: &str,
        session_id: &str,
        resume: bool,
        chat_id: &str,
        // 会话隔离 key（话题消息 = "chat:thread"，#14）。session 存储按 key 记账
        // （桥的 ensure_with_started/mark_started_if 都用 key）——回存真实 thread_id /
        // claude 重建换 UUID 必须写同一 key 的槽位，否则话题消息永远 mark 不上、
        // 且会把话题的 thread 写进主 chat 槽位（#49 审查：codex+话题）。
        session_key: &str,
        bot_key: &str,
        role: crate::config::SenderRole,
        sessions: Option<&crate::sessions::SessionStore>,
        progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<RunOutcome, String>;
}

/// 生产占位实现（P4.1）：CLI spawn（RealAgentRunner）已删，Bridge 的 spawn 回落路径
/// 在生产不可达（dispatch 恒被 buzz harness 短路——acp_handles 由 service 常驻装配，
/// None 仅出现在测试）。如实返回错误而非 panic/unimplemented：任务治理会捕获 panic
/// 只留 stderr，用户侧无声（LESSON：后端短路臂必须全部入口成守卫）。
pub struct SpawnRetiredRunner;

#[async_trait::async_trait]
impl AgentRunner for SpawnRetiredRunner {
    async fn run(
        &self,
        _prompt: &str,
        _session_id: &str,
        _resume: bool,
        _chat_id: &str,
        _session_key: &str,
        _bot_key: &str,
        _role: crate::config::SenderRole,
        _sessions: Option<&crate::sessions::SessionStore>,
        _progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        _cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<RunOutcome, String> {
        Err("⚠️ 内部错误：agent 未装配（harness 句柄缺失），本轮无法执行——请重启 ABB 服务；若反复出现请查看服务日志。".to_string())
    }
}

/// buzz 后端（共享 ACP agent 进程）的供应商 env 装配。命名空间与 pi 的宿主映射
/// 无关：buzz-agent 的 OPENAI_COMPAT_BASE_URL / ANTHROPIC_BASE_URL 直接可指任意
/// 端点，供应商配置的 base_url 原样透传。
/// 支持 anthropic / openai-chat / openai-responses；其余 kind → Err（用户可见）。
/// provider 为 None → Ok(None)（纯继承宿主 env，旧行为——e2e 即靠宿主注入）。
pub(crate) fn buzz_provider_env(
    provider: Option<&crate::config::ProviderConfig>,
) -> Result<Option<HashMap<String, String>>, String> {
    let Some(p) = provider else {
        return Ok(None);
    };
    let mut env = HashMap::new();
    match p.kind.as_str() {
        "anthropic" => {
            env.insert("BUZZ_AGENT_PROVIDER".into(), "anthropic".into());
            env.insert("ANTHROPIC_API_KEY".into(), p.api_key.clone());
            if !p.base_url.is_empty() {
                env.insert("ANTHROPIC_BASE_URL".into(), p.base_url.clone());
            }
            if !p.model.is_empty() {
                env.insert("ANTHROPIC_MODEL".into(), p.model.clone());
            }
        }
        "openai-chat" | "openai-responses" => {
            env.insert("BUZZ_AGENT_PROVIDER".into(), "openai".into());
            env.insert("OPENAI_COMPAT_API_KEY".into(), p.api_key.clone());
            // 与 pi 不同：buzz-agent 的 base_url 就是请求端点，不是 provider 身份
            // 判定——供应商配的什么 URL 就打什么（含自定义网关/本地端点）。
            if !p.base_url.is_empty() {
                env.insert("OPENAI_COMPAT_BASE_URL".into(), p.base_url.clone());
            }
            if !p.model.is_empty() {
                env.insert("OPENAI_COMPAT_MODEL".into(), p.model.clone());
            }
            let api = if p.kind == "openai-chat" { "chat" } else { "responses" };
            env.insert("OPENAI_COMPAT_API".into(), api.into());
        }
        other => {
            return Err(format!(
                "⚠️ 供应商「{}」类型 {other} 无法用于 buzz 后端（支持 anthropic / openai-chat / openai-responses）。",
                p.name
            ))
        }
    }
    Ok(Some(env))
}

/// 生效供应商是否「可用」：存在且 API Key 非空。空 key 的供应商一路放行到
/// agent 只会得到 `Missing environment variable: AGENT_BRIDGE_MODEL_KEY` 这类
/// 内部错误（Windows 实机）——硬闸在进 agent 前拒答并引导补填。
pub(crate) fn provider_ready(p: Option<&crate::config::ProviderConfig>) -> bool {
    p.is_some_and(|p| !p.api_key.trim().is_empty())
}

/// 会话文件删除的匹配模式（pi 会话文件清理的公共判定）。
/// - [`SidMatch::InSet`]：删「匹配集合内」的文件——/new 按被轮换的旧 sid 即时清理；
/// - [`SidMatch::NotInSet`]：删「匹配集合外」的文件——孤儿清理（tidy）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidMatch {
    InSet,
    NotInSet,
}

/// 删除 `.pi-sessions` 中的会话文件（文件名 `<ts>_<sid>.jsonl`，sid 用 contains 匹配——
/// ts 是纯数字+下划线、sid 是 UUID，文件名无分隔歧义）。**legacy 数据清理**：
/// buzz fork 不落 .pi-sessions，本函数只服务单后端化之前的存量 pi 会话文件
///（session_gc 精确清理 / tidy 孤儿清理消费）。
/// `fresh_secs`：mtime 距今小于该值的文件保留（宁留不删——可能是运行中的轮次）；
/// `None` = 不查 mtime（/new 的即时清理语义：被轮换的旧 sid 不可能再被使用）。
/// mtime 读不到按新鲜保留（宁留不删）。返回删除数。
pub fn remove_pi_transcripts(
    workspace: &std::path::Path,
    sids: &std::collections::HashSet<String>,
    mode: SidMatch,
    fresh_secs: Option<u64>,
) -> usize {
    remove_transcripts_in(workspace, ".pi-sessions", fresh_secs, |path| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // 空 sid 必须跳过：name.contains("") 恒真——session_gc 精确清理（InSet）若拿到
        // 空 sid（codex 的 thread_id 为空串时 agent::run 会把占位换成 ""，审查
        // 发现）会把整个 .pi-sessions 当命中删光；live 集（NotInSet）侧已由
        // live_session_ids 过滤空值，此处统一兜底（审查修复）。
        let hit = sids
            .iter()
            .any(|sid| !sid.is_empty() && name.contains(sid.as_str()));
        if mode == SidMatch::InSet {
            hit
        } else {
            !hit
        }
    })
}

/// 两个删除函数的公共骨架：扫目录 → mtime 护栏（读不到按新鲜，宁留不删）→ 谓词判定。
/// 只删文件（目录跳过）；目录不存在/枚举失败按 0（常态：该后端无会话文件）。
fn remove_transcripts_in<F: FnMut(&std::path::Path) -> bool>(
    workspace: &std::path::Path,
    dir_name: &str,
    fresh_secs: Option<u64>,
    mut should_remove: F,
) -> usize {
    let Ok(rd) = std::fs::read_dir(workspace.join(dir_name)) else {
        return 0;
    };
    let now = crate::chrono_lite::unix_secs();
    let cutoff = fresh_secs.map(|s| now.saturating_sub(s));
    let mut removed = 0usize;
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // 读不到 mtime 按新鲜（u64::MAX > 任何 cutoff）——宁留不删。
        // 无护栏（fresh_secs=None，session_gc 转录清理）时无需 stat（审查修复：
        // 原来无条件对每个文件做一次元数据调用）。
        let mtime = match cutoff {
            Some(_) => entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs())
                })
                .unwrap_or(u64::MAX),
            None => u64::MAX,
        };
        if cutoff.is_some_and(|c| mtime > c) {
            continue; // 新鲜文件可能是运行中的轮次，宁留不删
        }
        if should_remove(&path) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 虚拟 Bot「✨ 生成」提示词（8-20 需求）：根据角色/任务名让 LLM 写系统提示词
/// （群介绍，≤100 字符）。轻量单轮问答——无会话状态。输出：首个非空行（trim）+
/// 截断 100 字符（char 安全，truncate 语义）。
///
/// 单后端化 P3.3：旧 CLI spawn 形态（claude -p / codex exec / pi --mode json）整体
/// 删除，全 bot 收口 [`crate::buzz::oneshot::oneshot_turn`]——GUI 进程没有常驻 ACP
/// 句柄（句柄只活在 service 进程），oneshot 自起自拆；agent 装配与 normal handle
/// 同链（[`crate::service::oneshot_agent_config`]），维护任务跟 bot 配置档走。
pub async fn generate_role_prompt(
    bot: &crate::config::BotConfig,
    cfg: &crate::config::Config,
    role_name: &str,
) -> Result<String> {
    let agent_cfg = crate::service::oneshot_agent_config(bot, cfg);
    let workspace = crate::workspace_dir(&bot.key());
    // 全新 bot 可能尚无工作区目录——session/new 的 cwd 必须先存在
    let _ = std::fs::create_dir_all(&workspace);
    generate_role_prompt_with(agent_cfg, &workspace, role_name).await
}

/// 可测内层：agent 配置由调用方注入（生产 = oneshot_agent_config；测试注入 mock
/// 配置走「oneshot_turn → mock 子进程」全链路）。预算 60s 与旧 CLI 超时同口径
/// （按钮点击的 UX 上界）。
async fn generate_role_prompt_with(
    agent_cfg: crate::buzz::harness::AgentConfig,
    workspace: &std::path::Path,
    role_name: &str,
) -> Result<String> {
    let sys = "你是虚拟团队的角色设计助手。根据角色/任务名称写一条飞书群聊机器人的\
              系统提示词（群介绍），要求：不超过 100 个中文字符；直接输出提示词本体，\
              不要解释、不要引号、不要“角色名：”前缀。";
    let full = format!("{sys}\n\n角色/任务名称：{role_name}");
    let msg = crate::buzz::queue::InboundMsg {
        id_hex: uuid::Uuid::new_v4().to_string(),
        author_role: crate::config::SenderRole::Owner.as_str().to_string(),
        text: full,
        ts_secs: crate::chrono_lite::unix_secs() as i64,
        prompt_tag: "role_prompt".to_string(),
    };
    let outcome = crate::buzz::oneshot::oneshot_turn(
        agent_cfg,
        Some(workspace.display().to_string()),
        msg,
        std::time::Duration::from_secs(60),
        None,
    )
    .await;
    let text = match outcome {
        crate::buzz::harness::SyncTurnOutcome::Ok(text) => text,
        crate::buzz::harness::SyncTurnOutcome::Timeout => anyhow::bail!("生成超时（60s）"),
        crate::buzz::harness::SyncTurnOutcome::Closed => {
            anyhow::bail!("agent 不可用（会话未建起）")
        }
        // 无外部取消源（oneshot_turn 第四个参数恒 None），仅为穷尽匹配
        crate::buzz::harness::SyncTurnOutcome::Cancelled => anyhow::bail!("生成已中断"),
        crate::buzz::harness::SyncTurnOutcome::Failed(reason) => anyhow::bail!("{reason}"),
    };
    let text = post_process_role_prompt(&text);
    if text.is_empty() {
        // 防御分支（审查 P3-1）：空回合文本 harness 不投递（等待者挂满预算得
        // Timeout），Ok 文本恒非空——仅「模型输出全空白 + 剥后缀后归零」的病理
        // 边角可达。空结果的主要表现形式是「生成超时（60s）」。
        anyhow::bail!("生成结果为空（后端无输出）");
    }
    Ok(text)
}

/// 角色 prompt 后处理（纯函数）：首个非空行（trim）→ 截断 100 字符（char 安全）。
/// 空输入/全空白 → 空串（调用方转错误）。独立成函数是为了让「≤100 字符进 Slint
/// 输入框」这一用户可见契约被纯测试直接锁死（截断与取行是同一组合，不再靠
/// 全链路测试顺带覆盖——mock 应答长度不受控，那层锁不住截断）。
fn post_process_role_prompt(text: &str) -> String {
    let line = first_content_line(text);
    if line.is_empty() {
        return String::new();
    }
    truncate(&line, 100)
}

/// 首个非空行（trim 后）。模型可能多给解释行，只取第一行实质内容（旧 CLI 路径同款
/// 后处理）。
fn first_content_line(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

// ─────────────────────── legacy agent 子进程 pid 清理（一次性消化升级前残留）───────
// CLI spawn 时代（P4.1 前）run_once 把 spawn 的子进程 pid 落盘到
// workspaces/<bot>/agent-pids.json；service 崩溃时残留孤儿 agent，重启恢复 pending
// 前先清掉（否则 resume 撞「already in use」）。写端（track/untrack）已随 run_once
// 删除；读端 kill_stale_agents 保留消化**升级前最后一次崩溃**的残留文件——清空后
// 文件永为空，本路径自此静默（buzz-agent 进程生命周期由 harness 自管）。
// 清理时用 ps 校验「存活且命令行是 claude/codex」再 kill，防 pid 被系统复用误杀。

static AGENT_PID_LOCK: Mutex<()> = Mutex::new(());

fn agent_pids_path(bot_key: &str) -> std::path::PathBuf {
    crate::workspace_dir(bot_key).join("agent-pids.json")
}

fn read_agent_pids(bot_key: &str) -> Vec<u32> {
    fs::read_to_string(agent_pids_path(bot_key))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_agent_pids(bot_key: &str, pids: &[u32]) {
    let path = agent_pids_path(bot_key);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(pids) {
        let _ = crate::atomic_write_text(&path, &text);
    }
}

/// 目标 pid 是否还是「本桥 spawn 的 agent」：存活且命令行匹配 claude/codex/pi。
/// Windows 无 ps 语义，直接信任 pid 文件（taskkill /F）。
fn process_is_agent(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let cmd = String::from_utf8_lossy(&o.stdout);
                cmd.contains("claude") || cmd.contains("codex") || pi_command_matches(&cmd)
            }
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        let _ = pid;
        true
    }
}

/// pi 进程命令行匹配：npm 全局 bin 的 `pi` 是指向 cli.js 的软链，ps 显示的是解析后的
/// 解释器+脚本路径（如 `node …/pi-coding-agent/dist/cli.js`）。"pi" 子串太宽（login/pid 等
/// 都会误中），按特征匹配：首 token 基名恰为 pi，或路径含 pi-coding-agent。
#[cfg(unix)]
fn pi_command_matches(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let base = std::path::Path::new(first)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_default();
    base == "pi" || cmd.contains("pi-coding-agent") || cmd.contains("@earendil-works/pi")
}

/// 启动恢复前调用：把上次残留的 agent 子进程清掉（SIGTERM / taskkill），并清空 pid 文件。
/// 返回「是否发现残留」（pid 文件非空）——#164 恢复失败计数的异常退出信号：
/// 残留 = 上次进程被强杀/崩溃（CLI spawn 时代 guard 未及清理）；单后端化后写端
/// 已删，本函数只在升级到 P4.1 后首次启动消化一次 legacy 残留，此后恒 false。
pub fn kill_stale_agents(bot_key: &str) -> bool {
    let pids = {
        let _g = AGENT_PID_LOCK.lock().unwrap();
        let pids = read_agent_pids(bot_key);
        write_agent_pids(bot_key, &[]); // 先清空：即使 kill 失败也不留旧账
        pids
    };
    if pids.is_empty() {
        return false;
    }
    for pid in pids {
        if process_is_agent(pid) {
            crate::log!("[agent] 清理上次残留 agent 子进程 pid={pid}（bot={bot_key}）");
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .creation_flags(0x0800_0000)
                    .spawn();
            }
        } else {
            crate::log!(
                "[agent] 跳过 pid={pid}（已退出或非 agent 进程，防 pid 复用误杀，bot={bot_key}）"
            );
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 后处理纯函数：首个非空行（trim）；空输入/全空白 → 空串。
    #[test]
    fn first_content_line_picks_first_non_blank() {
        assert_eq!(
            first_content_line("\n  \n  角色提示词  \n第二行"),
            "角色提示词"
        );
        assert_eq!(first_content_line("只有一行"), "只有一行");
        assert_eq!(first_content_line(""), "");
        assert_eq!(first_content_line("\n\n  \n"), "");
    }

    /// 后处理组合锁（审查 P1 修复）：「首个非空行 + 截断 100」的生产组合
    /// post_process_role_prompt 直接被锁——>100 字符首行必须截到恰好 100（char
    /// 安全，CJK 按字符计）；空/全空白归零。truncate 在此之前无任何单测。
    #[test]
    fn post_process_role_prompt_locks_line_pick_and_truncation() {
        let long_line: String = "角".repeat(150);
        let out = post_process_role_prompt(&long_line);
        assert_eq!(
            out.chars().count(),
            100,
            ">100 字符首行必须截到恰好 100（char 安全）"
        );
        assert_eq!(out, "角".repeat(100));
        // 多行：取首个非空行（trim），后随解释行不进结果
        assert_eq!(post_process_role_prompt("\n  提示词  \n解释行"), "提示词");
        // 不足 100 原样；空/全空白 → 空串
        assert_eq!(post_process_role_prompt("短"), "短");
        assert_eq!(post_process_role_prompt(""), "");
        assert_eq!(post_process_role_prompt("  \n\n "), "");
    }

    /// mock 全链路（P3.3）：generate_role_prompt_with → oneshot_turn → mock 子进程。
    /// 锁的是「链路真通」：prompt 原文（含角色名）到达 agent、应答文本进入后处理
    /// （echo 首行进结果）。**不锁**截断与后缀剥除——mock 是 legacy agent，echo
    /// 首行实为 standing context 的 `echo: <base>`（短行，长度不受本测试控制）：
    /// 截断锁在 post_process_role_prompt_locks_line_pick_and_truncation（纯函数，
    /// 长度可控）；后缀剥除锁在 P3.1 oneshot_echo_roundtrip（全量 Ok 文本上断言，
    /// 本测试只取首行，断言后缀会空转——审查 P3-3）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn generate_role_prompt_with_mock_agent_roundtrip() {
        let rec = std::env::temp_dir().join(format!(
            "abb-roleprompt-test-{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let agent_cfg = crate::buzz::harness::AgentConfig {
            command: python3,
            args: vec![script.display().to_string()],
            extra_env: vec![
                ("PATH".to_string(), crate::deps::composed_path()),
                ("MOCK_RECORD_FILE".to_string(), rec.display().to_string()),
            ],
            backend: "mock".to_string(),
            session_sandbox: None,
        };
        let ws = std::env::temp_dir().join(format!("abb-roleprompt-ws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let text = generate_role_prompt_with(agent_cfg, &ws, "测试角色")
            .await
            .expect("mock 回合应成功");
        assert!(
            text.starts_with("echo:"),
            "mock echo 首行应进入后处理结果: {text}"
        );
        // prompt 原文（含角色名）确实到达 agent
        let records: Vec<serde_json::Value> = std::fs::read_to_string(&rec)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert!(
            records.iter().any(|e| e["event"] == "prompt"
                && e["text"].as_str().unwrap_or_default().contains("测试角色")),
            "prompt 记录应含角色名: {records:?}"
        );
    }

    /// 唯一 temp workspace（每个测试独立，避免并发互踩）。
    fn temp_ws(name: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("abb-agent-sess-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 把文件 mtime 拨旧（模拟不活跃会话文件；与 tidy/session_gc 测试同款手法）。
    fn set_mtime_old(path: &std::path::Path, secs_ago: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago),
            ),
        )
        .unwrap();
    }

    #[test]
    fn remove_pi_in_set_matches_by_contains() {
        // /new 语义（fresh_secs=None，无护栏）：只删文件名包含匹配 sid 的文件
        let ws = temp_ws("pi-in");
        std::fs::create_dir_all(ws.join(".pi-sessions")).unwrap();
        let a = ws.join(".pi-sessions/1000_sid_a.jsonl");
        let b = ws.join(".pi-sessions/1000_sid_b.jsonl");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        let sids = std::collections::HashSet::from(["sid_a".to_string()]);
        assert_eq!(
            remove_pi_transcripts(&ws, &sids, SidMatch::InSet, None),
            1,
            "无护栏即时删（/new 语义）"
        );
        assert!(!a.exists() && b.exists(), "只删匹配 sid");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn remove_pi_not_in_set_keeps_live_and_fresh() {
        // tidy 孤儿语义（NotInSet + 护栏）：live 集内与新鲜文件都保留
        let ws = temp_ws("pi-out");
        std::fs::create_dir_all(ws.join(".pi-sessions")).unwrap();
        let live_f = ws.join(".pi-sessions/1000_live_s.jsonl");
        let dead_old = ws.join(".pi-sessions/1000_dead_s.jsonl");
        let dead_fresh = ws.join(".pi-sessions/1000_dead_fresh.jsonl");
        for p in [&live_f, &dead_old, &dead_fresh] {
            std::fs::write(p, "x").unwrap();
        }
        set_mtime_old(&dead_old, 30 * 3600);
        set_mtime_old(&dead_fresh, 2 * 60); // 2 分钟 < 10 分钟护栏
        let live = std::collections::HashSet::from(["live_s".to_string()]);
        assert_eq!(
            remove_pi_transcripts(&ws, &live, SidMatch::NotInSet, Some(600)),
            1,
            "只删死 sid + 旧 mtime"
        );
        assert!(live_f.exists() && dead_fresh.exists() && !dead_old.exists());
        std::fs::remove_dir_all(&ws).ok();
    }

    fn prov(name: &str, kind: &str) -> crate::config::ProviderConfig {
        crate::config::ProviderConfig {
            name: name.into(),
            kind: kind.into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-secret".into(),
            model: "some-model".into(),
        }
    }

    fn prov_bu(name: &str, kind: &str, base_url: &str) -> crate::config::ProviderConfig {
        let mut p = prov(name, kind);
        p.base_url = base_url.into();
        p
    }

    #[test]
    fn buzz_provider_env_maps_anthropic() {
        let p = prov_bu("myant", "anthropic", "https://gateway.example.com");
        let env = buzz_provider_env(Some(&p)).unwrap().unwrap();
        assert_eq!(env["BUZZ_AGENT_PROVIDER"], "anthropic");
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-secret");
        // buzz-agent 支持自定义 anthropic 网关：base_url 原样透传（区别于 pi 的固定端点）
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://gateway.example.com");
        assert_eq!(env["ANTHROPIC_MODEL"], "some-model");
        assert!(!env.contains_key("OPENAI_COMPAT_API_KEY"));
    }

    #[test]
    fn buzz_provider_env_maps_openai_chat_and_responses() {
        let p = prov_bu("ds", "openai-chat", "https://api.deepseek.com");
        let env = buzz_provider_env(Some(&p)).unwrap().unwrap();
        assert_eq!(env["BUZZ_AGENT_PROVIDER"], "openai");
        assert_eq!(env["OPENAI_COMPAT_API_KEY"], "sk-secret");
        assert_eq!(env["OPENAI_COMPAT_BASE_URL"], "https://api.deepseek.com");
        assert_eq!(env["OPENAI_COMPAT_MODEL"], "some-model");
        assert_eq!(env["OPENAI_COMPAT_API"], "chat");
        let p2 = prov_bu("rs", "openai-responses", "https://api.openai.com/v1");
        let env2 = buzz_provider_env(Some(&p2)).unwrap().unwrap();
        assert_eq!(env2["OPENAI_COMPAT_API"], "responses");
    }

    #[test]
    fn buzz_provider_env_none_and_mismatch() {
        assert!(buzz_provider_env(None).unwrap().is_none());
        let p = prov("x", "gemini");
        let err = buzz_provider_env(Some(&p)).unwrap_err();
        assert!(err.contains("gemini"), "未知 kind → 用户可见错误: {err}");
    }

    #[test]
    fn workspace_guide_upgrades_old_template() {
        // 旧模板（无版本标记、写死 agent-bridge job）→ 覆盖升级为 $ABB_BIN 版
        let dir = std::env::temp_dir().join(format!("abb-guide-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = "# ABB 工作区\n\n## 定时 / 周期 / 延迟任务 → 用 job CLI\n\n用本机 `agent-bridge job` CLI 建定时任务…\n";
        std::fs::write(dir.join("CLAUDE.md"), old).unwrap();
        std::fs::write(dir.join("AGENTS.md"), old).unwrap();

        ensure_workspace_guide(&dir);
        for name in ["CLAUDE.md", "AGENTS.md"] {
            let text = std::fs::read_to_string(dir.join(name)).unwrap();
            assert!(text.contains(GUIDE_MARKER), "{name} 应含版本标记");
            assert!(text.contains("ABB_BIN"), "{name} 应引导用 $ABB_BIN");
            assert!(
                !text.contains("`agent-bridge job`"),
                "{name} 不应再写死裸命令名"
            );
        }

        // 已是最新 → 不重写（mtime 不变，幂等）
        let m = |n: &str| std::fs::metadata(dir.join(n)).unwrap().modified().unwrap();
        let before = (m("CLAUDE.md"), m("AGENTS.md"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        ensure_workspace_guide(&dir);
        assert_eq!(before, (m("CLAUDE.md"), m("AGENTS.md")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandbox_mode_mode_serde_roundtrip_and_defaults() {
        // #163：kebab-case 反序列化 + auto 默认（旧 config 无字段兼容）+ 非 auto 落盘
        use crate::config::SandboxMode;
        // 枚举 serde：kebab-case
        let v: SandboxMode = serde_json::from_str("\"workspace-write\"").unwrap();
        assert_eq!(v, SandboxMode::WorkspaceWrite);
        let v: SandboxMode = serde_json::from_str("\"full-access\"").unwrap();
        assert_eq!(v, SandboxMode::FullAccess);
        // 未知值 → 反序列化失败（fail-closed，不静默回落）
        assert!(serde_json::from_str::<SandboxMode>("\"weird\"").is_err());
        // 序列化 roundtrip
        assert_eq!(
            serde_json::to_string(&SandboxMode::ReadOnly).unwrap(),
            "\"read-only\""
        );
        // BotConfig 无该字段 → auto（旧 config 兼容）
        let cfg: crate::config::BotConfig =
            serde_json::from_str(r#"{"name":"b1","app_id":"cli_app","app_secret":"s"}"#).unwrap();
        assert_eq!(cfg.sandbox_mode, SandboxMode::Auto);
    }

    /// SpawnRetiredRunner：生产占位必须如实报错（绝不 panic——任务治理捕获 panic
    /// 只留 stderr，用户侧无声）。
    #[tokio::test]
    async fn spawn_retired_runner_errors_not_panics() {
        let r = SpawnRetiredRunner;
        let out = r
            .run(
                "hi",
                "sid",
                false,
                "chat",
                "chat",
                "bot",
                crate::config::SenderRole::Owner,
                None,
                None,
                None,
            )
            .await;
        match out {
            Err(m) => assert!(m.contains("未装配"), "错误文案应指装配缺失: {m}"),
            Ok(_) => panic!("SpawnRetiredRunner 不得返回 Ok"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn pi_command_matches_npm_symlink_and_plain() {
        // npm 全局软链：ps 显示解析后的 node + pi-coding-agent 路径
        assert!(pi_command_matches(
            "/usr/local/bin/node /Users/x/.npm-global/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js -p --mode json"
        ));
        // 首 token 基名恰为 pi（自定义安装/PATH 直装）
        assert!(pi_command_matches("/Users/x/.local/bin/pi -p hi"));
        assert!(!pi_command_matches("/usr/bin/login -p x")); // login 含 "pi" 子串但不匹配
        assert!(!pi_command_matches("/bin/bash -c 'spid=1'")); // pid 之类含 "pi" 的无关进程
        assert!(!pi_command_matches("/usr/bin/python3 x.py"));
        assert!(!pi_command_matches("/sbin/init"));
    }

    // ---- #25 重启恢复：legacy agent 子进程 pid 文件消化 ----

    fn pid_temp_key(tag: &str) -> String {
        format!("abb-agent-pid-{tag}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn kill_stale_agents_clears_file_and_skips_non_agent() {
        let key = pid_temp_key("stale");
        // 用不可能存在的 pid：ps 校验失败 → 不应误杀、文件清空、不 panic
        write_agent_pids(&key, &[999_999]);
        kill_stale_agents(&key);
        assert!(read_agent_pids(&key).is_empty(), "清理后 pid 文件应清空");
        // 空文件再次清理是 no-op
        kill_stale_agents(&key);
        let _ = std::fs::remove_dir_all(crate::workspace_dir(&key));
    }
}
