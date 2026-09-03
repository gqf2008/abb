//! Agent pool — owns N AcpClient instances and dispatches prompt tasks.
//!
//! # Mental model
//!
//! ```text
//!   AgentPool
//!   ├── agents: Vec<Option<OwnedAgent>>   ← idle agents sit here
//!   ├── join_set: JoinSet<()>             ← in-flight tasks
//!   ├── task_map: HashMap<Id, TaskMeta>   ← panic recovery metadata
//!   └── result_tx/rx: mpsc channel        ← tasks return agents here
//!
//!   Dispatch:
//!     try_claim() → OwnedAgent (removed from slot)
//!     spawn run_prompt_task(agent, ...) into join_set
//!     task sends PromptResult { agent, outcome } via result_tx
//!     rx_and_join_set() → poll result_rx for PromptResult
//!     return_agent(agent) → puts agent back in slot
//! ```
//!
//! `AcpClient` is NOT Clone — ownership moves out on claim and back on return.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::buzz::acp::{AcpClient, AcpError, EnvVar, McpServer, StopReason, SystemPromptTransport};
use crate::buzz::queue::{CancelReason, DedupMode, FlushBatch, PromptChannelInfo};

/// Window within which agent activity before a hard-cap death qualifies
/// the turn as "recently active" (eligible for requeue instead of dead-letter).
const RECENT_ACTIVITY_WINDOW: Duration = Duration::from_secs(60);

// FlushBatch and BatchEvent derive Clone (added in queue.rs) so we can store
// a recoverable copy in TaskMeta for panic recovery in Queue mode.

/// Metadata stored per in-flight task for panic recovery.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SuccessfulSteerDelivery {
    pub event_id: String,
    pub session_id: String,
}

pub struct TaskMeta {
    pub agent_index: usize,
    pub channel_id: Option<Uuid>,
    /// Clone of batch for Queue mode panic recovery.
    pub recoverable_batch: Option<FlushBatch>,
    /// Control signal for the in-flight prompt task.
    /// `None` for heartbeat tasks (not controllable) and after signal is consumed.
    pub control_tx: Option<tokio::sync::oneshot::Sender<ControlSignal>>,
    /// Steer request channel for non-cancelling mid-turn delivery.
    /// Capacity-1; `try_send` from the main loop fails on `Full`/`Closed`,
    /// in which case the caller must fall back to the universal
    /// `ControlSignal::Steer` cancel+merge path. `None` for heartbeat
    /// tasks only — all prompt tasks install a steer channel regardless
    /// of the agent's name.
    pub steer_tx: Option<tokio::sync::mpsc::Sender<SteerRequest>>,
    /// Successful non-cancelling steers acknowledged while this task owned the
    /// live session. The session ID prevents a late ack from contaminating a
    /// replacement session after task return.
    pub successful_steer_deliveries: HashSet<SuccessfulSteerDelivery>,
}

/// Successful deliveries associated with one live channel session.
#[derive(Default)]
pub struct ChannelDeliveryState {
    /// Whether a legacy user message has successfully carried standing context.
    pub standing_context_sent: bool,
    /// Event IDs already delivered to this ACP session — the steer-ack ledger,
    /// written by `AgentPool::record_successful_steer` (idle path). The read
    /// side (conversation-context delta dedupe) was trimmed with the relay
    /// fetch layer in this port; the ledger is kept so the ack watcher and
    /// lib.rs handles have their durable record intact.
    #[allow(dead_code)]
    pub delivered_event_ids: HashSet<String>,
}

/// Per-channel session IDs, turn counters, and delivery state.
///
/// Separated from `OwnedAgent` so the state machine is testable without
/// spawning a real agent subprocess.
#[derive(Default)]
pub struct SessionState {
    /// channel_id → session_id
    pub sessions: HashMap<Uuid, String>,
    /// Per-channel turn counters for proactive session rotation.
    /// Incremented on each successful prompt; reset when the session is rotated.
    pub turn_counts: HashMap<Uuid, u32>,
    /// Per-channel successful-delivery state. Created with the ACP session and
    /// cleared atomically with every invalidation path.
    pub deliveries: HashMap<Uuid, ChannelDeliveryState>,
}

impl SessionState {
    /// Invalidate the session (and turn counter) for a specific prompt source.
    pub fn invalidate(&mut self, source: &PromptSource) {
        match source {
            PromptSource::Channel(cid) => {
                self.invalidate_channel(cid);
            }
        }
    }

    /// Invalidate a single channel's session and turn counter.
    /// Returns `true` if the channel had an active session.
    pub fn invalidate_channel(&mut self, channel_id: &Uuid) -> bool {
        self.turn_counts.remove(channel_id);
        self.deliveries.remove(channel_id);
        self.sessions.remove(channel_id).is_some()
    }

    /// Invalidate all sessions and turn counters (e.g. after agent exit).
    pub fn invalidate_all(&mut self) {
        self.sessions.clear();
        self.turn_counts.clear();
        self.deliveries.clear();
    }

    pub(crate) fn mark_channel_delivery_success(
        &mut self,
        channel_id: Uuid,
        standing_context_sent: bool,
        event_ids: impl IntoIterator<Item = String>,
    ) {
        let delivery = self.deliveries.entry(channel_id).or_default();
        delivery.standing_context_sent |= standing_context_sent;
        delivery.delivered_event_ids.extend(event_ids);
    }
}

/// An agent with its session state, owned by the pool or a running task.
pub struct OwnedAgent {
    pub index: usize,
    pub acp: AcpClient,
    pub state: SessionState,
    /// Normalized agent name from initialize (`agentInfo.name`/`serverInfo.name`).
    pub agent_name: String,
    /// Whether Goose accepted its custom system-prompt method. `None` probes on
    /// the first session; method-not-found is cached as `Some(false)` so legacy
    /// user-message framing is used for this process thereafter.
    pub goose_system_prompt_supported: Option<bool>,
    /// Protocol version reported by the agent in its initialize response.
    pub protocol_version: u32,
}

/// Package name reported by `claude-agent-acp` in its `initialize` response.
/// Any adapter reporting this name supports `_meta.systemPrompt: {append: ...}`
/// on `session/new` — the feature landed in v0.6.0 (Oct 2025), before the
/// `@zed-industries/claude-code-acp` → `@agentclientprotocol/claude-agent-acp`
/// rename, so the new name is a reliable capability gate.
const CLAUDE_AGENT_ACP_NAME: &str = "@agentclientprotocol/claude-agent-acp";

fn has_system_prompt_support(
    protocol_version: u32,
    agent_name: &str,
    goose_system_prompt_supported: Option<bool>,
) -> bool {
    if agent_name == "goose" {
        goose_system_prompt_supported == Some(true)
    } else if agent_name == CLAUDE_AGENT_ACP_NAME {
        true
    } else {
        protocol_version >= 2
    }
}

fn session_new_system_prompt<'a>(
    is_goose: bool,
    protocol_version: u32,
    agent_name: &str,
    prompt: Option<&'a str>,
) -> Option<SystemPromptTransport<'a>> {
    if is_goose || (protocol_version < 2 && agent_name != CLAUDE_AGENT_ACP_NAME) {
        None
    } else if agent_name == CLAUDE_AGENT_ACP_NAME {
        prompt.map(SystemPromptTransport::ClaudeMeta)
    } else {
        prompt.map(SystemPromptTransport::Field)
    }
}

impl OwnedAgent {
    pub(crate) fn has_system_prompt_support(&self) -> bool {
        has_system_prompt_support(
            self.protocol_version,
            &self.agent_name,
            self.goose_system_prompt_supported,
        )
    }
}

