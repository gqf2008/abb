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
//!   杀进程组），不是「置个标志然后假装停了」——D1c 要求的就是这条。
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
    let _ = states.set(
        &id,
        TaskRuntime {
            kind: TaskStateKind::Running,
            started_at: Some(started),
            ..Default::default()
        },
    );
    crate::log!(
        "[task:{bot_key}] {short} 开跑（workspace={}）",
        task_workspace(bot_key, &task)
    );

    let agent_cfg = crate::service::oneshot_agent_config(bot, cfg);
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
    let outcome = crate::buzz::oneshot::oneshot_turn(
        agent_cfg,
        Some(workspace),
        msg,
        budget,
        Some(stop.clone()),
    )
    .await;

    let finished = crate::chrono_lite::unix_secs();
    let (rt, text) = match outcome {
        crate::buzz::harness::SyncTurnOutcome::Ok(text) => (
            TaskRuntime {
                kind: TaskStateKind::Succeeded,
                started_at: Some(started),
                finished_at: Some(finished),
                last_exit_code: Some(0),
                ..Default::default()
            },
            text,
        ),
        crate::buzz::harness::SyncTurnOutcome::Cancelled => (
            TaskRuntime {
                kind: TaskStateKind::Cancelled,
                started_at: Some(started),
                finished_at: Some(finished),
                last_error: "已取消".to_string(),
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
    write_log(bot_key, &task, &rt, &text);
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

    // 取消/超时/失败也有必要回投——用户看不到结果才是真痛点；但**关停联动**触发的
    // 取消不回投（进程正在退出，投递通道也在关，回了多半发不出去还刷日志）。
    if cancelled && stop.is_cancelled() {
        return;
    }
    let Some(chat) = delivery_chat(&task) else {
        crate::log!("[task:{bot_key}] {short} 无投递目标（创建者会话为空），结果未发送");
        return;
    };
    let header = if failed {
        "⚠️ 后台任务失败".to_string()
    } else if cancelled {
        "🛑 后台任务已取消".to_string()
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
    let item = DeliveryItem {
        id: uuid::Uuid::new_v4().to_string(),
        target_bot: bot_key.to_string(),
        target_chat: chat.clone(),
        text: format!("{header}\n\n{body}"),
        source_bot: bot_key.to_string(),
        source_chat: chat,
        created_at: finished,
        attachments: Vec::new(),
        // 非空 = 跳过防循环去重（同一任务重跑两次是合法重复）；同时也是 P1b 之前
        // 「定时/任务类投递」的既有标记口径。
        job_id: task.id.clone(),
        in_session: true,
    };
    router.deliver(&item).await;
}

/// 任务的工作目录：定义里显式给了就用它，否则回落该 bot 的工作区。
fn task_workspace(bot_key: &str, task: &Task) -> String {
    if task.payload.cwd.trim().is_empty() {
        crate::workspace_dir(bot_key).display().to_string()
    } else {
        task.payload.cwd.clone()
    }
}

/// 默认投递目标 = 创建者会话；显式 targets 优先。
fn delivery_chat(task: &Task) -> Option<String> {
    if let Some(t) = task.delivery.targets.first() {
        return Some(t.chat_id.clone());
    }
    if task.created_by.chat_id.trim().is_empty() {
        None
    } else {
        Some(task.created_by.chat_id.clone())
    }
}

/// 把一条任务的结局追加到 `task-logs/<id>.log`（超 `log_max_bytes` 时不写，避免吃满盘；
/// 轮转是 P4）。
fn write_log(bot_key: &str, task: &Task, rt: &TaskRuntime, text: &str) {
    let paths = crate::task_store::TaskPaths::for_bot(bot_key);
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

/// 上次进程残留的 `Running` 是孤儿（执行器随进程一起没了）→ 标回 `Pending` 重跑。
fn requeue_orphans(store: &TaskStore, states: &TaskStateStore) {
    for t in store.list() {
        let rt = states.get(&t.id);
        if rt.kind == TaskStateKind::Running {
            crate::log!(
                "[task] 上次运行残留 Running（{}）→ 归位 Pending 重跑",
                &t.id[..t.id.len().min(12)]
            );
            let _ = states.set(
                &t.id,
                TaskRuntime {
                    kind: TaskStateKind::Pending,
                    ..rt
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_store::{CreatedBy, TaskDelivery, TaskLimits, TaskPayload, TaskTrigger};

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

    #[test]
    fn default_delivery_is_creator_session() {
        let t = now_task("b", "tk_x", "wx_chat");
        assert_eq!(delivery_chat(&t).as_deref(), Some("wx_chat"));
    }

    #[test]
    fn explicit_targets_win_over_creator() {
        let mut t = now_task("b", "tk_x", "wx_chat");
        t.delivery.targets = vec![crate::task_store::TaskTarget {
            bot_key: String::new(),
            chat_id: "other".to_string(),
        }];
        assert_eq!(delivery_chat(&t).as_deref(), Some("other"));
    }

    /// 没有创建者会话（人工 CLI 建的）时必须**返回 None**，让调用方明确报「没目标」，
    /// 而不是投到空串（那会变成一条静默失败）。
    #[test]
    fn no_creator_and_no_target_means_no_delivery() {
        let t = now_task("b", "tk_x", "");
        assert_eq!(delivery_chat(&t), None);
    }

    /// 只认领 `now` + `Pending`。别的触发档不认领（编排器是 P3/P5），
    /// 已结束的也不重跑（否则每次轮询都会重跑成功过的任务）。
    #[test]
    fn only_now_and_pending_is_claimed() {
        let bot = format!("tclaim-{}", std::process::id());
        let paths = crate::task_store::TaskPaths::for_bot(&bot);
        let _ = std::fs::remove_dir_all(&paths.dir);
        let store = TaskStore::new(&bot);
        let states = TaskStateStore::new(&bot);

        let a = now_task(&bot, "tk_now", "c");
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
        let mut b = now_task(&bot, "tk_cron", "c");
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
        let bot = format!("torphan-{}", std::process::id());
        let paths = crate::task_store::TaskPaths::for_bot(&bot);
        let _ = std::fs::remove_dir_all(&paths.dir);
        let store = TaskStore::new(&bot);
        let states = TaskStateStore::new(&bot);
        store.add(now_task(&bot, "tk_run", "c")).unwrap();
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

        requeue_orphans(&store, &states);
        assert_eq!(states.get("tk_run").kind, TaskStateKind::Pending);
        assert_eq!(
            next_pending(&store, &states).map(|t| t.id),
            Some("tk_run".into())
        );

        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// 记录型 messenger：只记 send_text 的 (chat, text)，供投递断言。
    #[derive(Default)]
    struct RecordingMsgr {
        sent: std::sync::Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl crate::messenger::Messenger for RecordingMsgr {
        async fn send_text(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
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
    async fn task_runs_end_to_end_and_delivers_to_creator() {
        let bot = format!("te2e-{}", std::process::id());
        let paths = crate::task_store::TaskPaths::for_bot(&bot);
        let _ = std::fs::remove_dir_all(&paths.dir);
        let store = TaskStore::new(&bot);
        let states = TaskStateStore::new(&bot);

        let chat = format!("wx_{}", uuid::Uuid::new_v4());
        let task = now_task(&bot, "tk_e2e", &chat);
        store.add(task.clone()).unwrap();
        states.set("tk_e2e", TaskRuntime::default()).unwrap();

        let msgr = Arc::new(RecordingMsgr::default());
        let mut msgs: std::collections::HashMap<String, Arc<dyn crate::messenger::Messenger>> =
            std::collections::HashMap::new();
        msgs.insert(bot.clone(), msgr.clone());
        let mut bots = std::collections::HashMap::new();
        bots.insert(
            bot.clone(),
            crate::config::BotConfig {
                name: bot.clone(),
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
            &bot,
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
        let bot = format!("tlog-{}", std::process::id());
        let paths = crate::task_store::TaskPaths::for_bot(&bot);
        let _ = std::fs::remove_dir_all(&paths.dir);
        let t = now_task(&bot, "tk_log", "c");
        let rt = TaskRuntime {
            kind: TaskStateKind::Succeeded,
            finished_at: Some(1),
            ..Default::default()
        };
        write_log(&bot, &t, &rt, "hello");
        let body = std::fs::read_to_string(paths.log_file("tk_log")).unwrap();
        assert!(body.contains("hello"), "{body}");
        assert!(body.contains("Succeeded"), "{body}");

        // 上限设成 1 字节 → 已有文件超限，不再追加
        let mut tiny = t.clone();
        tiny.limits.log_max_bytes = 1;
        let before = std::fs::read_to_string(paths.log_file("tk_log")).unwrap();
        write_log(&bot, &tiny, &rt, "should-not-appear");
        let after = std::fs::read_to_string(paths.log_file("tk_log")).unwrap();
        assert_eq!(before, after, "超上限不该继续写");

        let _ = std::fs::remove_dir_all(&paths.dir);
    }
}
