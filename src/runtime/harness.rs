//! runtime —— 进程内 ACP agent 管理与投递主循环（ABB 单进程形态）。
//!
//! 上游 buzz-acp（`crates/buzz-acp` @ c3132c3）的 relay 形态整个被替换为进程内
//! 消息总线：ABB 桥把聊天消息 `push` 进来，harness 排队 → 投给唯一一个 pi-acp
//! 子进程 → 回合结束时把捕获的 agent 文本同步投回聊天。
//!
//! 裁剪对照（相对上游 lib.rs 主循环）：
//! - **懒池**：单 slot，首条消息才拉起 agent 子进程；崩溃后指数退避自动重拉
//!   （2s → 封顶 60s，成功即清零）。上游的熔断开闸/全池退出不适用——ABB 是宿主
//!   进程，runtime 后端不可用只影响该频道，不该拖垮整进程。
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

use crate::runtime::acp::{AcpClient, AcpError, ResourceUpdate, TokenUsage, TurnChunk};
use crate::runtime::pool::{
    AgentPool, ControlSignal, OwnedAgent, PromptContext, PromptOutcome, PromptResult, PromptSource,
    SteerAck, SteerError, SteerRequest, TimeoutKind,
};
use crate::runtime::queue::{
    BatchEvent, CancelReason, DedupMode, EventQueue, FlushBatch, InboundMsg, PromptChannelInfo,
    QueuedEvent,
};

/// 回合空闲超时：agent 静默（无 ACP 线活动）这么久判死。
///
/// 必须**长于**旧执行层当年的工具超时墙（默认 1260s，上游 #7185；该层已随 0685e66 删除，
/// 现役 abb-agent 没有同名墙）：工具墙先到，模型能拿到「工具超时」并在同一回合里换个做法；
/// 空闲墙先到就变成整回合被杀，用户只看到一句笼统失败。排序：
/// 工具 1260 < 空闲 1500 < 单回合硬上限 [`MAX_TURN_DURATION`] 3600。
const IDLE_TIMEOUT: Duration = Duration::from_secs(1_500);
/// 单回合硬上限（chat 与 job 同款；job 同步等待预算 = 本值 + 小余量）。
pub const MAX_TURN_DURATION: Duration = Duration::from_secs(3600);
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
    /// 会话类型："group"（登记表频道）/ "p2p"（私聊，dispatch 即时注册）。
    /// 巡检「消失清理」豁免 adhoc——不在登记表的频道（p2p / 自动登记的群）
    /// 是常态而非消失，误清会排空队列丢回复。
    pub chat_type: String,
    pub thread_id: Option<String>,
    /// 群展示名（角色名）：进 prompt 上下文与 session 标题。
    pub name: String,
    /// 话题回复锚点 mid（话题频道每次 dispatch 时由桥刷新）。
    pub anchor_mid: Option<String>,
    /// 即时登记（非巡检登记表来源）：p2p 私聊与「自动登记」的群。巡检 diff
    /// 清理跳过 adhoc 频道（它们不在登记表，误清会丢会话）。
    pub adhoc: bool,
    /// agent 工作目录（P0.B）：session/new 的 cwd 与 <workspace> 段用它；
    /// None = 回落 handle 级 cwd（进程启动目录）。
    pub workspace: Option<String>,
}

impl ChannelMeta {
    fn channel_info(&self) -> PromptChannelInfo {
        PromptChannelInfo {
            name: self.name.clone(),
            // 队列侧约定："dm" = 单聊，其余 = 群聊。写死 "channel" 曾让 p2p 私聊
            // 在 agent 上下文里显示「类型：群聊」。
            channel_type: if self.chat_type == "p2p" {
                "dm".to_string()
            } else {
                "channel".to_string()
            },
            description: None,
            workspace: self.workspace.clone(),
        }
    }
}

/// agent 子进程启动参数（service 侧解析 pi-acp 路径后传入）。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub command: String,
    pub args: Vec<String>,
    /// 无条件覆盖的环境变量（ABB 合成 PATH；与上游「缺失才注入」语义不同：
    /// service 用它对子进程 PATH 做全量接管）。
    pub extra_env: Vec<(String, String)>,
    /// ABB 后端标识（单后端化后恒 "buzz"）。**该字面量是持久值**：history 条目与
    /// session marker 里都按它写出处标注（老数据也写的是 "buzz"），改名要迁移落盘
    /// 数据，故保留。P4.3 删除回复尾部「── 后端：X」
    /// 标注后暂无消费方——字段保留作配置透传（service → handle）与将来日志
    /// 标注用；移除它要牵动 service/测试全部构造点，留给后续清理批次。
    #[allow(dead_code)] // 暂无读方（P4.3 后缀标注已删），见上行注释——后续批次移除
    pub backend: String,
    /// 本 handle 全部会话的执行档位载荷（单后端化 P2.2）：随 `session/new`
    /// `_meta` 下发。normal handle = bot 配置档（`sandbox_mode` 解析产物，
    /// None=FullAccess 今天行为）；granted handle = 强制 Restricted 剖面
    ///（workspace-write + shell 白名单 + `$ABB_BIN`）——同群 owner/granted
    /// 混合频道的档位只能绑进程，不能绑会话（角色 per-event）。
    pub session_sandbox: Option<crate::runtime::acp::SessionSandboxMeta>,
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

/// 同步回合结局（P3.1，[`RuntimeHandle::wait_turn_outcome`]）：oneshot 路径需要
/// 区分超时/关闭/agent 错误/取消终态；调用方按需自行折叠。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTurnOutcome {
    /// 回合文本（与 chat 投递同款处理；P4.3 起不再附后端标识后缀）。
    Ok(String),
    /// 调用方预算先到（在途回合可能仍在跑——调用方负责 cancel 防迟发）。
    Timeout,
    /// 句柄已关闭，消息 push 不进主循环。`wait_turn_outcome` 的 `Ok(None)` 臂
    /// （等待者回传端消失）也折叠到本变体。
    ///
    /// **可达**：`Ok(None)` 那条曾由「同一 chat 的两条 job 并发进入 `run_job`、
    /// 后登记那条顶掉在跑那条的 waiter」触发（#321）；job 侧已改为**同 chat 单飞
    /// 串行**（`run_job` 全程持 `Bridge::job_gate`，见 `docs/task-model.md` §B），
    /// 该来源不再可达——现在只剩「句柄已关闭（服务在关停）」。job 侧对 `Closed`
    /// 的处理见 `service::job_outcome_reply`（静默收尾，不复用超时文案）。
    Closed,
    /// 取消**终态**（#309 PR-A1）：该轮以取消收尾，且其批次已被丢弃、不会重提示
    /// ——不是「取消信号已发出」这种中间态。与 Timeout 同款 teardown 已完成。
    ///
    /// 自然完成优先：若该轮其实以 `Ok` 收尾，则不会走到这里（给等待方真实文本）。
    /// 需要「取消静默收尾」的调用方（如定时任务）直接匹配本变体。
    Cancelled,
    /// agent 终态失败（死信/失败告示的原因文案，notify_channel 旁路）。
    Failed(String),
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
    /// **丢弃排队中的定时任务消息**（#309 PR-B）：只按 `prompt_tag ==
    /// crate::schedule::JOB_PROMPT_TAG` 过滤丢弃该频道的排队事件，**不动**用户消息、
    /// 也不动 in-flight 轮次。
    ///
    /// 覆盖三个存放点（#320）：`queues`（普通排队）、`withheld_native_steer`（native steer
    /// 的 ack 窗口）、`cancelled_batches`（Steer 合并待重提示窗口）——只扫 `queues` 时，
    /// job 消息落在后两个窗口会 drop=0，停止词有回执但 job 之后仍会跑。
    ///
    /// ⚠️ `withheld_native_steer` 的 **Success 子情形**：ack 回来前，steer 请求可能**已经被
    /// 写进 agent 并并入在跑回合**（`handle_steer_ack` 的 Success 臂就是「drop withheld，
    /// 已投递防重投」）。所以本命令对那种情形只能防**重投/重排**，**撤不回**已并入的内容——
    /// 真正让 job 停下来的是随后的 `cancel(channel)` 腿（`cancel_job_turns_for_chat` 两腿都跑）。
    /// 排队中的 job 被丢弃后不会有任何回合结局，因此同时给同步 waiter 回取消终态
    /// （否则 run_job 要挂到超时）。reply 回被丢弃的条数。
    DropQueuedJobs {
        channel_id: Uuid,
        reply: oneshot::Sender<usize>,
    },
    /// 根频道全量同步（service 每 2s 从 vb 存储扫描）。diff 应用：新增
    /// 注册，消失的根频道排空队列、失效会话、其后的失败批次直接丢弃。
    SyncRoots(Vec<ChannelMeta>),
}

/// 同步 waiter 的回传消息（#309 PR-A1）。区分「正常结局」与「**丢弃式取消**」：
/// 后者表示这一轮被叫停**且批次不会再被重提示**，调用方应静默收尾、别再等文本。
#[derive(Debug, PartialEq, Eq)]
enum SyncWaitMsg {
    /// 回合文本（`Ok`）或终态失败原因（`Err`）——与改造前的 `Result<String,String>` 同义。
    Done(Result<String, String>),
    /// 取消终态：该轮以 `PromptOutcome::Cancelled` 结束，且其批次**已被丢弃、
    /// 不会被重提示**（被 Steer 合并重提示的那种不发这个，等待方等重提示文本）。
    /// 等待方据此静默收尾。
    Cancelled,
}

/// 懒启动 / 崩溃重拉的后台尝试结果。
///
/// 带 `index` = **预约的槽位号**：会话级隔离后池子里有多个槽位、启动是并发的（每个空槽位
/// 一个 attempt），回执必须说明自己是谁，才能放回正确的槽位（`return_agent` 靠 index）。
enum SpawnOutcome {
    Ok {
        index: usize,
        agent: Box<OwnedAgent>,
    },
    Err {
        index: usize,
        detail: String,
    },
}

/// 空闲多久就回收一个**额外** agent（0 号槽位是热实例，永不回收）。
///
/// 为什么按空闲回收、而不是限制并发数（owner 2026-10-04 指出）：会话本来就不常驻 ——
/// 每个会话的 agent 只在它自己跑回合时活着，跑完就闲置。限制并发数会把「会话互不影响」
/// 打折（第 9 个会话又得排在别人后面）；**回收闲置进程**才是真正的资源边界：稳态进程数
/// 就等于「正在跑的会话数」。
const IDLE_REAP_AFTER: Duration = Duration::from_secs(300);

/// 回收巡检间隔（主循环 select 里的一路定时器）。
const REAP_TICK: Duration = Duration::from_secs(60);

/// 需求 → 槽位数（纯函数，单测点）：**不设并发上限** —— 队列里有几个待跑会话就要几个 agent；
/// 空闲时也保留 1 个热实例，避免每条消息都冷启动。
///
/// 资源边界不在这里：① 跑完闲置 IDLE_REAP_AFTER 的额外槽位会被回收（reap_idle_agents）；
/// ② 起 agent 的速度由既有 spawn 守卫把关（spawn_guard：同 key ≥500ms 间隔 + 连续失败熔断，
/// v2.23.89），所以「不限并发」不等于「可能瞬间起一堆」。
fn wanted_slots(pending: usize) -> usize {
    pending.max(1)
}

/// 该不该回收这个槽位（纯函数，单测点）。
///
/// 0 号槽位是热实例（永不回收，保证下一条消息不必冷启动）；其余槽位只有**空闲**超时才收
/// —— `idle_for = None` 表示它在途（被借走在跑回合）或正在启动，绝不能动。
fn should_reap_slot(slot: usize, idle_for: Option<Duration>) -> bool {
    slot > 0 && idle_for.is_some_and(|d| d >= IDLE_REAP_AFTER)
}

/// 回收闲置的额外 agent（保留 0 号热实例）。
///
/// 回收 = 从池里取走 + 优雅关停：agent 进程随 AcpClient 结束而死，job 句柄关闭连带清掉
/// 它起的 MCP 孙进程（与崩溃路径同一套收尸语义）。槽位本身保留 —— 索引不变式不能破，
/// 下次有需求时 ensure_agent_capacity 会把它重新拉起来。
fn reap_idle_agents(l: &mut Loop) {
    let now = Instant::now();
    let mut reaped = 0usize;
    for idx in 1..l.pool.slot_count() {
        let idle_for = l
            .idle_since
            .get(&idx)
            .map(|t| now.saturating_duration_since(*t));
        if !should_reap_slot(idx, idle_for) {
            continue;
        }
        // 空槽位 = 在途/启动中，不动（should_reap_slot 已挡掉在途，这里是防御）。
        let Some(agent) = l.pool.take_slot(idx) else {
            continue;
        };
        l.idle_since.remove(&idx);
        // 登记表同步清理：该槽位被回收后它的 (slot, sid) 条目不再有意义（评审 N1；
        // 不清理不会串台——新会话登记会覆盖——但会永久留陈旧项）。
        l.registry.forget(
            idx,
            agent.state.sessions.values().cloned().collect::<Vec<_>>(),
        );
        reaped += 1;
        tokio::spawn(async move {
            let mut agent = agent;
            agent.acp.shutdown().await;
        });
    }
    if reaped > 0 {
        crate::log!(
            "[acp] 空闲回收：关停 {reaped} 个闲置 agent（保留热实例，槽位总数 {}）",
            l.pool.slot_count()
        );
    }
}

/// 资源事件的最小投递间隔（按 `(channel, server, uri)` 计）。
///
/// 为什么需要（card `abb-mcp-resource-throttle-20261011`，评审实测）：MCP 资源更新会**直接开一个回合**，
/// 同内容靠 `(server,uri,text)` 哈希去重，而**内容一变化就是新回合**。若某 server 在每次工具调用后都更新
/// 同一资源，就形成「回合 → 更新 → 回合」的自激循环（真实模型花费 + 刷屏）；评审在变异 run 里实测
/// 63s 内 11890 条带标记 prompt，确认放大真实存在。
///
/// 取值 10s：IM 场景下"同一资源 10s 内的多次变化"合并成一次提醒几乎无损；低于 5s 则高频资源
/// （日志/状态文件）仍能自激。阈值变更按 `RULE_阈值变更.md` 单独提交并附证据——
/// 本文件的 `resource_event_min_interval_is_pinned` 会把当前值钉住，改动即测试红。
const RESOURCE_EVENT_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// 单个 `(channel, server)` 每窗口最多放行的资源更新条数。
///
/// 为什么需要（评审 F2）：判定键含 uri ⇒「每次事件都换 uri」就能**完全绕开**逐资源窗口
/// （每次都是"首次"）。对这类话痨 server，逐资源节流等于没有节流。本上限把单 server 的
/// 回合数上界从"无限"压到 3 条/窗口（约 0.3 回合/s）。
///
/// 取值 3：一个 server 上常见资源数量很小（模板/记忆/状态各一），3 条足以覆盖正常并发；
/// 阈值变更同样按 `RULE_阈值变更.md` 单独提交（`resource_event_per_server_cap_blocks_uri_rotation` 会红）。
const RESOURCE_EVENT_MAX_PER_SERVER: u32 = 3;

/// 节流日志的**每窗口即时行预算**（超出只计数，由汇总行报告）。
///
/// 为什么需要：丢弃必须可观测（本卡 AC1），但放大速率实测约 190 事件/s——每 key 一条即时行，
/// 在多 key 反复更新时仍能刷爆 `bridge.out`（轮转上限 8 MiB，不限频约 5 分钟就挤掉其他诊断）。
/// 预算 20 行/窗口 + 每窗口至多 1 条汇总 ⇒ 日志上界约 2.1 行/s，**与负载无关**。
const RESOURCE_THROTTLE_LOG_BUDGET: u32 = 20;

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
/// fork `_meta.abbSandbox` 能力协商结果（单后端化 P2.3）。存储为 AtomicU8
/// （判别值见 RuntimeHandle::sandbox_support）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxSupport {
    /// agent 未启动/启动中——能力未知。
    Unknown = 0,
    /// initialize 声明 `_meta.abbSandbox`——认 `_meta.sandbox/shell`。
    Supported = 1,
    /// initialize 未声明——旧 fork/其他适配器，发不出档位。
    Unsupported = 2,
}

/// 生成中「插入（steer）」的能力协商结果（卡 `abb-syschat-d10-insert-interrupt`）。
///
/// 来源 = `AcpClient::steering_supported`（initialize 顶层 `_meta.steering.supported`），
/// 由 `handle_spawn_outcome` **每次成功拉起后**写入 `RuntimeHandle`。调用方（系统对话
/// 的发送判定）据此决定「插入在途回合」还是「排队等本轮结束」——**不得硬编码**：
/// 随包 `crates/abb-agent` 如实报 `false`（未实现 `_session/steering`），此时唯一正确
/// 行为是排队，而不是发一个必然 `-32601` 的 steer。
///
/// `Unknown` = agent 尚未 initialize（含 goose 这类只认 `expectedRunId`、不报该扩展的
/// 适配器：在 run id 出现前能力就是「不明」）⇒ 调用方按「不明 = 排队」处理（卡口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SteerSupport {
    /// agent 未启动/启动中（initialize 前）——能力未知，按排队处理。
    #[default]
    Unknown,
    /// initialize 声明 `_meta.steering.supported = true`——可写 `_session/steering`。
    Supported,
    /// initialize 未声明（或显式 false）——不可 steer，必须排队。
    Unsupported,
}

