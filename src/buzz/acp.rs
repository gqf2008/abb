//! ACP client module — manages communication with an AI agent subprocess over stdio
//! using JSON-RPC 2.0 (newline-delimited / NDJSON).
//!
//! # Lifecycle
//! 1. [`AcpClient::spawn`] — launch agent binary as subprocess
//! 2. [`AcpClient::initialize`] — protocol version negotiation
//! 3. [`AcpClient::session_new`] — create session with MCP server config
//! 4. [`AcpClient::session_prompt_with_idle_timeout`] — send prompt with idle/hard deadline, return stop reason
//! 5. [`AcpClient::session_cancel`] / [`AcpClient::cancel_with_cleanup`] — cancel in-flight turn

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};

/// Maximum allowed size of a single NDJSON line from the agent's stdout.
/// Lines exceeding this limit are rejected to prevent OOM from rogue agents.
const MAX_LINE_SIZE: usize = 10_000_000; // 10 MB

/// An MCP server configuration passed to `session/new`.
///
/// Corresponds to the `McpServerStdio` variant in the ACP schema.
/// All four fields are **required** by the schema (`args` and `env` may be empty arrays).
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
}

/// A single environment variable for an MCP server.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// Stop reason returned by `session/prompt` when the agent finishes a turn.
///
/// Maps to the `stopReason` field in the `SessionPromptResponse`.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    /// Agent completed the turn normally (`"end_turn"`).
    EndTurn,
    /// Turn was cancelled via `session/cancel` (`"cancelled"`).
    Cancelled,
    /// Agent hit its token limit (`"max_tokens"`).
    MaxTokens,
    /// Agent hit its per-turn request limit (`"max_turn_requests"`).
    MaxTurnRequests,
    /// Agent refused the prompt (`"refusal"`).
    /// Note: refused turns are dropped from history by the agent.
    Refusal,
}

impl StopReason {
    /// Parse a `stopReason` string from the ACP wire format.
    ///
    /// Matching is case-insensitive so agents that send `"END_TURN"` or
    /// `"Cancelled"` are handled correctly without a protocol error.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "end_turn" => Some(Self::EndTurn),
            "cancelled" => Some(Self::Cancelled),
            "max_tokens" => Some(Self::MaxTokens),
            "max_turn_requests" => Some(Self::MaxTurnRequests),
            "refusal" => Some(Self::Refusal),
            _ => None,
        }
    }
}

/// Errors that can occur in the ACP client.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Agent process exited unexpectedly")]
    AgentExited,

    #[error("Idle timeout — no agent activity for {0:?}")]
    IdleTimeout(std::time::Duration),

    #[error("Hard turn timeout exceeded (silence {silence:?})")]
    HardTimeout { silence: std::time::Duration },

    #[error("Agent did not stop within {0:?} after cancellation")]
    CancelDrainTimeout(std::time::Duration),

    #[error("Request timeout — agent did not respond within {0:?}")]
    Timeout(std::time::Duration),

    #[error("Write timeout — agent stopped reading stdin (blocked for {0:?})")]
    WriteTimeout(std::time::Duration),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Agent reported error (code {code}): {message}")]
    AgentError { code: i64, message: String },
}

/// Build an [`AcpError::AgentError`] from a JSON-RPC error object,
/// preserving the numeric code. When the `message` field is missing or
/// non-string, fall back to the full JSON object so provider-specific
/// detail (e.g. a `data` field) is not lost.
fn agent_error_from_json(error: &serde_json::Value) -> AcpError {
    let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000);
    let message = match error.get("message").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => error.to_string(),
    };
    AcpError::AgentError { code, message }
}

fn build_initialize_params() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": 2,
        "clientCapabilities": build_client_capabilities(),
        "clientInfo": {
            "name": "buzz-acp",
            "version": env!("CARGO_PKG_VERSION")
        },
    })
}

/// ACP client that owns an agent subprocess and communicates over its stdio.
///
/// One `AcpClient` per agent process. Multiple sessions can be created on the
/// same client via repeated calls to [`session_new`](AcpClient::session_new).
pub struct AcpClient {
    /// The agent child process (kept alive to prevent zombie).
    /// 进程内双工传输（测试 [`Self::connect`]）时为 `None`——无进程可杀，
    /// EOF 语义由对端半边 drop 产生（与子进程退出等价）。
    child: Option<Child>,
    /// Write end of the agent's stdin pipe.
    /// Box 化以兼容子进程管道与测试 duplex 两种传输（协议层零分叉）。
    stdin: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    /// Framed reader over the agent's stdout pipe (line-oriented, bounded).
    /// Uses `LinesCodec::new_with_max_length` to enforce MAX_LINE_SIZE at the
    /// read level — prevents OOM from rogue agents writing infinite non-newline bytes.
    reader: FramedRead<Box<dyn tokio::io::AsyncRead + Unpin + Send>, LinesCodec>,
    /// Monotonically increasing JSON-RPC request id counter.
    /// Harness-generated IDs are always numeric.
    next_id: u64,
    /// The id of a `session/request_permission` request that has been received
    /// but not yet responded to. Stored as `serde_json::Value` because JSON-RPC 2.0
    /// permits both numeric and string IDs from the agent.
    /// Used by [`cancel_with_cleanup`](AcpClient::cancel_with_cleanup) to send
    /// a `cancelled` outcome before the agent returns from `session/prompt`.
    pending_permission_id: Option<serde_json::Value>,
    /// Whether we have already sent a response to the pending permission request.
    /// Guards against double-response if a timeout fires after the allow_once
    /// response was written but before `pending_permission_id` was cleared.
    permission_responded: bool,
    /// The JSON-RPC id of the most recently sent `session/prompt` request.
    /// Used by [`cancel_with_cleanup`] to drain the correct response.
    /// Set in [`session_prompt_with_idle_timeout`]; consumed in [`cancel_with_cleanup`].
    last_prompt_id: Option<u64>,
    /// Hard deadline for the current turn, set by `session_prompt_with_idle_timeout`.
    /// Inherited by `cancel_with_cleanup` so the drain loop shares the same budget
    /// rather than starting a fresh timer (prevents double-jeopardy).
    current_hard_deadline: Option<tokio::time::Instant>,
    /// Most recently observed `_meta.goose.activeRunId` from a
    /// `session/update` notification of kind `session_info_update`.
    ///
    /// Both goose and buzz-agent emit `session_info_update` with this field;
    /// goose emits it whenever it starts or clears an active prompt run
    /// (`crates/goose/src/acp/server.rs:2277` `send_active_run_update`).
    /// Required as `expectedRunId` when calling the non-standard
    /// `_goose/unstable/session/steer` method to inject a message into an
    /// in-flight turn without cancelling it.
    ///
    /// `None` until the first `session_info_update` arrives, or after the
    /// run clears (goose/buzz-agent emit `activeRunId: null` at end of turn).
    /// Other agents may leave this unset — readers must treat `None` as
    /// "no active run to steer into" and fall back to cancel+merge.
    active_run_id: Option<String>,
    /// Whether the agent advertised `_meta.steering.supported: true` in its
    /// `initialize` response, meaning it implements the cross-adapter
    /// [`ACP_STEER_METHOD`] extension.
    ///
    /// Set once by [`initialize`](Self::initialize); `false` for agents that
    /// omit the key. This is the **only** gate on writing an
    /// [`ACP_STEER_METHOD`] request. It must never be replaced by error-code
    /// probing: codex-acp answers unrecognized extension methods with `{}` —
    /// a JSON-RPC *success*, not `-32601` — which the main loop would read as
    /// a delivered steer and drop the user's message from the queue.
    steering_supported: bool,
    /// Per-turn channel for receiving goose-native non-cancelling steer
    /// requests from the main loop. Installed by
    /// [`install_steer_rx`](Self::install_steer_rx) at dispatch and
    /// consumed (via `take()`) by `session_prompt_with_idle_timeout` so it
    /// is dropped at scope exit alongside the turn it served. `None`
    /// outside of a goose-native turn — the read loop's steer arm is
    /// disabled in that case.
    steer_rx: Option<tokio::sync::mpsc::Receiver<crate::buzz::pool::SteerRequest>>,
    /// Accumulated agent reply text for the current turn (`agent_message_chunk`
    /// deltas, session-filtered). Cleared by [`AcpClient::begin_turn`]; drained
    /// by [`AcpClient::take_turn_text`] at turn completion. A turn with no text
    /// (pure tool turn / silent refusal) yields an empty string.
    turn_text: String,
    /// Session id of the in-flight prompt, used to attribute
    /// `agent_message_chunk` deltas to this turn. Steer turns keep the same
    /// session and keep accumulating; a straggler from another session must
    /// not bleed into this turn's reply.
    turn_session: Option<String>,
}

/// goose's non-standard mid-turn steer method. Requires `expectedRunId`, so it
/// is only usable once a `session_info_update` has supplied
/// `_meta.goose.activeRunId`. Emitted by goose and buzz-agent only.
const GOOSE_STEER_METHOD: &str = "_goose/unstable/session/steer";

/// The cross-adapter mid-turn steer method, shipped by claude-agent-acp
/// (`src/acp-agent.ts:200`) and codex-acp (`src/AcpExtensions.ts:11`).
/// Params are `{sessionId, prompt}` — no run id — and the result is
/// `{outcome}`. Gated on [`AcpClient::steering_supported`].
const ACP_STEER_METHOD: &str = "_session/steering";

/// `outcome` value meaning the steer was applied to the turn Buzz is waiting
/// on, which therefore keeps running.
const STEER_OUTCOME_INJECTED: &str = "injected";

/// `outcome` value meaning the turn Buzz was steering had already finished, so
/// the adapter began a fresh turn carrying the message. Still a delivery
/// success, but the awaited turn is over — see the steer-response arm for why
/// this must not renew the hard deadline.
const STEER_OUTCOME_STARTED_NEW_TURN: &str = "startedNewTurn";

/// Which wire method carried an in-flight steer request, recorded so the
/// response arm decodes the shape that method actually returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SteerTransport {
    /// [`GOOSE_STEER_METHOD`] — any success result is a delivered steer.
    Goose,
    /// [`ACP_STEER_METHOD`] — success carries an `outcome` that must be
    /// positively recognized before the steer counts as delivered.
    AcpExtension,
}

fn build_client_capabilities() -> serde_json::Value {
    serde_json::json!({
        // Signal to ACP adapters that Buzz can hand users to terminal-native
        // auth flows. Adapters decide which auth methods to expose; Buzz does
        // not hardcode vendor login commands from this capability.
        "auth": {
            "terminal": true
        },
        // Signal to goose that we handle `_goose/unstable/session/update`
        // notifications. Without this the custom notification is suppressed
        // on goose's side and usage data is never emitted.
        "_meta": {
            "goose": {
                "customNotifications": true
            },
            // Non-standard extension used by claude-agent-acp to advertise the
            // exact terminal login argv for subscription auth. Unknown `_meta`
            // keys are ignored by other adapters.
            "terminal-auth": true
        }
    })
}

