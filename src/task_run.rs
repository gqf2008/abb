//! 任务执行（#326 / `docs/task-model.md` P2b/P3 的 agent 与 proc 载荷落地）。
//!
//! ## 为什么直接复用 `buzz::oneshot`
//!
//! Q7 拍板的是 **B1：task 用独立 handle/pool**（不与聊天共用单 slot）。`oneshot`
//! 正好就是这件东西——自起 handle + `run_loop`，自带频道、自带取消、用完即弃：
//!
//! - **不占聊天 slot**：任务在**自己的** handle 上跑，聊天句柄的队列/slot 完全不受影响。
//!   所以「后台任务跑着、聊天照常回」是结构上成立，不是靠约定或超时。
//! - **不污染聊天会话**：频道路由是 `Uuid::new_v4()`（oneshot 内建），不落
//!   `channel_uuid(bot, chat_id)`，因此不会把 `sessions.json` / ACP 会话槽位
//!   和聊天搅在一起（D1 的隔离要求）。
//! - **真取消**：`external_cancel` 触发走 oneshot 的 Timeout 同款拆栈（cancel 回执 +
//!   杀进程组），不是「置个标志然后假装停了」——D1c 要求的就是这条。Q3 的
//!   `task cancel` 用「关停令牌 ∪ 取消请求文件」的合并令牌接进同一条通路；取消后
//!   **不再投递结果**（与「服务关停联动取消」同口径）。
//!
//! ## 载荷路径
//!
//! `agent` 走上面的 `buzz::oneshot`；`proc` 走 [`crate::task_proc`] 的 Unix 进程组
//! 超管（流式日志、退出码、超时/取消整组停止）。Windows Job Object 尚未接入，proc
//! 在登记与执行两处显式拒绝，不静默降级。
//!
//! ## 投递语义
//!
//! 结果默认回**创建者会话**（D2）。信封按「发给当前会话」构造：`source == target`
//! 且 `in_session = true`——正是 CLI `--to-current` 的既有语义，Router 侧
//! `in_session_ok = in_session && is_self_loop(item)` 的判据**一个字没改**
//! （绝不用「地址相等即豁免」去削弱防循环，见 D2 的警告）。

use std::sync::Arc;
use std::time::Duration;

use crate::deliver::{DeliveryItem, DeliveryOrigin, Router};
use crate::task_identity::IdentityStatus;
use crate::task_store::{
    PayloadKind, Task, TaskRuntime, TaskStateKind, TaskStateStore, TaskStore, TriggerKind,
    KEEPALIVE_MAX_CONSECUTIVE_FAILURES, LOG_KEEP_FILES,
};

/// 任务提示的 `prompt_tag`。与 `schedule::JOB_PROMPT_TAG` 分开：任务不是定时任务，
/// 停止词/排队丢弃规则将来若分叉，共用一个标会静默串味。
pub const TASK_PROMPT_TAG: &str = "task_message";

/// 轮询间隔：worker 每这么久看一眼有没有待认领的任务。
/// `trigger=now` 的任务是「登记后立刻跑」，所以这是**登记到开跑**的延迟上限。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// `task cancel` 请求的轮询间隔：CLI 只投一个请求文件，worker 在**运行中**也要定期
/// 看它一眼才能真把取消送进 oneshot。
const CANCEL_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 用户主动取消的落盘原因（`task status` 展示 + 日志留痕）。
const CANCEL_REASON: &str = "已取消（用户 task cancel）";

/// service 关停联动取消的落盘原因。与 [`CANCEL_REASON`] 分开：两者都走
/// `SyncTurnOutcome::Cancelled`，但「谁取消的」是排障首先要看的信息（审查指出
/// 关停被记成「用户 task cancel」是错的）。
const CANCEL_REASON_SHUTDOWN: &str = "已取消（service 关停）";

/// 尚未开跑就被取消的落盘原因。
const CANCEL_REASON_BEFORE_START: &str = "已取消（task cancel，尚未开跑）";

/// keepalive 退出后的退避序列（秒）。连续失败越久退避越长，避免崩溃风暴。
const KEEPALIVE_BACKOFF_SECS: &[u64] = &[1, 2, 4, 8, 30];
/// keepalive 进程至少稳定运行这么久才把退出视为健康；短命退出即使是 0 也计入熔断，
/// 防止“启动即退出”的配置把 worker 变成忙循环。
const KEEPALIVE_STABLE_SECS: u64 = 10;

/// 终态任务的日志保留期（天）。Q4 只约束单文件大小，这里补一个**时间**上界，
/// 免得「跑过一次就再没人看」的任务把日志目录长期堆着。
const LOG_RETENTION_DAYS: u64 = 30;

/// 日志回收扫描间隔（秒）：worker 每 2s 轮询任务，日志回收每小时一次就够。
const LOG_GC_INTERVAL_SECS: u64 = 3600;

/// 逾期认领的记日志阈值（秒）：认领时刻比「应到时刻」晚这么多才写一行，
/// 免得把 2s 轮询的正常抖动也当成逾期刷日志。
const OVERDUE_LOG_SECS: u64 = 60;

/// cron 逾期认领的扫描窗口（秒）。
///
/// 执行位被长任务占住而错过的分钟点，只要还在这个窗口内就补跑一次；
/// 更早的错过点不再追溯（只补最近一次、不重放整段历史周期），同时也把
/// 「表达式永远不命中」这类任务每次轮询的扫描代价钉在上界。
const CRON_CATCHUP_WINDOW_SECS: u64 = 48 * 3600;

/// 每个 bot 一个 task worker：
/// Q7 定的是「每 bot 默认 1 个 worker，超限**排队**（不拒绝）」——单 worker 串行
/// 消费就是排队语义，无需另造队列。
pub async fn task_worker(
    bot: crate::config::BotConfig,
    cfg: Arc<crate::config::Config>,
    router: Arc<Router>,
    stop: tokio_util::sync::CancellationToken,
) {
    task_worker_with_poll(bot, cfg, router, stop, POLL_INTERVAL).await
}

/// `poll` 可注入版（测试用；生产走 [`task_worker`] 的固定间隔）。
pub(crate) async fn task_worker_with_poll(
    bot: crate::config::BotConfig,
    cfg: Arc<crate::config::Config>,
    router: Arc<Router>,
    stop: tokio_util::sync::CancellationToken,
    poll: Duration,
) {
    let bot_key = bot.key();
    let store = TaskStore::new(&bot_key);
    let states = TaskStateStore::new(&bot_key);
    task_worker_with_stores(bot, cfg, router, store, states, stop, poll).await;
}

/// 注入 store/state 的 worker 主体，测试可直接驱动真实启动/关停路径而不碰真实 HOME。
async fn task_worker_with_stores(
    bot: crate::config::BotConfig,
    cfg: Arc<crate::config::Config>,
    router: Arc<Router>,
    store: TaskStore,
    states: TaskStateStore,
    stop: tokio_util::sync::CancellationToken,
    poll: Duration,
) {
    let bot_key = bot.key();
    // keepalive 必须先走独立恢复：它的 Running 可能对应一个仍存活、只是失去父
    // service 的进程；先把它移出 Running，后面的通用 orphan 重跑才不会制造双实例。
    recover_keepalives(&store, &states, crate::chrono_lite::unix_secs()).await;
    // agent 任务在 oneshot 里跑，没有可复活的 pid 账本，重跑是唯一安全的归位
    // （副作用幂等性由任务 prompt 自己负责——文档 §风险表已记「结果重复投递」）。
    requeue_orphans(&store, &states);
    // 上次回收时间（0 = 启动后先扫一遍）
    let mut last_gc = 0u64;

    loop {
        if stop.is_cancelled() {
            finalize_keepalives_on_shutdown(&store, &states);
            return;
        }
        // 取消请求先于认领处理：排队等着的任务被 cancel 掉之后**不该再开跑**
        // （否则用户看到的是「已取消却仍然跑了一轮并投递结果」）。
        consume_cancel_requests(&store, &states);
        // P2b-D：日志回收每小时扫一次（纯文件操作；不在每个 2s 轮询里做）。
        let now = crate::chrono_lite::unix_secs();
        if now.saturating_sub(last_gc) >= LOG_GC_INTERVAL_SECS {
            last_gc = now;
            gc_logs(&store, &states, now);
        }
        // 每轮只认领一条：跑完再认领下一条 = 串行排队（Q7）。
        if let Some(task) = next_due_task(&store, &states, now) {
            run_one(&bot, &cfg, &router, &states, task, &bot_key, &stop).await;
            continue; // 立刻看下一条，不等轮询间隔
        }
        tokio::select! {
            _ = tokio::time::sleep(poll) => {}
            _ = stop.cancelled() => {
                // Backoff/Pending 没有正在执行的 run_one 负责收尾；在正常关停出口统一落终态，
                // 否则下次启动会把它们当成待恢复任务重新拉起。
                finalize_keepalives_on_shutdown(&store, &states);
                return;
            }
        }
    }
}

/// 把一个任务标成 Running（认领）并跑完。返回是否真的执行了（false = 跳过）。
async fn run_one(
    bot: &crate::config::BotConfig,
    cfg: &crate::config::Config,
    router: &Arc<Router>,
    states: &TaskStateStore,
    task: Task,
    bot_key: &str,
    stop: &tokio_util::sync::CancellationToken,
) {
    let id = task.id.clone();
    let short = id[..id.len().min(12)].to_string();

    let started = crate::chrono_lite::unix_secs();
    let prev = states.get(&id);
    // 先按**认领前**的运行态算出应到时刻，再认领（claim 会刷新 last_fired_at）。
    let due = due_for_claim(&task, &prev, started);
    let _ = states.set(&id, claim(&prev, started));
    // 逾期认领必须留痕：过去「执行位被长任务占住 → 该分钟点永久消失」是**完全静默**的，
    // 用户看不到「定时任务为什么没跑」。这里如实记一笔应到时刻与迟到秒数。
    if let Some(due) = due {
        let late = started.saturating_sub(due);
        if late >= OVERDUE_LOG_SECS {
            // 迟到可能是「执行位被占」「service 停过」「登记时时间点已过」——日志只如实记
            // 应到时刻与迟到时长，不替用户下因果结论。
            crate::log!(
                "[task:{bot_key}] {short} 逾期认领（{}档）：应到 {}，迟到 {late}s",
                trigger_label(task.trigger.kind),
                fmt_local(due)
            );
        }
    }
    crate::log!(
        "[task:{bot_key}] {short} 开跑（workspace={}）",
        task_workspace(bot_key, &task)
    );

    let workspace = task_workspace(bot_key, &task);
    match task.payload.kind {
        PayloadKind::Agent => {
            let agent_cfg = agent_cfg_for_task(bot, cfg, &task, bot_key);
            run_attempt(bot_key, task, agent_cfg, workspace, router, states, stop).await;
        }
        PayloadKind::Proc if task.trigger.kind == TriggerKind::Keepalive => {
            run_keepalive_attempt(&task, &workspace, states, stop).await;
        }
        PayloadKind::Proc => {
            let rt = crate::task_proc::run_proc_attempt(&task, &workspace, states, stop).await;
            let text = if rt.kind == TaskStateKind::Succeeded {
                rt.last_exit_code
                    .map(|code| format!("进程退出码 {code}"))
                    .unwrap_or_else(|| "进程正常退出".to_string())
            } else {
                rt.last_error.clone()
            };
            finish_task_attempt(bot_key, &task, rt, text, router, states).await;
        }
    }
}

/// keepalive 的一次进程代际执行与退出后的状态机推进。
///
/// 首次由 Pending 认领；进程退出后这里只把状态推到 Pending（正常退出）或
/// Backoff（失败未熔断），主 worker 再按轮询/退避重新认领。这样退避期间 service
/// 仍可响应 cancel/stop，不会把阻塞睡在单次 run_one 里。
async fn run_keepalive_attempt(
    task: &Task,
    workspace: &str,
    states: &TaskStateStore,
    stop: &tokio_util::sync::CancellationToken,
) {
    let id = task.id.clone();
    let rt = crate::task_proc::run_proc_attempt(task, workspace, states, stop).await;

    // 正常关停优先于“进程恰好自行退出”的竞态：只要 stop 已触发，本次代际绝不排队重启。
    if stop.is_cancelled() && rt.kind != TaskStateKind::Cancelled {
        let cancelled = TaskRuntime {
            kind: TaskStateKind::Cancelled,
            last_error: CANCEL_REASON_SHUTDOWN.to_string(),
            next_retry_at: None,
            ..rt
        };
        let _ = states.set(&id, cancelled);
        return;
    }
    if rt.kind == TaskStateKind::Cancelled {
        return; // task cancel / service 关停已是终态，不再拉起。
    }

    let now = crate::chrono_lite::unix_secs();
    let rapid_exit = rt
        .started_at
        .zip(rt.finished_at)
        .is_some_and(|(started, finished)| {
            finished.saturating_sub(started) < KEEPALIVE_STABLE_SECS
        });
    let failed = rt.kind == TaskStateKind::Failed || rapid_exit;
    let failure_reason = if !rt.last_error.trim().is_empty() {
        rt.last_error.clone()
    } else if rapid_exit {
        format!("进程启动后 {KEEPALIVE_STABLE_SECS}s 内退出（未达到稳定运行阈值）")
    } else {
        String::new()
    };
    let consecutive = if failed {
        rt.consecutive_failures.saturating_add(1)
    } else {
        0
    };
    let mut next = TaskRuntime {
        kind: TaskStateKind::Pending,
        pid: None,
        proc_identity: None,
        next_retry_at: None,
        restarts: rt.restarts.saturating_add(1),
        consecutive_failures: consecutive,
        finished_at: Some(now),
        ..rt.clone()
    };

    let note = if failed && consecutive >= KEEPALIVE_MAX_CONSECUTIVE_FAILURES {
        next.kind = TaskStateKind::Failed;
        next.last_error = format!(
            "keepalive 连续启动失败达到熔断上限 {}，已停止自动拉起；最近错误：{}",
            KEEPALIVE_MAX_CONSECUTIVE_FAILURES, failure_reason
        );
        crate::log!(
            "[task:{}] {} keepalive 熔断（连续失败 {} 次）：{}",
            task.bot_key,
            short_id(&id),
            consecutive,
            next.last_error
        );
        format!("[keepalive] {}\n", next.last_error)
    } else if failed {
        let backoff = keepalive_backoff_secs(consecutive);
        next.kind = TaskStateKind::Backoff;
        next.next_retry_at = Some(now.saturating_add(backoff));
        crate::log!(
            "[task:{}] {} keepalive 退避 {}s（连续失败 {} 次）：{}",
            task.bot_key,
            short_id(&id),
            backoff,
            consecutive,
            failure_reason
        );
        format!(
            "[keepalive] 第 {} 次连续失败，{}s 后重试：{}\n",
            consecutive, backoff, failure_reason
        )
    } else {
        // 常驻任务正常退出也要重新拉起；不延迟，状态机仍经 Pending 统一进入 claim。
        crate::log!(
            "[task:{}] {} keepalive 正常退出，立即重新拉起",
            task.bot_key,
            short_id(&id)
        );
        "[keepalive] 进程正常退出，立即重新拉起\n".to_string()
    };

    let _ = states.set(&id, next);
    let _ = crate::task_store::append_task_log_record(
        states.paths(),
        &id,
        task.limits.log_max_bytes,
        note.as_bytes(),
    );
}

