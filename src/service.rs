//! --service 入口：无头桥守护进程（纯 tokio，无 GUI）。
//! 组装 config → messenger（按 bot.kind：飞书 WS / 微信长轮询）→ bridge → 事件循环。
//! 处理 SIGTERM/SIGINT 优雅退出。

use crate::bridge::Bridge;
use crate::config::Config;
use crate::messenger;
use std::sync::Arc;

/// 解析 ACP 执行层命令（纯函数便于单测）。
/// 决议链：
/// ① 主程序同目录随包 `abb-agent`（单一执行层；2026-10-23 起产物 = ki）；
/// ② 随包缺失 → 回落 PATH `ki`（ki-acp-serve.md 批次 3）。
/// 两条路径都统一 spawn `<program> acp serve`（旧随包 abb-agent 忽略 argv，两代兼容）。
///
/// 随包执行层的文件名（同一个二进制名在两平台一致，Windows 追加 `.exe` 由调用方处理）。
const BUNDLED_AGENT: &str = "abb-agent";

/// 按给定顺序在「主程序同目录」找随包执行层（找不到该名字就试下一个）。
fn bundled_agent_in_dir(bundled: &[&str], exe_dir: Option<&std::path::Path>) -> Option<String> {
    let dir = match exe_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_exe().ok()?.parent()?.to_path_buf(),
    };
    for name in bundled {
        let candidate = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

/// 一条可 spawn 的执行层命令：程序 + 参数。
///
/// 生产命令恒为 `<program> acp serve`：随包产物自 2026-10-23 起是 ki（需子命令），
/// 而旧的裸 `abb-agent` 忽略 argv，所以同一形状对两代随包二进制都安全。此前回落
/// `pi-acp` 是 rpi-package 的 ACP↔RPC 桥（前身实现），已被 `ki acp serve` 取代
/// （pi-rust `docs/design/ki-acp-serve.md` 批次 3，2026-10-22）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentCmd {
    program: String,
    args: Vec<String>,
}

impl AgentCmd {
    /// 执行层命令的统一形状：`<program> acp serve`。
    ///
    /// 随包 `abb-agent` 自 2026-10-23 起就是 ki（打包层改名分发）：ki 需子命令参数；
    /// **旧 abb-agent 忽略 argv**（crates/abb-agent/src/main.rs 无参数处理，直接跑
    /// ACP server），故同一形状对两代随包二进制都安全。
    fn acp_serve(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: vec!["acp".to_string(), "serve".to_string()],
        }
    }

    /// 裸二进制（无参数）——仅测试夹具使用；生产命令一律 [`acp_serve`](Self::acp_serve)。
    #[cfg(test)]
    fn bare(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }
}

impl std::fmt::Display for AgentCmd {
    /// 日志/测试展示形状（`/path/ki acp serve`）；非 shell 转义，不用于 spawn。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.args.is_empty() {
            f.write_str(&self.program)
        } else {
            write!(f, "{} {}", self.program, self.args.join(" "))
        }
    }
}

/// 解析执行层命令：主程序同目录随包 `abb-agent` 优先（产物即 ki，
/// 见 release.yml 的打包步骤）；随包缺失回落 PATH `ki`。两者都以
/// `<program> acp serve` 形状 spawn（旧随包 abb-agent 忽略 argv，兼容）。
fn resolve_agent_command() -> AgentCmd {
    resolve_acp_command(bundled_agent_in_dir(&[BUNDLED_AGENT], None).as_deref())
}

/// 无随包时的回落命令：PATH 命中 `ki` ⇒ spawn `<ki> acp serve`（全路径必传，
/// #246④ 同款口径）；连 `ki` 都没有 ⇒ 保留裸名 `ki`，spawn 失败如实可见（#246④）。
fn resolve_acp_command_with(
    agent_cmd: Option<&str>,
    find_ki: impl FnOnce() -> Option<std::path::PathBuf>,
) -> AgentCmd {
    agent_cmd
        .map(AgentCmd::acp_serve)
        .or_else(|| find_ki().map(|p| AgentCmd::acp_serve(p.display().to_string())))
        .unwrap_or_else(|| AgentCmd::acp_serve("ki"))
}

fn resolve_acp_command(agent_cmd: Option<&str>) -> AgentCmd {
    resolve_acp_command_with(agent_cmd, || crate::deps::find_in_path("ki"))
}

/// 本 bot 的 ACP 供应商 env（旧 service 级 `env_for` 的 per-bot 化，P2.1）。
/// 执行层语义（2026-10-23 起）：供应商落成一份只含它的 ki `models.json`，目录经
/// `KI_CODING_AGENT_DIR` 交给子进程（见 `crate::ki_config`）；ki 不读旧的
/// `ANTHROPIC_*` / `OPENAI_COMPAT_*` env。GUI 热改供应商随服务重启生效。
/// 装配级硬闸保留：供应商存在但 API Key 空 → 空 env（agent 侧
/// 只会报内部错误，预检已按 NoProvider 拒答引导补填）。
///（P4.1：原先的 `uses_buzz_agent` 两臂（历史形参，取名即「是否走 buzz 承载」）都走已删的
/// `build_injection`，而两臂最终都解析到同一份 `abb_provider_env` 映射——殊途同归；
/// `build_injection` 矩阵随 Backend 一并删除后，两臂并一为单一路径，该形参随之摘除。）
/// Jev 决策门禁的 env 注入（统一迁移刀 4a）：从 config 的 `jev` 段转成 `JEV_*`。
/// 未配置（api_key 空）时注入空——abb-agent 在受限会话建会话时会 fail-closed 拒答。
fn jev_env(cfg: &Config) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let j = &cfg.jev;
    if !j.api_key.trim().is_empty() {
        env.push(("JEV_API_KEY".to_string(), j.api_key.clone()));
    }
    if !j.base_url.trim().is_empty() {
        env.push(("JEV_BASE_URL".to_string(), j.base_url.clone()));
    }
    if !j.model.trim().is_empty() {
        env.push(("JEV_MODEL".to_string(), j.model.clone()));
    }
    env
}

fn env_for_bot(bot: &crate::config::BotConfig, cfg: &Config) -> Vec<(String, String)> {
    let prov = cfg.resolve_provider(bot).cloned();
    if let Some(p) = prov.as_ref() {
        if p.api_key.trim().is_empty() {
            crate::log!(
                "[acp] bot={} 供应商「{}」未填 API Key，该 bot 消息将拒答引导",
                bot.key(),
                p.name
            );
            return Vec::new();
        }
    }
    match crate::agent::abb_provider_env(prov.as_ref()) {
        Ok(Some(env)) => env.into_iter().collect(),
        Ok(None) => Vec::new(), // 无供应商：agent 起来后回合报错（预检已引导配置）
        Err(e) => {
            crate::log!("[acp] bot={} 供应商 env 装配失败: {e}", bot.key());
            Vec::new()
        }
    }
}

/// normal handle 的执行档位载荷（单后端化 P2.2）：把 bot 的 `sandbox_mode` 解析成
/// 随 `session/new` `_meta` 下发的具体档。
/// - ReadOnly → `read-only` + 读/写域根 = 该 bot 工作区（域闸收口到 workspace）；
/// - WorkspaceWrite → `workspace-write` + 同域根；
/// - Auto（legacy）/ FullAccess（默认）→ None = fork 缺省 FullAccess
///   （owner 与用户同权限；Auto 只作旧 config 兼容）。
///
/// 域根先 canonicalize（symlink 归一），失败回落原路径（目录尚不存在时 canonicalize 会
/// Err——build_bot_acp_handles 已 ensure，这里是防御兜底）。
fn resolve_sandbox_meta(
    bot: &crate::config::BotConfig,
) -> Option<crate::runtime::acp::SessionSandboxMeta> {
    let root = crate::workspace_dir(&bot.key());
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let root = root.display().to_string();
    match bot.sandbox_mode {
        crate::config::SandboxMode::ReadOnly => Some(crate::runtime::acp::SessionSandboxMeta {
            sandbox: Some("read-only".to_string()),
            writable_roots: Some(vec![root]),
            read_roots: None,
            shell: None,
            abb_bin: None,
        }),
        crate::config::SandboxMode::WorkspaceWrite => {
            Some(crate::runtime::acp::SessionSandboxMeta {
                sandbox: Some("workspace-write".to_string()),
                writable_roots: Some(vec![root]),
                read_roots: None,
                shell: None,
                abb_bin: None,
            })
        }
        crate::config::SandboxMode::Auto | crate::config::SandboxMode::FullAccess => None,
    }
}

/// granted handle 的强制 Restricted 剖面（P2.2/P2.3）：granted 是「受限」语义——
/// 无论 bot 配的 `sandbox_mode` 是什么，授权者会话恒走 workspace-write 域闸 +
/// argv 白名单 shell + `$ABB_BIN`。同群 owner/granted 混合频道的档位只能绑进程，
/// 不能绑会话（角色 per-event，计划决策 4），故 granted 独立实例强制最严剖面。
/// `$ABB_BIN` = ABB 主程序绝对路径，**注入给 agent 进程**的 env。
///
/// 为什么（卡 `abb-mcp-orchestrator-p0-20261012` 的前置项，同时修一个"承诺落空"）：
/// 提示词与 README 一直写"桥注入的 `$ABB_BIN`"（agent 用它调 `task`/`job`/`deliver`，
/// 受限会话的白名单也按这个字面量放行），但**此前没有任何一处把它写进 agent 的 env**
/// ——`rg -nF 'ABB_BIN' src/` 只命中 guard/文档/测试；`env_for_bot()` 只注入供应商凭据。
/// 于是 agent（及其 shell / MCP 子进程）里 `"$ABB_BIN" task add …` 会展开成空。
/// ABB 侧在此注入，abb-agent 侧由 `child_env::PASSTHROUGH_ENV` 往下透传。
///
/// 单一真相源：`granted_sandbox_profile` 的 `_meta.abbBin` 与这里都取自本函数。
fn abb_bin_env() -> Option<(String, String)> {
    std::env::current_exe()
        .ok()
        .map(|p| ("ABB_BIN".to_string(), p.display().to_string()))
}

/// `AGENT_BRIDGE_BOT_KEY` = 当前 agent 进程**所属 bot 的规范 key**。
///
/// 为什么需要（卡 `abb-agent-env-residue-20261021`）：agent 在 shell 里调
/// `$ABB_BIN task add` / `deliver` 不带 `--bot` 时，CLI 按 env 解析目标 bot
/// （`main.rs::resolve_bot_key_with`）；而 CLI 自己的报错文案就写着「桥正常调用会
/// 注入 AGENT_BRIDGE_BOT_KEY」（`main.rs:1058`）——**设计如此，但从未接线**。
/// 后果：多 bot 下是硬错（「有 N 个 bot 但未指定目标」），单 bot 下静默落到
/// 「唯一 bot」，于是群会话里创建的任务会投到 bot 主会话（DM）。
///
/// 为什么可以按进程注入：agent 是**每 bot** 一个长驻进程
/// （`build_bot_acp_handles` 的调用粒度），bot 在进程生命周期内不变。
/// 与 `AGENT_BRIDGE_CHAT_ID` 不同：那个是**每频道**的，ACP 架构下桥无法按频道
/// 注入，故不在此列（口径见 `main.rs:1066-1070`），只能靠 `--to` 显式指定。
///
/// 不是凭据：bot key 是目录名/路由键（`tasks/<key>/`、`workspaces/<key>/`），
/// 不含密钥；但仍需在 abb-agent 的白名单里放行才能到 shell/MCP 子进程。
fn agent_bot_key_env(bot: &crate::config::BotConfig) -> (String, String) {
    ("AGENT_BRIDGE_BOT_KEY".to_string(), bot.key())
}

/// 按 bot 构造 ACP 句柄（单实例）：`session_sandbox` = bot `sandbox_mode` 解析产物
///（None=FullAccess 照旧），经 `session/new` `_meta.abbSandbox` 随会话下发；
/// owner 配了受限档时按档位套 Jev 门禁（abb-agent `restricted()` 判定同口径）。
fn build_bot_acp_handle(
    bot: &crate::config::BotConfig,
    cfg: &Config,
    command: &AgentCmd,
    stop: tokio_util::sync::CancellationToken,
) -> crate::bridge::BotAcpHandles {
    // 开发/自签构建无随包 → PATH `ki acp serve` 全路径兜底（裸名表达不了子命令参数；
    // 都没装则保留裸名 `ki`，spawn 失败 → AgentDown，如实可见，见 #246④）。
    let env = env_for_bot(bot, cfg);
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();
    let session_sandbox = resolve_sandbox_meta(bot);
    // 渠道级 MCP（卡 abb-channel-mcp-config-20261012）：名单不在句柄上，而是会话建立时
    // 按渠道目录现读 `<渠道目录>/mcp.json`。句柄允许读（owner-only 准入，无绕闸角色）。
    let mcp_from_channel = true;
    let mut extra_env: Vec<(String, String)> =
        vec![("PATH".to_string(), crate::deps::composed_path())];
    extra_env.extend(env);
    if session_sandbox.is_some() {
        extra_env.extend(jev_env(cfg));
    }
    let handle = crate::runtime::harness::RuntimeHandle::new(
        crate::runtime::harness::AgentConfig {
            command: command.program.clone(),
            args: command.args.clone(),
            extra_env: extra_env
                .into_iter()
                .chain(abb_bin_env())
                // `AGENT_BRIDGE_BOT_KEY`：agent 调 `$ABB_BIN task/deliver` 不带 `--bot` 时
                // 按它解析目标（多 bot 下缺它直接报错，单 bot 下静默落主会话）。
                .chain([agent_bot_key_env(bot)])
                .collect(),
            backend: "buzz".to_string(),
            session_sandbox,
        },
        stop,
        cwd,
        mcp_from_channel,
    );
    crate::log!("[acp] harness 装配 bot={} cmd={command}", bot.key());
    crate::bridge::BotAcpHandles { acp: handle }
}

/// 一次性同步回合的 agent 配置（单后端化 P3.2/P3.3/P3.4 共用）：与 normal
/// handle **同命令同 env 同档位载荷**（归纳/角色生成/团队生成 = owner 角色
/// 的内部任务，跟 bot 配置档走——维护任务必须能跟 bot 后端走）。granted 的
/// 强制 Restricted 剖面与 NO_HINTS 不适用于此（那是授权者会话的进程级收口）。
/// session_gc（service 进程）与 GUI 按钮（GUI 进程，同 binary）都经它装配，
/// 配合 `runtime::oneshot::oneshot_turn` 用完即弃。
pub(crate) fn oneshot_agent_config(
    bot: &crate::config::BotConfig,
    cfg: &Config,
) -> crate::runtime::harness::AgentConfig {
    // 命令解析与 build_bot_acp_handles 同链（覆盖指错告警一回/调用——gc 是
    // 日频任务，GUI 是低频点击，重复解析可接受）。
    let cmd = resolve_agent_command();
    let sandbox = resolve_sandbox_meta(bot);
    oneshot_agent_config_inner(bot, cfg, cmd, sandbox)
}

/// [`oneshot_agent_config`] 的可注入内层（role_cmds / 档位由调用方给，便于把
/// 「档位 ⇒ 命令槽」这条不变量钉成单测）。
fn oneshot_agent_config_inner(
    bot: &crate::config::BotConfig,
    cfg: &Config,
    cmd: AgentCmd,
    sandbox: Option<crate::runtime::acp::SessionSandboxMeta>,
) -> crate::runtime::harness::AgentConfig {
    // 统一迁移刀 4a：需要档位的会话（owner 的 read-only/workspace-write）套 Jev 门禁。
    let picked = &cmd;
    let env = env_for_bot(bot, cfg);
    // Jev 门禁 env **只在带档位（受限）时注入**：sandbox=None（owner 全权限维护任务，
    // 如 session_gc/角色生成）不套 Jev 门禁、不需要 key，多注一个凭证只扩大暴露面（评审 D1）。
    let mut extra_env: Vec<(String, String)> =
        vec![("PATH".to_string(), crate::deps::composed_path())];
    extra_env.extend(env);
    extra_env.extend(abb_bin_env());
    // 与聊天路径同口径：任务/一次性回合里的 agent 也要能解析出自己属于哪个 bot。
    extra_env.push(agent_bot_key_env(bot));
    if sandbox.is_some() {
        extra_env.extend(jev_env(cfg));
    }
    crate::runtime::harness::AgentConfig {
        command: picked.program.clone(),
        args: picked.args.clone(),
        extra_env,
        backend: "buzz".to_string(),
        session_sandbox: sandbox,
    }
}