impl AcpClient {
    /// Kill the agent subprocess and wait for it to exit (no zombies).
    ///
    /// `Drop` only calls `start_kill()` (sends SIGKILL but doesn't reap).
    /// Call this when you need guaranteed cleanup — e.g., in `run_models`
    /// before process exit.
    pub async fn shutdown(&mut self) {
        // 进程内双工（测试）无子进程：清理由 Drop 半边自然完成，直接返回。
        let Some(child) = self.child.as_mut() else {
            return;
        };
        // Kill the entire process group when possible. The child was spawned
        // with process_group(0), so its PID == its PGID. Killing the group
        // ensures subprocesses (MCP servers, tool processes) are cleaned up
        // rather than orphaned to init.
        //
        // Falls back to start_kill() (direct child only) on non-Unix or if
        // the child has been polled to completion (id() returns None).
        match child.id() {
            Some(pid) if kill_process_group(pid) => {}
            _ => {
                let _ = child.start_kill();
            }
        }
        // Bounded wait: if the child doesn't exit within 5s after SIGKILL,
        // give up and let Drop/OS handle it. An unbounded wait here would
        // wedge the harness during respawn or shutdown if a child is stuck.
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::debug!("child wait error after kill: {e}"),
            Err(_) => tracing::warn!("child did not exit within 5s after SIGKILL — abandoning"),
        }
    }

    /// Spawn the agent binary as a subprocess and connect to its stdio pipes.
    ///
    /// After spawning, call [`initialize`](Self::initialize) before any other method.
    pub async fn spawn(
        command: &str,
        args: &[String],
        extra_env: &[(String, String)],
    ) -> Result<Self, AcpError> {
        use std::process::Stdio;

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherit stderr so agent logs are visible in the harness terminal.
            .stderr(Stdio::inherit())
            // Ensure the child is killed when the AcpClient is dropped (best-effort).
            // Callers MUST still call shutdown().await for guaranteed cleanup.
            .kill_on_drop(true);

        // ABB 侧差异（相对上游）：extra_env 为**无条件覆盖**（上游仅缺失时注入）。
        // service 用它对子进程 PATH 做全量接管（composed_path）——launchd/GUI 进程
        // 的 PATH 极简（/usr/bin:/bin），"缺失才注入"会让 agent 永远找不到 npm 全局
        // 安装（见 AgentConfig 文档与 docs/buzz-port-sync.md）。
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        // Spawn the agent in its own process group so SIGKILL doesn't propagate
        // to the harness's own process group on Unix.
        // tokio::process::Command::process_group is a stable tokio API (no extra imports needed).
        #[cfg(unix)]
        cmd.process_group(0);

        // Suppress the console window that Windows otherwise allocates for every
        // console-subsystem child process spawned from a GUI/non-console parent.
        configure_no_window(&mut cmd);

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::Protocol("failed to open agent stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::Protocol("failed to open agent stdout".into()))?;

        Ok(Self::from_transport(
            Some(child),
            Box::new(stdin),
            Box::new(stdout),
        ))
    }

    /// 协议层公共构造：子进程管道（生产 spawn）与进程内 duplex（测试 connect）
    /// 共用——帧编解码、读循环、steer 记账完全同码。
    fn from_transport(
        child: Option<Child>,
        stdin: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        stdout: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    ) -> Self {
        Self {
            child,
            stdin,
            reader: FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_SIZE)),
            next_id: 0,
            pending_permission_id: None,
            permission_responded: false,
            last_prompt_id: None,
            current_hard_deadline: None,
            active_run_id: None,
            steering_supported: false,
            steer_rx: None,
            turn_text: String::new(),
            turn_session: None,
        }
    }

    /// 进程内双工通道构造（测试专用）：对端是 tokio task 而非子进程——
    /// 原 bash 脚本 mock 依赖 Unix shell 语义（sleep/cat/路径内嵌），windows 上
    /// 起进程即退（CI 31 例 AgentExited）；duplex 三平台行为一致且更快。
    /// 协议层与 [`Self::spawn`] 完全同码（见 [`Self::from_transport`]）。
    #[cfg(test)]
    fn connect(io: tokio::io::DuplexStream) -> Self {
        let (reader, stdin) = tokio::io::split(io);
        Self::from_transport(None, Box::new(stdin), Box::new(reader))
    }

    /// Start a new turn: clear accumulated reply text. Must be called by the
    /// dispatch path once per prompt task, before the first prompt of the turn.
    pub fn begin_turn(&mut self) {
        self.turn_text.clear();
        self.turn_session = None;
    }

    /// Drain the turn's accumulated agent text (the reply to deliver).
    pub fn take_turn_text(&mut self) -> String {
        std::mem::take(&mut self.turn_text)
    }

    /// Send the `initialize` request and return the agent's response result value.
    ///
    /// Must be called exactly once, before any other ACP method.
    /// The caller may inspect `agentCapabilities` in the returned value.
    ///
    /// Records `_meta.steering.supported` into
    /// [`steering_supported`](Self::steering_supported) so the read loop's steer
    /// arm can choose [`ACP_STEER_METHOD`] for adapters that implement it.
    /// Parsed here rather than at each call site so no caller can forget it.
    pub async fn initialize(&mut self) -> Result<serde_json::Value, AcpError> {
        // Requesting version 2 is an intentional temporary pin — we are squatting
        // on ACP v2 ahead of the upstream ACP RFD. Revisit when that RFD merges.
        let params = build_initialize_params();
        let result = self.send_request("initialize", params).await?;
        self.steering_supported = result
            .pointer("/_meta/steering/supported")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        tracing::debug!(target: "acp::init", "initialize response: {result}");
        Ok(result)
    }

    /// Send `session/new` and return the full response alongside the session ID.
    ///
    /// `cwd` must be an absolute path. `mcp_servers` may be empty.
    ///
    /// `system_prompt` controls how the prompt text is delivered:
    ///
    /// - `None` — no system-prompt field in the request (legacy framing).
    /// - `Some(SystemPromptTransport::Field(text))` — bare `systemPrompt` field
    ///   (ACP protocol v2, buzz-agent, goose unused).
    /// - `Some(SystemPromptTransport::ClaudeMeta(text))` — `_meta.systemPrompt`
    ///   as `{"append": text}`, keeping claude-agent-acp's native preset intact.
    ///
    /// `session_title` rides in `_meta.sessionTitle` when `Some`; `_meta` is
    /// omitted entirely otherwise, since adapters may distinguish an absent
    /// member from a null one. When both `ClaudeMeta` and `session_title` are
    /// present the two `_meta` members are merged into a single object.
    ///
    /// Callers use [`extract_model_config_options`] and [`extract_model_state`]
    /// to pull model info from the raw result.
    pub async fn session_new_full(
        &mut self,
        cwd: &str,
        mcp_servers: Vec<McpServer>,
        system_prompt: Option<SystemPromptTransport<'_>>,
        session_title: Option<&str>,
    ) -> Result<SessionNewResponse, AcpError> {
        let mut params = serde_json::json!({
            "cwd": cwd,
            "mcpServers": mcp_servers,
        });
        match system_prompt {
            Some(SystemPromptTransport::Field(sp)) => {
                params["systemPrompt"] = serde_json::Value::String(sp.to_owned());
            }
            Some(SystemPromptTransport::ClaudeMeta(sp)) => {
                // Merge into _meta so sessionTitle (set below) is not clobbered.
                params["_meta"]["systemPrompt"] = serde_json::json!({ "append": sp });
            }
            None => {}
        }
        if let Some(title) = session_title {
            // Merge — _meta may already carry systemPrompt from ClaudeMeta above.
            params["_meta"]["sessionTitle"] = serde_json::Value::String(title.to_owned());
        }
        let result = self.send_request("session/new", params).await?;
        let session_id = result["sessionId"]
            .as_str()
            .ok_or_else(|| AcpError::Protocol("session/new response missing sessionId".into()))?
            .to_owned();
        tracing::info!(target: "acp::session", "session created: {session_id}");
        Ok(SessionNewResponse {
            session_id,
            raw: result,
        })
    }

    /// Send `session/new` and return only the `sessionId` string.
    ///
    /// Convenience wrapper around [`session_new_full`].
    #[allow(dead_code)] // Public API — callers outside the harness may use this.
    pub async fn session_new(
        &mut self,
        cwd: &str,
        mcp_servers: Vec<McpServer>,
        system_prompt: Option<SystemPromptTransport<'_>>,
        session_title: Option<&str>,
    ) -> Result<String, AcpError> {
        Ok(self
            .session_new_full(cwd, mcp_servers, system_prompt, session_title)
            .await?
            .session_id)
    }

    /// Replace Goose's native system prompt after `session/new`.
    pub async fn session_set_goose_system_prompt(
        &mut self,
        session_id: &str,
        text: &str,
    ) -> Result<serde_json::Value, AcpError> {
        self.send_request(
            "_goose/unstable/session/system-prompt/set",
            serde_json::json!({
                "sessionId": session_id,
                "mode": "set",
                "key": "buzz",
                "text": text,
            }),
        )
        .await
    }

    /// Send `session/prompt` with idle-based timeout instead of wall-clock.
    ///
    /// The idle deadline resets on any stdout activity from the agent. The hard
    /// deadline is an absolute wall-clock cap (safety valve).
    pub async fn session_prompt_with_idle_timeout(
        &mut self,
        session_id: &str,
        prompt_text: &str,
        idle_timeout: std::time::Duration,
        max_duration: std::time::Duration,
    ) -> Result<StopReason, AcpError> {
        self.session_prompt_blocks_with_idle_timeout(
            session_id,
            std::slice::from_ref(&prompt_text),
            idle_timeout,
            max_duration,
        )
        .await
    }

    /// Like [`session_prompt_with_idle_timeout`](Self::session_prompt_with_idle_timeout),
    /// but sends each entry in `prompt_blocks` as a separate text content block.
    ///
    /// Used for slash-command pass-through: ACP connectors detect commands via
    /// the **first** block's text starting with `/`, so the harness sends
    /// `["/cmd args", "<buzz context>"]` instead of one wrapped block.
    pub async fn session_prompt_blocks_with_idle_timeout(
        &mut self,
        session_id: &str,
        prompt_blocks: &[&str],
        idle_timeout: std::time::Duration,
        max_duration: std::time::Duration,
    ) -> Result<StopReason, AcpError> {
        let params = build_prompt_params(session_id, prompt_blocks);
        let hard_deadline = tokio::time::Instant::now() + max_duration;
        self.current_hard_deadline = Some(hard_deadline);
        // Attribute this turn's `agent_message_chunk` deltas to `session_id`
        // (see `turn_session`). Steer re-prompts reuse the same session and
        // therefore keep accumulating into `turn_text`.
        self.turn_session = Some(session_id.to_string());

        self.last_prompt_id = Some(self.next_id);
        let id = self.next_id;
        self.next_id += 1;

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": params,
        });

        tracing::debug!(target: "acp::wire", "→ {}", &serde_json::to_string(&msg).unwrap_or_default());
        if let Err(e) = self.write_ndjson(&msg).await {
            self.last_prompt_id = None;
            self.current_hard_deadline = None;
            return Err(e);
        }

        let result = self
            .read_until_response_with_idle_timeout(
                session_id,
                id,
                idle_timeout,
                hard_deadline,
                max_duration,
            )
            .await;

        // On timeout errors, leave current_hard_deadline set so cancel_with_cleanup
        // can inherit the remaining budget. Clear it on all other outcomes.
        match &result {
            Ok(_) => {
                self.last_prompt_id = None;
                self.current_hard_deadline = None;
            }
            Err(AcpError::IdleTimeout(_) | AcpError::HardTimeout { .. }) => {
                // Leave last_prompt_id and current_hard_deadline set —
                // caller will invoke cancel_with_cleanup.
            }
            Err(_) => {
                self.last_prompt_id = None;
                self.current_hard_deadline = None;
            }
        }
        self.parse_prompt_response(&result?)
    }

    /// Send a `session/cancel` **notification** (no `id` field, no response expected).
    ///
    /// After calling this, the agent will eventually respond to the in-flight
    /// `session/prompt` with `stopReason: "cancelled"`. Use
    /// [`cancel_with_cleanup`](Self::cancel_with_cleanup) if you need to drain
    /// that response.
    ///
    /// Note: async because writing to stdin requires async I/O.
    pub async fn session_cancel(&mut self, session_id: &str) -> Result<(), AcpError> {
        let params = serde_json::json!({
            "sessionId": session_id,
        });
        self.send_notification("session/cancel", params).await
    }

    /// Returns `true` if a `session/prompt` request is currently in flight.
    pub fn has_in_flight_prompt(&self) -> bool {
        self.last_prompt_id.is_some()
    }

    /// Most recently observed goose `_meta.goose.activeRunId` from a
    /// `session_info_update`, if any.
    ///
    /// Both goose and buzz-agent emit `session_info_update`; other agents
    /// leave this `None` for the lifetime of the client. Read directly by
    /// `read_until_response_with_idle_timeout`'s
    /// steer arm at write time (see [`crate::buzz::pool::SteerRequest`] for
    /// why the read loop owns this); production callers do not need this
    /// accessor. Kept as `pub` so tests can introspect the field.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn active_run_id(&self) -> Option<&str> {
        self.active_run_id.as_deref()
    }

    /// Whether the agent advertised the [`ACP_STEER_METHOD`] extension at
    /// `initialize` time (`_meta.steering.supported`).
    ///
    /// The read loop's steer arm reads the field directly; this accessor exists
    /// for the supervisor's post-initialize log line.
    #[cfg(test)]
    pub fn steering_supported(&self) -> bool {
        self.steering_supported
    }

    /// Install a per-turn steer request channel for goose-native
    /// non-cancelling mid-turn delivery.
    ///
    /// Called by the dispatch path immediately before
    /// [`session_prompt_with_idle_timeout`] for all prompt tasks.
    /// The matching `Sender` is stored in `TaskMeta.steer_tx` for the
    /// main loop's mode-gate fork to drive.
    ///
    /// Panics if a receiver is already installed — there is exactly one
    /// turn per `AcpClient` at a time, and stacking receivers would
    /// silently misroute steer requests across turns. The previous
    /// turn's receiver must have been consumed by the read loop and
    /// dropped at scope exit before the next turn dispatches.
    pub fn install_steer_rx(
        &mut self,
        rx: tokio::sync::mpsc::Receiver<crate::buzz::pool::SteerRequest>,
    ) {
        assert!(
            self.steer_rx.is_none(),
            "install_steer_rx: previous turn's receiver was not consumed — \
             stacking receivers would misroute steer requests across turns"
        );
        self.steer_rx = Some(rx);
    }

    /// Clear any installed steer receiver without consuming it.
    ///
    /// Called by `send_prompt_result` on every exit path of `run_prompt_task`
    /// so that `install_steer_rx`'s `is_none()` invariant holds for the next
    /// dispatch even when the turn ended before the read loop ran `take()`.
    /// Idempotent — safe to call when `steer_rx` is already `None`.
    pub fn clear_steer_rx(&mut self) {
        self.steer_rx = None;
    }

    /// Cancel a turn cleanly, handling any pending permission request first.
    ///
    /// Steps:
    /// 1. If there is a pending `session/request_permission` that hasn't been
    ///    responded to yet, respond with `outcome: "cancelled"`.
    /// 2. Send `session/cancel` notification (no id).
    /// 3. Continue reading until the `session/prompt` response arrives with `stopReason: "cancelled"`.
    ///
    /// Returns the final [`StopReason`] (almost always [`StopReason::Cancelled`]).
    pub async fn cancel_with_cleanup(
        &mut self,
        session_id: &str,
        _idle_timeout: std::time::Duration,
    ) -> Result<StopReason, AcpError> {
        // Inherit the hard deadline from the timed-out turn so the drain loop
        // doesn't start a fresh timer (prevents double-jeopardy). If the original
        // deadline is already expired or near-expired, grant a 30s floor so the
        // cancel notification has time to propagate and the agent can respond.
        let stored_deadline = self.current_hard_deadline.take();
        let min_cleanup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let hard_deadline = match stored_deadline {
            Some(d) if d > min_cleanup_deadline => d,
            Some(_) => {
                tracing::debug!(
                    "original hard deadline expired or near-expired — using 30s cleanup grace"
                );
                min_cleanup_deadline
            }
            None => {
                tracing::warn!(
                    "cancel_with_cleanup called without current_hard_deadline — using 30s fallback"
                );
                min_cleanup_deadline
            }
        };

        self.cancel_with_cleanup_until(session_id, hard_deadline)
            .await
    }

    /// Cancel a user-interrupted turn with a bounded grace window.
    ///
    /// Some ACP servers currently keep streaming after `session/cancel`. For an
    /// explicit Stop button, waiting until the original turn deadline can make
    /// cancellation look broken. This variant gives the agent a short chance to
    /// acknowledge cancellation, then returns a timeout so the caller can respawn
    /// the agent process and actually stop the work.
    ///
    /// The `grace` window is a cleanup deadline, not the turn's real max-turn
    /// wall clock — a bounded drain that expires maps to
    /// [`AcpError::CancelDrainTimeout`], never [`AcpError::HardTimeout`], so
    /// callers can distinguish "agent didn't stop in time" from a genuine
    /// configured hard-cap breach.
    pub async fn cancel_with_cleanup_grace(
        &mut self,
        session_id: &str,
        grace: std::time::Duration,
    ) -> Result<StopReason, AcpError> {
        let _ = self.current_hard_deadline.take();
        let hard_deadline = tokio::time::Instant::now() + grace;
        match self
            .cancel_with_cleanup_until(session_id, hard_deadline)
            .await
        {
            Err(AcpError::HardTimeout { .. }) => Err(AcpError::CancelDrainTimeout(grace)),
            other => other,
        }
    }

    async fn cancel_with_cleanup_until(
        &mut self,
        session_id: &str,
        hard_deadline: tokio::time::Instant,
    ) -> Result<StopReason, AcpError> {
        // Validate precondition before any side effects — fail fast if there's
        // no in-flight prompt (prevents writing permission responses or cancel
        // notifications to the agent when no prompt is active).
        let prompt_id = self.last_prompt_id.take().ok_or_else(|| {
            AcpError::Protocol("cancel_with_cleanup called with no in-flight prompt".into())
        })?;

        // Step 1: respond to any pending permission request with "cancelled",
        // but only if we haven't already responded (guards against double-response race).
        if let Some(perm_id) = self.pending_permission_id.clone() {
            if !self.permission_responded {
                let response = permission_response_cancelled(&perm_id);
                self.write_ndjson(&response).await?;
                tracing::debug!(
                    target: "acp::cancel",
                    "responded cancelled to pending permission id={perm_id}"
                );
            }
            self.pending_permission_id = None;
            self.permission_responded = false;
        }

        // Step 2: send session/cancel notification (no id)
        self.session_cancel(session_id).await?;
        tracing::info!(target: "acp::cancel", "sent session/cancel for {session_id}");
        // Use a fixed 30s idle timeout during cleanup — the cancel notification
        // needs time to propagate and the agent may go silent while winding down.
        // The separate hard_deadline bounds agents that keep producing output
        // but ignore cancellation.
        let cleanup_idle = std::time::Duration::from_secs(30);
        let remaining = hard_deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_default();
        let result = self
            .read_until_response_with_idle_timeout(
                session_id,
                prompt_id,
                cleanup_idle,
                hard_deadline,
                remaining,
            )
            .await?;
        self.parse_prompt_response(&result)
    }

    /// Serialize `value` as a single NDJSON line and flush to the agent's stdin.
    ///
    /// Bounded by a 30-second write timeout. If the agent stops reading stdin
    /// (e.g., it's stuck or dead), the write would otherwise block forever.
    async fn write_ndjson(&mut self, value: &serde_json::Value) -> Result<(), AcpError> {
        const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let line = serde_json::to_string(value)?;
        tokio::time::timeout(WRITE_TIMEOUT, async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.write_all(b"\n").await?;
            self.stdin.flush().await?;
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|_| AcpError::WriteTimeout(WRITE_TIMEOUT))?
        .map_err(AcpError::Io)?;
        Ok(())
    }

    /// Default timeout for non-prompt RPCs (initialize, session/new, etc.).
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Send a JSON-RPC request and wait for the matching response.
    ///
    /// Assigns the next available id, writes the NDJSON line to stdin,
    /// then calls [`read_until_response`](Self::read_until_response).
    ///
    /// The write phase is bounded by `WRITE_TIMEOUT` (30s) and the read phase
    /// by `REQUEST_TIMEOUT` (60s), so worst-case wall clock is ~90s. Non-prompt
    /// RPCs like `initialize` and `session/new` should complete in seconds;
    /// if they don't, the agent is likely stuck and we must not block forever.
    async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AcpError> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        tracing::debug!(target: "acp::wire", "→ {}", &serde_json::to_string(&msg).unwrap_or_default());

        // Wrap write + read in a single timeout so a hung agent can't block forever.
        // We cannot use an async block that borrows `self` mutably across two awaits
        // inside timeout(), so we sequence them with early-return on timeout.
        let timeout = Self::REQUEST_TIMEOUT;
        match tokio::time::timeout(timeout, self.write_ndjson(&msg)).await {
            Ok(result) => result?,
            Err(_) => return Err(AcpError::Timeout(timeout)),
        }

        match tokio::time::timeout(timeout, self.read_until_response(id)).await {
            Ok(result) => result,
            Err(_) => Err(AcpError::Timeout(timeout)),
        }
    }

    /// Drain any buffered lines from the agent's stdout without blocking.
    ///
    /// After a [`AcpError::Timeout`] from [`send_request`], the agent may
    /// eventually send the late response. That stale message will sit in the
    /// `BufReader` buffer and be silently skipped by the next `read_until_response`
    /// call (ID mismatch). However, if the caller wants a clean slate — e.g.
    /// before retrying the same method — they can call this to consume any
    /// buffered data with a short deadline.
    ///
    /// This is a best-effort drain: it reads until the buffer is empty or
    /// `drain_timeout` elapses, whichever comes first. Errors are ignored.
    #[allow(dead_code)] // Scaffolding for future model-switch timeout cleanup; not yet wired.
    pub async fn drain_stale_responses(&mut self, drain_timeout: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + drain_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let read_result = tokio::time::timeout(remaining, self.reader.next()).await;
            match read_result {
                // Timeout or stream ended — buffer is empty or agent exited.
                Err(_) | Ok(None) => break,
                Ok(Some(Ok(_))) => {
                    // Consumed one buffered line; loop to drain more.
                    tracing::debug!(target: "acp::wire", "drained stale buffered line");
                }
                Ok(Some(Err(_))) => break,
            }
        }
    }

    /// Send a JSON-RPC **notification** — no `id` field, no response expected.
    ///
    /// Used for `session/cancel`. The absence of `id` is the JSON-RPC 2.0
    /// distinguisher between requests and notifications.
    async fn send_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), AcpError> {
        // Notifications deliberately have NO "id" field.
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        tracing::debug!(target: "acp::wire", "→ (notification) {}", &serde_json::to_string(&msg).unwrap_or_default());
        self.write_ndjson(&msg).await?;
        Ok(())
    }

    /// Core message loop: read NDJSON lines until we get a response matching `expected_id`.
    ///
    /// While waiting, handles:
    /// - `session/update` notifications → logged via tracing
    /// - `session/request_permission` requests → auto-approved with `allow_once`
    /// - Any other messages → debug-logged and ignored; if they carry an `id`
    ///   (i.e. they are requests, not notifications), a JSON-RPC -32601 error is sent.
    ///
    /// Compares the incoming `id` field as a `serde_json::Value` against
    /// `json!(expected_id)` so that both numeric and string IDs work correctly.
    async fn read_until_response(
        &mut self,
        expected_id: u64,
    ) -> Result<serde_json::Value, AcpError> {
        loop {
            // LinesCodec::new_with_max_length enforces MAX_LINE_SIZE at the
            // read level — the buffer never grows beyond the limit, preventing
            // OOM from rogue agents writing infinite non-newline bytes.
            let line = match self.reader.next().await {
                None => return Err(AcpError::AgentExited),
                Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                    return Err(AcpError::Protocol(
                        "agent stdout line exceeded 10MB limit".into(),
                    ));
                }
                Some(Err(e)) => {
                    return Err(AcpError::Io(std::io::Error::other(e)));
                }
                Some(Ok(line)) => line,
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Only log and reset idle after we have a valid non-empty line.
            tracing::debug!(target: "acp::wire", "← {trimmed}");

            let msg: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        target: "acp::wire",
                        "failed to parse line as JSON: {e} — skipping"
                    );
                    continue;
                }
            };

            // Check if this is a response to our expected request (has matching id
            // AND no `method` field — a `method` field means it's an agent-initiated
            // request, not a response, even if the id happens to match).
            if let Some(id) = msg.get("id") {
                if *id == serde_json::json!(expected_id) && msg.get("method").is_none() {
                    if let Some(error) = msg.get("error") {
                        return Err(agent_error_from_json(error));
                    }
                    return Ok(msg["result"].clone());
                }
            }

            // Dispatch by method name (notifications and agent-initiated requests).
            if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
                match method {
                    "session/update" => {
                        let _ = self.handle_session_update(&msg);
                    }
                    "session/request_permission" => {
                        self.handle_permission_request(&msg).await?;
                    }
                    other => {
                        // If the unknown message has an id, it's a request expecting a reply.
                        // Silence would cause the agent to hang waiting for a response.
                        // Send a JSON-RPC -32601 "Method not found" error.
                        if msg.get("id").is_some() {
                            let err_resp = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "error": {"code": -32601, "message": format!("Method not found: {other}")}
                            });
                            // Surface write failures — a broken pipe means the
                            // agent process is dead and continuing would hang.
                            self.write_ndjson(&err_resp).await?;
                        }
                        tracing::debug!(target: "acp::wire", "ignoring unknown method: {other}");
                    }
                }
            }
        }
    }

    /// Idle-aware message loop: like [`read_until_response`] but resets an idle
    /// deadline on every stdout line. Fires [`AcpError::IdleTimeout`] on silence
    /// or [`AcpError::HardTimeout`] on absolute wall-clock cap.
    ///
    /// `hard_deadline` is an absolute `Instant` (pre-computed by the caller) so
    /// that `cancel_with_cleanup` can inherit the remaining budget from the
    /// original turn rather than starting a fresh timer.
    /// Read agent messages until the response with `expected_id` arrives, or
    /// either of two timeouts fires. Returns `Result<value, IdleTimeout |
    /// HardTimeout | other>`.
    ///
    /// - `idle_timeout`: silent-agent guard, **reset on every line of valid
    ///   JSON** (and explicitly on `session/update` notifications).
    /// - `hard_deadline`: absolute wall-clock cap on the whole call, passed
    ///   in so that `cancel_with_cleanup` can inherit the remaining budget
    ///   from the original turn rather than starting a fresh timer.
    ///
    /// While reading, the loop interleaves goose-native non-cancelling steer
    /// requests via `tokio::select!`. The select uses `biased` for
    /// reader-first throughput, with a pre-select deadline check at the top
    /// of every loop iteration so a continuously-ready reader arm cannot
    /// starve the hard deadline (Max's review gate). The steer arm is
    /// guarded by `pending_steer.is_none()` so at most one steer is in
    /// flight at a time; a successful steer response is routed to the
    /// caller's oneshot ack instead of being returned as the prompt result.
    ///
    /// `session_id` is threaded in lexically by callers so the goose-native
    /// steer arm can complete `sessionId` in the steer JSON-RPC params at
    /// write time without needing access to outer state. See
    /// [`crate::buzz::pool::SteerRequest`] for why params are built here and not
    /// in the main loop.
    async fn read_until_response_with_idle_timeout(
        &mut self,
        session_id: &str,
        expected_id: u64,
        idle_timeout: std::time::Duration,
        hard_deadline: tokio::time::Instant,
        max_duration: std::time::Duration,
    ) -> Result<serde_json::Value, AcpError> {
        use tokio::time::Instant;

        // Take the per-turn steer receiver into a local so it can be
        // borrowed independently of `self.reader` inside `select!`.
        // Dropped at scope exit (return paths drain `pending_steer` first
        // so the ack_tx oneshot is never leaked silently).
        let mut steer_rx = self.steer_rx.take();

        // Tracks the in-flight steer write: `(request_id, transport, ack_tx)`.
        // While `Some`, the steer arm is gated off so we don't stack writes,
        // and a response matching `id` is routed to the ack_tx instead
        // of being treated as the prompt result. `transport` records which
        // method was written so the response arm decodes the result shape
        // that method actually returns. Drained on every return path with
        // `PromptCompletedNeutral` so callers are never left hanging.
        let mut pending_steer: Option<(
            u64,
            SteerTransport,
            tokio::sync::oneshot::Sender<crate::buzz::pool::SteerAck>,
        )> = None;

        let now = Instant::now();
        let mut idle_deadline = now + idle_timeout;
        let mut hard_deadline = hard_deadline;
        let mut last_activity_at = now;

        loop {
            // Determine which deadline fires first BEFORE sleeping — this is
            // the classification we'll use on timeout, immune to scheduler jitter.
            let idle_fires_first = idle_deadline < hard_deadline;
            let next_deadline = if idle_fires_first {
                idle_deadline
            } else {
                hard_deadline
            };

            // Pre-select deadline check — required by Max's review. Under
            // `biased`, a continuously-ready reader arm wins every poll and
            // `sleep_until(next_deadline)` is never reached, silently
            // defeating the hard-deadline guarantee for agents that keep
            // producing output (see `acp.rs:608` for why the hard deadline
            // exists). Check the classified deadline here so a steady-
            // stream agent is still bounded.
            if Instant::now() >= next_deadline {
                if let Some((_, _, ack_tx)) = pending_steer.take() {
                    // Prompt is timing out — release the withheld event via
                    // PromptCompletedNeutral (no fallback signal: there is
                    // no in-flight turn to signal once we return, and
                    // normal dispatch handles redelivery).
                    let _ = ack_tx.send(crate::buzz::pool::SteerAck::PromptCompletedNeutral);
                }
                if idle_fires_first {
                    tracing::warn!("idle timeout ({idle_timeout:?}) — no agent activity");
                    return Err(AcpError::IdleTimeout(idle_timeout));
                } else {
                    let silence = Instant::now().saturating_duration_since(last_activity_at);
                    tracing::warn!("hard turn timeout exceeded (silence {silence:?})");
                    return Err(AcpError::HardTimeout { silence });
                }
            }

            // LinesCodec::new_with_max_length enforces MAX_LINE_SIZE at the
            // read level — the buffer never grows beyond the limit.
            let read_result = tokio::select! {
                biased;
                read_result = self.reader.next() => Some(read_result),
                // Steer arm: gated off whenever a steer write is already in
                // flight so we don't stack two writes against the same
                // process. The `async { steer_rx.as_mut()?.recv().await }`
                // wrapper produces `None` when no receiver is installed,
                // which mismatches the `Some(req)` pattern and disables the
                // branch for that iteration (no busy loop). Cancel-safe:
                // `mpsc::Receiver::recv` does not lose messages on drop.
                Some(req) = async {
                    match steer_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                }, if pending_steer.is_none() => {
                    // Selected: choose the steer transport and build its
                    // params at write time using the lexical `session_id`
                    // and the freshest `active_run_id`.
                    //
                    // `active_run_id` is updated by `session/update`
                    // notifications inside this very loop; reading it here
                    // (rather than snapshotting at dispatch) guarantees the
                    // value matches what goose's run-id check will compare
                    // against.
                    //
                    // Transport precedence:
                    //   Some(run_id)              → GOOSE_STEER_METHOD. goose
                    //     wins whenever a run id exists: `expectedRunId` is
                    //     strictly more precise about *which* run is steered.
                    //   None + steering_supported → ACP_STEER_METHOD, the
                    //     cross-adapter extension (claude-agent-acp,
                    //     codex-acp), which takes no run id.
                    //   None + !steering_supported → write nothing and ack
                    //     `ExpectedRunIdMissing`; the main loop maps this to
                    //     the universal cancel+merge `Steer` fallback.
                    //
                    // The capability flag is the ONLY gate on writing
                    // ACP_STEER_METHOD. Probing an unknown method is unsafe:
                    // codex-acp answers unrecognized extension methods with
                    // `{}` — a JSON-RPC success — which would be read as a
                    // delivered steer and silently drop the user's message.
                    let prompt_block_refs: Vec<&str> =
                        req.prompt_blocks.iter().map(String::as_str).collect();
                    let selected = match (&self.active_run_id, self.steering_supported) {
                        (Some(run_id), _) => Some((
                            SteerTransport::Goose,
                            GOOSE_STEER_METHOD,
                            build_goose_steer_params(session_id, run_id, &prompt_block_refs),
                        )),
                        (None, true) => Some((
                            SteerTransport::AcpExtension,
                            ACP_STEER_METHOD,
                            build_acp_steer_params(session_id, &prompt_block_refs),
                        )),
                        (None, false) => None,
                    };
                    match selected {
                        None => {
                            tracing::warn!(
                                "steer: no active_run_id and agent did not advertise \
                                 {ACP_STEER_METHOD} — falling back to cancel+merge"
                            );
                            let _ = req.ack_tx.send(crate::buzz::pool::SteerAck::Err(
                                crate::buzz::pool::SteerError::ExpectedRunIdMissing,
                            ));
                        }
                        Some((transport, method, params)) => {
                            let id = self.next_id;
                            self.next_id += 1;
                            let msg = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "method": method,
                                "params": params,
                            });
                            tracing::debug!(
                                target: "acp::wire",
                                "→ {}",
                                serde_json::to_string(&msg).unwrap_or_default()
                            );
                            match self.write_ndjson(&msg).await {
                                Ok(()) => {
                                    pending_steer = Some((id, transport, req.ack_tx));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "steer write failed ({method}): {e} — releasing withheld event"
                                    );
                                    let _ = req.ack_tx.send(crate::buzz::pool::SteerAck::Err(
                                        crate::buzz::pool::SteerError::Transport(e.to_string()),
                                    ));
                                }
                            }
                        }
                    }
                    // Loop back to the next iteration without consuming a
                    // reader line; we'll wait for either the prompt
                    // response or the steer response next.
                    None
                }
                _ = tokio::time::sleep_until(next_deadline) => {
                    // The pre-select check at the top of the next iteration
                    // would catch this anyway, but firing the deadline arm
                    // here makes the wakeup immediate (no extra reader poll
                    // round-trip when stdout is idle).
                    if let Some((_, _, ack_tx)) = pending_steer.take() {
                        let _ = ack_tx.send(crate::buzz::pool::SteerAck::PromptCompletedNeutral);
                    }
                    if idle_fires_first {
                        tracing::warn!("idle timeout ({idle_timeout:?}) — no agent activity");
                        return Err(AcpError::IdleTimeout(idle_timeout));
                    } else {
                        let silence = Instant::now().saturating_duration_since(last_activity_at);
                        tracing::warn!("hard turn timeout exceeded (silence {silence:?})");
                        return Err(AcpError::HardTimeout { silence });
                    }
                }
            };

            // Steer arm fired (or the select selected nothing read-side this
            // iteration): no reader frame to process, loop to re-evaluate
            // deadlines and arm the next select.
            let read_result = match read_result {
                Some(r) => r,
                None => continue,
            };

            match read_result {
                None => {
                    if let Some((_, _, ack_tx)) = pending_steer.take() {
                        let _ = ack_tx.send(crate::buzz::pool::SteerAck::PromptCompletedNeutral);
                    }
                    return Err(AcpError::AgentExited);
                }
                Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                    if let Some((_, _, ack_tx)) = pending_steer.take() {
                        let _ = ack_tx.send(crate::buzz::pool::SteerAck::PromptCompletedNeutral);
                    }
                    return Err(AcpError::Protocol(
                        "agent stdout line exceeded 10MB limit".into(),
                    ));
                }
                Some(Err(e)) => {
                    if let Some((_, _, ack_tx)) = pending_steer.take() {
                        let _ = ack_tx.send(crate::buzz::pool::SteerAck::PromptCompletedNeutral);
                    }
                    return Err(AcpError::Io(std::io::Error::other(e)));
                }
                Some(Ok(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    tracing::debug!(target: "acp::wire", "← {trimmed}");

                    let msg: serde_json::Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                target: "acp::wire",
                                "failed to parse line as JSON: {e} — skipping"
                            );
                            continue;
                        }
                    };

                    let activity_now = Instant::now();
                    idle_deadline = activity_now + idle_timeout;
                    last_activity_at = activity_now;

                    // Steer response routing must come BEFORE the prompt
                    // response check: a steer response is a regular
                    // JSON-RPC response (id + result/error, no method),
                    // so the matcher must disambiguate by id. Both checks
                    // share the `no method` guard.
                    if let Some(id) = msg.get("id") {
                        if msg.get("method").is_none() {
                            if let Some((steer_id, _, _)) = pending_steer.as_ref() {
                                if *id == serde_json::json!(*steer_id) {
                                    // Take the ack_tx out and route the
                                    // response. We do not return — keep
                                    // reading until the prompt response
                                    // arrives.
                                    let (_, transport, ack_tx) =
                                        pending_steer.take().expect("just checked");
                                    let ack = if let Some(error) = msg.get("error") {
                                        let code = error
                                            .get("code")
                                            .and_then(|c| c.as_i64())
                                            .unwrap_or(-1);
                                        let message = error.to_string();
                                        crate::buzz::pool::SteerAck::Err(
                                            crate::buzz::pool::SteerError::AgentError {
                                                code,
                                                message,
                                            },
                                        )
                                    } else {
                                        // Success result. Whether it counts as
                                        // a delivered steer — and whether the
                                        // turn Buzz awaits is still running —
                                        // depends on the transport.
                                        let outcome = match transport {
                                            // goose returns no outcome field;
                                            // a success response means the
                                            // steer landed in the live run.
                                            SteerTransport::Goose => Some(STEER_OUTCOME_INJECTED),
                                            // The outcome must be positively
                                            // recognized. An unknown or absent
                                            // value (codex-acp answers
                                            // unrecognized ext methods with a
                                            // bare `{}`) is a rejection, never
                                            // a delivery — treating it as
                                            // success would drop the event.
                                            SteerTransport::AcpExtension => msg
                                                .pointer("/result/outcome")
                                                .and_then(|v| v.as_str())
                                                .filter(|o| {
                                                    *o == STEER_OUTCOME_INJECTED
                                                        || *o == STEER_OUTCOME_STARTED_NEW_TURN
                                                }),
                                        };
                                        match outcome {
                                            Some(STEER_OUTCOME_STARTED_NEW_TURN) => {
                                                // Delivered, but into a NEW
                                                // turn: the one this read loop
                                                // is awaiting had already
                                                // finished. Renewing the hard
                                                // deadline here would extend
                                                // the clock on a settled turn,
                                                // so leave it alone and let the
                                                // prompt response land on its
                                                // original budget.
                                                tracing::info!(
                                                    "steer accepted as {STEER_OUTCOME_STARTED_NEW_TURN}: \
                                                     awaited turn had ended — hard deadline not renewed"
                                                );
                                                crate::buzz::pool::SteerAck::Success {
                                                    session_id: session_id.to_owned(),
                                                }
                                            }
                                            Some(_) => {
                                                let renew_now = Instant::now();
                                                let new_deadline = renew_now + max_duration;
                                                if new_deadline > hard_deadline {
                                                    hard_deadline = new_deadline;
                                                    self.current_hard_deadline = Some(new_deadline);
                                                    tracing::info!(
                                                        "steer success: renewed hard deadline ({max_duration:?} from now)"
                                                    );
                                                }
                                                crate::buzz::pool::SteerAck::Success {
                                                    session_id: session_id.to_owned(),
                                                }
                                            }
                                            None => {
                                                // Report the raw string when
                                                // there is one, so logs read
                                                // `failed` not `"failed"`;
                                                // fall back to the JSON for a
                                                // non-string value.
                                                let reported = match msg.pointer("/result/outcome")
                                                {
                                                    None => "<absent>".to_string(),
                                                    Some(serde_json::Value::String(s)) => s.clone(),
                                                    Some(other) => other.to_string(),
                                                };
                                                tracing::warn!(
                                                    "steer rejected: {ACP_STEER_METHOD} returned \
                                                     unrecognized outcome {reported} — releasing \
                                                     withheld event for cancel+merge"
                                                );
                                                crate::buzz::pool::SteerAck::Err(
                                                    crate::buzz::pool::SteerError::OutcomeRejected {
                                                        outcome: reported,
                                                    },
                                                )
                                            }
                                        }
                                    };
                                    let _ = ack_tx.send(ack);
                                    continue;
                                }
                            }
                            if *id == serde_json::json!(expected_id) {
                                if let Some(error) = msg.get("error") {
                                    if let Some((_, _, ack_tx)) = pending_steer.take() {
                                        let _ = ack_tx.send(
                                            crate::buzz::pool::SteerAck::PromptCompletedNeutral,
                                        );
                                    }
                                    return Err(agent_error_from_json(error));
                                }
                                if let Some((_, _, ack_tx)) = pending_steer.take() {
                                    let _ = ack_tx
                                        .send(crate::buzz::pool::SteerAck::PromptCompletedNeutral);
                                }
                                return Ok(msg["result"].clone());
                            }
                        }
                    }

                    // Dispatch notifications and agent-initiated requests.
                    if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
                        match method {
                            "session/update" => {
                                if self.handle_session_update(&msg) {
                                    let activity_now = Instant::now();
                                    idle_deadline = activity_now + idle_timeout;
                                    last_activity_at = activity_now;
                                    tracing::debug!("idle clock reset: tool call started");
                                }
                            }
                            "session/request_permission" => {
                                self.handle_permission_request(&msg).await?;
                            }
                            other => {
                                // If the unknown message has an id, it's a request expecting a reply.
                                // Silence would cause the agent to hang waiting for a response.
                                // Send a JSON-RPC -32601 "Method not found" error.
                                if msg.get("id").is_some() {
                                    let err_resp = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": msg["id"],
                                        "error": {"code": -32601, "message": format!("Method not found: {other}")}
                                    });
                                    // Surface write failures — a broken pipe means the
                                    // agent process is dead and continuing would hang.
                                    self.write_ndjson(&err_resp).await?;
                                }
                                tracing::debug!(target: "acp::wire", "ignoring unknown method: {other}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// Log a `session/update` notification via tracing.
    ///
    /// The discriminator field is `sessionUpdate` (not `type`) per the ACP schema.
    /// Returns `true` if the update indicates a tool call started, signaling that
    /// the idle clock should be explicitly reset (the agent will be silent while
    /// the tool executes).
    ///
    /// Takes `&mut self` (not `&self`) because some updates carry agent state
    /// the client must observe — notably goose's `session_info_update` with
    /// `_meta.goose.activeRunId`, which seeds [`active_run_id`](Self::active_run_id)
    /// so the steer arm can target `_goose/unstable/session/steer` at the
    /// correct run. Agents that never emit it (claude-agent-acp, codex-acp)
    /// leave it `None` and are steered via `_session/steering` instead, which
    /// needs no run id.
    fn handle_session_update(&mut self, msg: &serde_json::Value) -> bool {
        let update = &msg["params"]["update"];
        let update_type = update
            .get("sessionUpdate")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        match update_type {
            "agent_message_chunk" => {
                // ABB #200 Phase 3: accumulate the agent's reply text delta into
                // `turn_text` for synchronous delivery at turn end. Filtered by
                // session id — steer turns reuse the same session and keep
                // accumulating; a straggler chunk from another session (e.g. a
                // cancelled turn draining after a respawn) must not bleed into
                // this turn's reply. Agents omitting `params.sessionId` are
                // excluded (schema requires it; integration tests pin pi-acp).
                if let Some(text) = update["content"]["text"].as_str() {
                    if msg["params"]["sessionId"].as_str() == self.turn_session.as_deref() {
                        self.turn_text.push_str(text);
                    }
                }
                false
            }
            "tool_call" => {
                let title = update
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let kind = update
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                tracing::info!(target: "acp::tool", "tool_call: {title} ({kind})");
                true
            }
            "tool_call_update" => {
                let tool_id = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                tracing::info!(target: "acp::tool", "tool_call_update: {tool_id} → {status}");
                false
            }
            "plan" => {
                tracing::info!(target: "acp::plan", "plan update received");
                false
            }
            "agent_thought_chunk" => {
                if let Some(text) = update["content"]["text"].as_str() {
                    tracing::debug!(target: "acp::thought", "{text}");
                }
                false
            }
            "available_commands_update" => {
                // Advertised slash commands (ACP slash-commands extension).
                // Logged for observability; UI surfacing is a follow-up.
                let names: Vec<&str> = update["availableCommands"]
                    .as_array()
                    .map(|cmds| cmds.iter().filter_map(|c| c["name"].as_str()).collect())
                    .unwrap_or_default();
                tracing::info!(
                    target: "acp::update",
                    "available_commands_update: {} commands [{}]",
                    names.len(),
                    names.join(", ")
                );
                false
            }
            "session_info_update" => {
                // Both goose and buzz-agent emit `session_info_update` with
                // `_meta.goose.activeRunId`: the id of the currently-active
                // prompt run, or `null` when the run has cleared. Other agents
                // don't emit this field; for them `active_run_id` stays `None`
                // and steer callers will fall back to cancel+merge.
                //
                // Per the ACP `SessionInfoUpdate` schema, `_meta` is a field
                // on the update object itself — nested inside `update`, not
                // alongside it at the params level. Goose and buzz-agent both
                // emit it at `params.update._meta.goose.activeRunId`.
                let meta = msg["params"]["update"]
                    .get("_meta")
                    .and_then(|m| m.get("goose"));
                if let Some(goose_meta) = meta {
                    match goose_meta.get("activeRunId") {
                        Some(serde_json::Value::String(run_id)) => {
                            tracing::debug!(
                                target: "acp::update",
                                "session_info_update: activeRunId={run_id}"
                            );
                            self.active_run_id = Some(run_id.clone());
                        }
                        Some(serde_json::Value::Null) => {
                            tracing::debug!(
                                target: "acp::update",
                                "session_info_update: activeRunId cleared"
                            );
                            self.active_run_id = None;
                        }
                        // Missing or non-string/null — leave state untouched.
                        _ => {}
                    }
                }
                false
            }
            "keepalive" => false,
            other => {
                tracing::debug!(target: "acp::update", "session/update: {other}");
                false
            }
        }
    }

    /// Auto-approve a `session/request_permission` request from the agent.
    ///
    /// Finds the option with `kind == "allow_once"` and responds with its `optionId`.
    /// If no `allow_once` option exists, falls back to `reject_once`.
    ///
    /// **Critical:** Never hardcode `optionId` — always find it dynamically by `kind`.
    ///
    /// The request `id` is stored as `serde_json::Value` to support both numeric
    /// and string IDs per JSON-RPC 2.0.
    async fn handle_permission_request(&mut self, msg: &serde_json::Value) -> Result<(), AcpError> {
        // Extract id as a Value — JSON-RPC 2.0 allows both numeric and string IDs.
        let id = msg
            .get("id")
            .cloned()
            .ok_or_else(|| AcpError::Protocol("permission request missing id".into()))?;

        // Store pending permission id so cancel_with_cleanup can respond to it.
        self.pending_permission_id = Some(id.clone());
        // Mark as not yet responded — guards against double-response race.
        self.permission_responded = false;

        let options = msg["params"]["options"]
            .as_array()
            .ok_or_else(|| AcpError::Protocol("permission request missing options".into()))?;

        tracing::debug!(
            target: "acp::permission",
            "session/request_permission id={id}, {} options",
            options.len()
        );

        // Find allow_once by kind — NEVER hardcode optionId.
        let allow_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("allow_once"));

        let response = if let Some(opt) = allow_once {
            let option_id = opt["optionId"]
                .as_str()
                .ok_or_else(|| AcpError::Protocol("allow_once option missing optionId".into()))?;
            tracing::info!(
                target: "acp::permission",
                "auto-approving permission id={id} with allow_once optionId={option_id:?}"
            );
            permission_response_selected(&id, option_id)
        } else {
            // No allow_once — fall back to reject_once.
            tracing::warn!(
                target: "acp::permission",
                "no allow_once option found in permission request id={id}, falling back to reject_once"
            );
            let reject = options
                .iter()
                .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("reject_once"));

            if let Some(opt) = reject {
                let option_id = opt["optionId"].as_str().unwrap_or("reject");
                permission_response_selected(&id, option_id)
            } else {
                return Err(AcpError::Protocol(
                    "no suitable permission option found (neither allow_once nor reject_once)"
                        .into(),
                ));
            }
        };

        // Write the response first, then mark as responded.
        //
        // Previous ordering (flag-before-write) was intended to guard against a
        // double-response if a timeout fires between write and flag-set. However,
        // the deadlock risk is worse: if write_ndjson fails (e.g. WriteTimeout),
        // the flag would be true but no response was actually sent. Then
        // cancel_with_cleanup would see permission_responded=true, skip sending
        // the cancelled outcome, and the agent would hang waiting for a reply
        // that never arrives — a guaranteed deadlock.
        //
        // The correct fix: set the flag AFTER a successful write. The double-
        // response window (between write completion and flag-set) is negligibly
        // small and bounded by a single memory store; the deadlock window was
        // unbounded.
        self.write_ndjson(&response).await?;
        self.permission_responded = true;
        self.pending_permission_id = None;
        Ok(())
    }

    /// Parse `stopReason` from a completed `session/prompt` response.
    fn parse_prompt_response(
        &mut self,
        result: &serde_json::Value,
    ) -> Result<StopReason, AcpError> {
        self.parse_stop_reason(result)
    }

    /// Parse `stopReason` from a `session/prompt` result value.
    fn parse_stop_reason(&self, result: &serde_json::Value) -> Result<StopReason, AcpError> {
        let raw = result["stopReason"].as_str().ok_or_else(|| {
            AcpError::Protocol("session/prompt response missing stopReason".into())
        })?;
        StopReason::from_str(raw)
            .ok_or_else(|| AcpError::Protocol(format!("unknown stopReason: {raw:?}")))
    }
}

