//! MCP 客户端：把 abb 通过 `session/new` 下发的 `mcpServers` 变成 rpi 的 `AgentTool`。
//!
//! 为什么必须有它：rpi **没有 MCP**（全仓零命中，`rpi-mcp` workspace 成员已被删除），
//! 而 abb 的 tool surface 主要就是两个 MCP server——
//! `abb-events`（= abb 二进制自身 `mcp-events` 子命令）与 `wassette`（Wasm 组件工具宿主）。
//! 不接 MCP 就等于把 abb 的能力面砍掉大半。
//!
//! 实现按 owner 决策用 `rmcp`（与被替代的 `crates/buzz-agent/src/mcp.rs` 同源），
//! 调用形状也沿用它的写法：`().serve(transport)` → `peer().list_all_tools()` /
//! `send_cancellable_request`。
//!
//! ## 工具命名
//!
//! 模型看到的名字是 **`{server}__{tool}`**（与被替代组件一致：其 `SEP` 即 `__`，
//! 且它明确拒绝 bare 名里含 `__` 的工具以避免歧义）。调用时回退到 bare 名发给 MCP。
//!
//! 名字/描述/schema 的上界与「重名怎么办」都对齐被替代的 `crates/buzz-agent/src/mcp.rs`
//! （它与 rpi 的工具表是同一份契约的消费方）。**有意保留一处差异**：那边遇到非法名字或
//! 重名是 `Err`（整轮 session 失败），这边是**记 ERROR 后跳过**——与 abb「一个坏插件不能
//! 阻断其余」同取向。差异写在 README 里，别读成「完全同一条规则」。
//!
//! ## 子进程环境
//!
//! MCP server 是本进程的孩子，**默认继承 abb-agent 的全量环境**（含供应商 API key）。
//! 这里对齐被替代组件的做法：`env_clear()` + 白名单（[`PASSTHROUGH_ENV`]）+ abb 按 server
//! 下发的 `spec.env`，并且工作目录取会话工作区（`session/new` 的 `cwd`）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{PeerRequestOptions, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{ImageContent, ImageContentType, Schema, Tool};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 限定名分隔符。与被替代组件一致（它同样用 `__`，且拒绝 bare 名含 `__` 的工具）。
const SEP: &str = "__";

/// 单个 server 的装配预算（`initialize` 与 `tools/list` **合计**共用这一个上界）。
const CONNECT_BUDGET_PER_SERVER: Duration = Duration::from_secs(20);

/// 一次 `session/new` 里**所有** server 的装配总预算。
///
/// 为什么必须有总预算：单 server 上界乘 server 数是无界的，而 abb 那一侧
/// `session/new` 的 RPC 超时是 60s、用户「停止」的宽限只有 5s
/// （`src/buzz/acp.rs` 的 `CONTROL_CANCEL_GRACE`）——装配必须在两者之内收尾。
/// 读循环本身已不再被装配占用（`session/new` 走独立任务），但应答仍须在 RPC 预算内。
const CONNECT_BUDGET_TOTAL: Duration = Duration::from_secs(30);

/// 名字 / 描述 / schema / 工具数的上界，逐条对齐被替代组件。
const MAX_NAME_LEN: usize = 128;
const MAX_QNAME_LEN: usize = 64;
const MAX_TOOLS_PER_SESSION: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 1024;
const MAX_SCHEMA_BYTES: usize = 4096;

/// 单个工具结果的字节预算：`total` 管全部内容（文本 + 图片），`text` 只管文本。
///
/// 取值对齐被替代组件的生产默认（`MAX_TOOL_RESULT_BYTES = 8 MiB`、
/// `DEFAULT_TOOL_RESULT_TEXT_BYTES = 50 KiB`）。文本是失控输出的来源（构建日志、文件
/// 转储），图片本身自限，所以只给文本单独设一个更紧的界。
const TOOL_RESULT_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const TOOL_RESULT_TEXT_BYTES: usize = 50 * 1024;

