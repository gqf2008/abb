//! buzz harness —— 进程内 ACP agent 管理与投递主循环（ABB 单进程形态）。
//!
//! 上游 buzz-acp（`crates/buzz-acp` @ c3132c3）的 relay 形态整个被替换为进程内
//! 消息总线：ABB 桥把聊天消息 `push` 进来，harness 排队 → 投给唯一一个 pi-acp
//! 子进程 → 回合结束时把捕获的 agent 文本同步投回聊天（docs/buzz-port-sync.md）。
//!
//! 裁剪对照（相对上游 lib.rs 主循环）：
//! - **懒池**：单 slot，首条消息才拉起 agent 子进程；崩溃后指数退避自动重拉
//!   （2s → 封顶 60s，成功即清零）。上游的熔断开闸/全池退出不适用——ABB 是宿主
//!   进程，buzz 后端不可用只影响 buzz 频道，不该拖垮整进程。
//! - **无心跳/无 relay/无 observer**：没有周期性唤醒与 REST 上下文，通知全部
//!   走 [`TurnOutput`] 出站（agent 回复与 harness 失败告示同管道，桥侧一视同仁
//!   写历史并发送）。
//! - **DedupMode::Queue 固定**：channel 在跑时到达的新消息必须入队，steer
//!   （cancel+merge 重提示）才能把它们并入下一轮；Drop 会静默丢用户消息。
//!   队列自带每频道 500 条上限与 MAX_RETRIES=10 死信（queue.rs）。
//! - **主循环骨架**照搬上游 select（prompt 结果 / 任务 panic / steer ack /
//!   关闭），dispatch_pending 与 handle_prompt_result 的批次命运顺序
//!   （requeue 先于 mark_complete、cancel/steer 合并、死信告示）逐条对齐
//!   （/tmp/up_lib_{dispatch,loop,respawn}.txt 为移植参考摘录）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::buzz::acp::{AcpClient, AcpError};
use crate::buzz::pool::{
    AgentPool, ControlSignal, OwnedAgent, PromptContext, PromptOutcome, PromptResult, PromptSource,
    SteerAck, SteerError, SteerRequest, TimeoutKind,
};
use crate::buzz::queue::{
    BatchEvent, CancelReason, DedupMode, EventQueue, FlushBatch, InboundMsg, PromptChannelInfo,
    QueuedEvent,
};

/// 回合空闲超时：agent 静默（无 ACP 线活动）这么久判死。
const IDLE_TIMEOUT: Duration = Duration::from_secs(900);
/// 单回合硬上限。
const MAX_TURN_DURATION: Duration = Duration::from_secs(3600);
/// 崩溃重拉退避：2^level 秒封顶 [`RESPAWN_BACKOFF_MAX_SECS`]。
const RESPAWN_BACKOFF_BASE_SECS: u64 = 2;
const RESPAWN_BACKOFF_MAX_SECS: u64 = 60;
/// 关闭时等在跑回合的宽限（上游同值）。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// 频道登记条目：channel uuid → 路由元数据。
///
/// 由 service（根频道同步）与桥（话题频道，dispatch 前 upsert）写入；harness
/// 在回合结束时解析出 [`TurnOutput`] 的路由与锚点。root 频道 `thread_id` 为
/// `None`；话题频道锚点是该话题最近一条已投递用户消息的 mid（线程回复
/// send_thread_reply 需要）。
#[derive(Debug, Clone)]
pub struct ChannelMeta {
    pub bot_key: String,
    pub chat_id: String,
    pub thread_id: Option<String>,
    /// 群展示名（角色名）：进 prompt 上下文与 session 标题。
    pub name: String,
    /// 话题回复锚点 mid（话题频道每次 dispatch 时由桥刷新）。
    pub anchor_mid: Option<String>,
}

impl ChannelMeta {
    fn channel_info(&self) -> PromptChannelInfo {
        PromptChannelInfo {
            name: self.name.clone(),
            channel_type: "channel".to_string(),
            description: None,
        }
    }
}

/// agent 子进程启动参数（service 侧解析 pi-acp 路径后传入）。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub command: String,
    pub args: Vec<String>,
    /// 无条件覆盖的环境变量（ABB 合成 PATH；与上游「缺失才注入」语义不同，
    /// 见 docs/buzz-port-sync.md）。
    pub extra_env: Vec<(String, String)>,
}

/// 出站回合/告示：桥侧消费（写历史 + 发送）。
#[derive(Debug)]
pub struct TurnOutput {
    pub channel_id: Uuid,
    /// 回合结束时的频道登记快照。`None` = 回合途中频道被移除——投递方应丢弃。
    pub meta: Option<ChannelMeta>,
    /// agent 回复文本（或 harness 失败告示文案）。
    pub text: String,
}

enum Cmd {
    Message {
        channel_id: Uuid,
        msg: InboundMsg,
    },
    Cancel {
        channel_id: Uuid,
        reply: oneshot::Sender<bool>,
    },
    /// 根频道全量同步（service 每 2s 从 vb 存储扫描）。diff 应用：新增
    /// 注册，消失的根频道排空队列、失效会话、其后的失败批次直接丢弃。
    SyncRoots(Vec<ChannelMeta>),
}

/// 懒启动 / 崩溃重拉的后台尝试结果。
enum SpawnOutcome {
    Ok(Box<OwnedAgent>),
    Err(String),
}

/// 频道登记表（桥线程与主循环共用；ctx.channels 提示元数据同锁更新）。
#[derive(Default)]
struct Registry {
    channels: HashMap<Uuid, ChannelMeta>,
}

