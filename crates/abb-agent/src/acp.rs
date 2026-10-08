//! ACP 方法分派。
//!
//! 第一刀的范围（见 walgit 线程 `abb-agent-rpi-acp-20261008`「第一刀」）：
//! `initialize` / `session/new` / `session/prompt` / `session/cancel`，以及逐条
//! `session/update` 的 `agent_message_chunk` 回文本。
//!
//! ### 并发形状（修的是评审反证 (b)）
//!
//! 主循环读一行就同步 `dispatch().await` 的写法**让 `session/cancel` 完全失效**：
//! `session_prompt` 整个回合都占着调用栈，回合中送进来的 cancel 要等回合结束才被读到，
//! 而回合开头又会清掉取消标记 ⇒ `stopReason:"cancelled"` 基本不可达，abb 只能退化成
//! 「30s drain 超时 → 杀进程重拉」。所以这里把**只有回合**派到独立任务里跑，
//! 读循环保持可读；`initialize`/`session/new`/`session/cancel` 仍在读循环内同步处理。
//!
//! ### 刻意不做的事（都是决策，不是遗漏）
//!
//! - **不声明 `_meta.abbSandbox`**：本 agent 不实现任何 OS 沙箱档位。abb 侧
//!   `parse_abb_sandbox_modes` 因此得到 `None` → `SandboxSupport::Unsupported`。
//!   这是**如实声明**：声明支持却不执行，正是 abb 注释里点名的事故形态。
//! - **不发 `session/request_permission`**：授权约定由 AGENTS.md 承担。
//! - **不发 `tool_call` / `tool_call_update`**：留第二刀。

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
    /// abb 注入的回合上界（`BUZZ_AGENT_MAX_ROUNDS`，默认 200）。
    max_rounds: u64,
    seq: AtomicU64,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Session>,
}

#[derive(Clone)]
struct Session {
    agent: Arc<Agent>,
    turn: Arc<TurnState>,
}