/// 传给 MCP 子进程的环境变量白名单（**先 `env_clear()` 再按此表注入**）。
///
/// 子工具需要的是「能不能出网、能不能用 git、临时目录在哪」，**不是**凭据。
/// 白名单内容与被替代组件同源（去掉那边 buzz 专属的几条）。
const PASSTHROUGH_ENV: &[&str] = &[
    // 基础
    "PATH",
    "HOME",
    "TERM",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    // SSH：git clone/push over SSH（git@github.com:…）要用
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    // Git：运维配置的 helper 与传输覆盖
    "GIT_ASKPASS",
    "GIT_SSH_COMMAND",
    "GIT_CONFIG_GLOBAL",
    // 代理：唯一出口是 CONNECT 代理的机器上，丢掉这几条不是「降级」而是让工具瞎掉
    // （apt/curl/pip/git 会直连，然后被出口防火墙 reset，看起来像「没有网络」）。
    // 大小写都要：curl/git 读小写，多数 Go/Python 工具读大写，而 libcurl 故意忽略
    // 大写的 `HTTP_PROXY`——只留一种会静默坏掉半个工具链。
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    // TLS 信任：终止 TLS 的代理自带 CA，镜像的信任库没有它 ⇒ 每次 https 都验签失败
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Windows 没有 $TMPDIR/$HOME：`std::env::temp_dir()` 看 TMP/TEMP，缺了就回落到子进程
/// 写不进去的 `C:\Windows`；USERPROFILE 是恒存在的兜底，APPDATA 带子工具配置。
#[cfg(windows)]
const PASSTHROUGH_ENV_WINDOWS: &[&str] = &["TMP", "TEMP", "USERPROFILE", "APPDATA"];

/// `env_clear()` + 白名单。调用方随后应用 `spec.env`（它优先级最高）。
fn apply_passthrough_env(cmd: &mut tokio::process::Command) {
    cmd.env_clear();
    for key in PASSTHROUGH_ENV {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    #[cfg(windows)]
    for key in PASSTHROUGH_ENV_WINDOWS {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
}

/// abb 在 `session/new` 里下发的 MCP server 规格。
///
/// 形状与 `src/buzz/acp.rs` 的 `McpServer`/`EnvVar` 逐字对应（那是唯一生产方）：
/// `{"name":…, "command":…, "args":[…], "env":[{"name":…,"value":…}]}`。
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerSpec {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<McpEnvVar>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpEnvVar {
    pub name: String,
    pub value: String,
}

/// 一个已连上的 server（客户端 + 它贡献的工具）。
struct Connected {
    name: String,
    /// **持有连接本身**：`RunningService` 一旦 drop 就会关掉那个子进程，而子进程是
    /// 工具能继续工作的前提。零工具（或全部被跳过的）server 没有 `McpTool` 持有它的
    /// clone，所以必须由这里抱住。
    #[allow(dead_code)]
    client: Arc<Client>,
}

type Client = RunningService<RoleClient, ()>;

/// 一次会话里连上的全部 MCP server + 展开后的工具。
pub struct McpSession {
    servers: Vec<Connected>,
    tools: Vec<Arc<dyn AgentTool>>,
}

impl McpSession {
    /// 连上所有 server 并展开工具。
    ///
    /// 单个 server 失败**不**拖垮整轮装配：记录告警后跳过（与 abb 对「一个坏插件不能
    /// 阻断其余」的取向一致）。返回的 `McpSession` 可能为空。
    ///
    /// 两个上界同时生效：单 server [`CONNECT_BUDGET_PER_SERVER`]、全体
    /// [`CONNECT_BUDGET_TOTAL`]（取小者）。`cwd` 是会话工作区，透传给子进程。
    pub async fn connect_all(specs: &[McpServerSpec], cwd: &str) -> McpSession {
        let mut servers = Vec::new();
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        let mut seen_qnames: Vec<String> = Vec::new();
        let deadline = tokio::time::Instant::now() + CONNECT_BUDGET_TOTAL;
        for spec in specs {
            // 非法 server 名会在限定名里制造歧义，而且 rpi 侧无法把它们和合法名区分开。
            if !valid_name(&spec.name) {
                tracing::error!("MCP server 名非法，整个 server 已跳过：{:?}", spec.name);
                continue;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let budget = remaining.min(CONNECT_BUDGET_PER_SERVER);
            if budget.is_zero() {
                tracing::error!(
                    "MCP server {} 未装配：全局装配预算（{}s）已耗尽，其余 server 一并跳过",
                    spec.name,
                    CONNECT_BUDGET_TOTAL.as_secs()
                );
                continue;
            }
            match tokio::time::timeout(budget, connect_one(spec, cwd)).await {
                Ok(Ok((client, defs))) => {
                    let client = Arc::new(client);
                    let mut accepted = Vec::new();
                    for def in defs {
                        if tools.len() + accepted.len() >= MAX_TOOLS_PER_SESSION {
                            tracing::error!(
                                "工具数已达上限 {MAX_TOOLS_PER_SESSION}，跳过 {} 及其余工具",
                                def.qualified
                            );
                            break;
                        }
                        // 跨 server 重名**必须可见**：rpi 按 `find` 取第一个匹配的工具，
                        // 重名会让后来者永不可达（被替代组件对重名是直接报错）。
                        if seen_qnames.iter().any(|seen| seen == &def.qualified) {
                            tracing::error!(
                                "工具名重复，跳过后者：{}（该名字已由先连上的 server 提供）",
                                def.qualified
                            );
                            continue;
                        }
                        seen_qnames.push(def.qualified.clone());
                        accepted.push(def);
                    }
                    tracing::info!(
                        "MCP server {} 已连接，工具 {} 个：{:?}",
                        spec.name,
                        accepted.len(),
                        accepted
                            .iter()
                            .map(|d| d.qualified.clone())
                            .collect::<Vec<_>>()
                    );
                    for def in accepted {
                        tools.push(Arc::new(McpTool {
                            qualified: def.qualified,
                            bare: def.bare,
                            schema: def.schema,
                            client: Arc::clone(&client),
                        }));
                    }
                    servers.push(Connected {
                        name: spec.name.clone(),
                        client,
                    });
                }
                // 如实可见：abb 能据此判断是「工具没接上」而不是模型不肯用。
                Ok(Err(reason)) => {
                    tracing::error!("MCP server {} 连接失败，已跳过：{reason}", spec.name)
                }
                Err(_) => tracing::error!(
                    "MCP server {} 连接失败，已跳过：装配超时（本 server 预算 {}s）",
                    spec.name,
                    budget.as_secs()
                ),
            }
        }
        McpSession { servers, tools }
    }

    /// 展开出来的工具（直接喂给 `AgentBuilder::tools`）。
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.clone()
    }

    pub fn server_names(&self) -> Vec<&str> {
        self.servers.iter().map(|s| s.name.as_str()).collect()
    }
}

/// 一个 server 的工具定义（rpi schema 已构造好）。
struct ToolDef {
    bare: String,
    qualified: String,
    schema: Tool,
}

async fn connect_one(spec: &McpServerSpec, cwd: &str) -> Result<(Client, Vec<ToolDef>), String> {
    let mut cmd = tokio::process::Command::new(&spec.command);
    cmd.args(&spec.args);
    // **先清空再白名单**：不经 `env_clear()` 的话，每个 MCP server（包括 wassette 这类
    // 第三方组件宿主）默认都能看到 abb 注入给 agent 的供应商 API key。
    // `spec.env` 是 abb 按 server 下发的（`AGENT_BRIDGE_HOME`、`ABB_EVENTS_REPO` 等），
    // 最后应用、优先级最高。
    apply_passthrough_env(&mut cmd);
    for env in &spec.env {
        cmd.env(&env.name, &env.value);
    }
    // 工作目录 = 会话工作区（abb 在 `session/new` 里下发的 `cwd`）。不设的话子进程落在
    // abb-agent 自己的 cwd 上，工具做相对路径 / git 操作会静默打错目标。
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    // 关掉继承来的 stdin/stdout 语义：MCP 的 stdio 传输要独占这三个管道，
    // 而本进程自己的 stdout 是 ACP 协议通道——绝不能让子进程写进来。
    // stderr 例外：**继承**（被替代组件同款）。落到 /dev/null 的话，server 起不来或报错的
    // 原因就只剩我们这一句 spawn 错误，服务端自己打印的原因全丢。
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    let transport =
        TokioChildProcess::new(cmd).map_err(|error| format!("spawn {}: {error}", spec.command))?;
    // 超时不在这里做：调用方用 [`CONNECT_BUDGET_PER_SERVER`] 包住整个装配，
    // 这样 initialize 花掉的时间会从 tools/list 的额度里扣——「每 server 20s」才是真话。
    let client: Client = ().serve(transport).await.map_err(|error| format!("initialize: {error}"))?;

    let listed = client
        .peer()
        .list_all_tools()
        .await
        .map_err(|error| format!("tools/list: {error}"))?;

    let mut defs = Vec::new();
    for tool in listed {
        // 用 serde 取值而不是直接读字段：rmcp 的 `Tool` 字段类型随版本变动过
        // （`Cow`/`Arc<JsonObject>` 等），而 JSON 形状是协议的一部分、稳定。
        let raw =
            serde_json::to_value(&tool).map_err(|error| format!("tool 序列化失败：{error}"))?;
        let bare = raw
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !valid_name(&bare) || bare.contains(SEP) {
            // 与被替代组件同一条判据（它那边是 `Err`，这里记 WARN 跳过——见模块文档）。
            tracing::warn!(
                "跳过非法工具名 {bare:?}（空、含 {SEP}、超长或含非 [A-Za-z0-9_-] 字符）"
            );
            continue;
        }
        let description = elide_middle(
            raw.get("description")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            MAX_DESCRIPTION_BYTES,
        );
        let parameters = cap_schema(
            &format!("{}{SEP}{}", spec.name, bare),
            raw.get("inputSchema")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
        );
        let qualified = format!("{}{SEP}{}", spec.name, bare);
        if qualified.len() > MAX_QNAME_LEN {
            tracing::warn!(
                "跳过工具 {qualified:?}：限定名 {} 字节超过上限 {MAX_QNAME_LEN}",
                qualified.len()
            );
            continue;
        }
        defs.push(ToolDef {
            qualified: qualified.clone(),
            bare,
            schema: Tool {
                // 模型看到的必须是**限定名**（否则同名工具在多 server 下无从区分，
                // 而 MCP 调用又必须用 bare 名——两者分开存）。
                name: qualified,
                description,
                parameters: Schema(parameters),
                constrained_sampling: None,
            },
        });
    }
    Ok((client, defs))
}

/// 一个 MCP 工具在 rpi 里的适配器。
struct McpTool {
    qualified: String,
    bare: String,
    schema: Tool,
    client: Arc<Client>,
}

#[async_trait]
impl AgentTool for McpTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.qualified
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        // MCP 只接受对象或省略；其它形状是畸形调用，本地就拒（不必打网络）。
        let arguments = match params {
            Value::Null => None,
            Value::Object(map) => Some(map),
            other => {
                return Err(AgentError::Tool(format!(
                    "{} 的参数必须是 JSON 对象，收到 {}",
                    self.qualified,
                    type_name(&other)
                )))
            }
        };

        let mut request = CallToolRequestParams::default();
        request.name = self.bare.clone().into();
        request.arguments = arguments;

        use rmcp::model::{CallToolRequest, ClientRequest, ServerResult};
        let call = ClientRequest::CallToolRequest(CallToolRequest::new(request));

        let mut handle = tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(AgentError::Abort),
            result = self
                .client
                .peer()
                .send_cancellable_request(call, PeerRequestOptions::no_options()) => {
                result.map_err(|error| AgentError::Tool(format!("{} 调用失败：{error}", self.qualified)))?
            }
        };

        let raw: Result<ServerResult, rmcp::ServiceError> = tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(AgentError::Abort),
            received = &mut handle.rx => received.map_err(|_| AgentError::Tool(format!("{} 传输已关闭", self.qualified)))?,
        };

        let result = match raw {
            Ok(ServerResult::CallToolResult(result)) => result,
            Ok(_) => {
                return Err(AgentError::Tool(format!(
                    "{} 返回了非预期的响应类型",
                    self.qualified
                )))
            }
            Err(error) => {
                return Err(AgentError::Tool(format!(
                    "{} 调用失败：{error}",
                    self.qualified
                )))
            }
        };

        let is_error = result.is_error.unwrap_or(false);
        if is_error {
            // 与被替代组件一致：MCP 的 isError 走错误路径，让 loop 编码成 error 工具结果。
            let text = flatten_text(&result.content);
            return Err(AgentError::Tool(if text.is_empty() {
                format!("{} 报告失败（无正文）", self.qualified)
            } else {
                text
            }));
        }
        Ok(AgentToolResult {
            // 图片保留成真 image part（rpi 会作为 image content 发给模型），
            // 不降级成 base64 正文。
            content: tool_result_content(&result.content),
            details: Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        })
    }
}

