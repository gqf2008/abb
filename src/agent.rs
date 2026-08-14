//! 调本机 agent —— claude / codex / pi，host 直跑。
//! prompt 走 stdin（避免多行/-开头被 argparse 误判）；per-chat 串行由 bridge 保证；
//! claude 注入 CC Switch 当前 provider 的 ANTHROPIC_* env；codex 走自己 ~/.codex 登录态；
//! pi 走自己 ~/.pi 登录态（配了供应商则注入对应 API key env + --provider/--model）。
//! pi 用法：`pi -p --mode json --session-id <uuid>`（--session-id 已存在即续聊、不存在即新建），
//! prompt 走 stdin（pi 非交互模式会读管道 stdin 作首条消息，见 pi main.js readPipedStdin）。
//! **无超时**：桥是推送模型——等 agent 跑完即回发，跑多久等多久（曾设 600s 上限，
//! 会把合法的长任务拦腰杀掉，用户拍板去掉，2026-08-07）。

use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;
use tokio::io::AsyncWriteExt;

/// 按**字符**截断（可能含中文，按字节切会落在 UTF-8 中间 panic）。日志/报错预览用。
pub fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Claude,
    Codex,
    Pi,
}

impl Backend {
    pub fn parse(s: &str) -> Backend {
        if s.eq_ignore_ascii_case("codex") {
            Backend::Codex
        } else if s.eq_ignore_ascii_case("pi") {
            Backend::Pi
        } else {
            Backend::Claude
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Codex => "codex",
            Backend::Pi => "pi",
        }
    }
}

/// 在每个 bot 的 workspace 里放指引（claude 读 CLAUDE.md、codex 读 AGENTS.md、pi 两者都读）。
/// 关键：告诉 agent 定时/周期需求要用桥注入的 `$ABB_BIN`（本程序绝对路径）调 job CLI 建任务后
/// **立即退出**，别自己写 sleep/while 循环挂着（会一直占着该聊天，期间新消息全部排队）。
/// 版本化（GUIDE_MARKER）：老工作区里无标记的旧模板（写死 `agent-bridge job`、实际在
/// mac/win 的 agent 环境都调不到）自动覆盖升级；已含标记的文件不动（幂等）。
const GUIDE_MARKER: &str = "abb-guide-v3";

fn ensure_workspace_guide(workspace: &std::path::Path) {
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
pub enum RunOutcome {
    /// 正常完成，附最终回复文本 + 本次运行结束时的 session_id
    /// （codex 首轮回存真实 thread_id、claude 自愈/换新后都是最终值）。
    /// bridge 用 session_id 做「mark 前校验当前槽位仍是本次会话」——运行中被
    /// /new 或 CLI `session reset` 换走的旧任务，不得把新槽位 mark 成 started（#23 审查修复）。
    Reply { reply: String, session_id: String },
    /// 被用户在聊天里打断（停止词）。无回复；bridge 自行发送停止提示，不 mark_started。
    Cancelled,
}

/// Agent 执行抽象（#23 测试可测性）：bridge 持 `Arc<dyn AgentRunner>`，按 `Messenger`
/// 同款注入模式。生产用 `RealAgentRunner`（转发下面的自由函数 `run`，spawn 真实 claude/codex
/// 子进程）；测试注入挡板以驱动「任务运行中」时序——真实 `run` 在测试环境无 claude/codex
/// 二进制只能走 Err 分支，覆盖不到 `Ok(Reply)` 路径上的 pending_new / mark_started 编排。
#[async_trait::async_trait]
pub trait AgentRunner: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        backend: Backend,
        prompt: &str,
        session_id: &str,
        resume: bool,
        chat_id: &str,
        bot_key: &str,
        role: crate::config::SenderRole,
        sessions: Option<&crate::sessions::SessionStore>,
        progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<RunOutcome, String>;
}

/// 默认实现：原样转发本模块的自由函数 `run`（spawn 真实 claude/codex 子进程）。
pub struct RealAgentRunner;

#[async_trait::async_trait]
impl AgentRunner for RealAgentRunner {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        backend: Backend,
        prompt: &str,
        session_id: &str,
        resume: bool,
        chat_id: &str,
        bot_key: &str,
        role: crate::config::SenderRole,
        sessions: Option<&crate::sessions::SessionStore>,
        progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<RunOutcome, String> {
        // 裸函数调用解析到本模块的自由函数 run（trait method 必须经 `.` 调用，不会递归）。
        run(
            backend, prompt, session_id, resume, chat_id, bot_key, role, sessions, progress, cancel,
        )
        .await
    }
}

/// 单次尝试（run_once）的错误：区分「用户打断」与「真实失败（用户可读文案）」。
enum AttemptErr {
    Cancelled,
    Failed(String),
}

/// 供应商解析产物：要注入子进程的 env，和仅 codex/pi 用的额外 CLI 参数。
struct Injection {
    env: Option<HashMap<String, String>>,
    /// codex：`-c model_provider=... -c model_providers.agent_bridge.*=...`；
    /// pi：`--provider <名> --model <模型>`。claude 永远为空。
    extra_args: Vec<String>,
}

/// codex 注入 api key 用的 env 变量名（经 `env_key` 引用，key 绝不进 argv / config.toml）。
const CODEX_KEY_ENV: &str = "AGENT_BRIDGE_MODEL_KEY";

/// 把字符串包成 TOML 基本串（双引号 + 转义 `\` `"` 控制符），供 codex `-c key=<toml>`。
/// codex 把 value 当 TOML 解析：不包引号的裸串只在是纯标量时才碰巧可用，含 `://`/`/` 必炸。
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 读 CC Switch 当前 claude provider 的 ANTHROPIC_* env（桥内未配供应商时的回落）。
/// 拿不到 → 用户可见错误文案（沿用原硬失败语义，不静默跑一个没凭证的 claude）。
fn ccswitch_env_or_err() -> Result<HashMap<String, String>, String> {
    match crate::ccswitch::active_env() {
        Some(env)
            if env.contains_key("ANTHROPIC_BASE_URL") && env.contains_key("ANTHROPIC_AUTH_TOKEN") =>
        {
            Ok(env)
        }
        _ => Err(format!(
            "⚠️ 未配置模型供应商，且读不到宿主 CC Switch 当前 claude provider 配置（{} 不可读或无激活项）。请在「模型供应商配置」加一个 anthropic 供应商，或在 CC Switch 选好 provider。",
            dirs::home_dir().unwrap_or_default().join(".cc-switch/cc-switch.db").display()
        )),
    }
}

