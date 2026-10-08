//! ACP 方法分派。
//!
//! 第一刀的范围（见 walgit 线程 `abb-agent-rpi-acp-20261008`「第一刀」）：
//! `initialize` / `session/new` / `session/prompt` / `session/cancel`，以及
//! 逐条 `session/update` 的 `agent_message_chunk` 回文本。目标是「**能起、
//! 能登记、能回文本**」，用来在不动 `src/buzz/**` 的前提下量真实缺口。
//!
//! ### 刻意不做的事（都是决策，不是遗漏）
//!
//! - **不声明 `_meta.abbSandbox`**：本 agent 不实现任何 OS 沙箱档位。abb 侧
//!   `parse_abb_sandbox_modes` 因此得到 `None` → `SandboxSupport::Unsupported`，
//!   受限（granted）会话会被 fail-closed 拒答。这是**如实声明**：声明支持却不
//!   执行，正是 abb 注释里点名的事故形态。
//! - **不发 `session/request_permission`**：授权约定由 AGENTS.md 承担。
//! - **不发 `tool_call` / `tool_call_update`**：留第二刀（第一刀只验协议与回文本）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use rpi_agent::{Agent, AgentBuilder, AgentEvent, AgentMessage};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::Mutex;

use crate::provider::{self, Backend};
use crate::wire::{Inbound, WireError, Writer};

/// 系统提示（第一刀从简）。
///
/// TODO(第二刀)：接约定链——`~/AGENTS.md` 作为全局层 + cwd→root 逐级，
/// 技能目录对齐 `~/.agents/skills` / `<cwd>/.agents/skills`（buzz `hints.rs`
/// 的现状）。注意**不能**直接用 rpi 的默认位（`<agent_dir>/AGENTS.md` +
/// `~/.rpi/agent/skills`），否则既有用户的约定会静默失效。
const SYSTEM_PROMPT: &str = "你是 ABB（agent-bridge）的执行层 agent，在用户本机上运行。";

pub struct Server {
    writer: Writer,
    state: Mutex<State>,
    /// 装配结果在启动时定一次：供应商配错要在**起进程时**就可见，
    /// 而不是等第一条消息才炸。
    backend: Result<Backend, String>,
    seq: AtomicU64,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Session>,
}

#[derive(Clone)]
struct Session {
    agent: Arc<Agent>,
    cancelled: Arc<AtomicBool>,
}

impl Server {
    pub fn new(writer: Writer) -> Self {
        Self::with_backend(writer, provider::select())
    }

    /// 注入式构造：把后端选择与进程 env 解耦，便于测试直接给一个 faux 后端
    /// （不在测试里改进程全局 env——那会让并行用例互相踩）。
    pub fn with_backend(writer: Writer, backend: Result<Backend, String>) -> Self {
        if let Err(reason) = &backend {
            tracing::error!("provider 装配失败（会话建立时会如实报错）：{reason}");
        }
        Self {
            writer,
            state: Mutex::new(State::default()),
            backend,
            seq: AtomicU64::new(1),
        }
    }

    pub async fn dispatch(&self, inbound: Inbound) -> Result<(), WireError> {
        match inbound {
            Inbound::Request { id, method, params } => match method.as_str() {
                "initialize" => self.initialize(id).await,
                "session/new" => self.session_new(id).await,
                "session/prompt" => self.session_prompt(id, params).await,
                // -32601 = method not found（JSON-RPC 约定）。
                other => {
                    self.writer
                        .fail(id, -32601, format!("未实现的方法：{other}"))
                        .await
                }
            },
            Inbound::Notification { method, params } => {
                match method.as_str() {
                    "session/cancel" => self.session_cancel(&params).await,
                    // 未识别的通知按协议忽略（通知不产生应答，也无从报错）。
                    other => tracing::debug!("忽略未识别的通知：{other}"),
                }
                Ok(())
            }
        }
    }

    /// `initialize`：报协议版本与能力。
    ///
    /// `_meta.steering.supported = true` 是 abb 读的路径
    /// （`acp.rs`: `result.pointer("/_meta/steering/supported")`）——steering 是
    /// 打断/追加的 UX 能力，与权限无关，所以保留。
    async fn initialize(&self, id: Value) -> Result<(), WireError> {
        self.writer
            .respond(
                id,
                json!({
                    "protocolVersion": 2,
                    "agentCapabilities": { "loadSession": false },
                    "_meta": { "steering": { "supported": true } },
                }),
            )
            .await
    }

    async fn session_new(&self, id: Value) -> Result<(), WireError> {
        let session_id = format!("abb-{}", self.seq.fetch_add(1, Ordering::SeqCst));
        let agent = match self.build_agent(&session_id) {
            Ok(agent) => agent,
            Err(reason) => return self.writer.fail(id, -32000, reason).await,
        };
        self.state.lock().await.sessions.insert(
            session_id.clone(),
            Session {
                agent: Arc::new(agent),
                cancelled: Arc::new(AtomicBool::new(false)),
            },
        );
        tracing::info!("已建立会话 {session_id}");
        self.writer
            .respond(id, json!({ "sessionId": session_id }))
            .await
    }