/// 把 MCP 的 content 数组压成一段文本（**错误路径**用：错误只能以文本回给 loop）。
///
/// 文本直取；图片/音频/资源等非文本项**不丢**——以 JSON 片段形式附在后面，并标明类型，
/// 这样模型至少知道「这里有东西」，而不是无声吞掉。整段受 [`TOOL_RESULT_TEXT_BYTES`]
/// 约束（中间省略，保留头尾）。
fn flatten_text(content: &[rmcp::model::Content]) -> String {
    let mut out = String::new();
    for item in content {
        let Ok(value) = serde_json::to_value(item) else {
            continue;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if kind == "text" {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                push_line(&mut out, text);
            }
            continue;
        }
        push_line(&mut out, &format!("[{kind} 内容] {value}"));
    }
    elide_middle(&out, TOOL_RESULT_TEXT_BYTES)
}

/// 把 MCP 的 content 数组转成 rpi 的工具结果内容。
///
/// 与被替代组件同构：
/// - **图片保留成真 image part**（`TextContentOrImage::Image`，rpi 侧会作为 image content
///   发给模型）。若降级成 `[image 内容] {base64}` 文本，模型看不到图，那串 base64 还会
///   永久留在会话历史里、每次请求重发。
/// - 图片合计受 [`TOOL_RESULT_TOTAL_BYTES`] 约束，文本受 [`TOOL_RESULT_TEXT_BYTES`]
///   约束；超预算的图片降级成一行说明（不静默丢）。
/// - rpi 侧没有对应 part 的类型（音频等）以一行说明保留痕迹。
fn tool_result_content(content: &[rmcp::model::Content]) -> Vec<TextContentOrImage> {
    let mut out: Vec<TextContentOrImage> = Vec::new();
    let mut text = String::new();
    let mut used = 0usize;
    for item in content {
        let Ok(value) = serde_json::to_value(item) else {
            continue;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match kind {
            "text" => {
                if let Some(part) = value.get("text").and_then(Value::as_str) {
                    push_line(&mut text, part);
                }
            }
            "image" => {
                flush_text(&mut out, &mut text);
                let data = value
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mime_type = value
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream");
                let bytes = data.len() + mime_type.len();
                if used.saturating_add(bytes) <= TOOL_RESULT_TOTAL_BYTES {
                    used += bytes;
                    out.push(TextContentOrImage::Image(ImageContent {
                        kind: ImageContentType,
                        data: data.to_owned(),
                        mime_type: mime_type.to_owned(),
                    }));
                } else {
                    push_line(
                        &mut text,
                        &format!(
                            "[image 已省略：{mime_type}，{} 字节 base64 超出工具结果预算]",
                            data.len()
                        ),
                    );
                }
            }
            other => push_line(&mut text, &format!("[{other} 内容] {value}")),
        }
    }
    flush_text(&mut out, &mut text);
    out
}

/// 累积的文本按行拼接，收尾时按文本预算省略中间。
fn flush_text(out: &mut Vec<TextContentOrImage>, text: &mut String) {
    if text.is_empty() {
        return;
    }
    out.push(TextContentOrImage::text(elide_middle(
        text,
        TOOL_RESULT_TEXT_BYTES,
    )));
    text.clear();
}

fn push_line(text: &mut String, line: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line);
}