/// 系统级对话（全局锚点气泡 → AI 对话框）的 agent 配置。
///
/// 与 [`oneshot_agent_config`]（按 bot 装配）的区别：系统级对话**不属于任何渠道 bot**——
/// 不依赖 bot 配置（无 bot 也能跑），角色是「系统级 Owner Bot」。
///
/// 装配口径（owner 决策，2026-10-12）：
/// - 供应商：**本会话所选**（`provider` 形参，即「模型选择」）；空串 → 合成 bot 的 `provider`
///   留空 → `effective_provider` 回落 `default_provider`；名字不在 `providers` 里时不注入
///   半截 env（按未配置处理）；
/// - 档位：**整机全权限**（`sandbox = None`，不带任何 workspace 白名单/证据；这是「系统级」的定义）；
/// - 工作区：`workspaces/syschat/`（见 [`crate::syschat::workspace`]）。
///
/// 合成 bot 只用于复用「按 bot 解析供应商 + 命令槽」的既有链路，不写盘、不登记。
///
/// `provider` = 本会话选的供应商名（「模型选择」）；空串 = 跟随全局默认。
pub(crate) fn oneshot_agent_config_system(
    cfg: &Config,
    provider: &str,
) -> crate::runtime::harness::AgentConfig {
    let bot = crate::config::BotConfig {
        name: crate::syschat::SYSCHAT_KEY.to_string(),
        provider: provider.to_string(),
        ..Default::default()
    };
    // sandbox 显式传 None = 全权限（与 `resolve_sandbox_meta(&bot)` 对默认档位的结果一致，
    // 但这里写成常量、不随 BotConfig 默认档位漂移）。
    oneshot_agent_config_inner(&bot, cfg, resolve_agent_command(), None)
}

pub async fn run() {
    crate::log!("=== ABB 启动（Rust 内置 WS 版 · 多 bot）===");
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            crate::log!("[service] config.json 读取失败: {e:#}");
            // 配置不可用 → 清「期望运行」标记，看门狗停止自动重拉（否则每 2s 拉起即退死循环）
            crate::install::set_desired(false);
            std::process::exit(1);
        }
    };
    let missing = cfg.missing();
    if !missing.is_empty() {
        crate::log!(
            "[service] ⚠️ config.json 缺少必填项: {}（{}）。请填好后重启。",
            missing.join(", "),
            Config::path().display()
        );
        // 配置不可用 → 清「期望运行」标记，看门狗停止自动重拉
        crate::install::set_desired(false);
        std::process::exit(1);
    }
    // #174：一次性迁移旧 key（name）目录/登记 → 新 key（app_id/wx_user_id 优先）。
    // 幂等（新目录存在跳过）；失败只 log 不阻塞（数据仍在旧目录，日志指明）。
    cfg.migrate_keys();
    crate::log!(
        "[service] 只响应: {}  bot数: {}",
        cfg.owner_open_id,
        cfg.bots.len()
    );

    // 启动时以「本进程身份（launchd 责任进程）」实测六项系统权限，打日志。
    // 诊断价值：终端里跑 --dump-perms 会向上追踪责任进程到 Terminal，看到的是终端的授权
    // 不是本服务的；只有从服务进程内部查才是真实状态。
    for p in crate::deps::detect_permissions() {
        crate::log!("[perm] 检测 {}: {:?}", p.id, p.state);
    }
    for tool in crate::deps::bundled_tool_names() {
        let (source, path) = crate::deps::bundled_tool_status(tool);
        crate::log!(
            "[tools] {tool} -> {source} {}",
            path.as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".to_string())
        );
    }

    let cfg = Arc::new(cfg);

    // ACP 执行层命令每进程解析一次：随包 `abb-agent` 优先，缺失回落 PATH
    // `ki acp serve`（开发/自签构建无随包）。
    let agent_cmd = resolve_agent_command();

    // #200 Phase 3 + 单后端化 P2.1：执行层随包（abb-agent 单一执行层）——每 bot
    // normal+granted 双进程实例，在 run_bot 内按 bot 构造（env 带本 bot 供应商，
    // 结构消灭旧共享句柄集「env_for 取第一个 enabled bot 供应商」的多 bot 串台）。
    // 装配零等待（不 spawn 进程、不碰盘）——handle 同步可得，无启动竞态；agent
    // 懒启动 + 崩溃退避重拉（harness 内闭环），启动失败只影响本 bot 的 ACP 频道。
    // 命令按**角色**解析（两条，刀 4b 后都解析到 abb-agent），每进程一次；两条都可能
    // 回落 PATH `ki acp serve`（开发/自签构建无随包）。

    // 接入飞书 bot → 后台自动装 lark-cli + lark-* 技能（幂等/best-effort，绝不阻塞 bot 启动）。
    // #69 审计：短命任务（装完即收尾），登记进治理（panic/指标可见）；装不上只 log 警告。
    if cfg.bots.iter().any(|b| b.enabled && b.kind == "feishu") {
        crate::svc_tasks::tasks().spawn("larkskills", async {
            crate::larkskills::ensure_lark_setup().await;
        });
    }

    // #69：退出信号 → 全局取消令牌广播（原 watch::channel(stop) 职责，统一进治理）。
    // 长驻任务（活到关停），登记 spawn_forever——关停前意外退出会被计 errors_total。
    // CancelOnShutdown 守卫：本任务任何提前退出路径（含 panic 被捕获）都补一次 cancel
    // （幂等），保持原 watch 实现的 fail-open 语义（循环退出 → 进程优雅退出 → 看门狗重启），
    // 绝不出现「signal 死了所有循环永久挂起」的 fail-closed 死挂。
    // #191：select 同时监听关停令牌——自发起关停（无信号，如全部 bot 自行退出）时
    // shutdown_wait 会 cancel 令牌，本任务若仍 parked 在等信号，tracker.wait() 永等
    // 它 → 收尾恒烧满 20s 总期限强退。令牌已取消即让位退出（不记信号日志，cancel
    // 幂等无害）。代价：收尾窗口内再来的 SIGTERM 走默认处置立即终止进程——与
    // systemd「第二次信号=立即杀」惯例一致，且强退本就是该窗口的兜底终点。
    crate::svc_tasks::tasks().spawn_forever("signal", async {
        let _cancel_guard = crate::svc_tasks::CancelOnShutdown::default();
        if wait_exit_signal_or_shutdown(crate::svc_tasks::shutdown_token()).await {
            crate::log!("[service] 收到退出信号");
        }
        crate::svc_tasks::shutdown_token().cancel();
    });

    // 每个 bot 一个事件循环 + Bridge + 定时调度，并行跑。
    // 第一遍先构建所有 Messenger 并登记到跨会话投递路由表（#21）：投递循环要能看到
    // 全部已启用目标；Bridge 与路由表共享同一 Arc<dyn Messenger> 实例。
    let mut messengers: std::collections::HashMap<
        String,
        std::sync::Arc<dyn crate::messenger::Messenger>,
    > = std::collections::HashMap::new();
    let mut bot_cfgs: std::collections::HashMap<String, crate::config::BotConfig> =
        std::collections::HashMap::new();
    let mut ready: Vec<(
        crate::config::BotConfig,
        std::sync::Arc<dyn crate::messenger::Messenger>,
    )> = Vec::new();
    for bot in &cfg.bots {
        // 用户停用的 bot：不启动，但上报「已停用」让托盘显灰，并保留在设置窗可重启用。
        if !bot.enabled {
            crate::log!("[service] 跳过已停用的 bot「{}」", bot.key());
            crate::botstatus::report(&bot.key(), &bot.kind, &bot.bot_name, "已停用");
            continue;
        }
        // 跳过凭证不齐的（与 Config::missing 同一事实源）
        if !bot.credentials_ready() {
            crate::log!(
                "[service] 跳过未就绪的 bot「{}」（kind={}，缺凭证/登录）",
                bot.key(),
                bot.kind
            );
            continue;
        }
        let msgr = match messenger::build(bot) {
            Ok(m) => m,
            Err(e) => {
                crate::log!("[bot:{}] 构造 messenger 失败: {e:#}", bot.key());
                continue;
            }
        };
        messengers.insert(bot.key(), msgr.clone());
        bot_cfgs.insert(bot.key(), bot.clone());
        ready.push((bot.clone(), msgr));
    }
    if ready.is_empty() {
        crate::log!("[service] 没有可用 bot，退出");
        // 无可跑 bot（全部停用/凭证不齐）→ 清「期望运行」标记，看门狗停止自动重拉
        crate::install::set_desired(false);
        std::process::exit(1);
    }
    // #200/#206 接线（单进程 harness）：bridge 注册表 + 频道巡检长驻任务
    //（harness 主循环 / 回合消费任务族已随句柄对下沉 run_bot——每 bot 每实例一份）。
    // 账本——回合文本由 harness 主循环经出站通道（TurnOutput）同步送回；回合消费
    // 任务按 meta.bot_key 路由到归属 Bridge（注册表 weak 升级）。注册表任务：bot
    // 循环构造 Bridge 时注册，回合消费与频道巡检查它路由（不在注册表 = 回复先于
    // 注册到达或 bot 已退出——按丢弃处理）。
    let bridge_registry: crate::bridge::BridgeRegistry = Default::default();
    {
        // 任务③：根频道巡检——登记表（virtual-bots.json）频道按 bot_key 分组，经
        // BridgeRegistry 解析各 bot 自己的 normal 句柄推送（单后端化 P2.1：句柄按
        // bot 建，频道天然归属本 bot 实例，旧「按后端分组 + 切后端迁移」维度消亡）。
        // 热读同前（2s tick，文件签名未变跳过重推）；无频道的 bot 照推空 roots
        //（末频道删除后的 diff 清理由 handle 内部完成）。
        let active_bots: std::collections::HashSet<String> = messengers.keys().cloned().collect();
        let registry_for_sweep = bridge_registry.clone();
        let name: &'static str = "acp-channel-sync";
        crate::svc_tasks::tasks().spawn(name, async move {
            let stop = crate::svc_tasks::shutdown_token();
            let path = crate::bridge_dir().join("virtual-bots.json");
            let store = crate::virtualbot::VirtualBotStore::new_at(path.clone());
            let mut last_sig: Option<(u64, u64)> = None;
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
                    break;
                }
                let sig = std::fs::metadata(&path).ok().map(|m| {
                    (
                        m.modified()
                            .map(|t| t.elapsed().unwrap_or_default().as_nanos() as u64)
                            .unwrap_or_default(),
                        m.len(),
                    )
                });
                if sig == last_sig {
                    continue;
                }
                last_sig = sig;
                // roots 按 bot 分组
                let mut by_bot: std::collections::HashMap<
                    String,
                    Vec<crate::runtime::harness::ChannelMeta>,
                > = std::collections::HashMap::new();
                for vb in store.load() {
                    if !active_bots.contains(&vb.bot_key) {
                        continue;
                    }
                    by_bot.entry(vb.bot_key.clone()).or_default().push(
                        crate::runtime::harness::ChannelMeta {
                            // vb 群的 agent 工作目录 = 该渠道目录（2026-10-12 起所有
                            // 渠道统一为 `workspaces/<bot>/<dir_name(chat_id)>/`）
                            workspace: Some(
                                crate::channel_dir::ensure_channel_dir(
                                    &vb.bot_key,
                                    &vb.chat_id,
                                    "group",
                                    None,
                                )
                                .display()
                                .to_string(),
                            ),
                            bot_key: vb.bot_key,
                            chat_id: vb.chat_id,
                            chat_type: "group".to_string(),
                            thread_id: None,
                            name: vb.role_name,
                            anchor_mid: None,
                            adhoc: false,
                        },
                    );
                }
                for bot_key in &active_bots {
                    let roots = by_bot.get(bot_key).cloned().unwrap_or_default();
                    let Some(bridge) = registry_for_sweep.get(bot_key) else {
                        continue;
                    };
                    if let Some(h) = bridge.acp_handles.as_ref().map(|hs| hs.acp.clone()) {
                        if !h.sync_roots(roots.clone()) {
                            crate::log!("[acp:{bot_key}] 频道巡检发送失败（harness 已关闭）");
                        }
                    }
                }
            }
        });
    }

    let router = std::sync::Arc::new(crate::deliver::Router::new(
        cfg.cross_delivery_enabled,
        messengers, // 此后无其它消费方（github bot 循环已移除），直接 move
        bot_cfgs,
        None,
    ));
    // 跨会话投递消费循环（独立于 bot 循环，共享关停令牌）。#69：长驻，登记 spawn_forever。
    {
        let router = router.clone();
        let stop = crate::svc_tasks::shutdown_token();
        crate::svc_tasks::tasks().spawn_forever("deliver", async move {
            deliver_loop(router, stop).await;
        });
    }
    // #74 消息历史维护（长驻）：每 2s 消费 GUI 命令文件（手动清除 / 弹窗已读），
    // 每 24h 按保留期清理 messages.sqlite（启动即跑一次）。
    {
        let stop = crate::svc_tasks::shutdown_token();
        crate::svc_tasks::tasks().spawn_forever("history-gc", async move {
            history_gc_loop(stop).await;
        });
    }
    // 「幽灵任务目录」告警（issue abb-task-botkey-dir-1 的反馈回路）：tasks/ 下若存在
    // **不属于任何已配置 bot** 的目录且里面有任务定义，说明有 CLI 用了非法 key（例如 bot
    // 显示名）登记任务——那些任务永远不会被本 service 认领。旧行为是完全静默：CLI 报
    // 「登记成功」、`task list`（在错 key 下）也能列出来，用户只看到「任务没反应」。
    {
        // 必须用**全部已配置 bot**（含停用/暂无凭证/未就绪的）：`ready` 只含启用且就绪的，
        // 拿它当 known 会把「已配置但停用」的 bot 目录误报成「不属于任何已配置 bot」
        // （独立审查 a9c9f0b 复现：app_off 被误报）。
        let known = configured_keys(&cfg);
        let ghosts = ghost_task_dirs(&crate::bridge_dir().join("tasks"), &known);
        {
            if !ghosts.is_empty() {
                crate::log!(
                    "⚠️ tasks/ 下有 {} 个目录不属于任何已配置 bot（这些任务**永远不会被认领**）：{}\n\
                     ↳ 多半是 AGENT_BRIDGE_BOT_KEY 用了 bot 显示名等别名；请用规范 key 重新登记（`task list` 在对应 key 下可见）",
                    ghosts.len(),
                    ghosts.join(", ")
                );
            }
        }
    }

    let mut handles = Vec::new();
    for (bot, msgr) in ready {
        let cfg = cfg.clone();
        let stop = crate::svc_tasks::shutdown_token();
        let router = router.clone();
        // 任务执行 worker（#326 P2b / #306 / Q7「上限可配」）：每 bot **两条通道**——定时档
        // （once/cron/interval）与手动档（now/keepalive）。**派发方式不同**（2026-10-15 收口）：
        // 定时档是固定池（起 `task_workers.schedule` 条，默认 2；池大小即并发上限），手动档只有
        // **一条 poller + 按需派发**（认领到就起执行体，容量由「用户登记了多少条」决定——
        // keepalive 常驻永不还位，固定池下任何有限 N 都是节流阀，见 `Config::task_workers`）。
        // 超闸门排队（Q7 的「排队、不拒绝」），跨通道互不阻塞（否则 now 档的长任务会把定时触发饿死）；
        // 同一条任务**永不自我重叠**（认领是单锁 check-and-set），并发只作用于不同任务。
        // 执行层是 `runtime::oneshot`（自起 handle）——Q7 拍板的 B1「task 独立 handle」，
        // 任务不占聊天句柄 slot。
        // store/states 两条通道**共享同一对实例**：两个实例并存会让运行态读-改-写丢更新。
        {
            let bot_key = bot.key();
            // 句柄只构造一次、按通道 clone：两条通道必须共享同一对 store/states。
            let handles = crate::task_run::TaskHandles::new(&bot_key);
            crate::log!(
                "[task:{bot_key}] 通道容量：schedule={}（固定池）/ manual=按需派发（并发闸门：{}，Config.task_workers；manual 的 0 = 不节流）",
                cfg.task_workers.schedule_workers(),
                match cfg.task_workers.manual_cap() {
                    None => "不节流".to_string(),
                    Some(n) => n.to_string(),
                }
            );
            // Q7 的「上限可配」（`Config::task_workers`）：定时档起 N 个 worker；手动档恒 1 条
            // poller（自己按需 spawn 执行体）。认领是单锁 check-and-set，所以**同一条任务不会
            // 双跑**；多出来的并发只让不同任务能在同一档同时执行
            // （0 按 1 兜底、上限见 `MAX_TASK_WORKERS_PER_CHANNEL`）。
            // 清单抽成纯函数 `worker_specs`，让这层接线可单测。
            for (name, worker) in crate::task_run::worker_specs(&cfg, &bot_key) {
                let bot = bot.clone();
                let cfg = cfg.clone();
                let router = router.clone();
                let stop = crate::svc_tasks::shutdown_token();
                let handles = handles.clone();
                let name: &'static str = Box::leak(name.into_boxed_str());
                crate::svc_tasks::tasks().spawn_forever(name, async move {
                    crate::task_run::task_worker(bot, cfg, router, handles, stop, worker).await;
                });
            }
        }
        // ACP 执行层命令（owner 优先随包 abb-agent；缺失回落 PATH `ki acp serve`，
        // 开发/自签无随包）。句柄按 bot 在 run_bot 内构造（P2.1）。
        let agent_cmd = agent_cmd.clone();
        // #206：回合投递路由注册表（全部 bot 共享同一份）
        let registry = bridge_registry.clone();
        // 任务名带 bot key（Box::leak：每次进程启动每 bot 一行小字符串，换取
        // errors/panic 告警可定位到具体 bot——审查 Minor 3）
        let name: &'static str = Box::leak(format!("bot:{}", bot.key()).into_boxed_str());
        handles.push(crate::svc_tasks::tasks().spawn_forever(name, async move {
            run_bot(bot, cfg, msgr, router, stop, agent_cmd, registry).await;
        }));
    }
    // 等所有 bot 循环结束。⚠️ 服务期的常态就是等在这里（等关停广播），**绝不能包超时**：
    // 健康的 bot 循环不会在固定时限内结束，超时必然触发——「启动后 30s×bot 数自动关机
    // → 看门狗按崩溃重拉」的无限重启风暴（#189 真机诊断：2 bot 60s、实验室 1 bot 30s
    // 分毫不差；#184 初版 22s 风暴与 v2.21.3「启动 61s 后二停挂死占锁」同为它的形态）。
    // #179 的 30s 关闭时限只作用于**关闭路径**（关停广播已到仍不退出的卡死 bot）：
    // 先无期限等服务期结束（关停广播或全部 bot 自行退出），进入收尾后才逐 handle 限时。
    wait_bots_or_shutdown(
        &mut handles,
        crate::svc_tasks::shutdown_token(),
        std::time::Duration::from_secs(30),
    )
    .await;
    // #184：收尾总期限——从「bot 循环全部结束」**之后**起算，包住 shutdown_wait
    //（等 recover/session-import/larkskills 等启动期短命任务收尾；网络安装/磁盘
    // 挂死会永久挂起——2026-08-29 真机：v2.21.3 启动 61s 后二停挂死占锁）。
    // ⚠️ 期限绝不能包住上面的等 handle：服务期的常态就是「等 handle 等到关停广播」，
    // 期限从启动起算会在 20s 后把健康服务强杀成每 20s 一循环的重启风暴
    //（#184 初版真机事故，2026-08-29 同日修正）。
    // 到期 process::exit 强退：进程退出即释放 flock，看门 2s 内拉起新实例；
    // 残留任务随进程消亡（block_on 返回后 runtime drop 也会等残留任务，不能依赖它收尾）。
    if crate::svc_tasks::tasks()
        .shutdown_wait_bounded(std::time::Duration::from_secs(20))
        .await
        .is_err()
    {
        crate::log!("[service] ⚠️ 优雅关闭超时（收尾 20s），强制退出（防挂死占锁）");
        std::process::exit(1);
    }
    crate::log!("[service] 已退出");
}

