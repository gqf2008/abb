//! 内置工具（`rpi-tools`）：`read` / `write` / `edit` / `bash` / `grep` / `find` / `ls`。
//!
//! 为什么需要：abb-agent 第一刀只有 MCP 工具（来自外部 MCP server），**没有文件读写与
//! shell**；被替代的 `旧执行层` 有 4 个内置 dev 工具（`shell`/`read`/`write`/`ls`，
//! 挂在 `dev` 伪命名空间下 ⇒ 模型看到的是 `dev__shell`）。本模块把 rpi-tools 的那套工具
//! 接上，并**沿用 `dev__` 限定名**，避免 owner 约定/技能里写过的工具名静默失效。
//!
//! ## 逐条对齐与被替代组件的差异（都写出来，别让读者猜）
//!
//! | 维度 | 被替代组件 | 本包 |
//! | --- | --- | --- |
//! | 集合 | `shell`/`read`/`write`/`ls`/`glob`/**`delegate`**（6 个） | 前五个**同名**（`glob` 由 rpi 的 `find` 工具提供——它的入参就是 glob 模式）+ `edit`/`grep`（rpi 增量）；**`delegate`（子代理委派）本包没有**，见下 |
//! | shell 工具 | `{command, timeout_secs}` | 同名前缀 `dev__shell`，schema 也把 `timeout` 改回 `timeout_secs` |
//! | read 的 `offset` | **0-based** | **1-based**（rpi 工具的语义；模型看得到 description，自洽但不通用） |
//! | 写入限定 | FullAccess 档也限定在会话 workspace（拒绝对路径与 `..` 逃逸） | **不放宽**：同样限定（见 [`confine_write_path`]） |
//! | 读的限定 | FullAccess 档允许绝对路径；受限档按 `read_roots` | 与 FullAccess 一致（**受限档的 roots 策略仍由 abb 的闸门承担**，本包不实现档位） |
//! | shell 超时 | 默认 120s、clamp 1..=600 | 同（`default_timeout=120`，模型给的 `timeout_secs` 也 clamp 进 1..=600） |
//! | shell 的环境 | 白名单（供应商凭据不进子进程） | 同：`inherit_env=false` + [`crate::child_env::passthrough_map`] |
//! | shell 截断后的全量输出 | — | rpi 会把全量输出写到临时文件（`full_output_path`）；登记，不改 |
//! | 开关 | `ABB_AGENT_DEV_TOOLS=0` 关闭 | 同一个环境变量、同一种读法（见 [`dev_tools_enabled_from`]） |
//! | Windows | shell 经 git-bash | 同（rpi 的 bash 工具在 Windows 也是 bash/POSIX，无 cmd 回退）；不额外暴露 `powershell` |
//!
//! **`delegate` 未实现**：参照物有一个把子任务委派给子代理的工具，本包没有对应实现。
//! 换执行层时这是**已知能力缺口**（不是等价替换），登记在 README，随需要再补。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{Schema, Tool};
use rpi_tools::{
    create_bash_tool, create_edit_tool, create_find_tool, create_grep_tool, create_ls_tool,
    create_read_tool, create_write_tool, BashToolOptions, ExecutionEnv, ExecutionToolContext,
    MutatingEnv, OsExecutionEnv, ReadToolOptions,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::provider::EnvSource;

/// 会话执行档位（`session/new` 的 `_meta.sandbox` 解析产物，统一迁移刀 3）。
///
/// 决定「哪些内置工具对模型可见」——与参照物 `参照物 wire.rs::Sandbox`
/// 同词表、同语义：
/// - `ReadOnly`：write/shell **不注入**（模型看不见；`allow_write`/`allow_shell` 为假）；
/// - `WorkspaceWrite`：全工具可用，写限工作区、读限工作区；
/// - `FullAccess`：读任意路径、写限工作区（写限是无条件的既有行为）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxMode {
    /// 宽松解析：未知/缺失回落 `FullAccess`（与参照物 `parse_opt` 同口径——协议噪音不挂会话）。
    /// `shell == restricted`（granted 承诺语义）与 workspace-write 同档位处理。
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "read-only" => Self::ReadOnly,
                "workspace-write" => Self::WorkspaceWrite,
                _ => Self::FullAccess,
            },
            None => Self::FullAccess,
        }
    }

    /// read-only 档不写。
    pub fn allow_write(self) -> bool {
        self != Self::ReadOnly
    }

    /// read-only 档不跑 shell。
    pub fn allow_shell(self) -> bool {
        self != Self::ReadOnly
    }

    /// 非 FullAccess 档读要限工作区（对齐参照物 `read_roots`）。
    pub fn restrict_read(self) -> bool {
        self != Self::FullAccess
    }
}

/// 进程级开关：`0` 关闭整套内置工具（与 fork 同款环境变量）。
pub const DEV_TOOLS_ENV: &str = "ABB_AGENT_DEV_TOOLS";

/// shell 的默认超时与 clamp 区间（与被替代组件一致：默认 120s、clamp 1..=600）。
const DEFAULT_SHELL_TIMEOUT_SECS: f64 = 120.0;
const MIN_SHELL_TIMEOUT_SECS: f64 = 1.0;
const MAX_SHELL_TIMEOUT_SECS: f64 = 600.0;

/// 限定名命名空间（与被替代组件一致：它把这些工具挂在一个 `dev` 伪服务器下）。
const DEV_NAMESPACE: &str = "dev";
const SEP: &str = "__";

/// `ABB_AGENT_DEV_TOOLS` 的读法：**逐条对齐 fork**（`parse_env(…, 1u8)? != 0`）。
///
/// 默认开；`0` 关；读不懂 ⇒ `Err`（调用方响亮失败，与 fork 的 `die()` 同款）。
/// 与 `NO_HINTS` 不同，这个开关不是安全闸（工具面宽窄由 abb 的档位闸门兜底），所以
/// 「读不懂就拒启」在这里主要是**与参照物一致**与「配置错误要可见」，而不是防泄漏。
pub fn dev_tools_enabled_from(value: Option<&str>) -> Result<bool, String> {
    let raw = value.unwrap_or("1");
    match raw.parse::<u8>() {
        Ok(0) => Ok(false),
        Ok(_) => Ok(true),
        Err(error) => Err(format!(
            "{DEV_TOOLS_ENV}={raw:?} 无法解析为 0..=255（{error}）——按被替代组件同款口径拒绝启动"
        )),
    }
}

/// 从环境源判定（可注入版本，单测不改进程全局 env）。
pub fn dev_tools_enabled(env: &dyn EnvSource) -> Result<bool, String> {
    dev_tools_enabled_from(env.get(DEV_TOOLS_ENV).as_deref())
}

/// 造好本会话的内置工具（已换成 `dev__{name}` 限定名）。
///
/// `workspace` 是会话工作区（`session/new` 的 `cwd`）：read/bash/grep/find/ls 的基准目录，
/// 也是 write/edit 的**写入边界**。
///
/// `mode`（统一迁移刀 3）：决定哪些工具对模型可见（与参照物档位过滤同语义）：
/// - `ReadOnly`：**不注入** `dev__shell` / `dev__write` / `dev__edit`（模型看不见），
///   read/ls/grep/find 限工作区（读域闸）；
/// - `WorkspaceWrite`：全工具可用，读写均限工作区；
/// - `FullAccess`：读任意路径（对齐参照物 `read_roots=None`），写仍限工作区（无条件）。
///
/// `shell_restricted`（granted 承诺语义）另成独立维度：不改变 shell/write/read 的可见性
/// （granted 会话能写工作区、有 shell——shell 的**内容**由 Jev 门禁判断），但**摘除
/// `delegate`**（对齐参照物 `delegate_def_visible` 的 `shell != Restricted` 腿：委派 CLI
/// 天生不受域闸约束，给了 = 白名单白做）。
///
/// `read_roots`（2026-10-12）：受限档的**附加只读根**（`_meta.readRoots`，如 bot 级
/// `attachments/`）。会话 cwd 变成渠道目录后，用户发给 bot 的附件在 cwd 之外——
/// 不给只读根的话受限会话就真读不到附件（能力回退）。只放宽**读**；写入仍限会话工作区。
pub fn dev_tools(
    workspace: &Path,
    mode: SandboxMode,
    shell_restricted: bool,
    read_roots: &[std::path::PathBuf],
) -> Vec<Arc<dyn AgentTool>> {
    let env: Arc<OsExecutionEnv> = Arc::new(OsExecutionEnv::with_cwd(workspace.to_path_buf()));
    let read_env: Arc<dyn ExecutionEnv> = env.clone();
    let mutate_env: Arc<dyn MutatingEnv> = env;
    let context = ExecutionToolContext::new(read_env, Some(mutate_env));

    // shell：默认超时与参照物对齐（120s；模型给的 timeout_secs 由 guard 再 clamp），并且
    // **不继承宿主环境**——abb 把供应商凭据注入 agent 进程，shell 不该看得到它们。
    let shell_options = BashToolOptions {
        command_prefix: None,
        default_timeout: Some(DEFAULT_SHELL_TIMEOUT_SECS),
        prepare: Some(Arc::new(|execution| {
            // 闭包体同步完成（只改两个字段），用 `ready` 的未来满足 `BashPrepare` 的形状。
            execution.inherit_env = false;
            execution.env = crate::child_env::passthrough_map();
            Box::pin(std::future::ready(Ok(())))
        })),
    };

    let read_guard = if mode.restrict_read() {
        Guard::ReadPath(read_roots.to_vec())
    } else {
        Guard::None
    };
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    if mode.allow_shell() {
        // `bash` 工具改成参照物的名字 `dev__shell`，schema 里把 `timeout` 改回 `timeout_secs`。
        tools.push(exposed(
            create_bash_tool(&context, Some(shell_options)),
            "shell",
            Guard::ShellTimeout,
            workspace,
        ));
    }
    // read 工具：读域限定（若受限）+ **图片预算**（所有会话）。预算必须先于工具构造取好，
    // 因为 `exposed` 把它搬进 `Guard`；`dev_tools` 每会话调一次 ⇒ 预算天然按会话隔离。
    let image_budget = ImageBudget::from_env(&crate::provider::ProcessEnv);
    let read_guard_for_read = match (mode.restrict_read(), image_budget.clone()) {
        (true, Some(budget)) => Guard::ReadWithBudget {
            roots: Some(read_roots.to_vec()),
            budget,
        },
        (false, Some(budget)) => Guard::ReadWithBudget {
            roots: None,
            budget,
        },
        // 预算被显式关掉（`ABB_AGENT_IMAGE_BUDGET_BYTES=0`）⇒ 退回原来那套。
        (true, None) => Guard::ReadPath(read_roots.to_vec()),
        (false, None) => Guard::None,
    };
    tools.push(exposed(
        create_read_tool(&context, Some(read_tool_options())),
        "read",
        read_guard_for_read,
        workspace,
    ));
    if mode.allow_write() {
        tools.push(exposed(
            create_write_tool(&context),
            "write",
            Guard::WritePath {
                deny_bridge_state: shell_restricted,
            },
            workspace,
        ));
        tools.push(exposed(
            create_edit_tool(&context),
            "edit",
            Guard::WritePath {
                deny_bridge_state: shell_restricted,
            },
            workspace,
        ));
    }
    tools.push(exposed(
        create_ls_tool(&context, None),
        "ls",
        read_guard.clone(),
        workspace,
    ));
    tools.push(exposed(
        create_grep_tool(&context, None),
        "grep",
        read_guard.clone(),
        workspace,
    ));
    // rpi 的 `find` 入参就是 glob 模式（`pattern` + `path` + `limit`）⇒ 暴露成参照物的
    // `dev__glob`，让写惯了 `dev__glob` 的约定/技能仍然有效。
    tools.push(exposed(
        create_find_tool(&context, None),
        "glob",
        read_guard.clone(),
        workspace,
    ));
    // 委派给本机 claude/codex CLI（参照物有、rpi 没有 ⇒ 本包自己实现，见 `delegate` 模块）。
    // **两个 CLI 都没装时不暴露**：与参照物的 `cli_available` 腿同取向——工具表里不该出现
    // 一个只会报错的工具（评审 F8）。read-only（无 shell）与 granted（shell=restricted）档
    // 也不暴露：对齐参照物 `delegate_def_visible` = `allow_shell && shell != Restricted &&
    // cli_available`（委派 CLI 天生不受域闸约束，给了 = 白名单白做）。
    let delegate_visible = mode.allow_shell()
        && !shell_restricted
        && crate::delegate::any_backend_available(&crate::provider::ProcessEnv);
    if delegate_visible {
        tools.push(exposed(
            Arc::new(crate::delegate::DelegateTool::new(workspace)),
            crate::delegate::DELEGATE_BARE_NAME,
            Guard::None,
            workspace,
        ));
    }
    tools
}