/// Pool of agents with take-and-return ownership semantics.
///
/// Agents are either idle (sitting in `agents[i]`) or checked out
/// (running inside a spawned task). The `task_map` tracks in-flight
/// tasks for panic recovery.
pub struct AgentPool {
    agents: Vec<Option<OwnedAgent>>,
    result_tx: mpsc::UnboundedSender<PromptResult>,
    result_rx: mpsc::UnboundedReceiver<PromptResult>,
    pub join_set: JoinSet<()>,
    task_map: HashMap<tokio::task::Id, TaskMeta>,
}

/// Result returned by a completed prompt task.
pub struct PromptResult {
    pub agent: OwnedAgent,
    pub source: PromptSource,
    pub outcome: PromptOutcome,
    /// Agent text captured during the completed turn (`AcpClient::take_turn_text`),
    /// trimmed of surrounding whitespace. `Some` only when the turn completed
    /// successfully with non-empty output — this is what the bridge delivers
    /// synchronously to the chat (docs/buzz-port-sync.md). Timeout, requeue and
    /// cancel paths drop the text (`None`); an empty `Ok` turn (tool-only,
    /// silent) is also `None` and is not delivered.
    pub final_text: Option<String>,
    /// Present on failure in Queue mode, for requeue.
    pub batch: Option<FlushBatch>,
}

/// The channel a prompt turn belongs to.
#[derive(Debug)]
pub enum PromptSource {
    Channel(Uuid),
}

/// Control signal for an in-flight channel turn.
///
/// 裁剪（阶段 2）：上游的 Interrupt / Rotate 两档在本 harness 不可达——桥侧只
/// 用默认 Steer（消息到达即合并重提示）与 /cancel（Cancel）；会话轮换由成功
/// 路径的 `should_rotate` 承担（Rotate 的旧职责），无独立信号入口。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlSignal {
    /// Stop the current turn and drop its triggering batch.
    Cancel,
    /// Stop the current turn and requeue its triggering batch for a merged
    /// re-prompt framed as a **steer**: a message arrived while the agent was
    /// working; it should continue its work and incorporate the message if
    /// relevant, not treat it as a replacement task. This is the default
    /// mid-turn delivery path.
    Steer,
}

/// Goose-native non-cancelling steer request, sent from the main loop to an
/// in-flight prompt task's read loop via a capacity-1 mpsc channel.
///
/// The read loop owns the `AcpClient`'s reader/writer for the duration of the
/// turn, so we cannot drive a steer write from the main thread directly. The
/// main loop carries the steer prompt body (already framed by
/// `queue::native_steer_framing()` + `queue::format_event_block`); the read
/// loop completes `sessionId` (lexical) and `expectedRunId`
/// (`AcpClient::active_run_id` at write time) when it actually emits the
/// JSON-RPC request. The main loop awaits a `SteerAck` on the `ack_tx`
/// oneshot.
///
/// ## Why the read loop fills params, not the main loop
///
/// `expectedRunId` is a *moving target*: the read loop updates
/// `self.active_run_id` as goose emits `session/update` notifications, and
/// the steer is rejected if the supplied id doesn't match the *current* run.
/// A snapshot taken at dispatch (or at mode-gate time) can be stale by the
/// time the read loop actually writes the steer line. Filling params at
/// write time uses the freshest possible run id and is correct-by-
/// construction on the one field whose freshness the protocol checks.
/// `sessionId` is in lexical scope inside the read loop's caller
/// (`session_prompt_blocks_with_idle_timeout`), so no plumbing is required
/// for that — only a function parameter pass-through.
///
/// If `active_run_id` is `None` at write time (no `session/update` seen yet
/// — e.g. agents that never emit run-id metadata), the goose-native method
/// cannot form a valid `expectedRunId`, and the read loop falls back to the
/// cross-adapter `_session/steering` method when the agent advertised
/// `_meta.steering.supported` at `initialize`. That method takes no run id, so
/// no freshness concern applies to it. When neither transport is available the
/// read loop acks [`SteerError::ExpectedRunIdMissing`]. The main loop maps that
/// to the "Err-before-pending" bucket: no withhold/mark was established at
/// `pool::send_steer` time because the request was rejected before any
/// write, so the watcher only needs to release nothing and fall back to the
/// universal `ControlSignal::Steer` cancel+merge path.
pub struct SteerRequest {
    /// Prompt body text blocks. Each entry becomes one `text` content
    /// block in `params.prompt`. Built by the main loop via
    /// `queue::native_steer_framing()` + `queue::format_event_block` so
    /// the wording cannot drift from the cancel+merge fallback path.
    pub prompt_blocks: Vec<String>,
    /// Oneshot for the read loop to report the outcome.
    pub ack_tx: tokio::sync::oneshot::Sender<SteerAck>,
}

/// Why a mid-turn steer failed, on either transport
/// (`_goose/unstable/session/steer` or `_session/steering`).
///
/// String and integer fields are intentionally `Debug`-only — read by
/// `tracing` macros in the main loop's `PoolEvent::SteerAck` arm via
/// `?ack`. The dead-code lint can't see that path because it doesn't
/// trace through `Debug` derives, hence the `#[allow]`.
#[allow(dead_code)]
#[derive(Debug)]
pub enum SteerError {
    /// The agent returned a JSON-RPC error response to the steer request.
    ///
    /// `code` is the JSON-RPC error code:
    /// - `-32601` (`method_not_found`): the agent does not implement the
    ///   steer extension. The main loop should fire the cancel+merge
    ///   fallback so the message still reaches the agent.
    /// - Any other code: the write landed and the agent rejected it at the
    ///   application level (e.g. wrong run id). Release the withheld event
    ///   for normal dispatch; do NOT fire the fallback — the turn is still
    ///   running or just ended.
    AgentError { code: i64, message: String },
    /// Transport-level failure: write error, read EOF, JSON-RPC framing
    /// violation, etc. The string carries the underlying `AcpError`'s display.
    Transport(String),
    /// At steer-write time neither steer transport was available: no
    /// `expectedRunId` (`AcpClient::active_run_id` was `None`, so the
    /// goose-native method could not be formed) and the agent did not
    /// advertise the cross-adapter `_session/steering` extension. The read
    /// loop drops the request without writing anything; the main loop should
    /// release any withheld event and fall back to the universal cancel+merge
    /// `ControlSignal::Steer` path. This is in the same "Err-before-pending"
    /// bucket as `Transport` write failures: no in-process state was
    /// established, so no in-process cleanup is needed.
    ExpectedRunIdMissing,
    /// A `_session/steering` request returned a JSON-RPC *success* whose
    /// `outcome` was not one of the two recognized delivery outcomes
    /// (`injected`, `startedNewTurn`) — including `failed` (codex-acp) and a
    /// missing `outcome` entirely. `outcome` carries what the agent actually
    /// reported, for logs.
    ///
    /// The steer did NOT land, so the main loop must release the withheld
    /// event and fire the cancel+merge fallback — exactly like a write that
    /// never happened. Treating an unrecognized success as delivery would
    /// drop the user's message: codex-acp answers unrecognized extension
    /// methods with a bare `{}` success rather than `-32601`.
    OutcomeRejected { outcome: String },
    /// The read loop never got to dispatch the steer because the prompt
    /// completed first. Delivery state for the underlying message is
    /// unknown after prompt completion — the main loop must treat this as
    /// "release the withheld event so normal dispatch handles it" with no
    /// claims that the agent did or did not incorporate it.
    ///
    /// Returned synchronously by `send_steer` when no task is in flight
    /// for the channel. Never sent through the ack channel — the ack
    /// watcher is only spawned on `send_steer` success.
    PromptCompleted,
}