pub struct RuntimeHandle {
    cfg: AgentConfig,
    stop: CancellationToken,
    ctx: Arc<PromptContext>,
    registry: Arc<Mutex<Registry>>,
    /// agent 进程不可用（bridge 预检读）。
    dead: AtomicBool,
    /// 最近一次 agent **起不来**的原因：启动失败（`spawn`/`initialize`）或运行中崩溃。
    ///
    /// 为什么要留一份：预检只会给用户一句「agent 未就绪（启动失败/崩溃退避中）」，
    /// 真实原因（缺 `abb-spawner.exe`、取不到桌面令牌、被 EDR 拦、供应商初始化失败…）
    /// 此前只写进 `bridge.out`——用户端完全无从下手（2026-09-29 owner 实报
    /// 「Windows 全都不回消息，也没有任何提示」）。启动成功即清空，避免旧原因误导。
    last_start_error: std::sync::Mutex<Option<String>>,
    /// fork `_meta.abbSandbox` 能力位（单后端化 P2.3）：initialize 成功后由
    /// handle_spawn_outcome 写入；新一轮 spawn 发起时复位 Unknown。
    /// 桥侧预检据此对「已知不支持」的 granted 消息提前拒答；**真闸**在
    /// `pool::create_session_and_apply_model`（拒绝创建受限会话，无懒启动竞态）。
    sandbox_supported: std::sync::atomic::AtomicU8,
    /// steer 能力位（本卡）：initialize 成功后由 handle_spawn_outcome 广播；新一轮
    /// spawn 发起时用 `send(Unknown)` 复位。用 watch 而非 Atomic：系统对话侧要
    /// **异步等**这个值从 Unknown 落锤，轮询原子会变成忙等（见 oneshot 的镜像任务）。
    steering: tokio::sync::watch::Sender<SteerSupport>,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    cmd_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Cmd>>>,
    life_tx: mpsc::UnboundedSender<SpawnOutcome>,
    life_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<SpawnOutcome>>>,
    turn_tx: mpsc::UnboundedSender<TurnOutput>,
    /// 同步回合等待表（job/oneshot 路径）：channel_id → 结局回传。Ok = 回合文本
    ///（turn consumer 消费 TurnOutput 时旁路）；Err = 终态失败原因（notify_channel
    /// 死信/失败告示时旁路——否则 agent 错误对同步等待者只表现为「挂到超时」，见 oneshot.rs）。
    sync_waiters: std::sync::Mutex<
        std::collections::HashMap<Uuid, tokio::sync::mpsc::UnboundedSender<SyncWaitMsg>>,
    >,
    /// 流式分片 sink（E1）：channel_id → 分片回传通道。调用方（`oneshot_turn_streaming`）
    /// 在等回合之前装入，回合收尾时移除；未装（非流式回合）时分片被丢弃（行为同接线前）。
    /// 与 `sync_waiters` 同键：一个 in-flight 频道至多一个流式消费者。
    chunk_sinks: std::sync::Mutex<
        std::collections::HashMap<Uuid, tokio::sync::mpsc::UnboundedSender<TurnChunk>>,
    >,
    /// 同步等待者（oneshot 路径）该读的**本回合真实 usage**（E3）：
    /// 主循环在该频道的 `Done` 回执之前落位，`oneshot` 收到回执后取走。
    ///
    /// 为什么不塞进 [`SyncWaitMsg`] / [`SyncTurnOutcome`]：那两个是**结局分类**，
    /// 六个模块按分类匹配；usage 只是系统对话（GUI）要的一个量。这里与
    /// `chunk_sinks` 同款——按频道挂的「本回合边车」，谁要谁取（take 一次即空，
    /// 因此不会把上一回合的数留给下一回合）。
    turn_usage: std::sync::Mutex<std::collections::HashMap<Uuid, TokenUsage>>,
    turn_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<TurnOutput>>>,
}

impl RuntimeHandle {
    /// 测试用：暴露构造该句柄时**实际要下发给 agent 的 env**。
    ///
    /// 为什么需要（评审 R1，卡 `abb-mcp-orchestrator-p0-20261012`）：聊天路径（`build_bot_acp_handles`
    /// 的 `mk` 闭包）的 `$ABB_BIN` 注入此前**无人看守**——只删掉那处 `.chain(abb_bin_env())`，
    /// 全量隔离套件仍全绿。有了这个访问器，测试就能在**真句柄**上断言，而不是只测 helper。
    #[cfg(test)]
    pub(crate) fn extra_env_for_test(&self) -> &[(String, String)] {
        &self.cfg.extra_env
    }
    /// 测试访问器：**该句柄实际会 spawn 的执行层命令**。
    ///
    /// 为什么需要它：执行层按角色 + 档位分流（`service::pick_command_by_sandbox`），
    /// 而「命令槽是否选对」**只有接线测试能钉住**——评审用变异实测过：把
    /// `normal_meta.is_some()` 改成 `false`（等于把「带档位的 owner 会话送去 abb-agent
    /// 被硬闸拒建」这个 bug 放回去），全量门禁依然全绿。
    #[cfg(test)]
    pub(crate) fn agent_command(&self) -> &str {
        &self.cfg.command
    }

    /// 测试访问器：该句柄的 `session_sandbox` 档位载荷（`None` = FullAccess/Auto）。
    #[cfg(test)]
    pub(crate) fn session_sandbox(&self) -> Option<&crate::runtime::acp::SessionSandboxMeta> {
        self.cfg.session_sandbox.as_ref()
    }

    /// 测试访问器：该句柄是否按**渠道**读 MCP 配置（normal = true，granted/oneshot = false）。
    #[cfg(test)]
    pub(crate) fn mcp_from_channel(&self) -> bool {
        self.ctx.mcp_from_channel
    }