/// steer ack 事件（原生 steer 从 watcher 写回主循环）。
struct SteerAckEvent {
    channel_id: Uuid,
    event_id: String,
    ack: Result<SteerAck, oneshot::error::RecvError>,
}

/// 桥侧句柄。
pub struct BuzzHandle {
    cfg: AgentConfig,
    stop: CancellationToken,
    ctx: Arc<PromptContext>,
    registry: Arc<Mutex<Registry>>,
    /// agent 进程不可用（bridge 预检读）。
    dead: AtomicBool,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    cmd_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Cmd>>>,
    life_tx: mpsc::UnboundedSender<SpawnOutcome>,
    life_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<SpawnOutcome>>>,
    turn_tx: mpsc::UnboundedSender<TurnOutput>,
    turn_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<TurnOutput>>>,
}

impl BuzzHandle {
    /// 新建句柄。不拉起任何进程——agent 懒启动，首条消息到达才 spawn。
    /// `cwd` = agent 子进程工作目录（ABB 启动时的当前目录）。
    pub fn new(cfg: AgentConfig, stop: CancellationToken, cwd: String) -> Arc<Self> {
        let ctx = Arc::new(PromptContext {
            mcp_servers: Vec::new(),
            initial_message: None,
            idle_timeout: IDLE_TIMEOUT,
            max_turn_duration: MAX_TURN_DURATION,
            // 必须 Queue：steer 合并与失败重试的前提（模块文档）。
            dedup_mode: DedupMode::Queue,
            system_prompt: None,
            session_title: None,
            team_instructions: None,
            // base_prompt：ABB 素材（阶段 3 重写）——新会话首回合随 session/new
            // 投递（legacy agent 走首条消息 <base> 段）。内容见 base_prompt.md。
            base_prompt: Some(include_str!("base_prompt.md")),
            cwd,
            channels: std::sync::RwLock::new(HashMap::new()),
            // 0 = 不主动轮换会话（ABB 无 persona 会话预算；频道会话持续到失效）。
            max_turns_per_session: 0,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (life_tx, life_rx) = mpsc::unbounded_channel();
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            cfg,
            stop,
            ctx,
            registry: Arc::new(Mutex::new(Registry::default())),
            dead: AtomicBool::new(false),
            cmd_tx,
            cmd_rx: std::sync::Mutex::new(Some(cmd_rx)),
            life_tx,
            life_rx: std::sync::Mutex::new(Some(life_rx)),
            turn_tx,
            turn_rx: std::sync::Mutex::new(Some(turn_rx)),
        })
    }

    /// agent 进程是否可用（桥侧预检读）。false = 启动/重拉失败退避中，新消息
    /// 会被预检拒绝（避免用户以为已受理）。
    pub fn is_agent_available(&self) -> bool {
        !self.dead.load(Ordering::Relaxed)
    }

    /// 入队一条用户消息（桥 dispatch 后调用）。channel 在跑时按 Queue 语义
    /// 排队并触发 steer（模块文档）。返回 `false` = 句柄已关闭。
    pub fn push_message(&self, channel_id: Uuid, msg: InboundMsg) -> bool {
        self.cmd_tx.send(Cmd::Message { channel_id, msg }).is_ok()
    }

    /// `!cancel`：取消该频道在跑回合。`Ok(true)` = 已向在跑任务发 Cancel 信号；
    /// `Ok(false)` = 没有在跑任务（无副作用）。`None` = 句柄已关闭。
    pub async fn cancel(&self, channel_id: Uuid) -> Option<bool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Cancel {
                channel_id,
                reply: tx,
            })
            .ok()?;
        rx.await.ok()
    }

    /// 登记/刷新一个频道（话题频道 dispatch 前由桥调用；同步完成，不经过主循环）。
    pub fn upsert_channel(&self, channel_id: Uuid, meta: ChannelMeta) {
        let mut reg = self.registry.lock().unwrap();
        if let Ok(mut ctx_w) = self.ctx.channels.write() {
            ctx_w.insert(channel_id, meta.channel_info());
        }
        reg.channels.insert(channel_id, meta);
    }

    pub fn channel_registered(&self, channel_id: &Uuid) -> bool {
        self.registry
            .lock()
            .unwrap()
            .channels
            .contains_key(channel_id)
    }

    pub fn channel_meta(&self, channel_id: &Uuid) -> Option<ChannelMeta> {
        self.registry
            .lock()
            .unwrap()
            .channels
            .get(channel_id)
            .cloned()
    }

    /// 根频道全量同步（service 频道巡检调用；diff 由主循环应用）。
    pub fn sync_roots(&self, roots: Vec<ChannelMeta>) -> bool {
        self.cmd_tx.send(Cmd::SyncRoots(roots)).is_ok()
    }

    /// 取走出站回合接收端（service 的 turn 消费任务调用一次）。
    pub fn take_turn_rx(&self) -> Option<mpsc::UnboundedReceiver<TurnOutput>> {
        self.turn_rx.lock().unwrap().take()
    }

    fn take_cmd_rx(&self) -> Option<mpsc::UnboundedReceiver<Cmd>> {
        self.cmd_rx.lock().unwrap().take()
    }

    fn take_life_rx(&self) -> Option<mpsc::UnboundedReceiver<SpawnOutcome>> {
        self.life_rx.lock().unwrap().take()
    }
}