    /// 驱动一个回合，并把回复文本按 abb 读的形状（`params.update.content.text`
    /// + `params.sessionId`）逐条发回。
    async fn session_prompt(&self, id: Value, params: Value) -> Result<(), WireError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(session) = self.state.lock().await.sessions.get(&session_id).cloned() else {
            return self
                .writer
                .fail(id, -32001, format!("未知会话：{session_id}"))
                .await;
        };
        // 新回合开始：清掉上一回合的取消标记。
        session.cancelled.store(false, Ordering::SeqCst);

        let prompt = prompt_text(&params);
        let mut events = session.agent.subscribe();
        let agent = Arc::clone(&session.agent);
        let writer = self.writer.clone();
        let turn_session = session_id.clone();

        let run = agent.prompt(prompt.as_str());
        tokio::pin!(run);

        let mut outcome = None;
        loop {
            tokio::select! {
                result = &mut run => {
                    outcome = Some(result);
                    break;
                }
                event = events.recv() => match event {
                    Ok(event) => {
                        if forward_event(&writer, &turn_session, &event).await.is_err() {
                            // 写不出去（父进程没了）：让回合继续跑完，由上层收尾。
                            tracing::warn!("session/update 写出失败，已忽略");
                        }
                    }
                    // Lagged = 事件积压被丢帧，不影响本回合结论，继续读。
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!("事件积压，已丢弃 {skipped} 条");
                    }
                    Err(RecvError::Closed) => break,
                },
            }
        }

        match outcome {
            Some(result) => {
                // 回合已结束，但广播缓冲里可能还留着本回合的尾部事件：`select!`
                // 两个分支同时就绪时若先取到 run，就会漏掉 MessageEnd，回复文本
                // 随之丢失（实测踩过：initialize/new/prompt 都对，但一条
                // session/update 也没发出去）。收尾前必须排空。
                loop {
                    match events.try_recv() {
                        Ok(event) => {
                            if forward_event(&writer, &turn_session, &event).await.is_err() {
                                tracing::warn!("session/update 写出失败，已忽略");
                            }
                        }
                        Err(TryRecvError::Lagged(skipped)) => {
                            tracing::warn!("事件积压，已丢弃 {skipped} 条");
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                    }
                }

                match result {
                    // 顺序敏感：`stopReason` 的值 abb 会解析（"end_turn" /
                    // "cancelled" 都是它认识的取值）。
                    Ok(()) => {
                        let stop = if session.cancelled.load(Ordering::SeqCst) {
                            "cancelled"
                        } else {
                            "end_turn"
                        };
                        self.writer.respond(id, json!({ "stopReason": stop })).await
                    }
                    Err(error) => {
                        self.writer
                            .fail(id, -32002, format!("回合失败：{error}"))
                            .await
                    }
                }
            }
            // 事件流先关闭而回合未返回：只在 agent 被打断时可能出现，如实报错。
            None => {
                self.writer
                    .fail(id, -32003, "回合被中断（事件流已关闭）")
                    .await
            }
        }
    }

    async fn session_cancel(&self, params: &Value) {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let session = self.state.lock().await.sessions.get(session_id).cloned();
        match session {
            Some(session) => {
                session.cancelled.store(true, Ordering::SeqCst);
                session.agent.abort();
                tracing::info!("会话 {session_id} 已请求取消");
            }
            None => tracing::warn!("取消未知会话 {session_id}"),
        }
    }

    fn build_agent(&self, session_id: &str) -> Result<Agent, String> {
        let backend = self.backend.as_ref().map_err(|reason| reason.clone())?;
        let model = backend.model()?;
        AgentBuilder::new()
            .model(model)
            .system_prompt(SYSTEM_PROMPT)
            .session_id(session_id)
            .stream_fn(backend.stream_fn())
            // TODO(第二刀)：接 rpi-tools 的内置工具（read/write/edit/bash/
            // grep/find/ls），env 用 OsExecutionEnv::with_cwd(工作目录)。
            .build()
            .map_err(|error| format!("agent 构建失败：{error}"))
    }
}

/// 把 agent 事件投影成 `session/update`。
///
/// 第一刀只投影 assistant 文本（`agent_message_chunk`）。刻意在 `MessageEnd`
/// 整条发、而不是逐 delta 发：buzz-agent 自己就是「Non-streaming」的
/// （见其 Cargo.toml 描述），abb 侧只是把 chunk 文本累加，整条发与逐字发对
/// 它等价，而逐 delta 需要再摸一层 `AssistantMessageEvent` 形状——第二刀再说。
async fn forward_event(
    writer: &Writer,
    session_id: &str,
    event: &AgentEvent,
) -> Result<(), WireError> {
    let AgentEvent::MessageEnd { message } = event else {
        return Ok(());
    };
    let Some(text) = assistant_text(message) else {
        return Ok(());
    };
    writer
        .notify(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": text },
                },
            }),
        )
        .await
}