    /// 新建句柄。不拉起任何进程——agent 懒启动，首条消息到达才 spawn。
    /// `cwd` = agent 子进程工作目录（ABB 启动时的当前目录）。
    /// `mcp_from_channel` = 该句柄的会话是否按渠道读 MCP 配置
    /// （`<渠道工作目录>/mcp.json`；normal = true，granted/oneshot = false——
    /// 外部 server 的工具面会绕过受限档沙箱，宁可少给能力也不给绕闸路径）。
    pub fn new(
        cfg: AgentConfig,
        stop: CancellationToken,
        cwd: String,
        mcp_from_channel: bool,
    ) -> Arc<Self> {
        // 渠道级 MCP：名单在 `session/new` 前按会话 cwd 现读（`channel_mcp::load`），
        // 这里只带一个开关。ABB 自己不再实现任何 MCP server（原 `abb-events` 已于
        // 2026-10-11 下线，见 card abb-mcp-declarer-only）。
        let ctx = Arc::new(PromptContext {
            mcp_from_channel,
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
        // steer 能力位：初值 Unknown（agent 懒启动，首条消息到达才 spawn）。
        let (steering_tx, _steering_rx) = tokio::sync::watch::channel(SteerSupport::Unknown);
        Arc::new(Self {
            cfg,
            stop,
            ctx,
            registry: Arc::new(Mutex::new(Registry::default())),
            dead: AtomicBool::new(false),
            last_start_error: std::sync::Mutex::new(None),
            sandbox_supported: std::sync::atomic::AtomicU8::new(SandboxSupport::Unknown as u8),
            steering: steering_tx,
            cmd_tx,
            cmd_rx: std::sync::Mutex::new(Some(cmd_rx)),
            life_tx,
            life_rx: std::sync::Mutex::new(Some(life_rx)),
            turn_tx,
            turn_rx: std::sync::Mutex::new(Some(turn_rx)),
            sync_waiters: std::sync::Mutex::new(std::collections::HashMap::new()),
            chunk_sinks: std::sync::Mutex::new(std::collections::HashMap::new()),
            turn_usage: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// 同步回合等待（job / oneshot 路径）：注册等待者后 push 消息并等终态，
    /// 结局五分——Ok(文本) / Timeout（调用方预算先到） / Closed（句柄已关闭，
    /// push 不进） / Failed(agent 终态失败原因，由 notify_channel 旁路) /
    /// Cancelled（取消终态且批次已丢弃，见 `SyncWaitMsg::Cancelled`）。
    pub async fn wait_turn_outcome(
        &self,
        channel_id: Uuid,
        msg: InboundMsg,
        timeout: std::time::Duration,
    ) -> SyncTurnOutcome {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        self.sync_waiters.lock().unwrap().insert(channel_id, tx);
        if !self.push_message(channel_id, msg) {
            self.sync_waiters.lock().unwrap().remove(&channel_id);
            return SyncTurnOutcome::Closed;
        }
        let outcome = match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(SyncWaitMsg::Done(Ok(text)))) => SyncTurnOutcome::Ok(text),
            Ok(Some(SyncWaitMsg::Done(Err(reason)))) => SyncTurnOutcome::Failed(reason),
            // 丢弃式取消：#309 起不再等重提示/超时，直接给调用方终态。
            Ok(Some(SyncWaitMsg::Cancelled)) => SyncTurnOutcome::Cancelled,
            // Ok(None) = 回传端意外消失（同 channel_id 并发两次等待时后者
            // 顶替前者的 tx；见 SyncTurnOutcome::Closed 文档），按关闭论。
            Ok(None) => SyncTurnOutcome::Closed,
            Err(_) => SyncTurnOutcome::Timeout,
        };
        self.sync_waiters.lock().unwrap().remove(&channel_id);
        outcome
    }

    /// agent 进程是否可用（桥侧预检读）。false = 启动/重拉失败退避中，新消息
    /// 会被预检拒绝（避免用户以为已受理）。
    pub fn is_agent_available(&self) -> bool {
        !self.dead.load(Ordering::Relaxed)
    }

    /// 最近一次启动失败的原因（给用户看的；无失败过则为 None）。
    pub fn last_start_error(&self) -> Option<String> {
        self.last_start_error.lock().ok().and_then(|g| g.clone())
    }

    /// 记录/清除「最近一次启动失败原因」（内核入口：spawn 结局两条臂）。
    fn set_last_start_error(&self, detail: Option<String>) {
        if let Ok(mut g) = self.last_start_error.lock() {
            *g = detail;
        }
    }

    /// fork `_meta.abbSandbox` 能力位（P2.3）。桥侧预检只在 **Unsupported**
    ///（agent 已起、initialize 未声明）时提前拒答；Unknown（懒启动未起）放行——
    /// 真闸在 session 创建处（pool），否则首条 granted 消息必被误拒。
    pub fn sandbox_support(&self) -> SandboxSupport {
        match self.sandbox_supported.load(Ordering::Relaxed) {
            1 => SandboxSupport::Supported,
            2 => SandboxSupport::Unsupported,
            _ => SandboxSupport::Unknown,
        }
    }

    /// steer 能力位（本卡）：给系统对话的发送判定用。`Unknown` = initialize 前。
    ///
    /// 与 [`sandbox_support`](Self::sandbox_support) 同款：**每次成功拉起后重判**，
    /// 新一轮 spawn 发起时复位 Unknown。watch 接收端可 `changed().await` 异步等落锤。
    pub fn steering_watch(&self) -> tokio::sync::watch::Receiver<SteerSupport> {
        self.steering.subscribe()
    }

    /// 入队一条用户消息（桥 dispatch 后调用）。channel 在跑时按 Queue 语义
    /// 排队并触发 steer（模块文档）。返回 `false` = 句柄已关闭。
    pub fn push_message(&self, channel_id: Uuid, msg: InboundMsg) -> bool {
        self.cmd_tx.send(Cmd::Message { channel_id, msg }).is_ok()
    }

    /// 移除同步等待者（P3.2 oneshot 外部取消臂）：`wait_turn_outcome` 的 future
    /// 被外部取消 select 提前 drop 时，其尾部 remove 不再执行——调用方须显式
    /// 清表防条目泄漏（迟到的回合文本/死信 send 进已 drop 的 rx 静默丢弃）。
    pub fn remove_sync_waiter(&self, channel_id: Uuid) {
        self.sync_waiters.lock().unwrap().remove(&channel_id);
    }

    /// 装入/替换某频道的流式分片 sink（E1）。调用方在等回合前装入；
    /// 未装时 `send_turn_chunk` 静默丢弃（非流式回合的常态）。
    pub fn set_turn_chunk_sink(
        &self,
        channel_id: Uuid,
        tx: tokio::sync::mpsc::UnboundedSender<TurnChunk>,
    ) {
        self.chunk_sinks.lock().unwrap().insert(channel_id, tx);
    }

    /// 移除某频道的流式 sink（回合结束/取消后收尾；不摘掉会让已 drop 的
    /// 接收端对应的 sender 一直占表，迟发分片也无人消费）。
    pub fn remove_turn_chunk_sink(&self, channel_id: Uuid) {
        self.chunk_sinks.lock().unwrap().remove(&channel_id);
    }

    /// 取走某频道本回合上报的 usage（E3；oneshot 在收到 `Done` 回执后调用）。
    ///
    /// take 语义：取一次就空——同一频道上后一个回合没上报 usage 时，调用方
    /// 必须看到 `None`，而不是上一个回合的旧数字。
    pub fn take_turn_usage(&self, channel_id: Uuid) -> Option<TokenUsage> {
        self.turn_usage.lock().unwrap().remove(&channel_id)
    }

    /// 把一条流式分片投给该频道的 sink（未装/接收端已 drop 则丢弃）。
    /// 主循环的 [`handle_turn_chunk`] 路由到这里。
    fn send_turn_chunk(&self, channel_id: Uuid, chunk: TurnChunk) {
        let g = self.chunk_sinks.lock().unwrap();
        if let Some(tx) = g.get(&channel_id) {
            let _ = tx.send(chunk);
        }
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

    /// **丢弃排队中的定时任务消息**（#309 PR-B）：返回被丢弃条数；`None` = 句柄已关闭。
    /// 只影响 `prompt_tag == crate::schedule::JOB_PROMPT_TAG` 的排队事件，用户消息与
    /// 在跑轮次不受影响。
    pub async fn drop_queued_jobs(&self, channel_id: Uuid) -> Option<usize> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::DropQueuedJobs {
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
    /// 正在启动的槽位号（每个空槽位最多一个 attempt 在跑 —— 单槽位时代的 `bool` 在多槽位下
    /// 会把并发启动压成串行，等于没隔离）。
    spawning: std::collections::BTreeSet<usize>,
    /// 各槽位「闲置起点」：agent 回到池子时记，被借走时删。缺项 = 在途/启动中。
    /// 空闲回收（reap_idle_agents）据此判断谁能收。
    idle_since: std::collections::HashMap<usize, Instant>,
    /// 日志活跃度。
    last_activity: Instant,
    /// 收到过取消请求、但结局尚未到达的频道（#309）：取消作用于**这一轮**，
    /// 而结局可能是 Cancelled / CancelDrainTimeout / AgentExited / Error / Timeout
    /// 中的任意一种——所以不能只在 Cancelled 分支处理，必须在该轮真实结局处统一消费。
    cancel_requested: HashSet<Uuid>,
    /// 会话登记表（session_id → channel_id）：由 `run_prompt_task` 在**建会话成功处**写入，
    /// 供 agent 主动上报的事件反查归属频道（回合进行中 agent 不在池里，池查不到）。
    registry: crate::runtime::pool::SessionRegistry,
    /// agent 主动上报的事件（MCP 资源更新）：接收端在本循环 select 里，发送端按槽位挂到
    /// 该槽位的 `AcpClient`（每次新拉起都重挂，见 `handle_spawn_outcome`）。
    resource_rx: mpsc::UnboundedReceiver<(usize, ResourceUpdate)>,
    /// 同上：留给"给新拉起的 agent 挂 sink"用的发送端克隆。
    /// 发送端带 **agent 槽位**：执行层会话号是进程内的，必须连同 agent 身份一起上报，
    /// 否则多 agent 并发时归属会串（评审 F1）。
    resource_tx: mpsc::UnboundedSender<(usize, ResourceUpdate)>,
    /// 流式正文分片（E1）：接收端在本循环 select 里，发送端按槽位挂到该槽位的 `AcpClient`
    /// （与 resource 同款：每次新拉起都重挂，respawn 后不会静默断流）。
    chunk_rx: mpsc::UnboundedReceiver<(usize, TurnChunk)>,
    chunk_tx: mpsc::UnboundedSender<(usize, TurnChunk)>,
    /// 资源事件节流器（判定 + 记账 + 限频日志，见 [`ResourceThrottle`]）。
    ///
    /// 窗口恒为 [`RESOURCE_EVENT_MIN_INTERVAL`]（**不提供测试注入**：评审 F4 指出原字段从无
    /// 赋值点，而阳性对照用例就是靠真等一个窗口来证明窗口真的生效——注入点会让那条证明失真）。
    resource_throttle: ResourceThrottle,
}

impl Loop {
    fn new(ctx: Arc<PromptContext>) -> (Self, mpsc::UnboundedReceiver<SteerAckEvent>) {
        // 懒池：起手 1 个槽位（热 agent），**队列里有更多待跑会话时按需追加**（会话级隔离）。
        // from_slots 保留索引不变式（slot i ↔ index i）。
        let pool = AgentPool::from_slots(vec![None]);
        let queue =
            EventQueue::new(DedupMode::Queue).with_in_flight_deadline(MAX_TURN_DURATION.as_secs());
        let (steer_ack_tx, steer_ack_rx) = mpsc::unbounded_channel::<SteerAckEvent>();
        // agent 主动上报的事件通道（MCP 资源更新）：接收端进本循环 select，发送端挂到各槽位的 AcpClient。
        let (resource_tx, resource_rx) =
            mpsc::unbounded_channel::<(usize, crate::runtime::acp::ResourceUpdate)>();
        // 流式正文分片通道（E1）：与 resource 同款，槽位随事件上报。
        let (chunk_tx, chunk_rx) = mpsc::unbounded_channel::<(usize, TurnChunk)>();
        let l = Self {
            ctx,
            pool,
            queue,
            steer_ack_tx,
            removed_channels: HashSet::new(),
            crash_backoff: 0,
            spawning: std::collections::BTreeSet::new(),
            idle_since: std::collections::HashMap::new(),
            last_activity: Instant::now(),
            cancel_requested: HashSet::new(),
            registry: crate::runtime::pool::SessionRegistry::default(),
            resource_rx,
            resource_tx,
            chunk_rx,
            chunk_tx,
            resource_throttle: ResourceThrottle::new(Instant::now()),
        };
        (l, steer_ack_rx)
    }
}

/// harness 主循环任务体（service 以 spawn_forever 驱动；退出 = 关停 token 触发
/// 后的优雅收尾完成）。
pub async fn run_loop(handle: Arc<RuntimeHandle>) {
    let mut cmd_rx = handle
        .take_cmd_rx()
        .expect("run_loop must own the cmd receiver (call once)");
    let mut life_rx = handle
        .take_life_rx()
        .expect("run_loop must own the spawn-outcome receiver (call once)");
    let (mut l, mut steer_ack_rx) = Loop::new(handle.ctx.clone());

    tracing::info!(
        "runtime started (lazy pool, agent command = {})",
        handle.cfg.command
    );

    // 空闲回收节拍（会话级隔离的资源边界：回收闲置 agent，而不是限制并发数）。
    let mut reap_tick = tokio::time::interval(REAP_TICK);
    loop {
        // 事件收集：select 内借用 pool 的 rx/join_set，出块即归还，处理器再拿
        // &mut pool（上游同款 dance）。
        let evt: Option<Evt> = {
            let (result_rx, join_set) = l.pool.rx_and_join_set();
            tokio::select! {
                biased;
                _ = handle.stop.cancelled() => break,
                _ = reap_tick.tick() => Some(Evt::Reap),
                cmd = cmd_rx.recv() => {
                    // None = 全部句柄已 drop（本循环持 Arc，正常不会发生）。
                    cmd.map(Evt::Cmd)
                }
                r = result_rx.recv() => r.map(|pr| Evt::Result(Box::new(pr))),
                j = join_set.join_next(), if !join_set.is_empty() => j.map(Evt::Panic),                a = steer_ack_rx.recv() => a.map(Evt::SteerAck),
                life = life_rx.recv() => life.map(Evt::Life),
                r = l.resource_rx.recv() => r.map(|(i, u)| Evt::Resource(i, u)),
                c = l.chunk_rx.recv() => c.map(|(i, c)| Evt::Chunk(i, c)),
            }
        };
        let Some(evt) = evt else {
            tracing::error!("runtime event channel closed — exiting loop");
            break;
        };
        match evt {
            Evt::Cmd(cmd) => {
                // 返回值只对资源事件节流有意义（见 handle_cmd 文档），主循环忽略。
                let _ = handle_cmd(&mut l, &handle, cmd);
            }
            Evt::Result(result) => handle_prompt_result(&mut l, &handle, *result),
            Evt::Panic(join_error) => recover_panicked_agent(&mut l, &handle, join_error),
            Evt::SteerAck(ack) => handle_steer_ack(&mut l, ack),
            Evt::Life(outcome) => handle_spawn_outcome(&mut l, &handle, outcome),
            Evt::Resource(index, update) => handle_resource_update(&mut l, &handle, index, update),
            Evt::Chunk(index, chunk) => handle_turn_chunk(&l, &handle, index, chunk),
            Evt::Reap => reap_idle_agents(&mut l),
        }
        dispatch_pending(&mut l);
    }

    shutdown(&mut l).await;
    tracing::info!("runtime stopped");
}

enum Evt {
    Cmd(Cmd),
    Result(Box<PromptResult>),
    /// join_set 任务结束：`Err` = panic；`Ok(())` = 正常提前返回（结果随后走
    /// result_rx，属良性竞态，忽略）。
    Panic(Result<(), tokio::task::JoinError>),
    SteerAck(SteerAckEvent),
    Life(SpawnOutcome),
    /// 回收巡检节拍：把闲置的额外 agent 收掉（保留 0 号热实例）。
    Reap,
    Resource(usize, ResourceUpdate),
    /// agent 在途回合上报的正文/思考分片（E1）：槽位号 + 分片本体。
    Chunk(usize, TurnChunk),
}

// ── Cmd 处理 ─────────────────────────────────────────────────────────────────

/// 处理一条命令。返回值表示**消息是否真的进了队列**（仅 `Cmd::Message` 有语义，其余恒 `true`）：
/// 资源事件节流要靠它决定是否记账（评审 F3），故不能再无条件返回 `()`。
fn handle_cmd(l: &mut Loop, handle: &RuntimeHandle, cmd: Cmd) -> bool {
    match cmd {
        Cmd::Message { channel_id, msg } => {
            let accepted = l.queue.push(QueuedEvent {
                channel_id,
                msg: msg.clone(),
                received_at: Instant::now(),
            });
            if !accepted {
                // 同上：本仓生产进程没有 tracing subscriber，这里也走 stdout。
                crate::log!("[queue] 消息被队列策略丢弃（channel={channel_id}），未投递");
                return false;
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
            // 会话级隔离：按「队列里有多少待跑会话」保证槽位与热 agent（上限内）。
            ensure_agent_capacity(l, handle);
            true // 已入队（资源事件节流据此记账，见 F3）
        }
        Cmd::Cancel { channel_id, reply } => {
            let fired = signal_in_flight_task(&mut l.pool, channel_id, ControlSignal::Cancel);
            // 只登记「这一轮收到过取消请求」——**不在这里**通知 waiter：`fired` 只表示
            // 信号已送达，不代表最终结局是取消（回合可能已自然完成、结果正排在结果
            // 分支里；run_loop 是 biased）。提前报取消会让调用方收到 Cancelled，而那
            // 次自然完成的文本照样进聊天。
            //
            // 终态统一在 handle_prompt_result 里按**真实结局**判定并消费（见
            // `cancel_waiter_terminal`）——覆盖 Cancelled / CancelDrainTimeout /
            // AgentExited / Error / Timeout 全部结局，不会漏成「挂到 timeout」。
            if fired {
                l.cancel_requested.insert(channel_id);
            }
            let _ = reply.send(fired);
            true
        }
        Cmd::DropQueuedJobs { channel_id, reply } => {
            let dropped = l
                .queue
                .drain_channel_tagged(channel_id, crate::schedule::JOB_PROMPT_TAG);
            if !dropped.is_empty() {
                // 被丢弃的排队 job 不会再产生回合结局 → 主动给等待者取消终态，
                // 让 run_job 立刻静默收尾（而不是挂到超时）。
                let _ = deliver_sync_wait_msg(handle, channel_id, SyncWaitMsg::Cancelled);
            }
            let _ = reply.send(dropped.len());
            true
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
                let uuid = Uuid::parse_str(&crate::runtime::keys::channel_uuid(
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
            // adhoc 频道（p2p 私聊 / 自动登记的群）豁免——它们不在登记表，
            // 巡检「不在 next」是常态而非消失，误清会丢私聊/未登记群回复。
            for (key, uuid) in current {
                let is_adhoc = handle
                    .registry
                    .lock()
                    .unwrap()
                    .channels
                    .get(&uuid)
                    .map(|m| m.adhoc)
                    .unwrap_or(false);
                if is_adhoc {
                    continue;
                }
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
                // 评审 F2：频道没了，它的节流状态也一并清掉（否则残留到 GC 下限期，
                // 且 uuid 若被复用会把新频道的首次更新误判成"窗口内"）。
                l.resource_throttle.forget_channel(uuid);
            }
            true
        }
    }
}

// ── agent 生命周期 ───────────────────────────────────────────────────────────

/// agent 上报的资源更新 → 该会话频道队列里的**一条入站消息**。
///
/// 刻意走与用户消息**完全相同**的入队路径（`Cmd::Message`）：去重、排队、steer、空闲墙、
/// 交付全部由既有机制统一管——不新开第二条回合通道（这正是"交付单点归 abb"的落地方式）。
/// 节流判定（纯函数，便于单测）：距上次投递不足 `min` 则**丢弃**。
///
/// 判定键是 `(channel, server, uri)`：同一资源的连续变化在窗口内合并；不同资源互不影响。
///
/// 语义：**"过期" 与 "不存在" 等价**（都放行）⇒ 定期 `retain` 过期条目不改变任何行为，只把状态变有界。
fn resource_event_allowed(last: Option<Instant>, now: Instant, min: Duration) -> bool {
    match last {
        Some(prev) => now.saturating_duration_since(prev) >= min,
        None => true,
    }
}

/// 节流判定结果（日志文案与测试都要能区分"为什么被丢"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceDecision {
    Allow,
    /// 同一 `(channel, server, uri)` 在窗口内已有过投递。
    DropByKey,
    /// 同一 `(channel, server)` 本窗口已放行 [`RESOURCE_EVENT_MAX_PER_SERVER`] 条（防 uri 轮换绕过）。
    DropByServer,
}

/// 资源事件节流器：状态 + 判定 + **日志行生成**（纯内存，不直接写 stdout，便于单测）。
///
/// 为什么把日志也放进来（评审 F1）：原实现用 `tracing::warn!`，而根二进制**没有安装 tracing
/// subscriber**（`tracing` 只是空壳；同因注释见 `emit_agent_start_failure` 与 `respawn_log_line`）
/// ⇒ 丢弃路径实际是**静默**的，
/// 与卡上 AC1「不得静默」不符。现在改为产出字符串、由生产侧用 [`crate::log!`] 写 stdout
/// （与全仓约定一致），并且**限频**（即时行预算 + 每窗口一条汇总），既不静默也不刷爆日志。
pub(crate) struct ResourceThrottle {
    /// `(channel, server, uri)` → 上次**成功入队**时刻。F3：只有真入队才记账
    /// （否则"被队列策略拒"的事件会白占一个窗口，把下一条真实更新误杀）。
    last_sent: HashMap<(Uuid, String, String), Instant>,
    /// `(channel, server)` → (窗口起点, 本窗口已放行条数)：防 uri 轮换绕过（F2）。
    per_server: HashMap<(Uuid, String), (Instant, u32)>,
    /// 本窗口内按 key 的丢弃计数（汇总行的明细，每窗口清空 ⇒ 有界）。
    dropped_detail: HashMap<(Uuid, String, String), u32>,
    /// 距上次汇总已放行/已丢弃条数（放行数只在 [`Self::mark_delivered`] 累加 = 真入队才算）。
    allowed_since_summary: u32,
    dropped_since_summary: u32,
    /// 本窗口内"找不到对应频道"的孤儿更新条数（同样会静默，故纳入汇总）。
    orphans_since_summary: u32,
    /// 本窗口即时行已用预算与因预算被压掉的条数。
    log_lines_used: u32,
    logs_suppressed: u32,
    last_summary: Instant,
    last_gc: Instant,
}

impl ResourceThrottle {
    fn new(now: Instant) -> Self {
        Self {
            last_sent: HashMap::new(),
            per_server: HashMap::new(),
            dropped_detail: HashMap::new(),
            allowed_since_summary: 0,
            dropped_since_summary: 0,
            orphans_since_summary: 0,
            log_lines_used: 0,
            logs_suppressed: 0,
            last_summary: now,
            last_gc: now,
        }
    }

    /// 过期清理（评审 F2：原实现只 insert 从不删 ⇒ 长驻服务数天即百 MB 级，且 uri 每次都变时
    /// 表随事件数无界增长）。因为"过期 == 不存在"语义等价，清理**不改变任何判定结果**。
    fn gc(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_gc) < RESOURCE_EVENT_MIN_INTERVAL {
            return;
        }
        self.last_gc = now;
        let min = RESOURCE_EVENT_MIN_INTERVAL;
        self.last_sent
            .retain(|_, t| now.saturating_duration_since(*t) < min);
        self.per_server
            .retain(|_, (start, _)| now.saturating_duration_since(*start) < min);
    }

    /// 频道被移除时清掉它的全部状态（评审 F2 的后半）：否则该频道的键会残留到 GC 之下限期间，
    /// 且（若 uuid 被复用）可能把新频道的首次更新误判为"窗口内"。
    fn forget_channel(&mut self, channel: Uuid) {
        self.last_sent.retain(|(ch, _, _), _| *ch != channel);
        self.per_server.retain(|(ch, _), _| *ch != channel);
        self.dropped_detail.retain(|(ch, _, _), _| *ch != channel);
    }

    /// 判定一条事件（**不记账**；记账见 [`Self::mark_delivered`]），返回决定与要写的日志行（0..=2）。
    fn on_event(
        &mut self,
        channel: Uuid,
        server: &str,
        uri: &str,
        now: Instant,
    ) -> (ResourceDecision, Vec<String>) {
        let mut lines = Vec::new();
        self.gc(now);
        if let Some(line) = self.flush_summary(now) {
            lines.push(line);
        }
        let key = (channel, server.to_string(), uri.to_string());
        let by_key = resource_event_allowed(
            self.last_sent.get(&key).copied(),
            now,
            RESOURCE_EVENT_MIN_INTERVAL,
        );
        let srv = (channel, server.to_string());
        let (win_start, count) = self.per_server.get(&srv).copied().unwrap_or((now, 0));
        let in_same_window = now.saturating_duration_since(win_start) < RESOURCE_EVENT_MIN_INTERVAL;
        let by_server = !in_same_window || count < RESOURCE_EVENT_MAX_PER_SERVER;
        if by_key && by_server {
            return (ResourceDecision::Allow, lines);
        }
        let decision = if !by_key {
            ResourceDecision::DropByKey
        } else {
            ResourceDecision::DropByServer
        };
        self.dropped_since_summary += 1;
        *self.dropped_detail.entry(key).or_insert(0) += 1;
        if let Some(line) = self.drop_log_line(channel, server, uri, decision, count) {
            lines.push(line);
        }
        (decision, lines)
    }

    /// "资源更新找不到对应频道"（会话/频道已失效）——**同样不得静默**（评审 F1 的同一条规则：
    /// 本函数原本也走 `tracing::warn!`，在生产里等于没有）。受同一日志预算约束，防话痨 server 刷屏。
    fn on_orphan(&mut self, now: Instant, server: &str, uri: &str) -> Vec<String> {
        let mut lines = Vec::new();
        self.gc(now);
        if let Some(line) = self.flush_summary(now) {
            lines.push(line);
        }
        self.dropped_since_summary += 1;
        self.orphans_since_summary += 1;
        if self.log_lines_used >= RESOURCE_THROTTLE_LOG_BUDGET {
            self.logs_suppressed += 1;
            return lines;
        }
        self.log_lines_used += 1;
        lines.push(format!(
            "[resource] 丢弃资源更新：找不到对应频道（会话已失效）server={server} uri={uri}"
        ));
        lines
    }

    /// 一条事件**真的进了回合队列**后调用：这才是窗口的起点（F3）。
    fn mark_delivered(&mut self, channel: Uuid, server: &str, uri: &str, now: Instant) {
        self.allowed_since_summary += 1;
        self.last_sent
            .insert((channel, server.to_string(), uri.to_string()), now);
        let srv = (channel, server.to_string());
        let (win_start, count) = self.per_server.get(&srv).copied().unwrap_or((now, 0));
        let in_same_window = now.saturating_duration_since(win_start) < RESOURCE_EVENT_MIN_INTERVAL;
        self.per_server.insert(
            srv,
            if in_same_window {
                (win_start, count + 1)
            } else {
                (now, 1)
            },
        );
    }

    /// 即时丢弃行（受每窗口预算约束；超预算只计数，由汇总行报告）。
    fn drop_log_line(
        &mut self,
        channel: Uuid,
        server: &str,
        uri: &str,
        decision: ResourceDecision,
        allowed_in_window: u32,
    ) -> Option<String> {
        if self.log_lines_used >= RESOURCE_THROTTLE_LOG_BUDGET {
            self.logs_suppressed += 1;
            return None;
        }
        self.log_lines_used += 1;
        let line = format!(
            "[resource] 节流丢弃（{}s 窗口）：channel={} server={} uri={} 原因={}（同 server 本窗口已放行 {} 条，上限 {}）",
            RESOURCE_EVENT_MIN_INTERVAL.as_secs(),
            channel,
            server,
            uri,
            match decision {
                ResourceDecision::DropByKey => "同 (channel,server,uri) 窗口内已投递",
                ResourceDecision::DropByServer => "同 (channel,server) 窗口内放行数达上限",
                ResourceDecision::Allow => "（不应出现）",
            },
            allowed_in_window,
            RESOURCE_EVENT_MAX_PER_SERVER
        );
        Some(line)
    }

    /// 每窗口至多一条汇总（**与负载无关的日志上界**）；有丢弃时必出，故不静默。
    fn flush_summary(&mut self, now: Instant) -> Option<String> {
        if now.saturating_duration_since(self.last_summary) < RESOURCE_EVENT_MIN_INTERVAL {
            return None;
        }
        self.last_summary = now;
        let allowed = std::mem::take(&mut self.allowed_since_summary);
        let dropped = std::mem::take(&mut self.dropped_since_summary);
        let suppressed = std::mem::take(&mut self.logs_suppressed);
        let orphans = std::mem::take(&mut self.orphans_since_summary);
        self.log_lines_used = 0;
        let detail: Vec<_> = self.dropped_detail.drain().collect();
        if allowed == 0 && dropped == 0 && suppressed == 0 && orphans == 0 {
            return None;
        }
        // 按丢弃条数降序（Reverse 让 sort_by_key 得到降序）
        let mut top: Vec<((Uuid, String, String), u32)> = detail;
        top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

        let top: Vec<String> = top
            .iter()
            .take(3)
            .map(|((_, server, uri), n)| format!("{server}/{uri}×{n}"))
            .collect();
        let line = format!(
            "[resource] 资源事件窗口汇总（{}s）：放行 {} 条回合，丢弃 {} 条{}；丢弃最多的键（前 3）：{}",
            RESOURCE_EVENT_MIN_INTERVAL.as_secs(),
            allowed,
            dropped,
            if suppressed > 0 {
                format!("（另有 {suppressed} 条丢弃因日志预算未逐条打印）")
            } else {
                String::new()
            },
            if top.is_empty() {
                "（无）".to_string()
            } else {
                top.join("、")
            }
        );
        Some(if orphans > 0 {
            format!("{line}；另有 {orphans} 条更新因找不到频道被丢弃")
        } else {
            line
        })
    }
}

/// 资源事件日志的**唯一出口**（丢弃 / 窗口汇总 / 孤儿更新都经此）：写 stdout。
///
/// 为什么单独抽出来（评审 O2）：生产可见路径是 `crate::log!`（stdout），而根二进制**没有** tracing
/// subscriber——`tracing::*` 在线上等于没有日志。把出口收敛成一个薄函数后，"接线"就能被一条**有边界**的
/// 源码守卫钉住（见 `resource_event_logs_wire_to_stdout_not_tracing`），而不是只扫 `handle_resource_update`
/// 的词法范围（那样把 tracing 藏进函数体外定义的 helper 就绕过了）。
fn log_resource_lines(lines: &[String]) {
    for line in lines {
        crate::runtime::log_out::resource_line(line);
    }
}

/// 流式正文分片 → 频道：按 `(槽位, session_id)` 反查频道，再投给该频道的 sink。
///
/// 反查不到时静默丢弃（会话已失效 / 取消后被重拉的迟到分片）——这里不能像资源事件那样
/// 落成入站消息：分片不是独立回合语义，只是当前回合的实时投影。
fn handle_turn_chunk(l: &Loop, handle: &Arc<RuntimeHandle>, agent_index: usize, chunk: TurnChunk) {
    let Some(channel_id) = l.registry.channel_for(agent_index, &chunk.session_id) else {
        return;
    };
    handle.send_turn_chunk(channel_id, chunk);
}

fn handle_resource_update(
    l: &mut Loop,
    handle: &Arc<RuntimeHandle>,
    agent_index: usize,
    update: ResourceUpdate,
) {
    let Some(channel_id) = l.registry.channel_for(agent_index, &update.session_id) else {
        // 不得退回 tracing（生产静默，评审 F1）；本路径与丢弃共用日志预算，防刷屏。
        log_resource_lines(&l.resource_throttle.on_orphan(
            Instant::now(),
            &update.server,
            &update.uri,
        ));
        return;
    };
    // 节流：同一 `(channel, server, uri)` 在窗口内只放行一次（内容变了也算同一资源，避免自激循环），
    // 且同一 `(channel, server)` 每窗口至多 `RESOURCE_EVENT_MAX_PER_SERVER` 条（防 uri 轮换绕过）。
    let now = Instant::now();
    let (decision, lines) =
        l.resource_throttle
            .on_event(channel_id, &update.server, &update.uri, now);
    // 丢弃与汇总日志一律走**唯一出口** `log_resource_lines`（内部 `crate::log!` ⇒ stdout）。
    // 为什么收敛成一个出口：`crate::log!` 才是生产可见路径（根二进制没有 tracing subscriber），
    // 而"接线"没法用行为判据钉住（`Loop` 需要真 handle），故把它做成**一个可被源码守卫覆盖的薄函数**
    // ——守卫同时断言"handler 不含 tracing、且经该出口"与"出口本身用 crate::log!"，
    // 这样"在 handler 里加个 helper 偷偷用 tracing"这种绕过（评审 O2）也会红。
    log_resource_lines(&lines);
    if decision != ResourceDecision::Allow {
        return;
    }
    let msg = resource_update_message(&update);
    enqueue_and_arm(l, handle, channel_id, &update, msg, now);
}

/// **单点接线**：把资源更新投进回合队列，并且**只在真的入队之后**才占用节流窗口
/// （评审 F3：队列策略拒收时不记账，否则一条没投出去的更新会把随后一条真实更新误杀 10s）。
///
/// 为什么抽成一个函数：这条"入队才记账"是不变式，散在调用点里时**没有任何判据**能钉住它
/// （变异"把记账挪到 if 之外"曾让 14 条用例全绿，见卡 `abb-resource-throttle-observations-20261011`）。
/// 收成单点后，`resource_event_logs_wire_to_stdout_not_tracing` 只需断言"这里有一行
/// `if accepted { … mark_delivered(…) }`"，把调用点挪出判据即红；调用点也被断言不得绕过它。
fn enqueue_and_arm(
    l: &mut Loop,
    handle: &RuntimeHandle,
    channel_id: Uuid,
    update: &ResourceUpdate,
    msg: InboundMsg,
    now: Instant,
) {
    let accepted = handle_cmd(l, handle, Cmd::Message { channel_id, msg });
    if accepted {
        l.resource_throttle
            .mark_delivered(channel_id, &update.server, &update.uri, now);
    }
}

/// 资源更新 → `InboundMsg`（纯函数，便于单测钉形状）。
///
/// - `author_role = "event"`：事件块里如实标注"不是人说的"；
/// - `prompt_tag = "mcp:resource"`：日志与排障可追；
/// - `id_hex` = `(server, uri, text)` 的内容哈希：**给排障定位用**（日志/steer 认这个 id）。
///   **不要**把它读成"队列会按 id 去重"——`EventQueue::push` 只看 `DedupMode`（本仓**显式**取 `Queue`；
///   注意 `DedupMode` 自身的 `#[default]` 是 `Drop`，只在飞时丢弃），**不比较 id**（2026-10-12 评审 O3 更正）；重复到达的同内容
///   通知是靠 `(channel, server, uri)` 的 10s 节流窗口合并的。
///   内容变了则是新消息（避免把真正的变化当重复丢掉）。
fn resource_update_message(update: &ResourceUpdate) -> InboundMsg {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    update.server.hash(&mut hasher);
    update.uri.hash(&mut hasher);
    update.text.hash(&mut hasher);
    InboundMsg {
        id_hex: format!("mcp-{:016x}", hasher.finish()),
        author_role: "event".to_string(),
        text: format!(
            "[MCP 资源更新] server={} uri={}\n\n{}",
            update.server, update.uri, update.text
        ),
        ts_secs: chrono::Utc::now().timestamp(),
        prompt_tag: "mcp:resource".to_string(),
        bridge_generation: String::new(),
    }
}

fn handle_spawn_outcome(l: &mut Loop, handle: &RuntimeHandle, outcome: SpawnOutcome) {
    let index = match &outcome {
        SpawnOutcome::Ok { index, .. } | SpawnOutcome::Err { index, .. } => *index,
    };
    l.spawning.remove(&index);
    match outcome {
        SpawnOutcome::Ok { index: _, agent } => {
            let agent = *agent;
            l.crash_backoff = 0;
            handle.dead.store(false, Ordering::Relaxed);
            // P2.3：能力位随本次 initialize 落锤（每次成功拉起都重判——同进程
            // 重启后 exe 可能已被换装）。
            handle.sandbox_supported.store(
                if agent.acp.abb_sandbox_supported() {
                    SandboxSupport::Supported as u8
                } else {
                    SandboxSupport::Unsupported as u8
                },
                Ordering::Relaxed,
            );
            // 本卡：steer 能力位同款落锤（initialize 声明 `_meta.steering.supported`）。
            // 语义边界：goose 的 `_goose/unstable/session/steer` 只认 `expectedRunId`、
            // 不报这个扩展 ⇒ 这里记为 Unsupported；系统对话据此排队（「能力不明/不支持
            // 都走排队」是卡定口径，不为了 goose 的 run-id 通道放宽闸门）。
            let _ = handle.steering.send(if agent.acp.steering_supported() {
                SteerSupport::Supported
            } else {
                SteerSupport::Unsupported
            });
            l.idle_since.insert(agent.index, Instant::now());
            let mut agent = agent;
            // 事件路径随槽位走：新拉起的 agent 必须重挂 sink（旧进程的 AcpClient 已 drop）。
            // 这一层把 **agent 槽位** 贴到每条事件上（AcpClient 自己不知道槽位号），
            // 归属键因此是 (槽位, session_id)——多 agent 并发时不会串（评审 F1）。
            let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<ResourceUpdate>();
            let tagged_tx = l.resource_tx.clone();
            let slot = agent.index;
            tokio::spawn(async move {
                while let Some(update) = raw_rx.recv().await {
                    if tagged_tx.send((slot, update)).is_err() {
                        break;
                    }
                }
            });
            agent.acp.set_resource_sink(raw_tx);
            // E1：流式正文分片同款——每次拉起都重挂 sink，respawn 后流式不会静默断掉。
            // 旧 AcpClient 随进程一起 drop，它的 sink 自然失效（不会重复投递）。
            let (chunk_raw_tx, mut chunk_raw_rx) = mpsc::unbounded_channel::<TurnChunk>();
            let chunk_tagged_tx = l.chunk_tx.clone();
            let chunk_slot = agent.index;
            tokio::spawn(async move {
                while let Some(chunk) = chunk_raw_rx.recv().await {
                    if chunk_tagged_tx.send((chunk_slot, chunk)).is_err() {
                        break;
                    }
                }
            });
            agent.acp.set_chunk_sink(chunk_raw_tx);
            l.pool.return_agent(agent);
            // 起来了就清掉旧失败原因：否则用户会拿着上一次的旧原因排查现在的故障。
            handle.set_last_start_error(None);
            tracing::info!("agent process ready");
        }
        SpawnOutcome::Err { index, detail } => {
            handle.dead.store(true, Ordering::Relaxed);
            handle.set_last_start_error(Some(detail.clone()));
            l.crash_backoff = l.crash_backoff.saturating_add(1);
            let delay = respawn_delay(l.crash_backoff);
            emit_agent_start_failure(&mut std::io::stdout(), &detail, delay);
            // 槽位 0 是保底热 agent：沿用原来的「退避后重试」语义。
            // 额外槽位（会话级隔离按需加的）失败时**不再自己重试**：留空即可，
            // 等下一次 ensure_agent_capacity 按真实需求决定要不要再拉 —— 否则一个
            // 起不来的槽位会变成永不停止的重试循环（owner 2026-10-04 明确不要死循环）。
            if index == 0 {
                schedule_agent_start(l, handle, delay, None, 0);
            } else {
                crate::log!("[acp] 额外槽位 {index} 启动失败，暂不重试（按需再拉）：{detail}");
            }
        }
    }
}

/// 启动失败必须落到 service stdout（bridge.out）；这里不要退回 tracing——
/// 生产进程没有 tracing subscriber，日志会静默丢掉（#246②）。
fn emit_agent_start_failure(writer: &mut dyn std::io::Write, detail: &str, delay: Duration) {
    crate::log_to!(
        writer,
        "[acp] agent start failed — retrying in {}s: {detail}",
        delay.as_secs()
    );
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
    handle: &RuntimeHandle,
    delay: Duration,
    old_agent: Option<OwnedAgent>,
    index: usize,
) {
    debug_assert!(
        !l.spawning.contains(&index),
        "must not stack spawn attempts for the same slot"
    );
    l.spawning.insert(index);
    // 新一轮 spawn 发起：能力位复位 Unknown（initialize 前任何读取都按
    // 「未就绪」拒答——绝不沿用上一次的判定给过旧 fork 开闸）。
    handle
        .sandbox_supported
        .store(SandboxSupport::Unknown as u8, Ordering::Relaxed);
    // 同款复位：initialize 前不得沿用上一次拉起的 steer 能力判定。
    let _ = handle.steering.send(SteerSupport::Unknown);
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
            spawn_and_init_agent(&cfg, index).await
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
async fn spawn_and_init_agent(cfg: &AgentConfig, index: usize) -> SpawnOutcome {
    let mut acp = match AcpClient::spawn(&cfg.command, &cfg.args, &cfg.extra_env).await {
        Ok(acp) => acp,
        Err(e) => {
            return SpawnOutcome::Err {
                index,
                detail: format!("failed to spawn agent: {e}"),
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(60), acp.initialize()).await {
        Ok(Ok(init_result)) => {
            let protocol_version = init_result["protocolVersion"].as_u64().unwrap_or(1) as u32;
            let agent_name = normalized_agent_name(&init_result);
            tracing::info!(name = %agent_name, protocol_version, "agent initialized");
            SpawnOutcome::Ok {
                index,
                agent: Box::new(OwnedAgent {
                    index,
                    acp,
                    state: Default::default(),
                    agent_name,
                    goose_system_prompt_supported: None,
                    protocol_version,
                    session_sandbox: cfg.session_sandbox.clone(),
                }),
            }
        }
        Ok(Err(e)) => {
            acp.shutdown().await;
            // stderr 尾巴（shutdown 已给它收尾窗口）：把「真正的病句」带给用户——
            // 只说「进程意外退出」等于什么都没说（见 RuntimeHandle::last_start_error）。
            let detail = crate::redact::error_with_stderr_tail(
                format!("agent initialize failed: {e}"),
                acp.stderr_tail().as_deref(),
            );
            SpawnOutcome::Err { index, detail }
        }
        Err(_) => {
            acp.shutdown().await;
            let detail = crate::redact::error_with_stderr_tail(
                "agent initialize timed out (60s)".to_string(),
                acp.stderr_tail().as_deref(),
            );
            SpawnOutcome::Err { index, detail }
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

/// 会话级隔离的容量规划：**队列里有多少待跑会话，就保底多少 agent**（上限 `MAX_AGENT_SLOTS`）。
///
/// 为什么必须在这里做：单槽位时，一个会话的长回合会把整个 bot 的其它会话全部排在它后面
/// （owner 2026-10-04 实报「全堵死了」）。按需求扩池后，每个待跑会话都有自己的 agent，
/// 会话之间互不影响；同一会话内部仍由 per-chat 队列串行（语义不变）。
///
/// 槽位只追加不回收（保持 index 不变式；空槽位不占资源，agent 也仍留在池里当热实例）。
fn ensure_agent_capacity(l: &mut Loop, handle: &RuntimeHandle) {
    let want = wanted_slots(l.queue.pending_channels());
    while l.pool.slot_count() < want {
        l.pool.push_slot();
    }
    // 给「有需求但还空着」的槽位各起一个 attempt（每槽位最多一个在跑）。
    for idx in 0..want.min(l.pool.slot_count()) {
        if !l.pool.slot_alive(idx) && !l.spawning.contains(&idx) {
            schedule_agent_start(l, handle, Duration::ZERO, None, idx);
        }
    }
}

/// 把队列里的可跑批次投给空闲 agent（会话级隔离：**投满所有空闲 agent**）。
///
/// 单槽位时代这里「投一个就返回」；多槽位下那样写等于让扩出来的 agent 干等着、白隔离。
/// 现在循环投递，直到没有可投批次或没有空闲 agent。
fn dispatch_pending(l: &mut Loop) {
    // `idle_count` 是防御性上界：正常情况下每轮要么投出一个、要么队列已空。
    while l.pool.idle_count() > 0 && dispatch_one(l) {}
}

/// 投一个批次；返回是否真的投出去了（供 `dispatch_pending` 循环）。
fn dispatch_one(l: &mut Loop) -> bool {
    let Some(batch) = l.queue.flush_next() else {
        return false;
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
            return false;
        }
    };
    tracing::debug!(agent = agent.index, %channel_id, affinity_hit, "agent_claimed");
    // 被借走 = 不再是闲置：回收巡检不得动它。
    l.idle_since.remove(&agent.index);

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
    let registry = l.registry.clone();
    let (steer_tx, steer_rx) = mpsc::channel::<SteerRequest>(1);
    agent.acp.install_steer_rx(steer_rx);
    let (control_tx, control_rx) = oneshot::channel::<ControlSignal>();

    let abort_handle = l.pool.join_set.spawn(async move {
        crate::runtime::pool::run_prompt_task(
            agent,
            batch,
            ctx_clone,
            result_tx,
            Some(control_rx),
            registry.clone(),
        )
        .await;
    });
    l.pool.task_map_mut().insert(
        abort_handle.id(),
        crate::runtime::pool::TaskMeta {
            agent_index,
            channel_id: Some(channel_id),
            recoverable_batch,
            control_tx: Some(control_tx),
            steer_tx: Some(steer_tx),
            successful_steer_deliveries: HashSet::new(),
        },
    );
    l.last_activity = Instant::now();
    true
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
    let (tag, closing) = crate::runtime::queue::native_steer_framing();
    let be = BatchEvent {
        msg: msg.clone(),
        received_at: Instant::now(),
    };
    let event_block = crate::runtime::queue::format_event_block(channel_id, None, &be);
    let new_message = crate::runtime::prompt_framing::semantic_section(tag, "");
    let event_section = crate::runtime::prompt_framing::semantic_section_with_attributes(
        "abb-event",
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

/// 死信（重试耗尽）给用户看的原因文案。
///
/// **为什么单独抽出来**（2026-10-04 owner 实报：机器人只回「多次重试后仍未处理成功（agent 进程
/// 退出）」）：这条路径以前**既不写日志、也不带任何细节**，维护者和用户都只能猜。而 agent 的
/// stderr 其实早就被有界缓存着（`AcpClient::stderr_tail`，与「agent 起不来」走的是同一份）——
/// 只是这里没用它。现在 `AgentExited` 会把那份尾巴（经 `redact` 脱敏、折行、截断）带给用户，
/// 让下一条失败回复自己说明原因。
fn dead_letter_reason(outcome: &PromptOutcome, stderr_tail: Option<&str>) -> String {
    match outcome {
        // 超时同样带尾巴：卡死的 agent 往往是「还在等某个子进程/网络」，它最后几行 stderr
        // 就是唯一线索（2026-10-04 那两组 0 CPU、零连接的 agent 就属于这种）。
        PromptOutcome::Timeout(kind) => {
            let base = match kind {
                TimeoutKind::Idle => "回合超时".to_string(),
                TimeoutKind::Hard { .. } => "回合超过时长上限".to_string(),
            };
            crate::redact::error_with_stderr_tail(base, stderr_tail)
        }
        PromptOutcome::AgentExited => {
            crate::redact::error_with_stderr_tail("agent 进程退出".to_string(), stderr_tail)
        }
        PromptOutcome::Error(e) => format!("{e}"),
        _ => "重复失败".to_string(),
    }
}

#[cfg(test)]
mod dead_letter_reason_tests {
    use super::*;

    /// 判别力：删掉 stderr 尾巴那段，这条必红 —— 而它正是 2026-10-04 实报的那个坑
    /// （机器人只回「agent 进程退出」，排查时只能靠进程快照倒推）。
    #[test]
    fn agent_exited_carries_stderr_tail() {
        let r = dead_letter_reason(
            &PromptOutcome::AgentExited,
            Some("Error: Cannot find module 'x'"),
        );
        assert!(r.contains("agent 进程退出"), "{r}");
        assert!(
            r.contains("Cannot find module"),
            "必须带出 agent 的 stderr 尾巴：{r}"
        );
    }

    /// 没有尾巴时不许出现「；agent stderr：」这种悬空前半句（`redact` 已保证，这里钉住调用方）。
    #[test]
    fn agent_exited_without_tail_stays_plain() {
        assert_eq!(
            dead_letter_reason(&PromptOutcome::AgentExited, None),
            "agent 进程退出"
        );
        assert_eq!(
            dead_letter_reason(&PromptOutcome::AgentExited, Some("   ")),
            "agent 进程退出"
        );
    }

    /// 超时也要带尾巴（卡死的 agent 最后几行 stderr 是唯一线索）。
    #[test]
    fn timeout_carries_stderr_tail() {
        let r = dead_letter_reason(
            &PromptOutcome::Timeout(TimeoutKind::Idle),
            Some("waiting for mcp server third-party-server"),
        );
        assert!(r.contains("回合超时"), "{r}");
        assert!(r.contains("third-party-server"), "{r}");
    }

    /// 重生日志：必须带出退避级别与 agent 的 stderr 尾巴 —— 「为什么一直在重启」以前只能靠
    /// 反复抓进程表倒推（tracing 从未接线，上游那句 `agent died — respawning` 被直接丢弃）。
    #[test]
    fn respawn_log_line_carries_backoff_and_tail() {
        let s = respawn_log_line("exited", Some("ABB_AGENT_PROVIDER is required"), 3);
        assert!(s.contains("退避级别 3"), "{s}");
        assert!(s.contains("outcome=exited"), "{s}");
        assert!(s.contains("ABB_AGENT_PROVIDER is required"), "{s}");
    }

    #[test]
    fn respawn_log_line_without_tail_stays_plain() {
        let s = respawn_log_line("idle_timeout", None, 0);
        assert!(s.contains("agent 重生"), "{s}");
        assert!(!s.contains("agent stderr"), "没尾巴不许留悬空前缀：{s}");
    }

    /// 其它结局的文案不变（防这次改动顺手改了别的分支）。
    #[test]
    fn other_outcomes_unchanged() {
        assert_eq!(
            dead_letter_reason(&PromptOutcome::Timeout(TimeoutKind::Idle), None),
            "回合超时"
        );
        assert_eq!(
            dead_letter_reason(&PromptOutcome::Cancelled, None),
            "重复失败"
        );
    }
}

/// agent 死掉/卡死后**重生**那一步的日志文案（纯函数，便于单测）。
///
/// **为什么必须显式写 `crate::log!` 而不是只留 `tracing::warn!`**（2026-10-04 owner 追问
/// 「为什么执行层一直在重启」）：本仓**从未安装 tracing subscriber**（`tracing = "0.1"`
/// 只是个空壳），移植区所有 `tracing::*` 事件——包括上游那句 `agent died — respawning`——
/// 全部被丢弃，`bridge.out` 里一个字都没有，只能靠反复抓进程表倒推。这行日志把「谁重生、
/// 退避到第几级、agent 最后一次 stderr 说了什么」一次落盘。
fn respawn_log_line(outcome_label: &str, stderr_tail: Option<&str>, backoff: u32) -> String {
    crate::redact::error_with_stderr_tail(
        format!("agent 重生（outcome={outcome_label}，退避级别 {backoff}）"),
        stderr_tail,
    )
}

fn handle_prompt_result(l: &mut Loop, handle: &RuntimeHandle, mut result: PromptResult) {
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
    // `cancel_requeued` = 本次取消把批次交给队列**重提示**了（Steer 合并 /
    // 排水超时同路）；只有**没有**重提示的取消才是同步等待方的终态（#309）。
    let mut cancel_requeued = false;
    if let Some(batch) = result.batch.take() {
        if !l.removed_channels.contains(&batch.channel_id) {
            if matches!(
                result.outcome,
                PromptOutcome::Cancelled | PromptOutcome::CancelDrainTimeout(_)
            ) {
                // 走到这里说明批次是被 **Steer**（回合中来了新消息）取消后由 pool
                // 交还的：进 cancelled_batches，flush_next 并入下一批重提示。
                // CancelDrainTimeout 与干净取消同路（5s 排水超时不是硬上限死信；
                // 批次不带重试账）。注意：纯 `Cmd::Cancel` 在 pool 侧就已丢批次，
                // 不会进到这个分支（requeue_cancelled_batch(Cancel) == None）。
                let reason = batch.cancel_reason.unwrap_or(CancelReason::Steer);
                l.queue.requeue_as_cancelled(batch, reason);
                cancel_requeued = true;
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
            } else if matches!(&result.outcome, PromptOutcome::Error(e) if is_sandbox_unsupported(e))
            {
                // P2.3：受限档位能力缺失（session 创建被 pool 硬闸拒绝）——不可
                // 重试：agent 进程本身健康，重试只会再撞同一闸。当场死信并给可
                // 行动提示（升级 ABB / 换回随包 agent）。
                tracing::warn!(
                    channel_id = %batch.channel_id,
                    "dead-lettering batch — agent lacks _meta.abbSandbox capability"
                );
                notify_channel(
                    l,
                    handle,
                    &batch,
                    "⚠️ 处理失败：随包 agent 不支持受限执行档位（请升级 ABB 后重试）。".to_string(),
                );
            } else if matches!(&result.outcome, PromptOutcome::Error(e) if is_model_not_found(e)) {
                // #7538（上游 051c3a270）：agent 报「配置的模型在 provider 端不存在」——
                // **不可重试**：同一个不存在的模型重试多少次都不会自愈，只会把失败回复
                // 拖到重试耗尽。当场死信并给可行动提示。走 notify_channel（而非上游的
                // relay 发布面）：它会同时把终态 Err 旁路给同步等待者，job/oneshot 才不会
                // 只表现为「挂到超时」。
                tracing::warn!(
                    channel_id = %batch.channel_id,
                    "dead-lettering batch — model not found"
                );
                notify_channel(
                    l,
                    handle,
                    &batch,
                    "⚠️ 处理失败：当前配置的模型在 provider 端不存在。请在 ABB 配置里换用可用模型后重发。"
                        .to_string(),
                );
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
            } else if matches!(&result.outcome, PromptOutcome::Error(e) if is_spawn_refused(e)) {
                // 2026-10-04 owner 硬要求「不能死循环」：熔断期间**当场死信**，不进 requeue ——
                // 重试只会再撞同一闸（还会把失败提示拖到重试耗尽）。agent 压根没起，故不记失败。
                let msg = match &result.outcome {
                    PromptOutcome::Error(e) => e.to_string(),
                    _ => "agent spawn 被进程保护拦下".to_string(),
                };
                // 带上守卫快照（连续失败数 + 冷却剩余）：这条日志就是「为什么又没回」的答案。
                let (fails, open) = crate::spawn_guard::SpawnGuard::global()
                    .snapshot(crate::spawn_guard::AGENT_KEY);
                crate::log!(
                    "[harness] 死信（spawn 熔断）：channel={} 连续失败={} 冷却剩余={:?} → {}",
                    batch.channel_id,
                    fails,
                    open,
                    msg
                );
                notify_channel(
                    l,
                    handle,
                    &batch,
                    format!("⚠️ 暂时不再重试：{msg}。稍后（约一分钟后）再发即可。"),
                );
            } else if let Some(dead) = l.queue.requeue(batch) {
                let reason =
                    dead_letter_reason(&result.outcome, result.agent.acp.stderr_tail().as_deref());
                // 这条路径以前**一行日志都没有**（2026-10-04 排查时 bridge.out 里查不到任何痕迹，
                // 只能靠进程快照倒推）。死信是「用户看得见的失败」，必须留痕。
                crate::log!(
                    "[harness] 死信（重试耗尽）：channel={} → {}",
                    dead.channel_id,
                    reason
                );
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

    // #309：本轮若收到过取消请求，就在**真实结局**处给同步 waiter 取消终态
    // （覆盖 Cancelled / CancelDrainTimeout / AgentExited / Error / Timeout 全部结局——
    // 不能只在 Cancelled 分支做，否则取消失败类结局会落回「挂到 timeout」）。
    // 批次被 Steer 合并重提示时不发终态：等待方应当等那次重提示的文本。
    let PromptSource::Channel(ch) = &result.source;
    if l.cancel_requested.remove(ch) {
        if let Some(msg) = cancel_waiter_terminal(true, cancel_requeued, &result.outcome) {
            let _ = deliver_sync_wait_msg(handle, *ch, msg);
        }
    }
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
            // 进程保护：健康回合把连续失败/熔断复位（否则历史失败会一直压着退避）。
            crate::spawn_guard::SpawnGuard::global().record_success(crate::spawn_guard::AGENT_KEY);
            let PromptSource::Channel(channel_id) = &result.source;
            if let Some(text) = result.final_text.take() {
                // 技能路径归一化：模型被问技能时会把 pi 自带系统提示里的
                // available_skills 段整段照抄（含全路径目录）——用户消息级的
                //「不贴路径」指令压不住 pi 原生提示（探针实证）。这里把「整行
                // 恰好是技能路径」的行改写成技能名，其余行一律不动（窄模式：
                // 只有纯路径行会命中，正常提及路径的叙述不受影响）。
                let text = redact_skill_paths(&text);
                // P4.3：后端标识后缀（`── 后端：X`）已删——执行层收口随包
                // 单一执行层后路由核验维度消亡，回复正文即最终文本。
                let meta = handle.channel_meta(channel_id);
                tracing::info!(%channel_id, text_chars = text.chars().count(), "turn text captured — delivering");
                // 同步等待者（job/oneshot 路径）旁路：文本直接回传，不产生 chat 投递
                if let Some(tx) = handle.sync_waiters.lock().unwrap().remove(channel_id) {
                    // E3：本回合的真实 usage 与文本**同一处、同一时刻**落位，
                    // 供 oneshot 收到回执后取走（见 `RuntimeHandle::turn_usage`）。
                    // 只挂在同步等待路径上：chat 投递路径没有消费方，挂了就是白占表。
                    if let Some(usage) = result.final_usage.take() {
                        handle.turn_usage.lock().unwrap().insert(*channel_id, usage);
                    }
                    let _ = tx.send(SyncWaitMsg::Done(Ok(text)));
                } else {
                    let _ = handle.turn_tx.send(TurnOutput {
                        channel_id: *channel_id,
                        meta,
                        text,
                    });
                }
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
            // 进程保护：把这次死亡记进全局退避账（连续失败会推高等待，达到阈值直接熔断）。
            crate::spawn_guard::SpawnGuard::global()
                .record_failure(crate::spawn_guard::AGENT_KEY, std::time::Instant::now());
            // 尾巴要在 schedule_death_respawn 取走 agent 之前读（它 move 掉 result.agent）。
            let tail = result.agent.acp.stderr_tail();
            crate::log!(
                "[harness] {}",
                respawn_log_line(outcome_label, tail.as_deref(), l.crash_backoff)
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
            crate::spawn_guard::SpawnGuard::global()
                .record_failure(crate::spawn_guard::AGENT_KEY, std::time::Instant::now());
            let tail = result.agent.acp.stderr_tail();
            crate::log!(
                "[harness] {}",
                respawn_log_line(outcome_label, tail.as_deref(), l.crash_backoff)
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
    // 回到池子 = 开始计闲置时间（空闲回收据此收掉长期不用的额外 agent）。
    l.idle_since.insert(agent.index, Instant::now());
    tracing::debug!(agent = agent.index, outcome = outcome_label, "{log_line}");
    l.pool.return_agent(agent);
}

/// 致命结局统一入口：退避升级 + 置 Dead（新消息预检拒绝）+ 后台重拉。
fn schedule_death_respawn(
    l: &mut Loop,
    handle: &RuntimeHandle,
    old_agent: OwnedAgent,
    outcome_label: &str,
) {
    l.crash_backoff = l.crash_backoff.saturating_add(1);
    let delay = respawn_delay(l.crash_backoff);
    handle.dead.store(true, Ordering::Relaxed);
    // 运行中崩溃也要留因：否则用户看到的是「未就绪」却不带任何原因。
    handle.set_last_start_error(Some(format!("运行中崩溃：{outcome_label}")));
    tracing::warn!(
        agent = old_agent.index,
        outcome = outcome_label,
        backoff_secs = delay.as_secs(),
        "agent died — respawning in {}s",
        delay.as_secs()
    );
    let dead_index = old_agent.index;
    schedule_agent_start(l, handle, delay, Some(old_agent), dead_index);
}

/// 取消终态决策（#309 PR-A1）——纯函数，便于把分支钉死在单测里：
///
/// - `cancel_requested`：这一轮收到过取消请求（`Cmd::Cancel` 且信号送达）；
/// - `cancel_requeued`：该批次的取消走的是「合并重提示」（Steer / 排水超时）；
/// - `outcome`：该轮**真实结局**。
///
/// 语义（**自然完成优先**）：
/// - 收到取消、但该轮其实以 `Ok` 收尾 → **不发**取消终态，让等待方拿真实文本。
///   取消与自然完成是竞速关系（`Cmd::Cancel` 只登记、终态在真实结局处判定）；
///   若这里硬报 Cancelled，就会出现「谎报取消 + 文本照样进聊天」的不一致；
/// - 收到取消且该轮**没有**产出结果（`Cancelled` / `CancelDrainTimeout` /
///   `AgentExited` / `Error` / `Timeout`）→ 给 `Cancelled` 终态；
/// - 批次被合并重提示 → 不发终态，等待方等重提示后的文本。
///
/// 注意：若错误旁路（`notify_channel`）已先把 waiter 取走并发了 `Done(Err)`，
/// 本决策会找不到 waiter 而自然 no-op——即**错误旁路优先**，与上述并列一致。
fn cancel_waiter_terminal(
    cancel_requested: bool,
    cancel_requeued: bool,
    outcome: &PromptOutcome,
) -> Option<SyncWaitMsg> {
    if !cancel_requested || cancel_requeued {
        return None;
    }
    if matches!(outcome, PromptOutcome::Ok(_)) {
        return None;
    }
    Some(SyncWaitMsg::Cancelled)
}

/// 把消息投给该频道的同步 waiter（取走条目）。返回是否确实有一个 waiter 收到。
fn deliver_sync_wait_msg(handle: &RuntimeHandle, channel_id: Uuid, msg: SyncWaitMsg) -> bool {
    match handle.sync_waiters.lock().unwrap().remove(&channel_id) {
        Some(tx) => {
            let _ = tx.send(msg);
            true
        }
        None => false,
    }
}

/// 失败批次死信/重试耗尽时的频道告示（出站 TurnOutput，桥侧写历史并发送）。
/// 同步等待者旁路（P3.1）：该频道有 waiter 时同文以 Err 回传——agent 终态失败
/// （死信/认证失效/档位不支持）对 job/oneshot 路径不再只表现为「挂到超时」。
fn notify_channel(l: &mut Loop, handle: &RuntimeHandle, batch: &FlushBatch, text: String) {
    if l.removed_channels.contains(&batch.channel_id) {
        return;
    }
    let meta = handle.channel_meta(&batch.channel_id);
    tracing::warn!(channel_id = %batch.channel_id, "sending failure notice to channel");
    if let Some(tx) = handle
        .sync_waiters
        .lock()
        .unwrap()
        .remove(&batch.channel_id)
    {
        let _ = tx.send(SyncWaitMsg::Done(Err(text.clone())));
    }
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

/// #7538（上游 051c3a270）：agent 报「配置的模型在 provider 端不存在」——`-32002` 且
/// message 含 `model not found`。**不可重试**：重试同一个不存在的模型不可能自愈。
///
/// 只认模型类 `-32002`：同码的其它 resource-not-found（如 `Resource not found: session
/// no longer exists`）仍走原有 requeue —— 误判会把本可自愈的批次当场死信（上游为这条
/// 专门加了回归 `non_model_resource_not_found_is_requeued`）。
fn is_model_not_found(error: &AcpError) -> bool {
    let AcpError::AgentError { code, message } = error else {
        return false;
    };
    *code == -32002 && message.contains("model not found")
}

/// P2.3：受限会话被 `pool::create_session_and_apply_model` 的档位硬闸拒绝
/// （随包 agent 未声明 `_meta.abbSandbox`，或词表不含请求档位）。不可重试——
/// agent 进程本身健康，重试只会再撞同一闸。
///
/// 走独立变体 [`AcpError::SandboxUnsupported`] 而非 `Protocol`：后者在
/// `is_transport_error` 之列，会把健康 agent 判为「管道可能坏」而
/// `schedule_death_respawn`（杀进程 + 置 dead，后续消息全被预检拒掉）。
/// spawn 守卫拒绝（熔断/限速）—— **不是** agent 病，重试只会再撞同一闸。
fn is_spawn_refused(error: &AcpError) -> bool {
    matches!(error, AcpError::SpawnRefused(_))
}

fn is_sandbox_unsupported(error: &AcpError) -> bool {
    matches!(error, AcpError::SandboxUnsupported(_))
}

// ── recover_panicked_agent（上游移植） ───────────────────────────────────────

fn recover_panicked_agent(
    l: &mut Loop,
    handle: &RuntimeHandle,
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
    schedule_agent_start(l, handle, delay, None, i);
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

/// 技能路径行归一化：`- /…/<name>/SKILL.md` → `- <name>`（仅当「整行恰好是
/// 一个技能路径（可带列表符号）」时改写；其余行——包括叙述里顺带提到的路径——
/// 一律不动）。用于模型照抄系统提示 available_skills 段（含全路径目录）时的
/// 回复净化；技能名含空格/斜杠等非标识符形态时跳过（保守）。
fn redact_skill_paths(text: &str) -> String {
    text.lines()
        .map(|line| {
            let trimmed = line.trim();
            let (bullet, rest) = if let Some(r) = trimmed.strip_prefix("- ") {
                ("- ", r)
            } else if let Some(r) = trimmed.strip_prefix("* ") {
                ("* ", r)
            } else if let Some(r) = trimmed.strip_prefix("• ") {
                ("• ", r)
            } else {
                ("", trimmed)
            };
            let Some(stripped) = rest.strip_suffix("/SKILL.md") else {
                return line.to_string();
            };
            if !stripped.starts_with('/') {
                return line.to_string();
            }
            let Some(name) = stripped.rsplit('/').next() else {
                return line.to_string();
            };
            if name.is_empty()
                || name
                    .chars()
                    .any(|c| c.is_whitespace() || c == '/' || c == '\\')
            {
                return line.to_string();
            }
            let indent = &line[..line.len() - trimmed.len()];
            format!("{indent}{bullet}{name}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod resource_event_tests {
    use super::*;

    fn update(text: &str) -> ResourceUpdate {
        ResourceUpdate {
            session_id: "abb-1".to_string(),
            server: "notes".to_string(),
            uri: "file:///n.md".to_string(),
            text: text.to_string(),
        }
    }

    /// 事件消息的形状 + id 语义：来源如实标注；同内容同 id（**供排障定位/steer 认 id**，队列并不按它去重；
    /// 重复同内容通知由 `(channel, server, uri)` 的 10s 节流窗口合并）、内容/uri 变了就是新消息。
    #[test]
    fn resource_update_message_shape_and_dedup_key() {
        let m = resource_update_message(&update("正文"));
        assert_eq!(m.author_role, "event", "事件不是人说的，必须如实标注");
        assert_eq!(m.prompt_tag, "mcp:resource");
        assert!(m.text.contains("server=notes"), "正文要带来源：{}", m.text);
        assert!(
            m.text.contains("uri=file:///n.md"),
            "正文要带 uri：{}",
            m.text
        );
        assert!(m.text.ends_with("正文"));

        assert_eq!(
            m.id_hex,
            resource_update_message(&update("正文")).id_hex,
            "同内容 → 同 id（供排障定位/steer 认 id，非队列去重）"
        );
        assert_ne!(
            m.id_hex,
            resource_update_message(&update("内容变了")).id_hex,
            "内容变了 → 新 id（真变化不能被当重复丢掉）"
        );
        assert_ne!(
            m.id_hex,
            resource_update_message(&ResourceUpdate {
                uri: "file:///other.md".to_string(),
                ..update("正文")
            })
            .id_hex,
            "uri 变了 → 新 id"
        );
    }

    /// **本卡核心端到端**：回合进行中 agent 主动上报资源更新 ⇒ ABB 在**同一次读**里读到它
    /// （无需常驻读循环），落成**原会话**的一条入站消息并最终出现在该会话的 prompt 里。
    ///
    /// 判据不靠内部状态，靠假 agent 的记录文件：它收到的某次 prompt 文本里必须带上
    /// `[MCP 资源更新]`（说明事件真的回到原会话并被交付）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn agent_reported_resource_update_lands_in_origin_session() {
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let rec = std::env::temp_dir().join(format!("mock-res-{}.jsonl", Uuid::new_v4()));
        let stop = CancellationToken::new();
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: python3,
                args: vec![script.display().to_string()],
                extra_env: vec![
                    ("PATH".to_string(), crate::deps::composed_path()),
                    ("MOCK_RECORD_FILE".to_string(), rec.display().to_string()),
                    (
                        "MOCK_RESOURCE_UPDATE".to_string(),
                        "资源正文-HELLO".to_string(),
                    ),
                    ("MOCK_RESOURCE_SERVER".to_string(), "mockmcp".to_string()),
                    ("MOCK_RESOURCE_URI".to_string(), "mock://res".to_string()),
                ],
                backend: "mock".to_string(),
                session_sandbox: None,
            },
            stop.clone(),
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
            false,
        );
        let run = tokio::spawn(run_loop(handle.clone()));
        let cid = Uuid::new_v4();
        handle.upsert_channel(
            cid,
            ChannelMeta {
                bot_key: "res2".to_string(),
                chat_id: "oc_res2".to_string(),
                chat_type: "p2p".to_string(),
                thread_id: None,
                name: "event".to_string(),
                anchor_mid: None,
                adhoc: true,
                workspace: None,
            },
        );
        // 首轮只用来"驱动 agent 起来并让它发出通知"：事件在回合内到达会走 steer/排队语义，
        // 首轮结局因此可能是 Ok，也可能是被 steer 打断的形态——**这里不断言它**（判据是下面那句
        // "标记是否出现在该会话的某次 prompt 里"，那才是本卡要证的事；全量套件下时序会漂移，
        // 断言首轮结局会让用例变成偶发红）。
        // 与真实聊天同形：**fire-and-forget** 推一条用户消息（不等同步结局）。
        // 事件在回合内到达会走 steer/排队语义，首轮结局不是本用例的断言对象。
        assert!(
            handle.push_message(
                cid,
                InboundMsg {
                    id_hex: "u-1".to_string(),
                    author_role: "user".to_string(),
                    text: "先跑一轮".to_string(),
                    ts_secs: 0,
                    prompt_tag: "test".to_string(),
                    bridge_generation: String::new(),
                },
            ),
            "句柄应在跑（push_message 返回 true）"
        );
        // 给事件回合一点时间落盘（它可能与首轮合并成 steer，也可能排到下一轮）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut landed = false;
        while std::time::Instant::now() < deadline && !landed {
            if let Ok(log) = std::fs::read_to_string(&rec) {
                landed = log.lines().any(|line| {
                    line.contains("[MCP 资源更新]")
                        && line.contains("mockmcp")
                        && line.contains("资源正文-HELLO")
                });
            }
            if !landed {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        stop.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), run).await;
        let log = std::fs::read_to_string(&rec).unwrap_or_default();
        assert!(
            landed,
            "资源更新必须回到原会话并出现在 prompt 里；实际记录：\n{log}"
        );
        let _ = std::fs::remove_file(&rec);
    }

    /// **接线守卫**（评审 F1 阻塞项 + O1/O2 收口）：资源事件日志必须走 `crate::log!`（stdout），
    /// **不得**走 `tracing::*`（根二进制没有 subscriber ⇒ 线上静默）。
    ///
    /// 三条断言各自的边界（**如实写明**，别把它读成万能）：
    /// ①`handle_resource_update` 词法范围内：不含 `tracing::`，且经唯一出口 `log_resource_lines(`；
    /// ②**出口函数** `log_resource_lines` 本身：用 `crate::log!` 且不含 `tracing::`
    ///   ——加这条是为了堵"把出口体内的宏换掉"（评审实测：换掉即红）；
    /// **实测边界（评审两轮复测后的准确版）**：
    ///
    /// - **红**：换掉出口体内的宏 / 把**两处**出口调用都换成 tracing 间接层 / handler 内直接写 `tracing::` /
    ///   把**丢弃-汇总**那一处单独换走（③b 专为它加的针）/ 把孤儿那处换走（③）/ handler 绕过单点接线自行入队记账；
    /// - **仍绿（已知盲区，如实登记）**：在 handler 内**并行再加一条**走函数体外 helper 的日志调用
    ///   （出口调用仍在 ⇒ ①②③ 全满足）。要闭合它需要行为级判据（writer 注入 / `log_to!`），本卡不做。
    ///
    /// ③ O1 的两处**接线**（此前"对象层有判据、集成层没判据"，变异 M7/M10 都能全绿）：
    ///
    /// - `l.resource_throttle.mark_delivered(` 必须出现在 `if handle_cmd(` 之内（入队后才记账）；
    /// - `l.resource_throttle.forget_channel(` 必须存在（频道移除时清状态）。
    ///
    /// 边界：断言的是"这两行还在、且 mark_delivered 在入队判定之内"，拦的是**删除/移出判据**这类回归，
    /// 不保证语义等价（真要做行为判据需要给 `Loop` 注入假 handle，属更大改造）。
    #[test]
    fn resource_event_logs_wire_to_stdout_not_tracing() {
        // **先归一化 EOL**（2026-10-12，卡 abb-windows-ps1-exit-code-leak-20261012）：
        // 这是**源码文本断言**，而 Windows 检出（`core.autocrlf=true`）给的 harness.rs 是 CRLF——
        // `rest.find("\n}\n")` 会找不到"顶格 }"，测试在 Windows 上 panic（CI 实证：
        // `panicked at src\runtime\harness.rs:2470`）。归一化后两平台一致；
        // 仓库根 .gitattributes 也钉了 `*.rs text eol=lf`（双保险）。
        let src = include_str!("harness.rs").replace("\r\n", "\n");
        let body_of = |fname: &str| -> String {
            let start = src
                .find(&format!("fn {fname}"))
                .unwrap_or_else(|| panic!("找不到函数 {fname}"));
            let rest = &src[start..];
            let end = rest.find("\n}\n").expect("函数体以顶格 } 结束");
            rest[..end].to_string()
        };
        // ① handler
        let handler = body_of("handle_resource_update");
        assert!(
            handler.contains("log_resource_lines("),
            "资源事件日志必须经唯一出口 log_resource_lines（否则守卫的边界会退回词法范围）"
        );
        assert!(
            !handler.contains("tracing::"),
            "资源事件路径不得使用 tracing::*：根二进制没有 subscriber，日志会静默丢掉（评审 F1）"
        );
        // ② 出口本身：harness 侧这层必须**转发到全仓唯一出口** `log_out::resource_line`
        //    （`crate::log!` 的断言由下面 ④ 对 `log_out.rs` 本体做）。
        let seam = body_of("log_resource_lines");
        assert!(
            seam.contains("log_out::resource_line("),
            "harness 日志出口必须转发到 log_out::resource_line"
        );
        assert!(
            !seam.contains("tracing::"),
            "出口不得用 tracing::*（评审 O2）"
        );
        // ③ O1 的接线判据（单点接线 + 频道清理）
        // handler **不得绕过单点接线**（自己直接调 handle_cmd / mark_delivered 就会绕过"入队才记账"）。
        assert!(
            handler.contains("enqueue_and_arm("),
            "资源事件必须经单点接线 enqueue_and_arm（否则 F3 的判据无处可钉）"
        );
        assert!(
            !handler.contains("handle_cmd(") && !handler.contains(".mark_delivered("),
            "handler 不得绕过单点接线直接入队/记账"
        );
        // 单点接线内部：按**展平后的结构**断言"记账在 if accepted 之内"。
        // 为什么展平：源码里的换行/缩进由 rustfmt 决定，按原始排版断会假红；展平后只剩结构。
        let flat: String = body_of("enqueue_and_arm")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        // 针按**展平后**的真实形态写（相邻 token 间是否有空格由源码决定：`l.resource_throttle`
        // 折行后留下一个空格，而 `handle_cmd(` 之后没有）。
        let needle = "if accepted { l.resource_throttle .mark_delivered(";
        assert!(
            flat.contains(needle),
            "记账必须落在 `if accepted`（= 真的入队）之内：{flat}"
        );
        assert!(
            flat.contains("let accepted = handle_cmd("),
            "入队判定必须是 `let accepted = handle_cmd(…)`（改语义即红）"
        );
        // 出口收敛（评审 N3）：`log_out::resource_line` 必须用 `crate::log!` 且不含 `tracing::`，
        // 且 acp 侧的 drop 路径必须经它——否则又回到"harness 有守卫、acp 没有"的不对称（实测那种不对称下
        // 把 acp 的 `crate::log!` 换回 `tracing::warn!`，全量用例仍绿）。
        let outlet = include_str!("log_out.rs").replace("\r\n", "\n");
        let outlet_body = &outlet[outlet.find("fn resource_line").expect("出口存在")..];
        assert!(
            outlet_body.contains("crate::log!("),
            "出口 log_out::resource_line 必须用 crate::log! 写 stdout"
        );
        assert!(
            !outlet_body.contains("tracing::"),
            "出口不得用 tracing::*（根二进制没有 subscriber）"
        );
        // ③ 孤儿更新也必须经出口（评审 N1：原先"唯一出口（含孤儿）"的枚举是假的——孤儿路径
        //   直接 `crate::log!`）。断言按**展平后**形态做，避免被 rustfmt 折行影响。
        let flat_handler: String = handler.split_whitespace().collect::<Vec<_>>().join(" ");
        let orphan_needle = format!("{}(&l.resource_throttle.on_orphan(", "log_resource_lines");
        assert!(
            flat_handler.contains(&orphan_needle),
            "孤儿更新也必须经唯一出口（否则 N1 的枚举又变成假的）"
        );
        // ③b **丢弃/汇总**那处调用点也必须单独钉住（评审第 2 轮 B1 实测）：
        //    把孤儿收进出口后 handler 里有**两处** `log_resource_lines(`，只做 `contains` 会被任意一处满足
        //    ⇒ "只把丢弃/汇总那一处换成 tracing 间接层"（= 让 AC1"不得静默"整条静默回退）逃得掉（实测绿）。
        //    这里的针也**运行期拼**（同一自我指涉理由）。
        let summary_needle = format!("{}(&{})", "log_resource_lines", "lines");
        assert!(
            flat_handler.contains(&summary_needle),
            "丢弃/汇总的日志调用点必须经出口（否则'不得静默'这条能被静默回退）"
        );
        let acp_src = include_str!("acp.rs").replace("\r\n", "\n");
        let drop_needle = format!("{}(&make(n))", "resource_line");
        assert!(
            acp_src.contains(&drop_needle),
            "acp 的 drop 日志必须经同一出口（评审 N3；拼针避免自我指涉）"
        );

        // **运行期拼针**：直接写 `src.contains("l.resource_throttle.forget_channel(")` 会**自我指涉**——
        // 那个字面量就出现在本测试源码里，而 `include_str!` 会把测试本身也读进来 ⇒ 断言恒真
        // （我第一版就是这么写的，删掉接线后守卫照样绿；实测记录见卡 abb-resource-throttle-observations-20261011）。
        // 拼出来之后，"断言里的字面量"与"被断言的针"不再相同，删除接线即红。
        let forget_needle = format!("l.resource_throttle.{}(uuid)", "forget_channel");
        assert!(
            src.contains(&forget_needle),
            "频道移除路径必须清节流状态（删掉即回归 F2 后半；变异 M10）"
        );
    }

    /// "找不到频道"的孤儿更新也不得静默（评审 F1 的同一条规则的另一处）：受同一日志预算约束。
    #[test]
    fn resource_throttle_logs_orphan_updates_within_budget() {
        let now = Instant::now();
        let mut t = ResourceThrottle::new(now);
        let lines = t.on_orphan(now, "notes", "a.md");
        assert_eq!(lines.len(), 1, "孤儿更新必须留一行: {lines:?}");
        assert!(lines[0].contains("找不到对应频道"), "{}", lines[0]);
        assert!(
            lines[0].contains("notes") && lines[0].contains("a.md"),
            "{}",
            lines[0]
        );
        // 预算用尽后只计数（由汇总行报告）
        let mut eager = 1;
        for i in 0..RESOURCE_THROTTLE_LOG_BUDGET * 2 {
            eager += t.on_orphan(now, &format!("s{i}"), "a.md").len();
        }
        assert_eq!(
            eager, RESOURCE_THROTTLE_LOG_BUDGET as usize,
            "即时行不得超预算"
        );
        let later = now + RESOURCE_EVENT_MIN_INTERVAL;
        let lines = t.on_orphan(later, "notes", "b.md");
        let summary = lines
            .iter()
            .find(|l| l.contains("窗口汇总"))
            .expect("应有汇总");
        assert!(
            summary.contains("找不到频道"),
            "汇总应报告孤儿条数: {summary}"
        );
    }

    /// **阈值被钉住**（评审 F5，引 `RULE_阈值变更.md` 第 3 条「无验证的阈值等于摆设」）：
    /// 原实现里没有任何断言守住 10s——变异成 60s 后阳性对照用例照样绿（因为它是从常量自缩放的）。
    /// 这条直接断言当前值：改阈值必须让本用例红，从而在 review 里显式可见（并按规则单独提交）。
    #[test]
    fn resource_event_min_interval_is_pinned() {
        assert_eq!(
            RESOURCE_EVENT_MIN_INTERVAL,
            Duration::from_secs(10),
            "阈值为 10s（card abb-mcp-resource-throttle-20261011 的依据写在常量注释里）；             要改它必须按 RULE_阈值变更.md 单独提交并附证据，并同步改这条断言"
        );
        assert_eq!(
            RESOURCE_EVENT_MAX_PER_SERVER, 3,
            "单 server 每窗口上限（防 uri 轮换绕过，评审 F2 的整改）；变更同上"
        );
        assert_eq!(
            RESOURCE_THROTTLE_LOG_BUDGET, 20,
            "节流日志每窗口即时行预算（防刷爆 bridge.out）；变更同上"
        );
    }

    /// 判定键含 uri 与 channel（评审 F6：原实现里把 uri 从键里去掉，6 条用例**全绿** ⇒ 键没被钉住）。
    #[test]
    fn resource_throttle_key_includes_uri_and_channel() {
        let now = Instant::now();
        let ch1 = Uuid::new_v4();
        let ch2 = Uuid::new_v4();
        let mut t = ResourceThrottle::new(now);
        // 同一 (channel, server) 下不同 uri：各自放行（互不影响）
        for uri in ["a.md", "b.md", "c.md"] {
            let (d, _) = t.on_event(ch1, "notes", uri, now);
            assert_eq!(d, ResourceDecision::Allow, "{uri} 首次应放行");
            t.mark_delivered(ch1, "notes", uri, now);
        }
        // 同一 uri 在窗口内的第二次：丢
        assert_eq!(
            t.on_event(ch1, "notes", "a.md", now).0,
            ResourceDecision::DropByKey
        );
        // 不同 channel 的同一 (server,uri)：不受 ch1 的窗口影响
        assert_eq!(
            t.on_event(ch2, "notes", "a.md", now).0,
            ResourceDecision::Allow,
            "channel 不同 ⇒ 互不影响（键含 channel）"
        );
    }

    /// 单 server 每窗口上限：**uri 每次都变也绕不过**（评审 F2 的核心攻击面）。
    /// 判别力：去掉 `by_server` 判定 ⇒ 第 4 条变成 Allow，本用例红。
    #[test]
    fn resource_event_per_server_cap_blocks_uri_rotation() {
        let now = Instant::now();
        let ch = Uuid::new_v4();
        let mut t = ResourceThrottle::new(now);
        for i in 0..RESOURCE_EVENT_MAX_PER_SERVER {
            let uri = format!("rotating-{i}.md");
            assert_eq!(
                t.on_event(ch, "chatty", &uri, now).0,
                ResourceDecision::Allow,
                "第 {i} 条（不同 uri）应放行"
            );
            t.mark_delivered(ch, "chatty", &uri, now);
        }
        assert_eq!(
            t.on_event(ch, "chatty", "rotating-99.md", now).0,
            ResourceDecision::DropByServer,
            "同一 server 第 {RESOURCE_EVENT_MAX_PER_SERVER}+1 个 uri 必须按 server 上限丢弃（防轮换绕过）"
        );
        // 另一个 server 不受影响
        assert_eq!(
            t.on_event(ch, "other", "x.md", now).0,
            ResourceDecision::Allow
        );
        // 窗口过去后恢复
        let later = now + RESOURCE_EVENT_MIN_INTERVAL;
        assert_eq!(
            t.on_event(ch, "chatty", "rotating-100.md", later).0,
            ResourceDecision::Allow,
            "新窗口应放行"
        );
    }

    /// 丢弃**必须可观测**（评审 F1 阻塞项）：即时行带上 key/原因/窗口值；窗口过后出汇总；
    /// 即时行受预算约束（超出只计数并在汇总里报告）⇒ 日志量与负载无关。
    /// 判别力：把即时行改回 `tracing::*`/删掉 ⇒ 本条红（`crate::log!` 才是生产可见路径）。
    #[test]
    fn resource_throttle_logs_first_drop_and_window_summary() {
        let now = Instant::now();
        let ch = Uuid::new_v4();
        let mut t = ResourceThrottle::new(now);
        // 建立窗口：第一条放行并记账，随后同键丢弃 ⇒ 必须给一行可读日志
        assert_eq!(
            t.on_event(ch, "notes", "a.md", now).0,
            ResourceDecision::Allow
        );
        t.mark_delivered(ch, "notes", "a.md", now);
        let (d, lines) = t.on_event(ch, "notes", "a.md", now);
        assert_eq!(d, ResourceDecision::DropByKey);
        assert_eq!(
            lines.len(),
            1,
            "首次丢弃必须输出一行（不得静默）: {lines:?}"
        );
        let line = &lines[0];
        for needle in ["节流丢弃", "notes", "a.md", "10s"] {
            assert!(line.contains(needle), "丢弃行缺 {needle}: {line}");
        }
        // 预算：制造大量"不同 key（不同 server）"的丢弃，即时行不得超预算
        let mut eager = 1usize; // 上面已用掉 1 行
        for i in 0..RESOURCE_THROTTLE_LOG_BUDGET * 2 {
            let server = format!("s{i}");
            assert_eq!(
                t.on_event(ch, &server, "a.md", now).0,
                ResourceDecision::Allow
            );
            t.mark_delivered(ch, &server, "a.md", now);
            let (_, l2) = t.on_event(ch, &server, "a.md", now);
            eager += l2.len();
        }
        assert_eq!(
            eager, RESOURCE_THROTTLE_LOG_BUDGET as usize,
            "即时行必须恰好用满预算后停止（不随负载增长）"
        );
        // 窗口过后：一条汇总，含放行/丢弃总数与"被预算压掉"的计数
        let later = now + RESOURCE_EVENT_MIN_INTERVAL;
        let (_, lines) = t.on_event(ch, "fresh", "z.md", later);
        assert_eq!(lines.len(), 1, "窗口过后应有且仅有一条汇总: {lines:?}");
        let summary = &lines[0];
        for needle in ["窗口汇总", "放行", "丢弃", "因日志预算未逐条打印"] {
            assert!(summary.contains(needle), "汇总行缺 {needle}: {summary}");
        }
        // 无事件的下一个窗口：不再刷空汇总
        let (_, lines) = t.on_event(ch, "fresh", "z.md", later + RESOURCE_EVENT_MIN_INTERVAL);
        assert!(
            lines.is_empty(),
            "窗口内没有丢弃/放行时不该刷空汇总: {lines:?}"
        );
    }

    /// 状态**有界**（评审 F2）：过期条目被 GC 掉（语义等价 ⇒ 判定不变），频道移除时整频道清空。
    #[test]
    fn resource_throttle_state_is_bounded_and_forgets_channel() {
        let now = Instant::now();
        let ch = Uuid::new_v4();
        let mut t = ResourceThrottle::new(now);
        for i in 0..50 {
            let uri = format!("u{i}.md");
            // 每个 uri 单独一个 server，避免撞上 per-server 上限
            let server = format!("s{i}");
            assert_eq!(
                t.on_event(ch, &server, &uri, now).0,
                ResourceDecision::Allow
            );
            t.mark_delivered(ch, &server, &uri, now);
        }
        assert_eq!(t.last_sent.len(), 50);
        assert_eq!(t.per_server.len(), 50);
        // GC：窗口过后全部清掉（"过期 == 不存在"，故判定语义不变）
        t.gc(now + RESOURCE_EVENT_MIN_INTERVAL);
        assert!(
            t.last_sent.is_empty(),
            "过期条目必须被清掉（否则长驻服务无界增长）"
        );
        assert!(t.per_server.is_empty());
        assert_eq!(
            t.on_event(ch, "s0", "u0.md", now + RESOURCE_EVENT_MIN_INTERVAL)
                .0,
            ResourceDecision::Allow,
            "GC 后同一键应重新放行（窗口本就过期）"
        );
        t.mark_delivered(ch, "s0", "u0.md", now + RESOURCE_EVENT_MIN_INTERVAL);
        // 频道移除：该频道状态清空，别的频道不受影响
        let other = Uuid::new_v4();
        t.mark_delivered(other, "s0", "u0.md", now + RESOURCE_EVENT_MIN_INTERVAL);
        t.forget_channel(ch);
        assert!(
            t.last_sent.iter().all(|((c, _, _), _)| *c != ch),
            "频道移除后不得残留该频道状态"
        );
        assert!(
            t.last_sent.iter().any(|((c, _, _), _)| *c == other),
            "别的频道状态不得被误清"
        );
    }

    /// 记账只在**真入队之后**（评审 F3）：`on_event` 单独调用不占用窗口。
    /// 判别力：把 `mark_delivered` 里的记账搬回 `on_event` ⇒ 第一条断言红。
    #[test]
    fn resource_throttle_arms_window_only_after_delivered() {
        let now = Instant::now();
        let ch = Uuid::new_v4();
        let mut t = ResourceThrottle::new(now);
        for _ in 0..5 {
            assert_eq!(
                t.on_event(ch, "notes", "a.md", now).0,
                ResourceDecision::Allow,
                "未 mark_delivered（= 队列策略拒收）时不得占用窗口"
            );
        }
        assert!(t.last_sent.is_empty(), "未入队不得写 last_sent");
        t.mark_delivered(ch, "notes", "a.md", now);
        assert_eq!(
            t.on_event(ch, "notes", "a.md", now).0,
            ResourceDecision::DropByKey,
            "入队后才开始占用窗口"
        );
    }

    /// 节流判定（纯函数）：首次放行；窗口内丢弃；窗口边界起放行。
    #[test]
    fn resource_event_allowed_window_semantics() {
        let now = Instant::now();
        let min = Duration::from_secs(10);
        assert!(resource_event_allowed(None, now, min), "首次必须放行");
        assert!(
            !resource_event_allowed(Some(now - Duration::from_millis(9_999)), now, min),
            "窗口内必须丢弃"
        );
        assert!(
            resource_event_allowed(Some(now - min), now, min),
            "到窗口即放行（边界含等号）"
        );
        assert!(resource_event_allowed(
            Some(now - min - Duration::from_secs(1)),
            now,
            min
        ));
    }

    /// **阳性对照**：窗口之外的更新仍要开回合（节流不得把真实变化误杀）。
    ///
    /// 用**真实窗口**（10s）跑：第一轮投递一条 → 等过一个窗口 → 第二条必须再投递一次。
    /// 这条与 `resource_event_burst_collapses_to_one_turn` 一起钉住"窗口内合并、窗口外放行"。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn resource_event_allows_again_after_window() {
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let rec = std::env::temp_dir().join(format!("mock-throttle2-{}.jsonl", Uuid::new_v4()));
        let stop = CancellationToken::new();
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: python3,
                args: vec![script.display().to_string()],
                extra_env: vec![
                    ("PATH".to_string(), crate::deps::composed_path()),
                    ("MOCK_RECORD_FILE".to_string(), rec.display().to_string()),
                    ("MOCK_RESOURCE_UPDATE".to_string(), "slow-body".to_string()),
                    ("MOCK_RESOURCE_SERVER".to_string(), "slowmcp".to_string()),
                    ("MOCK_RESOURCE_URI".to_string(), "mock://slow".to_string()),
                ],
                backend: "mock".to_string(),
                session_sandbox: None,
            },
            stop.clone(),
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
            false,
        );
        let run = tokio::spawn(run_loop(handle.clone()));
        let cid = Uuid::new_v4();
        handle.upsert_channel(
            cid,
            ChannelMeta {
                bot_key: "throttle2".to_string(),
                chat_id: "oc_throttle2".to_string(),
                chat_type: "p2p".to_string(),
                thread_id: None,
                name: "throttle2".to_string(),
                anchor_mid: None,
                adhoc: true,
                workspace: None,
            },
        );
        let count_delivered = |rec: &std::path::Path| {
            std::fs::read_to_string(rec)
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains("\"event\": \"prompt\"") && l.contains("[MCP 资源更新]"))
                .count()
        };
        assert!(handle.push_message(
            cid,
            InboundMsg {
                id_hex: "s-1".to_string(),
                author_role: "user".to_string(),
                text: "第一轮".to_string(),
                ts_secs: 0,
                prompt_tag: "test".to_string(),
                bridge_generation: String::new(),
            },
        ));
        // 等第一条投递
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && count_delivered(&rec) == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(count_delivered(&rec) >= 1, "第一轮必须先投递一条");
        // 等过一个节流窗口（生产 10s）+ 余量，再推第二轮
        tokio::time::sleep(RESOURCE_EVENT_MIN_INTERVAL + Duration::from_secs(1)).await;
        assert!(handle.push_message(
            cid,
            InboundMsg {
                id_hex: "s-2".to_string(),
                author_role: "user".to_string(),
                text: "第二轮".to_string(),
                ts_secs: 0,
                prompt_tag: "test".to_string(),
                bridge_generation: String::new(),
            },
        ));
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && count_delivered(&rec) < 2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), run).await;
        let delivered = count_delivered(&rec);
        assert!(
            delivered >= 2,
            "窗口之外的更新必须再投递（否则节流把真实变化误杀）；实际 {delivered}"
        );
        let _ = std::fs::remove_file(&rec);
    }

    /// **节流的核心判据**（card `abb-mcp-resource-throttle-20261011`）：同一 `(channel,server,uri)`
    /// 在一个回合内连发 3 条**内容各异**的更新 ⇒ 只有第一条开回合，其余 2 条被丢弃并留日志。
    ///
    /// 为什么判据落在假 agent 的记录上：它逐次 prompt 都记 `resource_notify_sent`（发了 3 条），
    /// 而"带标记的 prompt 条数"只能来自 ABB 的投递 —— 只数这一侧才能区分"发得多"与"投得少"。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn resource_event_burst_collapses_to_one_turn() {
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let rec = std::env::temp_dir().join(format!("mock-throttle-{}.jsonl", Uuid::new_v4()));
        let stop = CancellationToken::new();
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: python3,
                args: vec![script.display().to_string()],
                extra_env: vec![
                    ("PATH".to_string(), crate::deps::composed_path()),
                    ("MOCK_RECORD_FILE".to_string(), rec.display().to_string()),
                    ("MOCK_RESOURCE_UPDATE".to_string(), "burst-body".to_string()),
                    ("MOCK_RESOURCE_BURST".to_string(), "3".to_string()),
                    (
                        "MOCK_RESOURCE_SERVER".to_string(),
                        "throttlemcp".to_string(),
                    ),
                    (
                        "MOCK_RESOURCE_URI".to_string(),
                        "mock://throttle".to_string(),
                    ),
                ],
                backend: "mock".to_string(),
                session_sandbox: None,
            },
            stop.clone(),
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
            false,
        );
        let run = tokio::spawn(run_loop(handle.clone()));
        let cid = Uuid::new_v4();
        handle.upsert_channel(
            cid,
            ChannelMeta {
                bot_key: "throttle".to_string(),
                chat_id: "oc_throttle".to_string(),
                chat_type: "p2p".to_string(),
                thread_id: None,
                name: "throttle".to_string(),
                anchor_mid: None,
                adhoc: true,
                workspace: None,
            },
        );
        assert!(handle.push_message(
            cid,
            InboundMsg {
                id_hex: "t-1".to_string(),
                author_role: "user".to_string(),
                text: "跑一轮".to_string(),
                ts_secs: 0,
                prompt_tag: "test".to_string(),
                bridge_generation: String::new(),
            },
        ));
        // 等 3 条通知都发出去（假 agent 侧记录），再给 ABB 一点投递时间
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut sent = 0usize;
        while Instant::now() < deadline {
            let log = std::fs::read_to_string(&rec).unwrap_or_default();
            sent = log.matches("resource_notify_sent").count();
            if sent >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // 注意：夹具是"每个 prompt 都发一轮 burst"，而本轮至少有两个 prompt（用户消息 + 资源事件回合）
        // ⇒ sent 会是 3 的倍数。这里只要求"至少发过一轮"，真正的判据是下面 delivered==1。
        assert!(sent >= 3, "夹具应至少发出 3 条资源更新，实际 {sent}");
        tokio::time::sleep(Duration::from_millis(800)).await; // 让被放行的那条完成入队/开回合
        stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), run).await;
        let log = std::fs::read_to_string(&rec).unwrap_or_default();
        let delivered = log
            .lines()
            .filter(|l| l.contains("\"event\": \"prompt\"") && l.contains("[MCP 资源更新]"))
            .count();
        assert_eq!(
            delivered, 1,
            "3 条同资源更新在节流窗口内只应投递 1 条；实际 {delivered}。记录：\n{log}"
        );
        let _ = std::fs::remove_file(&rec);
    }

    /// 事件消息必须能像用户消息一样驱动一个完整回合（走同一条入队路径 ⇒ 语义由构造保证一致）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn resource_event_message_runs_a_turn() {
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let stop = CancellationToken::new();
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: python3,
                args: vec![script.display().to_string()],
                extra_env: vec![("PATH".to_string(), crate::deps::composed_path())],
                backend: "mock".to_string(),
                session_sandbox: None,
            },
            stop.clone(),
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
            false,
        );
        let run = tokio::spawn(run_loop(handle.clone()));
        let cid = Uuid::new_v4();
        handle.upsert_channel(
            cid,
            ChannelMeta {
                bot_key: "res".to_string(),
                chat_id: "oc_res".to_string(),
                chat_type: "p2p".to_string(),
                thread_id: None,
                name: "event".to_string(),
                anchor_mid: None,
                adhoc: true,
                workspace: None,
            },
        );
        let outcome = handle
            .wait_turn_outcome(
                cid,
                resource_update_message(&update("资源内容")),
                std::time::Duration::from_secs(30),
            )
            .await;
        stop.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), run).await;
        assert!(
            matches!(outcome, SyncTurnOutcome::Ok(_)),
            "事件回合应收尾 Ok，实际 {outcome:?}"
        );
    }
}

#[cfg(test)]
mod spawn_lifecycle_tests {
    use super::*;

    /// `mcp_from_channel` 开关必须真的进 `PromptContext`（normal=true / granted=false）。
    /// 名单本身不在句柄上——它在会话建立时按渠道目录现读（下面这条 e2e）。
    #[test]
    fn mcp_from_channel_flag_reaches_prompt_context() {
        let mk = |flag: bool| {
            RuntimeHandle::new(
                AgentConfig {
                    command: "true".to_string(),
                    args: Vec::new(),
                    extra_env: Vec::new(),
                    backend: "test".to_string(),
                    session_sandbox: None,
                },
                CancellationToken::new(),
                ".".to_string(),
                flag,
            )
        };
        assert!(mk(true).mcp_from_channel());
        assert!(!mk(false).mcp_from_channel());
    }

    /// e2e smoke：**渠道目录里的 `mcp.json`** 必须出现在真实 ACP session/new 的
    /// mcpServers 载荷里（卡 abb-channel-mcp-config-20261012 的核心接线）。
    /// 用固定夹具（不依赖随包二进制），任何环境都会真跑。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn channel_mcp_json_reaches_session_new_payload() {
        // 渠道工作目录 + 其中的 mcp.json（就是用户在会话里让 bot 写的那份）。
        let channel_dir =
            std::env::temp_dir().join(format!("abb-channel-mcp-e2e-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&channel_dir).unwrap();
        std::fs::write(
            channel_dir.join("mcp.json"),
            r#"[{"name":"extra-dummy","command":"true","args":["--flag"]}]"#,
        )
        .unwrap();

        let rec =
            std::env::temp_dir().join(format!("abb-channel-mcp-rec-{}.jsonl", Uuid::new_v4()));
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let stop = CancellationToken::new();
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: python3,
                args: vec![script.display().to_string()],
                extra_env: vec![
                    ("PATH".to_string(), crate::deps::composed_path()),
                    ("MOCK_RECORD_FILE".to_string(), rec.display().to_string()),
                ],
                backend: "mock".to_string(),
                session_sandbox: None,
            },
            stop.clone(),
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
            true,
        );
        let run = tokio::spawn(super::run_loop(handle.clone()));
        let cid = Uuid::new_v4();
        handle.upsert_channel(
            cid,
            ChannelMeta {
                bot_key: "wst".to_string(),
                chat_id: "oc_x".to_string(),
                chat_type: "p2p".to_string(),
                thread_id: None,
                name: "e2e".to_string(),
                anchor_mid: None,
                adhoc: true,
                // 会话 cwd = 该渠道目录 ⇒ mcp.json 就从这里读。
                workspace: Some(channel_dir.display().to_string()),
            },
        );
        let outcome = handle
            .wait_turn_outcome(
                cid,
                crate::runtime::queue::InboundMsg {
                    id_hex: "m1".to_string(),
                    author_role: "owner".to_string(),
                    text: "hi".to_string(),
                    ts_secs: 0,
                    prompt_tag: "test".to_string(),
                    bridge_generation: String::new(),
                },
                std::time::Duration::from_secs(30),
            )
            .await;
        stop.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), run).await;
        assert!(
            matches!(outcome, SyncTurnOutcome::Ok(_)),
            "回合应收尾 Ok，实际 {outcome:?}"
        );
        let log = std::fs::read_to_string(&rec).expect("record file 可读");
        let session_new = log
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["event"] == "session_new")
            .expect("必须有一次 session/new");
        // 会话 cwd 必须就是渠道目录（e2e 顺带钉住 P1 的 cwd 语义）
        assert_eq!(
            session_new["cwd"].as_str(),
            Some(channel_dir.display().to_string().as_str())
        );
        let servers = session_new["mcpServers"]
            .as_array()
            .expect("mcpServers 为数组");
        let extra = servers
            .iter()
            .find(|s| s["name"] == "extra-dummy")
            .expect("mcpServers 必须含渠道 mcp.json 里的 extra-dummy");
        assert_eq!(extra["args"], serde_json::json!(["--flag"]));
        let _ = std::fs::remove_file(&rec);
        let _ = std::fs::remove_dir_all(&channel_dir);
    }

    /// e2e：改渠道目录里的 `mcp.json` → **该渠道**会话必须重建（下一次消息重新
    /// `session/new`，把新名单发出去），且**不影响其它渠道**会话（卡 AC5）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn channel_mcp_change_rebuilds_only_that_channel() {
        let mk_dir = |tag: &str, server: &str| {
            let dir =
                std::env::temp_dir().join(format!("abb-channel-mcp-{tag}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("mcp.json"),
                format!(r#"[{{"name":"{server}","command":"true"}}]"#),
            )
            .unwrap();
            dir
        };
        let dir_a = mk_dir("rebuild-a", "first-server");
        let dir_b = mk_dir("rebuild-b", "other-server");
        let rec =
            std::env::temp_dir().join(format!("abb-chmcp-rebuild-rec-{}.jsonl", Uuid::new_v4()));
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let stop = CancellationToken::new();
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: python3,
                args: vec![script.display().to_string()],
                extra_env: vec![
                    ("PATH".to_string(), crate::deps::composed_path()),
                    ("MOCK_RECORD_FILE".to_string(), rec.display().to_string()),
                ],
                backend: "mock".to_string(),
                session_sandbox: None,
            },
            stop.clone(),
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
            true,
        );
        let run = tokio::spawn(super::run_loop(handle.clone()));
        let channel_a = Uuid::new_v4();
        let channel_b = Uuid::new_v4();
        for (cid, chat, ws) in [
            (channel_a, "oc_rebuild_a", &dir_a),
            (channel_b, "oc_rebuild_b", &dir_b),
        ] {
            handle.upsert_channel(
                cid,
                ChannelMeta {
                    bot_key: "wst".to_string(),
                    chat_id: chat.to_string(),
                    chat_type: "p2p".to_string(),
                    thread_id: None,
                    name: "e2e".to_string(),
                    anchor_mid: None,
                    adhoc: true,
                    workspace: Some(ws.display().to_string()),
                },
            );
        }
        let turn = |cid: Uuid, id: &str| {
            handle.wait_turn_outcome(
                cid,
                crate::runtime::queue::InboundMsg {
                    id_hex: id.to_string(),
                    author_role: "owner".to_string(),
                    text: "hi".to_string(),
                    ts_secs: 0,
                    prompt_tag: "test".to_string(),
                    bridge_generation: String::new(),
                },
                std::time::Duration::from_secs(30),
            )
        };
        assert!(matches!(
            turn(channel_a, "a1").await,
            SyncTurnOutcome::Ok(_)
        ));
        assert!(matches!(
            turn(channel_b, "b1").await,
            SyncTurnOutcome::Ok(_)
        ));

        // 只改 A 的配置
        std::fs::write(
            dir_a.join("mcp.json"),
            r#"[{"name":"second-server","command":"true"}]"#,
        )
        .unwrap();
        assert!(matches!(
            turn(channel_a, "a2").await,
            SyncTurnOutcome::Ok(_)
        ));
        stop.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), run).await;