/// 主循环持有的状态。
struct Loop {
    ctx: Arc<PromptContext>,
    pool: AgentPool,
    queue: EventQueue,
    /// 原生 steer ack watcher 写回通道（watcher 持克隆）。
    steer_ack_tx: mpsc::UnboundedSender<SteerAckEvent>,
    /// 登记表里已消失的频道：其后返回的失败批次直接丢弃、会话失效。
    /// 频道重新登记（upsert）或该频道回合结果处置后移除条目。
    removed_channels: HashSet<Uuid>,
    /// 连续失败次数（退避级别；成功回合/成功拉起清零）。
    crash_backoff: u32,
    /// 是否有懒启动/重拉尝试在跑（防叠加）。
    spawn_in_flight: bool,
    /// 日志活跃度。
    last_activity: Instant,
}

impl Loop {
    fn new(ctx: Arc<PromptContext>) -> (Self, mpsc::UnboundedReceiver<SteerAckEvent>) {
        // 单 slot 懒池：from_slots 保留索引不变式（slot 0 ↔ index 0）。
        let pool = AgentPool::from_slots(vec![None]);
        let queue =
            EventQueue::new(DedupMode::Queue).with_in_flight_deadline(MAX_TURN_DURATION.as_secs());
        let (steer_ack_tx, steer_ack_rx) = mpsc::unbounded_channel::<SteerAckEvent>();
        let l = Self {
            ctx,
            pool,
            queue,
            steer_ack_tx,
            removed_channels: HashSet::new(),
            crash_backoff: 0,
            spawn_in_flight: false,
            last_activity: Instant::now(),
        };
        (l, steer_ack_rx)
    }
}

/// harness 主循环任务体（service 以 spawn_forever 驱动；退出 = 关停 token 触发
/// 后的优雅收尾完成）。
pub async fn run_loop(handle: Arc<BuzzHandle>) {
    let mut cmd_rx = handle
        .take_cmd_rx()
        .expect("run_loop must own the cmd receiver (call once)");
    let mut life_rx = handle
        .take_life_rx()
        .expect("run_loop must own the spawn-outcome receiver (call once)");
    let (mut l, mut steer_ack_rx) = Loop::new(handle.ctx.clone());

    tracing::info!(
        "buzz harness started (lazy pool, agent command = {})",
        handle.cfg.command
    );

    loop {
        // 事件收集：select 内借用 pool 的 rx/join_set，出块即归还，处理器再拿
        // &mut pool（上游同款 dance）。
        let evt: Option<Evt> = {
            let (result_rx, join_set) = l.pool.rx_and_join_set();
            tokio::select! {
                biased;
                _ = handle.stop.cancelled() => break,
                cmd = cmd_rx.recv() => {
                    // None = 全部句柄已 drop（本循环持 Arc，正常不会发生）。
                    cmd.map(Evt::Cmd)
                }
                r = result_rx.recv() => r.map(|pr| Evt::Result(Box::new(pr))),
                j = join_set.join_next(), if !join_set.is_empty() => j.map(Evt::Panic),                a = steer_ack_rx.recv() => a.map(Evt::SteerAck),
                life = life_rx.recv() => life.map(Evt::Life),
            }
        };
        let Some(evt) = evt else {
            tracing::error!("buzz harness event channel closed — exiting loop");
            break;
        };
        match evt {
            Evt::Cmd(cmd) => handle_cmd(&mut l, &handle, cmd),
            Evt::Result(result) => handle_prompt_result(&mut l, &handle, *result),
            Evt::Panic(join_error) => recover_panicked_agent(&mut l, &handle, join_error),
            Evt::SteerAck(ack) => handle_steer_ack(&mut l, ack),
            Evt::Life(outcome) => handle_spawn_outcome(&mut l, &handle, outcome),
        }
        dispatch_pending(&mut l);
    }

    shutdown(&mut l).await;
    tracing::info!("buzz harness stopped");
}

enum Evt {
    Cmd(Cmd),
    Result(Box<PromptResult>),
    /// join_set 任务结束：`Err` = panic；`Ok(())` = 正常提前返回（结果随后走
    /// result_rx，属良性竞态，忽略）。
    Panic(Result<(), tokio::task::JoinError>),
    SteerAck(SteerAckEvent),
    Life(SpawnOutcome),
}

// ── Cmd 处理 ─────────────────────────────────────────────────────────────────