/// Build `session/prompt` params from one or more text content blocks.
fn build_prompt_params(session_id: &str, prompt_blocks: &[&str]) -> serde_json::Value {
    let blocks: Vec<serde_json::Value> = prompt_blocks
        .iter()
        .map(|text| serde_json::json!({ "type": "text", "text": text }))
        .collect();
    serde_json::json!({
        "sessionId": session_id,
        "prompt": blocks,
    })
}

/// Build `_goose/unstable/session/steer` params from one or more text
/// content blocks plus the freshest `expectedRunId`.
///
/// Wire shape:
/// ```json
/// { "sessionId": "...", "expectedRunId": "...", "prompt": [{"type":"text","text":"..."}, ...] }
/// ```
///
/// Called from the read-loop steer arm at write time so `expectedRunId`
/// matches goose's *current* run (it advances on each `session/update`).
/// See [`crate::buzz::pool::SteerRequest`] for why this is the read loop's job
/// and not the main loop's.
fn build_goose_steer_params(
    session_id: &str,
    expected_run_id: &str,
    prompt_blocks: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "sessionId": session_id,
        "expectedRunId": expected_run_id,
        "prompt": steer_prompt_blocks(prompt_blocks),
    })
}

/// Build the params for an [`ACP_STEER_METHOD`] request.
///
/// Wire shape:
/// ```json
/// { "sessionId": "...", "prompt": [{"type":"text","text":"..."}, ...] }
/// ```
///
/// Deliberately carries **no** `expectedRunId`: the cross-adapter method
/// steers whatever turn is currently running and neither claude-agent-acp nor
/// codex-acp emits a run id to target.
fn build_acp_steer_params(session_id: &str, prompt_blocks: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "sessionId": session_id,
        "prompt": steer_prompt_blocks(prompt_blocks),
    })
}