        let news: Vec<serde_json::Value> = std::fs::read_to_string(&rec)
            .expect("record file 可读")
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["event"] == "session_new")
            .collect();
        let count_for = |dir: &std::path::Path| {
            let want = serde_json::json!(dir.display().to_string());
            news.iter().filter(|v| v["cwd"] == want).count()
        };
        assert!(
            count_for(&dir_a) >= 2,
            "A 改了配置必须重建（≥2 次 session/new），实际 {}",
            count_for(&dir_a)
        );
        assert_eq!(
            count_for(&dir_b),
            1,
            "B 的配置没动，不得被重建（变更只影响对应渠道）"
        );
        let want_a = serde_json::json!(dir_a.display().to_string());
        let last_a = news.iter().rev().find(|v| v["cwd"] == want_a).unwrap();
        let names: Vec<String> = last_a["mcpServers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["name"].as_str().map(str::to_string))
            .collect();
        assert!(
            names.contains(&"second-server".to_string()),
            "重建后的会话必须带新名单，实际 {names:?}"
        );
        let _ = std::fs::remove_file(&rec);
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn agent_start_failure_is_emitted_with_spawn_detail() {
        let mut out = Vec::new();
        emit_agent_start_failure(
            &mut out,
            "failed to spawn agent: program not found",
            Duration::from_secs(4),
        );
        let line = String::from_utf8(out).unwrap();
        let (timestamp, message) = line.split_once("] ").unwrap();
        assert!(timestamp.starts_with('['), "缺日志时间戳: {line:?}");
        assert_eq!(
            message,
            "[acp] agent start failed — retrying in 4s: failed to spawn agent: program not found\n"
        );
    }
}