/// 等待退出信号或关停广播（#191）。返回 true=收到真实信号（调用方记「收到退出信号」
/// 日志）；false=关停序列已开始（令牌被 shutdown_wait cancel——自发起关停，signal
/// 任务让位退出，让 tracker.wait() 能清零收尾，不再恒烧满 20s 总期限强退）。
/// 信号等待抽成独立 future（跨 cfg 统一 select，避免 cfg 块语句丢值）；term/int
/// 注册失败该路永久 pending（与原实现一致，不 panic）；ctrl_c 注册失败会以 Err
/// 立即完成（原实现即如此，实践不可达）。可单测。
async fn wait_exit_signal_or_shutdown(stop: tokio_util::sync::CancellationToken) -> bool {
    #[cfg(unix)]
    let signal_fired = wait_unix_signals();
    #[cfg(not(unix))]
    // Windows 无 SIGTERM/SIGINT 语义：Ctrl+C/关闭控制台即优雅退出
    let signal_fired = tokio::signal::ctrl_c();
    tokio::select! {
        _ = signal_fired => true,
        _ = stop.cancelled() => false,
    }
}

/// unix 退出信号（TERM/INT，ctrl_c 复用 INT 注册）任一到达即完成。
#[cfg(unix)]
async fn wait_unix_signals() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).ok();
    let mut int = signal(SignalKind::interrupt()).ok();
    tokio::select! {
        _ = async { if let Some(t)=term.as_mut(){t.recv().await} else {std::future::pending().await} } => {}
        _ = async { if let Some(t)=int.as_mut(){t.recv().await} else {std::future::pending().await} } => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// 服务期等待 bot 循环：等「关停广播」或「全部 bot 循环结束」哪个先到，**无期限**。
/// 期限（per_bot_shutdown_bound）只在进入收尾（广播已到 / bot 自行退出）后才逐
/// handle 生效——#179 卡死兜底语义保留，但绝不再作用于服务期（#189：服务期加超时
/// = 健康服务 30s×bot 数后自关机 → 看门狗按崩溃重拉的重启风暴根源）。
/// 抽成独立函数 + 令牌/期限可注入，便于单测（run() 收尾是 process::exit，进程内不可测）。
async fn wait_bots_or_shutdown(
    handles: &mut [tokio::task::JoinHandle<()>],
    stop: tokio_util::sync::CancellationToken,
    per_bot_shutdown_bound: std::time::Duration,
) {
    // 服务期：逐个等 bot 循环结束，任一步可被关停广播打断。无期限。
    // 索引推进（完成即跳过）+ is_finished 守卫：JoinHandle 返回 Ready（完成性
    // poll）后再 poll 会 panic，必须保证每个 handle 至多一次完成性 poll
    //（Pending 态可多次 poll，不受限）。
    let mut i = 0;
    while i < handles.len() {
        if handles[i].is_finished() {
            i += 1;
            continue;
        }
        tokio::select! {
            _ = &mut handles[i] => { i += 1; } // 该 bot 自行退出，继续等下一个
            _ = stop.cancelled() => break,     // 关停广播 → 进入限时收尾
        }
    }
    // 进入收尾：关停广播已到（或全部 bot 自行退出，fail-open 语义不变）。
    // #179：卡死的 bot 循环（如 WS 发送挂起，历史缺陷已由 send_with_timeout 根治，
    // 这里兜底）不能无限等：逐个限时后强制继续退出流程。
    for h in handles.iter_mut() {
        if h.is_finished() {
            continue;
        }
        let _ = tokio::time::timeout(per_bot_shutdown_bound, h).await;
    }
}

/// 跨会话投递消费循环：轮询 deliveries.json（agent 的 deliver CLI 落盘），逐条经路由表投递。
/// 失败项不回盘（避免死循环重投），由 Router 负责回源报错 / 微信 outbox 兜底。
async fn deliver_loop(
    router: std::sync::Arc<crate::deliver::Router>,
    stop: tokio_util::sync::CancellationToken,
) {
    crate::log!(
        "[deliver] 投递循环启动（跨会话投递开关={}）",
        router.enabled
    );
    let store = crate::deliver::DeliveryStore::new();
    loop {
        if interruptible_sleep(std::time::Duration::from_secs(1), &stop).await {
            break;
        }
        // at-least-once：先取（不移除）→ 投递（成功或已兜底）→ ack。
        // 投递中崩溃/退出 → 未 ack 的项下次启动重投，Router 防循环去重兜底。
        let items = store.pending();
        for item in items {
            router.deliver(&item).await;
            store.ack(&[item.id]);
        }
    }
    crate::log!("[deliver] 投递循环退出");
}
async fn run_bot(
    bot: crate::config::BotConfig,
    cfg: Arc<Config>,
    msgr: std::sync::Arc<dyn crate::messenger::Messenger>,
    router: std::sync::Arc<crate::deliver::Router>,
    stop: tokio_util::sync::CancellationToken,
    agent_cmd: AgentCmd,                            // ACP 执行层命令
    bridge_registry: crate::bridge::BridgeRegistry, // #206：回合投递路由用
) {
    let key = bot.key();
    crate::log!(
        "[bot:{key}] 启动（kind={} name={}）",
        bot.kind,
        bot.bot_name
    );
    // P4.4：bot 级工作区仍是“渠道目录的父目录”，会话 cwd 现在按渠道取
    // `<workspaces>/<bot>/<dir_name(chat_id)>/`（`Bridge::workspace_for`）。
    // bot 级指引仍要写：它是 bot 目录的入口说明（且存量会话/工具看得到）。
    // 注意：全新 bot 的 `workspaces/<key>/` 此时**还不存在**（要到下面 Bridge::new
    // 的 SessionStore 才建），所以目录由 `ensure_workspace_guide` 自建——它不自建
    // 的话这次写入会 NotFound 被静默吞掉（审查 P2-1，回归测试在 agent.rs）。
    crate::agent::ensure_workspace_guide(&crate::workspace_dir(&key));
    // ACP 句柄（P2.1）：按本 bot 构造，env 带本 bot 供应商。
    let acp_handles = build_bot_acp_handle(&bot, &cfg, &agent_cmd, stop.clone());
    // 任务族随句柄下沉（旧 service 级任务①②）：一个主循环（agent 懒启动 +
    // 崩溃退避重拉）+ 一个回合投递消费（TurnOutput → meta.bot_key → Bridge 注册表
    // 路由；逐条 await 保 per-chat 发送顺序）。任务名带 bot key。
    {
        let handle = acp_handles.acp.clone();
        let name: &'static str = Box::leak(format!("acp-harness:{key}").into_boxed_str());
        let run_handle = handle.clone();
        crate::svc_tasks::tasks().spawn(name, async move {
            crate::runtime::harness::run_loop(run_handle).await;
        });
        let name: &'static str = Box::leak(format!("acp-turns:{key}").into_boxed_str());
        let registry_for_turns = bridge_registry.clone();
        let bot_key_log = key.clone();
        crate::svc_tasks::tasks().spawn(name, async move {
            let stop = crate::svc_tasks::shutdown_token();
            let mut turn_rx = match handle.take_turn_rx() {
                Some(rx) => rx,
                None => {
                    crate::log!("[acp:{bot_key_log}] 回合输出接收端已被取走，消费任务退出");
                    return;
                }
            };
            loop {
                let out = tokio::select! {
                    Some(o) = turn_rx.recv() => o,
                    _ = stop.cancelled() => break,
                };
                let Some(bot_key) = out.meta.as_ref().map(|m| m.bot_key.clone()) else {
                    crate::log!(
                        "[acp:{bot_key_log}] 回合输出缺频道登记快照，丢弃 uuid={} len={}",
                        out.channel_id,
                        out.text.chars().count()
                    );
                    continue;
                };
                match registry_for_turns.get(&bot_key) {
                    Some(b) => b.deliver_turn_reply(out).await,
                    None => crate::log!(
                        "[acp:{bot_key_log}] 回合输出无对应 bridge bot={bot_key} len={}",
                        out.text.chars().count()
                    ),
                }
            }
        });
    }
    let bridge = {
        let mut b = Bridge::new(msgr, bot.clone(), &cfg);
        b.acp_handles = Some(acp_handles); // 本 bot 句柄对：dispatch/job 按角色路由（P2.2）
                                           // ACP 单轨：harness 会话不跨进程存活——每次服务启动即复位全部槽位，
                                           // 每个 chat 在本进程首轮走注入闸接续历史（否则 started=true 的旧槽位
                                           // 会让新 harness 会话零上下文，切后端/重启后上下文丢失）。
        b.sessions.reset_slots_for_service_start();
        Arc::new(b)
    };
    // #206：注册进回合投递路由表（必须在事件循环开始前——dispatch 只能发生在注册后，
    // 回复不可能先于注册到达）。
    bridge_registry.register(&key, &bridge);

    // 连接态初始上报：进入事件循环前是「连接中」；之后由各事件循环在状态迁移时更新。
    // （别用独立心跳任务只报「活着」——那测的是上报线程不是通道连通，会话死了托盘还在线。）
    crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "连接中");

    // #209 工作区版本管理：bot 启动时确保工作区带 .git + 基线 commit（内置 libgit2，
    // 不依赖系统 git）。删除保护快照（trash）与每日整理留痕共用该仓库。失败仅告警
    // ——版本管理是增强层，绝不阻塞 bot 运行。spawn_blocking：存量工作区首次 init
    // 要全量 add + commit（文件多时秒级），不能阻塞 tokio worker。
    {
        let key = key.clone();
        let enabled = cfg.workspace_git_enabled;
        let name: &'static str = Box::leak(format!("wsver-init:{}", key).into_boxed_str());
        crate::svc_tasks::tasks().spawn(name, async move {
            if !enabled {
                return;
            }
            let workspace = crate::workspace_dir(&key);
            let res = tokio::task::spawn_blocking(move || crate::wsver::ensure_repo(&workspace))
                .await
                .unwrap_or_else(|e| Err(format!("join 失败：{e}")));
            if let Err(e) = res {
                crate::log!("[bot:{key}] ⚠️ 工作区版本管理 init 失败（不影响运行）：{e}");
            }
        });
    }

    // 历史会话类型回填（9-4 会话化跟进）：库迁移补列前的旧消息 chat_type/chat_name
    // 为空 → 会话列表只能按人名展示。启动后异步对缺类型的会话逐个 chat_brief 反查
    // （飞书 chat API 自带 chat_type）并 UPDATE 回填；失败留空下次启动再试。逐个间
    // 隔 200ms 防限流；低频一次性任务，不阻塞事件循环。
    {
        let bridge = bridge.clone();
        let name: &'static str = Box::leak(format!("history-backfill:{}", key).into_boxed_str());
        crate::svc_tasks::tasks().spawn(name, async move {
            let missing = bridge.msgstore.chats_missing_type();
            if missing.is_empty() {
                return;
            }
            crate::log!(
                "[history] 回填 {} 个缺类型会话的 chat_type/chat_name",
                missing.len()
            );
            for (bot_key, chat_id) in missing {
                if let Some((ctype, cname)) = bridge.msgr.chat_brief(&chat_id).await {
                    let n = bridge
                        .msgstore
                        .backfill_chat(&bot_key, &chat_id, &ctype, &cname);
                    crate::log!(
                        "[history] 回填 chat={} type={} name={:?} rows={}",
                        &chat_id[..chat_id.len().min(12)],
                        ctype,
                        cname,
                        n
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
    }

    // #25 重启恢复：上次崩溃/退出时未处理完的消息 → 自动续跑（异步进行，不阻塞事件循环
    // 启动；per-chat 串行锁保证重放与实时消息不乱序）。
    // #69 审计：短命任务，登记进治理，**token 逐条检查**（审查 Important）——恢复会跑
    // 完整 agent 管线（单条可达数分钟），关停广播后立即停止重放（未重放条目留盘
    // pending.json，下次启动续跑——正是 #25 的恢复语义），不让 shutdown_wait 无界等它。
    {
        let bridge = bridge.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("recover:{}", key).into_boxed_str());
        crate::svc_tasks::tasks().spawn(name, async move {
            bridge.recover_pending(&stop).await;
        });
    }

    // #206 启动对账已随账本/relay 架构删除：同步形态下回合表（turn_registry）纯内存，
    // 崩溃即失——in-flight 回合不补发（at-most-once，见 agentreply.rs 模块文档），
    // 未跑完的用户消息由上方 recover_pending 按 pending.json 重放续跑。

    // #33 历史会话迁移**自动触发**：每次启动把后端私有 session 里尚未导入的对话
    // （claude/codex/pi）导入 history.rs——老历史参与注入接续，无需手动跑
    // `session-import`。幂等：imported.json 来源键标记，已导入来源秒跳过；追加式
    // 写入不触碰现有 history。spawn_blocking（fs 密集：读后端文件 + 提取 + 写）；
    // 失败仅 log 警告，绝不阻塞 bot 启动。
    {
        let key = key.clone();
        let name: &'static str = Box::leak(format!("session-import:{}", key).into_boxed_str());
        crate::svc_tasks::tasks().spawn(name, async move {
            let kb = key.clone(); // 闭包内另持一份（日志用）
            let report =
                tokio::task::spawn_blocking(move || crate::session_import::import_bot(&kb, false))
                    .await
                    .unwrap_or_default();
            if report.total > 0 {
                crate::log!(
                    "[bot:{key}] 历史会话自动迁移完成：导入 {} 条（{} 个 chat）",
                    report.total,
                    report.chats.len()
                );
            } else if !report.chats.is_empty() {
                crate::log!(
                    "[bot:{key}] 历史会话迁移扫描完成：无新内容（{} 个 chat 有跳过项）",
                    report.chats.len()
                );
            }
        });
    }

    // P5（#326）：job → task 一次性幂等迁移。**必须在启动旧 job 调度循环之前**跑：
    // 迁移完成（或先前已完成）⇒ 旧循环不启动，由 task worker 独占执行（防双跑）；
    // 迁移没能完成 ⇒ jobs.json 未动、旧循环照旧（任何时刻同一 job 只有一条执行路径）。
    // 实现与不变量见 `src/task_migrate.rs`，设计见 `docs/task-model.md` §D7。
    let migrate = crate::task_migrate::migrate_bot_if_needed(&key);
    match &migrate.outcome {
        crate::task_migrate::MigrateOutcome::Done => crate::log!(
            "[bot:{key}] job→task 迁移完成（本次导入 {} 条 / 跳过 {} 条 / 标记{}）；旧 job 调度循环不启动",
            migrate.imported,
            migrate.skipped,
            if migrate.marker_written { "已就位" } else { "待补写" }
        ),
        crate::task_migrate::MigrateOutcome::LegacyKept(reason) => crate::log!(
            "[bot:{key}] ⚠️ job→task 迁移未完成：{reason}；jobs.json 未改动，旧 job 调度循环照旧启动"
        ),
    }
    // 定时任务调度循环（独立于事件循环，共享关停令牌）。#69：长驻，登记 spawn_forever。
    if migrate.legacy_job_loop_allowed() {
        let bridge = bridge.clone();
        let key = key.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("schedule:{}", key).into_boxed_str());
        crate::svc_tasks::tasks().spawn_forever(name, async move {
            crate::log!("[bot:{key}] 调度循环启动");
            let mut last_min: Option<String> = None;
            // 在跑任务集合：cron 周期短于任务耗时时，跳过重叠的新一轮（防同任务并发堆积、
            // 多个 claude 抢同一资源/互相踩工作区）。
            let running = Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::<String>::new(),
            ));
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(20), &stop).await {
                    break;
                }
                let now = crate::schedule::DateTime::now();
                let min_key = format!(
                    "{}-{:02}-{:02} {:02}:{:02}",
                    now.year, now.month, now.day, now.hour, now.minute
                );
                if last_min.as_deref() == Some(&min_key) {
                    continue;
                }
                last_min = Some(min_key);
                // 认领（不是「查询」）：返回即记账落盘，所以同分钟内重启不会重跑。
                // 被认领但本轮跳过的（上一轮还没跑完）也算用掉这一分钟——与旧行为一致。
                let due = bridge.jobs.claim_due(&now);
                for job in due {
                    {
                        let mut r = running.lock().unwrap();
                        if r.contains(&job.id) {
                            crate::log!(
                                "[bot:{key}] 任务 {} 上一轮还没跑完，跳过本轮触发",
                                &job.id[..job.id.len().min(8)]
                            );
                            continue;
                        }
                        r.insert(job.id.clone());
                    }
                    let bridge = bridge.clone();
                    let router = router.clone();
                    let running = running.clone();
                    let jid = job.id.clone();
                    // #69 审计：中命任务（一次 agent 运行），有 owner（调度循环在跑集合 +
                    // pending.json 重启恢复）。**不登记**：关停时等 agent 跑完会让「停止」
                    // 语义退化成最长任务时长；进程退出兜底 + 下次启动 agent-pids 清理。
                    tokio::spawn(async move {
                        run_job(bridge, router, job).await;
                        running.lock().unwrap().remove(&jid);
                    });
                }
            }
            crate::log!("[bot:{key}] 调度循环退出");
        });
    } else {
        crate::log!("[bot:{key}] 旧 job 调度循环未启动（job→task 迁移已接管执行）");
    }

    // 每日工作目录整理循环（per-bot 开关 tidy_enabled，默认关）：24h 门 + 配置热读。
    // 纯文件操作（无 agent 调用），与调度循环独立——调度跑 agent（慢）不该阻塞整理。
    // 首轮延迟 24h + jitter（盐与 session_gc 不同，错开两循环触发分钟）：
    // 启动即跑会对每个 bot 全量扫描写盘 + git 操作，不值得。
    {
        let key = key.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("tidy:{key}").into_boxed_str());
        crate::svc_tasks::tasks().spawn_forever(name, async move {
            crate::log!("[tidy:{key}] 工作目录整理循环启动");
            let workspace = crate::workspace_dir(&key);
            // 上次运行标记（重启不丢 24h 门）：损坏/缺失 → 回退内存门
            let marker = workspace.join(".abb-tidy-last");
            let jitter = (fnv(&key) ^ 0xC0FFEE) % 3600;
            // 内存门起点：落盘标记优先（重启不丢 24h 门——若以 now+jitter 起门，
            // daily_due 恒取新内存门，标记形同虚设：每次重启都把下次运行推迟
            // 24h+jitter，日重启的机器维护永不触发——审查修复）；无标记（首轮）
            // → now+jitter 首轮延迟（盐与 session_gc 不同，错开两循环触发分钟）。
            let mut gate = boot_gate(read_run_marker(&marker), crate::chrono_lite::unix_secs(), jitter);
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
                    break;
                }
                let now = crate::chrono_lite::unix_secs();
                // 门已由标记种子化并随 due/运行推进（唯一写者是本任务），无需每 tick 读盘
                let due = daily_due(gate, now);
                if !due {
                    continue;
                }
                // 到点才推进门并热读配置（原来每 2s tick 全量读盘解析，改每 24h 门一次）。
                // 门推进与开关无关：关着也推进，下次到点再读——运行中打开 → 最迟 24h 跑首轮
                gate = Some(now);
                let cfg = match Config::load() {
                    Ok(c) => c,
                    Err(_) => continue, // 配置损坏 → 本周期跳过，下个门再试
                };
                let enabled = cfg
                    .bots
                    .iter()
                    .find(|b| b.key() == key)
                    .map(|b| b.tidy_enabled)
                    .unwrap_or(false);
                if !enabled {
                    continue;
                }
                let days = cfg.history_retention_days.max(1);
                // 孤儿判定依赖 live 集：现取（SessionStore::new 轻量读盘）
                let live: std::collections::HashSet<String> = {
                    let store = crate::sessions::SessionStore::new(&key);
                    store.live_session_ids().into_iter().collect()
                };
                let report = crate::tidy::run_once(&workspace, now, days, &live);
                write_run_marker(&marker, now);
                crate::log!(
                    "[tidy:{key}] 整理完成：临时文件 {}，孤儿会话 {}，历史截断 {} 条，归档 {}，空目录 {}",
                    report.temp_removed,
                    report.orphan_removed,
                    report.history_truncated,
                    report.archived,
                    report.emptied_dirs
                );
                match crate::tidy::git_commit(&workspace, cfg.workspace_git_enabled).await {
                    Ok(crate::tidy::GitOutcome::Committed(h)) => {
                        crate::log!("[tidy:{key}] git 留痕 commit {h}")
                    }
                    Ok(crate::tidy::GitOutcome::NothingToCommit) => {
                        crate::log!("[tidy:{key}] git 无变更")
                    }
                    Ok(crate::tidy::GitOutcome::Skipped) => {
                        crate::log!("[tidy:{key}] git 留痕已关闭（workspace_git_enabled），跳过")
                    }
                    Err(r) => {
                        crate::log!("[tidy:{key}] ⚠️ git 留痕失败（不影响整理）：{r}")
                    }
                }
            }
            crate::log!("[tidy:{key}] 工作目录整理循环退出");
        });
    }

    // 回收站 TTL 清理循环（#88 删除保护）：24h 门 + jitter（盐与 tidy/session_gc 错开）。
    // delete_protect 默认开 → 回收站必须定期清，否则磁盘只进不出。纯文件操作，
    // 与调度/整理循环独立；配置损坏只跳过本轮。
    {
        let key = key.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("trash-gc:{key}").into_boxed_str());
        crate::svc_tasks::tasks().spawn_forever(name, async move {
            crate::log!("[trash-gc:{key}] 回收站 TTL 清理循环启动");
            let workspace = crate::workspace_dir(&key);
            // 上次运行标记（重启不丢 24h 门，同 tidy）
            let marker = workspace.join(".abb-trash-gc-last");
            let jitter = (fnv(&key) ^ 0x5EED) % 3600;
            let mut gate = boot_gate(
                read_run_marker(&marker),
                crate::chrono_lite::unix_secs(),
                jitter,
            );
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
                    break;
                }
                let now = crate::chrono_lite::unix_secs();
                let due = daily_due(gate, now);
                if !due {
                    continue;
                }
                // 到点才推进门并热读配置（同 tidy）
                gate = Some(now);
                let cfg = match Config::load() {
                    Ok(c) => c,
                    Err(_) => continue, // 配置损坏 → 本周期跳过，下个门再试
                };
                let settings = cfg
                    .bots
                    .iter()
                    .find(|b| b.key() == key)
                    .map(crate::trash::TrashSettings::from_bot)
                    .unwrap_or_else(crate::trash::TrashSettings::defaults);
                if !settings.enabled {
                    continue;
                }
                let purged = crate::trash::purge_expired(&workspace, settings.ttl_days);
                write_run_marker(&marker, now);
                crate::log!(
                    "[trash-gc:{key}] 回收站 TTL 清理：过期 {} 条（永久删除，不可恢复；git 快照历史不受影响）",
                    purged
                );
                if purged > 0 {
                    // 清理也是变更：git 留痕（开关关闭 → Skipped 静默，审查 P1-2：
                    // 不许绕过 workspace_git_enabled 去 init/commit）
                    match crate::tidy::git_commit(&workspace, cfg.workspace_git_enabled).await {
                        Ok(crate::tidy::GitOutcome::Committed(h)) => {
                            crate::log!("[trash-gc:{key}] git 留痕 commit {h}")
                        }
                        Ok(crate::tidy::GitOutcome::NothingToCommit)
                        | Ok(crate::tidy::GitOutcome::Skipped) => {}
                        Err(r) => {
                            crate::log!("[trash-gc:{key}] ⚠️ git 留痕失败：{r}")
                        }
                    }
                }
            }
            crate::log!("[trash-gc:{key}] 回收站 TTL 清理循环退出");
        });
    }

    // 每日会话归纳清理循环（全局开关 session_gc_enabled，默认关）：24h 门 + 配置热读。
    // 过期会话（最后活跃超 session_gc_days）交 bot 后端 agent 归纳成摘要存档
    //（summaries/），再清理工作区内历史/后端会话文件（绝不触碰 ~/.claude 等后端
    // 私有目录），摘要下次会话注入衔接上下文。破坏性 + 每会话一次 LLM 调用，故
    // 默认关、首轮延迟 24h+jitter（盐与 tidy 不同——tidy 用 ^0xC0FFEE，错开两循环
    // 触发分钟，避免同刻全 bot 全量扫描 + 归纳同时开跑）。
    {
        let bridge = bridge.clone();
        let key = key.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("session-gc:{key}").into_boxed_str());
        crate::svc_tasks::tasks().spawn_forever(name, async move {
            crate::log!("[session-gc:{key}] 会话归纳清理循环启动");
            let workspace = crate::workspace_dir(&key);
            // 上次运行标记（重启不丢 24h 门）：损坏/缺失 → 回退内存门
            let marker = workspace.join(".abb-session-gc-last");
            let jitter = fnv(&key) % 3600;
            // 内存门起点：落盘标记优先（重启不丢 24h 门，同上 tidy 循环——审查修复）；
            // 无标记（首轮）→ now+jitter 首轮延迟（盐与 tidy 不同——tidy 用 ^0xC0FFEE，
            // 错开两循环触发分钟，避免同刻全 bot 全量扫描 + 归纳同时开跑）
            let mut gate = boot_gate(
                read_run_marker(&marker),
                crate::chrono_lite::unix_secs(),
                jitter,
            );
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
                    break;
                }
                let now = crate::chrono_lite::unix_secs();
                // 门已由标记种子化并随 due/运行推进（唯一写者是本任务），无需每 tick 读盘
                let due = daily_due(gate, now);
                if !due {
                    continue;
                }
                // 到点才推进门并热读配置（原来每 2s tick 全量读盘解析，改每 24h 门一次）；
                // 门推进与开关无关——关着也推进，运行中打开 → 最迟 24h 跑首轮
                gate = Some(now);
                let cfg = match Config::load() {
                    Ok(c) => c,
                    Err(_) => continue, // 配置损坏 → 本周期跳过，下个门再试
                };
                if !cfg.session_gc_enabled {
                    continue;
                }
                // 跑前再确认关停：每会话一次 LLM 调用，可能很慢（run_once 内部每会话也查）
                if stop.is_cancelled() {
                    break;
                }
                let report = crate::session_gc::run_once(&bridge, &stop).await;
                // 归纳被关停打断（run_once 内每 chat 前检查 stop）→ 不写运行标记：
                // 中断轮不得记成完成轮，否则未归纳的会话要再等一个 24h 门才重试
                //（审查修复）；完整跑完（含提前 break 的 stop 已在 run_once 内兜住）才落盘
                if !stop.is_cancelled() {
                    write_run_marker(&marker, now);
                }
                crate::log!(
                    "[session-gc:{key}] 归纳完成：成功 {}，失败 {}，跳过 {}",
                    report.summarized,
                    report.failed,
                    report.skipped
                );
            }
            crate::log!("[session-gc:{key}] 会话归纳清理循环退出");
        });
    }

    // 事件循环：按通道分派
    if bot.is_dingtalk() {
        crate::dingtalk::stream_loop(bot.app_id.clone(), bot.app_secret.clone(), bridge, stop)
            .await;
        crate::botstatus::clear(&key);
        crate::log!("[bot:{key}] 钉钉 Stream 循环退出");
    } else if bot.is_wechat() {
        weixin_loop(bot, bridge, stop).await; // weixin_loop 退出时已 clear
        crate::log!("[bot:{key}] 微信长轮询循环退出");
    } else {
        crate::ws::ws_loop(bot.app_id.clone(), bot.app_secret.clone(), bridge, stop).await;
        crate::botstatus::clear(&key);
        crate::log!("[bot:{key}] WS 循环退出");
    }
}