fn handle_cmd(l: &mut Loop, handle: &BuzzHandle, cmd: Cmd) {
    match cmd {
        Cmd::Message { channel_id, msg } => {
            let accepted = l.queue.push(QueuedEvent {
                channel_id,
                msg: msg.clone(),
                received_at: Instant::now(),
            });
            if !accepted {
                tracing::warn!(%channel_id, "message dropped by queue policy");
                return;
            }
            // 已入队。频道在跑 → 固定 Steer 模式门：先试非取消 ACP steer
            // 分叉，失败（pi-acp 无扩展，恒失败）走 cancel+merge 回退信号。
            if l.queue.is_channel_in_flight(channel_id) {
                let native_attempted = try_native_steer(l, channel_id, &msg);
                if !native_attempted {
                    let fired =
                        signal_in_flight_task(&mut l.pool, channel_id, ControlSignal::Steer);
                    if !fired {
                        // 理论不可达（is_channel_in_flight 刚为真）；兜底：消息
                        // 留在队列，正常 dispatch 会带走。
                        tracing::warn!(%channel_id, "steer signal target vanished — message stays queued");
                    }
                }
            }
            // 懒启动：slot 空且没有尝试在跑 → 立即拉起。
            if !l.pool.slot_alive(0) && !l.spawn_in_flight {
                schedule_agent_start(l, handle, Duration::ZERO, None);
            }
        }
        Cmd::Cancel { channel_id, reply } => {
            let fired = signal_in_flight_task(&mut l.pool, channel_id, ControlSignal::Cancel);
            let _ = reply.send(fired);
        }
        Cmd::SyncRoots(roots) => {
            // diff：对比登记表里 thread_id == None 的根频道。
            let current: HashMap<(String, String), Uuid> = {
                let reg = handle.registry.lock().unwrap();
                reg.channels
                    .iter()
                    .filter(|(_, m)| m.thread_id.is_none())
                    .map(|(uuid, m)| ((m.bot_key.clone(), m.chat_id.clone()), *uuid))
                    .collect()
            };
            let mut next: HashMap<(String, String), Uuid> = HashMap::new();
            for meta in roots {
                let key = (meta.bot_key.clone(), meta.chat_id.clone());
                let uuid = Uuid::parse_str(&crate::buzz::keys::channel_uuid(
                    &meta.bot_key,
                    &meta.chat_id,
                ))
                .expect("channel_uuid output must parse as Uuid");
                if current.get(&key).copied() != Some(uuid) {
                    // 新增或改名/刷新。
                    handle.upsert_channel(uuid, meta);
                }
                next.insert(key, uuid);
            }
            // 消失的根频道：排空队列、失效会话、其后返回的批次直接丢弃。
            for (key, uuid) in current {
                if next.contains_key(&key) {
                    continue;
                }
                let drained = l.queue.drain_channel(uuid);
                l.removed_channels.insert(uuid);
                let invalidated = l.pool.invalidate_channel_sessions(uuid);
                {
                    let mut reg = handle.registry.lock().unwrap();
                    reg.channels.remove(&uuid);
                    tracing::info!(
                        %uuid,
                        chat = %key.1,
                        drained = drained.len(),
                        invalidated,
                        "root channel removed — drained queue and invalidated sessions"
                    );
                }
                if let Ok(mut ctx_w) = handle.ctx.channels.write() {
                    ctx_w.remove(&uuid);
                }
            }
        }
    }
}

// ── agent 生命周期 ───────────────────────────────────────────────────────────

fn handle_spawn_outcome(l: &mut Loop, handle: &BuzzHandle, outcome: SpawnOutcome) {
    l.spawn_in_flight = false;
    match outcome {
        SpawnOutcome::Ok(agent) => {
            let agent = *agent;
            l.crash_backoff = 0;
            handle.dead.store(false, Ordering::Relaxed);
            l.pool.return_agent(agent);
            tracing::info!("agent process ready");
        }
        SpawnOutcome::Err(detail) => {
            handle.dead.store(true, Ordering::Relaxed);
            l.crash_backoff = l.crash_backoff.saturating_add(1);
            let delay = respawn_delay(l.crash_backoff);
            tracing::error!(
                backoff_secs = delay.as_secs(),
                "agent start failed — retrying in {}s: {detail}",
                delay.as_secs()
            );
            schedule_agent_start(l, handle, delay, None);
        }
    }
}

/// 2^level 秒指数退避，封顶 RESPAWN_BACKOFF_MAX_SECS。
fn respawn_delay(level: u32) -> Duration {
    let secs = RESPAWN_BACKOFF_BASE_SECS
        .saturating_pow(level.min(6))
        .min(RESPAWN_BACKOFF_MAX_SECS);
    Duration::from_secs(secs)
}

/// 排队一个后台启动尝试。`old_agent` 存在（崩溃重拉）时先对其 shutdown 收尸。
/// 尝试全程对 stop token 让步：关停途中不残留新子进程。
fn schedule_agent_start(
    l: &mut Loop,
    handle: &BuzzHandle,
    delay: Duration,
    old_agent: Option<OwnedAgent>,
) {
    debug_assert!(!l.spawn_in_flight, "must not stack spawn attempts");
    l.spawn_in_flight = true;
    let cfg = handle.cfg.clone();
    let stop = handle.stop.clone();
    let life_tx = handle.life_tx.clone();
    tokio::spawn(async move {
        // 先收尸再睡再拉：child reaping 与停止语义无关，先做掉最稳。
        if let Some(mut agent) = old_agent {
            agent.acp.shutdown().await;
            drop(agent);
        }
        let attempt = async {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            spawn_and_init_agent(&cfg).await
        };
        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => None,
            r = attempt => Some(r),
        };
        let Some(result) = result else {
            return; // 关停：attempt 已放弃，无残留进程
        };
        let _ = life_tx.send(result);
    });
}

/// 后台启动尝试本体：spawn + initialize（60s 超时，与上游一致），成功后构造
/// OwnedAgent（全新会话状态——新进程的旧 session id 全部失效）。
async fn spawn_and_init_agent(cfg: &AgentConfig) -> SpawnOutcome {
    let mut acp = match AcpClient::spawn(&cfg.command, &cfg.args, &cfg.extra_env).await {
        Ok(acp) => acp,
        Err(e) => return SpawnOutcome::Err(format!("failed to spawn agent: {e}")),
    };
    match tokio::time::timeout(Duration::from_secs(60), acp.initialize()).await {
        Ok(Ok(init_result)) => {
            let protocol_version = init_result["protocolVersion"].as_u64().unwrap_or(1) as u32;
            let agent_name = normalized_agent_name(&init_result);
            tracing::info!(name = %agent_name, protocol_version, "agent initialized");
            SpawnOutcome::Ok(Box::new(OwnedAgent {
                index: 0,
                acp,
                state: Default::default(),
                agent_name,
                goose_system_prompt_supported: None,
                protocol_version,
            }))
        }
        Ok(Err(e)) => {
            acp.shutdown().await;
            SpawnOutcome::Err(format!("agent initialize failed: {e}"))
        }
        Err(_) => {
            acp.shutdown().await;
            SpawnOutcome::Err("agent initialize timed out (60s)".to_string())
        }
    }
}