#[cfg(test)]
mod channel_info_tests {
    use super::*;

    fn meta(chat_type: &str, workspace: Option<&str>) -> ChannelMeta {
        ChannelMeta {
            bot_key: "bot".into(),
            chat_id: "oc_x".into(),
            chat_type: chat_type.into(),
            thread_id: None,
            name: "名字".into(),
            anchor_mid: None,
            adhoc: false,
            workspace: workspace.map(str::to_string),
        }
    }

    #[test]
    fn workspace_maps_through_to_prompt_info() {
        // P0.B：workspace 随 channel_info 贯穿（p2p 的 dm 语义映射同时锁住）
        let info = meta("p2p", Some("/ws/vb/uuid-1")).channel_info();
        assert_eq!(info.channel_type, "dm");
        assert_eq!(info.workspace.as_deref(), Some("/ws/vb/uuid-1"));
        assert_eq!(meta("group", None).channel_info().workspace, None);
    }

    /// P3.1：主循环已退出（cmd 接收端随 run_loop 关闭）时，同步回合等待立即
    /// 得 Closed——不挂预算、不留等待者。纯单测（不起 agent、不跑 run_loop）。
    #[tokio::test]
    async fn wait_turn_outcome_closed_when_loop_gone() {
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: "true".to_string(),
                args: Vec::new(),
                extra_env: Vec::new(),
                backend: "test".to_string(),
                session_sandbox: None,
            },
            CancellationToken::new(),
            ".".to_string(),
            false,
        );
        // 模拟 run_loop 曾取走接收端后退出（生产上 Closed 只在此情形发生）。
        drop(handle.take_cmd_rx());
        let outcome = handle
            .wait_turn_outcome(
                Uuid::new_v4(),
                crate::runtime::queue::InboundMsg {
                    id_hex: "m1".to_string(),
                    author_role: "owner".to_string(),
                    text: "x".to_string(),
                    ts_secs: 0,
                    prompt_tag: "test".to_string(),
                    bridge_generation: String::new(),
                },
                std::time::Duration::from_secs(60),
            )
            .await;
        assert_eq!(outcome, SyncTurnOutcome::Closed);
        assert!(handle.sync_waiters.lock().unwrap().is_empty());
    }

    /// #309 PR-A1：丢弃式取消的 waiter 回传必须映射成 `Cancelled` **终态**——
    /// 不是 Timeout，也不等重提示后的文本。纯单测：直接拨 sync_waiters 表。
    #[tokio::test]
    async fn sync_wait_cancelled_maps_to_cancelled_outcome() {
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: "true".to_string(),
                args: Vec::new(),
                extra_env: Vec::new(),
                backend: "test".to_string(),
                session_sandbox: None,
            },
            CancellationToken::new(),
            ".".to_string(),
            false,
        );
        let _cmd_rx = handle.take_cmd_rx();
        let channel_id = Uuid::new_v4();
        let waiter = {
            let h = handle.clone();
            tokio::spawn(async move {
                h.wait_turn_outcome(
                    channel_id,
                    crate::runtime::queue::InboundMsg {
                        id_hex: "m1".to_string(),
                        author_role: "owner".to_string(),
                        text: "x".to_string(),
                        ts_secs: 0,
                        prompt_tag: "test".to_string(),
                        bridge_generation: String::new(),
                    },
                    std::time::Duration::from_secs(3600),
                )
                .await
            })
        };
        // 等等待者注册，再模拟丢弃式取消的回传
        let mut waited_ms = 0u64;
        let tx = loop {
            if let Some(tx) = handle.sync_waiters.lock().unwrap().remove(&channel_id) {
                break tx;
            }
            assert!(waited_ms < 5000, "等待者未注册");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            waited_ms += 10;
        };
        let started = std::time::Instant::now();
        tx.send(SyncWaitMsg::Cancelled).unwrap();
        let outcome = waiter.await.unwrap();
        assert_eq!(
            outcome,
            SyncTurnOutcome::Cancelled,
            "丢弃式取消必须给出 Cancelled 终态"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "必须是立刻返回，而不是挂到 timeout"
        );
        assert!(
            !handle
                .sync_waiters
                .lock()
                .unwrap()
                .contains_key(&channel_id),
            "终态后不得残留 waiter 条目"
        );
    }

    /// #309 PR-A1：取消终态决策（纯函数）——覆盖「收到过取消 × 批次是否被重提示」
    /// 四种组合。这是 A1 的核心契约：取消失败类结局（CancelDrainTimeout /
    /// AgentExited / Error / Timeout）只要没被重提示，也必须给等待方终态，
    /// 不能让它落回「挂到 timeout」。
    #[test]
    fn cancel_waiter_terminal_covers_all_cases() {
        // 取消失败类结局（batch=None）必须都给终态，不能漏成挂 timeout
        for outcome in [
            PromptOutcome::Cancelled,
            PromptOutcome::CancelDrainTimeout(std::time::Duration::from_secs(5)),
            PromptOutcome::AgentExited,
            PromptOutcome::Timeout(TimeoutKind::Idle),
            PromptOutcome::Error(AcpError::Protocol("boom".to_string())),
        ] {
            assert_eq!(
                cancel_waiter_terminal(true, false, &outcome),
                Some(SyncWaitMsg::Cancelled),
                "收到取消且该轮无产出 → 必须给 Cancelled 终态"
            );
        }
        // 自然完成优先：真的产出了文本就交给等待方，别谎报取消
        // （否则会出现「报了 Cancelled 但文本仍进聊天」的不一致）
        assert_eq!(
            cancel_waiter_terminal(
                true,
                false,
                &PromptOutcome::Ok(crate::runtime::acp::StopReason::EndTurn)
            ),
            None,
            "Ok 结局 → 不发取消终态，让等待方拿真实文本"
        );
        // 批次被合并重提示 → 等重提示后的文本
        assert_eq!(
            cancel_waiter_terminal(true, true, &PromptOutcome::Cancelled),
            None
        );
        // 没收到取消 → 不动 waiter
        assert_eq!(
            cancel_waiter_terminal(false, false, &PromptOutcome::Cancelled),
            None
        );
    }

    /// #309 PR-A1：把终态投给该频道 waiter——有 waiter 时送达且不残留；
    /// 没有 waiter 时是 no-op（不误伤、不 panic）。
    #[tokio::test]
    async fn deliver_sync_wait_msg_sets_terminal_state() {
        let handle = RuntimeHandle::new(
            AgentConfig {
                command: "true".to_string(),
                args: Vec::new(),
                extra_env: Vec::new(),
                backend: "test".to_string(),
                session_sandbox: None,
            },
            CancellationToken::new(),
            ".".to_string(),
            false,
        );
        let channel_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SyncWaitMsg>();
        handle.sync_waiters.lock().unwrap().insert(channel_id, tx);

        assert!(
            deliver_sync_wait_msg(&handle, channel_id, SyncWaitMsg::Cancelled),
            "有 waiter 时必须送达"
        );
        assert_eq!(
            rx.try_recv(),
            Ok(SyncWaitMsg::Cancelled),
            "waiter 必须收到 Cancelled 终态"
        );
        assert!(
            !handle
                .sync_waiters
                .lock()
                .unwrap()
                .contains_key(&channel_id),
            "送达后不得残留 waiter 条目（残留会吞掉后续聊天回复）"
        );
        assert!(
            !deliver_sync_wait_msg(&handle, channel_id, SyncWaitMsg::Cancelled),
            "没有 waiter 时是 no-op"
        );
    }
}