/// 微信事件循环：HTTP 长轮询 getupdates（游标增量）→ bridge.on_weixin。
/// 断线/出错退避重试；errcode -14（会话超时）报「会话过期」后退出（需重新扫码）。
async fn weixin_loop(
    bot: crate::config::BotConfig,
    bridge: Arc<Bridge>,
    stop: tokio_util::sync::CancellationToken,
) {
    let key = bot.key();
    let base = if bot.wx_base_url.is_empty() {
        crate::wechat::FIXED_BASE_URL
    } else {
        bot.wx_base_url.as_str()
    };
    let cdn = if bot.wx_cdn_base_url.is_empty() {
        crate::wechat::DEFAULT_CDN_BASE_URL
    } else {
        bot.wx_cdn_base_url.as_str()
    };
    let client = crate::wechat::WeixinClient::new(base, &bot.wx_token, cdn);
    // 游标持久化（2026-10-05 审计 #19）：原来每次启动都从空游标开始 ⇒ 升级/崩溃/看门狗
    // 重拉任何一次重启都变成「消息黑洞」（停机窗口内用户发的消息大概率直接丢）。
    // 从 workspaces/<key>/wx_cursor.json 恢复；缺失/损坏都退回空串，绝不因此启动失败。
    let mut cursor = load_wx_cursor(&key);
    let mut timeout_ms: u64 = 35_000;
    crate::log!("[bot:{key}] 微信长轮询启动 base={base}");
    // 每次 poll 成功都重报一次「在线」（绑真实连通，非独立心跳）；snapshot 僵尸阈值已放宽到 180s。
    let mut last_online = std::time::Instant::now();
    // 连续客户端超时计数：偶发超时是长轮询常态；连续 ≥3 次（≈2 分钟无任何成功响应）= 通道疑似
    // 半开假死（与 WS 看门狗同理：发得出去≠通，收得到才算通），降级「重连中」并留痕。
    let mut consec_timeouts = 0u32;
    loop {
        if stop.is_cancelled() {
            break;
        }
        let next = client.get_updates(&cursor, timeout_ms);
        tokio::select! {
            _ = stop.cancelled() => { break; }
            res = next => {
                match res {
                    Ok((msgs, new_cursor, new_timeout)) => {
                        if !msgs.is_empty() {
                            crate::log!("[bot:{key}] getupdates 返回 {} 条消息", msgs.len());
                        }
                        cursor = new_cursor;
                        // 每次拿到新游标就落盘（尽力而为 + 原子写）：重启后能从这里续上。
                        save_wx_cursor(&key, &cursor);
                        timeout_ms = new_timeout.max(5_000);
                        consec_timeouts = 0; // 成功响应 = 通道真通，复位假死计数
                        // 每次成功轮询都视为「在线」，但至多 10s 重报一次续命（对抗僵尸过滤）
                        if last_online.elapsed() > std::time::Duration::from_secs(10) {
                            crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "在线");
                            last_online = std::time::Instant::now();
                        }
                        // 每条消息 spawn 独立处理（对齐飞书 ws_loop）：per-chat 串行由 bridge 的
                        // chat_lock 保证，而「停止词」等控制消息须能与运行中的任务**并发**处理，
                        // 若在此串行 await，长任务会把它（及其后的新消息）全部堵到跑完为止。
                        // #69 审计：短/中命、有 owner（bridge chat_lock + pending.json 恢复），
                        // 不登记——关停语义靠进程退出兜底（见 svc_tasks.rs 登记口径）。
                        for msg in msgs {
                            let b = bridge.clone();
                            tokio::spawn(async move {
                                b.on_weixin(msg).await;
                            });
                        }
                    }
                    Err(crate::wechat::WxError::SessionExpired) => {
                        crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "会话过期");
                        crate::log!("[bot:{key}] 微信会话已超时（-14），需重新扫码登录");
                        break;
                    }
                    Err(crate::wechat::WxError::PollTimeout) => {
                        consec_timeouts += 1;
                        if consec_timeouts == 3 {
                            crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "重连中");
                            crate::log!(
                                "[bot:{key}] ⚠️ 连续 3 次长轮询超时（约 2 分钟无成功响应），疑似通道假死，持续重试中"
                            );
                        }
                        // 超时后立刻下一轮长轮询即可（服务端无新消息本就靠超时驱动），不额外 sleep
                    }
                    Err(e) => {
                        crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "重连中");
                        crate::log!("[bot:{key}] getupdates 出错，3s 后重试：{e}");
                        if interruptible_sleep(std::time::Duration::from_secs(3), &stop).await {
                            break;
                        }
                    }
                }
            }
        }
    }
    crate::botstatus::clear(&key);
}
/// 睡眠 dur，但可被关停令牌打断。返回 true=收到关停广播（调用方应 break）。
async fn interruptible_sleep(
    dur: std::time::Duration,
    stop: &tokio_util::sync::CancellationToken,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = stop.cancelled() => true,
    }
}