/// 工具执行前要做的那点额外事。
#[derive(Clone, PartialEq, Eq)]
enum Guard {
    /// 原样委托。
    None,
    /// 写类工具：把 `path` 限定到会话工作区内（见 [`confine_write_path`]）。
    ///
    /// `deny_bridge_state` = **受限（授权者）会话**专用的加码：拒写渠道「状态面」文件——
    /// 即那些**会被下一轮会话当成可信输入**读回去的路径（见 [`deny_bridge_state_path`] 的名单）。
    /// 为什么：这些面全是「受限会话写一次，owner 会话读一次」的提权链——
    /// ① `mcp.json` / `.mcp.json` 声明的是 abb-agent 要起的 MCP 子进程，子进程**不按会话档位沙箱运行**
    ///   （白名单 env 含 `HOME`/`$ABB_BIN`）⇒ 下一条 owner 消息重建会话即以用户身份无沙箱执行；
    /// ② `.agents/skills/**` 的技能 `name`/`description`/正文由 `hints.rs` 注入 owner 的系统提示
    ///   （`.goose/skills`、`.claude/skills` 同理）= 提示词注入；
    /// ③ `AGENTS.md` / `CLAUDE.md` 被 `ensure_workspace_guide` 以「不含 GUIDE_MARKER 才重写」判定，
    ///   而 marker 就印在 agent 提示词里 ⇒ 伪造 marker 就能让注入持久化；
    /// ④ `sessions.json` / `history/**`（虚拟 Bot 群会话里渠道目录即 vb 目录）= 会话槽位与历史注入面。
    /// owner 会话**不受此限**：这些本来就该由 owner 会话/人工维护。
    WritePath { deny_bridge_state: bool },
    /// 读类工具：把 `path` 限定到**会话工作区或附加只读根**内（见 [`confine_read_path`]）。
    /// 仅受限会话套用；owner 全权限用 `None`（对齐参照物 FullAccess 读任意路径）。
    /// 括号里是附加只读根（如 bot 级 `attachments/`）——它们是**读**域，仍不能写。
    ReadPath(Vec<PathBuf>),
    /// `read` 工具专用的**组合闸**：读域限定（同 [`Guard::ReadPath`]）+ 会话级**图片预算**（见 [`ImageBudget`]）。
    ///
    /// 为何不直接复用 `ReadPath` 再加一个变体：`Guard` 是**单个**字段，读工具的 `ls`/`grep`/`find`
    /// 共用同一个 `read_guard`，而预算只属于 `read`（后三者不出图）。合成一个变体比为它们各加一层更清楚。
    ///
    /// `roots = None` = 不限定读域（owner 全权限），但预算**仍生效**——要防的是所有会话
    /// 自己的图把请求体顶爆，与权限无关。
    ReadWithBudget {
        roots: Option<Vec<PathBuf>>,
        budget: Arc<ImageBudget>,
    },
    /// shell：把 schema 的 `timeout_secs` 翻回 rpi 的 `timeout`。
    ShellTimeout,
}

/// 把 rpi 的工具包一层：改限定名 + （按需）参数守卫。
struct Exposed {
    schema: Tool,
    inner: Arc<dyn AgentTool>,
    guard: Guard,
    workspace: PathBuf,
}

/// `read` 工具的选项：接上 rpi-tools 自带的 `InlineImageProcessor`。
///
/// 为什么（卡 `abb-agent-image-processor-wiring-20261012`）：rpi-tools 从 0.3.16 起就有
/// `ReadImageProcessor` seam，但**默认不注入实现**——那种默认下，`read` 对支持的图片格式
/// （PNG/JPEG/GIF/WebP）是**原字节 inline、不缩放**（`rpi-tools/src/tools/read.rs` 的 None 分支），
/// 只有 BMP 会给一行"已省略"提示。接上 0.3.21 的实现后，`read` 会把图片
/// **解码 → 按 EXIF 摆正 → 缩到 `INLINE_IMAGE_MAX_SIDE` → 需要时转码**（BMP/超大图 → PNG/JPEG）。
///
/// **别把"丢图"归到像素上限**（评审第 2 轮 F5、第 3 轮更正）：像素上限**不会**导致图片被丢——
/// 不装处理器时它是原样回来的。装上处理器后，`read` 侧**真实的**丢弃口径是 rpi 的
/// `tools/read.rs`「`process` 返 `Err` ⇒ 该图不进 content，改回一段纯文本错误」（**本卡接线才引入**这条路：
/// `image_processor: None` 时压根不调 `process`）。
///
/// 另注：本仓的 `TOOL_RESULT_TOTAL_BYTES = 8 MiB`（`src/mcp.rs`）只看 **MCP 工具结果**（`McpTool::execute`
/// → `tool_result_content`），**内置 `dev__read` 不经过它**——别把它写成 read 的预算（第 3 轮评审实测的机制错配）。
/// 接上处理器的收益是"超大图**缩放后仍能进模型**"，而不是"从丢掉变成不丢"。
///
/// `auto_resize_images: true` 是 rpi 的默认值，这里显式写出，表示我们**要**这个行为而不是碰巧继承它。
/// 注意边界：这只影响 **read 工具**里的图片；`Content::Image` 到了 anthropic 的**工具结果**序列化层
/// 仍会被换成占位文本（上游 TODO，见 README）。
fn read_tool_options() -> ReadToolOptions {
    ReadToolOptions {
        auto_resize_images: true,
        image_processor: image_processor_from(std::env::var(IMAGE_PROCESSOR_ENV).ok().as_deref()),
    }
}

/// 图片处理器开关的**环境变量名**（与 `ABB_AGENT_DEV_TOOLS` 同惯例：默认开、值 `0` 关）。
pub const IMAGE_PROCESSOR_ENV: &str = "ABB_AGENT_IMAGE_PROCESSOR";

/// 从环境值判定是否装处理器（**可注入版本**，单测不改进程全局 env）。
///
/// 语义（对齐 `dev_tools_enabled_from`）：
/// - 缺省（未设置）或任意非 `0` 值 ⇒ **装**（默认开：这是"读图别把大图丢掉"的能力，默认关会让用户
///   看到"图片被省略"却不知道为什么）；
/// - `"0"` ⇒ 不装（退回 rpi 默认：原样 inline 或省略提示）——给排障/回退留一条路；
/// - 读不懂（非数字）⇒ 也**装**（这里不是安全闸，容忍非法值比"悄悄关掉能力"更符合预期，且与
///   `dev_tools_enabled_from` 的"响亮失败"动机不同：那条是配置错误可见性，这条没有安全含义）。
pub fn image_processor_from(
    value: Option<&str>,
) -> Option<std::sync::Arc<dyn rpi_tools::ReadImageProcessor>> {
    let enabled = !matches!(value.map(str::trim), Some("0"));
    enabled.then(|| {
        std::sync::Arc::new(rpi_tools::image_processing::InlineImageProcessor)
            as std::sync::Arc<dyn rpi_tools::ReadImageProcessor>
    })
}

/// 会话级图片预算：控制 `dev__read` 往上下文里塞多少**图片 base64**。
///
/// 为什么需要（卡 `abb-413-image-budget-20261022`，为 `abb-413-body-limit-20261020` 治本）：
/// 上游网关的请求体上限约 48MB（issue 作者实测 `api.deepseek.com/v1`）。而 `dev__read`
/// 读图会把真 `Image` part 放进上下文，**每轮都随请求重发**，单张封顶
/// `INLINE_IMAGE_MAX_BASE64_BYTES`（9MB/2 = 4.5MB）——十几张作业图就能把请求体顶爆。
///
/// 为什么是「控制塞进去的量」而不是「塞进去再裁」：两条现成路都堵死了（本回合读 rpi 源码实测）——
/// - `rpi-agent` 的 `transform_context` 钩子（`hooks.rs:34/107`）在 `agent_loop.rs:447` 真会调，
///   但 `agent.rs:517` 硬编码 `transform_context: None`，而 `AgentBuilder` 的 12 个公开方法里
///   **没有 setter** ⇒ abb-agent 装不上；
/// - `Agent::state()` 注释明写「Clones tools/messages so callers can't mutate internal state」
///   ⇒ 运行期拿不到可变消息句柄，只能重建会话（那等于 `/new`，不是「预算内降级」）。
///
/// 所以唯一能真正拦住 413 的位置是**入口**：自己的 `dev__read` 不把超预算的图塞进上下文。
///
/// 口径与配置：预算与上限都用 **base64 字节**（与 `INLINE_IMAGE_MAX_BASE64_BYTES` 同单位）。
/// 默认 `IMAGE_BUDGET_DEFAULT_BYTES`（32MB，按 48MB 网关上限留余量）；
/// 环境变量 `ABB_AGENT_IMAGE_BUDGET_BYTES` 可覆盖（`0` = 不限，给排障留退路）。
struct ImageBudget {
    limit: usize,
    state: std::sync::Mutex<ImageBudgetState>,
}