/// 由（后端, 供应商）算出注入产物。优先级：桥内供应商 > CC Switch / codex 自认证。
/// 类型与后端不匹配 → Err（用户可见）。供应商为 None → 旧行为回落。
fn build_injection(
    backend: Backend,
    provider: Option<&crate::config::ProviderConfig>,
) -> Result<Injection, String> {
    use crate::config::ProviderConfig as P;
    let no_args = Vec::new();
    match (backend, provider) {
        // ── 未配供应商：旧行为回落 ──
        (Backend::Claude, None) => Ok(Injection {
            env: Some(ccswitch_env_or_err()?),
            extra_args: no_args,
        }),
        (Backend::Codex, None) => Ok(Injection {
            env: None, // codex 走自己 ~/.codex 的登录态，不注入
            extra_args: no_args,
        }),
        (Backend::Pi, None) => Ok(Injection {
            env: None, // pi 走自己 ~/.pi 的登录态/默认模型，不注入
            extra_args: no_args,
        }),

        // ── anthropic 供应商 ──
        (Backend::Claude, Some(p)) if p.kind == "anthropic" => {
            let mut env = HashMap::new();
            if !p.base_url.is_empty() {
                env.insert("ANTHROPIC_BASE_URL".into(), p.base_url.clone());
            }
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), p.api_key.clone());
            if !p.model.is_empty() {
                env.insert("ANTHROPIC_MODEL".into(), p.model.clone());
            }
            Ok(Injection {
                env: Some(env),
                extra_args: no_args,
            })
        }
        // ── pi + anthropic 供应商：注入 ANTHROPIC_API_KEY + --provider/--model。
        // pi 内置 anthropic provider 固定官方端点，不读 ANTHROPIC_BASE_URL——配了 base_url
        // 也照常打官方端点（日志警告，避免用户误以为走了自定义网关）；自定义端点需在
        // ~/.pi/agent/models.json 配 custom provider（超出桥职责，文档提示即可）。
        (Backend::Pi, Some(p)) if p.kind == "anthropic" => {
            let mut env = HashMap::new();
            env.insert("ANTHROPIC_API_KEY".into(), p.api_key.clone());
            let mut args = vec!["--provider".to_string(), "anthropic".to_string()];
            if !p.model.is_empty() {
                args.push("--model".to_string());
                args.push(p.model.clone());
            }
            if !p.base_url.is_empty() {
                crate::log!(
                    "[agent] pi 后端忽略 anthropic 供应商「{}」的 base_url（pi 内置 provider 固定官方端点；自定义端点请在 ~/.pi/agent/models.json 配）",
                    p.name
                );
            }
            Ok(Injection {
                env: Some(env),
                extra_args: args,
            })
        }
        (Backend::Codex, Some(p)) if p.kind == "anthropic" => Err(format!(
            "⚠️ 供应商「{}」是 Anthropic 型，codex 后端需要 OpenAI 兼容供应商（openai-chat / openai-responses）。请给该 bot 换个供应商或改回 claude 后端。",
            p.name
        )),

        // ── OpenAI 兼容供应商（chat / responses）──
        (Backend::Codex, Some(p)) if p.kind == "openai-chat" || p.kind == "openai-responses" => {
            let wire = if p.kind == "openai-chat" {
                "chat"
            } else {
                "responses"
            };
            let mut args = vec![
                format!("model_provider={}", toml_str("agent_bridge")),
                format!("model_providers.agent_bridge.name={}", toml_str(&p.name)),
                format!(
                    "model_providers.agent_bridge.base_url={}",
                    toml_str(&p.base_url)
                ),
                format!("model_providers.agent_bridge.wire_api={}", toml_str(wire)),
                format!(
                    "model_providers.agent_bridge.env_key={}",
                    toml_str(CODEX_KEY_ENV)
                ),
            ];
            if !p.model.is_empty() {
                args.push(format!("model={}", toml_str(&p.model)));
            }
            let mut env = HashMap::new();
            env.insert(CODEX_KEY_ENV.into(), p.api_key.clone());
            Ok(Injection {
                env: Some(env),
                extra_args: args,
            })
        }
        // ── pi + OpenAI 兼容供应商：注入 OPENAI_API_KEY + --provider openai + --model。
        // pi 内置 openai provider 固定 api.openai.com；非官方端点同样需 models.json 自定义
        // provider（见上 anthropic 分支注释）。wire_api（chat/responses）由 pi 侧决定，桥不干预。
        (Backend::Pi, Some(p)) if p.kind == "openai-chat" || p.kind == "openai-responses" => {
            let mut env = HashMap::new();
            env.insert("OPENAI_API_KEY".into(), p.api_key.clone());
            let mut args = vec!["--provider".to_string(), "openai".to_string()];
            if !p.model.is_empty() {
                args.push("--model".to_string());
                args.push(p.model.clone());
            }
            if !p.base_url.is_empty() {
                crate::log!(
                    "[agent] pi 后端忽略 OpenAI 兼容供应商「{}」的 base_url（pi 内置 provider 固定官方端点；自定义端点请在 ~/.pi/agent/models.json 配）",
                    p.name
                );
            }
            Ok(Injection {
                env: Some(env),
                extra_args: args,
            })
        }
        (Backend::Claude, Some(p)) if p.kind == "openai-chat" || p.kind == "openai-responses" => {
            Err(format!(
                "⚠️ 供应商「{}」是 OpenAI 兼容型；claude 只支持 Anthropic 原生 API（或留空走 CC Switch）。请给该 bot 换个 anthropic 供应商。",
                p.name
            ))
        }

        // ── 未知 kind ──
        (_, Some(P { .. })) => Err(format!(
            "⚠️ 供应商「{}」类型无法识别（应为 anthropic / openai-chat / openai-responses）。",
            provider.map(|p| p.name.as_str()).unwrap_or("")
        )),
    }
}