fn normalized_agent_name(init_result: &serde_json::Value) -> String {
    init_result
        .get("agentInfo")
        .or_else(|| init_result.get("serverInfo"))
        .and_then(|info| info.get("name"))
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase()
}

// ── dispatch_pending（上游移植；无 typing/心跳） ─────────────────────────────

/// 把队列里的可跑批次投给空闲 agent（单 slot：至多一个批次在跑）。
fn dispatch_pending(l: &mut Loop) {
    // 单 agent：至多一个批次在跑——投一个即返回，结果回来再 flush 下一个。
    let Some(batch) = l.queue.flush_next() else {
        return;
    };
    let channel_id = batch.channel_id;
    let affinity_hit = l.pool.has_session_for(channel_id);
    let mut agent = match l.pool.try_claim(Some(channel_id)) {
        Some(a) => a,
        None => {
            // 单 slot 被占用（或空）——批次放回队首，下个事件再试。
            tracing::debug!(
                pending = l.queue.pending_channels(),
                "agent busy or absent — batch stays queued"
            );
            l.queue.requeue_preserve_timestamps(batch);
            l.queue.mark_complete(channel_id);
            return;
        }
    };
    tracing::debug!(agent = agent.index, %channel_id, affinity_hit, "agent_claimed");

    // Queue 模式：克隆进 TaskMeta 供 panic 恢复重拉。
    let recoverable_batch = match l.ctx.dedup_mode {
        DedupMode::Queue => Some(batch.clone()),
        DedupMode::Drop => None,
    };

    let result_tx = l.pool.result_tx();
    let ctx_clone = l.ctx.clone();
    let agent_index = agent.index;

    // 每轮次安装 steer 接收端（读循环在写时刻自选传输；pi-acp 无扩展 →
    // ExpectedRunIdMissing → 主循环 cancel+merge 回退）。
    let (steer_tx, steer_rx) = mpsc::channel::<SteerRequest>(1);
    agent.acp.install_steer_rx(steer_rx);
    let (control_tx, control_rx) = oneshot::channel::<ControlSignal>();

    let abort_handle = l.pool.join_set.spawn(async move {
        crate::buzz::pool::run_prompt_task(agent, batch, ctx_clone, result_tx, Some(control_rx))
            .await;
    });
    l.pool.task_map_mut().insert(
        abort_handle.id(),
        crate::buzz::pool::TaskMeta {
            agent_index,
            channel_id: Some(channel_id),
            recoverable_batch,
            control_tx: Some(control_tx),
            steer_tx: Some(steer_tx),
            successful_steer_deliveries: HashSet::new(),
        },
    );
    l.last_activity = Instant::now();
}

// ── 中间轮次 steer / cancel 信号 ─────────────────────────────────────────────

/// 向 in-flight 任务发控制信号。返回是否发出。
fn signal_in_flight_task(pool: &mut AgentPool, channel_id: Uuid, mode: ControlSignal) -> bool {
    if let Some(meta) = pool
        .task_map_mut()
        .values_mut()
        .find(|m| m.channel_id == Some(channel_id))
    {
        if let Some(tx) = meta.control_tx.take() {
            tracing::info!(%channel_id, ?mode, "control signal sent to in-flight task");
            let _ = tx.send(mode);
            return true;
        }
    }
    false
}

/// 非取消 steer 分叉：先尝试原生 ACP steer；pi-acp 无扩展 → send_steer 失败，
/// 调用方必须走 cancel+merge 回退。语义与上游 try_native_steer 逐条对齐
/// （/tmp/up_lib_dispatch.txt；withhold 先于 watcher 关闭竞态）。
fn try_native_steer(l: &mut Loop, channel_id: Uuid, msg: &InboundMsg) -> bool {
    let (tag, closing) = crate::buzz::queue::native_steer_framing();
    let be = BatchEvent {
        msg: msg.clone(),
        received_at: Instant::now(),
    };
    let event_block = crate::buzz::queue::format_event_block(channel_id, None, &be);
    let new_message = crate::buzz::prompt_framing::semantic_section(tag, "");
    let event_section = crate::buzz::prompt_framing::semantic_section_with_attributes(
        "buzz-event",
        &[("type", msg.prompt_tag.as_str())],
        &event_block,
    );
    let body = format!("{new_message}\n\n{event_section}\n\n{closing}");

    let (ack_tx, ack_rx) = oneshot::channel::<SteerAck>();
    let request = SteerRequest {
        prompt_blocks: vec![body],
        ack_tx,
    };

    match l.pool.send_steer(channel_id, request) {
        Ok(()) => {
            // 先同步 withhold 再 spawn watcher：关闭 mark_complete 清 in-flight
            // 后 stray flush 重投的竞态（queue.rs mark_native_steer_pending）。
            let withheld = l.queue.mark_native_steer_pending(channel_id, &msg.id_hex);
            if !withheld {
                tracing::warn!(
                    %channel_id,
                    event_id = %msg.id_hex,
                    "native steer accepted but event not in queue to withhold — possible duplicate"
                );
            }
            let steer_ack_tx = l.steer_ack_tx.clone();
            let event_id = msg.id_hex.clone();
            tokio::spawn(async move {
                let ack = ack_rx.await;
                let _ = steer_ack_tx.send(SteerAckEvent {
                    channel_id,
                    event_id,
                    ack,
                });
            });
            true
        }
        Err(e) => {
            tracing::debug!(
                %channel_id,
                error = ?e,
                "non-cancelling steer not accepted — cancel+merge fallback"
            );
            false
        }
    }
}