#[derive(Default)]
struct ImageBudgetState {
    /// 已进上下文的 base64 字节总计。
    used: usize,
    /// 已进上下文的图片指纹（`DefaultHasher(base64)` + 长度）。
    /// 用 `(hash, len)` 而不是存整串：存整串会让内存翻倍，而预算本身就是 32MB 量级。
    /// 碰撞后果是「一张**不同**的图被当成重复而省略」，不静默损坏数据、且提示里看得见；
    /// 以 20 张图的规模计碰撞概率可忽略。
    seen: std::collections::HashSet<(u64, usize)>,
    /// 80% 水位是否已报过（只报一次，不刷屏）。
    warned: bool,
}

/// 默认图片预算（base64 字节）。32MB ≈ 网关 48MB 上限的三分之二，留出文本与工具结果的余量。
pub const IMAGE_BUDGET_DEFAULT_BYTES: usize = 32 * 1024 * 1024;

/// 图片预算的环境变量名（与 `ABB_AGENT_IMAGE_PROCESSOR` 同惯例：绝对字节数，`0` = 不限）。
pub const IMAGE_BUDGET_ENV: &str = "ABB_AGENT_IMAGE_BUDGET_BYTES";

/// 从环境值解析预算字节数（**可注入版本**，单测不改进程全局 env）。
///
/// - 缺省 ⇒ [`IMAGE_BUDGET_DEFAULT_BYTES`]；
/// - `"0"` ⇒ `None` = **不限**（排障/回退）；
/// - 非法值 ⇒ 也回默认（这不是安全闸，容忍非法值比悄悄关掉保护更符合预期）。
pub fn image_budget_from(value: Option<&str>) -> Option<usize> {
    match value.map(str::trim) {
        Some("0") => None,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n > 0 => Some(n),
            _ => Some(IMAGE_BUDGET_DEFAULT_BYTES),
        },
        None => Some(IMAGE_BUDGET_DEFAULT_BYTES),
    }
}

impl ImageBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            state: std::sync::Mutex::new(ImageBudgetState::default()),
        }
    }

    /// 从环境源构造（`None` = 不启用预算，调用方直接用 `Guard::None` 之外的原行为）。
    fn from_env(env: &dyn EnvSource) -> Option<Arc<Self>> {
        image_budget_from(env.get(IMAGE_BUDGET_ENV).as_deref())
            .map(|limit| Arc::new(Self::new(limit)))
    }

    /// 对一次 `read` 结果做**去重 + 预算降级**，返回本轮新占用的字节数。
    ///
    /// 逐块处理：`Image` 块按「已见过 ⇒ 省略」「超预算 ⇒ 省略」「否则保留并计入」三条走；
    /// `Text` 块原样不动。被省略的图**换成一行动作文本**，不静默丢弃——模型要知道去哪拿图。
    ///
    /// 锁中毒时**不 panic**：预算不是安全闸，宁可退化成「本次不检查」也不要搞崩工具调用。
    fn apply(&self, content: &mut [TextContentOrImage]) -> usize {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut added = 0usize;
        for block in content.iter_mut() {
            let TextContentOrImage::Image(image) = block else {
                continue;
            };
            let fingerprint = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                image.data.hash(&mut hasher);
                (hasher.finish(), image.data.len())
            };
            let bytes = image.data.len();
            let (replacement, reason) = if state.seen.contains(&fingerprint) {
                (
                    format!(
                        "[image 已省略：同一张图已在本次会话的上下文里（{}，{} 字节）；需要重新看图请用 dev__shell 处理，不要重复读入]",
                        image.mime_type, bytes
                    ),
                    "duplicate",
                )
            } else if state.used.saturating_add(bytes) > self.limit {
                (
                    format!(
                        "[image 已省略：本次会话图片预算已用尽（已用 {} / 上限 {} 字节，本图 {} 字节）。\
                         上游请求体有上限，继续读图会导致整个请求被拒。\
                         需要处理这张图请用 dev__shell（例如转成小尺寸后写到工作区再看局部）]",
                        state.used, self.limit, bytes
                    ),
                    "over-budget",
                )
            } else {
                state.seen.insert(fingerprint);
                state.used = state.used.saturating_add(bytes);
                added = added.saturating_add(bytes);
                continue;
            };
            tracing::warn!(
                reason,
                mime = %image.mime_type,
                bytes,
                used = state.used,
                limit = self.limit,
                "dev__read 图片被省略（避免上游请求体超限）"
            );
            *block = TextContentOrImage::text(replacement);
        }
        // 80% 水位警告：只报一次（否则每轮读图都刷一行，反而淹没真正的越限）。
        if !state.warned && state.used * 5 >= self.limit * 4 {
            state.warned = true;
            tracing::warn!(
                used = state.used,
                limit = self.limit,
                "会话图片预算已用超过 80%：再读图就可能把上游请求体顶爆"
            );
        }
        added
    }
}

impl PartialEq for ImageBudget {
    /// 只按**句柄身份**比：`Guard` 需要 `PartialEq`（`ShellTimeout` 分支在用），
    /// 而预算的内容是可变状态，逐字段比没有意义。
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for ImageBudget {}

fn exposed(
    inner: Arc<dyn AgentTool>,
    bare: &str,
    guard: Guard,
    workspace: &Path,
) -> Arc<dyn AgentTool> {
    let base = inner.schema();
    let mut parameters = base.parameters.clone();
    let mut description = base.description.clone();
    if guard == Guard::ShellTimeout {
        rename_param(&mut parameters, "timeout", "timeout_secs");
        // rpi 的原文写着「no default timeout」，而本包设了 120s 并把值 clamp 进 1..=600
        // ——工具的说明必须与真实行为一致，否则模型会按错的默认值规划命令。
        let hint = format!(
            "Optional timeout in seconds (default {:.0}, clamped to {:.0}..={:.0}).",
            DEFAULT_SHELL_TIMEOUT_SECS, MIN_SHELL_TIMEOUT_SECS, MAX_SHELL_TIMEOUT_SECS
        );
        description = description.replace("Optionally provide a timeout in seconds.", &hint);
        describe_timeout_bounds(&mut parameters);
    }
    if matches!(guard, Guard::WritePath { .. }) {
        // 参照物的 description 里写明「只能写工作区内」——工具的说明与真实约束一致，
        // 模型才不会白试几次（本包的约束与参照物同款，见 [`confine_write_path`]）。
        description.push_str(
            "\n\nPath is confined to the session workspace: paths escaping it \
             (absolute paths outside the workspace, `..` traversal, symlinks) are rejected.",
        );
    }
    if matches!(guard, Guard::ReadPath(_)) {
        description.push_str(
            "\n\nPath is confined to the session workspace (plus a few read-only roots \
             outside it, e.g. the bot-level attachments directory): paths outside them \
             (absolute paths, `..` traversal, symlinks) are rejected.",
        );
    }
    let schema = Tool {
        name: format!("{DEV_NAMESPACE}{SEP}{bare}"),
        description,
        parameters,
        constrained_sampling: base.constrained_sampling.clone(),
    };
    Arc::new(Exposed {
        schema,
        inner,
        guard,
        workspace: workspace.to_path_buf(),
    })
}

/// 给 `timeout_secs` 补上真实边界（clamp 区间），让 schema 与实现自洽。
fn describe_timeout_bounds(schema: &mut Schema) {
    let Value::Object(root) = &mut schema.0 else {
        return;
    };
    let Some(Value::Object(properties)) = root.get_mut("properties") else {
        return;
    };
    let Some(Value::Object(timeout)) = properties.get_mut("timeout_secs") else {
        return;
    };
    timeout.insert("minimum".to_string(), Value::from(MIN_SHELL_TIMEOUT_SECS));
    timeout.insert("maximum".to_string(), Value::from(MAX_SHELL_TIMEOUT_SECS));
    // **参数级** description 也是模型可见面：rpi 的原文写着「no default timeout」，与本包
    // 实际设的 120s/clamp 矛盾——只改工具级那句是不够的（评审实测抓到这一点）。
    timeout.insert(
        "description".to_string(),
        Value::String(format!(
            "Timeout in seconds (default {:.0}, clamped to {:.0}..={:.0}).",
            DEFAULT_SHELL_TIMEOUT_SECS, MIN_SHELL_TIMEOUT_SECS, MAX_SHELL_TIMEOUT_SECS
        )),
    );
}

/// 把 JSON Schema 里 `properties` 下的某个字段改名（用于 `timeout` → `timeout_secs`）。
fn rename_param(schema: &mut Schema, from: &str, to: &str) {
    let Value::Object(root) = &mut schema.0 else {
        return;
    };
    let Some(Value::Object(properties)) = root.get_mut("properties") else {
        return;
    };
    if let Some(value) = properties.remove(from) {
        properties.insert(to.to_string(), value);
    }
    // `required` 里若列了旧名，一起改（否则模型会被告知必填一个不存在的字段）。
    if let Some(Value::Array(required)) = root.get_mut("required") {
        for entry in required.iter_mut() {
            if entry.as_str() == Some(from) {
                *entry = Value::String(to.to_string());
            }
        }
    }
}

#[async_trait]
impl AgentTool for Exposed {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.schema.name
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let params = match &self.guard {
            Guard::None => params,
            Guard::WritePath { deny_bridge_state } => {
                if *deny_bridge_state {
                    deny_bridge_state_path(&params)?;
                }
                self.confine(&params).await?
            }
            Guard::ReadPath(roots) => self.confine_read(&params, roots).await?,
            // 读域限定与图片预算：前者改入参，后者后处理结果（见下方 `budget` 分支）。
            Guard::ReadWithBudget { roots, .. } => match roots {
                Some(roots) => self.confine_read(&params, roots).await?,
                None => params,
            },
            Guard::ShellTimeout => rename_timeout_param(params),
        };
        let mut result = self
            .inner
            .execute(tool_call_id, params, signal, on_update)
            .await?;
        if let Guard::ReadWithBudget { budget, .. } = &self.guard {
            let added = budget.apply(&mut result.content);
            if added > 0 {
                // 记到 `details` 里，让上游（与人工排障）能看到本回合新增了多少图片字节。
                if let Value::Object(map) = &mut result.details {
                    map.insert("image_budget_added_bytes".into(), Value::from(added));
                }
            }
        }
        Ok(result)
    }
}