/// 运行一个后端，流式读取输出：中途的完整消息经 `progress` 通道实时推出（不等进程结束），
/// 最终回复随 `RunOutcome::Reply` 返回。`cancel`（AtomicBool，bridge 置 true）触发后杀掉子进程。
/// chat_id 注入为 AGENT_BRIDGE_CHAT_ID env、bot_key 注入为 AGENT_BRIDGE_BOT_KEY env，
/// 让 claude 调 `agent-bridge job add` 时知道回发到哪个会话、写哪个 bot 的 jobs.json。
///
/// codex 上下文：codex 用自己的 thread_id（非桥生成的 UUID），故需要 `sessions` 在首轮抓到
/// 真实 thread_id 后回存（bridge 调 claude 时传 None，claude 直接用桥的 UUID 无需回存）。
#[allow(clippy::too_many_arguments)]
pub async fn run(
    backend: Backend,
    prompt: &str,
    session_id: &str,
    resume: bool,
    chat_id: &str,
    bot_key: &str,
    role: crate::config::SenderRole,
    sessions: Option<&crate::sessions::SessionStore>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<RunOutcome, String> {
    if prompt.is_empty() {
        return Err("（空消息，没收到内容）".into());
    }

    // 受限模式判定：授权者会话且该 bot 的「授权者 agent 隔离」开关未放宽。
    // 每次热读 config（授权/关开关即时生效，与访问控制一致）；读不到按安全默认 true。
    let restrict = role == crate::config::SenderRole::Granted
        && crate::config::Config::bot_for_bot_key(bot_key)
            .map(|b| b.restrict_granted_agent)
            .unwrap_or(true);
    // pi 无任何权限/沙箱系统（非交互无审批），受限会话无法降级——直接拒绝，
    // 绝不让授权者静默获得全权限 agent。owner 可换后端或关掉隔离开关恢复。
    if restrict && backend == Backend::Pi {
        return Err(
            "⚠️ 该 bot 的后端是 pi，不支持受限模式。授权者会话不可用：请 owner 在设置里给该 bot 换 claude/codex 后端，或关闭「授权者 agent 隔离」开关。"
                .into(),
        );
    }

    // 受限会话：生成/刷新 guard 文件（claude settings.json hook + codex execpolicy）。
    // 必须在 spawn 前完成——hook 配置未就位就启动 agent 等于裸奔，失败则拒绝启动
    //（返回用户可见错误，不静默降级成全权限）。
    if restrict {
        crate::guard::ensure_guard_files(bot_key)
            .map_err(|e| format!("⚠️ 受限会话 guard 文件生成失败，已拒绝启动：{e:#}"))?;
    }

    // 本 bot 的工作目录：~/.agent-bridge/workspaces/<bot_key>/（多 bot 相互隔离）
    let workspace = crate::workspace_dir(bot_key);
    let _ = std::fs::create_dir_all(&workspace);
    ensure_workspace_guide(&workspace);

    // 解析供应商 → env + codex -c 参数（桥内配置优先于 CC Switch / codex 自认证）。
    // 类型不匹配在此直接报错返回（用户可见文案），不进 run_once。
    let provider = crate::config::Config::provider_for_bot_key(bot_key);
    let inject = build_injection(backend, provider.as_ref())?;

    let mut sid = session_id.to_string();
    let mut is_resume = resume;
    for attempt in 0..2 {
        match run_once(
            backend,
            prompt,
            &sid,
            is_resume,
            chat_id,
            bot_key,
            &workspace,
            inject.env.as_ref(),
            &inject.extra_args,
            progress.clone(),
            cancel.clone(),
            role,
            restrict,
        )
        .await
        {
            Ok(out) => {
                // codex 首轮（或回退重建）抓到真实 thread_id → 回存，供后续轮 resume
                if backend == Backend::Codex {
                    if let (Some(tid), Some(store)) = (&out.thread_id, sessions) {
                        if tid != &sid {
                            store.set_session_id(chat_id, tid);
                        }
                    }
                }
                // pi 不用回存：--session-id 直接用桥的 UUID，首轮就固定（无需 thread_id）
                return Ok(RunOutcome::Reply {
                    reply: out.reply,
                    session_id: sid,
                });
            }
            Err(AttemptErr::Cancelled) => return Ok(RunOutcome::Cancelled),
            Err(AttemptErr::Failed(e)) => {
                // resume 失败（会话在对端已不存在）→ 回退全新会话重建一次，别让用户永久卡死。
                // codex：thread 没了（no rollout found）；claude：transcript 被删/机器迁移
                // （No conversation found）——两者都会让该聊天此后每轮必报错，无自愈路径。
                // pi 无此问题：--session-id 对应的会话文件被删/损坏时 pi 会新建或用报错兜底，
                // 不走该分支（首版不自动重建，用户可 `session reset` 换新 UUID）。
                let session_lost = is_resume
                    && ((backend == Backend::Codex && e.contains("no rollout found"))
                        || (backend == Backend::Claude && e.contains("No conversation found")));
                if session_lost && attempt == 0 {
                    crate::log!(
                        "[agent] {} resume 失败（会话已不存在），回退全新会话重建",
                        backend.name()
                    );
                    is_resume = false;
                    continue;
                }
                // #6/#7：claude 会话槽位被 jsonl 残留占用（already in use）或启动挂起被终止 →
                // 同 UUID 重试必然再失败，换新 UUID（started 复位 false）重建一次。
                if backend == Backend::Claude && attempt == 0 && claude_needs_fresh_session(&e) {
                    let new_sid = if let Some(store) = sessions {
                        store.reset_session(chat_id)
                    } else {
                        uuid::Uuid::new_v4().to_string()
                    };
                    crate::log!(
                        "[agent] claude 会话重建（already in use / 启动挂起），换新 UUID {}",
                        &new_sid[..new_sid.len().min(8)]
                    );
                    sid = new_sid;
                    is_resume = false;
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err("⚠️ codex 会话异常".into())
}

/// 单次后端执行产出。reply=最终回复；thread_id=codex 真实 thread（供 resume 回存）。
struct AgentOutput {
    reply: String,
    thread_id: Option<String>,
}

/// 打断轮询间隔：无输出时也最多这么久就检查一次 cancel（卡死的进程也能被叫停）。
const CANCEL_POLL_MS: u64 = 250;

/// 从 pi 的 assistant message 里取纯文本（content[].type == "text" 块拼接）。
/// 工具调用轮（只有 toolCall 块）返回空串——不产生进度候选。
fn pi_message_text(v: &serde_json::Value) -> String {
    let mut txt = String::new();
    if let Some(content) = v.get("content").and_then(|c| c.as_array()) {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    if !txt.is_empty() {
                        txt.push('\n');
                    }
                    txt.push_str(t);
                }
            }
        }
    }
    txt.trim().to_string()
}

/// 处理一行流式 JSONL，更新解析状态；返回被「滞后一位」挤出的进度文本（若有）。
/// 滞后一位（one-behind）：新候选回复到来时，把上一条候选作为进度推出，自己留下当最终回复——
/// 这样无需预知哪条是最后一条，也不会把最终回复重复发两遍。codex/claude/pi 通用。
/// pi 事件（--mode json）：message_end 是每条 assistant 消息的权威文本（含 stopReason/errorMessage），
/// 用它做候选；中间 tool 轮无文本则跳过。LLM 错误（stopReason=error/aborted）记进 pi_error，
/// 由 run_once 在 exit 0 时仍能判错（pi json 模式下进程对 LLM 错误 exit 0，见 pi print-mode.js）。
fn process_line(
    backend: Backend,
    line: &str,
    thread_id: &mut Option<String>,
    pending: &mut Option<String>,
    claude_result: &mut Option<(bool, String)>,
    pi_error: &mut Option<String>,
) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    match backend {
        Backend::Codex => match v.get("type").and_then(|t| t.as_str()) {
            Some("thread.started") => {
                *thread_id = v
                    .get("thread_id")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                None
            }
            Some("item.completed") => {
                let item = &v["item"];
                if item.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                    let t = item
                        .get("text")
                        .and_then(|t| t.as_str())?
                        .trim()
                        .to_string();
                    if !t.is_empty() {
                        return pending.replace(t); // 挤出上一条作进度
                    }
                }
                None
            }
            _ => None,
        },
        Backend::Claude => match v.get("type").and_then(|t| t.as_str()) {
            // assistant 轮的 text 块（thinking/tool_use 块忽略）。最终回复以 result 事件为准，
            // 这里的 text 只作中间进度候选（最后一条会被 result 取代，不会重复发）。
            Some("assistant") => {
                let mut txt = String::new();
                if let Some(content) = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                if !txt.is_empty() {
                                    txt.push('\n');
                                }
                                txt.push_str(t);
                            }
                        }
                    }
                }
                let txt = txt.trim().to_string();
                if txt.is_empty() {
                    None
                } else {
                    pending.replace(txt)
                }
            }
            // 结束事件：result 字段是权威最终回复，is_error 标记失败
            Some("result") => {
                let is_err = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
                let res = v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                *claude_result = Some((is_err, res));
                None
            }
            _ => None, // system/init、thinking_tokens、user(tool_result) 等忽略
        },
        Backend::Pi => match v.get("type").and_then(|t| t.as_str()) {
            // 每条 assistant 消息结束：message 字段是权威文本；stopReason=error/aborted → 记错。
            Some("message_end") => {
                let msg = &v["message"];
                if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                    return None;
                }
                let stop = msg.get("stopReason").and_then(|s| s.as_str()).unwrap_or("");
                if stop == "error" || stop == "aborted" {
                    *pi_error = Some(
                        msg.get("errorMessage")
                            .and_then(|e| e.as_str())
                            .unwrap_or("（pi 未给出错误详情）")
                            .to_string(),
                    );
                    return None;
                }
                let t = pi_message_text(msg);
                if t.is_empty() {
                    None
                } else {
                    pending.replace(t) // 挤出上一条作进度
                }
            }
            _ => None, // session 头、message_start/update、tool_execution_*、agent_end 等忽略
        },
    }
}

/// stderr 尾巴（报错时附在「没有输出」提示后）。
fn stderr_tail(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!("stderr: {}", truncate(stderr, 400))
    }
}

/// claude 命令行构造（拆出便于单测参数组合与 env 注入）。
/// 构造子进程命令：Windows 下 npm 全局装的 claude/codex/pi 都是 `.cmd`/`.bat` shim，
/// CreateProcess 不直接执行脚本（报 "program not found"）→ 必须经 `cmd.exe /c` 包装；
/// 其它平台直接执行。program 传入 deps::find_in_path 解析出的真实路径（找不到才回落裸名）。
fn shim_command(program: &std::path::Path) -> std::process::Command {
    #[cfg(windows)]
    {
        let ext = program
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("cmd" | "bat")) {
            let mut c = std::process::Command::new("cmd");
            c.arg("/c").arg(program);
            return c;
        }
    }
    std::process::Command::new(program)
}