/// Outcome of a mid-turn steer, sent from the read loop back to the
/// main loop's ack watcher.
#[derive(Debug)]
pub enum SteerAck {
    /// The agent returned a successful response to the steer request.
    /// The main loop must drop the withheld event (`remove_event`) — it
    /// has been delivered via the non-cancelling path.
    Success { session_id: String },
    /// The steer was attempted but failed. Delivery state for the
    /// underlying message is unknown after prompt completion; the main
    /// loop must release the withheld event and fall back to the
    /// universal `Steer` cancel+merge path so the message still reaches
    /// the agent.
    Err(SteerError),
    /// The prompt completed before the read loop selected the steer arm.
    /// Treated as a benign no-op: release the withheld event for normal
    /// dispatch. Do not fire the fallback `Steer` signal — there is no
    /// in-flight turn to signal, and normal dispatch handles delivery.
    PromptCompletedNeutral,
}

/// Whether a turn was cut by the idle clock or the hard wall-clock cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// No ACP wire activity for `idle_timeout` seconds.
    Idle,
    /// Turn ran for `max_turn_duration` seconds of wall-clock time.
    /// `recently_active` is true when the agent produced output within
    /// `RECENT_ACTIVITY_WINDOW` of the hard-cap firing.
    Hard { recently_active: bool },
}

/// Outcome of a prompt task.
#[allow(dead_code)]
pub enum PromptOutcome {
    Ok(StopReason),
    Error(AcpError),
    AgentExited,
    Timeout(TimeoutKind),
    /// Intentional cancel via `!cancel` command or interrupt mode.
    /// Agent is healthy — no respawn, no retry penalty.
    Cancelled,
    /// The agent did not stop within `grace` after `session/cancel` was sent
    /// for a control-signal cancellation (steer fallback, interrupt, or
    /// explicit stop). Distinct from [`TimeoutKind::Hard`]: this is a bounded
    /// cleanup deadline, not the turn's configured max-turn wall clock, so it
    /// must never be reported or dead-lettered as a hard-cap breach. The
    /// agent process is uncertain — treated as poisoned and respawned, same
    /// as a hard timeout, but the triggering batch's fate follows the
    /// `CancelReason` on the batch (steer/interrupt requeue, explicit cancel
    /// drops) rather than the hard-cap's unconditional dead-letter.
    CancelDrainTimeout(Duration),
}

/// Immutable config subset shared (via `Arc`) by all spawned prompt tasks.
///
/// Built once from `Config` at startup. Avoids cloning the full config
/// into every task. `channels` is the one mutable member: the bridge
/// refreshes it on channel sync, and prompt turns read metadata from it for
/// framing — no per-turn REST fetch exists in this port (upstream's
/// `ChannelInfoResolver` relay refresh was trimmed, docs/buzz-port-sync.md).
pub struct PromptContext {
    pub mcp_servers: Vec<McpServer>,
    pub initial_message: Option<String>,
    pub idle_timeout: Duration,
    pub max_turn_duration: Duration,
    pub dedup_mode: DedupMode,
    pub system_prompt: Option<String>,
    /// Sanitized title for each new ACP session, sent as `_meta.sessionTitle`
    /// on `session/new`. Never part of the prompt.
    pub session_title: Option<String>,
    pub team_instructions: Option<String>,
    /// Base prompt content, or `None` if `--no-base-prompt` was passed.
    ///
    /// `'static` because `PromptContext` is `Arc`-shared across async tasks.
    /// Content from `--base-prompt-file` is promoted via `Box::leak` in `main.rs`
    /// after validated file read in `Config::from_cli()`. The compiled-in default
    /// (`include_str!`) is inherently `'static`.
    pub base_prompt: Option<&'static str>,
    pub cwd: String,
    /// Shared channel metadata table for prompt framing, refreshed by the
    /// bridge on channel sync (join/leave/rename). Unknown channels fail open:
    /// prompt formatting identifies them by UUID alone.
    pub channels: std::sync::RwLock<std::collections::HashMap<Uuid, PromptChannelInfo>>,
    /// Max turns per session before proactive rotation. 0 = disabled.
    pub max_turns_per_session: u32,
}

impl AgentPool {
    /// Create a pool from pre-indexed slots (may contain None for failed startups).
    ///
    /// Slot positions are preserved so that `agent.index` always matches the
    /// index into `self.agents`. Use this instead of `new()` when the startup
    /// loop skips failed agents — `new()` would pack agents densely and break
    /// the index invariant.
    pub fn from_slots(slots: Vec<Option<OwnedAgent>>) -> Self {
        let (result_tx, result_rx) = mpsc::unbounded_channel();
        Self {
            agents: slots,
            result_tx,
            result_rx,
            join_set: JoinSet::new(),
            task_map: HashMap::new(),
        }
    }

    /// Try to claim an idle agent for the given channel (or heartbeat if `None`).
    ///
    /// Pass 1: prefer an agent that already has a session for `channel_id`.
    /// Pass 2: any idle agent.
    ///
    /// Returns `None` if all agents are checked out.
    pub fn try_claim(&mut self, channel_id: Option<Uuid>) -> Option<OwnedAgent> {
        // Pass 1: prefer agent with existing session for this channel.
        if let Some(cid) = channel_id {
            let idx = self.agents.iter().position(|slot| {
                slot.as_ref()
                    .map(|a| a.state.sessions.contains_key(&cid))
                    .unwrap_or(false)
            });
            if let Some(i) = idx {
                return self.agents[i].take();
            }
        }

        // Pass 2: first idle agent.
        let idx = self.agents.iter().position(|slot| slot.is_some());
        idx.map(|i| self.agents[i].take().unwrap())
    }

    /// Return an agent to its slot after a task completes.
    pub fn return_agent(&mut self, agent: OwnedAgent) {
        let idx = agent.index;
        if self.agents[idx].is_some() {
            // This is a bug: two tasks returned the same agent index. Log it
            // loudly so it shows up in production logs, then overwrite — the
            // alternative (dropping the incoming agent) would permanently leak
            // the slot.
            tracing::error!(
                idx,
                "BUG: return_agent called for slot {idx} which is already occupied — overwriting"
            );
        }
        self.agents[idx] = Some(agent);
    }

    /// Whether any idle agent already has a session for `channel_id`.
    /// Used to compute `affinity_hit` before calling `try_claim`.
    pub fn has_session_for(&self, channel_id: Uuid) -> bool {
        self.agents.iter().any(|slot| {
            slot.as_ref()
                .map(|a| a.state.sessions.contains_key(&channel_id))
                .unwrap_or(false)
        })
    }

    pub fn task_map(&self) -> &HashMap<tokio::task::Id, TaskMeta> {
        &self.task_map
    }

    pub fn task_map_mut(&mut self) -> &mut HashMap<tokio::task::Id, TaskMeta> {
        &mut self.task_map
    }

    /// Try to send a goose-native steer request to the in-flight task for
    /// `channel_id`.
    ///
    /// Returns `Ok(())` if the request was accepted by the read loop's
    /// receiver (capacity-1 mpsc; one slot is the single in-flight steer
    /// write). Returns `Err(SteerError::Transport(_))` on `Full`/`Closed`
    /// (already-in-flight write, or read loop torn down). Callers must
    /// fall back to the universal `ControlSignal::Steer` cancel+merge path
    /// on `Err`.
    ///
    /// This does **not** spawn the ack watcher — the caller owns the
    /// oneshot `ack_tx` inside `SteerRequest` and is responsible for
    /// awaiting it and applying the locked Success / Err / PromptCompletedNeutral
    /// semantics. Caller is also responsible for the synchronous
    /// `queue.mark_native_steer_pending(...)` *before* spawning the
    /// watcher, to close the result-vs-ack race.
    ///
    /// Returns `Err(SteerError::PromptCompleted)` if no task is in flight
    /// for `channel_id` (the prompt completed between the mode-gate check
    /// and this call, or the channel was never in flight). This is
    /// semantically a soft no-op — the caller should release any withheld
    /// event and let normal dispatch handle delivery.
    pub fn send_steer(
        &mut self,
        channel_id: Uuid,
        request: SteerRequest,
    ) -> Result<(), SteerError> {
        let meta = self
            .task_map
            .values_mut()
            .find(|m| m.channel_id == Some(channel_id))
            .ok_or(SteerError::PromptCompleted)?;
        let tx = meta
            .steer_tx
            .as_ref()
            .ok_or_else(|| SteerError::Transport("steer_tx not installed".into()))?;
        tx.try_send(request)
            .map_err(|e| SteerError::Transport(e.to_string()))
    }