fn keepalive_backoff_secs(consecutive_failures: u32) -> u64 {
    let idx = consecutive_failures.saturating_sub(1) as usize;
    KEEPALIVE_BACKOFF_SECS
        .get(idx)
        .copied()
        .unwrap_or_else(|| *KEEPALIVE_BACKOFF_SECS.last().unwrap_or(&30))
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// 真正跑一轮并把结局落盘/投递。`agent_cfg`/`workspace` 由调用方给——测试因此能用
/// mock agent 驱动**全链路**（登记 → 认领 → 跑 → 落运行态 → 写日志 → 投递回创建者），
/// 而不必去猜真实后端的输出。
pub(crate) async fn run_attempt(
    bot_key: &str,
    task: Task,
    agent_cfg: crate::buzz::harness::AgentConfig,
    workspace: String,
    router: &Arc<Router>,
    states: &TaskStateStore,
    stop: &tokio_util::sync::CancellationToken,
) {
    let id = task.id.clone();
    let started = crate::chrono_lite::unix_secs();

    let msg = crate::buzz::queue::InboundMsg {
        id_hex: uuid::Uuid::new_v4().to_string(),
        author_role: task.created_by.role.as_str().to_string(),
        text: task.payload.prompt.clone(),
        ts_secs: started as i64,
        prompt_tag: TASK_PROMPT_TAG.to_string(),
    };
    let budget = task
        .timeout()
        .unwrap_or(crate::buzz::harness::MAX_TURN_DURATION + Duration::from_secs(30));
    // Q3 `task cancel`：`oneshot_turn` 的 external_cancel 只接一个令牌，所以这里做
    // 「service 关停 ∪ 用户取消请求」的合并——`child_token()` 随父（关停）取消，
    // watcher 命中取消请求时再单独 cancel 这个子令牌（不反cancel 父，关停语义不变）。
    let attempt_cancel = stop.child_token();
    // 记录「这一轮的取消究竟是谁发的」：两种来源都折叠成 Cancelled，落盘原因要分开。
    let user_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_watch = {
        let token = attempt_cancel.clone();
        let user_cancelled = user_cancelled.clone();
        let req = states.paths().cancel_file(&id);
        tokio::spawn(async move {
            loop {
                if token.is_cancelled() {
                    return;
                }
                if req.exists() {
                    crate::log!("[task] 收到取消请求（{}），终止在跑轮次", req.display());
                    user_cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    token.cancel();
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {}
                    _ = token.cancelled() => return,
                }
            }
        })
    };
    let outcome = crate::buzz::oneshot::oneshot_turn(
        agent_cfg,
        Some(workspace),
        msg,
        budget,
        Some(attempt_cancel.clone()),
    )
    .await;
    cancel_watch.abort();
    // 消费掉请求文件（无论这轮是被它取消的还是正常跑完）——不消费的话，同一 id 的
    // 下一次重跑会立刻又被自己上一次的请求取消。
    let _ = std::fs::remove_file(states.paths().cancel_file(&id));

    let finished = crate::chrono_lite::unix_secs();
    // 终态运行态要**带住两样跨轮次记账**，否则重复档会被自己坑（审查/自查踩到过）：
    //   · `restarts`：不带住 → `task status` 看不到重跑次数，且重跑上界失效；
    //   · `last_fired_at`：**清零会让 cron 在同一分钟内立刻再触发一次**（分钟去重靠它），
    //     interval 也会退化成"跑完立刻再来一轮"。
    let prev = states.get(&id);
    let base = TaskRuntime {
        started_at: Some(started),
        finished_at: Some(finished),
        restarts: prev.restarts,
        last_fired_at: prev.last_fired_at,
        ..Default::default()
    };
    let (rt, text) = match outcome {
        crate::buzz::harness::SyncTurnOutcome::Ok(text) => (
            TaskRuntime {
                kind: TaskStateKind::Succeeded,
                last_exit_code: Some(0),
                ..base.clone()
            },
            text,
        ),
        crate::buzz::harness::SyncTurnOutcome::Cancelled => (
            TaskRuntime {
                kind: TaskStateKind::Cancelled,
                last_error: if user_cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    CANCEL_REASON.to_string()
                } else {
                    CANCEL_REASON_SHUTDOWN.to_string()
                },
                ..base.clone()
            },
            String::new(),
        ),
        crate::buzz::harness::SyncTurnOutcome::Timeout => (
            TaskRuntime {
                kind: TaskStateKind::Failed,
                last_error: format!("执行超时（预算 {}s）", budget.as_secs()),
                ..base.clone()
            },
            String::new(),
        ),
        crate::buzz::harness::SyncTurnOutcome::Closed => (
            TaskRuntime {
                kind: TaskStateKind::Failed,
                last_error: "agent 不可用（执行器未能拉起）".to_string(),
                ..base.clone()
            },
            String::new(),
        ),
        crate::buzz::harness::SyncTurnOutcome::Failed(reason) => (
            TaskRuntime {
                kind: TaskStateKind::Failed,
                last_error: reason.clone(),
                ..base.clone()
            },
            String::new(),
        ),
    };
    finish_task_attempt(bot_key, &task, rt, text, router, states).await;
}

/// 公共收尾：落终态、追加终态摘要日志、输出 service 日志并投递结果。
///
/// proc 的 stdout/stderr 已在执行期流式落盘；这里的 `text` 只是最后一行摘要，不复制整段
/// 输出。取消轮次仍走原有契约：留状态/日志，不投递。
async fn finish_task_attempt(
    bot_key: &str,
    task: &Task,
    rt: TaskRuntime,
    text: String,
    router: &Arc<Router>,
    states: &TaskStateStore,
) {
    let id = task.id.clone();
    let short = id[..id.len().min(12)].to_string();
    let failed = rt.kind == TaskStateKind::Failed;
    let cancelled = rt.kind == TaskStateKind::Cancelled;
    let reason = rt.last_error.clone();
    let finished = rt.finished_at.unwrap_or_else(crate::chrono_lite::unix_secs);
    let _ = states.set(&id, rt.clone());

    // 日志：任务本身就是「跑久/跑丢也看不见」的痛点，落盘 + 出口都要有（D6 的基础项）。
    write_log(states.paths(), task, &rt, &text);
    crate::log!(
        "[task:{bot_key}] {short} 结束：{:?}{}",
        if failed {
            "失败"
        } else if cancelled {
            "取消"
        } else {
            "成功"
        },
        if reason.is_empty() {
            String::new()
        } else {
            format!("（{reason}）")
        }
    );

    // 取消**不回投**——这是 `task cancel` 的明确语义（用户已经说了不要结果）：
    // 两种来源都适用 — ① service 关停联动（投递通道也在关，回了多半发不出去）；
    // ② 用户 `task cancel`（Q3，已落地）。所以下面「🛑 已取消」那条抬头仍然
    // **不可达**，不要把它当已实现的验收项：取消一律静默收尾 + 日志/运行态留痕。
    if cancelled {
        return;
    }
    let Some((target_bot, chat_raw)) = delivery_target(task, bot_key) else {
        crate::log!("[task:{bot_key}] {short} 无投递目标（创建者会话为空），结果未发送");
        return;
    };
    // 存量坏任务自愈：创建者会话若存的是 buzz 频道 UUID（外部注入的 AGENT_BRIDGE_CHAT_ID），
    // 直发必被平台拒（飞书 230001）——回落到该 bot 主会话，loud，让结果真送达。
    let primary = router
        .bots
        .get(&target_bot)
        .map(|b| b.primary_chat_id.clone())
        .unwrap_or_default();
    let (chat, healed) = heal_delivery_chat(&chat_raw, &primary);
    if healed {
        crate::log!(
            "[task:{bot_key}] {short} 投递目标 chat={chat_raw} 形如 buzz 频道 UUID（非平台 chat_id），已回落主会话 {chat}"
        );
    }
    // 注意：这里没有「已取消」抬头——见上 `if cancelled { return; }` 的说明，
    // 被取消的轮次（关停联动 / 用户 task cancel）压根走不到投递。
    let header = delivery_header(task, failed);
    let body = if text.trim().is_empty() {
        if reason.is_empty() {
            "（无输出）".to_string()
        } else {
            reason
        }
    } else {
        text
    };
    let created_chat = task.created_by.chat_id.clone();
    // `in_session`（= 走 Router 自环豁免的那条可信通路）只在**目标确实就是创建者
    // 会话**时成立；`--to` 指到别处时必须显式跨会话，否则等于伪造豁免（审查 P2）。
    // 回落主会话时，回源也指向那个会话（创建者会话已失效），维持「目标==来源」的自环语义。
    let source_chat = if healed { chat.clone() } else { created_chat };
    let in_session = target_bot == bot_key && chat == source_chat;
    let origin = task_delivery_origin(in_session);
    let item = DeliveryItem {
        id: uuid::Uuid::new_v4().to_string(),
        target_bot: target_bot.clone(),
        target_chat: chat.clone(),
        text: format!("{header}\n\n{body}"),
        source_bot: bot_key.to_string(),
        source_chat,
        created_at: finished,
        attachments: Vec::new(),
        // 非空仅供 outbox tag / 日志（P1b 起豁免判据走 origin，不再借本字段）；
        // 同时保住存量 `deliveries.json` 兼容口径。
        job_id: task.id.clone(),
        in_session,
        origin,
    };
    let outcome = router.deliver(&item).await;
    if outcome.is_delivered() {
        return;
    }
    // 第二告警通道（#306 验收）：默认投递目标 = 创建者会话，`Router` 的回源告警也就
    // 发回那个**已失效**的会话 —— 等于没有告警。这里补一条独立通道：把失败写进运行态
    // （`task status` 可见）并投该 bot 的主会话（owner 私聊，与 run_job 的回落同源）。
    let reason = match outcome {
        crate::deliver::DeliveryOutcome::NotDelivered(r) => r,
        crate::deliver::DeliveryOutcome::Delivered => String::new(),
    };
    let note = format!("结果投递失败：{reason}");
    let mut rt2 = states.get(&id);
    rt2.last_error = if rt2.last_error.is_empty() {
        note.clone()
    } else {
        format!("{}；{note}", rt2.last_error)
    };
    let _ = states.set(&id, rt2);
    crate::log!("[task:{bot_key}] {short} {note}（目标 bot={target_bot} chat={chat}）");
    alert_primary_chat(
        router,
        bot_key,
        &target_bot,
        &chat,
        &task.id,
        &task.display_name(),
        &note,
    )
    .await;
}

/// 结果投递的抬头。
///
/// **失败时必须带任务身份与排查入口**：原来失败只有一句「⚠️ 后台任务失败」，用户
/// （尤其跨机器、或多任务并发时）根本不知道是哪条任务失败、去哪看原因——「执行超时
/// （预算 7200s）」那条通知只能靠人反问就是这个原因。成功抬头保持原样（带任务名）。
/// 抽成纯函数是为了能直接断言，不必为一个字符串走完整 agent 回合。
fn delivery_header(task: &Task, failed: bool) -> String {
    if failed {
        format!(
            "⚠️ 后台任务失败：{}\n{}（`task status {}` 看原因 · `task logs {}` 看输出）",
            task.display_name(),
            task.id,
            task.id,
            task.id
        )
    } else {
        format!("🤖 后台任务完成：{}", task.display_name())
    }
}

/// 第二告警通道（#306）：把投递失败告诉该 bot 的**主会话**（owner 私聊）。
///
/// 只在主会话与失败目标不同时发——相同就说明主会话正是那个失效目标，再发一次没有
/// 意义（那种情况只剩日志与 `task-logs` 留痕）。best-effort：发不出去只记日志。
async fn alert_primary_chat(
    router: &Arc<Router>,
    bot_key: &str,
    target_bot: &str,
    target_chat: &str,
    task_id: &str,
    task_name: &str,
    note: &str,
) {
    // 主会话取路由表里的 BotConfig（= 同一份配置的 bots[]）而不是再读 config.json：
    // 生产同源，测试可注入（`Config::primary_chat` 直读盘，单测里拿不到）。
    let primary = router
        .bots
        .get(bot_key)
        .map(|b| b.primary_chat_id.clone())
        .unwrap_or_default();
    if primary.is_empty() {
        crate::log!("[task:{bot_key}] 无主会话可回落，投递失败仅留日志 + task-logs");
        return;
    }
    if target_bot == bot_key && primary == target_chat {
        return;
    }
    let Some(msgr) = router.messengers.get(bot_key) else {
        crate::log!("[task:{bot_key}] 主会话回落失败：本 bot messenger 不在路由表里");
        return;
    };
    // 同样带任务身份：投递失败时用户看到的这条就是唯一线索，没 id 无法自查
    let text = format!(
        "⚠️ 后台任务结果投递失败（{note}）\n\n任务：{task_name}\n{task_id} · `task status {task_id}` 看原因\n目标：{target_bot}:{target_chat}"
    );
    if let Err(e) = msgr.send_text(&primary, &text).await {
        crate::log!(
            "[task:{bot_key}] 主会话回落也失败 chat={}: {e:#}",
            crate::agent::truncate(&primary, 16)
        );
    }
}

/// 认领一条任务：把运行态推进到 `Running`。
///
/// **认领的唯一入口就是这里**（`run_one` 调它）：任何想写 `Running` 的地方都必须走
/// 本函数，别再内联一个 `TaskRuntime { kind: Running, ..Default::default() }`——
/// 那正是上一版把 `restarts` 清零、让重跑上界失效的写法。
///
/// **必须把 `restarts` 带走**。这里原先是 `..Default::default()`——认领即把计数清零，
/// 于是 `requeue_orphans` 的上界判定 `rt.restarts < max_restarts` 永远成立，
/// 「有界重跑」在生产路径上失效（崩溃 → 重启 → 归位 → 认领清零 → 再崩 → 无限）。
/// 单测直接锁这个纯函数，防「测试里手工构造 restarts 所以绿、生产里被清零」再次发生。
fn claim(prev: &TaskRuntime, started: u64) -> TaskRuntime {
    TaskRuntime {
        kind: TaskStateKind::Running,
        started_at: Some(started),
        restarts: prev.restarts,
        consecutive_failures: prev.consecutive_failures,
        // P2b-C 调度记账：**认领时刻**（不是跑完时刻）——cron 按它的分钟桶去重、
        // interval 按它 + 间隔算下次到点；与 Running 状态一起防同任务并发。
        last_fired_at: Some(started),
        ..Default::default()
    }
}

/// **选执行剖面并装配 agent 配置**——这是「这条任务用什么权限跑」的唯一定点。
///
/// 安全审查 B1：剖面必须按**创建者角色**选，不能一律用 owner 的内部任务配置。
/// `$ABB_BIN task add` 对 granted 会话是放行的（与 `job add` 同一条信任链），
/// 选错就等于 granted 会话里的 agent 能借任务拿全权限。
/// 判据与 `run_job` 完全一致：`config::restrict_granted(role, bot_key)`。
fn agent_cfg_for_task(
    bot: &crate::config::BotConfig,
    cfg: &crate::config::Config,
    task: &Task,
    bot_key: &str,
) -> crate::buzz::harness::AgentConfig {
    // 用 **worker 自己的 bot_key**（不是定义文件里的 `task.bot_key`）判剖面：定义文件
    // 可被手改，拿它当判据的话，改成一台 `restrict_granted_agent = false` 的 bot
    // 就能选到更松的档。`TaskStore::add` 另有一道「归属必须与所在目录一致」的校验。
    let restricted = crate::config::restrict_granted(task.created_by.role, bot_key);
    crate::service::oneshot_agent_config_for_role(bot, cfg, restricted)
}

/// 任务的工作目录：定义里显式给了就用它，否则回落该 bot 的工作区。
fn task_workspace(bot_key: &str, task: &Task) -> String {
    if task.payload.cwd.trim().is_empty() {
        crate::workspace_dir(bot_key).display().to_string()
    } else {
        task.payload.cwd.clone()
    }
}