fn claude_command(
    program: &std::path::Path,
    resume: bool,
    session_id: &str,
    restricted: bool,
    settings_path: &std::path::Path,
) -> std::process::Command {
    let mut c = shim_command(program);
    c.arg("-p");
    if restricted {
        // 受限模式（授权者会话）：去掉全权限旗标，改走「默认拒绝 + 白名单」。
        // dontAsk = 未预批准的工具调用直接拒绝（非交互下不挂起等输入）；
        // --allowedTools 只放行工作区相对路径的读/写/查工具（Read(./**) 等）。
        // Bash 不在 CLI 层放行：受限会话的 $ABB_BIN 命令由 guard hook 校验放行
        //（--settings 指向工作区外的 settings.json，hook 是强制闸）；
        // WebFetch/MCP 等其余工具全被 dontAsk 拒绝。
        c.arg("--permission-mode").arg("dontAsk");
        c.arg("--settings").arg(settings_path);
        c.arg("--allowedTools")
            .arg("Read(./**)")
            .arg("Glob")
            .arg("Grep")
            .arg("Edit(./**)")
            .arg("Write(./**)");
    } else {
        c.arg("--dangerously-skip-permissions");
    }
    c.arg("--verbose").arg("--output-format").arg("stream-json");
    c.arg(if resume { "--resume" } else { "--session-id" })
        .arg(session_id);
    // #7（2026-08-08 实测）：claude 2.x 启动会连 api.anthropic.com / datadoghq.com 遥测，
    // 无超时，走代理节点抖动即永久挂起（卡在启动早期，jsonl 都不创建）。注入该 env 关闭
    // 非必要流量（API 请求不受影响）；配合 run_once 的 60s 启动健康检查兜底。
    c.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    c
}

/// codex 会话命令构造（exec / exec resume，含桥内供应商 -c 注入）。
/// restricted=true（授权者受限会话）：--sandbox read-only（OS 级 seatbelt，可读全盘
/// 但不可写任何文件）。审批策略不传额外参数：codex exec 非交互（stdin 管道 + EOF）
/// 时需审批的操作被当作「用户拒绝」自动取消（openai/codex #24135 实测结论）。
/// 已知局限（尽力隔离，2026-08-14 实测）：① read-only 沙箱可读全盘——敏感读只能靠
/// 网络拦截兜底，无法 100% 防「读进回复」；② 网络拦截本机实测有效（curl DNS 失败），
/// 但 macOS 上 codex 网络隔离历史上不可靠，需按环境复测；③ execpolicy 在 codex
/// 0.147 上机制不明（文档与实测不符、写入 config.toml 会破坏登录态）→ 不生成；
/// ④ read-only 下 $ABB_BIN 写 jobs.json（定时任务）与 outbox（投递）不可用。
fn codex_command(
    program: &std::path::Path,
    resume: bool,
    session_id: &str,
    extra_args: &[String],
    restricted: bool,
) -> tokio::process::Command {
    let mut c = tokio::process::Command::from(shim_command(program));
    c.arg("exec");
    if resume {
        c.arg("resume").arg(session_id);
    }
    c.arg("--json").arg("--skip-git-repo-check");
    if restricted {
        c.arg("--sandbox").arg("read-only");
    } else {
        c.arg("--dangerously-bypass-approvals-and-sandbox");
    }
    // 桥内 OpenAI 兼容供应商 → -c 覆盖 model_provider/base_url/wire_api/env_key。
    // 追加在固定参数后（flags-after-subcommand 对 exec / exec resume 都成立，实测）。
    for a in extra_args {
        c.arg("-c").arg(a);
    }
    c
}

/// #6/#7：claude 旧会话槽位不可用，需要换新 UUID 全新会话重建一次。
/// - "already in use"：jsonl 残留——claude 对 `--session-id` 判定「占用」是看 jsonl 文件
///   是否存在（源码 statSync(session.jsonl)），不是真进程占用；同 UUID 重试必然再失败。
/// - "启动挂起"：run_once 启动健康检查终止的标记（#7）；杀进程后旧 UUID 可能已留 jsonl，
///   重建用新 UUID 更稳（与 #6 同一套换新逻辑）。
fn claude_needs_fresh_session(e: &str) -> bool {
    e.contains("already in use") || e.contains("启动挂起")
}

/// #7 启动健康检查窗口（秒）：claude 启动后该时间内无任何 stdout 产出即判定挂死并终止。
/// 只覆盖启动阶段；一旦有产出（含长任务）不再有任何超时——保持「无执行超时」语义。
const CLAUDE_STARTUP_GRACE_SECS: u64 = 60;

/// agent 可执行文件缺失/启动失败的用户可见文案（#8 M0：附安装指引，引导去设置窗装依赖）。
fn agent_missing_msg(backend: Backend, err: &std::io::Error) -> String {
    format!(
        "⚠️ 找不到命令或启动失败（{}）: {err}（如未安装：请打开 ABB 设置 → 环境配置 → 依赖，点「安装」）",
        backend.name()
    )
}

