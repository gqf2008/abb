//! 任务执行（#326 / `docs/task-model.md` P2b 的 **agent 载荷**落地，即 #306）。
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
//! ## 本轮范围
//!
//! 只做 `agent` 载荷。`proc` 载荷要的是**进程超管**（pid 账本 / 进程树终止 / Windows
//! Job Object / 退出码回收），与 ACP 执行层无共用件，属 P3；这里对 `proc` 显式拒绝，
//! 不静默降级。
//!
//! ## 投递语义
//!
//! 结果默认回**创建者会话**（D2）。信封按「发给当前会话」构造：`source == target`
//! 且 `in_session = true`——正是 CLI `--to-current` 的既有语义，Router 侧
//! `in_session_ok = in_session && is_self_loop(item)` 的判据**一个字没改**
//! （绝不用「地址相等即豁免」去削弱防循环，见 D2 的警告）。

use std::sync::Arc;
use std::time::Duration;

use crate::deliver::{DeliveryItem, Router};
use crate::task_store::{
    PayloadKind, Task, TaskRuntime, TaskStateKind, TaskStateStore, TaskStore, TriggerKind,
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

/// 尚未开跑就被取消的落盘原因。
const CANCEL_REASON_BEFORE_START: &str = "已取消（task cancel，尚未开跑）";

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
    // 启动清理：上次进程留下的 Running 是孤儿（进程已死），标回 Pending 让它重跑。
    // agent 任务在 oneshot 里跑，没有可复活的 pid 账本，重跑是唯一安全的归位
    // （副作用幂等性由任务 prompt 自己负责——文档 §风险表已记「结果重复投递」）。
    requeue_orphans(&store, &states);

    loop {
        if stop.is_cancelled() {
            return;
        }
        // 取消请求先于认领处理：排队等着的任务被 cancel 掉之后**不该再开跑**
        // （否则用户看到的是「已取消却仍然跑了一轮并投递结果」）。
        consume_cancel_requests(&store, &states);
        // 每轮只认领一条：跑完再认领下一条 = 串行排队（Q7）。
        if let Some(task) = next_pending(&store, &states) {
            run_one(&bot, &cfg, &router, &states, task, &bot_key, &stop).await;
            continue; // 立刻看下一条，不等轮询间隔
        }
        tokio::select! {
            _ = tokio::time::sleep(poll) => {}
            _ = stop.cancelled() => return,
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

    if task.payload.kind != PayloadKind::Agent {
        // proc 载荷要到 P3 才落地：**显式失败**，不静默跳过（否则「登记了却永远
        // pending」会被当成调度坏了）。
        let _ = states.set(
            &id,
            TaskRuntime {
                kind: TaskStateKind::Failed,
                finished_at: Some(crate::chrono_lite::unix_secs()),
                last_error: "proc 载荷尚未支持（P3 进程超管落地后可用）".to_string(),
                ..Default::default()
            },
        );
        crate::log!("[task:{bot_key}] {short} 跳过：proc 载荷未支持");
        return;
    }

    let started = crate::chrono_lite::unix_secs();
    let prev = states.get(&id);
    let _ = states.set(&id, claim(&prev, started));
    crate::log!(
        "[task:{bot_key}] {short} 开跑（workspace={}）",
        task_workspace(bot_key, &task)
    );

    let agent_cfg = agent_cfg_for_task(bot, cfg, &task, bot_key);
    let workspace = task_workspace(bot_key, &task);
    run_attempt(bot_key, task, agent_cfg, workspace, router, states, stop).await;
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
    let short = id[..id.len().min(12)].to_string();
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
    let cancel_watch = {
        let token = attempt_cancel.clone();
        let req = states.paths().cancel_file(&id);
        tokio::spawn(async move {
            loop {
                if token.is_cancelled() {
                    return;
                }
                if req.exists() {
                    crate::log!("[task] 收到取消请求（{}），终止在跑轮次", req.display());
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
    // 终态也要带住 restarts（否则 `task status` 看不到重跑过几次）。
    let prev = states.get(&id);
    let restarts = prev.restarts;
    let (rt, text) = match outcome {
        crate::buzz::harness::SyncTurnOutcome::Ok(text) => (
            TaskRuntime {
                kind: TaskStateKind::Succeeded,
                started_at: Some(started),
                finished_at: Some(finished),
                last_exit_code: Some(0),
                restarts,
                ..Default::default()
            },
            text,
        ),
        crate::buzz::harness::SyncTurnOutcome::Cancelled => (
            TaskRuntime {
                kind: TaskStateKind::Cancelled,
                started_at: Some(started),
                finished_at: Some(finished),
                last_error: CANCEL_REASON.to_string(),
                restarts,
                ..Default::default()
            },
            String::new(),
        ),
        crate::buzz::harness::SyncTurnOutcome::Timeout => (
            TaskRuntime {
                kind: TaskStateKind::Failed,
                started_at: Some(started),
                finished_at: Some(finished),
                last_error: format!("执行超时（预算 {}s）", budget.as_secs()),
                restarts,
                ..Default::default()
            },
            String::new(),
        ),
        crate::buzz::harness::SyncTurnOutcome::Closed => (
            TaskRuntime {
                kind: TaskStateKind::Failed,
                started_at: Some(started),
                finished_at: Some(finished),
                last_error: "agent 不可用（执行器未能拉起）".to_string(),
                restarts,
                ..Default::default()
            },
            String::new(),
        ),
        crate::buzz::harness::SyncTurnOutcome::Failed(reason) => (
            TaskRuntime {
                kind: TaskStateKind::Failed,
                started_at: Some(started),
                finished_at: Some(finished),
                last_error: reason.clone(),
                restarts,
                ..Default::default()
            },
            String::new(),
        ),
    };
    let failed = rt.kind == TaskStateKind::Failed;
    let cancelled = rt.kind == TaskStateKind::Cancelled;
    let reason = rt.last_error.clone();
    let _ = states.set(&id, rt.clone());

    // 日志：任务本身就是「跑久/跑丢也看不见」的痛点，落盘 + 出口都要有（D6 的基础项）。
    write_log(states.paths(), &task, &rt, &text);
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
    let Some((target_bot, chat)) = delivery_target(&task, bot_key) else {
        crate::log!("[task:{bot_key}] {short} 无投递目标（创建者会话为空），结果未发送");
        return;
    };
    // 注意：这里没有「已取消」抬头——见上 `if cancelled { return; }` 的说明，
    // 被取消的轮次（关停联动 / 用户 task cancel）压根走不到投递。
    let header = if failed {
        "⚠️ 后台任务失败".to_string()
    } else {
        format!("🤖 后台任务完成：{}", task.display_name())
    };
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
    let in_session = target_bot == bot_key && chat == created_chat;
    let item = DeliveryItem {
        id: uuid::Uuid::new_v4().to_string(),
        target_bot: target_bot.clone(),
        target_chat: chat.clone(),
        text: format!("{header}\n\n{body}"),
        source_bot: bot_key.to_string(),
        source_chat: created_chat,
        created_at: finished,
        attachments: Vec::new(),
        // 非空 = 跳过防循环去重（同一任务重跑两次是合法重复）；同时也是 P1b 之前
        // 「定时/任务类投递」的既有标记口径。
        job_id: task.id.clone(),
        in_session,
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
    alert_primary_chat(router, bot_key, &target_bot, &chat, &note).await;
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
    let text = format!("⚠️ 后台任务结果投递失败（{note}）\n\n目标：{target_bot}:{target_chat}");
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
        let known = store.list().iter().any(|t| t.id == id);
        if !known {
            // 定义已经没了（task rm / 孤儿清理）：请求没有意义，清掉别堆积。
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if rt.kind == TaskStateKind::Running {
            continue;
        }
        if rt.kind == TaskStateKind::Pending {
            crate::log!("[task] {id} 尚未开跑即被取消");
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

/// 把一条任务的结局追加到 `task-logs/<id>.log`（超 `log_max_bytes` 时不写，避免吃满盘；
/// 轮转是 P4）。
fn write_log(paths: &crate::task_store::TaskPaths, task: &Task, rt: &TaskRuntime, text: &str) {
    if paths.ensure().is_err() {
        return;
    }
    let file = paths.log_file(&task.id);
    // 日志目录要单独建（`TaskPaths::ensure` 只保证任务根目录）
    if let Some(dir) = file.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    if let Ok(meta) = std::fs::metadata(&file) {
        if meta.len() >= task.limits.log_max_bytes {
            return;
        }
    }
    let stamp = rt.finished_at.unwrap_or_else(crate::chrono_lite::unix_secs);
    let ok = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "[{}] {:?} {}", stamp, rt.kind, rt.last_error)?;
            if !text.trim().is_empty() {
                writeln!(f, "{text}\n")?;
            }
            Ok(())
        });
    let _ = ok;
}

/// 认领下一条：只认 `trigger=now` 且运行态为 `Pending` 的任务。
/// 其余触发档（once/cron/interval/keepalive）要的是编排器，属 P3/P5——这里**不认领**，
/// 免得把它们跑成「登记即执行」。
fn next_pending(store: &TaskStore, states: &TaskStateStore) -> Option<Task> {
    store.list().into_iter().find(|t| {
        t.trigger.kind == TriggerKind::Now && states.get(&t.id).kind == TaskStateKind::Pending
    })
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
    // 无定义的残留运行态：删掉，别让 task-logs/状态文件无限堆积。
    let known: std::collections::HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    for id in states.ids() {
        if !known.contains(id.as_str()) {
            let _ = states.remove(&id);
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
            delivery: TaskDelivery::default(),
            limits: TaskLimits::default(),
        }
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

    /// 只认领 `now` + `Pending`。别的触发档不认领（编排器是 P3/P5），
    /// 已结束的也不重跑（否则每次轮询都会重跑成功过的任务）。
    #[test]
    fn only_now_and_pending_is_claimed() {
        let root = tmp_root("claim");
        let paths = crate::task_store::TaskPaths::with_root(&root, "b");
        let bot = "b";
        let store = TaskStore::new_at(&root, bot);
        let states = TaskStateStore::new_at(&root, bot);

        let a = now_task(bot, "tk_now", "c");
        store.add(a.clone()).unwrap();
        assert_eq!(
            next_pending(&store, &states).map(|t| t.id),
            Some("tk_now".into())
        );

        // 跑完（Succeeded）后不再被认领
        states
            .set(
                "tk_now",
                TaskRuntime {
                    kind: TaskStateKind::Succeeded,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(next_pending(&store, &states).is_none());

        // cron 档不认领
        let mut b = now_task(bot, "tk_cron", "c");
        b.trigger = TaskTrigger {
            kind: TriggerKind::Cron,
            expr: "0 9 * * *".to_string(),
            timezone: String::new(),
        };
        store.add(b).unwrap();
        assert!(next_pending(&store, &states).is_none());

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
            next_pending(&store, &states).is_none(),
            "Running 不该被当待认领"
        );

        // max_restarts 默认 1：第一次中断允许归位重跑，并记一次 restarts
        requeue_orphans(&store, &states);
        assert_eq!(states.get("tk_run").kind, TaskStateKind::Pending);
        assert_eq!(states.get("tk_run").restarts, 1);
        assert_eq!(
            next_pending(&store, &states).map(|t| t.id),
            Some("tk_run".into())
        );

        let _ = std::fs::remove_dir_all(&paths.dir);
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
        assert!(next_pending(&store, &states).is_some(), "取消前应可认领");

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
            next_pending(&store, &states).is_none(),
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
        assert!(next_pending(&store, &states).is_none(), "Failed 不该被认领");

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
        assert!(next_pending(&store, &states).is_none(), "Failed 不该被认领");

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

        // 上限设成 1 字节 → 已有文件超限，不再追加
        let mut tiny = t.clone();
        tiny.limits.log_max_bytes = 1;
        let before = std::fs::read_to_string(paths.log_file("tk_log")).unwrap();
        write_log(&paths, &tiny, &rt, "should-not-appear");
        let after = std::fs::read_to_string(paths.log_file("tk_log")).unwrap();
        assert_eq!(before, after, "超上限不该继续写");

        let _ = std::fs::remove_dir_all(&paths.dir);
    }
}