#[cfg(test)]
mod redact_tests {
    use super::redact_skill_paths;

    #[test]
    fn rewrites_skill_path_lines_to_names() {
        let text = "pi v0.84.1\n\n## Skills\n- /Users/sqb/.agents/skills/wx-send/SKILL.md\n* /Users/sqb/.pi/skills/asr/SKILL.md\n/Users/sqb/.agents/skills/meegle/SKILL.md\n\n我有以下技能：";
        let out = redact_skill_paths(text);
        assert!(out.contains("- wx-send"));
        assert!(out.contains("* asr"));
        assert!(out.contains("\nmeegle\n"));
        assert!(!out.contains("/SKILL.md"));
        assert!(out.contains("## Skills"));
    }

    #[test]
    fn leaves_narrative_path_mentions_alone() {
        let text = "我修改了 /Users/sqb/.agents/skills/foo/SKILL.md 这个文件";
        assert_eq!(redact_skill_paths(text), text);
    }

    #[test]
    fn skips_non_identifier_names_and_relative_paths() {
        // 相对路径不改写（不是系统提示的泄漏形态）
        let rel = "- ./skills/foo/SKILL.md";
        assert_eq!(redact_skill_paths(rel), rel);
        // 技能名带空格（异常形态）保守跳过
        let weird = "- /Users/x/skills/my skill/SKILL.md";
        assert_eq!(redact_skill_paths(weird), weird);
    }
}