impl Exposed {
    /// 把写类工具的 `path` 限定到会话工作区，并把**已解析的绝对路径**传给内层工具。
    async fn confine(&self, params: &Value) -> Result<Value, AgentError> {
        let Some(raw) = params.get("path").and_then(Value::as_str) else {
            // 形状不对时不自作主张：交给内层工具按它自己的 schema 报错。
            return Ok(params.clone());
        };
        let target = confine_write_path(&self.workspace, raw)
            .await
            .map_err(AgentError::Tool)?;
        let mut params = params.clone();
        if let Value::Object(map) = &mut params {
            map.insert(
                "path".to_string(),
                Value::String(target.to_string_lossy().into_owned()),
            );
        }
        Ok(params)
    }

    /// 把读类工具的 `path` 限定到会话工作区（受限会话），并把**已解析的绝对路径**传给内层。
    ///
    /// 读类工具（read/ls/grep/find）的 `path` 是**可选**的：缺省 = 工作区（安全，原样委托），
    /// 给了才做 canonicalize + 前缀校验（与参照物受限档 `read_roots` 同语义）。
    async fn confine_read(
        &self,
        params: &Value,
        extra_roots: &[PathBuf],
    ) -> Result<Value, AgentError> {
        let Some(raw) = params.get("path").and_then(Value::as_str) else {
            // 缺省 path（ls/grep/find 都不必填）= 工作区本身，无需拦。
            return Ok(params.clone());
        };
        let target = confine_read_path(&self.workspace, extra_roots, raw)
            .await
            .map_err(AgentError::Tool)?;
        let mut params = params.clone();
        if let Value::Object(map) = &mut params {
            map.insert(
                "path".to_string(),
                Value::String(target.to_string_lossy().into_owned()),
            );
        }
        Ok(params)
    }
}

/// `timeout_secs` → `timeout`（rpi 的 bash 工具用这个名字），并 clamp 进参照物的区间。
///
/// 模型给的超时可能是 `0`、负数、`1e9` 或非数字：参照物是 clamp 1..=600（默认 120），
/// 这里同款——`0`/负数/非法值一律落回默认，过大截到上界（免得一条命令把回合挂到天荒地老）。
fn rename_timeout_param(params: Value) -> Value {
    let Value::Object(mut map) = params else {
        return params;
    };
    let requested = map
        .remove("timeout_secs")
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS);
    let clamped = requested.clamp(MIN_SHELL_TIMEOUT_SECS, MAX_SHELL_TIMEOUT_SECS);
    map.insert("timeout".to_string(), Value::from(clamped));
    Value::Object(map)
}

/// 写入路径限定：**已经解析好的绝对目标**必须落在会话工作区内。
///
/// 与参照物 `参照物 devtools.rs::confined_write_path` 同一套判据：
/// 父目录先建后 canonicalize（这样 `..` 与符号链接都被解成真实路径再比对前缀），
/// 从而拦掉「`../escape`」「指向工作区外的符号链接父目录」这两类逃逸。
/// 与参照物的一处**有意更紧**：本包允许**落在工作区内的**绝对路径（参照物一律拒绝绝对
/// 路径），因为 rpi 的 write/edit 会把相对路径先解成绝对路径再落盘，不给它绝对路径反而
/// 会破坏正常写入——「不得出工作区」这条本身没有放宽。
/// 受限会话不得改写渠道「状态面」——即会被下一轮会话当成可信输入读回去的路径。
///
/// 名单与 `src/guard.rs::is_bridge_state_path` **同口径**（那边是 claude 腿，这边是生产
/// 执行层；两处都有单测锁住同一批变体）。原先这里只判一个字面名 `mcp.json`，于是
/// 技能目录 / `AGENTS.md` / `sessions.json` 三条面在生产路径上**完全没有名单**（卡
/// `abb-channel-mcp-remaining-injection-surfaces-20261012` 的三条 finding）。
///
/// 判定分两层：
/// 1. **最终文件名**（大小写不敏感、剥离尾随 `.`/空格——APFS/NTFS 会剥，不剥就能用
///    `AGENTS.md.` 绕过）；`sub/../AGENTS.md` 这类穿越的最终组件同样是目标名，自然覆盖。
/// 2. **路径组件**：`.agents`/`.goose`/`.claude` 是技能根（`skills.rs::SKILL_DIRS`——
///    `name`/`description` 进系统提示，正文由 `load_skill` 读），另外 `.git`、`summaries`、
///    `history` 是桥自身状态。按**组件**判而不是按前缀字符串判，是为了同时挡住
///    `skills/x` 与 `sub/skills/x` 这类子目录写法。
fn deny_bridge_state_path(params: &Value) -> Result<(), AgentError> {
    let Some(raw) = params.get("path").and_then(Value::as_str) else {
        return Ok(());
    };
    if !is_bridge_state_path(raw) {
        return Ok(());
    }
    Err(AgentError::Tool(
        "受限会话不能改写渠道的状态文件或技能目录：它们是**下一条会话的可信输入**\
         （MCP 配置会被无沙箱起、技能/AGENTS.md 会进系统提示、sessions.json/history 是会话槽位与历史注入面），\
         写它等于把内容塞进后面的 owner 会话执行。请让 owner 会话或人工维护\
         （卡 abb-channel-mcp-remaining-injection-surfaces-20261012）。"
            .to_string(),
    ))
}

/// 文件名名单：会进提示词/被信任状态的单文件。
const DENIED_FILE_NAMES: &[&str] = &[
    "jobs.json",
    "pending.json",
    "sessions.json",
    "claude.md",
    "agents.md",
    ".mcp.json",
    "mcp.json",
];

/// 路径组件名单：技能根与桥自身状态目录。`.claude` 同时覆盖 `.claude/skills`。
const DENIED_COMPONENTS: &[&str] = &[
    ".claude",
    ".agents",
    ".goose",
    "summaries",
    "history",
    ".git",
];

/// 纯判据（无 IO）：与 `src/guard.rs::is_bridge_state_path` 同一套规则。
fn is_bridge_state_path(p: &str) -> bool {
    // APFS/NTFS 会剥离文件名/组件末尾的 '.' 与空格（`AGENTS.md.` 落盘即 `AGENTS.md`）：
    // 判定前按同样规则剪掉，否则尾随点就是一条绕过路径。
    let trim_trail = |s: &str| -> String { s.trim_end_matches(['.', ' ', '\t', '\r']).to_string() };
    let path = std::path::Path::new(p);
    if let Some(name) = path.file_name() {
        let name = trim_trail(&name.to_string_lossy().to_ascii_lowercase());
        if DENIED_FILE_NAMES.contains(&name.as_str())
            || name.starts_with("jobs.json.migrated")
            || name.ends_with(".agents.md")
            || name.ends_with(".claude.md")
        {
            return true;
        }
    }
    path.components().any(|c| {
        let s = trim_trail(&c.as_os_str().to_string_lossy());
        DENIED_COMPONENTS
            .iter()
            .any(|denied| s.eq_ignore_ascii_case(denied))
    })
}

async fn confine_write_path(workspace: &Path, raw: &str) -> Result<PathBuf, String> {
    let base = tokio::fs::canonicalize(workspace)
        .await
        .map_err(|error| format!("会话工作区不可访问：{}（{error}）", workspace.display()))?;
    let path = Path::new(raw);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let parent = candidate
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| base.clone());
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|error| format!("无法创建父目录：{raw}（{error}）"))?;
    let parent = tokio::fs::canonicalize(&parent)
        .await
        .map_err(|error| format!("父目录不可访问：{raw}（{error}）"))?;
    let file_name = candidate
        .file_name()
        .ok_or_else(|| format!("路径非法：{raw}"))?;
    let target = parent.join(file_name);
    if !target.starts_with(&base) {
        return Err(format!(
            "拒绝写入会话工作区之外：{raw}（工作区 {}）",
            base.display()
        ));
    }
    // 末段本身是符号链接时，`fs::write` 会**跟随链接**写到工作区外
    // （上面只 canonicalize 了父目录）——参照物在同一形状上也是显式拒绝。
    if let Ok(metadata) = tokio::fs::symlink_metadata(&target).await {
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "拒绝跟随符号链接写入（可能写到会话工作区之外）：{raw}"
            ));
        }
    }
    Ok(target)
}

/// 读取路径限定：`path`（相对或绝对）解析 + canonicalize 后必须落在会话工作区内。
///
/// 与参照物 `参照物 devtools.rs::resolve_read_path` 的受限档 `read_roots`
/// 分支同语义：相对路径按工作区为基准解、绝对路径原样，都 canonicalize 后再前缀比对。
/// 符号链接同样被解掉（canonicalize 还原真实路径）——所以「工作区内指向工作区外的链接」
/// 也读不到。与写域闸不同：读不创建父目录、不拒绝对路径（只要落在工作区内就放行）。
async fn confine_read_path(
    workspace: &Path,
    extra_roots: &[PathBuf],
    raw: &str,
) -> Result<PathBuf, String> {
    let base = tokio::fs::canonicalize(workspace)
        .await
        .map_err(|error| format!("会话工作区不可访问：{}（{error}）", workspace.display()))?;
    // 附加只读根（如 bot 级 attachments/）：canonicalize 失败的根忽略（不存在就不算可读面）。
    let mut roots: Vec<PathBuf> = vec![base.clone()];
    for root in extra_roots {
        if let Ok(canon) = tokio::fs::canonicalize(root).await {
            roots.push(canon);
        }
    }
    let path = Path::new(raw);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let canon = tokio::fs::canonicalize(&candidate)
        .await
        .map_err(|error| format!("路径不可访问：{raw}（{error}）"))?;
    if !roots.iter().any(|root| canon.starts_with(root)) {
        return Err(format!(
            "拒绝读取会话工作区与只读根之外：{raw}（工作区 {}）",
            base.display()
        ));
    }
    Ok(canon)
}