/// 单轮执行一个后端：流式读输出（不等 EOF），中途消息经 progress 推出，支持 cancel 打断。
#[allow(clippy::too_many_arguments)]
async fn run_once(
    backend: Backend,
    prompt: &str,
    session_id: &str,
    resume: bool,
    chat_id: &str,
    bot_key: &str,
    workspace: &std::path::Path,
    inject_env: Option<&HashMap<String, String>>,
    extra_args: &[String],
    progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    role: crate::config::SenderRole,
    restrict: bool,
) -> Result<AgentOutput, AttemptErr> {
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncBufReadExt;

    // 解析可执行文件真实路径（Windows：npm shim 是 .cmd，CreateProcess 不执行脚本；
    // mac：npm 全局 bin 是软链，find_in_path 跟随）。找不到才回落裸名（报错文案带安装指引）。
    let program = match backend {
        Backend::Pi => "pi",
        Backend::Codex => "codex",
        Backend::Claude => "claude",
    };
    let resolved =
        crate::deps::find_in_path(program).unwrap_or_else(|| std::path::PathBuf::from(program));

    let mut cmd = match backend {
        Backend::Codex => {
            // codex 多轮上下文（对齐 claude）：首轮 `codex exec`，后续轮 `codex exec resume <tid>`。
            // 关键坑（实测）：① 必须 `exec resume`（顶层 `codex resume` 是 TUI，stdin 非终端报错）；
            // ② codex 用自己的 thread_id（`thread.started` 事件），不是桥生成的 UUID —— 故加 --json
            //    从输出抓真实 tid 回存；③ resume 一个没建过的 tid 报 "no rollout found" → 上层回退 exec。
            codex_command(&resolved, resume, session_id, extra_args, restrict)
        }
        Backend::Claude => {
            // stream-json：逐事件流式输出（assistant/result…），配合逐行解析可实时推进度。
            // 注意：--output-format=stream-json 强制要求 --verbose（实测报错确认）。
            // 构造拆到 claude_command()（可单测）：含关遥测 env（#7）与受限模式分支
            //（受限时 --settings 指向工作区外的 guard settings.json）。
            let settings = crate::guard::guard_settings_path(bot_key);
            tokio::process::Command::from(claude_command(
                &resolved, resume, session_id, restrict, &settings,
            ))
        }
        Backend::Pi => {
            // pi 非交互 print 模式：`pi -p --mode json --session-id <uuid>`。
            // 关键点（对照 pi 源码）：
            // ① prompt 走 stdin——pi 非交互时读管道 stdin 作为首条消息（main.js readPipedStdin），
            //    与 claude/codex 同款，规避多行/-开头被 argparse 误判（pi 对 `-` 开头参数报 Unknown option）；
            // ② --session-id 已存在即续聊（SessionManager.open）、不存在即新建（create with id），
            //    首轮/后续轮同一参数，无需 --resume 分支；
            // ③ --session-dir 固定到本 bot 工作区：会话文件随 workspace 隔离，`session reset` 可整个清掉；
            // ④ --mode json 逐事件 JSONL 输出（message_end 是权威回复）；LLM 错误在 json 模式下
            //    进程仍 exit 0，错误信息在 message_end.stopReason/errorMessage——由 process_line 判错。
            let mut c = tokio::process::Command::from(shim_command(&resolved));
            c.arg("-p")
                .arg("--mode")
                .arg("json")
                .arg("--session-id")
                .arg(session_id)
                .arg("--session-dir")
                .arg(workspace.join(".pi-sessions"));
            // 桥内供应商 → --provider/--model（api key 走 env，见 build_injection）
            for a in extra_args {
                c.arg(a);
            }
            c
        }
    };

    // Windows：GUI 子系统进程 spawn 控制台子进程（claude/codex）会弹新控制台窗口，
    // CREATE_NO_WINDOW 让子进程无窗口运行（stdout/stderr 仍走管道，功能不受影响）。
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.as_std_mut().creation_flags(0x0800_0000);
    }

    // claude 和 codex 统一收进 ~/.agent-bridge/workspace（用户拍板 2026-08-05）：
    // 在飞书里不区分后端，哪个 agent 工作目录都该受控，而不是 codex 仍在 home。
    cmd.current_dir(workspace)
        .env("PATH", crate::deps::composed_path())
        .env("LANG", "en_US.UTF-8")
        .env("AGENT_BRIDGE_CHAT_ID", chat_id)
        .env("AGENT_BRIDGE_BOT_KEY", bot_key)
        .env("AGENT_BRIDGE_SENDER_ROLE", role.as_str())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // 注入本程序绝对路径：agent 调 job CLI 用 $ABB_BIN，保证是当前安装/当前版本，
    // 不依赖 PATH（macOS 在 .app 内、Windows 在安装目录，裸命令名都调不到）。
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("ABB_BIN", exe);
    }

    // 供应商 / CC Switch env（含 claude 的 ANTHROPIC_*、codex 的 AGENT_BRIDGE_MODEL_KEY）。
    // 永不进日志（env 不由桥打印；argv 里也没有 key）。
    if let Some(env) = inject_env {
        cmd.envs(env);
    }

    crate::log!(
        "[agent] 调用 {} session={} resume={} prompt={:?}",
        backend.name(),
        &session_id[..session_id.len().min(8)],
        resume,
        truncate(prompt, 60)
    );

    let mut child = cmd
        .spawn()
        .map_err(|e| AttemptErr::Failed(agent_missing_msg(backend, &e)))?;
    // 登记子进程 pid：重启恢复时清理孤儿用（guard 在 run_once 返回时 Drop → 自动移除，
    // 覆盖 cancel/超时/错误所有返回路径）。spawn 成功但拿不到 pid 的极端情况跳过登记。
    let _pid_guard = child.id().map(|pid| {
        track_agent_pid(bot_key, pid);
        AgentPidGuard {
            bot_key: bot_key.to_string(),
            pid,
        }
    });

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        // drop stdin → 关管道，agent 读到 EOF
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AttemptErr::Failed(format!("⚠️ {} 无法读取输出管道", backend.name())))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AttemptErr::Failed(format!("⚠️ {} 无法读取错误管道", backend.name())))?;

    // stderr 后台并行收（进程退出后用于报错归因；session_lost 判定也靠它）
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        let mut r = tokio::io::BufReader::new(stderr);
        let _ = r.read_to_string(&mut buf).await;
        buf
    });

    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut thread_id: Option<String> = None;
    let mut pending: Option<String> = None; // 滞后一位缓冲：最新候选回复
    let mut claude_result: Option<(bool, String)> = None; // (is_error, result)
    let mut pi_error: Option<String> = None; // pi：message_end.stopReason=error/aborted 的错误文案

    // #7 启动健康检查（仅 claude）：关遥测后仍有启动早期网络风险，给 60s 启动窗口，
    // 窗口内无任何 stdout 产出 → 判定启动挂死并终止（长任务也会有先行的流式事件，不受影响）。
    let mut got_output = false;
    let startup_deadline = if backend == Backend::Claude {
        Some(
            tokio::time::Instant::now() + std::time::Duration::from_secs(CLAUDE_STARTUP_GRACE_SECS),
        )
    } else {
        None
    };

    // 流式读取：每行即时解析；无输出时也每 CANCEL_POLL_MS 检查一次打断
    loop {
        if cancel
            .as_ref()
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
            crate::log!("[agent] {} 被用户打断（kill）", backend.name());
            return Err(AttemptErr::Cancelled);
        }
        match tokio::time::timeout(
            std::time::Duration::from_millis(CANCEL_POLL_MS),
            lines.next_line(),
        )
        .await
        {
            Err(_) => {
                // 超时无输出 → 先查启动健康检查，再回去查 cancel
                if let Some(deadline) = startup_deadline {
                    if !got_output && tokio::time::Instant::now() >= deadline {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        crate::log!(
                            "[agent] claude 启动 {}s 无输出（疑似启动遥测/网络阻塞），终止并自动重建",
                            CLAUDE_STARTUP_GRACE_SECS
                        );
                        return Err(AttemptErr::Failed(format!(
                            "⚠️ claude 启动挂起（{}s 无输出，疑似启动网络阻塞）。已自动终止并重建，请检查代理节点。",
                            CLAUDE_STARTUP_GRACE_SECS
                        )));
                    }
                }
                continue;
            }
            Ok(Ok(Some(l))) => {
                got_output = true;
                if let Some(p) = process_line(
                    backend,
                    &l,
                    &mut thread_id,
                    &mut pending,
                    &mut claude_result,
                    &mut pi_error,
                ) {
                    if let Some(tx) = &progress {
                        let _ = tx.send(p);
                    }
                }
            }
            Ok(Ok(None)) => break, // EOF
            Ok(Err(e)) => {
                crate::log!("[agent] 读取 {} 输出中断: {e}", backend.name());
                break;
            }
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| AttemptErr::Failed(format!("⚠️ {} 调用异常: {e}", backend.name())))?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    let stderr_text = stderr_text.trim().to_string();

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let detail = if !stderr_text.is_empty() {
            stderr_text.clone()
        } else {
            pending.clone().unwrap_or_default()
        };
        return Err(AttemptErr::Failed(format!(
            "⚠️ {} 出错（exit {code}）:\n{}",
            backend.name(),
            truncate(&detail, 800)
        )));
    }

    let reply = match backend {
        Backend::Codex => pending.take().filter(|s| !s.is_empty()).ok_or_else(|| {
            AttemptErr::Failed(format!("⚠️ codex 没有输出。{}", stderr_tail(&stderr_text)))
        })?,
        Backend::Claude => {
            if let Some((is_err, res)) = claude_result.take() {
                if is_err {
                    return Err(AttemptErr::Failed(format!(
                        "⚠️ claude 出错:\n{}",
                        truncate(&res, 800)
                    )));
                }
                if !res.trim().is_empty() {
                    res
                } else {
                    pending.take().filter(|s| !s.is_empty()).ok_or_else(|| {
                        AttemptErr::Failed(format!(
                            "⚠️ claude 没有输出。{}",
                            stderr_tail(&stderr_text)
                        ))
                    })?
                }
            } else {
                pending.take().filter(|s| !s.is_empty()).ok_or_else(|| {
                    AttemptErr::Failed(format!("⚠️ claude 没有输出。{}", stderr_tail(&stderr_text)))
                })?
            }
        }
        Backend::Pi => {
            // pi json 模式对 LLM 错误也 exit 0：错误信息只能从事件流（message_end）判，
            // 进程非零退出则走上面 status 分支；这里先查事件级错误，再取最后一条回复。
            if let Some(err) = pi_error.take() {
                return Err(AttemptErr::Failed(format!(
                    "⚠️ pi 出错:\n{}",
                    truncate(&err, 800)
                )));
            }
            pending.take().filter(|s| !s.is_empty()).ok_or_else(|| {
                AttemptErr::Failed(format!("⚠️ pi 没有输出。{}", stderr_tail(&stderr_text)))
            })?
        }
    };

    Ok(AgentOutput { reply, thread_id })
}