    /// Durably associate a successful steer with the exact ACP session that
    /// accepted it. Acks may arrive before or after the prompt result: while
    /// the task is in flight we stage the delivery in `TaskMeta`; after return
    /// we write directly to the idle agent's matching live-session ledger.
    pub fn record_successful_steer(
        &mut self,
        channel_id: Uuid,
        event_id: String,
        session_id: String,
    ) -> bool {
        if let Some(meta) = self
            .task_map
            .values_mut()
            .find(|meta| meta.channel_id == Some(channel_id))
        {
            meta.successful_steer_deliveries
                .insert(SuccessfulSteerDelivery {
                    event_id,
                    session_id,
                });
            return true;
        }

        let Some(agent) = self.agents.iter_mut().flatten().find(|agent| {
            agent.state.sessions.get(&channel_id).map(String::as_str) == Some(session_id.as_str())
        }) else {
            return false;
        };
        agent
            .state
            .mark_channel_delivery_success(channel_id, false, [event_id]);
        true
    }

    pub fn result_tx(&self) -> mpsc::UnboundedSender<PromptResult> {
        self.result_tx.clone()
    }

    /// Split-borrow: returns mutable refs to `result_rx` and `join_set`
    /// simultaneously. This lets callers poll both in a single `select!`
    /// without a double-borrow error on `&mut AgentPool`.
    pub fn rx_and_join_set(
        &mut self,
    ) -> (&mut mpsc::UnboundedReceiver<PromptResult>, &mut JoinSet<()>) {
        (&mut self.result_rx, &mut self.join_set)
    }

    /// Non-blocking drain of the result channel. Used during shutdown to
    /// collect agents that completed while join_set was being drained.
    pub fn result_rx_try_recv(&mut self) -> Result<PromptResult, mpsc::error::TryRecvError> {
        self.result_rx.try_recv()
    }

    /// Check whether a slot is alive: either idle in the pool or checked out
    /// for an in-flight task. Returns `false` only when the slot is truly
    /// empty and available for refill.
    pub fn slot_alive(&self, index: usize) -> bool {
        let idle = self.agents.get(index).is_some_and(|s| s.is_some());
        if idle {
            return true;
        }
        // Check if the agent is checked out (in-flight on a task).
        self.task_map.values().any(|m| m.agent_index == index)
    }

    pub fn agents_mut(&mut self) -> &mut Vec<Option<OwnedAgent>> {
        &mut self.agents
    }

    /// Remove the session for `channel_id` from all idle agents.
    ///
    /// Called when the agent is removed from a channel — stale sessions
    /// should not be reused. Checked-out agents (in-flight) are not
    /// modified; their sessions will fail naturally on the next prompt
    /// if the relay rejects the request.
    ///
    /// Returns the number of sessions invalidated.
    pub fn invalidate_channel_sessions(&mut self, channel_id: Uuid) -> usize {
        let mut count = 0;
        for slot in &mut self.agents {
            if let Some(agent) = slot.as_mut() {
                if agent.state.invalidate_channel(&channel_id) {
                    count += 1;
                }
            }
        }
        count
    }
}

/// Bounded grace window for the post-cancel drain after a control-signal
/// cancellation (steer fallback or explicit stop). This is a
/// cleanup deadline, not the turn's configured max-turn wall clock — see
/// [`AcpClient::cancel_with_cleanup_grace`] and
/// [`classify_control_cancel_failure`].
const CONTROL_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Placeholder used when a channel's metadata carries no usable `name` tag.
/// Not a real channel name — consumers that need an identifying name must
/// treat it as absent.
const UNKNOWN_CHANNEL_NAME: &str = "unknown";

/// Channel-derived inputs for a new session — `(title_channel, channel_type)`
/// — from **one** metadata resolve.
///
/// Both new-session consumers need the same lookup: the MCP git-origin env
/// derives `channel_type`, and the session title is qualified with the channel
/// name. `title_channel` is `None` whenever the channel can't usefully identify
/// the session: an unresolved channel, a DM (no meaningful name), or the
/// literal `"unknown"` metadata name. Composing that sentinel would title every
/// unnamed channel identically (`Agent · #unknown`).
///
/// Renames do not retitle an already-live session; the bridge refreshes the
/// shared `PromptContext::channels` table on sync, so a later session spawn
/// uses the current channel name without a harness restart.
fn resolve_new_session_channel_context(
    channel_info: Option<&PromptChannelInfo>,
) -> (Option<String>, Option<String>) {
    let Some(info) = channel_info else {
        return (None, None);
    };
    let is_dm = info.channel_type == "dm";
    let title_channel = (!is_dm && info.name != UNKNOWN_CHANNEL_NAME).then(|| info.name.clone());
    (title_channel, Some(info.channel_type.clone()))
}

/// Channel-derived inputs for a new session (name for the session title,
/// id + type for the MCP git-origin env).
///
/// On error from `session_new_full()`, returns the `AcpError` — caller handles
/// error reporting.
struct NewSessionChannelContext<'a> {
    name: Option<&'a str>,
    id: Option<Uuid>,
    channel_type: Option<&'a str>,
}

/// Maximum length, in characters, of a session title sent to the adapter.
const SESSION_TITLE_MAX_CHARS: usize = 80;