/// #7538（上游 051c3a270「report missing models without retrying」）的判据回归。
///
/// 为什么值得单测：这条判据决定一个批次是**当场死信**还是**继续重排**，而两者只差
/// `-32002` 里的一句 message。上游为「同码不同因」专门加了回归，ABB 侧同样要钉住——
/// 误判会把本可自愈的 session 过期类错误直接判死。
#[cfg(test)]
mod model_not_found_tests {
    use super::*;

    /// 模型缺失：-32002 + model not found ⇒ 不可重试。
    #[test]
    fn matches_missing_model() {
        assert!(is_model_not_found(&AcpError::AgentError {
            code: -32002,
            message: "llm model not found: gpt-x".to_string(),
        }));
    }

    /// 同码不同因（会话过期等其它 resource-not-found）**仍可重排**；
    /// 码不同（-32000，如用量/额度类）也不归这条管。
    #[test]
    fn other_resource_not_found_is_still_retryable() {
        assert!(!is_model_not_found(&AcpError::AgentError {
            code: -32002,
            message: "Resource not found: session no longer exists".to_string(),
        }));
        assert!(!is_model_not_found(&AcpError::AgentError {
            code: -32000,
            message: "model not found".to_string(),
        }));
    }
}

#[cfg(test)]
mod isolation_capacity_tests {
    use super::*;