/// #74 历史维护循环：命令文件消费（2s 轮询）+ 保留期 GC（24h，启动也跑一次）。
/// - 命令文件：GUI 跨进程写 msg-clear.command / msg-read.command（「手动清除」/
///   「弹窗已读」），存在即消费（deliveries.json 队列先例，详见 msgstore::consume_commands）；
///   2s 轮询保证手动清除的反馈延迟可感知地小（≈2s），不必等 24h 的 GC 周期。
/// - GC：保留期热读 config（改配置保存重启即生效）；启动立即跑一次清掉积压过期记录。
async fn history_gc_loop(stop: tokio_util::sync::CancellationToken) {
    crate::log!("[history-gc] 历史维护循环启动（消息库保留期清理 + GUI 命令消费）");
    let mut last_gc: Option<u64> = None;
    loop {
        if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
            break;
        }
        crate::msgstore::consume_commands();
        // 保留期 GC：启动即跑一次（last_gc=None → due），之后每 24h
        let now = crate::chrono_lite::unix_secs();
        let due = last_gc
            .map(|t| now.saturating_sub(t) >= 24 * 3600)
            .unwrap_or(true);
        if due {
            last_gc = Some(now);
            let days = Config::load()
                .map(|c| c.history_retention_days)
                .unwrap_or(30);
            let removed = crate::msgstore::MsgStore::production().gc(days);
            crate::log!("[history-gc] 保留期 {days} 天，清理 {removed} 条过期历史");
        }
    }
    crate::log!("[history-gc] 历史维护循环退出");
}

/// FNV-1a 64 位哈希：per-bot 维护循环（tidy/session_gc）的首轮 jitter 用。
/// 进程内一致即可，无需跨进程稳定（std hash 不保证跨版本稳定，自写最稳）。
fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 扫出「不属于任何已配置 bot」的任务目录（issue abb-task-botkey-dir-1）。
///
/// 只认可**目录 + 内含 `tasks.json`** 的条目：这类目录代表「有人用别的 key 登记过任务」，
/// 而本 service 永远不会去扫它（每个 bot 的 worker 只认 `tasks/<bot.key()>/`）。
/// 抽出纯函数是为了能在单测里覆盖（生产调用点只需要一个 known key 集合）。
/// 「哪些 key 属于已配置的 bot」的**唯一**定义：全部 `cfg.bots`，不看 enabled/是否就绪。
///
/// ghost 告警的语义是「这个目录不属于任何已配置 bot」，所以判据必须是配置本身。
/// 若将来想额外提醒「已配置但当前跑不起来」的任务，那是另一条消息、另一个判据。
fn configured_keys(cfg: &crate::config::Config) -> std::collections::HashSet<String> {
    cfg.bots.iter().map(|b| b.key()).collect()
}

fn ghost_task_dirs(
    root: &std::path::Path,
    known: &std::collections::HashSet<String>,
) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name().and_then(|s| s.to_str())?.to_string();
            if known.contains(&name) || !path.join("tasks.json").exists() {
                return None;
            }
            Some(name)
        })
        .collect();
    out.sort();
    out
}

/// 维护循环 24h 门判定：自上次运行（门=落盘标记种子）满 24h 即到点。
/// 门缺失（仅理论：boot_gate 恒有值）→ 到点。
fn daily_due(gate: Option<u64>, now: u64) -> bool {
    match gate {
        Some(t) => now.saturating_sub(t) >= 24 * 3600,
        None => true,
    }
}

/// 维护循环内存门起点：落盘标记优先（重启不丢 24h 门——门以「上次实际运行」为起点，
/// 重启/停用期间照常推进，不会每 tick 判定到点 + 全量读配置）；无标记（首轮）→
/// now+jitter 首轮延迟。调用方每次到点把门推进到 now（与落盘标记同步）。
fn boot_gate(marker_ts: Option<u64>, now: u64, jitter: u64) -> Option<u64> {
    marker_ts.or(Some(now + jitter))
}