/// Normalize a configured session title into something safe to hand an adapter.
///
/// Control characters are dropped, runs of whitespace collapse to a single
/// space, and the result is trimmed and capped at
/// [`SESSION_TITLE_MAX_CHARS`]. Returns `None` when nothing printable is left.
///
/// (Ported from the upstream `config` module; this port trimmed the clap/env
/// assembly around it — the title itself still rides `session/new`.)
fn sanitize_session_title(raw: &str) -> Option<String> {
    let collapsed = raw
        .split_whitespace()
        .map(|word| word.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    // Truncate by chars, not bytes, so a multi-byte name can't be cut mid-UTF-8.
    let title: String = collapsed
        .chars()
        .take(SESSION_TITLE_MAX_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Separator between the agent name and the channel in a composed title.
/// U+00B7 MIDDLE DOT, spaces on both sides.
const SESSION_TITLE_SEPARATOR: &str = " · ";

/// Compose a per-session title as `Agent · #channel`.
///
/// One agent in five channels gets five sessions; a bare agent name would show
/// five identical rows in the adapter's thread list. Only the channel part is
/// truncated to fit [`SESSION_TITLE_MAX_CHARS`], so the agent name always
/// survives. Returns the bare agent name when there is no channel, the channel
/// name is blank, or no room is left for it.
fn compose_session_title(agent: &str, channel_name: Option<&str>) -> String {
    let Some(channel) = channel_name.and_then(sanitize_session_title) else {
        return agent.to_string();
    };
    // Reserve the separator and the `#` sigil alongside the agent name.
    let reserved = agent.chars().count() + SESSION_TITLE_SEPARATOR.chars().count() + 1;
    let channel: String = channel
        .chars()
        .take(SESSION_TITLE_MAX_CHARS.saturating_sub(reserved))
        .collect::<String>()
        .trim_end()
        .to_string();
    if channel.is_empty() {
        return agent.to_string();
    }
    format!("{agent}{SESSION_TITLE_SEPARATOR}#{channel}")
}

/// Create a new ACP session via `session_new_full()`, applying base/system/team
/// standing context and a channel-qualified session title. Model capability
/// capture, live model switching, startup effort and permission-mode setting
/// are desktop-facing features trimmed from this port — the adapter runs with
/// its own defaults and the harness auto-approves permission requests.
///
/// On error from `session_new_full()`, returns the `AcpError` — caller handles
/// error reporting.
async fn create_session_and_apply_model(
    agent: &mut OwnedAgent,
    ctx: &PromptContext,
    channel: NewSessionChannelContext<'_>,
) -> Result<String, AcpError> {
    // Build base_prompt + system_prompt + team instructions into a single
    // session prompt. Standard protocol-v2 agents receive it in `session/new`;
    // Goose receives it through the custom request below. Legacy agents receive
    // the same content as user-message sections via `format_prompt`.
    let is_goose = agent.agent_name == "goose";
    let combined_system_prompt = with_team(
        framed_system_prompt(&ctx.cwd, ctx.base_prompt, ctx.system_prompt.as_deref()),
        ctx.team_instructions.as_deref(),
    );

    let session_title = ctx
        .session_title
        .as_deref()
        .map(|agent_name| compose_session_title(agent_name, channel.name));
    let mcp_servers = mcp_servers_with_git_origin(
        &ctx.mcp_servers,
        channel.id,
        channel.channel_type,
        ctx.session_title.as_deref(),
    );

    let resp = agent
        .acp
        .session_new_full(
            &ctx.cwd,
            mcp_servers,
            session_new_system_prompt(
                is_goose,
                agent.protocol_version,
                &agent.agent_name,
                combined_system_prompt.as_deref(),
            ),
            session_title.as_deref(),
        )
        .await?;

    if is_goose && agent.goose_system_prompt_supported != Some(false) {
        if let Some(prompt) = combined_system_prompt.as_deref() {
            match agent
                .acp
                .session_set_goose_system_prompt(&resp.session_id, prompt)
                .await
            {
                Ok(_) => agent.goose_system_prompt_supported = Some(true),
                Err(AcpError::AgentError { code: -32601, .. }) => {
                    agent.goose_system_prompt_supported = Some(false);
                    tracing::warn!(
                        target: "pool::session",
                        "Goose does not support its system-prompt extension; using user-message framing"
                    );
                }
                Err(error) => return Err(error),
            }
        }
    }

    Ok(resp.session_id)
}

fn mcp_servers_with_git_origin(
    servers: &[McpServer],
    channel_id: Option<Uuid>,
    channel_type: Option<&str>,
    agent_name: Option<&str>,
) -> Vec<McpServer> {
    let mut servers = servers.to_vec();
    let origin = match (channel_id, channel_type) {
        (Some(channel_id), Some("stream")) => Some(EnvVar {
            name: "BUZZ_GIT_ORIGIN_CHANNEL_ID".into(),
            value: channel_id.to_string(),
        }),
        (Some(_), _) => agent_name
            .filter(|name| !name.trim().is_empty())
            .map(|name| EnvVar {
                name: "BUZZ_GIT_ORIGIN_AGENT_NAME".into(),
                value: name.trim().to_string(),
            }),
        (None, _) => None,
    };
    if let Some(origin) = origin {
        for server in &mut servers {
            server.env.push(origin.clone());
        }
    }
    servers
}

/// Prepend a legacy agent's standing context to a user-message body.
///
/// Legacy agents (`protocol_version < 2`, no systemPrompt support) don't
/// receive standing context via the system role in `session/new`, so it must
/// ride along in the user message — in the session's *first* one, and never
/// again. Modern agents, or no standing content, get `body` unchanged.
///
/// Only the `initial_message` path needs this in this port: the main prompt
/// path delivers standing context through [`format_prompt`], which renders the
/// same base/system sections for a legacy session's first message.
pub(crate) fn prepend_standing_for_legacy(
    has_system_prompt_support: bool,
    base_prompt: Option<&str>,
    system_prompt: Option<&str>,
    body: &str,
) -> String {
    if has_system_prompt_support {
        return body.to_string();
    }
    let mut sections: Vec<String> = Vec::with_capacity(2);
    if let Some(bp) = base_prompt {
        sections.push(crate::buzz::queue::base_section(bp));
    }
    if let Some(sp) = system_prompt {
        sections.push(crate::buzz::prompt_framing::semantic_section("system", sp));
    }
    if sections.is_empty() {
        return body.to_string();
    }
    format!("{}\n\n{body}", sections.join("\n\n"))
}

/// Frame the `session/new` `systemPrompt` so each present prompt carries its own
/// paired tag, keeping the base/workspace/persona boundaries recoverable downstream.
///
/// The static base remains first for prompt-prefix caching. When a base is
/// present, the dynamic workspace anchor follows it and precedes the user-owned
/// agent instructions. A persona-only agent still yields
/// `<system>…</system>` rather than an unlabeled blob that would be mistaken
/// for `<base>`.
fn framed_system_prompt(
    cwd: &str,
    base_prompt: Option<&str>,
    system_prompt: Option<&str>,
) -> Option<String> {
    match (base_prompt, system_prompt) {
        (Some(bp), Some(sp)) => Some(format!(
            "{}\n\n{}\n\n{}",
            crate::buzz::queue::base_section(bp),
            workspace_section(cwd),
            crate::buzz::prompt_framing::semantic_section("system", sp),
        )),
        (Some(bp), None) => Some(format!(
            "{}\n\n{}",
            crate::buzz::queue::base_section(bp),
            workspace_section(cwd)
        )),
        (None, Some(sp)) => Some(crate::buzz::prompt_framing::semantic_section("system", sp)),
        (None, None) => None,
    }
}

fn workspace_section(cwd: &str) -> String {
    crate::buzz::prompt_framing::semantic_section(
        "workspace",
        &format!("Current working directory: {cwd}"),
    )
}

/// Append the team-owned instruction section after `<system>` and before core memory.
fn with_team(prompt: Option<String>, instructions: Option<&str>) -> Option<String> {
    let instructions = instructions
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (prompt, instructions) {
        (Some(prompt), Some(instructions)) => Some(format!(
            "{prompt}\n\n{}",
            crate::buzz::prompt_framing::semantic_section("team-instructions", instructions)
        )),
        (None, Some(instructions)) => Some(crate::buzz::prompt_framing::semantic_section(
            "team-instructions",
            instructions,
        )),
        (Some(prompt), None) => Some(prompt),
        (None, None) => None,
    }
}

/// Return `agent` to the pool via `result_tx`, clearing any steer receiver first.
///
/// Every path that returns an `OwnedAgent` to the pool via `PromptResult` goes
/// through this function. Panic/abort paths do not — and don't need to, since a
/// panicked task's agent is never sent back via `PromptResult`.
///
/// Clearing `steer_rx` here — rather than per-arm — makes the `install_steer_rx`
/// invariant (`steer_rx.is_none()` at dispatch) structurally unviolatable: a receiver
/// installed for a turn that ends before the read loop's `take()` (e.g. session-create
/// error) is always dropped before the agent re-enters the pool, so the next dispatch
/// can never trigger the assert.
///
/// On the happy path the read loop has already called `take()`, so this is a no-op.
///
/// `final_text` carries the turn's captured agent text on successful completion
/// (see [`PromptResult::final_text`]); every failure/requeue/cancel path passes
/// `None`.
fn send_prompt_result(
    result_tx: &mpsc::UnboundedSender<PromptResult>,
    mut agent: OwnedAgent,
    source: PromptSource,
    outcome: PromptOutcome,
    batch: Option<FlushBatch>,
    final_text: Option<String>,
) {
    agent.acp.clear_steer_rx();
    let _ = result_tx.send(PromptResult {
        agent,
        source,
        outcome,
        final_text,
        batch,
    });
}

/// Pull the captured turn text off the agent and return it only when
/// non-empty, trimmed of surrounding whitespace. Called at every completion
/// point (natural end, race-1 completion); timeout / requeue / cancel paths
/// pass `None` instead.
fn take_turn_text(agent: &mut OwnedAgent) -> Option<String> {
    let text = agent.acp.take_turn_text();
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Core async function spawned for each prompt.
///
/// Lifecycle:
/// 1. Resolve or create the channel session.
/// 2. Send `initial_message` on new channel sessions (if configured).
/// 3. Build the prompt text from the batch (channel metadata + message blocks).
/// 4. Send the actual prompt with turn timeout.
/// 5. Handle all error paths, always returning the agent via `result_tx`.
///
/// The agent is ALWAYS returned — even on panic the `JoinSet` detects the
/// abort and the caller uses `task_map` to recover the agent index.
pub async fn run_prompt_task(
    mut agent: OwnedAgent,
    batch: FlushBatch,
    ctx: Arc<PromptContext>,
    result_tx: mpsc::UnboundedSender<PromptResult>,
    control_rx: Option<tokio::sync::oneshot::Receiver<ControlSignal>>,
) {
    let source = PromptSource::Channel(batch.channel_id);

    // Resolve channel metadata for prompt framing from the shared channel table
    // (refreshed by the bridge on sync — no per-turn relay fetch in this port,
    // docs/buzz-port-sync.md). Unknown channels fail open: `format_prompt`
    // identifies the channel by UUID alone and the session title falls back to
    // the bare agent name.
    let resolved_channel_info = ctx
        .channels
        .read()
        .ok()
        .and_then(|channels| channels.get(&batch.channel_id).cloned());

    // Channel name and type for a fresh session (title qualification + MCP
    // git-origin env), derived from the same single metadata resolve. Only
    // consulted when this channel has no live session yet.
    let mut title_channel: Option<String> = None;
    let mut origin_channel_type: Option<String> = None;
    if !agent.state.sessions.contains_key(&batch.channel_id) {
        let (resolved_channel, resolved_channel_type) =
            resolve_new_session_channel_context(resolved_channel_info.as_ref());
        title_channel = resolved_channel;
        origin_channel_type = resolved_channel_type;
    }

    let (session_id, is_new_session) = {
        if let Some(sid) = agent.state.sessions.get(&batch.channel_id) {
            (sid.clone(), false)
        } else {
            // The title is channel-qualified (`Agent · #channel`) so one agent
            // in several chats doesn't produce identical session rows;
            // `title_channel` comes from the single resolve above and is `None`
            // for DM, unresolved, and unnamed channels.
            match create_session_and_apply_model(
                &mut agent,
                &ctx,
                NewSessionChannelContext {
                    name: title_channel.as_deref(),
                    id: Some(batch.channel_id),
                    channel_type: origin_channel_type.as_deref(),
                },
            )
            .await
            {
                Ok(sid) => {
                    tracing::info!(
                        target: "pool::session",
                        "created session {sid} for channel {}",
                        batch.channel_id
                    );
                    agent.state.sessions.insert(batch.channel_id, sid.clone());
                    agent
                        .state
                        .deliveries
                        .insert(batch.channel_id, ChannelDeliveryState::default());
                    (sid, true)
                }
                Err(AcpError::AgentExited) => {
                    agent.state.invalidate_all();
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::AgentExited,
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                    return;
                }
                Err(e) => {
                    // Session creation failed — the next retry re-attempts.
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::Error(e),
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                    return;
                }
            }
        }
    };

    // Standing context is fixed for the life of a session. Agents with
    // systemPrompt support already hold it from session/new; legacy agents
    // receive it in the session's first user message and never again
    // (`format_prompt` renders it once, gated on this flag).
    //
    // `is_new_session` comes from the session registry, which is cleared
    // whenever a session is invalidated — so the replacement session re-delivers
    // rather than leaving the agent unbriefed.
    //
    // Delivery state is committed only after ACP confirms success. Existing
    // sessions created before this field existed fail safe by behaving as
    // undelivered once, rather than silently omitting standing context.
    let mut standing_context_sent = agent
        .state
        .deliveries
        .get(&batch.channel_id)
        .is_some_and(|delivery| delivery.standing_context_sent);

    if is_new_session {
        if let Some(initial_msg) = &ctx.initial_message {
            tracing::info!(
                target: "pool::session",
                "sending initial_message to session {session_id} for channel {}",
                batch.channel_id
            );
            let init_msg = prepend_standing_for_legacy(
                agent.has_system_prompt_support(),
                ctx.base_prompt,
                ctx.system_prompt.as_deref(),
                initial_msg,
            );
            let init_result = agent
                .acp
                .session_prompt_with_idle_timeout(
                    &session_id,
                    &init_msg,
                    ctx.idle_timeout,
                    ctx.max_turn_duration,
                )
                .await;

            match init_result {
                Ok(stop_reason) => {
                    tracing::info!(
                        target: "pool::session",
                        "initial_message complete for channel {}: {stop_reason:?}",
                        batch.channel_id
                    );
                    // The legacy agent has its standing context now; the turn
                    // prompt below must not repeat it. Every other arm returns.
                    standing_context_sent = true;
                    if !agent.has_system_prompt_support() {
                        agent
                            .state
                            .mark_channel_delivery_success(batch.channel_id, true, []);
                    }
                }
                Err(AcpError::AgentExited) => {
                    agent.state.invalidate_all();
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::AgentExited,
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                    return;
                }
                Err(AcpError::IdleTimeout(_)) => {
                    tracing::warn!(
                        target: "pool::session",
                        "initial_message idle timeout ({}s) for channel {} — cancelling",
                        ctx.idle_timeout.as_secs(),
                        batch.channel_id
                    );
                    match agent
                        .acp
                        .cancel_with_cleanup(&session_id, ctx.idle_timeout)
                        .await
                    {
                        Ok(_) => {
                            agent.state.invalidate(&source);
                        }
                        Err(AcpError::AgentExited) => {
                            agent.state.invalidate_all();
                            send_prompt_result(
                                &result_tx,
                                agent,
                                source,
                                PromptOutcome::AgentExited,
                                requeue_batch_if_queue(&ctx, batch),
                                None,
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(
                                target: "pool::session",
                                "cancel_with_cleanup failed during initial_message timeout: {e}"
                            );
                            agent.state.invalidate(&source);
                        }
                    }
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::Timeout(TimeoutKind::Idle),
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                    return;
                }
                Err(AcpError::HardTimeout { silence }) => {
                    let recently_active = silence < RECENT_ACTIVITY_WINDOW;
                    tracing::error!(
                        target: "pool::session",
                        "hard timeout ({}s cap, silence {silence:?}, recently_active={recently_active}) during initial_message for channel {} — agent process is unrecoverable",
                        ctx.max_turn_duration.as_secs(),
                        batch.channel_id
                    );
                    agent.state.invalidate_all();
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::Timeout(TimeoutKind::Hard { recently_active }),
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                    return;
                }
                Err(e) => {
                    tracing::error!(
                        target: "pool::session",
                        "initial_message failed for channel {}: {e} — invalidating session",
                        batch.channel_id
                    );
                    agent.state.invalidate(&source);
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::Error(e),
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                    return;
                }
            }
        }
    }

    // `slash_command` holds the bare command. It is sent as the FIRST prompt
    // content block so ACP connectors' slash-command detection
    // (`prompt[0].text.startsWith("/")`) fires; the wrapped Buzz context
    // follows as a second block.
    // Profile lookup was trimmed with the relay layer — no known display
    // names to strip, so slash-command detection handles bare commands.
    let known_names: &[&str] = &[];
    let slash_command = crate::buzz::queue::slash_command_for_batch(&batch, known_names);
    let prompt_sections: Vec<String> = crate::buzz::queue::format_prompt(
        &batch,
        &crate::buzz::queue::FormatPromptArgs {
            channel_info: resolved_channel_info.as_ref(),
            has_system_prompt_support: agent.has_system_prompt_support(),
            base_prompt: ctx.base_prompt,
            system_prompt: ctx.system_prompt.as_deref(),
            standing_context_sent,
        },
    );

    // Slash-command pass-through sends the bare command as the first text
    // block (so connector detection fires), then each prompt section as its
    // own block.
    let prompt_blocks: Vec<&str> = match slash_command {
        Some(ref cmd) => std::iter::once(cmd.as_str())
            .chain(prompt_sections.iter().map(String::as_str))
            .collect(),
        None => prompt_sections.iter().map(String::as_str).collect(),
    };
    let prompt_bytes: usize = prompt_blocks.iter().map(|block| block.len()).sum();
    let has_standing_context = ctx.base_prompt.is_some() || ctx.system_prompt.is_some();
    let standing_context_included =
        !agent.has_system_prompt_support() && !standing_context_sent && has_standing_context;
    tracing::info!(
        target: "pool::prompt",
        prompt_bytes,
        standing_context_included,
        "prompt context delivery"
    );

    // Begin capturing this turn's agent text: everything the agent outputs
    // from here on (including native-steer follow-up text on the same session)
    // accumulates in `AcpClient::turn_text`, taken out at successful completion
    // and delivered synchronously by the bridge (docs/buzz-port-sync.md).
    agent.acp.begin_turn();

    // log reads as start/stop pairs. Purely observational: an unpaired start is
    // the only durable evidence that a turn was entered and never returned, and
    // without it a stalled agent and an agent nobody woke leave identical logs —
    // zero completions either way, so anything reading them afterwards has to
    // guess which happened.
    tracing::info!(
        target: "pool::prompt",
        "turn starting for {}",
        prompt_label(&source)
    );

    // When control_rx is Some (channel tasks), wrap the prompt in select! so
    // the main loop can cancel, interrupt, steer, or rotate it. A `None`
    // control channel (no caller in this port) takes the simple await path.
    let prompt_result = match control_rx {
        None => {
            agent
                .acp
                .session_prompt_blocks_with_idle_timeout(
                    &session_id,
                    &prompt_blocks,
                    ctx.idle_timeout,
                    ctx.max_turn_duration,
                )
                .await
        }
        Some(rx) => {
            tokio::select! {
                biased;
                result = agent.acp.session_prompt_blocks_with_idle_timeout(
                    &session_id,
                    &prompt_blocks,
                    ctx.idle_timeout,
                    ctx.max_turn_duration,
                ) => result,
                mode = rx => {
                    let control_signal = mode.unwrap_or(ControlSignal::Cancel);
                    // Control signal received. Guard against Race 1: the turn may
                    // have completed naturally just as cancel fired.
                    if agent.acp.has_in_flight_prompt() {
                        // Prompt is genuinely in-flight — cancel it.
                        match agent
                            .acp
                            .cancel_with_cleanup_grace(&session_id, CONTROL_CANCEL_GRACE)
                            .await
                        {
                            Ok(stop_reason) => {
                                log_stop_reason(&source, &stop_reason);
                                agent.state.invalidate(&source);
                                let retry_batch =
                                    requeue_cancelled_batch(&ctx, control_signal, batch);
                                send_prompt_result(
                                    &result_tx,
                                    agent,
                                    source,
                                    PromptOutcome::Cancelled,
                                    retry_batch,
                                    None,
                                );
                                return;
                            }
                            Err(error) => {
                                // Single production arm: classify the error→outcome
                                // and outcome→batch-fate boundary once via the seam
                                // shared with tests, then invalidate/send once.
                                let failure = classify_control_cancel_failure(
                                    &ctx,
                                    error,
                                    control_signal,
                                    batch,
                                );
                                if failure.invalidate_all {
                                    agent.state.invalidate_all();
                                } else {
                                    agent.state.invalidate(&source);
                                }
                                send_prompt_result(
                                    &result_tx,
                                    agent,
                                    source,
                                    failure.outcome,
                                    failure.retry_batch,
                                    None,
                                );
                                return;
                            }
                        }
                    } else {
                        // Race 1 resolution: turn completed naturally before cancel
                        // could fire. last_prompt_id is None — cleared by
                        // session_prompt_with_idle_timeout() on success. The prompt
                        // future was dropped by select! — its Ok result is gone.
                        //
                        // Note: this `else` branch (last_prompt_id is None) cannot
                        // fire during the pre-prompt phase because `biased` select!
                        // polls the prompt arm first. That arm sets last_prompt_id
                        // synchronously before its first yield point, so by the time
                        // the cancel arm can win, last_prompt_id is already Some.
                        // This branch only fires when the turn genuinely completed
                        // and last_prompt_id was cleared by the success path.
                        //
                        // MUST send a PromptResult or the main loop deadlocks.
                        tracing::debug!(
                            target: "pool::prompt",
                            "control signal arrived but turn already completed — treating as success"
                        );
                        log_stop_reason(&source, &StopReason::EndTurn);
                        if !agent.has_system_prompt_support() {
                            agent
                                .state
                                .mark_channel_delivery_success(batch.channel_id, true, []);
                        }
                        let final_text = take_turn_text(&mut agent);
                        send_prompt_result(
                            &result_tx,
                            agent,
                            source,
                            PromptOutcome::Ok(StopReason::EndTurn),
                            None, // turn succeeded — batch was processed, no requeue
                            final_text,
                        );
                        return;
                    }
                }
            }
        }
    };

    match prompt_result {
        Ok(stop_reason) => {
            log_stop_reason(&source, &stop_reason);

            // Delivery state is committed only after ACP confirms success. The
            // legacy standing-context flag gates the one-time `<base>`/`<system>`
            // rendering inside `format_prompt`; system-prompt agents hold
            // standing context from session/new and need no flag.
            if !agent.has_system_prompt_support() {
                agent
                    .state
                    .mark_channel_delivery_success(batch.channel_id, true, []);
            }

            let should_rotate = matches!(
                stop_reason,
                StopReason::MaxTokens | StopReason::MaxTurnRequests
            );

            let should_rotate = should_rotate || {
                let limit = ctx.max_turns_per_session;
                if limit > 0 {
                    let count = agent.state.turn_counts.entry(batch.channel_id).or_insert(0);
                    *count += 1;
                    *count >= limit
                } else {
                    false
                }
            };

            if should_rotate {
                tracing::info!(
                    target: "pool::session",
                    "rotating session for {source:?} after {stop_reason:?}",
                );
                agent.state.invalidate(&source);
            }

            let final_text = take_turn_text(&mut agent);
            send_prompt_result(
                &result_tx,
                agent,
                source,
                PromptOutcome::Ok(stop_reason),
                None,
                final_text,
            );
        }
        Err(AcpError::AgentExited) => {
            tracing::error!(target: "pool::prompt", "agent {} exited during prompt", agent.index);
            agent.state.invalidate_all();
            send_prompt_result(
                &result_tx,
                agent,
                source,
                PromptOutcome::AgentExited,
                requeue_batch_if_queue(&ctx, batch),
                None,
            );
        }
        Err(AcpError::IdleTimeout(_)) => {
            tracing::warn!(
                target: "pool::prompt",
                "idle timeout ({}s) — cancelling session {session_id}",
                ctx.idle_timeout.as_secs()
            );
            match agent
                .acp
                .cancel_with_cleanup(&session_id, ctx.idle_timeout)
                .await
            {
                Ok(stop_reason) => {
                    log_stop_reason(&source, &stop_reason);
                    // Timeout triggers respawn in handle_prompt_result —
                    // session state will be discarded with the old agent.
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::Timeout(TimeoutKind::Idle),
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                }
                Err(AcpError::AgentExited) => {
                    tracing::error!(
                        target: "pool::prompt",
                        "agent {} exited during cancel_with_cleanup",
                        agent.index
                    );
                    agent.state.invalidate_all();
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::AgentExited,
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                }
                Err(e) => {
                    tracing::error!(
                        target: "pool::prompt",
                        "cancel_with_cleanup error: {e} — invalidating session"
                    );
                    agent.state.invalidate(&source);
                    send_prompt_result(
                        &result_tx,
                        agent,
                        source,
                        PromptOutcome::Timeout(TimeoutKind::Idle),
                        requeue_batch_if_queue(&ctx, batch),
                        None,
                    );
                }
            }
        }
        Err(AcpError::HardTimeout { silence }) => {
            let recently_active = silence < RECENT_ACTIVITY_WINDOW;
            tracing::error!(
                target: "pool::prompt",
                "hard timeout ({}s cap, silence {silence:?}, recently_active={recently_active}) — agent process is unrecoverable, invalidating all sessions",
                ctx.max_turn_duration.as_secs()
            );
            agent.state.invalidate_all();
            send_prompt_result(
                &result_tx,
                agent,
                source,
                PromptOutcome::Timeout(TimeoutKind::Hard { recently_active }),
                requeue_batch_if_queue(&ctx, batch),
                None,
            );
        }
        Err(e) => {
            tracing::error!(target: "pool::prompt", "session_prompt error: {e}");
            // AgentError means the agent caught a problem before mutating
            // session state (e.g. bad LLM response). The session is healthy —
            // don't invalidate it. Other errors may have corrupted state.
            if !matches!(e, AcpError::AgentError { .. }) {
                agent.state.invalidate(&source);
            }
            send_prompt_result(
                &result_tx,
                agent,
                source,
                PromptOutcome::Error(e),
                requeue_batch_if_queue(&ctx, batch),
                None,
            );
        }
    }
}

#[inline]
fn requeue_batch_if_queue(ctx: &PromptContext, batch: FlushBatch) -> Option<FlushBatch> {
    match ctx.dedup_mode {
        DedupMode::Queue => Some(batch),
        DedupMode::Drop => None,
    }
}

/// Map a cancelling [`ControlSignal`] to the [`CancelReason`] that should frame
/// the merged re-prompt, then requeue the batch (in `Queue` dedup mode) with
/// that reason stamped onto [`FlushBatch::cancel_reason`]. `Cancel` drops the
/// batch entirely. The reason is consumed by the main loop at requeue time
/// (`requeue_as_cancelled`) and ultimately by `format_prompt`.
#[inline]
fn requeue_cancelled_batch(
    ctx: &PromptContext,
    signal: ControlSignal,
    batch: FlushBatch,
) -> Option<FlushBatch> {
    match signal {
        // Steer → 合并重提示（标记 cancel 原因）。
        ControlSignal::Steer => requeue_batch_if_queue(ctx, batch).map(|mut b| {
            b.cancel_reason = Some(CancelReason::Steer);
            b
        }),
        // Cancel → 丢弃批次，不重提示。
        ControlSignal::Cancel => None,
    }
}

/// Result of classifying a failed [`AcpClient::cancel_with_cleanup_grace`]
/// call: the [`PromptOutcome`] to report and the triggering batch's fate,
/// decided together so tests cross the exact error→outcome→batch-fate
/// boundary the production `Err(error)` arm uses.
struct ControlCancelFailure {
    outcome: PromptOutcome,
    retry_batch: Option<FlushBatch>,
    /// `AgentExited` invalidates every session on the agent; every other
    /// failure invalidates only the source that triggered this turn.
    invalidate_all: bool,
}

/// Classify a failed control-signal cancellation (steer fallback, interrupt,
/// or explicit stop) into the [`PromptOutcome`] to report and the triggering
/// batch's fate. This is the single production seam used by the `Err(error)`
/// arm of the control-cancel branch in [`run_prompt_task`] — the boundary
/// this exists to keep singular, so regressions there are regression-tested.
///
/// [`AcpError::CancelDrainTimeout`] is the expected, common case: the agent
/// didn't stop within its bounded grace window. [`AcpError::HardTimeout`] is
/// not expected here — [`AcpClient::cancel_with_cleanup_grace`] translates its
/// own drain-deadline `HardTimeout` into `CancelDrainTimeout` before
/// returning — but for defense in depth an unexpected `HardTimeout` at this
/// bounded cancellation boundary must never regain real hard-cap/dead-letter
/// classification, so it maps to `CancelDrainTimeout(CONTROL_CANCEL_GRACE)`
/// rather than `Timeout(Hard)`.
fn classify_control_cancel_failure(
    ctx: &PromptContext,
    error: AcpError,
    signal: ControlSignal,
    batch: FlushBatch,
) -> ControlCancelFailure {
    let (outcome, invalidate_all) = match error {
        AcpError::AgentExited => (PromptOutcome::AgentExited, true),
        AcpError::IdleTimeout(_) => (PromptOutcome::Timeout(TimeoutKind::Idle), false),
        AcpError::CancelDrainTimeout(grace) => (PromptOutcome::CancelDrainTimeout(grace), false),
        // Defense in depth: this bounded cancellation API is documented to
        // translate its own HardTimeout into CancelDrainTimeout, so this arm
        // should be unreachable in practice. If it ever fires anyway, still
        // report the truthful non-hard outcome rather than the real hard-cap
        // (which would dead-letter the batch and claim the configured cap).
        AcpError::HardTimeout { .. } => (
            PromptOutcome::CancelDrainTimeout(CONTROL_CANCEL_GRACE),
            false,
        ),
        other => (PromptOutcome::Error(other), false),
    };
    ControlCancelFailure {
        outcome,
        retry_batch: requeue_cancelled_batch(ctx, signal, batch),
        invalidate_all,
    }
}

/// How a turn's source is named in the `pool::prompt` log lines.
///
/// Shared by the turn-start and turn-stop lines so a log can be read as pairs.
fn prompt_label(source: &PromptSource) -> String {
    let PromptSource::Channel(cid) = source;
    format!("channel {cid}")
}

/// Log a stop reason at the appropriate tracing level.
fn log_stop_reason(source: &PromptSource, stop_reason: &StopReason) {
    let label = prompt_label(source);
    match stop_reason {
        StopReason::EndTurn => {
            tracing::info!(target: "pool::prompt", "turn complete for {label}: end_turn");
        }
        StopReason::Cancelled => {
            tracing::warn!(target: "pool::prompt", "turn cancelled for {label}");
        }
        StopReason::MaxTokens => {
            tracing::warn!(target: "pool::prompt", "turn hit max_tokens for {label} — session will be rotated");
        }
        StopReason::MaxTurnRequests => {
            tracing::warn!(target: "pool::prompt", "turn hit max_turn_requests for {label} — session will be rotated");
        }
        StopReason::Refusal => {
            tracing::warn!(target: "pool::prompt", "turn refused for {label}");
        }
    }
}