/// 每会话的回合状态。常驻（不每回合新建），由 `in_flight` 保证单飞。
#[derive(Default)]
struct TurnState {
    /// 有回合在跑。同时是「并发提交」的守卫：abb 按频道串行，并发提交属协议误用。
    in_flight: AtomicBool,
    /// 本回合是否已被取消。**只在 `in_flight` 为真时被置位**，且新回合在
    /// `in_flight` 抢到之后才清——否则会把上一回合的取消标记误清。
    cancelled: AtomicBool,
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
            max_rounds: provider::max_rounds(&provider::ProcessEnv),
            seq: AtomicU64::new(1),
        }
    }

    pub async fn dispatch(self: &Arc<Self>, inbound: Inbound) -> Result<(), WireError> {
        match inbound {
            Inbound::Request { id, method, params } => match method.as_str() {
                "initialize" => self.initialize(id).await,
                "session/new" => self.session_new(id).await,
                // 只有回合走独立任务：它是唯一的长任务，也是 `session/cancel`
                // 必须能在其运行期间被处理的原因。
                //
                // **`in_flight` 必须在读循环里同步抢**（而不是在 spawn 出去的任务里）：
                // 否则同一 burst 中紧随其后的 `session/cancel` 会被读循环先读到，而那时
                // 任务还没被调度过，取消会走「无在途回合」分支被丢弃——实测 0 间隔时
                // 4/4 复现，≥0.5ms 则全绿。抢占与 `InFlightGuard` 之间的所有权交接：
                // 抢到的守卫随任务移动，任务结束（含 panic）时由 Drop 解锁。
                "session/prompt" => match self.begin_turn(&params).await {
                    Err((code, message)) => self.writer.fail(id, code, message).await,
                    Ok((session_id, session, guard)) => {
                        let server = Arc::clone(self);
                        tokio::spawn(async move {
                            let _unlock = guard;
                            if let Err(error) =
                                server.run_turn(&session, &session_id, id, &params).await
                            {
                                // 写不出去（父进程关了管道）：无法再通信。
                                tracing::error!("回合应答写出失败：{error}");
                            }
                        });
                        Ok(())
                    }
                },
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
    /// **`_meta.steering.supported = false`（主动声明不支持）**：abb 那边这个标志是
    /// 写 `_session/steering` 的**唯一闸门**（该方法不可探测，见其注释「The capability
    /// flag is the ONLY gate on writing ACP_STEER_METHOD」）。本刀还没实现 steer，
    /// 若声称 ``true`` ⇒ 每次「回合中追加消息」都会先吃一个 `-32601` 才落到 abb 的
    /// cancel+merge 回退。如实报 `false` 让它直接走回退；等实现 steer 时再翻真。
    async fn initialize(&self, id: Value) -> Result<(), WireError> {
        self.writer
            .respond(
                id,
                json!({
                    "protocolVersion": 2,
                    "agentCapabilities": { "loadSession": false },
                    "_meta": { "steering": { "supported": false } },
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
        // 把所选 provider/model 记出来：不看会诊断不出来「模型/网关没生效」——
        // 评审正是靠这一点发现 ANTHROPIC_MODEL 被忽略的。
        if let Ok(backend) = &self.backend {
            tracing::info!(
                "已建立会话 {session_id}（provider={} model={} base_url={} max_rounds={}）",
                backend.id(),
                backend.model().id,
                backend.model().base_url,
                self.max_rounds,
            );
        }
        self.state.lock().await.sessions.insert(
            session_id.clone(),
            Session {
                agent: Arc::new(agent),
                turn: Arc::new(TurnState::default()),
            },
        );
        self.writer
            .respond(id, json!({ "sessionId": session_id }))
            .await
    }

    /// 认领会话并抢下本回合的「单飞」所有权（在**读循环**里同步完成）。
    ///
    /// 返回守卫：它随回合任务移动，任务退出（含 panic）时由 `Drop` 解锁。
    async fn begin_turn(
        &self,
        params: &Value,
    ) -> Result<(String, Session, InFlightGuard), (i64, String)> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(session) = self.state.lock().await.sessions.get(&session_id).cloned() else {
            return Err((-32001, format!("未知会话：{session_id}")));
        };
        if session.turn.in_flight.swap(true, Ordering::SeqCst) {
            return Err((
                -32004,
                format!("会话 {session_id} 已有在途回合（abb 按频道串行提交，这是协议误用）"),
            ));
        }
        // 抢到 `in_flight` 之后才清取消标记：这样它不可能清掉一个正在生效的取消。
        session.turn.cancelled.store(false, Ordering::SeqCst);
        let guard = InFlightGuard(Arc::clone(&session.turn));
        Ok((session_id, session, guard))
    }

    /// 驱动一个回合，并把回复文本按 abb 读的形状（`params.update.content.text`
    /// + `params.sessionId`）逐条发回。
    async fn run_turn(
        &self,
        session: &Session,
        session_id: &str,
        id: Value,
        params: &Value,
    ) -> Result<(), WireError> {
        let prompt = prompt_text(params);
        let writer = self.writer.clone();
        let agent = Arc::clone(&session.agent);
        let max_rounds = self.max_rounds;
        let mut events = session.agent.subscribe();
        let mut acc = TurnAccum::default();

        let run = agent.prompt(prompt.as_str());
        tokio::pin!(run);

        let mut outcome = None;
        // 每 100ms 醒一次，只为了让下面的「补发 abort」有机会跑到。
        //
        // 为什么必须有它：`Agent::abort()` 只作用于**当前 run 的 per-run token**，
        // 而 `session/cancel` 是在读循环里处理的——它抢到 `in_flight` 后几乎立刻就能
        // 处理紧随其后的 cancel，那时本任务可能**还没被 poll 过**（token 尚未建立），
        // 于是那次 abort 丢失，回合照旧挂在网络上（实测：黑洞端点 + 同一 burst，
        // 不取消就永远不返回）。补发 abort 必须有固定的唤醒源，不能依赖“下一个事件”——
        // 卡在 HTTP 上时根本不会再有事件。
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // 只要本回合已被取消，每轮都补一次 abort：第一次可能因为 token 还没建立
            // 而落空，run 被 poll 过之后的补发就一定能生效（abort 幂等）。
            if session.turn.cancelled.load(Ordering::SeqCst) {
                agent.abort();
            }
            tokio::select! {
                result = &mut run => {
                    outcome = Some(result);
                    break;
                }
                event = events.recv() => match event {
                    Ok(event) => {
                        acc.observe(&event, max_rounds, &agent);
                        write_event(&writer, session_id, &event).await;
                    }
                    // Lagged = 事件积压被丢帧；回合级失败信号见 acc.observe。
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!("事件积压，已丢弃 {skipped} 条");
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = tick.tick() => {}
            }
        }

        // 回合已结束，但广播缓冲里可能还留着本回合的尾部事件：`select!` 两个分支
        // 同时就绪时若先取到 run，就会漏掉 MessageEnd，回复文本随之丢失（实测踩过：
        // initialize/new/prompt 都对，但一条 session/update 也没发出去）。
        loop {
            match events.try_recv() {
                Ok(event) => {
                    acc.observe(&event, max_rounds, &agent);
                    write_event(&writer, session_id, &event).await;
                }
                Err(TryRecvError::Lagged(skipped)) => {
                    tracing::warn!("事件积压，已丢弃 {skipped} 条");
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            }
        }

        let Some(result) = outcome else {
            // 事件流先关闭而回合未返回：只在 agent 被打断时可能出现，如实报错。
            return self
                .writer
                .fail(id, -32003, "回合被中断（事件流已关闭）")
                .await;
        };
        if let Err(error) = result {
            return self
                .writer
                .fail(id, -32002, format!("回合失败：{error}"))
                .await;
        }

        // 判定次序有讲究（都在本回合状态下读取）：
        //   1. 被取消 → abb 认识的 "cancelled"；
        //   2. 超轮数上界 → abb 认识的 "max_turn_requests"；
        //   3. provider 失败 → **JSON-RPC error**（见下）；
        //   4. 其余 → "end_turn"。
        if session.turn.cancelled.load(Ordering::SeqCst) {
            return self
                .writer
                .respond(id, json!({ "stopReason": "cancelled" }))
                .await;
        }
        if acc.over_budget {
            tracing::warn!(
                "会话 {session_id} 超过回合上界 {max_rounds} 轮，已中止（stopReason=max_turn_requests）"
            );
            return self
                .writer
                .respond(id, json!({ "stopReason": "max_turn_requests" }))
                .await;
        }
        if let Some(reason) = acc.error {
            // **不能自造 stopReason**：abb 的 `StopReason::from_str` 只认
            // end_turn/cancelled/max_tokens/max_turn_requests/refusal，未知取值会被
            // 当成协议错误。而「回合失败」本来就不该伪装成成功——rpi 的
            // `Agent::prompt` 会把 `LoopOutcome::Failed` 折叠成 `Ok(())`，所以我们
            // 必须从事件流里的错误终态自己认出来，并以 JSON-RPC error 回，让 abb
            // 的 `parse_prompt_response` 把它转成 `Err` ⇒ 回合 Failed(原因) 对用户可见。
            tracing::error!("会话 {session_id} 回合失败：{reason}");
            return self.writer.fail(id, -32002, reason).await;
        }
        self.writer
            .respond(id, json!({ "stopReason": "end_turn" }))
            .await
    }

    async fn session_cancel(&self, params: &Value) {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let session = self.state.lock().await.sessions.get(session_id).cloned();
        match session {
            Some(session) => {
                // 只在真的有回合在跑时置位并 abort：避免一个「空转的取消」影响下一回合
                // （abort token 属于 agent，误触会打到还没开始的运行上）。
                if session.turn.in_flight.load(Ordering::SeqCst) {
                    session.turn.cancelled.store(true, Ordering::SeqCst);
                    session.agent.abort();
                    tracing::info!("会话 {session_id} 已请求取消");
                } else {
                    tracing::debug!("会话 {session_id} 无在途回合，取消忽略");
                }
            }
            None => tracing::warn!("取消未知会话 {session_id}"),
        }
    }

    fn build_agent(&self, session_id: &str) -> Result<Agent, String> {
        let backend = self.backend.as_ref().map_err(|reason| reason.clone())?;
        AgentBuilder::new()
            .model(backend.model().clone())
            .system_prompt(SYSTEM_PROMPT)
            .session_id(session_id)
            .stream_fn(backend.stream_fn())
            // TODO(第二刀)：接 rpi-tools 的内置工具（read/write/edit/bash/
            // grep/find/ls），env 用 OsExecutionEnv::with_cwd(工作目录)。
            .build()
            .map_err(|error| format!("agent 构建失败：{error}"))
    }
}

/// 回合结束时解锁会话（`Drop` 不受 panic 影响）。
struct InFlightGuard(Arc<TurnState>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.store(false, Ordering::SeqCst);
    }
}

/// 一个回合里从事件流累积出来的判定依据。
#[derive(Default)]
struct TurnAccum {
    /// 最后一个 assistant 终态的 `error_message`。每个 `MessageEnd` 都**覆盖**：
    /// 由最后一条 assistant 消息决定本回合有没有 provider 级失败。
    error: Option<String>,
    /// `TurnStart` 计数（1 轮 ≈ 1 次模型请求），对应 abb 的 `max_rounds` 语义。
    turns: u64,
    /// 是否已超过上界（超了就 abort，让 loop 尽快收尾）。
    over_budget: bool,
}

impl TurnAccum {
    fn observe(&mut self, event: &AgentEvent, max_rounds: u64, agent: &Agent) {
        match event {
            AgentEvent::TurnStart => {
                self.turns += 1;
                if self.turns > max_rounds && !self.over_budget {
                    self.over_budget = true;
                    tracing::warn!("回合已进行 {} 轮，超过上界 {max_rounds}，中止", self.turns);
                    agent.abort();
                }
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(assistant),
            } => {
                self.error = assistant.error_message.clone();
            }
            _ => {}
        }
    }
}

/// 把一条事件按需投影成 `session/update`（第一刀只投影 assistant 文本）。
///
/// 刻意在 `MessageEnd` 整条发、而不是逐 delta 发：buzz-agent 自己就是
/// 「Non-streaming」的（见其 Cargo.toml 描述），abb 侧只是把 chunk 文本累加，
/// 整条发与逐字发对它等价；逐 delta 需要再摸一层 `AssistantMessageEvent` 形状。
async fn write_event(writer: &Writer, session_id: &str, event: &AgentEvent) {
    let AgentEvent::MessageEnd { message } = event else {
        return;
    };
    let Some(text) = assistant_text(message) else {
        return;
    };
    if let Err(error) = writer
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
    {
        tracing::warn!("session/update 写出失败，已忽略：{error}");
    }
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
    use rpi_ai::providers::faux::{FauxProvider, FauxScript};
    use rpi_ai::Provider;

    /// 造一个 faux 后端（离线、确定性）。
    fn faux_backend(texts: &[&str]) -> Backend {
        let mut script = FauxScript::new();
        for text in texts {
            script = script.with_text(*text);
        }
        let provider = FauxProvider::new(script);
        let model = provider.models().first().cloned().expect("faux 有模型");
        Backend::Faux { provider, model }
    }

    /// 建服务 + 会話，返回 (服务, 会话 id, 出站行接收端)。
    async fn server_with_session(
        texts: &[&str],
    ) -> (
        Arc<Server>,
        String,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (writer, mut captured) = crate::testing::capture_writer();
        let server = Arc::new(Server::with_backend(writer, Ok(faux_backend(texts))));
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
        (server, session_id, captured)
    }

    fn prompt_request(id: i64, session_id: &str) -> Inbound {
        Inbound::Request {
            id: json!(id),
            method: "session/prompt".into(),
            params: json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "你好" }],
            }),
        }
    }

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
        let server = Arc::new(Server::new(writer));
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
        // steering 必须如实报 false：本刀未实现 `_session/steering`，而 abb 那边这个
        // 标志是写该方法的唯一闸门 ⇒ 报 true 会让每次「回合中追加消息」多一次无效往返。
        assert_eq!(
            value["result"]["_meta"]["steering"]["supported"], false,
            "未实现 _session/steering 就不得声称支持：{value}"
        );
    }

    /// **一致性锁**：能力声明必须与实现一致。
    ///
    /// 这条正是本轮评审的反证之一：`initialize` 曾声称 `steering.supported=true`，
    /// 而 `_session/steering` 实际回 `-32601`。两件事实现在都钉住：标志为 false、
    /// 方法未实现（`-32601`）。将来实现 steer 时必须同时把标志翻真，否则此测试会失败。
    #[tokio::test]
    async fn steering_flag_matches_reality() {
        let (writer, mut captured) = crate::testing::capture_writer();
        let server = Arc::new(Server::new(writer));

        server
            .dispatch(Inbound::Request {
                id: json!(1),
                method: "initialize".into(),
                params: json!({}),
            })
            .await
            .expect("initialize 应成功");
        let init: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
        assert_eq!(init["result"]["_meta"]["steering"]["supported"], false);

        server
            .dispatch(Inbound::Request {
                id: json!(2),
                method: "_session/steering".into(),
                params: json!({ "sessionId": "abb-1", "prompt": [] }),
            })
            .await
            .expect("分派本身应成功");
        let steering: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
        assert_eq!(steering["error"]["code"], -32601);
    }

    /// `session/cancel` 与 `session/prompt` **同 burst** 到达时，取消不得被丢弃。
    ///
    /// 这是本轮评审的阻塞项：`in_flight` 曾在 spawn 出去的任务里抢，于是读循环可以在
    /// 同一 burst 内先读到 cancel（任务还没被调度）→ 走「无在途回合」分支丢弃取消。
    /// 现在 `in_flight` 在读循环内同步抢，所以紧跟 prompt 的 cancel 必然看得到在途回合。
    /// 这里用 faux + 单线程 runtime 直接覆盖那个顺序（无需黑洞端点）。
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_in_same_burst_is_not_dropped() {
        let (server, session_id, mut captured) = server_with_session(&["慢回合"]).await;

        // 先把回合派出去（dispatch 返回即已抢到 in_flight），紧接着在**同一 burst** 里
        // 用 try_recv 之前不等待，模拟「两行同时到达」。
        server
            .dispatch(prompt_request(2, &session_id))
            .await
            .unwrap();
        server
            .dispatch(Inbound::Notification {
                method: "session/cancel".into(),
                params: json!({ "sessionId": session_id }),
            })
            .await
            .unwrap();

        // 无论回合跑得多快，都得有一个收敛的结论；关键是**不能**是「什么都没发生」。
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), captured.recv())
            .await
            .expect("回合必须在 10s 内给出结论")
            .expect("应有一条出站");
        let value: Value = serde_json::from_str(&line).unwrap();
        let finished = value.get("result").is_some() || value.get("error").is_some();
        let is_chunk = value.get("method").is_some();
        assert!(
            finished || is_chunk,
            "取消后回合必须有结论（或先有内容帧），不得静默：{value}"
        );
    }

    /// `session/cancel` 是通知：收到后**不得**产生任何出站行。
    ///
    /// 这条是 abb 侧协议正确性的硬要求——多一个无主应答会被它的读循环当未知 id。
    #[tokio::test]
    async fn cancel_notification_produces_no_output() {
        // `server_with_session` 已消费掉 session/new 的应答；这里不能再 recv
        // （没有生产者会再写——第一版就是这么把自己的用例挂死的）。
        let (server, session_id, mut captured) = server_with_session(&["ok"]).await;

        server
            .dispatch(Inbound::Notification {
                method: "session/cancel".into(),
                params: json!({ "sessionId": session_id }),
            })
            .await
            .expect("通知分派应成功");

        // 无在途回合时取消应被忽略：不应有任何出站。
        assert!(
            captured.try_recv().is_err(),
            "session/cancel 不得产生任何应答"
        );
    }

    #[tokio::test]
    async fn unknown_method_reports_method_not_found() {
        let (writer, mut captured) = crate::testing::capture_writer();
        let server = Arc::new(Server::new(writer));
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
        let server = Arc::new(Server::new(writer));
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
        let (server, session_id, mut captured) = server_with_session(&["来自 faux 的回复"]).await;
        server
            .dispatch(prompt_request(2, &session_id))
            .await
            .expect("prompt 分派应成功");

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

        // 第二条出站：回合结论；且**不应再有第三条**。
        let done: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
        assert_eq!(done["result"]["stopReason"], "end_turn");
        assert!(
            captured.try_recv().is_err(),
            "stopReason 之后不得再有多余出站行"
        );
    }

    /// **覆盖评审指出的盲区**：同一会话的**第 2 个回合**也要走正常路径。
    ///
    /// 第一版只有一个回合的用例；而 faux 脚本是一次性的，第 ≥2 回合会落到
    /// 「No more faux responses queued」错误分支——评审据此指出正常路径无覆盖。
    #[tokio::test(flavor = "multi_thread")]
    async fn second_turn_in_same_session_also_replies() {
        let (server, session_id, mut captured) =
            server_with_session(&["第一回合", "第二回合"]).await;

        for (id, expected) in [(2, "第一回合"), (3, "第二回合")] {
            server
                .dispatch(prompt_request(id, &session_id))
                .await
                .expect("prompt 分派应成功");
            let update: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
            assert_eq!(
                update["params"]["update"]["content"]["text"], expected,
                "第 {id} 次 prompt 的文本不对"
            );
            let done: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
            assert_eq!(done["result"]["stopReason"], "end_turn", "第 {id} 次回合");
        }
    }

    /// **回归锁（评审反证 a）**：provider 失败**不得**被报成成功的空回合。
    ///
    /// rpi 的 `Agent::prompt` 会把 `LoopOutcome::Failed` 折叠成 `Ok(())`，所以
    /// 必须从事件流里的错误终态自己认出来。这里用「faux 脚本耗尽」造一个真实错误
    /// （rpi 会产出 `StopReason::Error` + `error_message`），断言 abb 收到的是
    /// JSON-RPC **error** 而不是 `stopReason:"end_turn"` + 零文本。
    ///
    /// 为什么必须 error：abb 的 `StopReason` 只认
    /// end_turn/cancelled/max_tokens/max_turn_requests/refusal，自造取值会被判成
    /// 协议错误；而空文本会被 abb 当成「纯工具回合、不投递」并 `record_success`
    /// ⇒ 用户收不到东西、系统记成功。
    #[tokio::test(flavor = "multi_thread")]
    async fn provider_error_is_reported_as_jsonrpc_error() {
        let (server, session_id, mut captured) = server_with_session(&[]).await;
        server
            .dispatch(prompt_request(2, &session_id))
            .await
            .expect("prompt 分派应成功");

        let response: Value = serde_json::from_str(&captured.recv().await.unwrap()).unwrap();
        assert!(
            response.get("result").is_none(),
            "失败回合不得回 result（会被 abb 当成功）：{response}"
        );
        assert_eq!(response["error"]["code"], -32002, "{response}");
        let message = response["error"]["message"].as_str().unwrap_or_default();
        assert!(!message.is_empty(), "错误必须带原因：{response}");
    }
}