// ── handle_prompt_result（上游移植；无 observer/心跳/rest） ───────────────────

fn handle_prompt_result(l: &mut Loop, handle: &BuzzHandle, mut result: PromptResult) {
    let agent_index = result.agent.index;
    // 该 agent 的任务元数据整体清出（单 agent：task_map 至多一条），并把成功
    // steer 交付并入存活会话的账本（防迟到 ack 污染换代会话）。
    let successful_steer_deliveries = l
        .pool
        .task_map()
        .values()
        .find(|meta| meta.agent_index == agent_index)
        .map(|meta| meta.successful_steer_deliveries.clone())
        .unwrap_or_default();
    l.pool
        .task_map_mut()
        .retain(|_, meta| meta.agent_index != agent_index);
    let PromptSource::Channel(channel_id) = &result.source;
    if let Some(live_session_id) = result.agent.state.sessions.get(channel_id).cloned() {
        let event_ids = successful_steer_deliveries
            .into_iter()
            .filter(|d| d.session_id == live_session_id)
            .map(|d| d.event_id);
        result
            .agent
            .state
            .mark_channel_delivery_success(*channel_id, false, event_ids);
    }

    // 批次命运：requeue 先于 mark_complete（requeue 设 retry_after 而
    // mark_complete 清 retry_counts——顺序颠倒废掉退避与死信保护）。
    if let Some(batch) = result.batch.take() {
        if !l.removed_channels.contains(&batch.channel_id) {
            if matches!(
                result.outcome,
                PromptOutcome::Cancelled | PromptOutcome::CancelDrainTimeout(_)
            ) {
                // cancel 重提示：进 cancelled_batches，flush_next 并入下一批
                // 重提示。CancelDrainTimeout 与干净取消同路（5s 排水超时不是
                // 硬上限死信；批次不带重试账）。
                let reason = batch.cancel_reason.unwrap_or(CancelReason::Steer);
                l.queue.requeue_as_cancelled(batch, reason);
            } else if matches!(
                result.outcome,
                PromptOutcome::Timeout(TimeoutKind::Hard {
                    recently_active: false
                })
            ) {
                tracing::error!(
                    channel_id = %batch.channel_id,
                    events = batch.events.len(),
                    "dead-lettering batch after hard-cap timeout (no recent activity)"
                );
                notify_channel(
                    l,
                    handle,
                    &batch,
                    format!(
                        "⚠️ 上一轮处理超过时长上限（{}s），消息未完成；如仍需要请重发。",
                        MAX_TURN_DURATION.as_secs()
                    ),
                );
            } else if matches!(
                result.outcome,
                PromptOutcome::Timeout(TimeoutKind::Hard {
                    recently_active: true
                })
            ) {
                tracing::warn!(
                    channel_id = %batch.channel_id,
                    "hard-cap timeout with recent activity — requeueing for retry"
                );
                if let Some(dead) = l.queue.requeue(batch) {
                    notify_channel(
                        l,
                        handle,
                        &dead,
                        format!(
                            "⚠️ 多次重试后仍未处理完成（超过时长上限 {}s）；如仍需要请重发。",
                            MAX_TURN_DURATION.as_secs()
                        ),
                    );
                }
            } else if matches!(&result.outcome, PromptOutcome::Error(e) if is_auth_error(e)) {
                // 认证错误不可重试：立即死信并提示重登。
                tracing::warn!(
                    channel_id = %batch.channel_id,
                    "dead-lettering batch — non-retryable auth error"
                );
                notify_channel(
                    l,
                    handle,
                    &batch,
                    "⚠️ 处理失败：agent 认证失效。请重新登录对应 CLI 后再试。".to_string(),
                );
            } else if let Some(dead) = l.queue.requeue(batch) {
                let reason = match &result.outcome {
                    PromptOutcome::Timeout(TimeoutKind::Idle) => "回合超时".to_string(),
                    PromptOutcome::Timeout(TimeoutKind::Hard { .. }) => {
                        "回合超过时长上限".to_string()
                    }
                    PromptOutcome::AgentExited => "agent 进程退出".to_string(),
                    PromptOutcome::Error(e) => format!("{e}"),
                    _ => "重复失败".to_string(),
                };
                notify_channel(
                    l,
                    handle,
                    &dead,
                    format!("⚠️ 多次重试后仍未处理成功（{reason}）；如仍需要请重发。"),
                );
            }
        } else {
            tracing::debug!(
                channel_id = %batch.channel_id,
                "dropping failed batch for removed channel"
            );
        }
    }

    let PromptSource::Channel(ch) = &result.source;
    l.queue.mark_complete(*ch);
    // 该频道的 in-flight 批次已处置——从 removed 里消费掉（同频道后续
    // 推送只可能来自重新登记，upsert 会再把它清掉）。
    l.removed_channels.remove(ch);

    // 回合途中被移除的频道：清掉 agent 上它的会话。
    for ch in &l.removed_channels {
        result.agent.state.invalidate_channel(ch);
    }

    let outcome_label = match &result.outcome {
        PromptOutcome::Ok(_) => "ok",
        PromptOutcome::Error(_) => "error",
        PromptOutcome::Timeout(TimeoutKind::Idle) => "idle_timeout",
        PromptOutcome::Timeout(TimeoutKind::Hard { .. }) => "hard_timeout",
        PromptOutcome::AgentExited => "exited",
        PromptOutcome::Cancelled => "cancelled",
        PromptOutcome::CancelDrainTimeout(_) => "cancel_drain_timeout",
    };

    match result.outcome {
        // 成功：收 agent + 同步投递捕获文本（空文本 = 纯工具回合，不投递）。
        PromptOutcome::Ok(_) => {
            let PromptSource::Channel(channel_id) = &result.source;
            if let Some(text) = result.final_text.take() {
                let meta = handle.channel_meta(channel_id);
                tracing::info!(%channel_id, text_chars = text.chars().count(), "turn text captured — delivering");
                let _ = handle.turn_tx.send(TurnOutput {
                    channel_id: *channel_id,
                    meta,
                    text,
                });
            }
            return_agent(l, result.agent, outcome_label, "agent_returned");
        }
        // 致命：进程死/中毒——重拉任务里先收尸再拉起。
        PromptOutcome::AgentExited | PromptOutcome::Timeout(_) => {
            tracing::warn!(
                agent = agent_index,
                outcome = outcome_label,
                "agent died — respawning"
            );
            schedule_death_respawn(l, handle, result.agent, outcome_label);
        }
        // CancelDrainTimeout：cancel 宽限内没停——进程不确定，同致命重拉。
        PromptOutcome::CancelDrainTimeout(_) => {
            tracing::warn!(
                agent = agent_index,
                outcome = outcome_label,
                "cancel drain timeout — respawning agent"
            );
            schedule_death_respawn(l, handle, result.agent, outcome_label);
        }
        // 显式取消：agent 健康，直接回池。
        PromptOutcome::Cancelled => {
            tracing::debug!(
                agent = agent_index,
                outcome = outcome_label,
                "agent_returned (cancelled)"
            );
            return_agent(l, result.agent, outcome_label, "agent_returned (cancelled)");
        }
        // 错误分两类：transport 类（管道可能坏）→ 重拉；应用类 → 回池复用。
        PromptOutcome::Error(ref e) => {
            let is_transport_error = matches!(
                e,
                AcpError::Io(_)
                    | AcpError::WriteTimeout(_)
                    | AcpError::Timeout(_)
                    | AcpError::Protocol(_)
            );
            if is_transport_error {
                tracing::warn!(
                    agent = agent_index,
                    outcome = outcome_label,
                    error = %e,
                    "transport/protocol error — respawning agent"
                );
                schedule_death_respawn(l, handle, result.agent, outcome_label);
            } else {
                tracing::warn!(
                    agent = agent_index,
                    outcome = outcome_label,
                    error = %e,
                    "agent_returned (application error — pipe intact)"
                );
                return_agent(l, result.agent, outcome_label, "agent_returned");
            }
        }
    }
}

