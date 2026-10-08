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

/// 单个工具结果的字节预算：`total` 管全部内容（文本 + 图片），`text` 管**整条结果**里的文本。
///
/// 取值对齐被替代组件的生产默认（`MAX_TOOL_RESULT_BYTES = 8 MiB`、
/// `DEFAULT_TOOL_RESULT_TEXT_BYTES = 50 KiB`）。两个额度都**累计**（与 fork 的 `used`/`text_used`
/// 同义）：只按“每段”算的话，多段交替内容能线性堆出任意大的结果。
const TOOL_RESULT_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const TOOL_RESULT_TEXT_BYTES: usize = 50 * 1024;

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
    pub async fn connect_all(
        specs: &[McpServerSpec],
        cwd: &str,
        images_deliverable: bool,
    ) -> McpSession {
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
                            images_deliverable,
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
    // 白名单在 `child_env`（MCP 子进程与内置工具的 shell 共用同一份）。
    crate::child_env::apply_passthrough_env(&mut cmd);
    for env in &spec.env {
        cmd.env(&env.name, &env.value);
    }
    // 工作目录 = 会话工作区（abb 在 `session/new` 里下发的 `cwd`）。不设的话子进程落在
    // abb-agent 自己的 cwd 上，工具做相对路径 / git 操作会静默打错目标。
    if !cwd.is_empty() {
        // 先验一次：不验的话，cwd 不存在时 spawn 的报错是「No such file or directory」，
        // 会被读成「命令不存在」，排查时指错方向。
        if !std::path::Path::new(cwd).is_dir() {
            return Err(format!("会话工作区不是目录：cwd={cwd}"));
        }
        cmd.current_dir(cwd);
    }
    // 关掉继承来的 stdin/stdout 语义：MCP 的 stdio 传输要独占这三个管道，
    // 而本进程自己的 stdout 是 ACP 协议通道——绝不能让子进程写进来。
    // stderr 例外：**继承**（被替代组件同款）。落到 /dev/null 的话，server 起不来或报错的
    // 原因就只剩我们这一句 spawn 错误，服务端自己打印的原因全丢。
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    let transport = TokioChildProcess::new(cmd)
        .map_err(|error| format!("spawn {} (cwd={cwd}): {error}", spec.command))?;
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
    /// 本会话的 provider 能否**真的**把图片交给模型（见
    /// `provider::Backend::delivers_tool_result_images`）。不能时图片降级成**如实**的一行文本，
    /// 而不是交给 rpi 换成 `(see attached image)` 那种「指着一张不存在的图」的占位。
    images_deliverable: bool,
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
            // 图片要么真发出去，要么以**如实**的一行文本交代（取决于本会话 provider 的
            // agent 层能不能发图）——两种都不会出现「图没附上却说 attached」的占位文本。
            content: tool_result_content(&result.content, self.images_deliverable),
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
/// - 当 `images_deliverable` 为真时，图片保留成真 image part（rpi 会把它交给模型）；
///   为假时降级成**如实**的一行文本（“未传给模型”），而**不是**交给 rpi 换成
///   `(see attached image)` / `(tool image omitted: …)` 这类与事实不符的占位。
/// - 文本与图片**合计**受 [`TOOL_RESULT_TOTAL_BYTES`] 约束，整条结果的文本另受
///   [`TOOL_RESULT_TEXT_BYTES`] **累计**约束；超预算的部分降级成一行说明（不静默丢）。
/// - rpi 侧没有对应 part 的类型（音频等）以一行说明保留痕迹。
fn tool_result_content(
    content: &[rmcp::model::Content],
    images_deliverable: bool,
) -> Vec<TextContentOrImage> {
    tool_result_content_with(
        content,
        images_deliverable,
        TOOL_RESULT_TOTAL_BYTES,
        TOOL_RESULT_TEXT_BYTES,
    )
}

/// 预算可注入的版本：单测用极小的额度就能锁住「文本计入合计」与「文本额度累计」两件事，
/// 不必真造 8 MiB 的 fixture（造小了就锁不住——评审实测过这种“空锁”）。
fn tool_result_content_with(
    content: &[rmcp::model::Content],
    images_deliverable: bool,
    total_bytes: usize,
    text_bytes: usize,
) -> Vec<TextContentOrImage> {
    let mut out: Vec<TextContentOrImage> = Vec::new();
    let mut text = String::new();
    // `used` 与 `text_used` 都是**累计**的：只按“每段”算的话，多段交替内容能线性堆出任意大
    // 的工具结果（评审实测 `[image, 200 KiB text] × 300` ⇒ 15.3 MB 真的被发出去）。
    let mut used = 0usize;
    let mut text_used = 0usize;
    let mut truncated = false;
    for item in content {
        if used >= total_bytes {
            truncated = true;
            break;
        }
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
                flush_text(
                    &mut out,
                    &mut text,
                    &mut used,
                    &mut text_used,
                    total_bytes,
                    text_bytes,
                );
                let data = value
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mime_type = value
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream");
                let bytes = data.len() + mime_type.len();
                if !images_deliverable {
                    // 如实交代，不交给 rpi 去编一个与事实不符的占位。
                    push_line(
                        &mut text,
                        &format!(
                            "[image 未传给模型：{mime_type}，{} 字节 base64（本会话的 provider agent 层搬不动工具结果图片）]",
                            data.len()
                        ),
                    );
                } else if used.saturating_add(bytes) <= total_bytes {
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
    flush_text(
        &mut out,
        &mut text,
        &mut used,
        &mut text_used,
        total_bytes,
        text_bytes,
    );
    if truncated {
        out.push(TextContentOrImage::text(format!(
            "[... 工具结果已达总预算（{total_bytes} 字节），其余内容已省略 ...]"
        )));
    }
    out
}

/// 累积的文本按行拼接，收尾时按**文本额度（累计）**与**合计预算剩余**的较小者省略中间。
fn flush_text(
    out: &mut Vec<TextContentOrImage>,
    text: &mut String,
    used: &mut usize,
    text_used: &mut usize,
    total_bytes: usize,
    text_bytes: usize,
) {
    if text.is_empty() {
        return;
    }
    let allowance = text_bytes
        .saturating_sub(*text_used)
        .min(total_bytes.saturating_sub(*used));
    let piece = elide_middle(text, allowance);
    text.clear();
    if piece.is_empty() {
        // 额度用尽时不丢一块空的 text part（与被替代组件的 `if !kept.is_empty()` 同义）：
        // 空片段只会白占模型上下文里的位置。
        return;
    }
    *used = used.saturating_add(piece.len());
    *text_used = text_used.saturating_add(piece.len());
    out.push(TextContentOrImage::text(piece));
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
        let parts = tool_result_content(&[rmcp::model::Content::text("看图"), image], true);
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

    /// 回归锁（第一把）：文本必须计入**合计**预算。
    ///
    /// 形状要**让合计预算成为唯一的约束**，否则这把锁又是空的（上一版用「总预算 1000 /
    /// 文本额度 800」，两种实现只差 22 字节，被断言余量淹了——评审用「单独去掉文本计入」
    /// 的反例证实过）。现在：文本额度给得很宽（5000）、合计只有 1000、文本裸和 3000 ⇒
    /// 只有「文本计入合计」的实现会收在 1000 附近；不记的实现会堆出 ~3000。
    #[test]
    fn total_budget_counts_text_towards_the_total() {
        let mut content = Vec::new();
        for _ in 0..3 {
            content.push(rmcp::model::Content::text("T".repeat(1000)));
            content.push(
                serde_json::from_value::<rmcp::model::Content>(serde_json::json!({
                    "type": "image",
                    "data": "AA",
                    "mimeType": "image/png",
                }))
                .expect("image 内容应能反序列化"),
            );
        }
        let parts = tool_result_content_with(&content, true, 1000, 5000);
        let total = parts_bytes(&parts);
        assert!(
            total <= 1000 + 256,
            "文本必须计入合计预算：{total} 明显超过 1000"
        );
    }

    /// 回归锁（第二把）：文本额度是**整条结果累计**的，不是“每段都有一份”。
    /// 旧实现下 20 段 × 60 字节 ≈ 1200 字节文本会全部留下（而额度声称 800）。
    #[test]
    fn text_budget_is_cumulative_across_segments() {
        let mut content = Vec::new();
        for _ in 0..20 {
            content.push(
                serde_json::from_value::<rmcp::model::Content>(serde_json::json!({
                    "type": "image",
                    "data": "AA",
                    "mimeType": "image/png",
                }))
                .expect("image 内容应能反序列化"),
            );
            content.push(rmcp::model::Content::text("T".repeat(60)));
        }
        // 总预算给得很宽，只让文本额度起作用。
        let parts = tool_result_content_with(&content, true, 64 * 1024, 800);
        let text_total: usize = parts
            .iter()
            .map(|part| match part {
                TextContentOrImage::Text(text) => text.text.len(),
                TextContentOrImage::Image(_) => 0,
            })
            .sum();
        assert!(
            text_total <= 800 + 256,
            "文本额度必须累计：{text_total} 超过 800"
        );
    }

    /// 图片不可交付时（例如 anthropic：rpi 的 agent 层搬不动工具结果图片），必须留下
    /// **如实**的一行文本，而不是交给 rpi 换成 `(see attached image)` 那种与事实不符的占位。
    #[test]
    fn images_not_deliverable_leave_an_honest_note() {
        let image: rmcp::model::Content = serde_json::from_value(serde_json::json!({
            "type": "image",
            "data": "aGVsbG8=",
            "mimeType": "image/png",
        }))
        .expect("image 内容应能反序列化");
        let parts = tool_result_content(&[rmcp::model::Content::text("看图"), image], false);
        let text: String = parts
            .iter()
            .filter_map(|part| match part {
                TextContentOrImage::Text(text) => Some(text.text.clone()),
                TextContentOrImage::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!parts
            .iter()
            .any(|p| matches!(p, TextContentOrImage::Image(_))));
        assert!(text.contains("image 未传给模型"), "要如实交代：{text}");
        assert!(
            !text.contains("attached"),
            "不能出现与事实不符的占位：{text}"
        );
    }

    /// 回归锁（**走真路径**）：两个预算常量必须接到正确的参位上。
    ///
    /// 两把小锁都直接调 `tool_result_content_with`，于是「`TOOL_RESULT_TOTAL_BYTES` /
    /// `TOOL_RESULT_TEXT_BYTES` 在真路径里写反」不会被任何测试发现（评审指出真路径常量
    /// 零断言覆盖）。这条把两个方向都钉住：
    /// - 文本 200 KiB ⇒ 真路径必须把它裁到 **50 KiB** 量级（若文本额度被写成了 8 MiB，会原样留下 ⇒ 红）；
    /// - 图片 6 MiB ⇒ 真路径必须**当作图片保留**（若合计额度被写成了 50 KiB，会被降级成一行文本 ⇒ 红）。
    #[test]
    fn real_path_wires_both_budgets_to_the_right_slots() {
        let text = rmcp::model::Content::text("T".repeat(200 * 1024));
        let parts = tool_result_content(&[text], true);
        let text_len = parts_bytes(&parts);
        assert!(
            text_len <= TOOL_RESULT_TEXT_BYTES + 256,
            "真路径的文本额度应是 50 KiB 量级，实际 {text_len}"
        );
        assert!(
            text_len > TOOL_RESULT_TEXT_BYTES / 2,
            "不能把预算写小到把正文全丢掉：{text_len}"
        );

        let image = serde_json::from_value::<rmcp::model::Content>(serde_json::json!({
            "type": "image",
            "data": "A".repeat(6 * 1024 * 1024),
            "mimeType": "image/png",
        }))
        .expect("image 内容应能反序列化");
        let parts = tool_result_content(&[image], true);
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, TextContentOrImage::Image(_))),
            "6 MiB 的图片必须落在合计额度（8 MiB）内并被保留"
        );
    }

    fn parts_bytes(parts: &[TextContentOrImage]) -> usize {
        parts
            .iter()
            .map(|part| match part {
                TextContentOrImage::Text(text) => text.text.len(),
                TextContentOrImage::Image(image) => image.data.len() + image.mime_type.len(),
            })
            .sum()
    }

    #[test]
    fn audio_content_keeps_a_visible_trace() {
        let audio: rmcp::model::Content = serde_json::from_value(serde_json::json!({
            "type": "audio",
            "data": "aGVsbG8=",
            "mimeType": "audio/wav",
        }))
        .expect("audio 内容应能反序列化");
        let parts = tool_result_content(&[audio], true);
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