/// 名字判据：非空、不超长、只含 ASCII 字母数字与 `_`/`-`（与被替代组件同值）。
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// schema 过大就换成空对象并告警（与被替代组件同处置）：免得把几十 KB 的 schema
/// 塞进每一次请求。
fn cap_schema(qualified: &str, schema: Value) -> Value {
    let size = serde_json::to_vec(&schema)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if size <= MAX_SCHEMA_BYTES {
        return schema;
    }
    tracing::warn!(
        "工具 {qualified} 的 inputSchema 有 {size} 字节（> {MAX_SCHEMA_BYTES}），替换为空对象"
    );
    Value::Object(serde_json::Map::new())
}

/// 头部/尾部保留、中间省略的截断。
///
/// 工具输出把身份放在开头、结论放在末尾（测试汇总、错误尾巴），只砍尾巴会把模型最需要的
/// 那半段砍掉；省略标记写明丢了多少，模型才知道该重跑一次更窄的命令，而不是相信这段空洞。
/// 按**字符边界**切，不按字节硬切（UTF-8 安全是仓库约定）。
fn elide_middle(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    // 省略标记本身约 60 字节，留足够余量让算术恒偏安全一侧。
    const MARKER_ALLOWANCE: usize = 128;
    let keep = max.saturating_sub(MARKER_ALLOWANCE);
    if keep == 0 {
        // 预算连标记都放不下：退化成只留头部。
        return truncate_at_boundary(text, max).to_owned();
    }
    let head = truncate_at_boundary(text, keep.div_ceil(2));
    let mut tail_start = text.len() - keep / 2;
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail = &text[tail_start..];
    let elided = text.len() - head.len() - tail.len();
    format!(
        "{head}\n[... 工具结果省略 {elided} / {} 字节 ...]\n{tail}",
        text.len()
    )
}