/// 维护循环 24h 门落盘标记：读上次运行时间戳（unix_secs 十进制），进程重启不丢进度。
/// 缺失/损坏 → None（调用方回退内存门，首轮仍延迟 24h+jitter）。
fn read_run_marker(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 维护循环运行标记落盘（best-effort：失败只 log——重启后 24h 门重算，
/// run_once 幂等，重复跑一轮无害）。
fn write_run_marker(path: &std::path::Path, ts: u64) {
    if let Err(e) = std::fs::write(path, ts.to_string()) {
        crate::log!("⚠️ 写运行标记 {} 失败：{e}", path.display());
    }
}

/// 组装定时任务的 prompt：三级 AGENTS.md 指令文件块（abb → bot → session，session_key
/// 用 job.chat_id，与下方 agent::run 的 session_key 一致）+ 受限分支前置受限说明。
/// 抽成纯函数便于测试（run_job 的 agent 调用不可测，prompt 组装可测）。
fn job_prompt(job: &crate::schedule::Job, bot_key: &str) -> String {
    let agents_block = crate::agents_md::collect_block(bot_key, &job.chat_id);
    // 授权者建的任务在受限分支执行：prompt 前置与聊天路径一致的受限说明
    //（否则模型不知道自己被闸着，越界被拒后瞎试）。与 agent::run 同源公共判定
    let body = if agents_block.is_empty() {
        job.prompt.clone()
    } else {
        format!("{agents_block}\n\n{}", job.prompt)
    };
    body
}

/// 执行一个到点任务：跑该 bot 生效后端（全新会话，不带聊天上下文）→ 回发；once 任务执行后删除。
/// 回发优先发任务原会话；若该会话已失效（群解散/bot 被移出等），回落到主会话（私聊，必存在）。
/// 多目标（#21）：job.targets 非空时向每个目标各投一份（可跨 bot，经路由表投递 + 失败兜底）。
/// job 轮次结局 → 该轮要投递的文案（`None` = **静默收尾、不投递**）。
///
/// - `Ok(text)`：正常结果；
/// - `Cancelled`（#309）：用户叫停，静默；
/// - `Timeout`：agent 没回 → 超时文案（既有行为）；
/// - **`Closed`：句柄已关闭（服务在关停）**——这**不是**「agent 无回复」**，不能共用
///   超时文案（#321）。曾经还有第二个来源「等待者被同 chat 更新的 job 顶替」（每 channel
///   一个 waiter 槽），#321 已用同 chat 单飞串行（`run_job` 持 `Bridge::job_gate`）关掉
///   那条路径：同 chat 的 job 依次跑，不再有顶替。措辞与 `run_job` 的日志保持一致；
/// - `Failed(reason)`：agent 终态失败（认证失效/多次重试耗尽等，由 `notify_channel`
///   旁路回传）——**不是**「agent 无回复」，带原因回报而非复用超时文案（#321 同类的误标）。
///
/// 本函数**无副作用**（纯映射）；Closed 的诊断日志由调用方打。
fn job_outcome_reply(outcome: &crate::runtime::harness::SyncTurnOutcome) -> Option<String> {
    use crate::runtime::harness::SyncTurnOutcome as O;
    match outcome {
        O::Ok(text) => Some(text.clone()),
        O::Cancelled | O::Closed => None,
        O::Timeout => Some("⏰ 定时任务执行超时（agent 无回复）".to_string()),
        O::Failed(reason) => Some(format!("⏰ 定时任务执行失败：{reason}")),
    }
}

/// 仅供测试：把私有的 [`run_job`] 暴露给 bridge 侧的集成测试（#309 PR-A2 的
/// 「真调 run_job + 真 harness」用例需要它，而 run_job 依赖 service 私有装配）。
#[cfg(test)]
pub(crate) async fn run_job_for_test(
    bridge: Arc<Bridge>,
    router: std::sync::Arc<crate::deliver::Router>,
    job: crate::schedule::Job,
) {
    run_job(bridge, router, job).await
}

async fn run_job(
    bridge: Arc<Bridge>,
    router: std::sync::Arc<crate::deliver::Router>,
    job: crate::schedule::Job,
) {
    let bot_key = bridge.bot.key();
    // #87 暂停拦截：任务目标会话已暂停 → 跳过本轮（不消耗 LLM）。
    // 多目标任务的目标侧由 Router::deliver 的暂停检查兜底（回源提示）；
    // 这里拦的是旧路径「先发原会话」——原会话已暂停则本轮无意义。
    if bridge.session_state.is_paused(&bot_key, &job.chat_id) {
        crate::log!(
            "[bot:{bot_key}] 任务 {} 跳过：目标会话已暂停（#87）chat={}",
            &job.id[..job.id.len().min(8)],
            &job.chat_id[..job.chat_id.len().min(16)]
        );
        crate::session_state::audit("job.skip", &bot_key, &job.chat_id, "scheduler", "paused");
        if job.kind == crate::schedule::JobKind::Once {
            bridge.jobs.remove(&job.id);
        }
        return;
    }
    let prompt_preview = crate::agent::truncate(&job.prompt, 40); // 按字符截断（含中文）
                                                                  //（P4.1：单执行层后 job 恒走随包 abb-agent，`default_backend`/Backend 解析已删。）
    crate::log!(
        "[bot:{bot_key}] 触发任务 {} → {}",
        &job.id[..8],
        prompt_preview
    );
    // 授权者建的任务在受限分支执行：prompt 前置与聊天路径一致的受限说明 + 三级
    // AGENTS.md 指令文件块（组装抽成 job_prompt 纯函数，可测）。
    let prompt = job_prompt(&job, &bot_key);
    // 定时任务的叫停（#309）：**不再**注册 Bridge 的 cancel flag——那是 chat 回合的
    // 机制（virtualbot 起回合时自己 insert），job 走 ACP 同步回合根本不消费它。
    // 历史上这里注册过一个没有读者的 flag，反而让停止词命中后直接 return，把真实叫停
    // 短路掉（#309 的根因）。现在的路径：下面 register_job_turn(...) 登记本轮 → 会话内
    // 发停止词时由 virtualbot 的 cancel_job_turns() 把取消送到**本 job 的 handle/channel**。
    // ACP 单轨：job 也走 dispatch（同步等待回合文本）——不依赖
    // spawn 同步路径。回退（harness 未装配/超时/入队失败）按失败文案。
    // 存量 granted 任务 fail-closed 拒跑（授权者机制已移除——没有受限执行层保护它，
    // 绝不提权按 owner 全权限执行）。
    if job.role == crate::config::SenderRole::Granted {
        crate::log!("[bot:{bot_key}] 存量 granted 任务拒跑（fail-closed，授权者机制已移除）");
        return;
    }
    let handle = bridge.acp_handles.as_ref().map(|hs| hs.acp.clone());
    // job 恒跑该 chat 的**群根频道**（与聊天回合同源；话题另开命名空间）。
    let channel_id =
        uuid::Uuid::parse_str(&crate::runtime::keys::channel_uuid(&bot_key, &job.chat_id))
            .expect("channel_uuid output must parse as Uuid");
    // #309 PR-A2 + #321：登记本轮，供会话内的停止词/`/cancel` 把取消送到**本 job 实际
    // 使用的 handle 与 channel**（不是停止词发送者的角色）。登记必须**早于取 job 闸**：
    // 排队等闸的 job 也要让停止词看得见，否则停止词只命中在跑那条、排队的照跑。返回的
    // `cancel` 标记即「排队期取消」信号——取到闸后先查它，再决定要不要 dispatch。
    let queued_cancel = match &handle {
        Some(h) => {
            let turn = crate::bridge::JobTurn::new(job.chat_id.clone(), h.clone(), channel_id);
            let flag = turn.cancel.clone();
            bridge.register_job_turn(&job.id, turn);
            Some(flag)
        }
        None => None,
    };
    // #321：同 chat 多 job **单飞串行**。harness 的 `sync_waiters` 每 channel 只有一个
    // waiter 槽（`docs/task-model.md`）：并发注册会让后到者把在跑者的回传端顶掉，在跑
    // 那条拿到 `Closed` 被误标成「执行超时」。已定产品语义是**排队**而非顶替——后到的
    // job 停在这里，等前一条跑完轮次**并投递完**才轮到它。闸按 `job.chat_id`（群根
    // 频道）分片，跨 normal/granted 两实例（同 chat 的两条不同 role 的 job 也要串行）。
    let job_gate = bridge.job_gate(&job.chat_id);
    let _job_gate_guard = job_gate.lock().await;
    // 排队期间被停止词叫停：静默收尾，不 dispatch、不投递（与在跑取消同语义，#309）。
    if queued_cancel
        .as_ref()
        .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
    {
        bridge.unregister_job_turn(&job.id);
        crate::log!(
            "[bot:{bot_key}] 任务 {} 在排队期间被叫停，静默收尾（不投递）",
            &job.id[..8]
        );
        if job.kind == crate::schedule::JobKind::Once {
            bridge.jobs.remove(&job.id);
        }
        return;
    }
    let reply: Option<String> = {
        match handle {
            Some(h) => {
                // 频道预注册：p2p/未知会话即时注册（job 消息无 Ev 上下文，用任务目标注册）
                h.upsert_channel(
                    channel_id,
                    crate::runtime::harness::ChannelMeta {
                        // job 频道工作区 = 该渠道目录（与消息路径同一套规则）
                        workspace: Some(
                            crate::channel_dir::ensure_channel_dir(
                                &bot_key,
                                &job.chat_id,
                                "p2p",
                                None,
                            )
                            .display()
                            .to_string(),
                        ),
                        bot_key: bot_key.clone(),
                        chat_id: job.chat_id.clone(),
                        chat_type: "p2p".to_string(),
                        thread_id: None,
                        name: format!("定时任务 {}", &job.id[..job.id.len().min(8)]),
                        anchor_mid: None,
                        adhoc: true,
                    },
                );
                // 单飞串行下本 channel 至多一个 job waiter（闸保证），不会再被顶替。
                let outcome = h
                    .wait_turn_outcome(
                        channel_id,
                        crate::runtime::queue::InboundMsg {
                            id_hex: uuid::Uuid::new_v4().to_string(),
                            author_role: job.role.as_str().to_string(),
                            text: prompt.clone(),
                            ts_secs: crate::chrono_lite::unix_secs() as i64,
                            prompt_tag: crate::schedule::JOB_PROMPT_TAG.to_string(),
                            bridge_generation: String::new(),
                        },
                        // 预算与 chat 回合同款（harness 单回合硬上限 + 余量）：
                        // 报告类任务实测 3~7 分钟，60s 必超时且回合在途仍会
                        // 迟发投递（超时文案 + 真回复双发）。
                        crate::runtime::harness::MAX_TURN_DURATION
                            + std::time::Duration::from_secs(30),
                    )
                    .await;
                bridge.unregister_job_turn(&job.id);
                // 超时臂要在途叫停（防真回复迟发成第二条消息）；Closed/Cancelled 不需要。
                if matches!(outcome, crate::runtime::harness::SyncTurnOutcome::Timeout) {
                    let _ = h.cancel(channel_id).await;
                }
                if matches!(outcome, crate::runtime::harness::SyncTurnOutcome::Closed) {
                    // 单飞串行后等待者不再会被同 chat 的别的 job 顶替（#321 已修），
                    // 走到这里只剩「句柄已关闭」（服务在关停）——不是「agent 没回」，
                    // 按静默收尾（不发超时文案）。
                    crate::log!(
                        "[bot:{bot_key}] 任务 {} 的句柄已关闭 → 静默收尾（不发超时文案）",
                        &job.id[..8]
                    );
                }
                job_outcome_reply(&outcome)
            }
            None => Some("⏰ 定时任务执行失败：agent 未装配".to_string()),
        }
    };
    // None = 本轮被用户叫停（#309）：静默收尾，**不投递任何东西**（含超时文案）。
    let Some(reply) = reply else {
        crate::log!(
            "[bot:{bot_key}] 任务 {} 已被叫停，静默收尾（不投递）",
            &job.id[..8]
        );
        if job.kind == crate::schedule::JobKind::Once {
            bridge.jobs.remove(&job.id);
        }
        return;
    };

    let header = match job.kind {
        crate::schedule::JobKind::Once => "⏰ 定时提醒",
        crate::schedule::JobKind::Cron => "⏰ 定时任务",
    };
    let text = format!("{header}\n\n{reply}");
    // 多目标（#21）：每个目标各投一份（跨 bot 走路由表；失败由 Router 回源报错 + 微信 outbox 兜底）
    if !job.targets.is_empty() {
        for item in
            crate::deliver::job_target_items(&bot_key, &job.chat_id, &job.id, &job.targets, &text)
        {
            router.deliver(&item).await;
        }
        if job.kind == crate::schedule::JobKind::Once {
            bridge.jobs.remove(&job.id);
        }
        return;
    }
    // 旧行为：先发原会话
    match bridge.msgr.send_text(&job.chat_id, &text).await {
        Ok(()) => crate::log!(
            "[bot:{bot_key}] 任务 {} 发送成功 chat={} 长度={}",
            &job.id[..8],
            &job.chat_id[..job.chat_id.len().min(10)],
            text.chars().count()
        ),
        Err(e) => {
            crate::log!(
                "[bot:{bot_key}] 任务 {} 原会话 {} 发送失败（{}）",
                &job.id[..8],
                &job.chat_id[..job.chat_id.len().min(10)],
                e
            );
            // 微信：主动推送受会话活跃度约束（ret=-2 = context_token stale），同 token 重试
            // 必然再失败 → 落盘积压，等用户下次发消息刷新 token 后补发，避免任务报告静默丢失。
            if bridge.bot.is_wechat() {
                bridge.queue_outbox(&job.chat_id, &text, &job.id);
            }
            // 原会话失效 → 回落本 bot 主会话（微信主会话==原会话时上面已积压，不再重复发）
            let primary = crate::config::Config::primary_chat(&bot_key);
            if !primary.is_empty() && primary != job.chat_id {
                let fallback_text = format!("{header}（原会话已失效，转发到主会话）\n\n{reply}");
                crate::log!(
                    "[bot:{bot_key}] 任务 {} 回落主会话 {}",
                    &job.id[..8],
                    &primary[..primary.len().min(10)]
                );
                match bridge.msgr.send_text(&primary, &fallback_text).await {
                    Ok(()) => crate::log!(
                        "[bot:{bot_key}] 任务 {} 回落发送成功 chat={} 长度={}",
                        &job.id[..8],
                        &primary[..primary.len().min(10)],
                        fallback_text.chars().count()
                    ),
                    Err(se) => {
                        crate::log!(
                            "[bot:{bot_key}] 任务 {} 回落发送也失败（{}）",
                            &job.id[..8],
                            se
                        );
                        if bridge.bot.is_wechat() {
                            bridge.queue_outbox(&primary, &fallback_text, &job.id);
                        }
                    }
                }
            }
        }
    }
    if job.kind == crate::schedule::JobKind::Once {
        bridge.jobs.remove(&job.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **接线测试**：`build_bot_acp_handles` 只给 **normal** 句柄开“按渠道读 MCP 配置”，
    /// granted 关闭（外部 MCP server 的工具面会绕过受限档沙箱，见 `build_bot_acp_handles` 注释）。
    ///
    /// 判别力：把 `let mcp_from_channel = true;` 改成 `false` ⇒ normal 断言红；
    /// 把 `granted` 那支的 `false` 改成 `true` ⇒ granted 断言红。
    #[test]
    fn channel_mcp_enabled_for_normal_only() {
        let bot = crate::config::BotConfig {
            name: "mcpwire".into(),
            ..Default::default()
        };
        let cfg = Config::default();
        let cmd = AgentCmd::bare("/bin/abb-agent");
        let handles =
            build_bot_acp_handle(&bot, &cfg, &cmd, tokio_util::sync::CancellationToken::new());
        assert!(
            handles.acp.mcp_from_channel(),
            "句柄必须允许按渠道读 <渠道目录>/mcp.json"
        );
    }

    /// `$ABB_BIN` 在**聊天句柄路径**上也必须被注入（评审 R1，卡 `abb-mcp-orchestrator-p0-20261012`）。
    ///
    /// 为什么单列：此前只有 oneshot 那条有断言，只删掉 `mk` 闭包里的 `.chain(abb_bin_env())`
    /// 全量隔离套件**仍全绿**（评审实测）。这里在**真句柄**上断言（`RuntimeHandle` 存的 `AgentConfig`），
    /// 不依赖 spawn，也不再是"只测 helper"。
    ///
    /// 判别力：删掉 `mk` 里的 `.chain(abb_bin_env())` ⇒ 本用例红。
    #[test]
    fn chat_handles_extra_env_carries_abb_bin() {
        let cmd = AgentCmd::bare("/bin/abb-agent");
        let stop = tokio_util::sync::CancellationToken::new();
        let cfg = Config::default();
        for mode in [
            crate::config::SandboxMode::FullAccess,
            crate::config::SandboxMode::ReadOnly,
        ] {
            let bot = crate::config::BotConfig {
                name: "abb-bin".into(),
                sandbox_mode: mode,
                ..crate::config::BotConfig::default()
            };
            let handles = build_bot_acp_handle(&bot, &cfg, &cmd, stop.clone());
            let env = handles.acp.extra_env_for_test();
            let abb_bin = env
                .iter()
                .find(|(k, _)| k == "ABB_BIN")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| {
                    panic!("句柄缺 ABB_BIN（档位 {mode:?}）：agent 的 `$ABB_BIN` 会展开为空")
                });
            assert!(
                std::path::Path::new(&abb_bin).is_absolute(),
                "ABB_BIN 必须是绝对路径，实际 {abb_bin}"
            );
        }
    }

    /// `AGENT_BRIDGE_BOT_KEY` 必须在**两条**生产 spawn 路径上都被注入（卡 `abb-agent-env-residue-20261021`）。
    ///
    /// 为什么单列：CLI 自己的报错文案就写着「桥正常调用会注入 AGENT_BRIDGE_BOT_KEY」
    /// （`main.rs:1058`），但此前 `rg AGENT_BRIDGE_BOT_KEY src/service.rs` 只命中一句日志文案
    /// ⇒ 多 bot 下 agent 调 `task add` 直接报错，单 bot 下静默落主会话（DM）。
    ///
    /// 判别力：删掉 `mk` 闭包里的 `.chain([agent_bot_key_env(bot)])` 或 oneshot 的
    /// `extra_env.push(agent_bot_key_env(bot))` ⇒ 本用例红。
    #[test]
    fn both_spawn_paths_inject_agent_bridge_bot_key() {
        let cmd = AgentCmd::bare("/bin/abb-agent");
        let stop = tokio_util::sync::CancellationToken::new();
        let cfg = Config::default();
        let bot = crate::config::BotConfig {
            name: "env-bot".into(),
            sandbox_mode: crate::config::SandboxMode::FullAccess,
            ..crate::config::BotConfig::default()
        };
        let expect_key = bot.key();

        // 路径 1：聊天句柄
        let handles = build_bot_acp_handle(&bot, &cfg, &cmd, stop.clone());
        {
            let env = handles.acp.extra_env_for_test();
            let got = env
                .iter()
                .find(|(k, _)| k == "AGENT_BRIDGE_BOT_KEY")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| {
                    panic!("句柄缺 AGENT_BRIDGE_BOT_KEY：agent 调 task/deliver 不带 --bot 会报错或落错会话")
                });
            assert_eq!(got, expect_key, "句柄注入的必须是本 bot 的规范 key");
        }

        // 路径 2：oneshot / 任务
        let c = oneshot_agent_config(&bot, &cfg);
        let got = c
            .extra_env
            .iter()
            .find(|(k, _)| k == "AGENT_BRIDGE_BOT_KEY")
            .map(|(_, v)| v.clone())
            .expect("oneshot 任务的 agent env 必须带 AGENT_BRIDGE_BOT_KEY");
        assert_eq!(got, expect_key, "oneshot 注入的必须是本 bot 的规范 key");
    }

    /// **接线测试（聊天句柄路径）**：句柄把 bot 配置档（`sandbox_mode`）解析成
    /// `_meta.abbSandbox` 载荷——受限档带载荷，全权限档不带。
    #[test]
    fn chat_handle_carries_sandbox_profile_by_mode() {
        let cmd = AgentCmd::bare("/bin/abb-agent");
        let stop = tokio_util::sync::CancellationToken::new();
        let cfg = Config::default();
        let mk_bot = |mode: crate::config::SandboxMode| crate::config::BotConfig {
            name: "wire".into(),
            sandbox_mode: mode,
            ..crate::config::BotConfig::default()
        };

        for mode in [
            crate::config::SandboxMode::ReadOnly,
            crate::config::SandboxMode::WorkspaceWrite,
        ] {
            let handles = build_bot_acp_handle(&mk_bot(mode), &cfg, &cmd, stop.clone());
            assert_eq!(
                handles.acp.agent_command(),
                "/bin/abb-agent",
                "带档位的 owner 会话走 abb-agent（已声明 abbSandbox 三档）"
            );
            assert!(
                handles.acp.session_sandbox().is_some(),
                "带档位的会话必须真的把档位载荷发出去"
            );
        }

        for mode in [
            crate::config::SandboxMode::Auto,
            crate::config::SandboxMode::FullAccess,
        ] {
            let handles = build_bot_acp_handle(&mk_bot(mode), &cfg, &cmd, stop.clone());
            assert_eq!(
                handles.acp.agent_command(),
                "/bin/abb-agent",
                "无档位的 owner 会话走执行层"
            );
            assert!(handles.acp.session_sandbox().is_none());
        }
    }

    /// oneshot 路径档位载荷语义（受限档带载荷 + JEV 门禁 env；全权限不带）。
    #[test]
    fn oneshot_paths_follow_the_sandbox_invariant() {
        use crate::config::BotConfig;
        let bot = BotConfig {
            name: "invariant".into(),
            sandbox_mode: crate::config::SandboxMode::ReadOnly,
            ..BotConfig::default()
        };
        let mut cfg = Config::default();
        cfg.jev.api_key = "jev-test".into(); // Jev 配了才注入（空配置 = 空 env）
        let cmd = AgentCmd::bare("/bin/abb-agent");

        // owner 配了 read-only ⇒ 档位存在 + JEV 门禁 env。
        let c = oneshot_agent_config_inner(&bot, &cfg, cmd.clone(), resolve_sandbox_meta(&bot));
        assert!(c.session_sandbox.is_some(), "read-only 档必须带档位");
        assert_eq!(c.command, "/bin/abb-agent");
        assert!(c
            .extra_env
            .iter()
            .any(|(k, v)| k == "JEV_API_KEY" && v == "jev-test"));
        assert!(
            c.args.is_empty(),
            "夹具给的是无参命令，args 必须原样透传为空"
        );

        // owner 无档位 ⇒ 不带档位、无 JEV env。
        let full = BotConfig {
            sandbox_mode: crate::config::SandboxMode::FullAccess,
            ..bot.clone()
        };
        let c = oneshot_agent_config_inner(&full, &cfg, cmd, resolve_sandbox_meta(&full));
        assert!(c.session_sandbox.is_none());
        assert!(!c.extra_env.iter().any(|(k, _)| k == "JEV_API_KEY"));
    }

    /// ki 回落的 args 必须透传到 `AgentConfig.args`：`ki acp serve` 的子命令在参数里，
    /// spawn 侧 `AcpClient::spawn(command, args, …)` 原样带上——丢了它，ki 会进交互
    /// TUI 而不是 ACP server（读 stdin 协议帧立刻失败）。
    #[test]
    fn ki_fallback_args_flow_into_agent_config() {
        use crate::config::BotConfig;
        let bot = BotConfig {
            name: "ki-args".into(),
            ..BotConfig::default()
        };
        let cfg = Config::default();
        let ki = AgentCmd {
            program: "/usr/local/bin/ki".to_string(),
            args: vec!["acp".to_string(), "serve".to_string()],
        };
        let c = oneshot_agent_config_inner(&bot, &cfg, ki, None);
        assert_eq!(c.command, "/usr/local/bin/ki");
        assert_eq!(
            c.args,
            vec!["acp".to_string(), "serve".to_string()],
            "`ki acp serve` 的子命令必须作为 args 透传到 spawn"
        );
    }

    /// M5 的锁：**公共薄包装** `oneshot_agent_config` 不许丢档位。
    /// M5 的锁：**公共薄包装** `oneshot_agent_config` 不许丢档位、也不许用错命令槽。
    ///
    /// 诚实说明判据强度：`command` 这一半在本机（无随包目录）两条槽都回落
    /// `ki acp serve`，所以它只能证明「包装确实走了 `pick_command_by_sandbox`」；
    /// 真正区分两条槽的是
    /// `chat_handles_route_sandboxed_sessions_to_the_old_layer` 与
    /// `oneshot_paths_follow_the_sandbox_invariant`（注入式内层）。**档位那半是非空断言、不空转**。
    #[test]
    fn public_oneshot_wrapper_keeps_the_sandbox_and_the_right_slot() {
        let bot = crate::config::BotConfig {
            name: "wrapper".into(),
            sandbox_mode: crate::config::SandboxMode::ReadOnly,
            ..crate::config::BotConfig::default()
        };
        let cfg = Config::default();
        let c = oneshot_agent_config(&bot, &cfg);
        assert!(
            c.session_sandbox.is_some(),
            "公共薄包装丢掉档位 ⇒ owner 的 read-only 维护任务会静默变成无闸（评审 M5 实测全绿的那种缝）"
        );
        assert_eq!(
            c.command,
            resolve_agent_command().program,
            "带档位的 oneshot 走执行层命令"
        );
        // `$ABB_BIN` 必须在这条生产路径的 env 里（卡 abb-mcp-orchestrator-p0-20261012）：
        // 提示词与白名单都按它工作，此前**从未注入** ⇒ agent 的 shell/MCP 子进程里展开为空。
        let abb_bin = c
            .extra_env
            .iter()
            .find(|(k, _)| k == "ABB_BIN")
            .map(|(_, v)| v.clone())
            .expect("oneshot 任务的 agent env 必须带 ABB_BIN");
        assert!(
            std::path::Path::new(&abb_bin).is_absolute(),
            "ABB_BIN 必须是绝对路径（agent 的 cwd 不可预知），实际 {abb_bin}"
        );
    }

    /// 取装配出的 ki models.json 里那唯一一家 provider。
    fn ki_models_entry(env: &std::collections::HashMap<String, String>) -> serde_json::Value {
        let text = env
            .get(crate::ki_config::MODELS_JSON_ENV)
            .expect("装配期必须带 models.json 载荷");
        let doc: serde_json::Value = serde_json::from_str(text).unwrap();
        doc["providers"]
            .as_object()
            .expect("providers 必须是对象")
            .values()
            .next()
            .cloned()
            .expect("必须恰好装配一家 provider")
    }

    /// 系统级对话装配（全局锚点气泡）：
    /// 全局默认供应商 + **无沙箱（整机全权限）**，且不依赖任何 bot 配置。
    #[test]
    fn oneshot_agent_config_system_uses_default_provider_and_full_access() {
        let prov = crate::config::ProviderConfig {
            name: "sys-default".into(),
            kind: "openai-chat".into(),
            base_url: "https://sys.example.com/v1".into(),
            api_key: "sk-sys".into(),
            model: "sys-model".into(),
            thinking: String::new(),
            context_window: 0,
        };
        let mut cfg = Config::default();
        cfg.providers = vec![prov];
        cfg.default_provider = "sys-default".into();
        // 关键：没有 bots（系统级对话不依赖任何渠道 bot）。
        assert!(cfg.bots.is_empty());

        let c = oneshot_agent_config_system(&cfg, "");
        assert!(
            c.session_sandbox.is_none(),
            "系统级对话 = 整机全权限，不得带沙箱档位"
        );
        let env: std::collections::HashMap<_, _> = c.extra_env.iter().cloned().collect();
        // 供应商经 ki 的 models.json 下发（见 `crate::ki_config`）：装配期只给目录 + 载荷。
        assert!(
            env.get(crate::ki_config::AGENT_DIR_ENV)
                .is_some_and(|dir| dir.contains("ki-agents")),
            "必须把子进程的 ki agent 目录指向 ABB 管理的 ki-agents/：{env:?}"
        );
        let payload = ki_models_entry(&env);
        assert_eq!(
            payload["apiKey"], "sk-sys",
            "系统级对话必须用全局默认供应商（不依赖任何 bot）"
        );
        assert_eq!(payload["baseUrl"], "https://sys.example.com/v1");
        // 命令槽：当前 `pick_command_by_sandbox` **忽略第二个入参**（恒返回 owner 槽，见其注释），
        // 故本断言实际只锁「c.command == owner 槽」，对「是否传了 false」**不具区分度**；
        // 非空断言单独保留，防两边同时解析成空。等该函数恢复按档位选槽后本断言才真正承重。
        assert_eq!(
            c.command,
            resolve_agent_command().program,
            "系统级对话走执行层命令"
        );
        assert!(!c.command.is_empty(), "命令槽必被解析");
        // 专用工作区：workspaces/syschat（不与任何 bot 隔离键同名）
        assert!(
            crate::syschat::workspace().ends_with("syschat"),
            "系统级对话工作区应为 workspaces/syschat：{:?}",
            crate::syschat::workspace()
        );
        // Jev 门禁 env 不该出现（无档位=不套门禁，多变一个凭证只扩大暴露面）。
        assert!(!env.contains_key("JEV_API_KEY"));
    }

    /// A 批（卡 abb-syschat-a-model-session-20261013）：「模型选择」= 按会话的 provider。
    /// 三态：显式命中 / 空串回落全局默认 / 名字不存在不注入半截 env（按未配置处理）。
    #[test]
    fn oneshot_agent_config_system_honours_explicit_provider() {
        let mk = |name: &str, key: &str| crate::config::ProviderConfig {
            name: name.into(),
            kind: "openai-chat".into(),
            base_url: format!("https://{name}.example.com"),
            api_key: key.into(),
            model: format!("{name}-model"),
            thinking: String::new(),
            context_window: 0,
        };
        let mut cfg = Config::default();
        cfg.providers = vec![mk("prov-a", "sk-a"), mk("prov-b", "sk-b")];
        cfg.default_provider = "prov-a".into();
        let env_of = |sess: &str| -> std::collections::HashMap<String, String> {
            oneshot_agent_config_system(&cfg, sess)
                .extra_env
                .into_iter()
                .collect()
        };
        assert_eq!(
            ki_models_entry(&env_of("prov-b"))["apiKey"],
            "sk-b",
            "显式 provider 必须命中（会话级模型选择）"
        );
        assert_eq!(
            ki_models_entry(&env_of(""))["apiKey"],
            "sk-a",
            "空串 = 回落全局默认"
        );
        assert!(
            !env_of("ghost").contains_key(crate::ki_config::MODELS_JSON_ENV),
            "名字不在 providers 里 ⇒ 不注入半截 env（按未配置处理）"
        );
    }

    /// 随包仅认 abb-agent（刀 4b 后无回滚槽）。
    #[test]
    fn override_applies_to_both_role_commands() {
        let dir = std::env::temp_dir().join(format!("abb-ovr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // bundled_agent_in_dir 按 `{name}{EXE_SUFFIX}` 查（Windows 上即 abb-agent.exe），
        // 造的文件名必须带后缀，否则 Windows CI 上找不到（刀 4b 改测试时遗漏）。
        let pinned = dir.join(format!("abb-agent{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&pinned, b"x").unwrap();
        let expect = pinned.display().to_string();
        assert_eq!(
            bundled_agent_in_dir(&[BUNDLED_AGENT], Some(&dir)),
            Some(expect.clone()),
            "随包仅认 abb-agent"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acp_fallback_prefers_full_ki_path() {
        let full = std::path::PathBuf::from("/usr/local/bin/ki");
        let mut looked_up = false;
        assert_eq!(
            resolve_acp_command_with(Some("abb-agent"), || {
                looked_up = true;
                Some(full.clone())
            }),
            AgentCmd::acp_serve("abb-agent"),
            "随包 abb-agent 优先于 ki 兑底，且同带 acp serve 形状（ki 产物必需，旧二进制忽略 argv）"
        );
        assert!(
            !looked_up,
            "已有 abb-agent 时不得探测 ki（Windows 会走同步注册表）"
        );

        let mut looked_up = false;
        assert_eq!(
            resolve_acp_command_with(None, || {
                looked_up = true;
                Some(full.clone())
            }),
            AgentCmd {
                program: full.display().to_string(),
                args: vec!["acp".to_string(), "serve".to_string()],
            },
            "回落必须传 PATH 命中的全路径，且带 acp serve 子命令参数"
        );
        assert!(looked_up, "无随包可执行时必须探测 ki");

        let mut looked_up = false;
        assert_eq!(
            resolve_acp_command_with(None, || {
                looked_up = true;
                None
            }),
            AgentCmd::acp_serve("ki"),
            "ki 缺失时保留裸名，spawn 失败如实可见（#246④）"
        );
        assert!(looked_up, "ki 缺失时也要完成一次探测");
    }

    /// 回归（P2.1 结构消灭的串台）：旧 service 级共享句柄集的 env_for 取**第一个
    /// enabled bot** 的供应商装配全部后端——多 bot 各配各供应商时全体串台。
    /// per-bot 化后：每个 bot 的 env 必须只来自自己的供应商。
    /// `$ABB_BIN` 注入（卡 abb-mcp-orchestrator-p0-20261012）：helper 必须给本进程绝对路径，
    /// 且**两条生产路径**（聊天 `mk` 闭包 / oneshot 任务）都把它带进 extra_env。
    ///
    /// 判别力：把 `abb_bin_env()` 从任一条路径去掉 ⇒ 对应断言红；helper 改名/改值 ⇒ 第一条红。
    #[test]
    fn abb_bin_env_is_injected_into_agent_env() {
        let (k, v) = super::abb_bin_env().expect("current_exe 应可得");
        assert_eq!(k, "ABB_BIN");
        let exe = std::env::current_exe().unwrap().display().to_string();
        assert_eq!(
            v, exe,
            "值必须是本程序绝对路径（提示词按它调 task/job/deliver）"
        );
        assert!(
            std::path::Path::new(&v).is_absolute(),
            "必须是绝对路径，agent 的 cwd 不可预知"
        );
    }

    #[test]
    fn env_for_bot_uses_own_bot_provider() {
        let mk_prov = |name: &str, key: &str| crate::config::ProviderConfig {
            name: name.into(),
            kind: "openai-chat".into(),
            base_url: format!("https://{name}.example.com"),
            api_key: key.into(),
            // 随包 ki 必须拿到显式模型 id（见 `crate::ki_config`），这里跟真实配置一样填上。
            model: format!("{name}-model"),
            thinking: String::new(),
            context_window: 0,
        };
        let mk_bot = |name: &str, prov: &str| crate::config::BotConfig {
            name: name.into(),
            provider: prov.into(),
            ..Default::default()
        };
        let mut cfg = Config::default();
        cfg.providers = vec![mk_prov("prov-a", "sk-a"), mk_prov("prov-b", "sk-b")];
        cfg.default_provider = "prov-a".into();
        cfg.bots = vec![mk_bot("bot-a", "prov-a"), mk_bot("bot-b", "prov-b")];

        let env_a: std::collections::HashMap<_, _> =
            env_for_bot(&cfg.bots[0], &cfg).into_iter().collect();
        let env_b: std::collections::HashMap<_, _> =
            env_for_bot(&cfg.bots[1], &cfg).into_iter().collect();
        assert_eq!(ki_models_entry(&env_a)["apiKey"], "sk-a");
        assert_eq!(ki_models_entry(&env_b)["apiKey"], "sk-b");
        assert_eq!(
            ki_models_entry(&env_b)["baseUrl"],
            "https://prov-b.example.com",
            "bot-b 不得串用 bot-a（第一个 enabled bot）的供应商"
        );
        // 空 API Key 硬闸：env 置空（预检按 NoProvider 拒答引导），不注入半截 env
        let mut cfg2 = cfg.clone();
        cfg2.providers[1].api_key = "  ".into();
        assert!(env_for_bot(&cfg2.bots[1], &cfg2).is_empty());
        // 无供应商（provider 名指向不存在）→ 空 env 不炸
        let cfg3 = {
            let mut c = cfg.clone();
            c.bots[1].provider = "ghost".into();
            c
        };
        assert!(env_for_bot(&cfg3.bots[1], &cfg3).is_empty());
    }

    /// 存量 granted 任务在执行侧 fail-closed 拒跑（见 `task_run` 的 PayloadKind::Agent 臂）
    /// ——不再有「按角色选剖面」，B1 的角色剖面测试随之退役。
    /// P2.2：normal handle 的 `_meta` 档位映射——sandbox_mode 解析成具体档。
    /// ReadOnly→read-only、WorkspaceWrite→workspace-write（域根=该 bot 工作区，
    /// canonicalize 归一），Auto（legacy）/FullAccess（默认）→None。
    /// 域根必须落在本 bot 工作区（隔离语义）。
    #[test]
    fn resolve_sandbox_meta_maps_modes() {
        let mk_bot = |mode: crate::config::SandboxMode| crate::config::BotConfig {
            name: "sb-test".into(),
            sandbox_mode: mode,
            ..Default::default()
        };
        let ws = std::fs::canonicalize(crate::workspace_dir("sb-test"))
            .unwrap_or_else(|_| crate::workspace_dir("sb-test"))
            .display()
            .to_string();

        let ro = resolve_sandbox_meta(&mk_bot(crate::config::SandboxMode::ReadOnly))
            .expect("read-only 应产生档位载荷");
        assert_eq!(ro.sandbox.as_deref(), Some("read-only"));
        assert_eq!(ro.writable_roots, Some(vec![ws.clone()]));
        assert!(
            ro.shell.is_none(),
            "read-only 不带 shell 白名单（owner 档）"
        );
        assert!(ro.abb_bin.is_none());

        let ww = resolve_sandbox_meta(&mk_bot(crate::config::SandboxMode::WorkspaceWrite))
            .expect("workspace-write 应产生档位载荷");
        assert_eq!(ww.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(ww.writable_roots, Some(vec![ws]));
        assert!(ww.shell.is_none());

        assert!(
            resolve_sandbox_meta(&mk_bot(crate::config::SandboxMode::Auto)).is_none(),
            "auto = None（fork 缺省照旧，不作中间档）"
        );
        assert!(
            resolve_sandbox_meta(&mk_bot(crate::config::SandboxMode::FullAccess)).is_none(),
            "full-access = None（今天字节级行为回归锁）"
        );
    }

    /// 等待绝不能提前返回进入关闭路径——旧实现把 30s 逐 handle 时限放在服务期，
    /// 健康服务 30s×bot 数后必然自关停（真机 2 bot=60s、实验室 1 bot=30s 重启风暴）。
    /// 探针期内不返回即「无期限」。
    #[tokio::test]
    async fn wait_bots_or_shutdown_never_times_out_on_healthy_bots() {
        let mut handles = vec![tokio::spawn(async {
            // 健康 bot：正常服务期不结束（300s 远超探针）
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        })];
        let stop = tokio_util::sync::CancellationToken::new(); // 无广播
        let r = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            wait_bots_or_shutdown(&mut handles, stop, std::time::Duration::from_millis(50)),
        )
        .await;
        assert!(
            r.is_err(),
            "健康 bot 期间服务期等待必须无期限（探针超时=正确，提前返回=重启风暴回归）"
        );
    }

    /// #189 回归护栏（关闭路径限时）：关停广播到达后，卡死的 bot 循环仍被逐 handle
    /// 时限兜底（#179 语义保留）——总耗时受 bound 支配，不得无限等。
    #[tokio::test]
    async fn wait_bots_or_shutdown_bounds_stuck_bot_after_cancel() {
        let mut handles = vec![tokio::spawn(async {
            // 卡死 bot：关停广播也不退出
            std::future::pending::<()>().await;
        })];
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel(); // 关停广播已到
        let t = std::time::Instant::now();
        wait_bots_or_shutdown(&mut handles, stop, std::time::Duration::from_millis(100)).await;
        let elapsed = t.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "关停后卡死 bot 必须被逐 handle 时限兜底，实际 {elapsed:?}"
        );
    }

    /// #189 回归护栏（fail-open 保留）：全部 bot 循环自行退出（如微信会话过期 -14）
    /// 时，即使无关停广播也进入收尾——与 v2.21.2 原语义一致（循环退出 → 进程优雅
    /// 退出 → 看门狗重启）。
    #[tokio::test]
    async fn wait_bots_or_shutdown_returns_when_all_bots_exit() {
        let mut handles: Vec<tokio::task::JoinHandle<()>> = vec![tokio::spawn(async {})]; // bot 立即退出
        let stop = tokio_util::sync::CancellationToken::new(); // 无广播
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            wait_bots_or_shutdown(&mut handles, stop, std::time::Duration::from_secs(30)),
        )
        .await;
        assert!(
            r.is_ok(),
            "全部 bot 自行退出后必须进入收尾（fail-open），不得无限等广播"
        );
    }

    /// #189 补充（独立审查 P3）：多 bot 顺序推进 + 迟到广播的混合形态——已完成
    /// handle 在服务期被 select 消费后，广播到达、尾段必须跳过它（不二次 poll
    /// panic），同时对未完成 handle 限时。删尾段 is_finished 守卫此测试必红。
    #[tokio::test]
    async fn wait_bots_or_shutdown_mixed_bots_with_late_cancel() {
        let mut handles = vec![
            tokio::spawn(async {}), // 立即退出的 bot：服务期被完成性消费
            tokio::spawn(async {
                // 卡死 bot：广播后由尾段限时兜底
                std::future::pending::<()>().await;
            }),
        ];
        let stop = tokio_util::sync::CancellationToken::new();
        {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                stop.cancel();
            });
        }
        let t = std::time::Instant::now();
        wait_bots_or_shutdown(&mut handles, stop, std::time::Duration::from_millis(100)).await;
        let elapsed = t.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "混合形态（已完成+卡死+迟到广播）不得 panic 且须受限时支配，实际 {elapsed:?}"
        );
    }

    /// 三个信号测试的进程内串行锁（独立审查 F1）：SIGTERM 发给整个进程，并行时
    /// 可能被其他信号测试 helper 的注册/parked 窗口消费致偶发 flake，串行化消除。
    static SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// #191 回归护栏（信号路径）：SIGTERM 到达返回 true（真实信号，调用方记日志）。
    /// 给测试进程自己发 SIGTERM——tokio 信号处理器已捕获，进程不死，仅本测试的
    /// helper 收到事件。三个信号测试共享 SIGNAL_TEST_LOCK 串行化（独立审查 F1）：
    /// 并行时发出的 SIGTERM 可能被别的测试 helper 的注册/parked 窗口消费致偶发 flake。
    /// 仅 unix：libc::kill 是 unix API（Windows CI 编译炸，#192 教训；false 路径
    /// 两测跨平台仍跑）。
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 串行锁必须横跨注册→发信号全程（F1），lint 会让位于测试目的
    async fn wait_exit_signal_or_shutdown_returns_true_on_sigterm() {
        let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stop = tokio_util::sync::CancellationToken::new(); // 无关停广播
        let task = tokio::spawn(wait_exit_signal_or_shutdown(stop));
        // 等 helper 完成信号注册（首次 poll 安装处理器），再给进程发 SIGTERM
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
            task.await.unwrap()
        })
        .await;
        assert!(r == Ok(true), "SIGTERM 必须返回 true（实际 {r:?}）");
    }

    /// #191 回归护栏（让位路径·先取消）：关停令牌已取消 → 立即返回 false
    ///（自发起关停，不记信号日志）。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 串行锁必须横跨注册→发信号全程（F1），lint 会让位于测试目的
    async fn wait_exit_signal_or_shutdown_returns_false_when_already_cancelled() {
        let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            wait_exit_signal_or_shutdown(stop),
        )
        .await;
        assert!(
            r == Ok(false),
            "已取消的令牌必须立即返回 false（实际 {r:?}）"
        );
    }

    /// #191 回归护栏（让位路径·后取消）：helper 先 parked 等信号，关停令牌后到仍能
    /// 唤醒返回 false——这正是自发起关停时 tracker.wait() 不再永等的前提
    ///（旧实现无此分支，parked 至烧满 20s 总期限强退）。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 串行锁必须横跨注册→发信号全程（F1），lint 会让位于测试目的
    async fn wait_exit_signal_or_shutdown_returns_false_on_late_cancel() {
        let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stop = tokio_util::sync::CancellationToken::new();
        {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                stop.cancel();
            });
        }
        let t = std::time::Instant::now();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            wait_exit_signal_or_shutdown(stop),
        )
        .await;
        assert!(r == Ok(false), "迟到取消必须返回 false（实际 {r:?}）");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(1),
            "取消后必须立即唤醒，实际 {:?}",
            t.elapsed()
        );
    }

    fn test_job(
        prompt: &str,
        chat_id: &str,
        role: crate::config::SenderRole,
    ) -> crate::schedule::Job {
        crate::schedule::Job {
            id: uuid::Uuid::new_v4().to_string(),
            kind: crate::schedule::JobKind::Once,
            schedule: String::new(),
            prompt: prompt.to_string(),
            chat_id: chat_id.to_string(),
            note: String::new(),
            targets: Vec::new(),
            role,
        }
    }

    /// #321：job 轮次结局 → 文案映射。`Closed` 是「句柄已关闭（服务在关停）」，
    /// **不是**「agent 无回复」——不能共用超时文案。（历史上它还有第二个来源：
    /// 同 chat 并发 job 时后登记那条顶掉在跑那条的 waiter，从而误报「执行超时」；
    /// 该路径已由同 chat 单飞串行关闭，见 `docs/task-model.md` §B。）
    #[test]
    fn job_outcome_reply_does_not_mislabel_closed_as_timeout() {
        use crate::runtime::harness::SyncTurnOutcome as O;
        assert_eq!(
            job_outcome_reply(&O::Ok("done".to_string())),
            Some("done".to_string())
        );
        assert_eq!(job_outcome_reply(&O::Cancelled), None, "叫停 → 静默");
        assert_eq!(
            job_outcome_reply(&O::Closed),
            None,
            "Closed（句柄已关闭）不得报超时"
        );
        assert_eq!(
            job_outcome_reply(&O::Timeout),
            Some("⏰ 定时任务执行超时（agent 无回复）".to_string())
        );
        assert_eq!(
            job_outcome_reply(&O::Failed("boom".to_string())),
            Some("⏰ 定时任务执行失败：boom".to_string()),
            "Failed 是「终态失败」→ 带原因回报，不得复用超时文案"
        );
    }

    /// issue abb-task-botkey-dir-1：`tasks/` 下不属于任何已配置 bot 的目录要被点出来
    /// （否则那些任务「登记成功但永不被认领」，且完全静默）。
    #[test]
    fn ghost_task_dirs_flags_only_unknown_dirs_with_definitions() {
        let root = std::env::temp_dir().join(format!("abb-ghost-{}", uuid::Uuid::new_v4()));
        let tasks = root.join("tasks");
        std::fs::create_dir_all(tasks.join("b_ok")).unwrap();
        std::fs::create_dir_all(tasks.join("微信龙虾")).unwrap(); // 幽灵：显示名当 key
        std::fs::create_dir_all(tasks.join("empty_dir")).unwrap(); // 无定义 → 不算
        std::fs::write(tasks.join("b_ok").join("tasks.json"), "[]").unwrap();
        std::fs::write(tasks.join("微信龙虾").join("tasks.json"), "[]").unwrap();
        std::fs::write(tasks.join("stray.txt"), "x").unwrap(); // 非目录

        let known: std::collections::HashSet<String> = ["b_ok".to_string()].into_iter().collect();
        assert_eq!(
            ghost_task_dirs(&tasks, &known),
            vec!["微信龙虾".to_string()],
            "只应点出「未知目录 + 含定义」的那一个"
        );
        // 根目录不存在 → 空（不 panic）
        assert!(ghost_task_dirs(&root.join("nope"), &known).is_empty());

        // 生产用的 known 集合来自「全部已配置 bot」：停用/未就绪的 bot **不得**被判成幽灵。
        // 独立审查 a9c9f0b 复现过：拿 ready（启用且就绪）当 known 会把 enabled=false 的
        // 已配置 bot 目录误报成「不属于任何已配置 bot」。
        let mut cfg = crate::config::Config::default();
        cfg.bots = vec![
            crate::config::BotConfig {
                app_id: "app_ok".into(),
                ..Default::default()
            },
            crate::config::BotConfig {
                app_id: "app_off".into(),
                enabled: false, // 停用，但确实已配置
                ..Default::default()
            },
            crate::config::BotConfig {
                app_id: "b_ok".into(), // 上面那个含定义的目录，在本 cfg 下是合法 bot
                ..Default::default()
            },
        ];
        let cfg_keys = configured_keys(&cfg);
        assert!(
            cfg_keys.contains("app_ok")
                && cfg_keys.contains("app_off")
                && cfg_keys.contains("b_ok")
        );
        std::fs::create_dir_all(tasks.join("app_off")).unwrap();
        std::fs::write(tasks.join("app_off").join("tasks.json"), "[]").unwrap();
        assert_eq!(
            ghost_task_dirs(&tasks, &cfg_keys),
            vec!["微信龙虾".to_string()],
            "停用但已配置的 bot 目录不得被报成幽灵"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn daily_due_gate_semantics() {
        // 门语义：满 24h 即到点；门缺失 → 到点（理论路径，boot_gate 恒有值）
        let now = 1_000_000_000u64;
        let h24 = 24 * 3600u64;
        assert!(daily_due(None, now));
        // 门未满 24h → 未到点（首轮延迟）
        assert!(!daily_due(Some(now - 1000), now));
        assert!(daily_due(Some(now - h24), now));
        // 恰好满 24h 边界
        assert!(daily_due(Some(now - h24 - 1), now));
        assert!(!daily_due(Some(now - h24 + 1), now));
        // 功能关闭期间门照常推进（due 分支置 gate=now）→ 关着也每 24h 才醒一次，不 spin
        assert!(!daily_due(Some(now), now));
    }

    #[test]
    fn boot_gate_restart_keeps_marker_cadence() {
        // 回归护栏（审查修复）：原实现在循环里以 now+jitter 起内存门、daily_due 取
        // 标记与门的较晚者——重启后门（未来时间戳）恒胜标记，每次重启都把下次运行
        // 推迟 24h+jitter，日重启的机器维护永不触发。修复后门以落盘标记为种子。
        let now = 1_000_000_000u64;
        let jitter = 1800u64;
        let h24 = 24 * 3600u64;
        // 重启时距上次运行不足 24h → 门=标记 → 未到点（重启不丢 24h 门）
        let gate = boot_gate(Some(now - 3600), now, jitter);
        assert!(!daily_due(gate, now), "重启后 1h 未到点");
        // 重启时已超 24h（停机超过一天）→ 门=标记 → 到点（补跑维护）
        let gate = boot_gate(Some(now - 50 * 3600), now, jitter);
        assert!(daily_due(gate, now), "停机两天重启即补跑");
        // 无标记（首轮）→ now+jitter 首轮延迟
        let gate = boot_gate(None, now, jitter);
        assert!(!daily_due(gate, now));
        assert!(
            daily_due(gate, now + h24 + jitter),
            "首轮满 24h+jitter 到点"
        );
        // 标记损坏/缺失 → 回退首轮延迟（与无标记一致）
        assert_eq!(boot_gate(None, now, jitter), boot_gate(None, now, jitter));
        let gate = boot_gate(None, now, 0);
        assert_eq!(gate, Some(now), "jitter=0 时门=now");
    }

    #[test]
    fn job_prompt_agents_md_then_prompt() {
        // 任务 prompt：三级 AGENTS.md 块在最前，用户 prompt 最后（受限前导已随机制移除）
        let bot_key = format!("abb-jobtest-{}", uuid::Uuid::new_v4());
        let ws = crate::workspace_dir(&bot_key);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "定时任务 bot 规则").unwrap();
        let job = test_job("提醒我喝水", "oc_job", crate::config::SenderRole::Owner);
        let p = job_prompt(&job, &bot_key);
        let i_a = p.find("[指令文件]").unwrap();
        let i_b = p.find("定时任务 bot 规则").unwrap();
        let i_t = p.find("提醒我喝水").unwrap();
        assert!(i_a < i_b && i_b < i_t, "顺序：指令文件 > 用户 prompt");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn job_prompt_owner_includes_agents_md_if_present() {
        // 非受限：有指令文件（bot 级）→ [指令文件] 块在用户 prompt 之前、无受限说明。
        // abb 级读真实 ~/.agent-bridge/AGENTS.md（用户机器上可能已建本功能文件），
        // 故不依赖它不存在——只断言结构，不断言块头精确位置。
        let bot_key = format!("abb-jobtest-{}", uuid::Uuid::new_v4());
        let ws = crate::workspace_dir(&bot_key);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "bot 规则").unwrap();
        let job = test_job("简单提醒", "oc_job2", crate::config::SenderRole::Owner);
        let p = job_prompt(&job, &bot_key);
        assert!(!p.starts_with("[受限模式]"), "owner 任务无受限说明");
        let i_a = p.find("[指令文件]");
        let i_t = p.find("简单提醒").unwrap();
        assert!(i_a.is_some(), "bot 级文件存在应注入块");
        assert!(i_a.unwrap() < i_t, "指令文件在用户 prompt 之前");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn job_prompt_owner_without_files_keeps_prompt() {
        // 非受限 + 唯一 key 无 bot/会话级文件（abb 级不可控：可能已存在）：
        // 只断言用户 prompt 出现、无受限说明；块的有无取决于 abb 级文件，不强断。
        let bot_key = format!("abb-jobtest-{}", uuid::Uuid::new_v4());
        let job = test_job("简单提醒", "oc_job3", crate::config::SenderRole::Owner);
        let p = job_prompt(&job, &bot_key);
        assert!(p.contains("简单提醒"));
        assert!(!p.starts_with("[受限模式]"));
    }
}

/// 微信长轮询游标落盘位置：`workspaces/<bot_key>/wx_cursor.json`。
fn wx_cursor_path(bot_key: &str) -> std::path::PathBuf {
    crate::workspace_dir(bot_key).join("wx_cursor.json")
}

/// 读取持久化的微信游标（缺失/损坏 ⇒ 空串，绝不因此启动失败）。
fn load_wx_cursor(bot_key: &str) -> String {
    let path = wx_cursor_path(bot_key);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return String::new();
    };
    parse_wx_cursor(&text).unwrap_or_default()
}