fn return_agent(l: &mut Loop, agent: OwnedAgent, outcome_label: &str, log_line: &str) {
    tracing::debug!(agent = agent.index, outcome = outcome_label, "{log_line}");
    l.pool.return_agent(agent);
}

/// 致命结局统一入口：退避升级 + 置 Dead（新消息预检拒绝）+ 后台重拉。
fn schedule_death_respawn(
    l: &mut Loop,
    handle: &BuzzHandle,
    old_agent: OwnedAgent,
    outcome_label: &str,
) {
    l.crash_backoff = l.crash_backoff.saturating_add(1);
    let delay = respawn_delay(l.crash_backoff);
    handle.dead.store(true, Ordering::Relaxed);
    tracing::warn!(
        agent = old_agent.index,
        outcome = outcome_label,
        backoff_secs = delay.as_secs(),
        "agent died — respawning in {}s",
        delay.as_secs()
    );
    schedule_agent_start(l, handle, delay, Some(old_agent));
}

/// 失败批次死信/重试耗尽时的频道告示（出站 TurnOutput，桥侧写历史并发送）。
fn notify_channel(l: &mut Loop, handle: &BuzzHandle, batch: &FlushBatch, text: String) {
    if l.removed_channels.contains(&batch.channel_id) {
        return;
    }
    let meta = handle.channel_meta(&batch.channel_id);
    tracing::warn!(channel_id = %batch.channel_id, "sending failure notice to channel");
    let _ = handle.turn_tx.send(TurnOutput {
        channel_id: batch.channel_id,
        meta,
        text,
    });
}

/// 认证错误识别（不可重试；两模式高精度防误杀，上游同款）。
fn is_auth_error(error: &AcpError) -> bool {
    let AcpError::AgentError { message, .. } = error else {
        return false;
    };
    message.contains("Re-authenticate") || message.contains("API Error: 401")
}

// ── recover_panicked_agent（上游移植） ───────────────────────────────────────