/// `load_skill` 的工具名：**裸名**（不经 `dev__` 命名空间）。
///
/// 参照物是把这个工具直接 push 进工具表（`agent.rs` 里 `tools.push(builtin::load_skill_def())`），
/// 所以模型看到的就是 `load_skill`——本包照做，改名会让既有的技能约定/技能正文里的
/// `load_skill(name: …)` 提示全部失效。
pub const LOAD_SKILL_TOOL: &str = "load_skill";

/// 造本会话的 `load_skill` 工具。
///
/// 技能清单来自 [`crate::skills::discover`]，**只在约定链开启且真有技能时**才暴露（参照物：
/// hints 关掉时 skills 是空 vec、且 `agent.rs` 只在 `!skills.is_empty()` 时 push 这个工具）。
/// 无技能时仍然暴露的话，模型会看到一个指向不存在段落的工具（评审实测到的对外可见偏离）。
pub fn load_skill_tool(skills: Vec<crate::skills::SkillEntry>) -> Arc<dyn AgentTool> {
    let schema = Tool {
        name: LOAD_SKILL_TOOL.to_string(),
        description: "Load the full content of a skill by name. \
            Call this before using a skill — the system prompt lists skill names \
            and descriptions only; the full instructions are loaded on demand. \
            To load a supporting file within a skill, use the form \
            \"skill-name/relative/path\" (e.g. \"my-skill/references/foo.md\")."
            .to_string(),
        parameters: Schema(serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill name as listed in the Available Skills section, \
                        or \"skill-name/relative/path\" to load a supporting file."
                }
            },
            "required": ["name"]
        })),
        constrained_sampling: None,
    };
    Arc::new(LoadSkillTool { schema, skills })
}

struct LoadSkillTool {
    schema: Tool,
    skills: Vec<crate::skills::SkillEntry>,
}