fn truncate_at_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_spec_parses_abb_shape() {
        // 与 src/buzz/acp.rs 的 McpServer 序列化形状逐字对应。
        let raw = serde_json::json!({
            "name": "abb-events",
            "command": "/Applications/ABB.app/Contents/MacOS/agent-bridge",
            "args": ["mcp-events"],
            "env": [{ "name": "ABB_EVENTS_REPO", "value": "/repo" }],
        });
        let spec: McpServerSpec = serde_json::from_value(raw).expect("应能解析");
        assert_eq!(spec.name, "abb-events");
        assert_eq!(spec.args, vec!["mcp-events"]);
        assert_eq!(spec.env[0].name, "ABB_EVENTS_REPO");
    }

    #[test]
    fn server_spec_tolerates_missing_optional_fields() {
        // 只有 name/command 也要能解析（args/env 有 default）。
        let raw = serde_json::json!({ "name": "x", "command": "/bin/x" });
        let spec: McpServerSpec = serde_json::from_value(raw).expect("应能解析");
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
    }

    #[test]
    fn flatten_text_content() {
        let content = vec![
            rmcp::model::Content::text("第一段"),
            rmcp::model::Content::text("第二段"),
        ];
        assert_eq!(flatten_text(&content), "第一段\n第二段");
    }

    /// 非文本内容不得被静默吞掉——至少要留下一段可读的痕迹（错误路径）。
    #[test]
    fn flatten_keeps_non_text_content_visible() {
        // 用 serde 构造图片项：不依赖 rmcp 的构造函数签名。
        let image: rmcp::model::Content = serde_json::from_value(serde_json::json!({
            "type": "image",
            "data": "aGVsbG8=",
            "mimeType": "image/png",
        }))
        .expect("image 内容应能反序列化");
        let flattened = flatten_text(&[image]);
        assert!(flattened.contains("image"), "类型要留下痕迹：{flattened}");
        assert!(
            flattened.contains("aGVsbG8="),
            "数据要留下痕迹：{flattened}"
        );
    }

    /// 成功路径：图片必须是**真 image part**，不是 base64 正文。
    #[test]
    fn image_content_stays_an_image_part() {
        let image: rmcp::model::Content = serde_json::from_value(serde_json::json!({
            "type": "image",
            "data": "aGVsbG8=",
            "mimeType": "image/png",
        }))
        .expect("image 内容应能反序列化");
        let parts = tool_result_content(&[rmcp::model::Content::text("看图"), image]);
        assert_eq!(parts.len(), 2, "文本 + 图片：{parts:?}");
        assert!(matches!(parts[0], TextContentOrImage::Text(_)));
        match &parts[1] {
            TextContentOrImage::Image(image) => {
                assert_eq!(image.data, "aGVsbG8=");
                assert_eq!(image.mime_type, "image/png");
            }
            other => panic!("图片必须保留成 image part，实际：{other:?}"),
        }
    }

    #[test]
    fn audio_content_keeps_a_visible_trace() {
        let audio: rmcp::model::Content = serde_json::from_value(serde_json::json!({
            "type": "audio",
            "data": "aGVsbG8=",
            "mimeType": "audio/wav",
        }))
        .expect("audio 内容应能反序列化");
        let parts = tool_result_content(&[audio]);
        match &parts[0] {
            TextContentOrImage::Text(text) => {
                assert!(text.text.contains("audio"), "要留下类型痕迹：{}", text.text)
            }
            other => panic!("audio 应降级成带痕迹的文本，实际：{other:?}"),
        }
    }

    #[test]
    fn valid_name_matches_replaced_component_rule() {
        assert!(valid_name("echo_query"));
        assert!(valid_name("a-b"));
        assert!(!valid_name(""));
        assert!(!valid_name("bad name"), "空格非法");
        assert!(!valid_name("工具名"), "非 ASCII 非法");
        assert!(!valid_name(&"a".repeat(MAX_NAME_LEN + 1)), "超长非法");
    }

    #[test]
    fn cap_schema_replaces_oversized_schema() {
        let small = serde_json::json!({ "type": "object" });
        assert_eq!(cap_schema("s__t", small.clone()), small);
        let big = serde_json::json!({ "type": "object", "description": "x".repeat(8192) });
        assert_eq!(
            cap_schema("s__t", big),
            serde_json::json!({}),
            "超限 schema 应换成空对象"
        );
    }

    /// 省略必须落在字符边界上（否则会 panic / 产生非法 UTF-8）。
    #[test]
    fn elide_middle_is_char_boundary_safe() {
        // 每个汉字 3 字节：预算 303 字节不在字符边界上（keep/2 = 87 不算，但 head 份额
        // 88 不是 3 的倍数），必须自行回落。
        let text = "中".repeat(200);
        let elided = elide_middle(&text, 303);
        assert!(elided.contains("省略"), "要带省略标记：{elided}");
        assert!(elided.starts_with("中") && elided.ends_with("中"));
        assert!(elided.len() < text.len(), "确实被截短了");
        // 预算连标记都放不下时退化成只留头部（仍然 UTF-8 安全，只是没有标记）。
        let head_only = elide_middle(&text, 64);
        assert!(head_only.len() <= 64, "不得超预算：{}", head_only.len());
        assert!(text.starts_with(&head_only));
        let short = "a".repeat(50);
        assert_eq!(elide_middle(&short, 1024), short, "未超预算要原样返回");
    }

    #[test]
    fn type_name_covers_json_kinds() {
        assert_eq!(type_name(&Value::String("x".into())), "string");
        assert_eq!(type_name(&Value::Array(vec![])), "array");
        assert_eq!(type_name(&Value::Null), "null");
    }
}