/// 取 assistant 消息里的文本内容；非 assistant / 无文本返回 `None`。
fn assistant_text(message: &AgentMessage) -> Option<String> {
    let AgentMessage::Assistant(assistant) = message else {
        return None;
    };
    let text: String = assistant
        .content
        .iter()
        .filter_map(|content| match content {
            rpi_ai::types::Content::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

/// `session/prompt` 的 `params.prompt` 是内容块数组，取其中文本块拼起来。
fn prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_joins_text_blocks_and_skips_others() {
        let params = json!({
            "sessionId": "s1",
            "prompt": [
                { "type": "text", "text": "第一段" },
                { "type": "image", "data": "…" },
                { "type": "text", "text": "第二段" },
            ],
        });
        assert_eq!(prompt_text(&params), "第一段\n第二段");
    }

    #[test]
    fn prompt_text_is_empty_when_absent() {
        assert_eq!(prompt_text(&json!({})), "");
    }

    /// `initialize` 响应**不得**出现 `_meta.abbSandbox`：一旦声明，abb 会把
    /// granted 会话路由成「档位受限」并期待执行，而本 agent 不执行任何档位。
    #[tokio::test]
    async fn initialize_does_not_declare_abb_sandbox() {
        let (writer, mut captured) = crate::testing::capture_writer();
        let server = Server::new(writer);
        server
            .dispatch(Inbound::Request {
                id: json!(1),
                method: "initialize".into(),
                params: json!({ "protocolVersion": 2 }),
            })
            .await
            .expect("initialize 应成功");

        let line = captured.recv().await.expect("应写出一条应答");
        let value: Value = serde_json::from_str(&line).expect("应答是 JSON");
        assert_eq!(value["result"]["protocolVersion"], 2);
        assert!(
            value["result"]["_meta"].get("abbSandbox").is_none(),
            "不得声明 abbSandbox（本 agent 不实现 OS 沙箱档位）：{value}"
        );
        // steering 是 abb 读的路径，必须留着。
        assert_eq!(value["result"]["_meta"]["steering"]["supported"], true);
    }

    /// 未知方法回 -32601，且**不能** panic。
    #[tokio::test]
    async fn unknown_method_reports_method_not_found() {
        let (writer, mut captured) = crate::testing::capture_writer();
        let server = Server::new(writer);
        server
            .dispatch(Inbound::Request {
                id: json!(2),
                method: "session/does-not-exist".into(),
                params: json!({}),
            })
            .await
            .expect("分派本身应成功");
        let line = captured.recv().await.expect("应写出一条错误应答");
        let value: Value = serde_json::from_str(&line).expect("应答是 JSON");
        assert_eq!(value["error"]["code"], -32601);
    }

    /// 未建立会话就 prompt：结构化错误，不是 panic。
    #[tokio::test]
    async fn prompt_unknown_session_is_an_error() {
        let (writer, mut captured) = crate::testing::capture_writer();
        let server = Server::new(writer);
        server
            .dispatch(Inbound::Request {
                id: json!(3),
                method: "session/prompt".into(),
                params: json!({ "sessionId": "nope", "prompt": [] }),
            })
            .await
            .expect("分派本身应成功");
        let line = captured.recv().await.expect("应写出一条错误应答");
        let value: Value = serde_json::from_str(&line).expect("应答是 JSON");
        assert_eq!(value["error"]["code"], -32001);
    }

    /// **回归锁**：一个回合必须先把回复文本发成 `session/update`，再回
    /// `stopReason`。
    ///
    /// 第一版实现漏了这一条——`select!` 的「回合结果」与「事件到达」两个分支
    /// 同时就绪时，先取到结果就 `break`，广播缓冲里剩下的 `MessageEnd` 再也没人
    /// 转发，于是 initialize/new/prompt 全对、但用户收到空回复。故此处同时断言
    /// **内容**与**顺序**。
    #[tokio::test(flavor = "multi_thread")]
    async fn prompt_emits_message_chunk_before_stop_reason() {
        use rpi_ai::providers::faux::{FauxProvider, FauxScript};

        let (writer, mut captured) = crate::testing::capture_writer();
        let backend = Backend::Faux(FauxProvider::new(
            FauxScript::new().with_text("来自 faux 的回复"),
        ));
        let server = Server::with_backend(writer, Ok(backend));

        server
            .dispatch(Inbound::Request {
                id: json!(1),
                method: "session/new".into(),
                params: json!({}),
            })
            .await
            .expect("session/new 应成功");
        let created: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
        let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();
        assert!(!session_id.is_empty());

        server
            .dispatch(Inbound::Request {
                id: json!(2),
                method: "session/prompt".into(),
                params: json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "你好" }],
                }),
            })
            .await
            .expect("session/prompt 应成功");

        // 第一条出站：回复文本块（abb 读的是 params.update.content.text +
        // params.sessionId，两者都断）。
        let update: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
        assert_eq!(update["method"], "session/update");
        assert_eq!(update["params"]["sessionId"], session_id.as_str());
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(
            update["params"]["update"]["content"]["text"],
            "来自 faux 的回复"
        );

        // 第二条出站：回合结论。
        let done: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
        assert_eq!(done["result"]["stopReason"], "end_turn");
    }
}
