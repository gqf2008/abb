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
use rpi_ai::types::{Schema, Tool};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 限定名分隔符。与被替代组件一致（它同样用 `__`，且拒绝 bare 名含 `__` 的工具）。
const SEP: &str = "__";

/// `initialize` / `tools/list` 的上界：卡死的 server 不能把 `session/new` 无限挂住。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

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
    /// 连一个 server 并列出它的工具。
    ///
    /// 单个 server 失败**不**拖垮整轮装配：记录告警后跳过（与 abb 对「一个坏插件不能
    /// 阻断其余」的取向一致）。返回的 `McpSession` 可能为空。
    pub async fn connect_all(specs: &[McpServerSpec]) -> McpSession {
        let mut servers = Vec::new();
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        for spec in specs {
            match connect_one(spec).await {
                Ok((client, defs)) => {
                    let client = Arc::new(client);
                    tracing::info!(
                        "MCP server {} 已连接，工具 {} 个：{:?}",
                        spec.name,
                        defs.len(),
                        defs.iter().map(|d| d.qualified.clone()).collect::<Vec<_>>()
                    );
                    for def in defs {
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
                Err(reason) => {
                    tracing::error!("MCP server {} 连接失败，已跳过：{reason}", spec.name)
                }
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

async fn connect_one(spec: &McpServerSpec) -> Result<(Client, Vec<ToolDef>), String> {
    let mut cmd = tokio::process::Command::new(&spec.command);
    cmd.args(&spec.args);
    for env in &spec.env {
        cmd.env(&env.name, &env.value);
    }
    // 关掉继承来的 stdin/stdout 语义：MCP 的 stdio 传输要独占这三个管道，
    // 而本进程自己的 stdout 是 ACP 协议通道——绝不能让子进程写进来。
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let transport =
        TokioChildProcess::new(cmd).map_err(|error| format!("spawn {}: {error}", spec.command))?;
    let client: Client = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
        .await
        .map_err(|_| format!("initialize 超时（{}s）", HANDSHAKE_TIMEOUT.as_secs()))?
        .map_err(|error| format!("initialize: {error}"))?;

    let listed = tokio::time::timeout(HANDSHAKE_TIMEOUT, client.peer().list_all_tools())
        .await
        .map_err(|_| format!("tools/list 超时（{}s）", HANDSHAKE_TIMEOUT.as_secs()))?
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
        if bare.is_empty() || bare.contains(SEP) {
            // 与被替代组件同一条规则：bare 名含分隔符会造成限定名歧义。
            tracing::warn!("跳过非法工具名 {bare:?}（空或含 {SEP}）");
            continue;
        }
        let description = raw
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let parameters = raw
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        let qualified = format!("{}{SEP}{}", spec.name, bare);
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
        let text = flatten_content(&result.content);
        if is_error {
            // 与被替代组件一致：MCP 的 isError 走错误路径，让 loop 编码成 error 工具结果。
            return Err(AgentError::Tool(if text.is_empty() {
                format!("{} 报告失败（无正文）", self.qualified)
            } else {
                text
            }));
        }
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(text)],
            details: Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        })
    }
}

/// 把 MCP 的 content 数组压成一段文本。
///
/// 文本直取；图片/音频/资源等非文本项**不丢**——以 JSON 片段形式附在后面，并标明类型，
/// 这样模型至少知道「这里有东西」，而不是无声吞掉。
fn flatten_content(content: &[rmcp::model::Content]) -> String {
    let mut out = String::new();
    for item in content {
        let value = match serde_json::to_value(item) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match kind {
            "text" => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            other => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{other} 内容] {value}"));
            }
        }
    }
    out
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
        assert_eq!(flatten_content(&content), "第一段\n第二段");
    }

    /// 非文本内容不得被静默吞掉——至少要留下一段可读的痕迹。
    #[test]
    fn flatten_keeps_non_text_content_visible() {
        // 用 serde 构造图片项：不依赖 rmcp 的构造函数签名。
        let image: rmcp::model::Content = serde_json::from_value(serde_json::json!({
            "type": "image",
            "data": "aGVsbG8=",
            "mimeType": "image/png",
        }))
        .expect("image 内容应能反序列化");
        let flattened = flatten_content(&[image]);
        assert!(flattened.contains("image"), "类型要留下痕迹：{flattened}");
        assert!(
            flattened.contains("aGVsbG8="),
            "数据要留下痕迹：{flattened}"
        );
    }

    #[test]
    fn type_name_covers_json_kinds() {
        assert_eq!(type_name(&Value::String("x".into())), "string");
        assert_eq!(type_name(&Value::Array(vec![])), "array");
        assert_eq!(type_name(&Value::Null), "null");
    }
}