/// Render steer body strings as ACP `text` content blocks. Shared by both
/// steer transports so the prompt shape cannot drift between them.
fn steer_prompt_blocks(prompt_blocks: &[&str]) -> Vec<serde_json::Value> {
    prompt_blocks
        .iter()
        .map(|text| serde_json::json!({ "type": "text", "text": text }))
        .collect()
}

/// Build a JSON-RPC permission response with `outcome: "selected"`.
fn permission_response_selected(id: &serde_json::Value, option_id: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "outcome": { "outcome": "selected", "optionId": option_id } }
    })
}

/// Build a JSON-RPC permission response with `outcome: "cancelled"`.
fn permission_response_cancelled(id: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "outcome": { "outcome": "cancelled" } }
    })
}

/// Full `session/new` response — session ID plus the raw JSON result.
///
/// Callers use the extractor helpers to pull model info from `raw`.
pub struct SessionNewResponse {
    pub session_id: String,
    /// The full `result` value from the JSON-RPC response. Production code
    /// only needs the session id; the raw response is kept for the wire-level
    /// test assertions (`resp.raw["_receivedRequest"]`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub raw: serde_json::Value,
}

/// How to deliver a system prompt on `session/new`.
///
/// The two variants match the two mechanisms supported by current adapters:
///
/// - **`Field`** — bare `systemPrompt` field (ACP protocol v2, buzz-agent).
/// - **`ClaudeMeta`** — `_meta.systemPrompt: {"append": text}`, used by
///   `claude-agent-acp` to append to the adapter's own native system prompt
///   while keeping its tool-use preset intact.
#[derive(Debug, Clone, PartialEq)]
pub enum SystemPromptTransport<'a> {
    /// Deliver as a bare top-level `systemPrompt` field.
    Field(&'a str),
    /// Deliver as `_meta.systemPrompt: {"append": text}`.
    ClaudeMeta(&'a str),
}

// ─── Drop: kill child process ─────────────────────────────────────────────────

impl Drop for AcpClient {
    fn drop(&mut self) {
        // 进程内双工（测试）无子进程：半边随 Self 一起 drop 即对端 EOF。
        let Some(child) = self.child.as_mut() else {
            return;
        };
        // Best-effort SIGKILL + reap. We cannot `await` in Drop (sync context).
        // Kill the process group when possible so subprocesses don't leak.
        // Callers SHOULD still call `shutdown().await` for guaranteed reaping.
        match child.id() {
            Some(pid) if kill_process_group(pid) => {}
            _ => {
                let _ = child.start_kill();
            }
        }
        // Non-blocking reap attempt — prevents zombie accumulation in the
        // common case where SIGKILL takes effect before Drop returns.
        let _ = child.try_wait();
    }
}

/// Send SIGKILL to an entire process group. Returns `true` if the signal was sent.
///
/// The child is spawned with `process_group(0)`, so its PID equals its PGID.
/// Killing the group ensures subprocesses (MCP servers, tool processes) are
/// cleaned up rather than orphaned to init on repeated crash-recovery cycles.
///
/// Uses `nix::sys::signal::killpg` — a safe wrapper around the POSIX `killpg`
/// syscall — so the crate's `#![deny(unsafe_code)]` policy is preserved.
#[cfg(unix)]
fn kill_process_group(pid: u32) -> bool {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    // pid == pgid because the child was spawned with process_group(0).
    killpg(Pid::from_raw(pid as i32), Signal::SIGKILL).is_ok()
}

/// Fallback for non-Unix: process-group kill not available.
/// Returns `false` so the caller falls back to `child.start_kill()`.
#[cfg(not(unix))]
fn kill_process_group(_pid: u32) -> bool {
    false
}

/// Suppress the console window that Windows otherwise allocates for every
/// console-subsystem child process spawned from a GUI (non-console) parent.
/// No-op on non-Windows platforms.
fn configure_no_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_parses_all_known_values() {
        assert_eq!(StopReason::from_str("end_turn"), Some(StopReason::EndTurn));
        assert_eq!(
            StopReason::from_str("cancelled"),
            Some(StopReason::Cancelled)
        );
        assert_eq!(
            StopReason::from_str("max_tokens"),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            StopReason::from_str("max_turn_requests"),
            Some(StopReason::MaxTurnRequests)
        );
        assert_eq!(StopReason::from_str("refusal"), Some(StopReason::Refusal));
    }

    #[test]
    fn stop_reason_returns_none_for_unknown() {
        assert_eq!(StopReason::from_str("unknown_value"), None);
        assert_eq!(StopReason::from_str(""), None);
        assert_eq!(StopReason::from_str("endturn"), None); // no camelCase — still unknown
    }

    #[test]
    fn stop_reason_is_case_insensitive() {
        // Agents may send uppercase or mixed-case variants — all should parse correctly.
        assert_eq!(StopReason::from_str("END_TURN"), Some(StopReason::EndTurn));
        assert_eq!(
            StopReason::from_str("CANCELLED"),
            Some(StopReason::Cancelled)
        );
        assert_eq!(
            StopReason::from_str("Max_Tokens"),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            StopReason::from_str("MAX_TURN_REQUESTS"),
            Some(StopReason::MaxTurnRequests)
        );
        assert_eq!(StopReason::from_str("Refusal"), Some(StopReason::Refusal));
    }

    #[test]
    fn find_allow_once_by_kind_not_by_option_id() {
        // optionId values are intentionally non-obvious to prove we don't hardcode them.
        let options: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
            {"optionId": "opt-reject-42",  "name": "Reject",       "kind": "reject_once"},
            {"optionId": "opt-allow-99",   "name": "Allow once",   "kind": "allow_once"},
            {"optionId": "opt-always-7",   "name": "Always allow", "kind": "allow_always"}
        ]"#,
        )
        .unwrap();

        let allow_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("allow_once"));

        assert!(allow_once.is_some(), "should find allow_once option");
        let opt = allow_once.unwrap();
        // Found by kind, not by hardcoded optionId
        assert_eq!(opt["kind"].as_str(), Some("allow_once"));
        assert_eq!(opt["optionId"].as_str(), Some("opt-allow-99"));
    }

    #[test]
    fn find_allow_once_returns_none_when_absent() {
        let options: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
            {"optionId": "reject-1",      "name": "Reject",        "kind": "reject_once"},
            {"optionId": "reject-always", "name": "Always reject", "kind": "reject_always"}
        ]"#,
        )
        .unwrap();

        let allow_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("allow_once"));

        assert!(allow_once.is_none());
    }

    #[test]
    fn find_reject_once_fallback_when_no_allow_once() {
        let options: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"optionId": "rej-x", "name": "Reject", "kind": "reject_once"}]"#,
        )
        .unwrap();

        let allow_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("allow_once"));
        assert!(allow_once.is_none());

        let reject_once = options
            .iter()
            .find(|opt| opt.get("kind").and_then(|k| k.as_str()) == Some("reject_once"));
        assert!(reject_once.is_some());
        assert_eq!(reject_once.unwrap()["optionId"].as_str(), Some("rej-x"));
    }

    #[test]
    fn request_has_id_field() {
        let id: u64 = 42;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {}
        });
        assert!(msg.get("id").is_some(), "request must have id field");
        assert_eq!(msg["id"].as_u64(), Some(42));
        assert_eq!(msg["jsonrpc"].as_str(), Some("2.0"));
        assert_eq!(msg["method"].as_str(), Some("initialize"));
    }

    #[test]
    fn notification_has_no_id_field() {
        // session/cancel is a notification — must NOT have an id field.
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {
                "sessionId": "sess_abc123"
            }
        });
        assert!(
            msg.get("id").is_none(),
            "notification must NOT have id field"
        );
        assert_eq!(msg["jsonrpc"].as_str(), Some("2.0"));
        assert_eq!(msg["method"].as_str(), Some("session/cancel"));
    }

    #[test]
    fn initialize_request_format() {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0u64,
            "method": "initialize",
            "params": {
                "protocolVersion": 2,
                "clientCapabilities": build_client_capabilities(),
                "clientInfo": {
                    "name": "buzz-acp",
                    "version": "0.1.0"
                }
            }
        });
        assert_eq!(msg["params"]["protocolVersion"].as_u64(), Some(2));
        assert_eq!(
            msg["params"]["clientInfo"]["name"].as_str(),
            Some("buzz-acp")
        );
        assert!(msg["params"]["clientCapabilities"].is_object());
        assert_eq!(
            msg["params"]["clientCapabilities"]["auth"]["terminal"].as_bool(),
            Some(true),
            "terminal auth capability must be advertised so adapters can expose terminal login methods"
        );
        assert_eq!(
            msg["params"]["clientCapabilities"]["_meta"]["goose"]["customNotifications"].as_bool(),
            Some(true),
            "goose customNotifications capability must be advertised"
        );
    }

    #[test]
    fn session_new_mcp_server_has_required_fields() {
        // Schema requires name, command, args, env — all present, args/env may be empty.
        let server = McpServer {
            name: "test-mcp".into(),
            command: "/usr/local/bin/test-mcp-server".into(),
            args: vec![],
            env: vec![
                EnvVar {
                    name: "BUZZ_RELAY_URL".into(),
                    value: "ws://localhost:3000".into(),
                },
                EnvVar {
                    name: "BUZZ_PRIVATE_KEY".into(),
                    value: "nsec1abc".into(),
                },
            ],
        };
        let serialized = serde_json::to_value(&server).unwrap();
        assert_eq!(serialized["name"].as_str(), Some("test-mcp"));
        assert_eq!(
            serialized["command"].as_str(),
            Some("/usr/local/bin/test-mcp-server")
        );
        assert!(serialized["args"].is_array());
        assert_eq!(serialized["args"].as_array().unwrap().len(), 0);
        assert!(serialized["env"].is_array());
        assert_eq!(serialized["env"].as_array().unwrap().len(), 2);
        assert_eq!(
            serialized["env"][0]["name"].as_str(),
            Some("BUZZ_RELAY_URL")
        );
    }

    #[test]
    fn session_prompt_request_format() {
        let prompt_text = "[Buzz @mention]\nChannel: test\nFrom: npub1...\nMessage: hello";
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2u64,
            "method": "session/prompt",
            "params": {
                "sessionId": "sess_abc123",
                "prompt": [
                    { "type": "text", "text": prompt_text }
                ]
            }
        });
        assert_eq!(msg["method"].as_str(), Some("session/prompt"));
        let prompt = msg["params"]["prompt"].as_array().unwrap();
        assert_eq!(prompt.len(), 1);
        assert_eq!(prompt[0]["type"].as_str(), Some("text"));
        assert_eq!(prompt[0]["text"].as_str(), Some(prompt_text));
    }

    #[test]
    fn session_prompt_slash_command_two_block_format() {
        // Slash-command pass-through: bare command first, wrapped context second.
        let params = build_prompt_params(
            "sess_abc123",
            &[
                "/goal ship it",
                "[Buzz event: @mention]\nContent: @Eva /goal ship it",
            ],
        );
        let prompt = params["prompt"].as_array().unwrap();
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0]["type"].as_str(), Some("text"));
        assert_eq!(prompt[0]["text"].as_str(), Some("/goal ship it"));
        assert!(prompt[0]["text"].as_str().unwrap().starts_with('/'));
        assert_eq!(prompt[1]["type"].as_str(), Some("text"));
    }

    #[test]
    fn permission_response_selected_format() {
        let id: u64 = 5;
        let option_id = "opt-allow-99";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "outcome": {
                    "outcome": "selected",
                    "optionId": option_id
                }
            }
        });
        assert_eq!(response["id"].as_u64(), Some(5));
        assert_eq!(
            response["result"]["outcome"]["outcome"].as_str(),
            Some("selected")
        );
        assert_eq!(
            response["result"]["outcome"]["optionId"].as_str(),
            Some("opt-allow-99")
        );
    }

    #[test]
    fn permission_response_cancelled_format() {
        let id: u64 = 5;
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "outcome": {
                    "outcome": "cancelled"
                }
            }
        });
        assert_eq!(
            response["result"]["outcome"]["outcome"].as_str(),
            Some("cancelled")
        );
        // cancelled outcome has no optionId
        assert!(response["result"]["outcome"].get("optionId").is_none());
    }

    #[test]
    fn session_cancel_notification_has_session_id_in_params() {
        let session_id = "sess_xyz789";
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {
                "sessionId": session_id
            }
        });
        // Must have no id (notification)
        assert!(msg.get("id").is_none());
        // Must have sessionId in params
        assert_eq!(msg["params"]["sessionId"].as_str(), Some("sess_xyz789"));
    }

    #[test]
    fn permission_request_with_string_id() {
        // Verify that permission response uses the same ID type as the request.
        // JSON-RPC 2.0 permits string IDs from the agent.
        let string_id = serde_json::json!("perm-req-001");
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": string_id,
            "result": {
                "outcome": { "outcome": "selected", "optionId": "allow-once" }
            }
        });
        assert_eq!(response["id"], "perm-req-001");
        assert!(response["id"].is_string());
    }

    #[test]
    fn id_comparison_works_for_numeric_and_string() {
        // Verify json!(expected_id) comparison logic used in read_until_response.
        let expected_id: u64 = 3;
        let numeric_response_id = serde_json::json!(3u64);
        let string_response_id = serde_json::json!("3");

        // Numeric matches
        assert_eq!(numeric_response_id, serde_json::json!(expected_id));
        // String does NOT match numeric (correct — different types)
        assert_ne!(string_response_id, serde_json::json!(expected_id));
    }

    #[test]
    fn permission_cancelled_response_preserves_id_type() {
        // String ID from agent should be echoed back as string in cancelled response.
        let string_id = serde_json::json!("req-abc");
        let cancelled = serde_json::json!({
            "jsonrpc": "2.0",
            "id": string_id.clone(),
            "result": { "outcome": { "outcome": "cancelled" } }
        });
        assert_eq!(cancelled["id"], string_id);
        assert!(cancelled["id"].is_string());

        // Numeric ID from agent should be echoed back as numeric.
        let numeric_id = serde_json::json!(42u64);
        let cancelled_numeric = serde_json::json!({
            "jsonrpc": "2.0",
            "id": numeric_id.clone(),
            "result": { "outcome": { "outcome": "cancelled" } }
        });
        assert_eq!(cancelled_numeric["id"], numeric_id);
        assert!(cancelled_numeric["id"].is_number());
    }

    #[test]
    fn idle_timeout_error_includes_duration() {
        let err = AcpError::IdleTimeout(std::time::Duration::from_secs(320));
        let msg = err.to_string();
        assert!(
            msg.contains("320"),
            "IdleTimeout display should include duration: {msg}"
        );
    }

    #[test]
    fn hard_timeout_error_display() {
        let err = AcpError::HardTimeout {
            silence: std::time::Duration::from_secs(120),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Hard turn timeout"),
            "HardTimeout display: {msg}"
        );
    }

    /// 进程内 mock agent 剧本步骤（替代原 bash 脚本——sleep/cat/单引号拼接/反斜杠
    /// 路径内嵌在 windows 上语义不稳：CI 31 例 AgentExited 全部源于此）。三平台
    /// 一致、更快、无 PATH 依赖；协议字节与原 bash 版逐字一致（Emit 原文输出）。
    #[derive(Debug, Clone)]
    enum Step {
        /// 写一行（LF 结尾）；`{captured}` 占位符替换为最近一次 ReadLine 的原始行
        /// （等价原脚本的 `'"$VAR"'` 拼接）。
        Emit(String),
        /// 读客户端一行（超时 ms）；读到存入 captured，超时/EOF 继续后续步骤
        /// （等价 bash `read -t N _var`：失败不中断顺序）。
        ReadLine(u64),
        /// 静默存活 ms（模拟 agent 处理中不输出；期间不读不写）。
        SleepMs(u64),
        /// 每 every_ms 输出 line 共 count 行（0=无限直到客户端断开；等价
        /// `for i in $(seq 1 N)` / `while true` 洪水形态）。
        Flood(String, u64, u64),
        /// 把 captured 原样写入文件（steer 请求捕获断言；等价 `printf '%s' > path`）。
        CaptureTo(String),
        /// 全双工回显直到 EOF（等价 `cat`：惰性对端，请求原样反弹）。
        EchoAll,
        /// 立即关闭对端（客户端读到 EOF → AgentExited；等价 `exit 0`）。
        Exit,
    }

    use Step::{CaptureTo, EchoAll, Emit, Exit, Flood, ReadLine, SleepMs};

    /// 逐步执行剧本；跑完即 drop 半边（客户端读到 EOF，等价脚本结束进程退出）。
    #[allow(clippy::too_many_arguments)]
    async fn run_mock_agent(
        rx: tokio::io::ReadHalf<tokio::io::DuplexStream>,
        mut wx: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        steps: Vec<Step>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        // split::ReadHalf 不实现 AsyncBufRead，BufReader 包装后读行
        let mut br = tokio::io::BufReader::new(rx);
        let mut captured = String::new();
        for step in steps {
            match step {
                Emit(line) => {
                    let line = line.replace("{captured}", &captured);
                    if wx.write_all(line.as_bytes()).await.is_err()
                        || wx.write_all(b"\n").await.is_err()
                    {
                        return;
                    }
                    let _ = wx.flush().await;
                }
                ReadLine(timeout_ms) => {
                    let mut buf = Vec::new();
                    let fut = br.read_until(b'\n', &mut buf);
                    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut)
                        .await
                    {
                        Ok(Ok(0)) | Err(_) => {} // EOF/超时：继续后续步骤（bash read -t 同语义）
                        Ok(Ok(_)) => {
                            captured = String::from_utf8_lossy(&buf)
                                .trim_end_matches(['\n', '\r'])
                                .to_string();
                        }
                        Ok(Err(_)) => return, // 读错误：对端已断，结束 mock
                    }
                }
                SleepMs(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                Flood(line, every_ms, count) => {
                    let mut sent = 0u64;
                    loop {
                        if count > 0 && sent >= count {
                            break;
                        }
                        sent += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(every_ms)).await;
                        let line = line.replace("{captured}", &captured);
                        if wx.write_all(line.as_bytes()).await.is_err()
                            || wx.write_all(b"\n").await.is_err()
                        {
                            return;
                        }
                        let _ = wx.flush().await;
                    }
                }
                CaptureTo(path) => {
                    if std::fs::write(&path, captured.as_bytes()).is_err() {
                        return;
                    }
                }
                EchoAll => loop {
                    let mut buf = Vec::new();
                    match br.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {
                            if wx.write_all(&buf).await.is_err() {
                                return;
                            }
                            let _ = wx.flush().await;
                        }
                    }
                },
                Exit => return,
            }
        }
    }

    async fn spawn_script(steps: Vec<Step>) -> AcpClient {
        let (client_io, agent_io) = tokio::io::duplex(64 * 1024);
        let (agent_rx, agent_wx) = tokio::io::split(agent_io);
        tokio::spawn(run_mock_agent(agent_rx, agent_wx, steps));
        AcpClient::connect(client_io)
    }

    #[tokio::test]
    async fn idle_timeout_fires_on_silent_process() {
        let mut client = spawn_script(vec![SleepMs(10_000)]).await;
        let max_dur = std::time::Duration::from_secs(30);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_millis(100),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(
            matches!(result, Err(AcpError::IdleTimeout(_))),
            "expected IdleTimeout, got {result:?}"
        );
    }

    #[tokio::test]
    async fn hard_timeout_fires_when_deadline_is_immediate() {
        let mut client = spawn_script(vec![Flood("noise".into(), 10, 0)]).await;
        let max_dur = std::time::Duration::from_millis(1);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_secs(60),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(
            matches!(result, Err(AcpError::HardTimeout { .. })),
            "expected HardTimeout, got {result:?}"
        );
    }

    /// `cancel_with_cleanup_grace`'s bounded drain deadline must map to
    /// [`AcpError::CancelDrainTimeout`], never [`AcpError::HardTimeout`] —
    /// the two share an underlying deadline mechanism but must not share
    /// classification, since callers dead-letter a real `HardTimeout` and
    /// must not dead-letter a drain that simply ran past its grace window.
    #[tokio::test]
    async fn cancel_with_cleanup_grace_maps_expiry_to_cancel_drain_timeout() {
        // Agent ignores `session/cancel` on stdin and keeps producing noise
        // forever — never drains within the grace window.
        let mut client = spawn_script(vec![Flood("noise".into(), 10, 0)]).await;
        client.last_prompt_id = Some(999);
        let grace = std::time::Duration::from_millis(200);
        let result = client
            .cancel_with_cleanup_grace("test-session", grace)
            .await;
        assert!(
            matches!(result, Err(AcpError::CancelDrainTimeout(g)) if g == grace),
            "expected CancelDrainTimeout({grace:?}), got {result:?}"
        );
    }

    #[tokio::test]
    async fn idle_resets_on_stdout_activity() {
        // Send valid JSON (session/update notifications) to reset the idle timer.
        // Non-JSON lines no longer reset idle — only valid JSON notifications do.
        let mut client = spawn_script(vec![
            Flood(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"text":"thinking"}}}}"#.to_string(), 50, 10),
            SleepMs(10_000),
        ])
        .await;
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let start = std::time::Instant::now();
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_millis(200),
                hard_deadline,
                max_dur,
            )
            .await;
        let elapsed = start.elapsed();
        // 10 messages × 50ms = ~500ms of activity, then idle timeout fires after 200ms more
        assert!(elapsed >= std::time::Duration::from_millis(400));
        assert!(elapsed < std::time::Duration::from_secs(3));
        assert!(matches!(result, Err(AcpError::IdleTimeout(_))));
    }

    #[tokio::test]
    async fn response_returned_when_matching_id_arrives() {
        let mut client = spawn_script(vec![Emit(
            r#"{"jsonrpc":"2.0","id":42,"result":{"stopReason":"end_turn"}}"#.to_string(),
        )])
        .await;
        let max_dur = std::time::Duration::from_secs(5);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                42,
                std::time::Duration::from_secs(2),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["stopReason"].as_str(), Some("end_turn"));
    }

    #[tokio::test]
    async fn agent_exit_detected_as_eof() {
        let mut client = spawn_script(vec![Exit]).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let max_dur = std::time::Duration::from_secs(5);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_secs(2),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(matches!(result, Err(AcpError::AgentExited)));
    }

    /// A message with both `id` and `method` is an agent-initiated request,
    /// not a response. The response matcher must not consume it even if the
    /// id happens to match the expected value.
    #[tokio::test]
    async fn agent_request_with_matching_id_not_consumed_as_response() {
        // The script sends an agent-initiated request (has both id and method)
        // whose id matches what we're waiting for (0), then sends the real
        // response. The request should be dispatched (triggering -32601 since
        // "test/method" is unknown), and the real response should be returned.
        let script = vec![
            Emit(r#"{"jsonrpc":"2.0","id":0,"method":"test/method","params":{}}"#.to_string()),
            ReadLine(2_000),
            Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"ok":true}}"#.to_string()),
            SleepMs(1000),
        ];
        let mut client = spawn_script(script).await;
        let max_dur = std::time::Duration::from_secs(5);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                0,
                std::time::Duration::from_secs(3),
                hard_deadline,
                max_dur,
            )
            .await;
        assert!(result.is_ok(), "expected Ok response, got {result:?}");
        assert_eq!(result.unwrap()["ok"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn idle_fires_before_hard_when_idle_is_shorter() {
        let mut client = spawn_script(vec![SleepMs(10_000)]).await;
        let idle = std::time::Duration::from_millis(100);
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let result = client
            .read_until_response_with_idle_timeout("test", 999, idle, hard_deadline, max_dur)
            .await;
        assert!(
            matches!(result, Err(AcpError::IdleTimeout(_))),
            "idle should fire before hard when idle << hard, got {result:?}"
        );
    }

    /// Hard-deadline starvation regression (Max's review gate, Eva's required test).
    ///
    /// When the read-loop became a `tokio::select!` with `biased; reader →
    /// steer → sleep_until`, a continuously-ready reader arm could win every
    /// poll and starve the timer arm — silently defeating the hard-deadline
    /// guarantee. The fix is a pre-select deadline check at the top of every
    /// loop iteration; this test pins that behavior.
    ///
    /// Setup: agent emits a **gapless** stream of valid JSON `session/update`
    /// notifications (no `sleep` between lines) so the reader arm is
    /// continuously ready. Each line is valid JSON, so it resets the idle
    /// clock — and we set idle ≫ hard so idle cannot fire first. With
    /// `biased; reader → steer → sleep_until`, the reader arm would win
    /// every poll and `sleep_until` would never be reached. Only the
    /// pre-select deadline check at the top of the loop can stop us.
    ///
    /// Without the pre-select check, this test hangs against the infinite
    /// bash subprocess until the test harness's own outer timeout, and the
    /// returned error would never be `HardTimeout`.
    #[tokio::test]
    async fn hard_deadline_fires_under_continuous_valid_json_stream() {
        // Truly infinite, gapless stream of valid JSON. No `sleep` between
        // echoes — the reader arm is continuously ready, which is the
        // exact starvation scenario the pre-select check guards against.
        // `while :; do echo ...; done` (not a fixed-count `for`) so the
        // subprocess never naturally exits before the hard deadline,
        // regardless of how fast the host drains bash output. Without
        // this, fast hardware drains a bounded loop in < hard_deadline
        // and the reader hits EOF (`AgentExited`) before the timer fires,
        // masking whether the pre-select check actually works.
        let mut client = spawn_script(vec![Flood(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"x"}}}}"#.to_string(),
            0,
            0,
        )])
        .await;
        let hard = std::time::Duration::from_millis(300);
        let hard_deadline = tokio::time::Instant::now() + hard;
        let idle = std::time::Duration::from_secs(60); // idle ≫ hard
        let start = std::time::Instant::now();
        let result = client
            .read_until_response_with_idle_timeout("test", 999, idle, hard_deadline, hard)
            .await;
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(AcpError::HardTimeout { .. })),
            "expected HardTimeout under gapless valid-JSON stream, got {result:?} (elapsed {elapsed:?})"
        );
        // Must fire close to the hard deadline, not late. Without the
        // pre-select check the reader arm starves sleep_until and elapsed
        // tracks the bash subprocess lifetime instead.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "HardTimeout fired late ({elapsed:?}); reader arm may be starving sleep_until"
        );
    }

    /// Same as `agent_request_with_matching_id_not_consumed_as_response` but
    /// exercises the non-idle `read_until_response` path (via `send_request`).
    #[tokio::test]
    async fn agent_request_not_consumed_via_send_request() {
        // Script: wait for the initialize request, reply, then send an
        // agent-initiated request with id=1 (matching the next send_request id),
        // wait for the -32601 error reply, then send the real response.
        let script = vec![
            ReadLine(2_000),
            Emit(
                r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#
                    .to_string(),
            ),
            ReadLine(2_000),
            Emit(r#"{"jsonrpc":"2.0","id":1,"method":"test/unknown","params":{}}"#.to_string()),
            ReadLine(2_000),
            Emit(r#"{"jsonrpc":"2.0","id":1,"result":{"worked":true}}"#.to_string()),
            SleepMs(1000),
        ];
        let mut client = spawn_script(script).await;
        // initialize consumes id=0
        let _init = client
            .initialize()
            .await
            .expect("initialize should succeed");
        // send_request uses id=1 — the agent's request with id=1 and method
        // must not be consumed as the response.
        let result = client
            .send_request("test/echo", serde_json::json!({}))
            .await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert_eq!(result.unwrap()["worked"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn keepalive_resets_idle_past_deadline() {
        // Keepalive session/update lines every 50ms against a 100ms idle deadline.
        // The turn should survive well past the 100ms deadline (proves the fix).
        let mut client = spawn_script(vec![
            Flood(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"keepalive"}}}"#.to_string(), 50, 20),
            SleepMs(10_000),
        ])
        .await;
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let start = std::time::Instant::now();
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_millis(100),
                hard_deadline,
                max_dur,
            )
            .await;
        let elapsed = start.elapsed();
        // 20 keepalives × 50ms = ~1000ms of activity, then idle fires after 100ms more.
        // Must survive well past the 100ms deadline.
        assert!(
            elapsed >= std::time::Duration::from_millis(500),
            "keepalive should reset idle past the deadline; elapsed only {elapsed:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(5));
        assert!(matches!(result, Err(AcpError::IdleTimeout(_))));
    }

    #[tokio::test]
    async fn tool_call_resets_idle_then_silence_times_out() {
        // A tool_call session/update resets the idle timer (belt-and-suspenders path),
        // then silence causes idle timeout. This proves the reset works for tool_call
        // specifically — not just via the general valid-JSON reset at line 839.
        //
        // The script emits a tool_call, waits 80ms (under the 200ms idle), then goes
        // silent. If the tool_call reset didn't fire, idle would fire at 200ms from
        // start. With the reset, idle fires at 80ms + 200ms = ~280ms from start.
        let mut client = spawn_script(vec![
            Emit(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","title":"long_running","kind":"shell"}}}"#.to_string()),
            SleepMs(80),
            SleepMs(10_000),
        ])
        .await;
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let start = std::time::Instant::now();
        let result = client
            .read_until_response_with_idle_timeout(
                "test",
                999,
                std::time::Duration::from_millis(200),
                hard_deadline,
                max_dur,
            )
            .await;
        let elapsed = start.elapsed();
        // The tool_call arrives near-instantly and resets idle.
        // Then 80ms of silence, then idle fires at ~280ms from start.
        // Must be > 200ms (proves the reset happened after the tool_call).
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "tool_call should reset idle; elapsed only {elapsed:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(2));
        assert!(
            matches!(result, Err(AcpError::IdleTimeout(_))),
            "expected IdleTimeout after silence, got {result:?}"
        );
    }

    #[tokio::test]
    async fn session_new_full_includes_system_prompt_when_some() {
        // Script: respond to initialize, then echo back the session/new request.
        let script = vec![
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#.to_string()),
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_test","_receivedRequest":{captured}}}"#.to_string()),
        SleepMs(1000),
    ];
        let mut client = spawn_script(script).await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");

        let resp = client
            .session_new_full(
                "/tmp",
                vec![],
                Some(SystemPromptTransport::Field("Custom system prompt")),
                None,
            )
            .await
            .expect("session_new_full should succeed");

        assert_eq!(resp.session_id, "ses_test");
        let received = &resp.raw["_receivedRequest"];
        assert_eq!(
            received["params"]["systemPrompt"].as_str(),
            Some("Custom system prompt"),
            "systemPrompt should be included in params when Some"
        );
    }

    #[tokio::test]
    async fn goose_system_prompt_request_uses_set_contract() {
        let script = vec![
            ReadLine(2_000),
            Emit(
                r#"{"jsonrpc":"2.0","id":0,"result":{"_receivedRequest":{captured}}}"#.to_string(),
            ),
            SleepMs(1000),
        ];
        let mut client = spawn_script(script).await;
        let result = client
            .session_set_goose_system_prompt("ses_goose", "Be terse")
            .await
            .expect("custom request succeeds");
        let received = &result["_receivedRequest"];
        assert_eq!(
            received["method"],
            "_goose/unstable/session/system-prompt/set"
        );
        assert_eq!(received["params"]["sessionId"], "ses_goose");
        assert_eq!(received["params"]["mode"], "set");
        assert_eq!(received["params"]["key"], "buzz");
        assert_eq!(received["params"]["text"], "Be terse");
    }

    #[tokio::test]
    async fn goose_system_prompt_preserves_method_not_found_for_fallback() {
        let script = vec![
            ReadLine(2_000),
            Emit(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}"#
                    .to_string(),
            ),
            SleepMs(1000),
        ];
        let mut client = spawn_script(script).await;
        assert!(matches!(
            client
                .session_set_goose_system_prompt("ses_goose", "Be terse")
                .await,
            Err(AcpError::AgentError { code: -32601, .. })
        ));
    }

    #[tokio::test]
    async fn goose_system_prompt_preserves_invalid_params_as_error() {
        let script = vec![
            ReadLine(2_000),
            Emit(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"Invalid params"}}"#
                    .to_string(),
            ),
            SleepMs(1000),
        ];
        let mut client = spawn_script(script).await;
        assert!(matches!(
            client
                .session_set_goose_system_prompt("ses_goose", "Be terse")
                .await,
            Err(AcpError::AgentError { code: -32602, .. })
        ));
    }

    #[tokio::test]
    async fn session_new_full_omits_system_prompt_when_none() {
        // When system_prompt is None, the field should not appear in params.
        let script = vec![
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#.to_string()),
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_test","_receivedRequest":{captured}}}"#.to_string()),
        SleepMs(1000),
    ];
        let mut client = spawn_script(script).await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");

        let resp = client
            .session_new_full("/tmp", vec![], None, None)
            .await
            .expect("session_new_full should succeed");

        assert_eq!(resp.session_id, "ses_test");
        let received = &resp.raw["_receivedRequest"];
        assert!(
            received["params"]["systemPrompt"].is_null(),
            "systemPrompt should NOT be in params when value is None"
        );
    }

    #[tokio::test]
    async fn session_new_full_sends_session_title_in_meta_when_some() {
        let script = vec![
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#.to_string()),
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_test","_receivedRequest":{captured}}}"#.to_string()),
        SleepMs(1000),
    ];
        let mut client = spawn_script(script).await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");

        let resp = client
            .session_new_full("/tmp", vec![], None, Some("Fizz · #buzz-dev"))
            .await
            .expect("session_new_full should succeed");

        let received = &resp.raw["_receivedRequest"];
        assert_eq!(
            received["params"]["_meta"]["sessionTitle"].as_str(),
            Some("Fizz · #buzz-dev"),
            "title should ride in _meta.sessionTitle, out of band from the prompt"
        );
    }

    #[tokio::test]
    async fn session_new_full_omits_meta_when_session_title_none() {
        let script = vec![
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#.to_string()),
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_test","_receivedRequest":{captured}}}"#.to_string()),
        SleepMs(1000),
    ];
        let mut client = spawn_script(script).await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");

        let resp = client
            .session_new_full("/tmp", vec![], None, None)
            .await
            .expect("session_new_full should succeed");

        let received = &resp.raw["_receivedRequest"];
        assert!(
            received["params"].get("_meta").is_none(),
            "_meta should be absent entirely, not an empty object or null"
        );
    }

    // ── claude-agent-acp _meta.systemPrompt transport ─────────────────────

    #[tokio::test]
    async fn session_new_full_sends_claude_meta_system_prompt_when_claude_meta_transport() {
        // When ClaudeMeta transport is requested, the prompt must appear as
        // _meta.systemPrompt: {"append": text} — never as a bare systemPrompt field.
        let script = vec![
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#.to_string()),
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_claude","_receivedRequest":{captured}}}"#.to_string()),
        SleepMs(1000),
    ];
        let mut client = spawn_script(script).await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");

        let resp = client
            .session_new_full(
                "/tmp",
                vec![],
                Some(SystemPromptTransport::ClaudeMeta("Be concise")),
                None,
            )
            .await
            .expect("session_new_full should succeed");

        let received = &resp.raw["_receivedRequest"];
        assert!(
            received["params"].get("systemPrompt").is_none(),
            "bare systemPrompt must not be present for ClaudeMeta transport"
        );
        assert_eq!(
            received["params"]["_meta"]["systemPrompt"]["append"].as_str(),
            Some("Be concise"),
            "_meta.systemPrompt.append must carry the prompt text"
        );
    }

    #[tokio::test]
    async fn session_new_full_merges_claude_meta_and_session_title_into_single_meta_object() {
        // Both ClaudeMeta prompt and session_title must coexist under _meta —
        // the prompt must not clobber sessionTitle or vice versa.
        let script = vec![
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#.to_string()),
        ReadLine(2_000),
        Emit(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_merged","_receivedRequest":{captured}}}"#.to_string()),
        SleepMs(1000),
    ];
        let mut client = spawn_script(script).await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");

        let resp = client
            .session_new_full(
                "/tmp",
                vec![],
                Some(SystemPromptTransport::ClaudeMeta("Be concise")),
                Some("Fizz · #buzz-dev"),
            )
            .await
            .expect("session_new_full should succeed");

        let received = &resp.raw["_receivedRequest"];
        assert_eq!(
            received["params"]["_meta"]["systemPrompt"]["append"].as_str(),
            Some("Be concise"),
            "_meta.systemPrompt.append must be present"
        );
        assert_eq!(
            received["params"]["_meta"]["sessionTitle"].as_str(),
            Some("Fizz · #buzz-dev"),
            "_meta.sessionTitle must be present alongside systemPrompt"
        );
    }

    // ── Goose-native steer scaffold (PR follow-up to #1160) ──────────────

    /// Helper: spawn an inert echo peer（原 bash `cat`——进程内 duplex 版）
    /// so we have a real AcpClient to drive `handle_session_update` against.
    /// It never writes anything *new* — these tests don't read from the
    /// agent, they just feed JSON into the parser.
    async fn spawn_inert_client() -> AcpClient {
        spawn_script(vec![EchoAll]).await
    }

    /// Build a `session/update` JSON-RPC notification carrying a
    /// `session_info_update` with the given `_meta.goose.activeRunId` value.
    /// Pass `None` to omit the `activeRunId` field entirely.
    ///
    /// `_meta` is nested inside the `update` object (per the ACP
    /// `SessionInfoUpdate` schema), matching what goose and buzz-agent
    /// emit on the wire.
    fn session_info_update_msg(active_run_id: Option<serde_json::Value>) -> serde_json::Value {
        let mut goose = serde_json::Map::new();
        if let Some(v) = active_run_id {
            goose.insert("activeRunId".to_string(), v);
        }
        let mut meta = serde_json::Map::new();
        meta.insert("goose".to_string(), serde_json::Value::Object(goose));
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "test-session",
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": serde_json::Value::Object(meta),
                },
            }
        })
    }

    #[tokio::test]
    async fn active_run_id_sets_on_string() {
        let mut client = spawn_inert_client().await;
        assert!(client.active_run_id().is_none(), "starts as None");

        let msg = session_info_update_msg(Some(serde_json::json!("run-abc-123")));
        let _ = client.handle_session_update(&msg);

        assert_eq!(client.active_run_id(), Some("run-abc-123"));
    }

    #[tokio::test]
    async fn active_run_id_clears_on_null() {
        let mut client = spawn_inert_client().await;
        // Set it first
        let set_msg = session_info_update_msg(Some(serde_json::json!("run-xyz")));
        let _ = client.handle_session_update(&set_msg);
        assert_eq!(client.active_run_id(), Some("run-xyz"));

        // Then clear with explicit null
        let clear_msg = session_info_update_msg(Some(serde_json::Value::Null));
        let _ = client.handle_session_update(&clear_msg);
        assert!(
            client.active_run_id().is_none(),
            "explicit null must clear active_run_id"
        );
    }

    #[tokio::test]
    async fn active_run_id_untouched_when_missing() {
        // Field absent entirely — must NOT clear existing state (only an
        // explicit null clears; missing means "no new info this update").
        let mut client = spawn_inert_client().await;
        let set_msg = session_info_update_msg(Some(serde_json::json!("run-stable")));
        let _ = client.handle_session_update(&set_msg);
        assert_eq!(client.active_run_id(), Some("run-stable"));

        // session_info_update with no activeRunId field — leave state alone.
        let missing_msg = session_info_update_msg(None);
        let _ = client.handle_session_update(&missing_msg);
        assert_eq!(
            client.active_run_id(),
            Some("run-stable"),
            "missing activeRunId must leave state untouched"
        );
    }

    #[tokio::test]
    async fn active_run_id_untouched_on_wrong_type() {
        // A number or object in activeRunId is malformed — neither set nor clear.
        let mut client = spawn_inert_client().await;
        let set_msg = session_info_update_msg(Some(serde_json::json!("run-stable")));
        let _ = client.handle_session_update(&set_msg);
        assert_eq!(client.active_run_id(), Some("run-stable"));

        let wrong_type_msg = session_info_update_msg(Some(serde_json::json!(42)));
        let _ = client.handle_session_update(&wrong_type_msg);
        assert_eq!(
            client.active_run_id(),
            Some("run-stable"),
            "non-string/non-null activeRunId must leave state untouched"
        );
    }

    // ── Goose-native steer arm tests ──────────────────────────────────────
    //
    // These exercise the seam between `install_steer_rx` and the read
    // loop's steer arm, isolated from `AgentPool` / `EventQueue` /
    // dispatch. They prove the locked Option-X contract at the read-loop
    // boundary:
    //   1. With `active_run_id == None`, the steer arm acks
    //      `Err(ExpectedRunIdMissing)` and writes nothing — the main
    //      loop's "Err-before-pending" fallback path is reachable.
    //   2. With `active_run_id` set, the steer arm writes the JSON-RPC
    //      request with the matching `expectedRunId` and routes the
    //      response to the ack oneshot as `Success`.
    //
    // We don't test the full mode-gate fork here — that lives in lib.rs
    // and is covered by goose e2e (Eva's lane).

    /// Steer with no `active_run_id` set acks `ExpectedRunIdMissing`
    /// without writing anything. The read loop continues normally and
    /// eventually hits the idle timeout (which is fine — we just need to
    /// observe the ack).
    #[tokio::test]
    async fn native_steer_with_no_active_run_id_acks_expected_run_id_missing() {
        // Quiet process: never emits anything, so the read loop has only
        // the steer arm and the idle timeout to consider.
        let mut client = spawn_script(vec![SleepMs(10_000)]).await;
        assert!(
            client.active_run_id().is_none(),
            "precondition: active_run_id starts as None"
        );

        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::buzz::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);

        // Fire-and-forget: send a SteerRequest from a separate task so
        // the read loop picks it up via the select! arm.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::buzz::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::buzz::pool::SteerRequest {
                    prompt_blocks: vec!["test steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        // Drive the read loop with short idle timeout so the test
        // doesn't hang. The expected_id is intentionally never going to
        // be matched (the script writes nothing); the read loop will
        // exit via IdleTimeout shortly after the steer arm fires.
        let idle = std::time::Duration::from_millis(500);
        let max_dur = std::time::Duration::from_secs(5);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let read_result = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        // Read loop exit shape: IdleTimeout (no agent activity).
        assert!(
            matches!(read_result, Err(AcpError::IdleTimeout(_))),
            "expected IdleTimeout once steer was acked + script stayed silent, got {read_result:?}"
        );

        // Ack must be ExpectedRunIdMissing — the steer arm bailed out
        // without writing because active_run_id was None at write time.
        let ack = ack_rx
            .await
            .expect("ack oneshot must have received a SteerAck");
        match ack {
            crate::buzz::pool::SteerAck::Err(
                crate::buzz::pool::SteerError::ExpectedRunIdMissing,
            ) => {}
            other => panic!("expected SteerAck::Err(ExpectedRunIdMissing), got {other:?}"),
        }
    }

    /// Steer with `active_run_id` set writes the JSON-RPC request and
    /// routes the matching response to the ack oneshot as `Success`.
    /// Verifies the wire shape (`sessionId` + `expectedRunId` + `prompt`)
    /// indirectly: the bash script emits a response keyed by the steer
    /// id (0), and `Success` only fires if the read loop matched that
    /// id to its `pending_steer` entry.
    #[tokio::test]
    async fn native_steer_with_active_run_id_routes_response_to_ack() {
        // Script: pause briefly so the test task can install the steer
        // and we can be sure the response doesn't race ahead of the
        // write — then emit the steer response (id=0 because next_id
        // starts at 0 and the steer is the first request the read loop
        // writes), then idle. This is a JSON-RPC success response with
        // a `stopReason` payload (matching the shape goose uses for
        // steer responses in fake_llm.rs).
        let script = vec![
            SleepMs(500),
            Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"stopReason":"end_turn"}}"#.to_string()),
            SleepMs(10_000),
        ];
        let mut client = spawn_script(script).await;

        // Set active_run_id via a synthesized session_info_update so the
        // steer arm has a non-None value to read at write time.
        let update = session_info_update_msg(Some(serde_json::json!("run-42")));
        let _ = client.handle_session_update(&update);
        assert_eq!(client.active_run_id(), Some("run-42"));

        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::buzz::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::buzz::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::buzz::pool::SteerRequest {
                    prompt_blocks: vec!["test steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        // Drive the read loop. Expected_id 999 will never be emitted by
        // the script so the read loop exits via idle timeout after the
        // steer response is routed to ack.
        let idle = std::time::Duration::from_secs(2);
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let read_result = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        // Read loop exit: IdleTimeout (no further activity after the
        // routed steer response). AgentExited would also be a valid
        // exit if the bash script terminated early; either is fine —
        // what matters is the ack.
        assert!(
            matches!(
                read_result,
                Err(AcpError::IdleTimeout(_)) | Err(AcpError::AgentExited)
            ),
            "expected IdleTimeout or AgentExited after steer ack, got {read_result:?}"
        );

        // Ack must be Success: the steer response (id=0) was routed to
        // pending_steer.ack_tx.
        let ack = ack_rx
            .await
            .expect("ack oneshot must have received a SteerAck");
        match ack {
            crate::buzz::pool::SteerAck::Success { .. } => {}
            other => panic!("expected SteerAck::Success, got {other:?}"),
        }
    }

    /// Steer-success renewal keeps the turn alive past the original hard
    /// deadline. This is the red-on-old/green-on-new test for the core bug
    /// fix (acp.rs:1440-1444): without renewal, the read loop returns
    /// `HardTimeout` before the prompt response arrives.
    ///
    /// Timeline:
    ///   t≈0:    read loop starts, `hard_deadline = now + 1s`
    ///   t≈0.5s: script emits steer response (id=0) → Success renewal
    ///           moves `hard_deadline` to `now + 3s` (≈3.5s from start)
    ///   t≈1.5s: script emits prompt response (id=999) → `Ok`
    ///
    /// Old code: `HardTimeout` at t≈1s (before prompt response).
    /// New code: deadline renewed at t≈0.5s → prompt response at t≈1.5s → `Ok`.
    #[tokio::test]
    async fn steer_success_renews_hard_deadline_and_survives_past_original() {
        let script = vec![
            SleepMs(500),
            Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"stopReason":"end_turn"}}"#.to_string()),
            SleepMs(1_000),
            Emit(r#"{"jsonrpc":"2.0","id":999,"result":{"done":true}}"#.to_string()),
        ];
        let mut client = spawn_script(script).await;

        let update = session_info_update_msg(Some(serde_json::json!("run-99")));
        let _ = client.handle_session_update(&update);

        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::buzz::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::buzz::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::buzz::pool::SteerRequest {
                    prompt_blocks: vec!["steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        let idle = std::time::Duration::from_secs(10);
        let max_dur = std::time::Duration::from_secs(3);
        let hard_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let result = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        assert!(
            result.is_ok(),
            "expected Ok (prompt response after renewed deadline), got {result:?}"
        );
        assert_eq!(result.unwrap()["done"], serde_json::json!(true));

        let ack = ack_rx
            .await
            .expect("ack oneshot must have received a SteerAck");
        match ack {
            crate::buzz::pool::SteerAck::Success { .. } => {}
            other => panic!("expected SteerAck::Success, got {other:?}"),
        }
    }

    // ── Cross-harness steer transport tests ───────────────────────────────
    //
    // These cover the `_session/steering` transport added alongside the
    // goose-native method: capability capture at `initialize`, write-time
    // transport selection, and outcome decoding. Wire-shape assertions read
    // the actual serialized request bytes via `capture_steer_request` rather
    // than inferring the shape from response-id routing.

    /// Spawn a client whose script captures the first line written to its
    /// stdin into `capture_path`, then emits `response` (already-serialized
    /// JSON-RPC) and idles.
    ///
    /// The steer request is the first thing this read loop writes, so the
    /// captured line IS the steer request bytes.
    /// steer 请求捕获：读客户端一行 → 原样落盘 → 回固定响应 → 静默存活。
    async fn spawn_steer_capture_script(
        capture_path: &std::path::Path,
        response: &str,
    ) -> AcpClient {
        spawn_script(vec![
            ReadLine(10_000),
            CaptureTo(capture_path.display().to_string()),
            Emit(response.to_string()),
            SleepMs(10_000),
        ])
        .await
    }

    /// Drive one steer through the read loop and return
    /// `(captured_request_bytes, ack)`.
    ///
    /// `capture_path` may be absent afterwards when the arm wrote nothing —
    /// callers assert on that. The read loop is expected to exit via a
    /// timeout or EOF; the ack is what these tests care about.
    async fn run_one_steer(
        client: &mut AcpClient,
        capture_path: &std::path::Path,
    ) -> (Option<String>, crate::buzz::pool::SteerAck) {
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::buzz::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::buzz::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::buzz::pool::SteerRequest {
                    prompt_blocks: vec!["steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        let idle = std::time::Duration::from_millis(800);
        let max_dur = std::time::Duration::from_secs(10);
        let hard_deadline = tokio::time::Instant::now() + max_dur;
        let _ = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        let ack = ack_rx
            .await
            .expect("ack oneshot must have received a SteerAck");
        (std::fs::read_to_string(capture_path).ok(), ack)
    }

    /// Unique temp path for one test's captured request bytes.
    fn capture_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("buzz-acp-steer-capture");
        std::fs::create_dir_all(&dir).expect("create capture dir");
        let path = dir.join(format!("{name}.json"));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Mark a client as having advertised `_meta.steering.supported` without
    /// running a real `initialize` handshake. The capability-parsing tests
    /// cover the handshake itself.
    fn set_steering_supported(client: &mut AcpClient) {
        client.steering_supported = true;
    }

    /// Run `initialize` against a mock that replies with `init_result` as
    /// the JSON-RPC result, and return the resulting `steering_supported`.
    async fn steering_supported_after_initialize(init_result: &str) -> bool {
        let mut client = spawn_script(vec![
            ReadLine(10_000),
            Emit(format!(
                r#"{{"jsonrpc":"2.0","id":0,"result":{init_result}}}"#
            )),
            SleepMs(5_000),
        ])
        .await;
        client
            .initialize()
            .await
            .expect("initialize should succeed");
        client.steering_supported()
    }

    /// Test 1a: an adapter advertising `_meta.steering.supported: true`
    /// (claude-agent-acp `src/acp-agent.ts:1444`, codex-acp
    /// `src/CodexAcpServer.ts:247`) is recorded as steering-capable.
    #[tokio::test]
    async fn initialize_records_steering_supported_when_advertised() {
        let supported = steering_supported_after_initialize(
            r#"{"protocolVersion":2,"agentCapabilities":{},"_meta":{"steering":{"supported":true}}}"#,
        )
        .await;
        assert!(
            supported,
            "_meta.steering.supported: true must set steering_supported"
        );
    }

    /// Test 1b: no `_meta` at all (goose, buzz-agent, any older adapter) must
    /// leave the capability off — this is what keeps a steer off the wire for
    /// agents that never implemented it.
    #[tokio::test]
    async fn initialize_leaves_steering_unsupported_when_meta_absent() {
        let supported =
            steering_supported_after_initialize(r#"{"protocolVersion":2,"agentCapabilities":{}}"#)
                .await;
        assert!(
            !supported,
            "absent _meta must leave steering_supported false"
        );
    }

    /// Test 1c: an explicit `supported: false` is respected, not treated as
    /// "the key exists so it must work".
    #[tokio::test]
    async fn initialize_leaves_steering_unsupported_when_explicitly_false() {
        let supported = steering_supported_after_initialize(
            r#"{"protocolVersion":2,"_meta":{"steering":{"supported":false}}}"#,
        )
        .await;
        assert!(
            !supported,
            "_meta.steering.supported: false must leave steering_supported false"
        );
    }

    /// Test 2: no `active_run_id` + capability advertised → the bytes on the
    /// wire are an `_session/steering` request carrying `sessionId` and
    /// `prompt`, and carrying **no** `expectedRunId` (the adapters reject
    /// unknown required fields, and there is no run id to report anyway).
    #[tokio::test]
    async fn acp_steer_request_omits_expected_run_id_and_carries_session_and_prompt() {
        let capture = capture_path("acp_shape");
        let mut client = spawn_steer_capture_script(
            &capture,
            r#"{"jsonrpc":"2.0","id":0,"result":{"outcome":"injected"}}"#,
        )
        .await;
        set_steering_supported(&mut client);
        assert!(
            client.active_run_id().is_none(),
            "precondition: no active_run_id"
        );

        let (written, ack) = run_one_steer(&mut client, &capture).await;

        let written = written.expect("steer request must have been written");
        let msg: serde_json::Value =
            serde_json::from_str(&written).expect("written line must be valid JSON");
        assert_eq!(
            msg["method"].as_str(),
            Some(ACP_STEER_METHOD),
            "must use the cross-adapter steer method; wrote: {written}"
        );
        assert_eq!(msg["params"]["sessionId"].as_str(), Some("sess-test"));
        assert_eq!(
            msg["params"]["prompt"][0]["text"].as_str(),
            Some("steer body"),
            "prompt must carry the steer body as a text block"
        );
        assert!(
            msg["params"].get("expectedRunId").is_none(),
            "_session/steering must not carry expectedRunId; wrote: {written}"
        );
        assert!(
            matches!(ack, crate::buzz::pool::SteerAck::Success { .. }),
            "injected outcome must ack Success, got {ack:?}"
        );
    }

    /// Test 3: goose keeps priority. With both an `active_run_id` and the
    /// advertised capability, the goose method wins — `expectedRunId` is
    /// strictly more precise about which run is being steered.
    #[tokio::test]
    async fn goose_transport_wins_when_both_run_id_and_capability_present() {
        let capture = capture_path("goose_priority");
        let mut client =
            spawn_steer_capture_script(&capture, r#"{"jsonrpc":"2.0","id":0,"result":{}}"#).await;
        set_steering_supported(&mut client);
        let update = session_info_update_msg(Some(serde_json::json!("run-77")));
        let _ = client.handle_session_update(&update);

        let (written, ack) = run_one_steer(&mut client, &capture).await;

        let written = written.expect("steer request must have been written");
        let msg: serde_json::Value =
            serde_json::from_str(&written).expect("written line must be valid JSON");
        assert_eq!(
            msg["method"].as_str(),
            Some(GOOSE_STEER_METHOD),
            "goose method must win when a run id exists; wrote: {written}"
        );
        assert_eq!(msg["params"]["expectedRunId"].as_str(), Some("run-77"));
        // A bare `{}` result is a success on the goose transport (goose sends
        // no `outcome`) — the OutcomeRejected guard applies only to
        // `_session/steering`.
        assert!(
            matches!(ack, crate::buzz::pool::SteerAck::Success { .. }),
            "goose success result must ack Success, got {ack:?}"
        );
    }

    /// Test 7: codex-acp's third outcome, `failed`
    /// (`src/AcpExtensions.ts:92`), is a delivery rejection despite being a
    /// JSON-RPC success — release the event and fall back.
    #[tokio::test]
    async fn acp_steer_failed_outcome_acks_outcome_rejected() {
        let capture = capture_path("outcome_failed");
        let mut client = spawn_steer_capture_script(
            &capture,
            r#"{"jsonrpc":"2.0","id":0,"result":{"outcome":"failed"}}"#,
        )
        .await;
        set_steering_supported(&mut client);

        let (_written, ack) = run_one_steer(&mut client, &capture).await;

        match ack {
            crate::buzz::pool::SteerAck::Err(crate::buzz::pool::SteerError::OutcomeRejected {
                outcome,
            }) => {
                assert_eq!(
                    outcome, "failed",
                    "rejected outcome must report what the agent said, unquoted"
                );
            }
            other => panic!("expected Err(OutcomeRejected), got {other:?}"),
        }
    }

    /// Test 8: **codex `extMethod` silent-loss regression guard.** codex-acp's
    /// ext dispatcher answers unrecognized methods with a bare `{}` — a
    /// JSON-RPC *success*, not `-32601` (`src/CodexAcpServer.ts:255-258`).
    /// Buzz maps `SteerAck::Success` to `queue.remove_event`, so decoding
    /// `{}` as success would delete the user's message with no error, no
    /// fallback, and no log. An absent `outcome` must therefore be a
    /// rejection, which releases the event and fires cancel+merge.
    #[tokio::test]
    async fn acp_steer_missing_outcome_acks_outcome_rejected_and_never_drops_event() {
        let capture = capture_path("outcome_absent");
        let mut client =
            spawn_steer_capture_script(&capture, r#"{"jsonrpc":"2.0","id":0,"result":{}}"#).await;
        set_steering_supported(&mut client);

        let (_written, ack) = run_one_steer(&mut client, &capture).await;

        match ack {
            crate::buzz::pool::SteerAck::Err(crate::buzz::pool::SteerError::OutcomeRejected {
                outcome,
            }) => {
                assert_eq!(
                    outcome, "<absent>",
                    "a result with no outcome field must be reported as absent"
                );
            }
            other => panic!(
                "expected Err(OutcomeRejected) for a bare {{}} success — \
                 anything else risks dropping the event, got {other:?}"
            ),
        }
    }

    /// Test 5: `injected` renews the hard deadline, so the turn survives past
    /// its original one. Mirrors
    /// `steer_success_renews_hard_deadline_and_survives_past_original` for
    /// the `_session/steering` transport.
    ///
    /// Timeline: original hard deadline at t≈1s; steer response at t≈0.5s
    /// renews it to t≈3.5s; prompt response at t≈1.5s lands inside it.
    #[tokio::test]
    async fn acp_steer_injected_renews_hard_deadline_and_survives_past_original() {
        let script = vec![
            SleepMs(500),
            Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"outcome":"injected"}}"#.to_string()),
            SleepMs(1_000),
            Emit(r#"{"jsonrpc":"2.0","id":999,"result":{"done":true}}"#.to_string()),
        ];
        let mut client = spawn_script(script).await;
        set_steering_supported(&mut client);

        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::buzz::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::buzz::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::buzz::pool::SteerRequest {
                    prompt_blocks: vec!["steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        let idle = std::time::Duration::from_secs(10);
        let max_dur = std::time::Duration::from_secs(3);
        let hard_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let result = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        assert!(
            result.is_ok(),
            "injected must renew the deadline so the prompt response still lands, got {result:?}"
        );
        assert_eq!(result.unwrap()["done"], serde_json::json!(true));
        let ack = ack_rx.await.expect("ack must be received");
        assert!(
            matches!(ack, crate::buzz::pool::SteerAck::Success { .. }),
            "injected must ack Success, got {ack:?}"
        );
    }

    /// Test 6: **red/green for the no-renewal rule.** `startedNewTurn` means
    /// the turn Buzz was steering had already ended and the adapter began a
    /// fresh, detached one. It acks `Success` (the message WAS delivered, so
    /// the event must not be redelivered) but must NOT renew the hard
    /// deadline — that clock belongs to a turn which is already settled.
    ///
    /// Same timeline as the `injected` test, so the only difference is the
    /// outcome string: original hard deadline at t≈1s, steer response at
    /// t≈0.5s, prompt response at t≈1.5s. With renewal the prompt response
    /// would land and this returns `Ok`; without renewal the original
    /// deadline fires first and we get `HardTimeout`.
    #[tokio::test]
    async fn acp_steer_started_new_turn_acks_success_without_renewing_hard_deadline() {
        let script = vec![
            SleepMs(500),
            Emit(r#"{"jsonrpc":"2.0","id":0,"result":{"outcome":"startedNewTurn"}}"#.to_string()),
            SleepMs(1_000),
            Emit(r#"{"jsonrpc":"2.0","id":999,"result":{"done":true}}"#.to_string()),
        ];
        let mut client = spawn_script(script).await;
        set_steering_supported(&mut client);

        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<crate::buzz::pool::SteerRequest>(1);
        client.install_steer_rx(steer_rx);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<crate::buzz::pool::SteerAck>();
        let send_task = tokio::spawn(async move {
            steer_tx
                .send(crate::buzz::pool::SteerRequest {
                    prompt_blocks: vec!["steer body".into()],
                    ack_tx,
                })
                .await
                .expect("steer_tx send should succeed");
        });

        let idle = std::time::Duration::from_secs(10);
        let max_dur = std::time::Duration::from_secs(3);
        let hard_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let result = client
            .read_until_response_with_idle_timeout("sess-test", 999, idle, hard_deadline, max_dur)
            .await;
        send_task.await.expect("send_task should complete");

        // The original deadline must still fire — renewal here would extend
        // the clock on a turn the adapter has already finished.
        assert!(
            matches!(result, Err(AcpError::HardTimeout { .. })),
            "startedNewTurn must NOT renew the hard deadline, so the original \
             one must still fire; got {result:?}"
        );
        // Delivery still succeeded, so the withheld event must be dropped
        // rather than released — hence Success, not an Err.
        let ack = ack_rx.await.expect("ack must be received");
        assert!(
            matches!(ack, crate::buzz::pool::SteerAck::Success { .. }),
            "startedNewTurn is a delivery success, got {ack:?}"
        );
    }

    /// Test 4 (companion to the existing
    /// `native_steer_with_no_active_run_id_acks_expected_run_id_missing`):
    /// no run id AND no advertised capability means nothing is written at
    /// all. This is the gate that keeps a steer off the wire for adapters
    /// that never implemented either method.
    #[tokio::test]
    async fn steer_writes_nothing_when_no_run_id_and_capability_absent() {
        let capture = capture_path("no_transport");
        let mut client =
            spawn_steer_capture_script(&capture, r#"{"jsonrpc":"2.0","id":0,"result":{}}"#).await;
        assert!(!client.steering_supported(), "precondition: not advertised");
        assert!(
            client.active_run_id().is_none(),
            "precondition: no active_run_id"
        );

        let (written, ack) = run_one_steer(&mut client, &capture).await;

        assert!(
            written.is_none(),
            "no transport available must write nothing; wrote: {written:?}"
        );
        match ack {
            crate::buzz::pool::SteerAck::Err(
                crate::buzz::pool::SteerError::ExpectedRunIdMissing,
            ) => {}
            other => panic!("expected Err(ExpectedRunIdMissing), got {other:?}"),
        }
    }

    #[test]
    fn agent_error_from_json_falls_back_to_full_json_when_message_missing() {
        // Errors without a string `message` field (e.g. only a `data` field) must
        // not be silently truncated to "unknown error" — the full JSON is preserved.
        let error = serde_json::json!({"code": -32000, "data": "quota exceeded"});
        match super::agent_error_from_json(&error) {
            AcpError::AgentError { code, message } => {
                assert_eq!(code, -32000);
                assert!(
                    message.contains("quota exceeded"),
                    "expected full JSON in message, got: {message}"
                );
            }
            other => panic!("expected AgentError, got {other:?}"),
        }
    }

    #[test]
    fn agent_error_from_json_uses_message_field_when_present() {
        let error = serde_json::json!({"code": -32001, "message": "auth denied"});
        match super::agent_error_from_json(&error) {
            AcpError::AgentError { code, message } => {
                assert_eq!(code, -32001);
                assert_eq!(message, "auth denied");
            }
            other => panic!("expected AgentError, got {other:?}"),
        }
    }
}