    /// 会话级隔离的容量策略：至少 1（热实例），**不设并发上限** —— 有几个待跑会话就几个 agent。
    ///
    /// 资源边界改由「空闲回收」表达（见下一条测试）：会话不常驻，跑完闲置就收，
    /// 稳态进程数 = 正在跑的会话数。
    #[test]
    fn wanted_slots_is_elastic_with_one_warm() {
        assert_eq!(
            wanted_slots(0),
            1,
            "空闲也要留一个热 agent，避免每条消息冷启动"
        );
        assert_eq!(wanted_slots(1), 1);
        assert_eq!(
            wanted_slots(3),
            3,
            "三个待跑会话就该有三个 agent（互不排队）"
        );
        assert_eq!(wanted_slots(9), 9, "第 9 个会话不该排队：不限并发");
        assert_eq!(wanted_slots(50), 50, "没有任何人为上限");
    }

    /// 空闲回收策略：0 号热实例永不回收；其余只有**空闲超时**才收；在途（None）绝不动。
    #[test]
    fn idle_reap_only_touches_stale_extra_slots() {
        let stale = Some(IDLE_REAP_AFTER + Duration::from_secs(1));
        let fresh = Some(Duration::from_secs(1));
        assert!(!should_reap_slot(0, stale), "0 号是热实例，永不回收");
        assert!(
            !should_reap_slot(3, fresh),
            "刚跑完的额外槽位不收（还要接着服务它的会话）"
        );
        assert!(
            !should_reap_slot(3, None),
            "在途/启动中（无闲置起点）绝不动"
        );
        assert!(should_reap_slot(3, stale), "闲置超时的额外槽位该收");
    }
}