#[async_trait]
impl AgentTool for LoadSkillTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        LOAD_SKILL_TOOL
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(AgentError::Tool(
                "load_skill: missing required argument \"name\"".to_string(),
            ));
        };
        // 读取是同步文件 IO：放进 blocking 池，别占住 runtime（沙箱/加载都可能慢）。
        let skills = self.skills.clone();
        let request = name.to_string();
        let loaded = tokio::task::spawn_blocking(move || crate::skills::load(&request, &skills))
            .await
            .map_err(|error| AgentError::Tool(format!("load_skill: 任务失败：{error}")))?;
        match loaded {
            Ok(text) => Ok(AgentToolResult::text(text)),
            // 找不到/越界都走「错误工具结果」（与参照物一致）：模型据此自我纠偏。
            Err(reason) => Err(AgentError::Tool(reason)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProcessEnv;

    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abb-builtin-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("建临时工作区");
        dir
    }

    /// 开关读法逐条对齐 fork：未设置/非零开、`0` 关、读不懂报错。
    #[test]
    fn dev_tools_switch_matches_the_replaced_component() {
        assert_eq!(
            dev_tools_enabled_from(None),
            Ok(true),
            "未设置 = 开（fork 默认 1）"
        );
        for on in ["1", "2", "255"] {
            assert_eq!(dev_tools_enabled_from(Some(on)), Ok(true), "{on:?}");
        }
        assert_eq!(dev_tools_enabled_from(Some("0")), Ok(false));
        for bad in ["true", "-1", "256", "1\n", " 1 "] {
            assert!(dev_tools_enabled_from(Some(bad)).is_err(), "{bad:?} 应报错");
        }
        assert_eq!(dev_tools_enabled(&ProcessEnv), Ok(true));
    }

    /// 暴露给模型的名字必须是 `dev__{参照物名}`，且 shell 的超时字段名与参照物一致。
    #[test]
    fn tools_are_exposed_under_dev_namespace_with_replaced_component_shapes() {
        let workspace = temp_workspace("names");
        let tools = dev_tools(
            &workspace,
            crate::builtin::SandboxMode::FullAccess,
            false,
            &[],
        );
        let names: Vec<String> = tools.iter().map(|t| t.schema().name.clone()).collect();
        for expected in [
            "dev__shell",
            "dev__read",
            "dev__write",
            "dev__edit",
            "dev__ls",
            "dev__grep",
            // 参照物的工具名是 `glob`（rpi 那个工具叫 find，入参就是 glob 模式）⇒ 同名暴露。
            "dev__glob",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "缺 {expected}：{names:?}"
            );
        }
        assert!(
            !names.iter().any(|name| !name.starts_with("dev__")),
            "不得暴露裸名（会与 MCP 工具命名空间混）：{names:?}"
        );

        let shell = tools
            .iter()
            .find(|t| t.schema().name == "dev__shell")
            .expect("shell 工具");
        let props = shell.schema().parameters.0["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert!(props.contains(&"timeout_secs".to_string()), "{props:?}");
        assert!(
            !props.contains(&"timeout".to_string()),
            "rpi 的字段名不得泄漏给模型：{props:?}"
        );
        assert!(props.contains(&"command".to_string()), "{props:?}");
    }

    /// 档位决定工具面（统一迁移刀 3 整改 C1）：read-only 下 write/shell/delegate **不注入**
    /// （模型看不见），read/ls/grep/find 仍在；workspace-write 与 full-access 全工具可见。
    #[test]
    fn tool_surface_follows_sandbox_mode() {
        let workspace = temp_workspace("mode");

        let read_only = dev_tools(
            &workspace,
            crate::builtin::SandboxMode::ReadOnly,
            false,
            &[],
        );
        let ro_names: Vec<String> = read_only.iter().map(|t| t.schema().name.clone()).collect();
        for absent in ["dev__shell", "dev__write", "dev__edit", "dev__delegate"] {
            assert!(
                !ro_names.contains(&absent.to_string()),
                "read-only 档不得暴露 {absent}：{ro_names:?}"
            );
        }
        for present in ["dev__read", "dev__ls", "dev__grep", "dev__glob"] {
            assert!(
                ro_names.contains(&present.to_string()),
                "read-only 档应保留 {present}：{ro_names:?}"
            );
        }

        let ww = dev_tools(
            &workspace,
            crate::builtin::SandboxMode::WorkspaceWrite,
            false,
            &[],
        );
        let ww_names: Vec<String> = ww.iter().map(|t| t.schema().name.clone()).collect();
        for present in ["dev__shell", "dev__write", "dev__edit", "dev__read"] {
            assert!(
                ww_names.contains(&present.to_string()),
                "workspace-write 档应保留 {present}：{ww_names:?}"
            );
        }

        let full = dev_tools(
            &workspace,
            crate::builtin::SandboxMode::FullAccess,
            false,
            &[],
        );
        let full_names: Vec<String> = full.iter().map(|t| t.schema().name.clone()).collect();
        assert!(full_names.contains(&"dev__shell".to_string()));

        // granted（shell=restricted）档：delegate 不注入（对齐参照物 delegate_def_visible
        // 的 `shell != Restricted` 腿）；shell/write 仍在（内容由 Jev 门禁判断）。
        let granted = dev_tools(
            &workspace,
            crate::builtin::SandboxMode::WorkspaceWrite,
            true,
            &[],
        );
        let granted_names: Vec<String> = granted.iter().map(|t| t.schema().name.clone()).collect();
        assert!(
            !granted_names.contains(&"dev__delegate".to_string()),
            "granted 档不得暴露 delegate：{granted_names:?}"
        );
        for present in ["dev__shell", "dev__write", "dev__read"] {
            assert!(
                granted_names.contains(&present.to_string()),
                "granted 档应保留 {present}：{granted_names:?}"
            );
        }
    }

    /// `SandboxMode::parse` 词表与 abb 下发一致：未知/缺失回落 FullAccess。
    #[test]
    fn sandbox_mode_parse_matches_abb_vocabulary() {
        assert_eq!(
            crate::builtin::SandboxMode::parse(Some("read-only")),
            crate::builtin::SandboxMode::ReadOnly
        );
        assert_eq!(
            crate::builtin::SandboxMode::parse(Some("workspace-write")),
            crate::builtin::SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            crate::builtin::SandboxMode::parse(Some("full-access")),
            crate::builtin::SandboxMode::FullAccess
        );
        assert_eq!(
            crate::builtin::SandboxMode::parse(None),
            crate::builtin::SandboxMode::FullAccess
        );
        assert_eq!(
            crate::builtin::SandboxMode::parse(Some("bogus")),
            crate::builtin::SandboxMode::FullAccess
        );
    }

    /// 模型看到的说明必须与真实行为一致：超时默认 120、区间 1..=600；写类工具声明写入限定。
    #[test]
    fn exposed_schemas_match_real_behavior() {
        let workspace = temp_workspace("docs");
        let tools = dev_tools(
            &workspace,
            crate::builtin::SandboxMode::FullAccess,
            false,
            &[],
        );
        let by_name = |name: &str| -> Arc<dyn AgentTool> {
            tools
                .iter()
                .find(|tool| tool.schema().name == name)
                .unwrap_or_else(|| panic!("缺 {name}"))
                .clone()
        };

        let shell = by_name("dev__shell").schema().clone();
        assert!(
            !shell.description.contains("no default timeout"),
            "rpi 原文与新行为矛盾：{}",
            shell.description
        );
        assert!(
            shell.description.contains("default 120"),
            "{}",
            shell.description
        );
        let timeout = &shell.parameters.0["properties"]["timeout_secs"];
        assert_eq!(timeout["minimum"], 1.0, "{timeout}");
        assert_eq!(timeout["maximum"], 600.0, "{timeout}");
        // 关键：**参数级** description 也是模型可见的，rpi 原文那句「no default timeout」必须没了。
        let param_desc = timeout["description"].as_str().unwrap_or_default();
        assert!(
            !param_desc.contains("no default timeout"),
            "参数级说明与真实默认超时矛盾：{param_desc}"
        );
        assert!(param_desc.contains("default 120"), "{param_desc}");

        for name in ["dev__write", "dev__edit"] {
            let schema = by_name(name).schema().clone();
            assert!(
                schema
                    .description
                    .contains("confined to the session workspace"),
                "{name} 的说明未声明写入限定：{}",
                schema.description
            );
            assert!(
                !schema.description.contains("  "),
                "{name} 的说明里有多余空格（形如格式化残留）：{:?}",
                schema.description
            );
        }
    }

    /// `load_skill`：**裸名**（不带 `dev__`）+ 缺参/找不到都有可纠偏的错误。
    #[tokio::test]
    async fn load_skill_tool_is_bare_named_and_reports_actionable_errors() {
        let tool = load_skill_tool(Vec::new());
        assert_eq!(
            tool.schema().name,
            "load_skill",
            "参照物是裸名，不能加 dev__ 前缀"
        );
        assert!(
            tool.schema().parameters.0["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|v| v == "name")),
            "name 必填：{}",
            tool.schema().parameters.0
        );

        let no_param = tool
            .execute(
                "call",
                serde_json::json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .expect_err("缺 name 必须报错");
        assert!(no_param.to_string().contains("name"), "{no_param}");

        let not_found = tool
            .execute(
                "call",
                serde_json::json!({ "name": "nope" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .expect_err("找不到技能必须报错");
        assert!(
            not_found.to_string().contains("not found"),
            "错误要可纠偏：{not_found}"
        );
    }

    /// `timeout_secs` → `timeout` 的翻译 + clamp（参照物：默认 120、区间 1..=600）。
    #[test]
    fn shell_timeout_param_is_translated_and_clamped() {
        let translated = rename_timeout_param(serde_json::json!({
            "command": "ls",
            "timeout_secs": 30,
        }));
        assert_eq!(translated["timeout"], 30.0);
        assert!(translated.get("timeout_secs").is_none());

        // 缺失 / 0 / 负数 / 非法值 ⇒ 默认 120。
        for params in [
            serde_json::json!({"command": "ls"}),
            serde_json::json!({"command": "ls", "timeout_secs": 0}),
            serde_json::json!({"command": "ls", "timeout_secs": -5}),
            serde_json::json!({"command": "ls", "timeout_secs": "soon"}),
        ] {
            assert_eq!(
                rename_timeout_param(params)["timeout"],
                120.0,
                "应落回参照物的默认超时"
            );
        }
        // 上界截断（免得一条命令把回合挂死）。
        assert_eq!(
            rename_timeout_param(serde_json::json!({"command": "ls", "timeout_secs": 1e9}))
                ["timeout"],
            600.0
        );
        assert_eq!(
            rename_timeout_param(serde_json::json!({"command": "ls", "timeout_secs": 0.5}))
                ["timeout"],
            1.0
        );
        // 非对象原样返回（内层工具会自己报形状错误）。
        assert_eq!(rename_timeout_param(Value::Null), Value::Null);
    }

    /// 写入限定：工作区内可写、`..` 与工作区外绝对路径被拒。
    #[tokio::test]
    async fn write_confinement_rejects_escapes() {
        let workspace = temp_workspace("write");
        let inside = confine_write_path(&workspace, "sub/file.txt")
            .await
            .expect("工作区内相对路径应可写");
        let canon = tokio::fs::canonicalize(&workspace).await.expect("canon");
        assert!(inside.starts_with(&canon), "{inside:?}");

        let absolute_inside =
            confine_write_path(&workspace, &format!("{}/abs.txt", canon.to_string_lossy()))
                .await
                .expect("工作区内绝对路径应可写（有意比参照物更紧的那一条不放宽）");
        assert!(absolute_inside.starts_with(&canon));

        let escape = confine_write_path(&workspace, "../escape.txt").await;
        assert!(escape.is_err(), "`..` 逃逸必须被拒：{escape:?}");
        let outside = confine_write_path(&workspace, "/tmp/abb-escape-outside.txt").await;
        assert!(outside.is_err(), "工作区外绝对路径必须被拒：{outside:?}");
    }

    /// **末段是符号链接**时必须拒绝：`fs::write` 会跟随链接写到工作区外
    /// （只 canonicalize 父目录挡不住这一形状）。参照物在同一形状上也是显式拒绝。
    ///
    /// **整条挂 `#[cfg(unix)]`**（而不是在体内 `#[cfg(not(unix))] return`）：后者在 Windows 上
    /// 会留下「只被 cfg(unix) 块用到的 `let link`」与 `return` 之后的不可达语句，而
    /// `abb-agent-windows` 作业的 clippy 是 `-D warnings` ⇒ 直接把 CI 弄红（评审实测 7 条）。
    #[cfg(unix)]
    #[tokio::test]
    async fn write_confinement_rejects_symlinked_file_escape() {
        let workspace = temp_workspace("symlink-file");
        let outside = temp_workspace("symlink-file-outside");
        let outside_file = outside.join("target.txt");
        std::fs::write(&outside_file, "ORIGINAL").expect("建工作区外文件");
        let link = workspace.join("link.txt");
        std::os::unix::fs::symlink(&outside_file, &link).expect("建符号链接文件");
        let attempt = confine_write_path(&workspace, "link.txt").await;
        assert!(attempt.is_err(), "末段符号链接必须被拒：{attempt:?}");
        // 目标文件必须原封不动（这条断言才是「没写穿」的判据）。
        assert_eq!(
            std::fs::read_to_string(&outside_file).expect("读工作区外文件"),
            "ORIGINAL"
        );
    }

    /// 符号链接父目录指向工作区外时也要拦住（canonicalize 之后再比前缀）。
    #[cfg(unix)]
    #[tokio::test]
    async fn write_confinement_rejects_symlinked_parent_escape() {
        let workspace = temp_workspace("symlink");
        let outside = temp_workspace("symlink-outside");
        let link = workspace.join("link");
        std::os::unix::fs::symlink(&outside, &link).expect("建符号链接");
        let attempt = confine_write_path(&workspace, "link/escape.txt").await;
        assert!(attempt.is_err(), "符号链接逃逸必须被拒：{attempt:?}");
    }

    /// 附加只读根（`_meta.readRoots`，如 bot 级 `attachments/`）：受限会话可读它，
    /// 但**根之外仍拒**（不能因为给了根就放宽成任意读）。卡 abb-channel-mcp-followups-20261012。
    #[tokio::test]
    async fn read_confinement_allows_extra_read_root_only() {
        let workspace = temp_workspace("read-roots-ws");
        let attachments = temp_workspace("read-roots-bot");
        std::fs::write(workspace.join("inside.txt"), "IN").expect("建工作区内文件");
        std::fs::write(attachments.join("a.png"), "IMG").expect("建附件");
        let roots = vec![attachments.clone()];

        let got = confine_read_path(
            &workspace,
            &roots,
            &attachments.join("a.png").to_string_lossy(),
        )
        .await
        .expect("只读根内的文件应可读");
        assert!(got.ends_with("a.png"), "{got:?}");
        confine_read_path(&workspace, &roots, "inside.txt")
            .await
            .expect("工作区仍可读");
        let outside = confine_read_path(&workspace, &roots, "/etc/passwd").await;
        assert!(outside.is_err(), "只读根之外的路径必须仍被拒：{outside:?}");
        // 根不存在（已删/未建）时忽略该根，不 panic、也不放宽
        let gone = vec![workspace.join("nope")];
        assert!(confine_read_path(&workspace, &gone, "inside.txt")
            .await
            .is_ok());
        assert!(confine_read_path(&workspace, &gone, "/etc/passwd")
            .await
            .is_err());
    }

    /// 读域闸（统一迁移刀 3）：工作区内可读、`..` 与工作区外绝对路径被拒。
    /// **受限会话不得改写渠道 `mcp.json`**（卡 `abb-channel-mcp-write-escalation-20261012`）：
    /// 它声明的是会被 abb-agent **无沙箱**起的 MCP 子进程 ⇒ 受限写它 = 把代码塞进下一条 owner 会话执行。
    ///
    /// 三组判据：①受限（`shell_restricted=true`）写 `mcp.json` ⇒ **拒**（含大小写/尾随点/子目录变体）；
    /// ②同文件在 **owner**（`shell_restricted=false`）下 ⇒ **允许**（设计上就该由 owner 维护，别顺手堵死）；
    /// ③普通文件名（`note.txt`）在两档下都允许（证明拒绝不是"把所有写都拦了"）。
    #[tokio::test]
    async fn restricted_session_cannot_write_channel_mcp_json() {
        let workspace = temp_workspace("mcp-deny");
        let write_tool = |restricted: bool| -> Arc<dyn AgentTool> {
            dev_tools(&workspace, SandboxMode::WorkspaceWrite, restricted, &[])
                .into_iter()
                .find(|t| t.schema().name == "dev__write")
                .expect("dev__write 存在（WorkspaceWrite 档）")
        };
        let call = |tool: Arc<dyn AgentTool>, path: &str| {
            let path = path.to_string();
            async move {
                tool.execute(
                    "call",
                    serde_json::json!({ "path": path, "content": "[]" }),
                    CancellationToken::new(),
                    Arc::new(|_| {}),
                )
                .await
            }
        };

        for bad in [
            "mcp.json",
            "MCP.JSON",
            "mcp.json.",
            "sub/mcp.json",
            "sub/../mcp.json",
        ] {
            let err = call(write_tool(true), bad)
                .await
                .expect_err(&format!("受限会话写 {bad} 必须被拒"));
            assert!(
                err.to_string().contains("状态文件或技能目录"),
                "{bad} 的拒绝理由应说明是状态面：{err}"
            );
        }

        // owner 档：同一文件可写（负向对照）
        call(write_tool(false), "mcp.json")
            .await
            .expect("owner 会话应仍可维护渠道 mcp.json");
        assert!(workspace.join("mcp.json").exists(), "owner 写入应真的落盘");

        // 普通文件不受影响（负向对照：别把受限会话的写全拦了）
        call(write_tool(true), "note.txt")
            .await
            .expect("受限会话写普通文件应仍允许");
    }

    /// 卡 `abb-channel-mcp-remaining-injection-surfaces-20261012` 的三条 finding：
    /// 技能目录 / `AGENTS.md` / `sessions.json` 在**生产执行层**上也必须被拒。
    ///
    /// 判别力：把 `DENIED_COMPONENTS` 或 `DENIED_FILE_NAMES` 删项 ⇒ 对应用例红；
    /// 把尾随点剪裁去掉 ⇒ `AGENTS.md.` 一类变体红。
    #[tokio::test]
    async fn restricted_session_cannot_write_bridge_state_surfaces() {
        let workspace = temp_workspace("state-deny");
        let write_tool = |restricted: bool| -> Arc<dyn AgentTool> {
            dev_tools(&workspace, SandboxMode::WorkspaceWrite, restricted, &[])
                .into_iter()
                .find(|t| t.schema().name == "dev__write")
                .expect("dev__write 存在（WorkspaceWrite 档）")
        };
        let call = |tool: Arc<dyn AgentTool>, path: &str| {
            let path = path.to_string();
            async move {
                tool.execute(
                    "call",
                    serde_json::json!({ "path": path, "content": "x" }),
                    CancellationToken::new(),
                    Arc::new(|_| {}),
                )
                .await
            }
        };

        // 三条 finding 各自的面 + 既有状态文件；每条带大小写/尾随点/子目录/穿越变体。
        // 子目录与穿越是**承重**变体：名单若写成 `starts_with` 或只比对原始字符串，这两类会漏。
        for bad in [
            ".agents/skills/pwn/SKILL.md",
            ".goose/skills/pwn/SKILL.md",
            ".claude/skills/pwn/SKILL.md",
            "AGENTS.md",
            "agents.md.",
            "CLAUDE.md",
            "sub/AGENTS.md",
            "sessions.json",
            "SESSIONS.JSON",
            "history/abc.jsonl",
            "jobs.json",
            "pending.json",
            ".mcp.json",
            ".git/config",
        ] {
            let err = call(write_tool(true), bad)
                .await
                .expect_err(&format!("受限会话写 {bad} 必须被拒（放行即提权链）"));
            assert!(
                err.to_string().contains("状态文件或技能目录"),
                "{bad} 的拒绝理由应说明是状态面：{err}"
            );
        }

        // 纯判据直喂（不经工具层）：确认「组件名单」不只靠路径整体相等，
        // 且普通技能内容的写不在名单内（否则会把正常写全拦掉）。
        assert!(is_bridge_state_path(".agents/skills/x/SKILL.md"));
        assert!(is_bridge_state_path("a/.goose/skills/y/SKILL.md"));
        assert!(is_bridge_state_path("AGENTS.md."), "尾随点必须被剪裁后再判");
        assert!(
            !is_bridge_state_path("skills/x/SKILL.md"),
            "无点前缀不是技能根"
        );
        assert!(!is_bridge_state_path("note.txt"));

        // owner 档同面可写（负向对照：只挡受限会话）
        call(write_tool(false), ".agents/skills/pwn/SKILL.md")
            .await
            .expect("owner 会话应仍可维护技能目录");
        assert!(
            workspace.join(".agents/skills/pwn/SKILL.md").exists(),
            "owner 写入应真的落盘"
        );

        // 普通文件不受影响（负向对照：别把受限会话的写全拦了）
        call(write_tool(true), "notes/plain.txt")
            .await
            .expect("受限会话写普通文件应仍允许");
    }

    #[tokio::test]
    async fn read_confinement_rejects_escapes() {
        let workspace = temp_workspace("read-conf");
        std::fs::write(workspace.join("inside.txt"), "IN").expect("建工作区内文件");
        let canon = tokio::fs::canonicalize(&workspace).await.expect("canon");

        let inside = confine_read_path(&workspace, &[], "inside.txt")
            .await
            .expect("工作区内相对路径应可读");
        assert!(inside.starts_with(&canon), "{inside:?}");
        // 绝对路径用 `Path::join` 造：Windows 上 `canonicalize` 返回 verbatim 前缀
        // （`\\?\C:\…`），字符串拼 `/` 会得到 verbatim 路径不接受的形态
        // （os error 123「文件名、目录名或卷标语法不正确」）——那是用例装置问题，不是读域闸。
        let absolute_raw = canon.join("inside.txt");
        let absolute_inside = confine_read_path(&workspace, &[], &absolute_raw.to_string_lossy())
            .await
            .expect("工作区内绝对路径应可读");
        assert!(absolute_inside.starts_with(&canon));

        let escape = confine_read_path(&workspace, &[], "../escape.txt").await;
        assert!(escape.is_err(), "`..` 逃逸必须被拒：{escape:?}");
        let outside = confine_read_path(&workspace, &[], "/etc/passwd").await;
        assert!(outside.is_err(), "工作区外绝对路径必须被拒：{outside:?}");
    }

    /// 读域闸把符号链接解掉：工作区内指向工作区外的链接也读不到（canonicalize 还原真实路径）。
    #[cfg(unix)]
    #[tokio::test]
    async fn read_confinement_rejects_symlink_escape() {
        let workspace = temp_workspace("read-symlink");
        let outside = temp_workspace("read-symlink-outside");
        std::fs::write(outside.join("secret.txt"), "SECRET").expect("建工作区外文件");
        let link = workspace.join("link.txt");
        std::os::unix::fs::symlink(outside.join("secret.txt"), &link).expect("建符号链接");
        let attempt = confine_read_path(&workspace, &[], "link.txt").await;
        assert!(
            attempt.is_err(),
            "指向工作区外的符号链接必须被拒：{attempt:?}"
        );
    }
    /// 接线判据（卡 `abb-agent-image-processor-wiring-20261012`）：`read` 工具的选项**必须**装了
    /// 图片处理器、且显式打开自动缩放——rpi 的默认是 `image_processor: None`，那时支持的格式
    /// **原字节 inline、不缩放**（仅 BMP 给"已省略"）；**像素上限本身不会丢图**（不装处理器时图是原样回来的）。
    ///
    /// 判别力（**按评审第 2 轮 F4 更正**）：**在 helper 里把 `image_processor` 置 None ⇒ 本用例红**（实测）。
    /// 而"把 `:175` 的构造点改回 `None`"**不会**让本用例红（本用例只观察 helper 的返回值）——那一层由
    /// `read_tool_is_constructed_with_processor_options`（文本守卫）与端到端用例各自认领。
    /// 判定用 rpi 自己的 `Debug`（把该字段渲染成 `image_processor: true/false`），不靠源码文本。
    #[test]
    fn read_tool_options_install_inline_image_processor() {
        let opts = read_tool_options();
        assert!(
            opts.auto_resize_images,
            "必须显式打开自动缩放（否则超限图等于被丢）"
        );
        assert!(
            opts.image_processor.is_some(),
            "read 工具必须注入 ReadImageProcessor 实现（rpi 默认 None）"
        );
        let rendered = format!("{opts:?}");
        assert!(
            rendered.contains("image_processor: true"),
            "rpi 的 Debug 应报告处理器已装：{rendered}"
        );
    }

    /// **端到端判据（评审 F1 的整改）**：走**真实的 `dev__read` 工具**（`dev_tools` → `OsExecutionEnv`），
    /// 断言返回的 content part 里有一张 `Image`、且**真实像素尺寸**已被缩到上限内、比例保持。
    ///
    /// 为什么必须有这条：上面两条行为用例是**直接调 rpi 库**的，钉的是"rpi 会缩放"，不是"本仓的 read
    /// 装了处理器"——评审实测：把生产接线改回 `None`（变异 A）或把 helper 的处理器置空（变异 B）或换成
    /// 不缩放的实现（变异 C），那两条**照样绿**。这条对三者都会红（不装 ⇒ 尺寸仍是 2500×100）。
    ///
    /// 不需要假 ExecutionEnv：`dev_tools` 自己建 `OsExecutionEnv::with_cwd(workspace)`（评审实测口径）。
    #[tokio::test]
    async fn read_tool_returns_resized_image_part_end_to_end() {
        use base64::Engine as _;
        let workspace = temp_workspace("img-e2e");
        // 2500×100 红色 PNG（超过 INLINE_IMAGE_MAX_SIDE=2000）
        let mut img = image::RgbImage::new(2500, 100);
        for px in img.pixels_mut() {
            *px = image::Rgb([200, 30, 30]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("编码 PNG");
        std::fs::write(workspace.join("big.png"), png.into_inner()).expect("写测试图");

        let tools = dev_tools(&workspace, SandboxMode::FullAccess, false, &[]);
        let read = tools
            .iter()
            .find(|t| t.schema().name == "dev__read")
            .expect("read 工具必须存在")
            .clone();
        let out = read
            .execute(
                "call",
                serde_json::json!({ "path": "big.png" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .expect("读图应成功");

        // 从 content part 里取图（不是文本提示）——"仍拿得到图"这一点必须由 part 类型证明。
        let (mime, bytes) = out
            .content
            .iter()
            .find_map(|part| match part {
                rpi_agent::types::TextContentOrImage::Image(img) => Some((
                    img.mime_type.clone(),
                    base64::engine::general_purpose::STANDARD
                        .decode(img.data.as_bytes())
                        .expect("图片 part 应是 base64"),
                )),
                _ => None,
            })
            .expect("read 应返回 Image part（而不是一行省略提示）");
        assert_eq!(mime, "image/png", "处理后的 mime");
        let decoded = image::load_from_memory(&bytes).expect("解码 part");
        assert!(
            decoded.width() <= rpi_tools::image_processing::INLINE_IMAGE_MAX_SIDE,
            "端到端：超限图必须被缩到上限内，实际 {}×{}",
            decoded.width(),
            decoded.height()
        );
        let ratio = decoded.width() as f64 / decoded.height() as f64;
        assert!(
            (ratio - 25.0).abs() < 0.5,
            "端到端：应等比缩放（原比 25.0，实际 {ratio:.2}：{}×{}）",
            decoded.width(),
            decoded.height()
        );
    }

    // ── 会话图片预算（卡 abb-413-image-budget-20261022）────────────────────

    fn image_block(data: &str) -> TextContentOrImage {
        TextContentOrImage::Image(rpi_ai::types::ImageContent {
            kind: rpi_ai::types::ImageContentType,
            data: data.to_string(),
            mime_type: "image/png".to_string(),
        })
    }

    fn image_data_of(block: &TextContentOrImage) -> Option<&str> {
        match block {
            TextContentOrImage::Image(img) => Some(img.data.as_str()),
            TextContentOrImage::Text(_) => None,
        }
    }

    fn text_of(block: &TextContentOrImage) -> Option<&str> {
        match block {
            TextContentOrImage::Text(t) => Some(t.text.as_str()),
            TextContentOrImage::Image(_) => None,
        }
    }

    /// 预算内：图片 part **原样保留**（逐字比对，证明没有副作用）。
    ///
    /// 判别力：把 `apply` 里「预算内」那一支也改成替换 ⇒ 本用例红。
    #[test]
    fn image_budget_keeps_images_within_limit() {
        let budget = ImageBudget::new(1000);
        let data = "A".repeat(600);
        let mut content = vec![image_block(&data), TextContentOrImage::text("说明")];
        let added = budget.apply(&mut content);
        assert_eq!(added, 600, "应记下本轮新增字节");
        assert_eq!(
            image_data_of(&content[0]),
            Some(data.as_str()),
            "预算内必须原样保留"
        );
        assert_eq!(text_of(&content[1]), Some("说明"), "文本块不得被动");
    }

    /// 超预算：图片 part 换成**可行动文本**（不是静默丢弃）。
    ///
    /// 判别力：删掉 `over-budget` 分支 ⇒ 图会漏进上下文 ⇒ 本用例红。
    #[test]
    fn image_budget_degrades_when_over_limit() {
        let budget = ImageBudget::new(1000);
        let mut content = vec![image_block(&"A".repeat(800)), image_block(&"B".repeat(400))];
        budget.apply(&mut content);
        assert!(
            image_data_of(&content[0]).is_some(),
            "第一张在预算内，应保留"
        );
        assert_eq!(
            image_data_of(&content[1]),
            None,
            "第二张超预算，必须不再以 Image 进上下文"
        );
        let msg = text_of(&content[1]).expect("应换成文本");
        assert!(msg.contains("预算已用尽"), "文案要说清是预算问题：{msg}");
        assert!(msg.contains("dev__shell"), "必须给出可行动的后路：{msg}");
    }

    /// 同一张图重复读：只算一次，第二份换成「已在上下文里」提示。
    ///
    /// 判别力：删掉 `seen.contains` 分支 ⇒ 第二份会计入预算且仍为 Image ⇒ 本用例红。
    #[test]
    fn image_budget_dedupes_identical_images() {
        let budget = ImageBudget::new(10_000);
        let data = "C".repeat(500);
        let mut first = vec![image_block(&data)];
        assert_eq!(budget.apply(&mut first), 500);
        assert!(image_data_of(&first[0]).is_some());

        let mut second = vec![image_block(&data)];
        assert_eq!(budget.apply(&mut second), 0, "重复图不得再占用预算");
        assert_eq!(image_data_of(&second[0]), None, "重复图不应第二次进上下文");
        assert!(
            text_of(&second[0]).unwrap().contains("同一张图"),
            "提示要说明是重复，不是超预算"
        );

        // 不同的图仍能进（证明去重没有误杀）。
        let mut third = vec![image_block(&"D".repeat(500))];
        assert_eq!(budget.apply(&mut third), 500, "不同图必须计入");
        assert!(image_data_of(&third[0]).is_some());
    }

    /// 端到端接线：`dev__read` 真的走了**带预算的 Guard**。
    ///
    /// 为何必须有这条：上面三条是直接调 `apply()` 的 —— 把 `dev_tools` 里的接线改回
    /// `read_guard.clone()`（即不装预算），那三条**照样全绿**。这条才会红。
    #[tokio::test]
    async fn read_tool_carries_image_budget_guard() {
        let workspace = temp_workspace("img-budget-e2e");
        let mut img = image::RgbImage::new(400, 400);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, 40, 200]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("编码 PNG");
        std::fs::write(workspace.join("pic.png"), png.into_inner()).expect("写测试图");

        let read = dev_tools(&workspace, SandboxMode::FullAccess, false, &[])
            .into_iter()
            .find(|t| t.schema().name == "dev__read")
            .expect("read 工具必须存在");

        // 默认预算（32MB）下小图应正常进上下文 —— 证明预算没误伤正常路径。
        let out = read
            .execute(
                "call",
                serde_json::json!({ "path": "pic.png" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .expect("读图应成功");
        assert!(
            out.content.iter().any(|b| image_data_of(b).is_some()),
            "默认预算下小图应正常进上下文"
        );

        // 同一张图再读一次：去重生效 ⇒ 不再出现 Image part（证明 Guard 真的装了预算）。
        let again = read
            .execute(
                "call",
                serde_json::json!({ "path": "pic.png" }),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .expect("第二次读图也应成功");
        assert!(
            !again.content.iter().any(|b| image_data_of(b).is_some()),
            "同一会话重复读同一张图，第二次不应再以 Image 进上下文（否则预算没接线）"
        );
    }

    /// 环境值解析：默认 32MB；`0` = 不限；非法值回默认（不悄悄关保护）。
    #[test]
    fn image_budget_env_parsing() {
        assert_eq!(image_budget_from(None), Some(IMAGE_BUDGET_DEFAULT_BYTES));
        assert_eq!(image_budget_from(Some("0")), None, "0 = 不限（排障退路）");
        assert_eq!(image_budget_from(Some(" 0 ")), None, "两侧空白应被容忍");
        assert_eq!(image_budget_from(Some("1048576")), Some(1_048_576));
        assert_eq!(
            image_budget_from(Some("not-a-number")),
            Some(IMAGE_BUDGET_DEFAULT_BYTES),
            "非法值回默认（这不是安全闸，错误不能悄悄关掉保护）"
        );
    }

    /// 开关语义（评审 O3：AC1 的"可配置"这一腿）：默认开；`"0"` 关；非法值按**开**处理（这里不是安全闸）。
    #[test]
    fn image_processor_switch_defaults_on_and_zero_disables() {
        assert!(image_processor_from(None).is_some(), "未设置 ⇒ 默认开");
        assert!(image_processor_from(Some("1")).is_some());
        assert!(image_processor_from(Some("0")).is_none(), "值为 0 ⇒ 关");
        assert!(
            image_processor_from(Some(" 0 ")).is_none(),
            "两侧空白应被容忍"
        );
        assert!(
            image_processor_from(Some("yes")).is_some(),
            "非法值按开处理（本开关无安全含义，不该悄悄关掉能力）"
        );
    }

    /// **接线判据**：`read` 工具必须**真的用** `read_tool_options()` 构造（只测 helper 不够——
    /// 把 `Some(read_tool_options())` 改回 `None`，helper 用例照样绿；这条就是为那次变异补的）。
    ///
    /// 手法：源码文本断言 + **运行期拼针**。为什么拼针：字面量会出现在本测试自身源码里，而
    /// `include_str!` 把测试一起读进来 ⇒ 直接写 `contains("…")` 会恒真（我在 harness 那边踩过一次。
    /// 边界（**按评审实测更正**）：这是源码级判据，边界是词法范围而非调用图——评审给出过一条实测绕过：
    /// 把生产接线改回 `None`、同时把 needle 原文留在尾注释里，本用例仍绿。**真正的行为级判据见下面
    /// `read_tool_returns_resized_image_part_end_to_end`**（走真实 `dev__read` 工具），本条只当"接线还在"的快速提示。
    ///
    /// 我原先在这里写过"要行为级判据得给 `build_tools` 注入假 ExecutionEnv，属更大改造"——**两句都不实**：
    /// ①全仓没有 `build_tools` 这个符号（真实是 `dev_tools`）；②**不需要假 env**：`dev_tools` 自己就
    /// `OsExecutionEnv::with_cwd(workspace)`，给个真实临时目录即可（评审用 ~45 行探针跑通并给了实测数据）。
    #[test]
    fn read_tool_is_constructed_with_processor_options() {
        let src = include_str!("builtin.rs").replace("\r\n", "\n");
        let needle = format!(
            "create_read_tool(&context, Some({}()))",
            "read_tool_options"
        );
        assert!(
            src.contains(&needle),
            "read 工具必须用 read_tool_options() 构造（否则图片处理不会被接上）"
        );
    }

    /// 行为判据：接上的处理器真的会把**超大图**缩到上限内（`INLINE_IMAGE_MAX_SIDE` = 2000）。
    /// 用 `image` 合成 2500×100 的 PNG 后过一遍处理器，再解码回来量尺寸——
    /// 断言"仍能拿到图（不是一行省略提示）且宽 ≤ 上限"。
    /// 判别力（**按评审实测更正**）：本条只覆盖**被调的库实现**——换掉处理器实现/不装处理器时它**不会**红
    /// （实测：变异 B/C 下它绿）。本仓接线由端到端用例 `read_tool_returns_resized_image_part_end_to_end` 覆盖。
    #[test]
    fn inline_image_processor_resizes_oversized_image() {
        use base64::Engine as _;
        use rpi_tools::ReadImageProcessor as _;
        // 造一张 2500×100 的红色 PNG（超过 2000 边长上限）
        let mut img = image::RgbImage::new(2500, 100);
        for px in img.pixels_mut() {
            *px = image::Rgb([200, 30, 30]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("编码 PNG");
        let raw = png.into_inner();

        let out = rpi_tools::image_processing::InlineImageProcessor
            .process(&raw, "image/png", true)
            .expect("处理器应成功处理 PNG");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(out.data.as_bytes())
            .expect("返回的应是 base64");
        let decoded = image::load_from_memory(&bytes).expect("解码处理后的图");
        assert!(
            decoded.width() <= rpi_tools::image_processing::INLINE_IMAGE_MAX_SIDE,
            "超限图必须被缩到上限内，实际宽 {}",
            decoded.width()
        );
        // 等比：原图 2500×100（比 25.0），缩放后宽被压到 2000 ⇒ 高应约 80，比例保持。
        let ratio = decoded.width() as f64 / decoded.height() as f64;
        assert!(
            (ratio - 25.0).abs() < 0.5,
            "应等比缩放（原比 25.0，实际 {ratio:.2}：{}×{}）",
            decoded.width(),
            decoded.height()
        );
    }

    /// 行为判据：BMP 不再"已省略"，而是被转成可 inline 的格式（PNG）。
    #[test]
    fn inline_image_processor_converts_bmp_to_png() {
        use rpi_tools::ReadImageProcessor as _;
        let mut img = image::RgbImage::new(8, 8);
        for px in img.pixels_mut() {
            *px = image::Rgb([10, 200, 10]);
        }
        let mut bmp = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut bmp, image::ImageFormat::Bmp)
            .expect("编码 BMP");
        let out = rpi_tools::image_processing::InlineImageProcessor
            .process(&bmp.into_inner(), "image/bmp", true)
            .expect("处理器应能转码 BMP");
        assert_eq!(out.mime_type, "image/png", "BMP 应被转成 PNG（而不是省略）");
    }
}