/// 投递目标 `(bot_key, chat_id)`：显式 `targets` 优先，否则回创建者会话。
///
/// `targets[i].bot_key` 留空 = 本 bot（`task add --to` 的缺省写法）；#306 之前执行侧
/// 忽略它、按本 bot 投，`Task::validate` 于是显式拒绝跨 bot——现在两处一起解除。
/// P1b 投递来源（#326）：目标**确实就是创建者会话**（自环）→ `InSession`——正是改前
/// `in_session=true` 的那条可信通路，护栏豁免面（开关 + 自环 + 去重）与旧行为逐条一致；
/// 投到别处 → `TaskCompletion`（豁免自环 + 去重，跨会话开关照旧约束）。
///
/// 待确认（见 patch body）：设计文字写「task_run → TaskCompletion」；若自环也强制记
/// `TaskCompletion`，跨会话开关（默认关）会开始拦截「任务完成回发创建者会话」这一默认
/// 投递目标，属线上行为变更——按「拿不准即保持旧行为」暂不采用，等设计确认。
fn task_delivery_origin(in_session: bool) -> DeliveryOrigin {
    if in_session {
        DeliveryOrigin::InSession
    } else {
        DeliveryOrigin::TaskCompletion
    }
}

fn delivery_target(task: &Task, bot_key: &str) -> Option<(String, String)> {
    if let Some(t) = task.delivery.targets.first() {
        let target_bot = if t.bot_key.trim().is_empty() {
            bot_key.to_string()
        } else {
            t.bot_key.clone()
        };
        return Some((target_bot, t.chat_id.clone()));
    }
    if task.created_by.chat_id.trim().is_empty() {
        None
    } else {
        Some((bot_key.to_string(), task.created_by.chat_id.clone()))
    }
}

/// 投递目标校正（纯函数，可测）：目标 chat 形如 buzz 频道 UUID 时回落到该 bot 主会话。
///
/// 背景：ACP 架构下 agent 是每 bot 长驻进程，桥无法按频道注入 `AGENT_BRIDGE_CHAT_ID`；
/// 外部启动器注入的若是 buzz 频道 UUID，会经 `task add` 原样存进 `created_by.chat_id`。
/// 该值不是平台 receive_id，直发必被平台拒（飞书 230001 invalid receive_id）——这里
/// 回落到 `primary`（真实平台会话），让**存量坏任务**的结果也能真送达。返回
/// `(生效 chat, 是否发生回落)`；`primary` 为空时原样返回（交由投递侧 loud 失败）。
fn heal_delivery_chat(chat: &str, primary: &str) -> (String, bool) {
    if !primary.is_empty() && crate::buzz::keys::looks_like_channel_uuid(chat) {
        (primary.to_string(), true)
    } else {
        (chat.to_string(), false)
    }
}