fn recover_panicked_agent(
    l: &mut Loop,
    handle: &BuzzHandle,
    joined: Result<(), tokio::task::JoinError>,
) {
    let join_error = match joined {
        Ok(()) => {
            // 正常返回的任务其结果必已/将走 result_rx——无需恢复。
            return;
        }
        Err(e) => e,
    };
    let task_id = join_error.id();
    let Some(meta) = l.pool.task_map_mut().remove(&task_id) else {
        tracing::error!("panic for unknown task {task_id:?} — bug");
        return;
    };
    let i = meta.agent_index;

    // requeue 先于 mark_complete（同 handle_prompt_result 理由）。
    if let Some(batch) = meta.recoverable_batch {
        if let Some(ch) = meta.channel_id {
            if !l.removed_channels.contains(&ch) {
                let _ = l.queue.requeue(batch);
                tracing::warn!("requeued batch for panicked agent {i}");
            } else {
                tracing::debug!(channel_id = %ch, "dropping panicked batch for removed channel");
            }
        }
    }
    if let Some(ch) = meta.channel_id {
        l.queue.mark_complete(ch);
        l.removed_channels.remove(&ch);
        tracing::warn!("cleared wedged in-flight channel {ch} from panicked agent {i}");
    }

    tracing::error!(agent = i, "agent task panicked: {join_error}");
    // panic 的任务已把 AcpClient drop（kill_on_drop 尽力收尸）——只需安排重拉。
    l.crash_backoff = l.crash_backoff.saturating_add(1);
    let delay = respawn_delay(l.crash_backoff);
    handle.dead.store(true, Ordering::Relaxed);
    tracing::warn!(
        agent = i,
        backoff_secs = delay.as_secs(),
        "respawn after panic"
    );
    schedule_agent_start(l, handle, delay, None);
}

// ── steer ack（上游移植；无 typing） ─────────────────────────────────────────

fn handle_steer_ack(l: &mut Loop, event: SteerAckEvent) {
    let SteerAckEvent {
        channel_id,
        event_id,
        ack,
    } = event;
    // 锁定语义（/tmp/up_lib_loop.txt 注释全文）：
    //   Success                      → drop withheld（已投递，防正常 dispatch 重投）
    //   Err(-32601)                  → release + cancel+merge 回退（无扩展）
    //   Err(AgentError 其他)         → release，不回退（写已落地，应用层拒绝）
    //   Err(Transport/ExpectedRunIdMissing/OutcomeRejected) → release + 回退
    //   PromptCompletedNeutral       → release，不回退（turn 已终，正常 dispatch 重投）
    //   oneshot 被关                 → 同 PromptCompletedNeutral
    let (release_withheld, drop_withheld, signal_fallback) = match &ack {
        Ok(SteerAck::Success { .. }) => (false, true, false),
        Ok(SteerAck::Err(SteerError::AgentError { code, .. })) if *code == -32601 => {
            (true, false, true)
        }
        Ok(SteerAck::Err(SteerError::AgentError { .. })) => (true, false, false),
        Ok(SteerAck::Err(_)) => (true, false, true),
        Ok(SteerAck::PromptCompletedNeutral) => (true, false, false),
        Err(_recv_err) => (true, false, false),
    };
    tracing::debug!(
        %channel_id,
        %event_id,
        release_withheld,
        drop_withheld,
        signal_fallback,
        "non-cancelling steer ack received"
    );
    if let Ok(SteerAck::Success { session_id }) = &ack {
        l.queue
            .extend_in_flight_deadline(channel_id, MAX_TURN_DURATION.as_secs());
        if !l
            .pool
            .record_successful_steer(channel_id, event_id.clone(), session_id.clone())
        {
            tracing::warn!(
                %channel_id,
                %event_id,
                "successful steer lost its in-flight delivery ledger"
            );
        }
    }
    if drop_withheld {
        l.queue.remove_event(channel_id, &event_id);
    }
    if release_withheld {
        l.queue.release_native_steer(channel_id, &event_id);
    }
    if signal_fallback {
        // cancel+merge 回退：withheld 事件已放回队首，cancel 后 flush 会把
        // cancelled 事件并入合并批次（格式重提示）。
        signal_in_flight_task(&mut l.pool, channel_id, ControlSignal::Steer);
    }
}

// ── 优雅关闭（上游 shutdown 尾巴移植：30s 宽限排水 → 终止 → 收尸） ───────────

async fn shutdown(l: &mut Loop) {
    tracing::info!("shutdown: waiting for in-flight prompts (up to 30s)");
    // 后台启动尝试已对 stop 让步，但可能卡在 spawn 系统调用里；宽限内 join 不
    // 回就 abort（AcpClient 在任务里被 drop，kill_on_drop 尽力收尸）。
    let (rx_ref, js_ref) = l.pool.rx_and_join_set();
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        loop {
            tokio::select! {
                r = js_ref.join_next() => {
                    match r {
                        Some(Err(e)) => tracing::warn!("task error during shutdown: {e}"),
                        Some(Ok(())) => {}
                        None => break, // join_set 空
                    }
                }
                r = rx_ref.recv() => {
                    if let Some(mut pr) = r {
                        pr.agent.acp.shutdown().await;
                        tracing::debug!(agent = pr.agent.index, "reaped checked-out agent on shutdown");
                    }
                    // None = 频道关（本 harness 持有 sender，不会发生）
                }
            }
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("grace period expired — aborting remaining tasks");
        l.pool.join_set.shutdown().await;
    }
    while let Ok(mut pr) = l.pool.result_rx_try_recv() {
        pr.agent.acp.shutdown().await;
        tracing::debug!(
            agent = pr.agent.index,
            "reaped late-arriving agent on shutdown"
        );
    }
    for slot in l.pool.agents_mut().iter_mut() {
        if let Some(mut agent) = slot.take() {
            agent.acp.shutdown().await;
            tracing::debug!(agent = agent.index, "reaped idle agent on shutdown");
        }
    }
}