// ─────────────────────── agent 子进程 pid 跟踪（重启恢复配套）───────────────────────
// service 崩溃/退出时，spawn 的 claude/codex 子进程可能残留为孤儿；下次启动恢复
// pending 消息前先清掉，否则 resume 撞「already in use」/旧进程继续占用会话。
// pid 落盘到 workspaces/<bot>/agent-pids.json（数组），任务结束（run_once 返回）移除。
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

/// spawn 成功后登记子进程 pid（任务结束时由 AgentPidGuard drop 移除）。
fn track_agent_pid(bot_key: &str, pid: u32) {
    let _g = AGENT_PID_LOCK.lock().unwrap();
    let mut pids = read_agent_pids(bot_key);
    if !pids.contains(&pid) {
        pids.push(pid);
        write_agent_pids(bot_key, &pids);
    }
}

fn untrack_agent_pid(bot_key: &str, pid: u32) {
    let _g = AGENT_PID_LOCK.lock().unwrap();
    let mut pids = read_agent_pids(bot_key);
    let before = pids.len();
    pids.retain(|p| *p != pid);
    if pids.len() != before {
        write_agent_pids(bot_key, &pids);
    }
}

/// run_once 返回（含 cancel/超时/错误路径）时自动 untrack，避免遗漏。
struct AgentPidGuard {
    bot_key: String,
    pid: u32,
}
impl Drop for AgentPidGuard {
    fn drop(&mut self) {
        untrack_agent_pid(&self.bot_key, self.pid);
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
pub fn kill_stale_agents(bot_key: &str) {
    let pids = {
        let _g = AGENT_PID_LOCK.lock().unwrap();
        let pids = read_agent_pids(bot_key);
        write_agent_pids(bot_key, &[]); // 先清空：即使 kill 失败也不留旧账
        pids
    };
    if pids.is_empty() {
        return;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // process_line 测试辅助的解析状态：
    // (pending 候选, thread_id, claude_result, pi_error)
    type LineState = (
        Option<String>,
        Option<String>,
        Option<(bool, String)>,
        Option<String>,
    );

    fn run_lines(backend: Backend, lines: &[&str]) -> LineState {
        let mut tid = None;
        let mut pending = None;
        let mut res = None;
        let mut pi_err = None;
        for l in lines {
            process_line(backend, l, &mut tid, &mut pending, &mut res, &mut pi_err);
        }
        (pending, tid, res, pi_err)
    }

    #[test]
    fn codex_one_behind_forwards_intermediates() {
        // 三条 agent_message：前两条应被挤出为进度，最后一条留作回复
        let mut tid = None;
        let mut pending = None;
        let mut res = None;
        let mut forwarded = Vec::new();
        for l in [
            r#"{"type":"thread.started","thread_id":"t-123"}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"第一步"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"第二步"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"最终答案"}}"#,
        ] {
            if let Some(p) = process_line(
                Backend::Codex,
                l,
                &mut tid,
                &mut pending,
                &mut res,
                &mut None,
            ) {
                forwarded.push(p);
            }
        }
        assert_eq!(forwarded, vec!["第一步".to_string(), "第二步".to_string()]);
        assert_eq!(pending.as_deref(), Some("最终答案"));
        assert_eq!(tid.as_deref(), Some("t-123"));
    }

    #[test]
    fn codex_single_message_no_progress() {
        let (pending, tid, _, _) = run_lines(
            Backend::Codex,
            &[
                r#"{"type":"thread.started","thread_id":"t-1"}"#,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"唯一回复"}}"#,
            ],
        );
        assert_eq!(pending.as_deref(), Some("唯一回复"));
        assert_eq!(tid.as_deref(), Some("t-1"));
    }

    #[test]
    fn claude_result_wins_and_no_dup() {
        // 一条 assistant text + result：text 留在 pending，result 为权威回复 → 不重复
        let mut tid = None;
        let mut pending = None;
        let mut res = None;
        let mut forwarded = Vec::new();
        for l in [
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"x"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"OK"}]}}"#,
            r#"{"type":"result","is_error":false,"result":"OK"}"#,
        ] {
            if let Some(p) = process_line(
                Backend::Claude,
                l,
                &mut tid,
                &mut pending,
                &mut res,
                &mut None,
            ) {
                forwarded.push(p);
            }
        }
        assert!(forwarded.is_empty(), "单轮不应有进度: {forwarded:?}");
        assert_eq!(pending.as_deref(), Some("OK"));
        assert_eq!(res, Some((false, "OK".to_string())));
    }

    #[test]
    fn claude_multi_turn_forwards_intermediates() {
        let mut tid = None;
        let mut pending = None;
        let mut res = None;
        let mut forwarded = Vec::new();
        for l in [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"我先查一下"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"查到了，答案是42"}]}}"#,
            r#"{"type":"result","is_error":false,"result":"查到了，答案是42"}"#,
        ] {
            if let Some(p) = process_line(
                Backend::Claude,
                l,
                &mut tid,
                &mut pending,
                &mut res,
                &mut None,
            ) {
                forwarded.push(p);
            }
        }
        assert_eq!(forwarded, vec!["我先查一下".to_string()]);
        assert_eq!(pending.as_deref(), Some("查到了，答案是42"));
        assert_eq!(res, Some((false, "查到了，答案是42".to_string())));
    }

    #[test]
    fn claude_error_result_flagged() {
        let (_, _, res, _) = run_lines(
            Backend::Claude,
            &[r#"{"type":"result","is_error":true,"result":"boom"}"#],
        );
        assert_eq!(res, Some((true, "boom".to_string())));
    }

    // ── 供应商注入构造（build_injection / toml_str）──

    fn prov(name: &str, kind: &str) -> crate::config::ProviderConfig {
        crate::config::ProviderConfig {
            name: name.into(),
            kind: kind.into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-secret".into(),
            model: "some-model".into(),
        }
    }

    #[test]
    fn toml_str_escapes() {
        assert_eq!(toml_str("plain"), "\"plain\"");
        assert_eq!(toml_str("https://x/v1"), "\"https://x/v1\"");
        assert_eq!(toml_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn anthropic_to_claude_env() {
        let p = prov("myclaude", "anthropic");
        let inj = build_injection(Backend::Claude, Some(&p)).unwrap();
        let env = inj.env.unwrap();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://api.example.com/v1");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-secret");
        assert_eq!(env["ANTHROPIC_MODEL"], "some-model");
        assert!(inj.extra_args.is_empty(), "claude 不产生额外参数");
    }

    #[test]
    fn openai_chat_to_codex_args() {
        let p = prov("deepseek", "openai-chat");
        let inj = build_injection(Backend::Codex, Some(&p)).unwrap();
        let args = &inj.extra_args;
        assert!(args.iter().any(|a| a == "model_provider=\"agent_bridge\""));
        assert!(args
            .iter()
            .any(|a| a == "model_providers.agent_bridge.base_url=\"https://api.example.com/v1\""));
        assert!(args
            .iter()
            .any(|a| a == "model_providers.agent_bridge.wire_api=\"chat\""));
        assert!(args
            .iter()
            .any(|a| a == &format!("model_providers.agent_bridge.env_key=\"{CODEX_KEY_ENV}\"")));
        assert!(args.iter().any(|a| a == "model=\"some-model\""));
        // api key 走 env，绝不出现在任一 -c 参数里
        assert!(args.iter().all(|a| !a.contains("sk-secret")));
        assert_eq!(inj.env.as_ref().unwrap()[CODEX_KEY_ENV], "sk-secret");
    }

    #[test]
    fn openai_responses_wire_api() {
        let p = prov("local", "openai-responses");
        let inj = build_injection(Backend::Codex, Some(&p)).unwrap();
        assert!(inj
            .extra_args
            .iter()
            .any(|a| a == "model_providers.agent_bridge.wire_api=\"responses\""));
    }

    #[test]
    fn kind_backend_mismatch_errors() {
        // anthropic 供应商 + codex 后端 → 报错；+ pi 后端 → 可映射（env key + --provider/--model）
        let pa = prov("a", "anthropic");
        assert!(build_injection(Backend::Codex, Some(&pa)).is_err());
        assert!(build_injection(Backend::Pi, Some(&pa)).is_ok());
        // openai 供应商 + claude 后端 → 报错；+ pi 后端 → 可映射
        let po = prov("o", "openai-chat");
        assert!(build_injection(Backend::Codex, Some(&po)).is_ok());
        assert!(build_injection(Backend::Claude, Some(&po)).is_err());
        assert!(build_injection(Backend::Pi, Some(&po)).is_ok());
        // 未知 kind → 报错（三个后端一致）
        let px = prov("x", "gemini");
        assert!(build_injection(Backend::Claude, Some(&px)).is_err());
        assert!(build_injection(Backend::Pi, Some(&px)).is_err());
    }

    #[test]
    fn no_provider_codex_no_injection() {
        // codex 未配供应商 → 不注入任何 env/参数（走自己 ~/.codex 登录态）
        let inj = build_injection(Backend::Codex, None).unwrap();
        assert!(inj.env.is_none());
        assert!(inj.extra_args.is_empty());
    }

    #[test]
    fn no_provider_pi_no_injection() {
        // pi 未配供应商 → 不注入任何 env/参数（走自己 ~/.pi 登录态/默认模型）
        let inj = build_injection(Backend::Pi, None).unwrap();
        assert!(inj.env.is_none());
        assert!(inj.extra_args.is_empty());
    }

    #[test]
    fn anthropic_to_pi_env_and_args() {
        let p = prov("mypi", "anthropic");
        let inj = build_injection(Backend::Pi, Some(&p)).unwrap();
        // api key 走 env（不进 argv），模型/provider 走参数
        assert_eq!(inj.env.as_ref().unwrap()["ANTHROPIC_API_KEY"], "sk-secret");
        assert!(!inj.env.as_ref().unwrap().contains_key("ANTHROPIC_BASE_URL"));
        let args = &inj.extra_args;
        assert!(args.iter().any(|a| a == "--provider"));
        assert!(args.iter().any(|a| a == "anthropic"));
        assert!(args.iter().any(|a| a == "--model"));
        assert!(args.iter().any(|a| a == "some-model"));
        assert!(
            args.iter().all(|a| !a.contains("sk-secret")),
            "key 绝不进 argv"
        );
    }

    #[test]
    fn openai_to_pi_env_and_args() {
        let p = prov("deepseek", "openai-chat");
        let inj = build_injection(Backend::Pi, Some(&p)).unwrap();
        assert_eq!(inj.env.as_ref().unwrap()["OPENAI_API_KEY"], "sk-secret");
        let args = &inj.extra_args;
        assert!(args.iter().any(|a| a == "--provider"));
        assert!(args.iter().any(|a| a == "openai"));
        assert!(args.iter().any(|a| a == "some-model"));
        assert!(
            args.iter().all(|a| !a.contains("sk-secret")),
            "key 绝不进 argv"
        );
    }

    #[test]
    fn pi_backend_parse() {
        assert_eq!(Backend::parse("pi"), Backend::Pi);
        assert_eq!(Backend::parse("PI"), Backend::Pi);
        assert_eq!(Backend::parse("codex"), Backend::Codex);
        assert_eq!(Backend::parse("claude"), Backend::Claude);
        assert_eq!(Backend::parse(""), Backend::Claude);
        assert_eq!(Backend::parse("weird"), Backend::Claude);
        assert_eq!(Backend::Pi.name(), "pi");
    }

    // ── pi JSON 事件流解析（--mode json）──

    #[test]
    fn pi_message_end_one_behind() {
        // 两条 assistant 消息（中间夹 tool 轮）：前一条被挤出为进度，后一条留作回复
        let mut tid = None;
        let mut pending = None;
        let mut res = None;
        let mut pi_err = None;
        let mut forwarded = Vec::new();
        for l in [
            r#"{"type":"session","version":3,"id":"u-1","cwd":"/tmp"}"#,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"我先查一下"}],"stopReason":"stop"}}"#,
            r#"{"type":"message_end","message":{"role":"toolResult","content":[{"type":"text","text":"ls 输出"}],"stopReason":"stop"}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"最终答案"}],"stopReason":"stop"}}"#,
        ] {
            if let Some(p) = process_line(
                Backend::Pi,
                l,
                &mut tid,
                &mut pending,
                &mut res,
                &mut pi_err,
            ) {
                forwarded.push(p);
            }
        }
        assert_eq!(forwarded, vec!["我先查一下".to_string()]);
        assert_eq!(pending.as_deref(), Some("最终答案"));
        assert!(pi_err.is_none(), "正常完成不应有错误");
    }

    #[test]
    fn pi_single_message_no_progress() {
        let (pending, _, _, pi_err) = run_lines(
            Backend::Pi,
            &[
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"唯一回复"}],"stopReason":"stop"}}"#,
            ],
        );
        assert_eq!(pending.as_deref(), Some("唯一回复"));
        assert!(pi_err.is_none());
    }

    #[test]
    fn pi_tool_only_turns_skip() {
        // 只有 toolCall 块（无文本）的 assistant 消息不产生候选
        let (pending, _, _, pi_err) = run_lines(
            Backend::Pi,
            &[
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{}}],"stopReason":"toolUse"}}"#,
            ],
        );
        assert!(pending.is_none(), "纯工具轮不应成为回复候选");
        assert!(pi_err.is_none());
    }

    #[test]
    fn pi_error_message_end_flagged() {
        let (_, _, _, pi_err) = run_lines(
            Backend::Pi,
            &[
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"partial"}],"stopReason":"error","errorMessage":"429 overloaded"}}"#,
            ],
        );
        assert_eq!(pi_err.as_deref(), Some("429 overloaded"));
    }

    #[test]
    fn pi_aborted_flagged() {
        let (_, _, _, pi_err) = run_lines(
            Backend::Pi,
            &[
                r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"aborted"}}"#,
            ],
        );
        assert_eq!(pi_err.as_deref(), Some("（pi 未给出错误详情）"));
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
    fn claude_command_injects_telemetry_disable() {
        let c = claude_command(
            std::path::Path::new("claude"),
            false,
            "sess-1",
            false,
            std::path::Path::new("/tmp/abb-settings.json"),
        );
        let has_disable = c.get_envs().any(|(k, v)| {
            k.to_str() == Some("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
                && v == Some(std::ffi::OsStr::new("1"))
        });
        assert!(
            has_disable,
            "应注入 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1"
        );
        // 非 resume → --session-id；resume → --resume
        assert!(c.get_args().any(|a| a == "--session-id"));
        assert!(c.get_args().any(|a| a == "sess-1"));
        assert!(
            c.get_args().any(|a| a == "--dangerously-skip-permissions"),
            "owner 会话保持全权限"
        );
        let r = claude_command(
            std::path::Path::new("claude"),
            true,
            "sess-2",
            false,
            std::path::Path::new("/tmp/abb-settings.json"),
        );
        assert!(r.get_args().any(|a| a == "--resume"));
        assert!(r.get_args().any(|a| a == "sess-2"));
        assert!(!r.get_args().any(|a| a == "--session-id"));
    }

    #[test]
    fn claude_command_restricted_drops_full_permissions() {
        // 受限模式（授权者会话）：去掉全权限旗标，改走 dontAsk + 工作区相对路径白名单
        let c = claude_command(
            std::path::Path::new("claude"),
            false,
            "sess-1",
            true,
            std::path::Path::new("/tmp/abb-settings.json"),
        );
        let args: Vec<&std::ffi::OsStr> = c.get_args().collect();
        assert!(
            !args.iter().any(|a| *a == "--dangerously-skip-permissions"),
            "受限模式绝不允许全权限旗标"
        );
        assert!(args.iter().any(|a| *a == "--permission-mode"));
        assert!(args.iter().any(|a| *a == "dontAsk"));
        for tool in ["Read(./**)", "Glob", "Grep", "Edit(./**)", "Write(./**)"] {
            assert!(
                args.iter().any(|a| a.to_str() == Some(tool)),
                "受限模式应放行工作区相对路径工具 {tool}"
            );
        }
        // 基础参数不受影响（会话 id / stream-json）
        assert!(args.iter().any(|a| *a == "--session-id"));
        assert!(args.iter().any(|a| *a == "sess-1"));
    }

    #[test]
    fn codex_command_restricted_uses_readonly_sandbox() {
        // 受限模式：--sandbox read-only（OS 级写禁；审批靠非交互 EOF 自动拒绝，
        // codex 0.147 实测无 --approval-policy flag），不带全权限旗标；
        // -c 供应商注入两分支都保留
        let extra = vec!["model_provider=abc".to_string()];
        let c = codex_command(std::path::Path::new("codex"), false, "tid-1", &extra, true);
        let args: Vec<&std::ffi::OsStr> = c.as_std().get_args().collect();
        assert!(
            !args
                .iter()
                .any(|a| *a == "--dangerously-bypass-approvals-and-sandbox"),
            "受限模式绝不允许全权限旗标"
        );
        assert!(args.iter().any(|a| *a == "--sandbox"));
        assert!(args.iter().any(|a| *a == "read-only"));
        assert!(
            !args.iter().any(|a| *a == "--approval-policy"),
            "codex 0.147 无该 flag（实测会报 unexpected argument）"
        );
        assert!(args.iter().any(|a| *a == "-c"));
        assert!(args.iter().any(|a| *a == "model_provider=abc"));
        // resume 形态
        let r = codex_command(std::path::Path::new("codex"), true, "tid-2", &[], true);
        assert!(r.as_std().get_args().any(|a| a == "resume"));
        assert!(r.as_std().get_args().any(|a| a == "tid-2"));
    }

    #[test]
    fn codex_command_full_keeps_bypass_flag() {
        // owner 会话：保持现状全权限旗标 + -c 注入
        let extra = vec!["model=abc".to_string()];
        let c = codex_command(std::path::Path::new("codex"), false, "tid-1", &extra, false);
        let args: Vec<&std::ffi::OsStr> = c.as_std().get_args().collect();
        assert!(args
            .iter()
            .any(|a| *a == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!args.iter().any(|a| *a == "--sandbox"));
        assert!(args.iter().any(|a| *a == "-c"));
        assert!(args.iter().any(|a| *a == "model=abc"));
    }

    #[cfg(windows)]
    #[test]
    fn shim_command_wraps_cmd_shims_on_windows() {
        // Windows：.cmd shim → cmd.exe /c 包装（CreateProcess 不执行脚本）
        let c = shim_command(std::path::Path::new(
            "C:\\Users\\x\\AppData\\Roaming\\npm\\pi.cmd",
        ));
        let prog = c.get_program().to_str().unwrap().to_string();
        assert_eq!(prog.to_ascii_lowercase(), "cmd", "应经 cmd.exe 执行");
        assert!(c
            .get_args()
            .any(|a| a.to_str().map(|s| s.to_ascii_lowercase()) == Some("/c".into())));
        // .exe 直接执行
        let e = shim_command(std::path::Path::new("C:\\tools\\pi.exe"));
        assert!(e.get_program().to_str().unwrap().ends_with("pi.exe"));
    }

    #[test]
    fn claude_needs_fresh_session_classifies() {
        // #6：jsonl 残留 already in use → 换新 UUID
        assert!(claude_needs_fresh_session(
            "⚠️ claude 出错（exit 1）:\nError: Session ID abc-123 is already in use."
        ));
        // #7：启动健康检查终止标记 → 换新 UUID
        assert!(claude_needs_fresh_session(
            "⚠️ claude 启动挂起（60s 无输出，疑似启动网络阻塞）"
        ));
        // resume 会话丢失 / codex 错误 → 不走换新 UUID 分支（各自既有自愈路径）
        assert!(!claude_needs_fresh_session(
            "⚠️ claude 出错（exit 1）:\nNo conversation found"
        ));
        assert!(!claude_needs_fresh_session(
            "⚠️ codex 出错（exit 1）:\nno rollout found"
        ));
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

    #[test]
    fn agent_missing_msg_includes_install_hint() {
        // #8 M0：agent 缺失时错误文案必须带安装指引，用户照着能去设置窗装依赖
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");
        let m = agent_missing_msg(Backend::Claude, &err);
        assert!(m.contains("找不到命令或启动失败（claude）"));
        assert!(m.contains("环境配置"));
        assert!(m.contains("安装"));
        let m2 = agent_missing_msg(Backend::Codex, &err);
        assert!(m2.contains("找不到命令或启动失败（codex）"));
        assert!(m2.contains("安装"));
        let m3 = agent_missing_msg(Backend::Pi, &err);
        assert!(m3.contains("找不到命令或启动失败（pi）"));
        assert!(m3.contains("安装"));
    }

    // ---- #25 重启恢复：agent 子进程 pid 跟踪 / 孤儿清理 ----

    fn pid_temp_key(tag: &str) -> String {
        format!("abb-agent-pid-{tag}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn track_untrack_pid_roundtrip() {
        let key = pid_temp_key("roundtrip");
        track_agent_pid(&key, 111);
        track_agent_pid(&key, 222);
        track_agent_pid(&key, 111); // 重复登记去重
        let pids = read_agent_pids(&key);
        assert_eq!(pids, vec![111, 222]);
        untrack_agent_pid(&key, 111);
        assert_eq!(read_agent_pids(&key), vec![222]);
        untrack_agent_pid(&key, 222);
        assert!(read_agent_pids(&key).is_empty());
        let _ = std::fs::remove_dir_all(crate::workspace_dir(&key));
    }

    #[test]
    fn kill_stale_agents_clears_file_and_skips_non_agent() {
        let key = pid_temp_key("stale");
        // 用不可能存在的 pid：ps 校验失败 → 不应误杀、文件清空、不 panic
        track_agent_pid(&key, 999_999);
        kill_stale_agents(&key);
        assert!(read_agent_pids(&key).is_empty(), "清理后 pid 文件应清空");
        // 空文件再次清理是 no-op
        kill_stale_agents(&key);
        let _ = std::fs::remove_dir_all(crate::workspace_dir(&key));
    }
}