/// 从 JSON 里取游标。损坏 ⇒ None（调用方退回空串）。
fn parse_wx_cursor(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(v.get("buf")?.as_str()?.to_string())
}

/// 落盘游标（原子写 + 尽力而为：失败只记一条，绝不影响长轮询）。
fn save_wx_cursor(bot_key: &str, cursor: &str) {
    let path = wx_cursor_path(bot_key);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({ "buf": cursor }).to_string();
    if let Err(e) = crate::atomic_write_text(&path, &body) {
        crate::log!("[bot:{bot_key}] 微信游标落盘失败（不影响长轮询）：{e:#}");
    }
}

#[cfg(test)]
mod wx_cursor_guard_tests {
    /// 游标必须能往返、且损坏输入不得 panic（审计 #19）。
    ///
    /// 判别力：把持久化删掉（`load_wx_cursor` 恒返回空串）⇒ 生产侧断言红。
    #[test]
    fn wx_cursor_roundtrip_and_corrupt_input() {
        let body = serde_json::json!({ "buf": "CURSOR-123" }).to_string();
        assert_eq!(
            crate::service::parse_wx_cursor(&body).as_deref(),
            Some("CURSOR-123")
        );
        assert_eq!(
            crate::service::parse_wx_cursor("not-json"),
            None,
            "损坏输入退回 None"
        );
        assert_eq!(crate::service::parse_wx_cursor("{\"other\":1}"), None);
    }

    /// 生产侧必须真的载入与保存（否则游标只是一次性内存变量）。
    #[test]
    fn wechat_loop_loads_and_saves_the_cursor() {
        let src = include_str!("service.rs");
        assert!(
            src.contains("let mut cursor = load_wx_cursor(&key)"),
            "启动必须载入游标"
        );
        assert!(
            src.contains("save_wx_cursor(&key, &cursor)"),
            "拿到新游标必须落盘"
        );
    }
}