/// 消费取消请求文件（Q3 `task cancel` 的 service 侧）。
///
/// 方向是**CLI 写请求、service 消费**（运行态只有一个写者）。这里只处理「此刻没在跑」
/// 的那类：`Pending` → 直接判 `Cancelled`（不再开跑）；已是终态 → 只清 requests 文件
/// （CLI 侧已拦，但手写的文件不该把已成功的任务改写成取消）；`Running` → **留着不删**，
/// 由在跑那轮的 watcher 消费，否则等于把取消信号从它嘴里抢走。
fn consume_cancel_requests(store: &TaskStore, states: &TaskStateStore) {
    let dir = states.paths().cancel_requests_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(id) = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        let rt = states.get(&id);
        let Some(task) = store.list().into_iter().find(|t| t.id == id) else {
            // 定义已经没了（task rm / 孤儿清理）：请求没有意义，清掉别堆积。
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if rt.kind == TaskStateKind::Running {
            continue; // 在跑那轮由 watcher 消费（见 run_attempt 的取消令牌）
        }
        // 可取消的两类「非在跑」状态：
        //   ① Pending（还没开跑）；
        //   ② **重复档**（cron/interval）的 Succeeded/Failed——跑完一轮并不代表结束，
        //      下一分钟/下一个间隔还会再跑；用户此时取消必须能停掉后续触发
        //      （审查实测：只处理 Pending 时会回「已结束，无需取消」然后照跑）。
        // 一次性档（now/once）的 Succeeded/Failed 是真终态，不动。
        let cancellable = matches!(
            rt.kind,
            TaskStateKind::Pending | TaskStateKind::Backoff | TaskStateKind::Interrupted
        ) || (task.trigger.kind.is_repeating()
            && matches!(rt.kind, TaskStateKind::Succeeded | TaskStateKind::Failed));
        if cancellable {
            crate::log!("[task] {id} 收到取消请求（{:?}）→ 标记 Cancelled", rt.kind);
            let _ = states.set(
                &id,
                TaskRuntime {
                    kind: TaskStateKind::Cancelled,
                    finished_at: Some(crate::chrono_lite::unix_secs()),
                    last_error: CANCEL_REASON_BEFORE_START.to_string(),
                    ..rt
                },
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// 把一条任务的结局追加到 `task-logs/<id>.log`。
///
/// P2b-D：超过 `log_max_bytes` 时**轮转**（`id.log` → `id.log.1` → `id.log.2`，最老丢弃，
/// 共 `LOG_KEEP_FILES` 份）。旧实现是「超上限就静默不再写」——日志是任务排障的唯一入口，
/// 静默停写比丢弃最老历史更坏（用户不知道日志断了）。
fn write_log(paths: &crate::task_store::TaskPaths, task: &Task, rt: &TaskRuntime, text: &str) {
    use std::io::Write;

    let stamp = rt.finished_at.unwrap_or_else(crate::chrono_lite::unix_secs);
    let mut body = Vec::new();
    let _ = writeln!(body, "[{}] {:?} {}", stamp, rt.kind, rt.last_error);
    if !text.trim().is_empty() {
        let _ = writeln!(body, "{text}\n");
    }
    let _ = crate::task_store::append_task_log_record(
        paths,
        &task.id,
        task.limits.log_max_bytes,
        &body,
    );
}

/// 该任务此刻是否可被认领（P2b-C 编排判定，纯函数便于单测）。
///
/// 语义（与 `docs/task-model.md` 的 P2b-C 一节一致）：
/// - `now`：登记即跑一次（#306 的后台子代理，行为不变）；
/// - `once`：到点跑；**错过后补跑**（与 `job` 调度器 `Job::is_due` 一致，`t <= now`）；
/// - `cron`：**上次记账之后应到期的那个分钟点已过**即认领（详见 [`cron_due`]）——
///   执行位被长任务占住而错过的分钟点会补跑一次，且同一分钟只触发一次；
/// - `interval`：`last_fired_at` 为空（首次）即跑，之后每 N 秒一次；
/// - `keepalive`：Pending 或退避到期时认领；带进程代际身份的 Backoff 必须等恢复
///   路径确认旧代际已死，不能在 Running 直接再拉一个；
/// - 任何触发档在 `Running` 时都不认领（同任务不并发），`Cancelled` 是用户明确的终态
///   （要再跑就重新登记）；重复档（cron/interval）跑完一轮后回到可认领状态。
fn is_due_for_claim(task: &Task, rt: &TaskRuntime, now: u64) -> bool {
    due_for_claim(task, rt, now).is_some()
}

/// 「此刻应被认领」的应到时刻（秒）；`None` = 不该认领。
///
/// 把应到时刻**返回**出来（而不是只回 bool）是为了可观测性：认领方拿它与当前时刻
/// 比对，就能如实记下「逾期 N 秒才轮到它（执行位此前被占）」——cron 档过去的
/// 完全静默正是缺这一笔（见 `run_one` 里的逾期日志）。
fn due_for_claim(task: &Task, rt: &TaskRuntime, now: u64) -> Option<u64> {
    let kind = task.trigger.kind;
    match rt.kind {
        TaskStateKind::Running | TaskStateKind::Cancelled | TaskStateKind::Interrupted => {
            return None
        }
        TaskStateKind::Pending => {}
        TaskStateKind::Backoff => {
            let due = kind == TriggerKind::Keepalive
                && rt.proc_identity.is_none()
                && rt.next_retry_at.map(|t| t <= now).unwrap_or(true);
            return due.then_some(rt.next_retry_at.unwrap_or(now));
        }
        TaskStateKind::Succeeded | TaskStateKind::Failed => {
            // 一次性任务跑过就是跑过了；重复档等下一次到点
            if !kind.is_repeating() {
                return None;
            }
        }
    }
    match kind {
        TriggerKind::Now => (rt.kind == TaskStateKind::Pending).then_some(now),
        TriggerKind::Once => crate::schedule::parse_once(&task.trigger.expr)
            .map(|t| t.to_unix().max(0) as u64)
            .filter(|t| *t <= now),
        TriggerKind::Cron => {
            let expr = crate::schedule::CronExpr::parse(&task.trigger.expr)?;
            cron_due(&expr, rt.last_fired_at, now)
        }
        TriggerKind::Interval => {
            let secs = crate::task_store::parse_interval_secs(&task.trigger.expr)?;
            match rt.last_fired_at {
                None => Some(now),
                // 记账时刻在**未来**（系统时钟回拨、或手改状态文件）→ 不信这笔记账，
                // 按"到点"处理；否则任务会一直不跑，直到墙钟追上那个未来时间点。
                Some(last) if last > now => Some(now),
                Some(last) => {
                    let due = last.saturating_add(secs);
                    (due <= now).then_some(due)
                }
            }
        }
        // Pending 是新登记/正常退出后的入口；Succeeded 是旧状态或旁路写入的“未在跑”。
        // Failed 只由熔断产生，Interrupted 表示旧代际未安全接管，两者都不得自动拉起。
        TriggerKind::Keepalive => {
            (matches!(rt.kind, TaskStateKind::Pending | TaskStateKind::Succeeded)
                && rt.proc_identity.is_none())
            .then_some(now)
        }
    }
}

/// cron 的应到时刻：**上次记账之后应到期的那个命中分钟点**。
///
/// - 从未跑过（`last_fired_at` 为空）：维持原语义——当前分钟命中表达式即可跑，
///   新登记的任务不追溯历史周期（`missed_schedules_do_not_backfill_history` 锁的就是这条）；
/// - 已跑过：判据放宽为「上次记账之后存在一个已到期的命中分钟点」→ 该点被长任务
///   占住而错过时，会在 worker 空出来后**补跑一次**。只补一次：认领即刷新
///   `last_fired_at`，不会重放整段历史；
/// - 同一分钟桶已记账 → 不认领（保留「同一分钟只触发一次」，防 2s 轮询重复触发）；
/// - 记账时刻在**未来且不在同一分钟桶**（时钟回拨/手改状态文件）→ 不信这笔记账，
///   回落为当前分钟匹配；同一分钟桶仍按去重不认领（最坏推迟到下一分钟，不会卡死）；
/// - 超过 [`CRON_CATCHUP_WINDOW_SECS`] 的错过点不补（只补窗口内最近一次，不重放整段历史周期）。
fn cron_due(expr: &crate::schedule::CronExpr, last_fired_at: Option<u64>, now: u64) -> Option<u64> {
    let now_dt = crate::schedule::DateTime::from_unix(now as i64);
    let Some(last) = last_fired_at else {
        return expr.matches(&now_dt).then_some(now);
    };
    // 同一分钟只触发一次（worker 每 2s 轮询，没有这条会在一分钟内反复触发）。
    // 这条是**纵深防御**：性质其实由下面的扫描起点结构性保证（起点是「上次记账的下一分钟」，
    // 当前分钟永远不会被扫到）。保留它显式表达契约，也防止将来改动扫描起点时静默破坏它。
    if last / 60 == now / 60 {
        return None;
    }
    if last > now {
        return expr.matches(&now_dt).then_some(now);
    }
    // 从「上次记账所在分钟」的下一分钟起，按分钟扫描到当前分钟：
    // 命中即说明该点应当被跑而没跑（执行位被占），补跑一次。
    let from = (last / 60 + 1) * 60;
    let start = from.max(now.saturating_sub(CRON_CATCHUP_WINDOW_SECS));
    if start > now {
        return None;
    }
    (start..=now)
        .step_by(60)
        .find(|t| expr.matches(&crate::schedule::DateTime::from_unix(*t as i64)))
}

/// 本地时区（UTC+8）的 `YYYY-MM-DD HH:MM:SS`——日志里给人看的应到时刻。
fn fmt_local(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = crate::chrono_lite::epoch_to_ymd(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// 触发档的短标签（日志用），与 CLI 的档位名保持一致。
fn trigger_label(kind: TriggerKind) -> &'static str {
    match kind {
        TriggerKind::Now => "now",
        TriggerKind::Once => "once",
        TriggerKind::Cron => "cron",
        TriggerKind::Interval => "interval",
        TriggerKind::Keepalive => "keepalive",
    }
}

/// service 正常关停出口：Pending/Backoff/Succeeded 没有在跑的 `run_proc_attempt`
/// 负责落终态，必须在这里统一置 `Cancelled`；否则下次启动会被重新认领。
fn finalize_keepalives_on_shutdown(store: &TaskStore, states: &TaskStateStore) {
    let now = crate::chrono_lite::unix_secs();
    for task in store
        .list()
        .into_iter()
        .filter(|t| t.trigger.kind == TriggerKind::Keepalive)
    {
        let rt = states.get(&task.id);
        let (pid, proc_identity) = match rt.kind {
            TaskStateKind::Running => (rt.pid, rt.proc_identity.clone()),
            TaskStateKind::Pending | TaskStateKind::Backoff | TaskStateKind::Succeeded => {
                (None, None)
            }
            TaskStateKind::Failed | TaskStateKind::Cancelled | TaskStateKind::Interrupted => {
                continue;
            }
        };
        let _ = states.set(
            &task.id,
            TaskRuntime {
                kind: TaskStateKind::Cancelled,
                pid,
                proc_identity,
                finished_at: Some(now),
                next_retry_at: None,
                last_error: CANCEL_REASON_SHUTDOWN.to_string(),
                ..rt
            },
        );
        crate::log!(
            "[task:{}] {} keepalive service 关停 → Cancelled",
            task.bot_key,
            short_id(&task.id)
        );
    }
}

/// service 启动时的 keepalive 独立恢复路径。**不能复用 `requeue_orphans`**：后者把
/// 残留 Running 当作进程已死，会为仍存活的 keepalive 制造第二个实例。
async fn recover_keepalives(store: &TaskStore, states: &TaskStateStore, now: u64) {
    for task in store
        .list()
        .into_iter()
        .filter(|t| t.trigger.kind == TriggerKind::Keepalive)
    {
        let rt = states.get(&task.id);
        let short = short_id(&task.id).to_string();

        if !task.resume_on_boot {
            if matches!(
                rt.kind,
                TaskStateKind::Cancelled | TaskStateKind::Failed | TaskStateKind::Interrupted
            ) {
                continue;
            }
            if matches!(
                rt.kind,
                TaskStateKind::Pending
                    | TaskStateKind::Running
                    | TaskStateKind::Backoff
                    | TaskStateKind::Succeeded
            ) {
                let interrupted = stop_keepalive_for_opt_out(&task, rt, now, states).await;
                let _ = states.set(&task.id, interrupted);
                crate::log!(
                    "[task:{}] {short} keepalive 按 resume_on_boot=false 保持停止",
                    task.bot_key
                );
            }
            continue;
        }

        match rt.kind {
            TaskStateKind::Running => {
                let Some(identity) = rt.proc_identity.clone() else {
                    let interrupted = interrupted_runtime(
                        rt,
                        now,
                        "旧 keepalive 进程缺少代际身份，只清理不 adopt（不重复拉起）",
                        false,
                    );
                    let _ = states.set(&task.id, interrupted);
                    crate::log!(
                        "[task:{}] {short} keepalive 无进程代际身份，已清理但不 adopt",
                        task.bot_key
                    );
                    continue;
                };
                match crate::task_identity::verify(&identity) {
                    IdentityStatus::Alive => {
                        let why = "旧 keepalive 进程仍存活，拒绝 adopt 且不重复拉起（避免双实例）";
                        let interrupted = interrupted_runtime(rt, now, why, true);
                        let _ = states.set(&task.id, interrupted);
                        crate::log!("[task:{}] {short} {why}", task.bot_key);
                    }
                    IdentityStatus::Unknown => {
                        let why = "旧 keepalive 进程身份无法核验，按可能存活处理：拒绝 adopt 且不重复拉起";
                        let interrupted = interrupted_runtime(rt, now, why, true);
                        let _ = states.set(&task.id, interrupted);
                        crate::log!("[task:{}] {short} {why}", task.bot_key);
                    }
                    IdentityStatus::Dead => {
                        let backoff = keepalive_backoff_secs(rt.consecutive_failures.max(1));
                        let resumed = TaskRuntime {
                            kind: TaskStateKind::Backoff,
                            pid: None,
                            proc_identity: None,
                            next_retry_at: Some(now.saturating_add(backoff)),
                            restarts: rt.restarts.saturating_add(1),
                            finished_at: Some(now),
                            last_error: "service 重启：旧进程代际已确认结束，等待恢复".to_string(),
                            ..rt
                        };
                        let _ = states.set(&task.id, resumed);
                        crate::log!(
                            "[task:{}] {short} keepalive 旧代际已死，{}s 后恢复",
                            task.bot_key,
                            backoff
                        );
                    }
                }
            }
            TaskStateKind::Pending | TaskStateKind::Backoff => {
                // Pending 是新登记/已安全归位的恢复入口；Backoff 保留原到期时刻。
            }
            TaskStateKind::Succeeded
            | TaskStateKind::Failed
            | TaskStateKind::Cancelled
            | TaskStateKind::Interrupted => {}
        }
    }
}

async fn stop_keepalive_for_opt_out(
    task: &Task,
    rt: TaskRuntime,
    now: u64,
    states: &TaskStateStore,
) -> TaskRuntime {
    let Some(identity) = rt.proc_identity.clone() else {
        return interrupted_runtime(
            rt,
            now,
            "resume_on_boot=false：service 重启后不恢复，运行态已清理",
            false,
        );
    };

    match crate::task_identity::verify(&identity) {
        IdentityStatus::Dead => interrupted_runtime(
            rt,
            now,
            "resume_on_boot=false：旧代际已确认结束，运行态已清理",
            false,
        ),
        IdentityStatus::Unknown => interrupted_runtime(
            rt,
            now,
            "resume_on_boot=false：旧进程身份无法核验，保留身份但不 adopt、不拉起",
            true,
        ),
        IdentityStatus::Alive => {
            let grace = Duration::from_secs(task.limits.grace_secs);
            match crate::task_proc::stop_orphan_process_group(identity.pid, grace).await {
                Ok(stop) if !stop.alive_after => {
                    let how = if stop.escalated {
                        "SIGTERM→宽限→SIGKILL"
                    } else {
                        "SIGTERM"
                    };
                    let why = format!(
                        "resume_on_boot=false：service 重启时已按 {how} 收掉旧进程，运行态已清理"
                    );
                    let note = format!("[keepalive] {why}\n");
                    let _ = crate::task_store::append_task_log_record(
                        states.paths(),
                        &task.id,
                        task.limits.log_max_bytes,
                        note.as_bytes(),
                    );
                    crate::log!("[task:{}] {} {why}", task.bot_key, short_id(&task.id));
                    interrupted_runtime(rt, now, &why, false)
                }
                Ok(stop) => {
                    let why = format!(
                        "resume_on_boot=false：旧进程组停止后仍有存活成员（escalated={}），保留身份供人工处理",
                        stop.escalated
                    );
                    let note = format!("[keepalive] {why}\n");
                    let _ = crate::task_store::append_task_log_record(
                        states.paths(),
                        &task.id,
                        task.limits.log_max_bytes,
                        note.as_bytes(),
                    );
                    crate::log!("[task:{}] {} {why}", task.bot_key, short_id(&task.id));
                    interrupted_runtime(rt, now, &why, true)
                }
                Err(e) => {
                    let why =
                        format!("resume_on_boot=false：停止旧进程组失败：{e}；保留身份供人工处理");
                    let note = format!("[keepalive] {why}\n");
                    let _ = crate::task_store::append_task_log_record(
                        states.paths(),
                        &task.id,
                        task.limits.log_max_bytes,
                        note.as_bytes(),
                    );
                    crate::log!("[task:{}] {} {why}", task.bot_key, short_id(&task.id));
                    interrupted_runtime(rt, now, &why, true)
                }
            }
        }
    }
}

fn interrupted_runtime(
    rt: TaskRuntime,
    now: u64,
    why: &str,
    preserve_live_process: bool,
) -> TaskRuntime {
    TaskRuntime {
        kind: TaskStateKind::Interrupted,
        pid: preserve_live_process.then_some(rt.pid).flatten(),
        proc_identity: if preserve_live_process {
            rt.proc_identity.clone()
        } else {
            None
        },
        next_retry_at: None,
        finished_at: (!preserve_live_process).then_some(now),
        last_error: why.to_string(),
        ..rt
    }
}

/// P2b-D 日志回收：终态任务超过 [`LOG_RETENTION_DAYS`] 的日志删掉。
///
/// **只删日志**：任务定义与运行态都保留（`task status` 仍要能看到结果与错误），
/// 也绝不自动删用户登记的任务——那属于用户资产，删了没法恢复。
fn gc_logs(store: &TaskStore, states: &TaskStateStore, now: u64) {
    let paths = states.paths();
    let retention = LOG_RETENTION_DAYS * 86_400;
    for task in store.list() {
        let rt = states.get(&task.id);
        if !matches!(
            rt.kind,
            TaskStateKind::Succeeded | TaskStateKind::Failed | TaskStateKind::Cancelled
        ) {
            continue;
        }
        let Some(finished) = rt.finished_at else {
            continue;
        };
        if now.saturating_sub(finished) < retention {
            continue;
        }
        // **不能**以「当前 id.log 存在」为前置：轮转之后可能只剩 `.1/.2`（当前文件被删过/
        // 轮转过），那样它们会被永久遗留（审查实测）。这里直接按统一入口清（不存在即 no-op），
        // 只在确实清掉了东西时打日志。
        let had_any = (0..LOG_KEEP_FILES).any(|i| {
            let p = if i == 0 {
                paths.log_file(&task.id)
            } else {
                std::path::PathBuf::from(format!("{}.{i}", paths.log_file(&task.id).display()))
            };
            p.exists()
        });
        crate::task_store::remove_task_logs(paths, &task.id);
        if had_any {
            crate::log!(
                "[task] {} 的日志已超保留期（{} 天）→ 回收（定义与运行态保留）",
                &task.id[..task.id.len().min(12)],
                LOG_RETENTION_DAYS
            );
        }
    }
}

/// 认领下一条到点的任务（`now` 注入便于单测）。
fn next_due_task(store: &TaskStore, states: &TaskStateStore, now: u64) -> Option<Task> {
    store
        .list()
        .into_iter()
        .find(|t| is_due_for_claim(t, &states.get(&t.id), now))
}

/// 启动清理：上次进程残留的 `Running` 是孤儿（执行器随进程一起没了）。
///
/// **必须有上界**（审查 B3）：重跑的是**整条 prompt**，不是只重投结果——发消息、
/// 改文件、推流这些副作用会整体重放；若某条 prompt 正好能把 ABB 跑挂（OOM/自杀
/// 命令），无上界重跑就会变成「崩溃→重启→再崩」的循环。
/// 故用 `limits.max_restarts` 定上界（默认 [`crate::task_store::DEFAULT_MAX_RESTARTS`]）：
/// 未超限 → 归位 `Pending` 并 `restarts += 1`；超限 → `Failed`，由用户显式重跑。
///
/// 同时清掉「有运行态但没有定义」的孤儿条目（`task rm` 与 worker 认领的竞态会留下）。
fn requeue_orphans(store: &TaskStore, states: &TaskStateStore) {
    let tasks = store.list();
    for t in &tasks {
        let rt = states.get(&t.id);
        if rt.kind != TaskStateKind::Running {
            continue;
        }
        let short = &t.id[..t.id.len().min(12)];
        if rt.restarts < t.limits.max_restarts {
            crate::log!(
                "[task] 上次运行残留 Running（{short}）→ 归位 Pending 重跑（第 {} 次，上限 {}）",
                rt.restarts + 1,
                t.limits.max_restarts
            );
            let _ = states.set(
                &t.id,
                TaskRuntime {
                    kind: TaskStateKind::Pending,
                    restarts: rt.restarts + 1,
                    ..rt
                },
            );
        } else {
            crate::log!(
                "[task] 上次运行残留 Running（{short}）→ 重跑已达上限 {}，标记 Failed",
                t.limits.max_restarts
            );
            let _ = states.set(
                &t.id,
                TaskRuntime {
                    kind: TaskStateKind::Failed,
                    finished_at: Some(crate::chrono_lite::unix_secs()),
                    last_error: "上次运行被进程退出中断（未自动重跑，可手动重跑）".to_string(),
                    ..rt
                },
            );
        }
    }
    // 无定义的残留运行态：状态行与**日志一起**清掉（P2b-D：只删定义会留下孤儿日志，
    // 而 `task rm` 之外没有任何入口能再看到它们）。
    let known: std::collections::HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    for id in states.ids() {
        if !known.contains(id.as_str()) {
            let _ = states.remove(&id);
            // 只有**合法 id** 才按它删日志：状态文件的键不受 `validate_task_id` 约束，
            // 而 `safe_path_component` 是**有损映射**（`a/b` → `a_b`）——若照它删日志，
            // 一个恶意/损坏的键 `a/b` 会把**活任务 `a_b` 的日志**删掉（审查实测）。
            // 非法键只丢状态行，不碰任何文件。
            if crate::task_store::validate_task_id(&id).is_ok() {
                // 当前文件 + 轮转历史一起清（只删当前会留下 .1/.2 永久孤儿）
                crate::task_store::remove_task_logs(states.paths(), &id);
            } else {
                crate::log!("[task] 丢弃非法 id 的运行态（{id:?}）：不按它拼路径删日志");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_store::{CreatedBy, TaskDelivery, TaskLimits, TaskPayload, TaskTrigger};

    /// 每个用例一个 temp 根目录：**绝不碰用户真实的 `~/.agent-bridge`**。
    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("abb-task-test-{tag}-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&p);
        p
    }

    fn now_task(bot: &str, id: &str, chat: &str) -> Task {
        Task {
            schema_version: crate::task_store::TASK_SCHEMA_VERSION,
            id: id.to_string(),
            name: "t".to_string(),
            legacy_job_id: String::new(),
            bot_key: bot.to_string(),
            created_by: CreatedBy {
                role: crate::config::SenderRole::Owner,
                bot_key: bot.to_string(),
                chat_id: chat.to_string(),
            },
            payload: TaskPayload {
                kind: PayloadKind::Agent,
                prompt: "do it".to_string(),
                ..Default::default()
            },
            trigger: TaskTrigger {
                kind: TriggerKind::Now,
                ..Default::default()
            },
            resume_on_boot: crate::task_store::DEFAULT_RESUME_ON_BOOT,
            delivery: TaskDelivery::default(),
            limits: TaskLimits::default(),
        }
    }

    #[cfg(unix)]
    fn keepalive_task(
        bot: &str,
        id: &str,
        _workspace: &std::path::Path,
        script: String,
        resume_on_boot: bool,
    ) -> Task {
        let mut task = now_task(bot, id, "c0");
        task.payload.kind = PayloadKind::Proc;
        task.payload.prompt.clear();
        task.payload.cmd = vec!["/bin/sh".into(), "-c".into(), script];
        task.trigger = TaskTrigger {
            kind: TriggerKind::Keepalive,
            ..Default::default()
        };
        task.resume_on_boot = resume_on_boot;
        // 常驻测试由 stop/cancel 收尾，不走生产默认的 30 分钟回合预算。
        task.limits.timeout_secs = 0;
        task
    }

    #[cfg(unix)]
    fn pgrep_count(tag: &str) -> usize {
        let pattern = format!("[{}]{}", &tag[..1], &tag[1..]);
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("pgrep -f '{pattern}' || true"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    }

    #[cfg(unix)]
    fn spawn_probe(script: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt;

        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(script).process_group(0);
        command.spawn().unwrap()
    }

    #[cfg(unix)]
    fn kill_probe_group(pid: u32) {
        use nix::sys::signal::{killpg, Signal};
        use nix::unistd::Pid;

        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }

    #[cfg(unix)]
    async fn wait_for_count(tag: &str, want: usize, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if pgrep_count(tag) == want {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待 pgrep tag={tag} count={want} 超时，实际 {}",
                pgrep_count(tag)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    fn wait_for_count_sync(tag: &str, want: usize, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while pgrep_count(tag) != want {
            assert!(
                std::time::Instant::now() < deadline,
                "等待 pgrep tag={tag} count={want} 超时，实际 {}",
                pgrep_count(tag)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn dead_identity(pid: u32) -> crate::task_identity::ProcIdentity {
        crate::task_identity::ProcIdentity {
            pid,
            start_token: "dead-start".into(),
            command_line: "dead-command".into(),
        }
    }

    /// 失败抬头必须带**任务身份 + 排查入口**（跨机器/多任务时唯一的定位线索）。
    /// 这条针对的是线上真事：只收到「执行超时（预算 7200s）」这种通知，人无法判断
    /// 是哪条任务失败、去哪看原因。
    #[test]
    fn delivery_header_carries_task_identity_on_failure() {
        let mut t = now_task("b", "tk_20260918_abc123", "oc_1");
        t.name = "长跑探针".to_string();

        let failed = delivery_header(&t, true);
        assert!(failed.contains("后台任务失败"), "{failed}");
        assert!(failed.contains("长跑探针"), "要带任务名：{failed}");
        assert!(
            failed.contains("tk_20260918_abc123"),
            "要带任务 id：{failed}"
        );
        // 审查抓到的假绿：只断言「id 出现在某处」会被后面命令里的同一个 id 满足——
        // 删掉独立的「<id>（…）」那一段测试照样绿。这里锁住**独立 id 段**本身。
        assert!(
            failed.contains(&format!("\n{}（", t.id)),
            "独立的 task id 段缺失（不是只在命令里出现）：{failed}"
        );
        assert!(
            failed.contains("task status tk_20260918_abc123"),
            "要给排查入口：{failed}"
        );
        assert!(
            failed.contains("task logs tk_20260918_abc123"),
            "要给日志入口：{failed}"
        );

        // 成功抬头保持原样（含任务名）；无 name 时回落 id 前 12 位
        let ok = delivery_header(&t, false);
        assert!(ok.starts_with("🤖 后台任务完成：长跑探针"), "{ok}");
        let anon = now_task("b", "tk_20260918_abcdef", "oc_1"); // name 默认是 "t"
        let mut anon = anon;
        anon.name = String::new();
        let text = delivery_header(&anon, false);
        assert!(text.contains("tk_20260918"), "无 name 用 id：{text}");
    }

    /// 安全审查 B1 的回归锁：granted 建的任务必须拿 granted 剖面（workspace-write +
    /// restricted shell + NO_HINTS），owner 的才跟 bot 配置档走。
    /// 这条锁的是**装配处的接线**——只测 `oneshot_agent_config_for_role` 本身
    /// 无法证明 `run_one` 真的把 `created_by.role` 传下去了。
    #[test]
    fn granted_task_gets_restricted_profile() {
        let bot = crate::config::BotConfig {
            name: "taskrole-wire".into(),
            kind: "feishu".into(),
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        // 未知 bot_key → restrict_granted 对 Granted 取安全默认 true（不依赖本机 config 内容）
        let bot_key = "no-such-bot-for-taskrole-test";

        let mut g = now_task(bot_key, "tk_g", "c");
        g.created_by.role = crate::config::SenderRole::Granted;
        let gc = agent_cfg_for_task(&bot, &cfg, &g, bot_key);
        let sb = gc
            .session_sandbox
            .expect("granted 任务必须有档位载荷（None = FullAccess，等于提权）");
        assert_eq!(sb.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(sb.shell.as_deref(), Some("restricted"));
        assert!(
            gc.extra_env
                .iter()
                .any(|(k, v)| k == "BUZZ_AGENT_NO_HINTS" && v == "1"),
            "granted 任务要带进程级 hints 收口"
        );

        let mut o = g.clone();
        o.created_by.role = crate::config::SenderRole::Owner;
        let oc = agent_cfg_for_task(&bot, &cfg, &o, bot_key);
        assert!(
            oc.session_sandbox.is_none(),
            "owner 任务不该被额外收紧（默认档位 FullAccess → None）"
        );
        assert!(!oc.extra_env.iter().any(|(k, _)| k == "BUZZ_AGENT_NO_HINTS"));
    }

    /// P1b：任务完成投递的来源——自环（目标==创建者会话）记 `InSession`（保持旧行为），
    /// 显式跨会话记 `TaskCompletion`。
    #[test]
    fn task_delivery_origin_is_in_session_only_for_self_loop() {
        assert_eq!(
            task_delivery_origin(true),
            crate::deliver::DeliveryOrigin::InSession
        );
        assert_eq!(
            task_delivery_origin(false),
            crate::deliver::DeliveryOrigin::TaskCompletion
        );
    }

    #[test]
    fn default_delivery_is_creator_session() {
        let t = now_task("b", "tk_x", "wx_chat");
        assert_eq!(
            delivery_target(&t, "b"),
            Some(("b".to_string(), "wx_chat".to_string()))
        );
    }

    #[test]
    fn explicit_targets_win_over_creator() {
        let mut t = now_task("b", "tk_x", "wx_chat");
        t.delivery.targets = vec![crate::task_store::TaskTarget {
            bot_key: String::new(),
            chat_id: "other".to_string(),
        }];
        assert_eq!(
            delivery_target(&t, "b"),
            Some(("b".to_string(), "other".to_string())),
            "targets.bot_key 留空 = 本 bot"
        );
    }

    /// #306：`--to other_bot:oc_x` 必须**真的投到那个 bot**（旧实现忽略 bot_key 一律
    /// 按本 bot 投，所以 validate 里曾显式拒绝跨 bot；两处必须一起改）。
    #[test]
    fn explicit_cross_bot_target_is_honored() {
        let mut t = now_task("b", "tk_x", "wx_chat");
        t.delivery.targets = vec![crate::task_store::TaskTarget {
            bot_key: "other_bot".to_string(),
            chat_id: "oc_x".to_string(),
        }];
        assert_eq!(
            delivery_target(&t, "b"),
            Some(("other_bot".to_string(), "oc_x".to_string()))
        );
    }

    /// 没有创建者会话（人工 CLI 建的）时必须**返回 None**，让调用方明确报「没目标」，
    /// 而不是投到空串（那会变成一条静默失败）。
    #[test]
    fn no_creator_and_no_target_means_no_delivery() {
        let t = now_task("b", "tk_x", "");
        assert_eq!(delivery_target(&t, "b"), None);
    }

    /// 触发档 → 认领判定（P2b-C 编排的**核心语义**，纯函数逐个锁死）。
    #[test]
    fn trigger_claim_semantics_per_kind() {
        let mut t = now_task("b", "tk_x", "c");
        let pending = TaskRuntime::default();
        // now：登记即跑一次；跑过就是跑过了
        assert!(
            is_due_for_claim(&t, &pending, 1_000),
            "now + Pending 应可认领"
        );
        let done = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            ..Default::default()
        };
        assert!(!is_due_for_claim(&t, &done, 1_000), "now 跑过不再认领");

        // once：到点才跑；**过期补跑**（与 job 调度器 is_due 语义一致）
        t.trigger = TaskTrigger {
            kind: TriggerKind::Once,
            expr: "2026-09-20 09:00".to_string(),
            ..Default::default()
        };
        let due = crate::schedule::parse_once("2026-09-20 09:00")
            .unwrap()
            .to_unix() as u64;
        assert!(
            !is_due_for_claim(&t, &pending, due - 60),
            "once 未到点不认领"
        );
        assert!(is_due_for_claim(&t, &pending, due), "once 到点认领");
        assert!(is_due_for_claim(&t, &pending, due + 3600), "once 过期补跑");
        assert!(
            !is_due_for_claim(&t, &done, due + 3600),
            "once 跑过不再认领"
        );

        // cron：新登记（无记账）当前分钟匹配才跑，且同一分钟只触发一次
        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "* * * * *".to_string(),
            ..Default::default()
        };
        let now = 1_700_000_030u64; // 固定时刻，避免跨分钟抖动
        assert!(
            is_due_for_claim(&t, &pending, now),
            "cron 匹配分钟且未跑过 → 认领"
        );
        let fired_this_minute = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(now - 10),
            ..Default::default()
        };
        assert!(
            !is_due_for_claim(&t, &fired_this_minute, now),
            "同一分钟内不得重复触发（worker 每 2s 轮询）"
        );
        assert!(
            is_due_for_claim(&t, &fired_this_minute, now + 60),
            "下一个分钟桶应再次可认领"
        );
        // 不匹配的分钟：用「当前分钟 +1」构造表达式（同一小时内，不会与本分钟相等）
        let dt = crate::schedule::DateTime::from_unix(now as i64);
        t.trigger.expr = format!("{} {} * * *", (dt.minute + 1) % 60, dt.hour);
        assert!(
            !is_due_for_claim(&t, &pending, now),
            "cron 不匹配当前分钟不认领（expr={}）",
            t.trigger.expr
        );

        // interval：首次即跑，之后每 N 秒
        t.trigger = TaskTrigger {
            kind: TriggerKind::Interval,
            expr: "5m".to_string(),
            ..Default::default()
        };
        assert!(is_due_for_claim(&t, &pending, now), "interval 首次即跑");
        let just_ran = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(now),
            ..Default::default()
        };
        assert!(!is_due_for_claim(&t, &just_ran, now + 60), "间隔未到不认领");
        assert!(
            is_due_for_claim(&t, &just_ran, now + 300),
            "满一个间隔后可认领"
        );
        // 时钟回拨/手改状态文件：记账时刻在"未来"时不能死等（否则任务一直不跑）
        let future = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(now + 86_400),
            ..Default::default()
        };
        assert!(
            is_due_for_claim(&t, &future, now),
            "last_fired_at 在未来 → 不信记账，按到点处理（时钟回拨护栏）"
        );

        // keepalive：Pending 可拉起；Backoff 到期且无旧代际时才可拉起
        t.trigger = TaskTrigger {
            kind: TriggerKind::Keepalive,
            ..Default::default()
        };
        t.payload.kind = PayloadKind::Proc;
        t.payload.prompt.clear();
        t.payload.cmd = vec!["/bin/true".into()];
        assert!(
            is_due_for_claim(&t, &pending, now),
            "keepalive Pending 应可拉起"
        );
        let waiting = TaskRuntime {
            kind: TaskStateKind::Backoff,
            next_retry_at: Some(now + 10),
            ..Default::default()
        };
        assert!(!is_due_for_claim(&t, &waiting, now), "退避未到不拉起");
        assert!(
            is_due_for_claim(&t, &waiting, now + 10),
            "退避到期应可重新拉起"
        );
        let held = TaskRuntime {
            kind: TaskStateKind::Backoff,
            next_retry_at: Some(now),
            proc_identity: Some(crate::task_identity::ProcIdentity {
                pid: 999_999,
                start_token: "dead-start".into(),
                command_line: "dead-command".into(),
            }),
            ..Default::default()
        };
        assert!(
            !is_due_for_claim(&t, &held, now),
            "仍带代际身份时不得在 Backoff 直接再拉一个"
        );

        // 任何档在 Running 时都不认领（同任务不并发）；Cancelled 是用户终态
        let running = TaskRuntime {
            kind: TaskStateKind::Running,
            ..Default::default()
        };
        assert!(!is_due_for_claim(&t, &running, now), "Running 不认领");
        let cancelled = TaskRuntime {
            kind: TaskStateKind::Cancelled,
            ..Default::default()
        };
        assert!(!is_due_for_claim(&t, &cancelled, now), "Cancelled 不认领");
    }

    #[test]
    fn missed_schedules_do_not_backfill_history() {
        let mut t = now_task("b", "tk_schedule", "c");
        let pending = TaskRuntime::default();

        let due = crate::schedule::parse_once("2026-09-20 09:00")
            .unwrap()
            .to_unix() as u64;
        t.trigger = TaskTrigger {
            kind: TriggerKind::Once,
            expr: "2026-09-20 09:00".into(),
            ..Default::default()
        };
        assert!(is_due_for_claim(&t, &pending, due + 10 * 60));
        let once_done = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            ..Default::default()
        };
        assert!(
            !is_due_for_claim(&t, &once_done, due + 10 * 60),
            "once 过期只补 1 次，跑过后不得再次认领"
        );

        let cron_105 = crate::schedule::parse_once("2026-09-20 10:05")
            .unwrap()
            .to_unix() as u64;
        let cron_1100 = crate::schedule::parse_once("2026-09-20 11:00")
            .unwrap()
            .to_unix() as u64;
        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "0 * * * *".into(),
            ..Default::default()
        };
        assert!(
            !is_due_for_claim(&t, &pending, cron_105),
            "cron 错过 10 分钟不得补 10 次历史触发"
        );
        assert!(
            is_due_for_claim(&t, &pending, cron_1100),
            "cron 到下一个当前匹配分钟只认领一次"
        );

        t.trigger = TaskTrigger {
            kind: TriggerKind::Interval,
            expr: "5m".into(),
            ..Default::default()
        };
        let base = 1_700_000_000;
        let idle = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(base),
            ..Default::default()
        };
        assert!(
            is_due_for_claim(&t, &idle, base + 20 * 60),
            "interval 跨多周期后应补一轮"
        );
        let claimed = claim(&idle, base + 20 * 60);
        assert!(
            !is_due_for_claim(&t, &claimed, base + 20 * 60),
            "interval 跨多周期最多补一轮，不能在同一轮内连续补"
        );
    }

    /// cron 逾期认领：执行位被长任务占住而错过的分钟点，要在 worker 空出来后补跑一次，
    /// 且不重放整段历史、不在同一分钟里重复触发。这条是「定时任务被静默吞掉」的核心回归锁。
    #[test]
    fn cron_catches_up_missed_minute_once() {
        let mut t = now_task("b", "tk_cron_late", "c");
        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "*/10 * * * *".into(),
            ..Default::default()
        };
        // 10:00 那一轮正常跑过（认领时记账 10:00:05）
        let base = crate::schedule::parse_once("2026-09-20 10:00")
            .unwrap()
            .to_unix() as u64;
        let fired_1000 = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(base + 5),
            ..Default::default()
        };
        // 执行位被长任务占住到 10:25 → 10:10 与 10:20 两个分钟点都没轮到
        let busy_until = base + 25 * 60;
        assert_eq!(
            due_for_claim(&t, &fired_1000, busy_until),
            Some(base + 10 * 60),
            "应到的时刻取最近一次错过的分钟点（用于日志里如实记「迟到多久」）"
        );
        assert!(
            is_due_for_claim(&t, &fired_1000, busy_until),
            "逾期后必须可认领（改前这里恒为 false = 静默丢弃）"
        );

        // 补跑的那一轮跑完了（10:25 认领、记账在 10:25）→ 同一分钟不再认领
        let after_catch_up = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(busy_until),
            ..Default::default()
        };
        assert!(
            !is_due_for_claim(&t, &after_catch_up, busy_until + 30),
            "补跑后同一分钟内不得重复触发"
        );
        // 只补一次：不会把 10:20 那一轮也补出来，下一轮按 10:30 的正常到点走
        assert_eq!(
            due_for_claim(&t, &after_catch_up, base + 30 * 60),
            Some(base + 30 * 60),
            "跨越多个错过的分钟点也只认领一次"
        );
        assert!(
            is_due_for_claim(&t, &after_catch_up, base + 30 * 60),
            "补跑不影响后续正常到点"
        );
    }

    /// cron 逾期认领的边界：新登记不追溯历史周期；无命中点不认领；
    /// 记账在未来（时钟回拨/手改状态文件）按当前分钟匹配。
    #[test]
    fn cron_catch_up_boundaries() {
        let mut t = now_task("b", "tk_cron_edge", "c");
        let base = crate::schedule::parse_once("2026-09-20 10:00")
            .unwrap()
            .to_unix() as u64;

        // ① 新登记（无记账）不追溯：10:05 登记 → 10:10 才跑
        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "*/10 * * * *".into(),
            ..Default::default()
        };
        let pending = TaskRuntime::default();
        assert!(
            !is_due_for_claim(&t, &pending, base + 5 * 60),
            "新登记的任务不补它登记之前的历史周期"
        );
        assert!(
            is_due_for_claim(&t, &pending, base + 10 * 60),
            "到下一个命中分钟照常认领"
        );

        // ② 窗口内最近一次错过点会补：每天 09:00 的任务，09:00 那轮被占住 → 12:00 补跑一次
        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "0 9 * * *".into(),
            ..Default::default()
        };
        let yesterday_9 = crate::schedule::parse_once("2026-09-19 09:00")
            .unwrap()
            .to_unix() as u64;
        let fired_yesterday = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(yesterday_9),
            ..Default::default()
        };
        let today_9 = crate::schedule::parse_once("2026-09-20 09:00")
            .unwrap()
            .to_unix() as u64;
        assert_eq!(
            due_for_claim(&t, &fired_yesterday, base + 2 * 3600),
            Some(today_9),
            "当天 09:00 被占住 → 12:00 认领时补这一轮"
        );

        // ③ 扫描窗口（{}h）内没有命中点就不补：周期长于窗口的任务（如每年 1 月 1 日）
        //    在非命中时刻不得因为「一年前有一轮」而被拉起来补跑。
        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "0 0 1 1 *".into(),
            ..Default::default()
        };
        let long_ago = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(today_9 - 365 * 86400),
            ..Default::default()
        };
        assert!(
            !is_due_for_claim(&t, &long_ago, base + 2 * 3600),
            "窗口（{}h）内没有命中点 → 不认领，不追溯更早的周期",
            CRON_CATCHUP_WINDOW_SECS / 3600
        );
        let new_year = crate::schedule::parse_once("2027-01-01 00:00")
            .unwrap()
            .to_unix() as u64;
        assert!(
            is_due_for_claim(&t, &long_ago, new_year),
            "命中分钟照常认领"
        );

        // ④ 记账在未来（时钟回拨）→ 不信这笔记账，按当前分钟匹配
        t.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "* * * * *".into(),
            ..Default::default()
        };
        let skewed = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            last_fired_at: Some(base + 3600),
            ..Default::default()
        };
        assert!(
            is_due_for_claim(&t, &skewed, base),
            "未来记账不得把任务卡死，按当前分钟匹配处理"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalive_resume_false_cleans_stale_runtime_without_process() {
        let root = tmp_root("ka_no_resume");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let tag = format!("ABB_KEEPALIVE_{}", uuid::Uuid::new_v4().simple());
        let task = keepalive_task(
            "b",
            "tk_ka_no_resume",
            &root,
            format!("echo {tag}; while :; do sleep 1; done"),
            false,
        );
        store.add(task.clone()).unwrap();
        states
            .set(
                &task.id,
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    pid: Some(999_999),
                    proc_identity: Some(dead_identity(999_999)),
                    started_at: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();

        recover_keepalives(&store, &states, crate::chrono_lite::unix_secs()).await;

        let rt = states.get(&task.id);
        assert_eq!(rt.kind, TaskStateKind::Interrupted);
        assert!(rt.pid.is_none() && rt.proc_identity.is_none());
        assert!(rt.last_error.contains("resume_on_boot=false"));
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "resume_on_boot=false 的 keepalive 重启后不得自动拉起"
        );
        assert_eq!(pgrep_count(&tag), 0, "不得产生任务进程");
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalive_resume_false_stops_live_old_generation() {
        let root = tmp_root("ka_no_resume_live");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let tag = format!("ABB_KEEPALIVE_{}", uuid::Uuid::new_v4().simple());
        let pidfile = root.join("probe.pid");
        let script = format!(
            "echo $$ > {}; echo {tag}; while :; do sleep 1; done",
            pidfile.display()
        );
        let mut task = keepalive_task("b", "tk_ka_no_resume_live", &root, script.clone(), false);
        task.limits.grace_secs = 1;
        store.add(task.clone()).unwrap();
        let mut child = spawn_probe(&script);
        wait_for_count_sync(&tag, 1, Duration::from_secs(3));
        let pid = child.id();
        let identity = crate::task_identity::capture(pid).expect("探针进程必须可采集代际身份");
        states
            .set(
                &task.id,
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    pid: Some(pid),
                    proc_identity: Some(identity),
                    started_at: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();

        // 测试进程仍由本测试持有；并发 reap，复现生产里 orphan 被 launchd 回收后的
        // “进程组消失”语义，否则僵尸会让 group_alive 一直为真。
        let waiter = tokio::task::spawn_blocking(move || child.wait());
        let recovery = recover_keepalives(&store, &states, crate::chrono_lite::unix_secs());
        let (_, wait_result) = tokio::join!(recovery, waiter);
        assert!(wait_result.unwrap().is_ok(), "探针必须被停止链回收");

        let rt = states.get(&task.id);
        let count = pgrep_count(&tag);
        assert_eq!(rt.kind, TaskStateKind::Interrupted);
        assert!(rt.pid.is_none() && rt.proc_identity.is_none());
        assert!(rt.last_error.contains("收掉旧进程"), "{}", rt.last_error);
        assert_eq!(
            count, 0,
            "resume_on_boot=false 必须在重启恢复时收干净旧进程"
        );
        assert!(next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none());
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalive_resume_false_does_not_kill_reused_unrelated_pid() {
        let root = tmp_root("ka_no_resume_reuse");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let task = keepalive_task(
            "b",
            "tk_ka_no_resume_reuse",
            &root,
            "while :; do sleep 1; done".into(),
            false,
        );
        store.add(task.clone()).unwrap();
        let mut child = spawn_probe("while :; do sleep 1; done");
        states
            .set(
                &task.id,
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    pid: Some(child.id()),
                    // 同 PID 但启动标记/命令行是旧代际：verify 必须判 Dead，不能误杀无关进程。
                    proc_identity: Some(crate::task_identity::ProcIdentity {
                        pid: child.id(),
                        start_token: "stale-start-token".into(),
                        command_line: "stale-command".into(),
                    }),
                    started_at: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();

        recover_keepalives(&store, &states, crate::chrono_lite::unix_secs()).await;

        let unrelated_alive = child.try_wait().unwrap().is_none();
        kill_probe_group(child.id());
        let _ = child.wait();
        assert!(unrelated_alive, "PID 已复用为无关进程时不得误杀");
        let rt = states.get(&task.id);
        assert_eq!(rt.kind, TaskStateKind::Interrupted);
        assert!(rt.pid.is_none() && rt.proc_identity.is_none());
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    // Windows 未接入 Job Object，proc 载荷在 validate 里就被平台闸拒绝；本用例的
    // keepalive 任务全是 proc 载荷（keepalive 只支持 proc），在该平台连构造都做不到。
    // 与同文件其它 proc/进程组用例一致按 unix 门控。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_cancels_pending_and_backoff_keepalives() {
        let root = tmp_root("ka_shutdown");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        for (id, kind) in [
            ("tk_ka_shutdown_pending", TaskStateKind::Pending),
            ("tk_ka_shutdown_backoff", TaskStateKind::Backoff),
        ] {
            let mut task = now_task("b", id, "c");
            task.payload.kind = PayloadKind::Proc;
            task.payload.prompt.clear();
            task.payload.cmd = vec!["/bin/true".into()];
            task.trigger = TaskTrigger {
                kind: TriggerKind::Keepalive,
                ..Default::default()
            };
            store.add(task.clone()).unwrap();
            states
                .set(
                    &task.id,
                    TaskRuntime {
                        kind,
                        next_retry_at: (kind == TaskStateKind::Backoff).then_some(9_999_999_999),
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let bot = crate::config::BotConfig {
            name: "b".into(),
            kind: "feishu".into(),
            app_id: "b".into(),
            ..Default::default()
        };
        let router = Arc::new(Router::new(
            false,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            None,
        ));
        task_worker_with_stores(
            bot,
            Arc::new(crate::config::Config::default()),
            router,
            store,
            states,
            stop,
            Duration::from_millis(10),
        )
        .await;

        // 从盘上重新加载，证明终态化由真实 worker 关停路径落盘，而不是只在内存里。
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");

        for id in ["tk_ka_shutdown_pending", "tk_ka_shutdown_backoff"] {
            assert_eq!(states.get(id).kind, TaskStateKind::Cancelled, "{id}");
            assert!(states.get(id).next_retry_at.is_none(), "{id}");
        }
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "Pending/Backoff keepalive 正常关停后重启不得拉起"
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalive_resume_true_without_identity_only_cleans_does_not_adopt() {
        let root = tmp_root("ka_no_identity");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let tag = format!("ABB_KEEPALIVE_{}", uuid::Uuid::new_v4().simple());
        let task = keepalive_task(
            "b",
            "tk_ka_no_identity",
            &root,
            format!("echo {tag}; while :; do sleep 1; done"),
            true,
        );
        store.add(task.clone()).unwrap();
        states
            .set(
                &task.id,
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    pid: Some(999_999),
                    started_at: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();

        recover_keepalives(&store, &states, crate::chrono_lite::unix_secs()).await;
        let rt = states.get(&task.id);
        assert_eq!(rt.kind, TaskStateKind::Interrupted);
        assert!(rt.pid.is_none() && rt.proc_identity.is_none());
        assert!(rt.last_error.contains("缺少代际身份"));
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "无身份时只清理不 adopt，也不得自动拉起"
        );
        assert_eq!(pgrep_count(&tag), 0);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn keepalive_backoff_schedule_is_bounded_and_monotonic() {
        assert_eq!(keepalive_backoff_secs(1), 1);
        assert_eq!(keepalive_backoff_secs(2), 2);
        assert_eq!(keepalive_backoff_secs(3), 4);
        assert_eq!(keepalive_backoff_secs(4), 8);
        assert_eq!(keepalive_backoff_secs(5), 30);
        assert_eq!(keepalive_backoff_secs(99), 30, "退避封顶，防止无限增长");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalive_resume_true_starts_exactly_one_and_updates_generation() {
        let root = tmp_root("ka_resume");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let tag = format!("ABB_KEEPALIVE_{}", uuid::Uuid::new_v4().simple());
        let pidfile = root.join("probe.pid");
        let script = format!(
            "echo $$ > {}; echo {tag}; while :; do sleep 1; done",
            pidfile.display()
        );
        let task = keepalive_task("b", "tk_ka_resume", &root, script, true);
        store.add(task.clone()).unwrap();
        // 模拟 service 上次崩溃：状态残留 Running，但代际身份对应的 pid 已死。
        states
            .set(
                &task.id,
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    pid: Some(999_999),
                    proc_identity: Some(dead_identity(999_999)),
                    started_at: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();

        recover_keepalives(&store, &states, 2).await;
        let resumed = states.get(&task.id);
        assert_eq!(resumed.kind, TaskStateKind::Backoff);
        assert_eq!(resumed.restarts, 1, "恢复本身计入 keepalive 重拉次数");
        let mut due = resumed;
        due.next_retry_at = Some(0);
        states.set(&task.id, due).unwrap();
        let due_task = next_due_task(&store, &states, 2).expect("退避到期应可认领");
        let prev = states.get(&task.id);
        states.set(&task.id, claim(&prev, 3)).unwrap();

        let stop = tokio_util::sync::CancellationToken::new();
        let workspace = root.display().to_string();
        let attempt = run_keepalive_attempt(&due_task, &workspace, &states, &stop);
        tokio::pin!(attempt);
        tokio::select! {
            _ = &mut attempt => panic!("常驻探针不应自行退出"),
            _ = wait_for_count(&tag, 1, Duration::from_secs(5)) => {}
        }

        let rt = states.get(&task.id);
        assert_eq!(rt.kind, TaskStateKind::Running);
        assert_eq!(pgrep_count(&tag), 1, "恰好一个新实例");
        assert!(
            rt.started_at.is_some_and(|t| t > 1),
            "恢复后 started_at 必须更新"
        );
        assert_eq!(
            rt.restarts, 1,
            "keepalive 恢复次数与 agent orphan 语义分开记账"
        );
        assert!(rt.proc_identity.is_some(), "新代际必须落盘身份");

        stop.cancel();
        attempt.await;
        assert_eq!(states.get(&task.id).kind, TaskStateKind::Cancelled);
        assert_eq!(pgrep_count(&tag), 0, "关停后探针必须被整组收掉");
        recover_keepalives(&store, &states, crate::chrono_lite::unix_secs()).await;
        assert_eq!(
            states.get(&task.id).kind,
            TaskStateKind::Cancelled,
            "service 正常关停后模拟重启不得自动拉起"
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalive_live_old_generation_is_not_adopted_or_duplicated() {
        let root = tmp_root("ka_live");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let tag = format!("ABB_KEEPALIVE_{}", uuid::Uuid::new_v4().simple());
        let pidfile = root.join("probe.pid");
        let script = format!(
            "echo $$ > {}; echo {tag}; while :; do sleep 1; done",
            pidfile.display()
        );
        let task = keepalive_task("b", "tk_ka_live", &root, script.clone(), true);
        store.add(task.clone()).unwrap();
        let mut child = spawn_probe(&script);
        wait_for_count_sync(&tag, 1, Duration::from_secs(3));
        let identity =
            crate::task_identity::capture(child.id()).expect("探针进程必须可采集代际身份");
        states
            .set(
                &task.id,
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    pid: Some(child.id()),
                    proc_identity: Some(identity),
                    started_at: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();

        recover_keepalives(&store, &states, crate::chrono_lite::unix_secs()).await;
        let rt = states.get(&task.id);
        let due = next_due_task(&store, &states, crate::chrono_lite::unix_secs());
        let count = pgrep_count(&tag);
        kill_probe_group(child.id());
        let _ = child.wait();

        assert_eq!(rt.kind, TaskStateKind::Interrupted);
        assert!(rt.last_error.contains("仍存活"));
        assert!(due.is_none(), "活进程未被 adopt 时不得再拉起第二个");
        assert_eq!(count, 1, "旧进程仍应恰好一个，不能出现双实例");
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalive_circuit_breaker_stops_after_three_fast_crashes() {
        let root = tmp_root("ka_breaker");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let tag = format!("ABB_KEEPALIVE_{}", uuid::Uuid::new_v4().simple());
        let pidfile = root.join("crashes.pid");
        let script = format!("echo $$ >> {}; echo {tag}; exit 7", pidfile.display());
        let task = keepalive_task("b", "tk_ka_breaker", &root, script, true);
        store.add(task.clone()).unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let workspace = root.display().to_string();

        for attempt in 1..=3u64 {
            let prev = states.get(&task.id);
            states.set(&task.id, claim(&prev, attempt)).unwrap();
            run_keepalive_attempt(&task, &workspace, &states, &stop).await;
            let rt = states.get(&task.id);
            if attempt < 3 {
                assert_eq!(rt.kind, TaskStateKind::Backoff);
                let mut due = rt;
                due.next_retry_at = Some(0);
                states.set(&task.id, due).unwrap();
            }
        }

        let rt = states.get(&task.id);
        assert_eq!(rt.kind, TaskStateKind::Failed);
        assert!(
            rt.last_error.contains("熔断"),
            "last_error 必须写明熔断原因：{}",
            rt.last_error
        );
        assert_eq!(rt.consecutive_failures, 3);
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "第 4 次不得再拉起"
        );
        let launches = std::fs::read_to_string(&pidfile).unwrap().lines().count();
        assert_eq!(launches, 3, "实际只允许启动 3 次");
        assert_eq!(pgrep_count(&tag), 0);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalive_cancel_before_boot_recovery_stays_cancelled() {
        let root = tmp_root("ka_cancel");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let mut task = now_task("b", "tk_ka_cancel", "c");
        task.payload.kind = PayloadKind::Proc;
        task.payload.prompt.clear();
        task.payload.cmd = vec!["/bin/true".into()];
        task.trigger = TaskTrigger {
            kind: TriggerKind::Keepalive,
            ..Default::default()
        };
        store.add(task.clone()).unwrap();
        std::fs::create_dir_all(paths.cancel_requests_dir()).unwrap();
        std::fs::write(paths.cancel_file(&task.id), b"{}").unwrap();

        consume_cancel_requests(&store, &states);
        recover_keepalives(&store, &states, crate::chrono_lite::unix_secs()).await;

        assert_eq!(states.get(&task.id).kind, TaskStateKind::Cancelled);
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "Cancelled 是终态，模拟重启后仍不得拉起"
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 认领即记 `last_fired_at`（调度记账），且必须带走 `restarts`（既有回归）。
    #[test]
    fn claim_records_last_fired_at_and_preserves_restarts() {
        let prev = TaskRuntime {
            restarts: 2,
            ..Default::default()
        };
        let rt = claim(&prev, 1_700_000_000);
        assert_eq!(rt.kind, TaskStateKind::Running);
        assert_eq!(rt.started_at, Some(1_700_000_000));
        assert_eq!(
            rt.last_fired_at,
            Some(1_700_000_000),
            "认领时刻必须记账（cron 分钟去重 / interval 间隔都靠它）"
        );
        assert_eq!(rt.restarts, 2, "restarts 必须带走（重跑上界）");
    }

    /// 回归（自查抓到）：**跑完一轮不得把 `last_fired_at` 清掉**。
    ///
    /// 终态运行态若用 `..Default::default()` 直接构造，`last_fired_at` 会被清成 None →
    /// cron 的分钟去重失效（同一分钟立刻再触发一轮）、interval 退化成「跑完立刻再来」。
    /// 这条用真 `run_attempt`（mock agent）走完整轮次再断言记账还在。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn run_attempt_preserves_last_fired_at_for_repeating_triggers() {
        let root = tmp_root("lastfired");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);
        let mut task = now_task(bot, "tk_cron", "c0");
        task.trigger = crate::task_store::TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "* * * * *".to_string(),
            ..Default::default()
        };
        task.limits.timeout_secs = 30;
        store.add(task.clone()).unwrap();
        // 取「当前分钟的第 5 秒」而不是裸 `unix_secs()`：断言里要比较"同一分钟"与
        // "下一分钟"，贴着分钟边界取时间会让 `+5s` 跨桶（本用例第一版就这么偶发红过）。
        let fired_at = (crate::chrono_lite::unix_secs() / 60) * 60 + 5;
        states
            .set(
                &task.id,
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    started_at: Some(fired_at),
                    last_fired_at: Some(fired_at),
                    ..Default::default()
                },
            )
            .unwrap();

        let msgr = Arc::new(RecordingMsgr::default());
        let router = Arc::new(Router::new(
            false,
            std::collections::HashMap::from([(
                bot.to_string(),
                msgr.clone() as Arc<dyn crate::messenger::Messenger>,
            )]),
            std::collections::HashMap::new(),
            None,
        ));
        let rec = std::env::temp_dir().join(format!("abb-task-lf-{}.jsonl", uuid::Uuid::new_v4()));
        let stop = tokio_util::sync::CancellationToken::new();
        run_attempt(
            bot,
            task.clone(),
            mock_cfg(&rec),
            std::env::temp_dir().display().to_string(),
            &router,
            &states,
            &stop,
        )
        .await;

        let rt = states.get(&task.id);
        assert_eq!(rt.kind, TaskStateKind::Succeeded, "mock agent 正常应答");
        assert_eq!(
            rt.last_fired_at,
            Some(fired_at),
            "跑完一轮后调度记账必须还在（否则 cron 同分钟重复触发）"
        );
        assert!(
            !is_due_for_claim(&task, &rt, fired_at + 5),
            "同一分钟内不得因记账被清掉而重复触发"
        );
        assert!(
            is_due_for_claim(&task, &rt, fired_at + 60),
            "下一分钟桶应恢复可触发"
        );

        let _ = std::fs::remove_file(&rec);
        let _ = std::fs::remove_dir_all(&states.paths().dir);
    }

    /// 审查回归：**周期任务跑完一轮后（Succeeded）也必须能被取消**，否则下一分钟又跑。
    #[test]
    fn cancel_request_stops_idle_repeating_task() {
        let root = tmp_root("cancel_idle_cron");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let mut t = now_task("b", "tk_cron_idle", "c");
        t.trigger = crate::task_store::TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "* * * * *".to_string(),
            ..Default::default()
        };
        store.add(t.clone()).unwrap();
        // 跑完一轮的空闲态（**非** Running、非 Pending）
        states
            .set(
                &t.id,
                TaskRuntime {
                    kind: TaskStateKind::Succeeded,
                    last_fired_at: Some(crate::chrono_lite::unix_secs()),
                    ..Default::default()
                },
            )
            .unwrap();
        std::fs::create_dir_all(paths.cancel_requests_dir()).unwrap();
        std::fs::write(paths.cancel_file(&t.id), b"{}").unwrap();

        consume_cancel_requests(&store, &states);

        assert_eq!(
            states.get(&t.id).kind,
            TaskStateKind::Cancelled,
            "周期任务空闲态收到取消请求 → 必须转 Cancelled（否则后续触发停不掉）"
        );
        assert!(
            !is_due_for_claim(&t, &states.get(&t.id), crate::chrono_lite::unix_secs()),
            "取消后不得再被认领"
        );

        // 一次性档的 Succeeded 是真终态：取消请求不得把它改成 Cancelled
        let mut once = now_task("b", "tk_once_done", "c");
        once.trigger = crate::task_store::TaskTrigger {
            kind: TriggerKind::Once,
            expr: "2026-01-01 00:00".to_string(),
            ..Default::default()
        };
        store.add(once.clone()).unwrap();
        states
            .set(
                &once.id,
                TaskRuntime {
                    kind: TaskStateKind::Succeeded,
                    ..Default::default()
                },
            )
            .unwrap();
        std::fs::write(paths.cancel_file(&once.id), b"{}").unwrap();
        consume_cancel_requests(&store, &states);
        assert_eq!(
            states.get(&once.id).kind,
            TaskStateKind::Succeeded,
            "一次性任务的终态不得被取消请求改写"
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 审查回归（阻塞）：**非法的状态键不得按清洗后的名字去删活任务的日志**。
    ///
    /// `safe_path_component` 是有损映射（`a/b` → `a_b`），若孤儿清理照它删日志，
    /// 一个损坏/恶意的状态键 `a/b` 会删掉**活任务 `a_b`** 的日志（审查实测）。
    #[test]
    fn orphan_state_key_without_traversal_must_not_delete_other_task_logs() {
        let root = tmp_root("statekey_collision");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        // 活任务 a_b + 它的日志
        let live = now_task("b", "a_b", "c");
        store.add(live.clone()).unwrap();
        let rt = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            ..Default::default()
        };
        write_log(&paths, &live, &rt, "live-log");
        assert!(paths.log_file("a_b").exists(), "前置：活任务日志已写");
        // 恶意/损坏的状态键：清洗后与活任务同名
        states.set("a/b", rt.clone()).unwrap();

        requeue_orphans(&store, &states);

        assert!(
            paths.log_file("a_b").exists(),
            "活任务 a_b 的日志不得被他人的非法状态键 a/b 连坐删除"
        );
        assert!(
            states.get("a/b").kind == TaskStateKind::Pending,
            "非法状态键应被丢弃（回到默认态）"
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 审查回归：终态 GC 在**只剩轮转文件**（当前 `.log` 不存在）时也必须清干净。
    #[test]
    fn gc_logs_removes_rotated_files_even_without_current() {
        let root = tmp_root("gc_rotated_only");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let t = now_task("b", "tk_rot_only", "c");
        store.add(t.clone()).unwrap();
        let now = crate::chrono_lite::unix_secs();
        states
            .set(
                &t.id,
                TaskRuntime {
                    kind: TaskStateKind::Succeeded,
                    finished_at: Some(now - (LOG_RETENTION_DAYS + 1) * 86_400),
                    ..Default::default()
                },
            )
            .unwrap();
        std::fs::create_dir_all(paths.logs_dir()).unwrap();
        let base = paths.log_file(&t.id);
        // 只留轮转历史（当前文件不存在）
        std::fs::write(base.with_file_name("tk_rot_only.log.1"), b"old1").unwrap();
        std::fs::write(base.with_file_name("tk_rot_only.log.2"), b"old2").unwrap();

        gc_logs(&store, &states, now);

        assert!(
            !base.with_file_name("tk_rot_only.log.1").exists()
                && !base.with_file_name("tk_rot_only.log.2").exists(),
            "只剩轮转文件时也必须按保留期清掉（否则永久遗留）"
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 审查回归：`remove_task_logs` 必须把轮转历史一并清掉（`task rm` 只删当前文件会留孤儿）。
    #[test]
    fn remove_task_logs_drops_rotated_history() {
        let root = tmp_root("rmlogs");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let t = now_task("b", "tk_rm", "c");
        let rt = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            ..Default::default()
        };
        let mut tiny = t.clone();
        tiny.limits.log_max_bytes = 1;
        for i in 0..4 {
            write_log(&paths, &tiny, &rt, &format!("round-{i}-xxxxxxxxxx"));
        }
        let base = paths.log_file(&t.id);
        assert!(base.exists() && base.with_file_name("tk_rm.log.1").exists());
        crate::task_store::remove_task_logs(&paths, &t.id);
        assert!(!base.exists(), "当前日志应被删");
        assert!(
            !base.with_file_name("tk_rm.log.1").exists(),
            "轮转历史也必须一起删（否则成永久孤儿）"
        );
        assert!(!base.with_file_name("tk_rm.log.2").exists());
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// P2b-D：日志超上限**轮转**（保留 3 份），不再「超限静默不写」。
    #[test]
    fn log_rotates_and_keeps_bounded_files() {
        let root = tmp_root("logrotate");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let mut t = now_task("b", "tk_log", "c");
        t.limits.log_max_bytes = 40; // 小上限，几轮就触发
        let rt = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            finished_at: Some(1_700_000_000),
            ..Default::default()
        };
        for i in 0..6 {
            write_log(&paths, &t, &rt, &format!("payload-{i}-xxxxxxxxxxxxxxxx"));
        }
        let f = paths.log_file("tk_log");
        assert!(f.exists(), "当前日志必须存在");
        assert!(f.with_file_name("tk_log.log.1").exists(), "应有第 1 份轮转");
        assert!(f.with_file_name("tk_log.log.2").exists(), "应有第 2 份轮转");
        assert!(
            !f.with_file_name("tk_log.log.3").exists(),
            "只保留 {} 份（含当前），不得无限增长",
            LOG_KEEP_FILES
        );
        let newest = std::fs::read_to_string(&f).unwrap();
        assert!(
            newest.contains("payload-5"),
            "最新一轮必须写在当前日志里（否则等于丢最新记录）：{newest}"
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// P2b-D：终态任务超保留期的**日志**回收，但定义与运行态保留（`task status` 仍可看）。
    #[test]
    fn gc_logs_drops_expired_logs_but_keeps_task_and_state() {
        let root = tmp_root("loggc");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let store = TaskStore::new_at(&root, "b");
        let states = TaskStateStore::new_at(&root, "b");
        let t = now_task("b", "tk_old", "c");
        store.add(t.clone()).unwrap();
        let now = crate::chrono_lite::unix_secs();
        states
            .set(
                &t.id,
                TaskRuntime {
                    kind: TaskStateKind::Succeeded,
                    finished_at: Some(now - (LOG_RETENTION_DAYS + 1) * 86_400),
                    ..Default::default()
                },
            )
            .unwrap();
        write_log(&paths, &t, &states.get(&t.id), "old run");
        assert!(paths.log_file(&t.id).exists(), "前置：日志已写入");

        gc_logs(&store, &states, now);

        assert!(!paths.log_file(&t.id).exists(), "超保留期的日志应被回收");
        assert!(
            store.list().iter().any(|x| x.id == t.id),
            "任务定义绝不能被自动删（用户资产）"
        );
        assert_eq!(
            states.get(&t.id).kind,
            TaskStateKind::Succeeded,
            "运行态要保留（task status 仍显示上次结果）"
        );

        // 未超期的终态日志不动
        states
            .set(
                &t.id,
                TaskRuntime {
                    kind: TaskStateKind::Succeeded,
                    finished_at: Some(now - 3600),
                    ..Default::default()
                },
            )
            .unwrap();
        write_log(&paths, &t, &states.get(&t.id), "recent run");
        gc_logs(&store, &states, now);
        assert!(paths.log_file(&t.id).exists(), "保留期内的日志不得被回收");
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 上次进程留下的 Running 是孤儿（没有可复活的 pid 账本）→ 必须归位 Pending 重跑，
    /// 否则任务会永远卡在「运行中」。
    #[test]
    fn orphaned_running_is_requeued() {
        let root = tmp_root("orphan");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);
        store.add(now_task(bot, "tk_run", "c")).unwrap();
        states
            .set(
                "tk_run",
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "Running 不该被当待认领"
        );

        // max_restarts 默认 1：第一次中断允许归位重跑，并记一次 restarts
        requeue_orphans(&store, &states);
        assert_eq!(states.get("tk_run").kind, TaskStateKind::Pending);
        assert_eq!(states.get("tk_run").restarts, 1);
        assert_eq!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).map(|t| t.id),
            Some("tk_run".into())
        );

        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 回归锁（本次修复的核心）：创建者会话存的是 buzz 频道 UUID（历史坏数据）时，
    /// 投递目标回落到该 bot 主会话——绝不把频道 UUID 当平台 receive_id。
    #[test]
    fn delivery_heals_channel_uuid_to_primary_chat() {
        let uuid = crate::buzz::keys::channel_uuid(
            "cli_a8a27ff268b8900e",
            "oc_1f097b843c4d12b3bc8b91205cfe4dd8",
        );
        assert!(crate::buzz::keys::looks_like_channel_uuid(&uuid));
        let (chat, healed) = heal_delivery_chat(&uuid, "oc_1f097b843c4d12b3bc8b91205cfe4dd8");
        assert!(healed);
        assert_eq!(chat, "oc_1f097b843c4d12b3bc8b91205cfe4dd8");
        // 真实平台 id：不动
        let (chat, healed) = heal_delivery_chat("oc_1f097b843c4d12b3bc8b91205cfe4dd8", "oc_other");
        assert!(!healed);
        assert_eq!(chat, "oc_1f097b843c4d12b3bc8b91205cfe4dd8");
        // 频道 UUID 但无主会话可回落：原样（交由投递侧 loud 失败）
        let (chat, healed) = heal_delivery_chat(&uuid, "");
        assert!(!healed);
        assert_eq!(chat, uuid);
    }

    /// 记录型 messenger：只记 send_text 的 (chat, text)，供投递断言。
    /// `fail_chats` 里的会话返回 Err（模拟「创建者会话已失效」——群解散/bot 被移出）。
    #[derive(Default)]
    struct RecordingMsgr {
        sent: std::sync::Mutex<Vec<(String, String)>>,
        fail_chats: Vec<String>,
    }

    #[async_trait::async_trait]
    impl crate::messenger::Messenger for RecordingMsgr {
        async fn send_text(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
            if self.fail_chats.iter().any(|c| c == chat_id) {
                anyhow::bail!("模拟会话失效");
            }
            self.sent
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
            Ok(())
        }
        async fn send_attachment(
            &self,
            _chat_id: &str,
            _meta: &crate::attachments::AttachmentMeta,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// 与 `buzz::oneshot` 测试同款的 mock 装配（`tests/mock_acp_agent.py`）。
    fn mock_cfg(record_file: &std::path::Path) -> crate::buzz::harness::AgentConfig {
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        crate::buzz::harness::AgentConfig {
            command: python3,
            args: vec![script.display().to_string()],
            extra_env: vec![
                ("PATH".to_string(), crate::deps::composed_path()),
                (
                    "MOCK_RECORD_FILE".to_string(),
                    record_file.display().to_string(),
                ),
            ],
            backend: "mock".to_string(),
            session_sandbox: None,
        }
    }

    /// **端到端**：登记 → 跑（oneshot 独立 handle）→ 运行态落盘 → 写日志 → 结果投回创建者。
    ///
    /// 这条用例锁的是 #306 的卖点本身：「异步跑、完成后经 deliver 投递、默认回创建者」。
    /// 把它拆成「只测状态机」会让最容易断的那截（投递信封的 in_session/自环组合）无人看守。
    #[tokio::test]
    // 与仓库既有 17 处同款：mock agent fixture 依赖 python3，Windows runner 没装。
    // 缺了这条会在 windows CI 上挂满回合预算才红（#328 让 main 红了 38 分钟）。
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn task_runs_end_to_end_and_delivers_to_creator() {
        let root = tmp_root("e2e");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);

        let chat = format!("wx_{}", uuid::Uuid::new_v4());
        let mut task = now_task(bot, "tk_e2e", &chat);
        // 用例自带短预算：mock 拉不起来时 30s 内红，不占满生产默认的 30 分钟
        // （#328 的教训：一个没自限的用例能把 CI job 拖到 38 分钟）。
        task.limits.timeout_secs = 30;
        store.add(task.clone()).unwrap();
        states.set("tk_e2e", TaskRuntime::default()).unwrap();

        let msgr = Arc::new(RecordingMsgr::default());
        let mut msgs: std::collections::HashMap<String, Arc<dyn crate::messenger::Messenger>> =
            std::collections::HashMap::new();
        msgs.insert(bot.to_string(), msgr.clone());
        let mut bots = std::collections::HashMap::new();
        bots.insert(
            bot.to_string(),
            crate::config::BotConfig {
                name: bot.to_string(),
                kind: "feishu".into(),
                ..Default::default()
            },
        );
        // enabled=false：本例走的是**同会话**投递（in_session + 自环），不受跨会话开关约束
        // ——这同时验证了「默认回创建者」不需要打开跨会话投递。
        let router = Arc::new(Router::new(false, msgs, bots, None));

        let rec = std::env::temp_dir().join(format!("abb-task-e2e-{}.jsonl", uuid::Uuid::new_v4()));
        let stop = tokio_util::sync::CancellationToken::new();
        run_attempt(
            bot,
            task,
            mock_cfg(&rec),
            std::env::temp_dir().display().to_string(),
            &router,
            &states,
            &stop,
        )
        .await;

        assert_eq!(
            states.get("tk_e2e").kind,
            TaskStateKind::Succeeded,
            "mock agent 正常应答就该是 Succeeded"
        );
        let sent = msgr.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "结果应投给创建者会话：{sent:?}");
        assert_eq!(sent[0].0, chat);
        assert!(
            sent[0].1.contains("后台任务完成"),
            "投递应带任务抬头：{}",
            sent[0].1
        );

        let log = std::fs::read_to_string(paths.log_file("tk_e2e")).unwrap();
        assert!(log.contains("Succeeded"), "日志要落盘：{log}");

        let _ = std::fs::remove_file(&rec);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 审查 B3 的**真正的**回归锁：认领（`claim`）不得清空 `restarts`。
    ///
    /// 上一版把归位侧写对了，但 `run_one` 认领时用 `..Default::default()` 把计数
    /// 清零 → 上界在生产路径上永不触发，而当时的用例**手工构造 restarts**、从不
    /// 经过认领，所以照样绿。这里直接锁认领这一步。
    #[test]
    fn claim_preserves_restarts() {
        let prev = TaskRuntime {
            kind: TaskStateKind::Pending,
            restarts: 3,
            ..Default::default()
        };
        let rt = claim(&prev, 1_700_000_000);
        assert_eq!(rt.kind, TaskStateKind::Running);
        assert_eq!(rt.restarts, 3, "认领必须带着计数，否则重跑上界永远到不了");
        assert_eq!(rt.started_at, Some(1_700_000_000));
    }

    /// #306 `task cancel`：**尚未开跑**的任务被取消后，不得再开跑，也不得投递结果。
    /// CLI 只写请求文件，运行态由 worker 侧的 `consume_cancel_requests` 落账（单写者）。
    #[test]
    fn cancel_before_start_marks_cancelled_and_is_not_claimed() {
        let root = tmp_root("cancel_pending");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);
        store.add(now_task(bot, "tk_cancel_me", "c")).unwrap();
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_some(),
            "取消前应可认领"
        );

        // CLI 侧的动作：只落一个请求文件。
        std::fs::create_dir_all(paths.cancel_requests_dir()).unwrap();
        std::fs::write(paths.cancel_file("tk_cancel_me"), b"{}").unwrap();

        consume_cancel_requests(&store, &states);
        assert_eq!(states.get("tk_cancel_me").kind, TaskStateKind::Cancelled);
        assert!(
            states
                .get("tk_cancel_me")
                .last_error
                .contains("task cancel"),
            "取消原因要留痕：{}",
            states.get("tk_cancel_me").last_error
        );
        assert!(
            !paths.cancel_file("tk_cancel_me").exists(),
            "请求文件必须被消费掉（否则下次重跑会立刻又被自己取消）"
        );
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "已取消的任务不得再被认领"
        );
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// #306：取消请求**不得**把已经跑完的任务改写成取消（手写/竞态残留的请求文件
    /// 不能吃掉一条成功结果）；请求文件本身要被清掉。
    #[test]
    fn cancel_request_never_rewrites_a_finished_task() {
        let root = tmp_root("cancel_terminal");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);
        store.add(now_task(bot, "tk_done", "c")).unwrap();
        states
            .set(
                "tk_done",
                TaskRuntime {
                    kind: TaskStateKind::Succeeded,
                    ..Default::default()
                },
            )
            .unwrap();
        std::fs::create_dir_all(paths.cancel_requests_dir()).unwrap();
        std::fs::write(paths.cancel_file("tk_done"), b"{}").unwrap();

        consume_cancel_requests(&store, &states);
        assert_eq!(
            states.get("tk_done").kind,
            TaskStateKind::Succeeded,
            "终态不得被取消请求改写"
        );
        assert!(!paths.cancel_file("tk_done").exists());
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// #306 `task cancel` **运行中**：请求文件必须真把在跑轮次叫停（走 oneshot 的
    /// external_cancel 拆栈），且**不投递结果**（用户已明确不要了）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn cancel_mid_run_aborts_and_delivers_nothing() {
        let root = tmp_root("cancel_running");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);

        let chat = format!("wx_{}", uuid::Uuid::new_v4());
        let mut task = now_task(bot, "tk_slow", &chat);
        task.limits.timeout_secs = 60; // 足够长：正常跑不会在本例结束时超时
        store.add(task.clone()).unwrap();
        states.set("tk_slow", TaskRuntime::default()).unwrap();

        let msgr = Arc::new(RecordingMsgr::default());
        let mut msgs: std::collections::HashMap<String, Arc<dyn crate::messenger::Messenger>> =
            std::collections::HashMap::new();
        msgs.insert(bot.to_string(), msgr.clone());
        let mut bots = std::collections::HashMap::new();
        bots.insert(
            bot.to_string(),
            crate::config::BotConfig {
                name: bot.to_string(),
                kind: "feishu".into(),
                ..Default::default()
            },
        );
        let router = Arc::new(Router::new(false, msgs, bots, None));

        let rec =
            std::env::temp_dir().join(format!("abb-task-cancel-{}.jsonl", uuid::Uuid::new_v4()));
        // 让 mock agent 记录 prompt 之后延迟应答：造出确定的「已进 agent、尚未收尾」
        // 窗口，取消请求才有东西可打断（否则 mock 快到请求写下去时这轮已经跑完了）。
        let mut cfg = mock_cfg(&rec);
        cfg.extra_env
            .push(("MOCK_PROMPT_DELAY_MS".to_string(), "3000".to_string()));
        let stop = tokio_util::sync::CancellationToken::new();

        // 取消请求由另一个任务在「这轮真的进了 agent」之后投递（= 跑到一半取消）；
        // 主流程前台跑 `run_attempt`，这样 `states` 不必跨任务移动。
        let rec_for_watch = rec.clone();
        let req = paths.cancel_file("tk_slow");
        let watcher = tokio::spawn(async move {
            let mut waited_ms = 0u64;
            loop {
                let seen = std::fs::read_to_string(&rec_for_watch)
                    .map(|s| s.contains("prompt"))
                    .unwrap_or(false);
                if seen {
                    break;
                }
                assert!(waited_ms < 30_000, "任务未进入 agent 提示阶段");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                waited_ms += 50;
            }
            if let Some(dir) = req.parent() {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(&req, b"{}").unwrap();
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            run_attempt(
                bot,
                task,
                cfg,
                std::env::temp_dir().display().to_string(),
                &router,
                &states,
                &stop,
            ),
        )
        .await
        .expect("取消请求必须在预算内拆栈收尾");
        watcher.await.unwrap();

        assert_eq!(
            states.get("tk_slow").kind,
            TaskStateKind::Cancelled,
            "运行中被取消 → Cancelled"
        );
        assert!(
            msgr.sent.lock().unwrap().is_empty(),
            "取消后不得投递任何结果：{:?}",
            msgr.sent.lock().unwrap()
        );
        assert!(
            !paths.cancel_file("tk_slow").exists(),
            "在跑那轮结束时必须消费掉自己的取消请求"
        );
        let _ = std::fs::remove_file(&rec);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// #306 验收「创建者会话失效时投递失败要回源告警，不静默丢」：默认目标就是创建者
    /// 会话，Router 回源也发回那个失效会话（等于没告警）⇒ 必须有**第二通道**——
    /// 该 bot 的主会话，并把失败写进运行态（`task status` 可见）。
    #[tokio::test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "mock agent fixture 依赖 python3（Windows runner 未装）"
    )]
    async fn delivery_failure_alerts_primary_chat_and_records_error() {
        let root = tmp_root("alert_fallback");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);

        let dead_chat = format!("oc_dead_{}", uuid::Uuid::new_v4());
        let primary = format!("ou_owner_{}", uuid::Uuid::new_v4());
        let mut task = now_task(bot, "tk_alert", &dead_chat);
        task.limits.timeout_secs = 30;
        store.add(task.clone()).unwrap();
        states.set("tk_alert", TaskRuntime::default()).unwrap();

        let msgr = Arc::new(RecordingMsgr {
            sent: Default::default(),
            fail_chats: vec![dead_chat.clone()],
        });
        let mut msgs: std::collections::HashMap<String, Arc<dyn crate::messenger::Messenger>> =
            std::collections::HashMap::new();
        msgs.insert(bot.to_string(), msgr.clone());
        let mut bots = std::collections::HashMap::new();
        bots.insert(
            bot.to_string(),
            crate::config::BotConfig {
                name: bot.to_string(),
                kind: "feishu".into(),
                primary_chat_id: primary.clone(),
                ..Default::default()
            },
        );
        let router = Arc::new(Router::new(false, msgs, bots, None));

        let rec =
            std::env::temp_dir().join(format!("abb-task-alert-{}.jsonl", uuid::Uuid::new_v4()));
        let stop = tokio_util::sync::CancellationToken::new();
        run_attempt(
            bot,
            task,
            mock_cfg(&rec),
            std::env::temp_dir().display().to_string(),
            &router,
            &states,
            &stop,
        )
        .await;

        let sent = msgr.sent.lock().unwrap().clone();
        assert_eq!(
            sent.len(),
            1,
            "失效创建者会话发不出去 → 必须有一条告警落到主会话：{sent:?}"
        );
        assert_eq!(sent[0].0, primary, "告警要落主会话：{sent:?}");
        assert!(
            sent[0].1.contains("投递失败"),
            "告警要说明是投递失败：{}",
            sent[0].1
        );
        assert!(
            sent[0].1.contains("tk_alert"),
            "告警要带任务 id，便于从通知反查：{}",
            sent[0].1
        );
        assert!(
            sent[0].1.contains("`task status tk_alert`"),
            "告警要带可直接执行的排查入口：{}",
            sent[0].1
        );
        let rt = states.get("tk_alert");
        assert!(
            rt.last_error.contains("结果投递失败"),
            "运行态要留痕（task status 可见）：{}",
            rt.last_error
        );
        assert_eq!(
            rt.kind,
            TaskStateKind::Succeeded,
            "投递失败不该把任务本身改判成失败（agent 那一轮是成功的）"
        );
        let _ = std::fs::remove_file(&rec);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 端到端循环：`归位 → 认领 → 中断 → 归位`，`max_restarts = 1` 时第二步必须落
    /// Failed。走的是真实状态机（不再手工跳过认领），所以能挡住「计数被清零」这类回归。
    #[test]
    fn rerun_bound_holds_across_claim_cycle() {
        let root = tmp_root("cycle");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);
        let mut t = now_task(bot, "tk_cycle", "c");
        t.limits.max_restarts = 1;
        store.add(t).unwrap();

        // 第 1 次中断（进程重启时残留 Running）
        states
            .set("tk_cycle", claim(&TaskRuntime::default(), 1))
            .unwrap();
        requeue_orphans(&store, &states);
        assert_eq!(states.get("tk_cycle").kind, TaskStateKind::Pending);
        assert_eq!(states.get("tk_cycle").restarts, 1);

        // worker 认领（真实路径）
        let prev = states.get("tk_cycle");
        states.set("tk_cycle", claim(&prev, 2)).unwrap();
        assert_eq!(states.get("tk_cycle").restarts, 1, "认领不得清零");

        // 第 2 次中断 → 已达上限，落 Failed 且不再被认领
        requeue_orphans(&store, &states);
        let rt = states.get("tk_cycle");
        assert_eq!(rt.kind, TaskStateKind::Failed);
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "Failed 不该被认领"
        );

        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 审查 B3：重跑的是**整条 prompt**（副作用整体重放），必须有上界——超限转 Failed，
    /// 否则「prompt 能把 ABB 跑挂」会变成崩溃—重启—再崩的循环。
    #[test]
    fn orphan_rerun_is_bounded_by_max_restarts() {
        let root = tmp_root("bounded");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);
        let mut t = now_task(bot, "tk_bound", "c");
        t.limits.max_restarts = 2;
        store.add(t).unwrap();

        // 第 1、2 次中断 → 归位重跑
        for want in 1..=2u32 {
            states
                .set(
                    "tk_bound",
                    TaskRuntime {
                        kind: TaskStateKind::Running,
                        restarts: want - 1,
                        ..Default::default()
                    },
                )
                .unwrap();
            requeue_orphans(&store, &states);
            let rt = states.get("tk_bound");
            assert_eq!(rt.kind, TaskStateKind::Pending, "第 {want} 次应归位");
            assert_eq!(rt.restarts, want);
        }

        // 第 3 次：已达上限 → Failed，且**不再**被认领
        states
            .set(
                "tk_bound",
                TaskRuntime {
                    kind: TaskStateKind::Running,
                    restarts: 2,
                    ..Default::default()
                },
            )
            .unwrap();
        requeue_orphans(&store, &states);
        let rt = states.get("tk_bound");
        assert_eq!(rt.kind, TaskStateKind::Failed);
        assert!(rt.last_error.contains("中断"), "{}", rt.last_error);
        assert!(
            next_due_task(&store, &states, crate::chrono_lite::unix_secs()).is_none(),
            "Failed 不该被认领"
        );

        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 审查 N5：`task rm` 与 worker 认领的竞态会留下「有运行态、无定义」的孤儿条目，
    /// 启动清理要顺手删掉（否则状态文件与日志目录无限堆积）。
    #[test]
    fn requeue_orphans_drops_state_without_definition() {
        let root = tmp_root("ghost");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);
        states
            .set(
                "tk_ghost",
                TaskRuntime {
                    kind: TaskStateKind::Succeeded,
                    ..Default::default()
                },
            )
            .unwrap();

        requeue_orphans(&store, &states);
        assert!(
            states.ids().is_empty(),
            "无定义的残留运行态应被清掉：{:?}",
            states.ids()
        );

        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn workspace_falls_back_to_bot_workspace() {
        let mut t = now_task("somebot", "tk_x", "c");
        assert_eq!(
            task_workspace("somebot", &t),
            crate::workspace_dir("somebot").display().to_string()
        );
        t.payload.cwd = "/tmp/explicit".to_string();
        assert_eq!(task_workspace("somebot", &t), "/tmp/explicit");
    }

    /// 日志必须落盘（跑久/跑丢看不见是任务的头号痛点），且超上限后不再增长。
    #[test]
    fn log_is_written_and_capped() {
        let root = tmp_root("log");
        let bot = "b";
        let paths = crate::task_store::TaskPaths::with_root(&root, bot);
        let t = now_task(bot, "tk_log", "c");
        let rt = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            finished_at: Some(1),
            ..Default::default()
        };
        write_log(&paths, &t, &rt, "hello");
        let body = std::fs::read_to_string(paths.log_file("tk_log")).unwrap();
        assert!(body.contains("hello"), "{body}");
        assert!(body.contains("Succeeded"), "{body}");

        // 上限设成 1 字节 → 已有文件超限，**轮转**而不是停写（P2b-D 行为变更）：
        // 旧实现「超上限就静默不再写」会让用户以为日志断了；现在把历史挪到 .1、当前文件重新开始。
        let mut tiny = t.clone();
        tiny.limits.log_max_bytes = 1;
        write_log(&paths, &tiny, &rt, "second-run");
        // 注意：路径要用 with_file_name 拼（`log_file("tk_log.log.1")` 会再补一个 `.log`）
        let rotated_path = paths.log_file("tk_log").with_file_name("tk_log.log.1");
        let rotated =
            std::fs::read_to_string(&rotated_path).expect("超上限时应轮转出 .1（历史保留）");
        assert!(
            rotated.contains("hello"),
            "历史内容应在轮转文件里：{rotated}"
        );
        let after = std::fs::read_to_string(paths.log_file("tk_log")).unwrap();
        assert!(
            after.contains("second-run"),
            "轮转后当前日志必须继续写（不得静默停写）：{after}"
        );
        assert!(!after.contains("hello"), "当前日志应是新的一份：{after}");

        let _ = std::fs::remove_dir_all(&paths.dir);
    }
}
