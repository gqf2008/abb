//! 一次性同步回合（单后端化 P3.1）：自起 handle + run_loop，跑一轮同步回合，
//! 拆栈——给「借不到/不该碰常驻聊天句柄」的调用方一条自包含路径：
//!
//! - **GUI 进程**（generate_role_prompt / teambuilder 的「✨生成」按钮）：进程里
//!   没有任何 ACP 句柄（句柄只活在 service 进程）；
//! - **service 旁路**（session_gc 每日归纳）：常驻句柄是面向频道队列的（单 slot +
//!   steer/cancel 语义），一次性任务往里塞会占 slot、触发 steer 合并、和聊天抢回合。
//!
//! 用法（P3.2/3.3/3.4 各自接入）：调用方负责解析命令与供应商 env（参照
//! `service::build_bot_acp_handles` 的装配段），把 prompt 文本/角色填进
//! [`InboundMsg`]，给定回合预算；本函数管句柄生命周期与结局分类。
//!
//! 本文件是移植区新件（上游无对应），全部机制复用 harness 既有件；唯一机制增量
//! 是 harness 侧的死信→同步等待者 Err 旁路（见 harness.rs `notify_channel`）。

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::harness::{AgentConfig, BuzzHandle, ChannelMeta, SyncTurnOutcome};
use super::queue::InboundMsg;

/// 拆栈预算：正常路径 = 回合已毕、杀空闲 agent 进程组（≤5s）；Timeout 路径 =
/// cancel 排水宽限（`CONTROL_CANCEL_GRACE` 5s）+ 杀进程组（≤5s）+ 余量。
/// 超时仍拆不净则告警并 detach（残留任务由关停宽限/kill_on_drop 尽力收尸，
/// 有界不泄漏——绝不为了等它把调用方挂死）。
// 阶段性落地：P3.1 只交 helper 本体，首个生产调用方随 P3.2（session_gc）接入。
#[allow(dead_code)]
const TEARDOWN_BUDGET: Duration = Duration::from_secs(12);

/// 超时后向主循环发 cancel 的等待上限（主循环活着时即时；防御 wedged）。
#[allow(dead_code)]
const CANCEL_CMD_BUDGET: Duration = Duration::from_secs(5);

/// 自起句柄跑一轮同步回合并拆栈。
///
/// - `cfg`：agent 子进程命令/env/档位载荷（调用方按 bot 装配，参照 service 侧）；
/// - `workspace`：session/new 的 cwd 与 `<workspace>` 段（`None` = ABB 进程当前目录）；
/// - `msg`：prompt 本体（`author_role`/`prompt_tag` 由调用方定，如归纳 = Owner +
///   `"gc_summary"`）；
/// - `budget`：回合预算（调用方 tokio 超时；触发即 `SyncTurnOutcome::Timeout`，
///   在途回合由本函数 cancel 防迟发）。
///
/// 返回 [`SyncTurnOutcome`]：Ok(回合文本) / Timeout / Closed / Failed(agent 终态
/// 失败原因)。句柄用自建的 [`CancellationToken`]（不碰全局关停令牌），用完即弃；
/// 拆栈保证 agent 子进程组被杀（正常路径 join 内完成，最坏路径 detach 后由
/// 关停宽限兜底，进程组收尸有界）。
// 阶段性落地：P3.1 只交 helper 本体，首个生产调用方随 P3.2（session_gc）接入，
// 届时移除此 allow。
#[allow(dead_code)]
pub async fn oneshot_turn(
    cfg: AgentConfig,
    workspace: Option<String>,
    msg: InboundMsg,
    budget: Duration,
) -> SyncTurnOutcome {
    let stop = CancellationToken::new();
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();
    let handle = BuzzHandle::new(cfg, stop.clone(), cwd);
    let mut run_task = tokio::spawn(super::harness::run_loop(handle.clone()));

    let channel_id = Uuid::new_v4();
    handle.upsert_channel(
        channel_id,
        ChannelMeta {
            bot_key: "oneshot".to_string(),
            chat_id: channel_id.to_string(),
            chat_type: "p2p".to_string(),
            thread_id: None,
            name: "oneshot".to_string(),
            anchor_mid: None,
            adhoc: true,
            workspace,
        },
    );

    let outcome = handle.wait_turn_outcome(channel_id, msg, budget).await;
    if matches!(outcome, SyncTurnOutcome::Timeout) {
        // 防迟发：叫停在途回合。等 cancel 回执 = 主循环已处置该指令（随后
        // token.cancel()  break 主循环不会抢在处置前）。排水+杀进程组在
        // run_loop 关停宽限内完成（见 shutdown）。
        let _ = tokio::time::timeout(CANCEL_CMD_BUDGET, handle.cancel(channel_id)).await;
    }

    stop.cancel();
    if tokio::time::timeout(TEARDOWN_BUDGET, &mut run_task)
        .await
        .is_err()
    {
        tracing::warn!("oneshot teardown exceeded budget — detached (shutdown grace reaps)");
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buzz::harness::MAX_TURN_DURATION;

    /// 与 bridge/mod.rs make_test_harness 同款的 mock 装配，但完全自含
    ///（oneshot 的卖点正是不借任何常驻件）。extra_env 追加 mock 触发臂。
    fn mock_cfg(record_file: &std::path::Path, extra_env: Vec<(String, String)>) -> AgentConfig {
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp_agent.py");
        let python3 = crate::deps::find_in_path("python3")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        AgentConfig {
            command: python3,
            args: vec![script.display().to_string()],
            extra_env: vec![
                ("PATH".to_string(), crate::deps::composed_path()),
                (
                    "MOCK_RECORD_FILE".to_string(),
                    record_file.display().to_string(),
                ),
            ]
            .into_iter()
            .chain(extra_env)
            .collect(),
            backend: "mock".to_string(),
            session_sandbox: None,
        }
    }

    fn msg(text: &str) -> InboundMsg {
        InboundMsg {
            id_hex: Uuid::new_v4().to_string(),
            author_role: "owner".to_string(),
            text: text.to_string(),
            ts_secs: 0,
            prompt_tag: "oneshot_test".to_string(),
        }
    }

    fn record_file(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("abb-oneshot-test-{tag}-{}.jsonl", Uuid::new_v4()))
    }

    fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// echo 回合：Ok(含 echo 文本) + session cwd 落 workspace + prompt 原文到达。
    #[tokio::test]
    async fn oneshot_echo_roundtrip() {
        let rec = record_file("echo");
        let ws = std::env::temp_dir().join(format!("abb-oneshot-ws-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let started = std::time::Instant::now();
        let outcome = oneshot_turn(
            mock_cfg(&rec, Vec::new()),
            Some(ws.display().to_string()),
            msg("oneshot-echo-probe"),
            Duration::from_secs(60),
        )
        .await;
        let SyncTurnOutcome::Ok(text) = outcome else {
            panic!("expected Ok, got {outcome:?}");
        };
        assert!(text.contains("echo:"), "echo 文本: {text}");
        assert!(text.contains("oneshot-echo-probe"), "prompt 原文: {text}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "回合+拆栈应远快于预算: {:?}",
            started.elapsed()
        );
        let events = read_records(&rec);
        let session_new = events
            .iter()
            .find(|e| e["event"] == "session_new")
            .expect("session/new 记录");
        assert_eq!(
            session_new["cwd"].as_str(),
            Some(ws.display().to_string().as_str()),
            "session cwd 应为 workspace（原样透传，canonicalize 是调用方责任）: {session_new}"
        );
        assert!(
            events.iter().any(|e| e["event"] == "prompt"
                && e["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("oneshot-echo-probe")),
            "prompt 记录: {events:?}"
        );
    }

    /// agent 终态失败（401 认证，不可重试）：Failed(原因) 且远早于预算——
    /// 锁死 notify_channel → 同步等待者 Err 旁路（机制增量本體）。
    #[tokio::test]
    async fn oneshot_agent_error_surfaces_failed_fast() {
        let rec = record_file("auth");
        let started = std::time::Instant::now();
        let outcome = oneshot_turn(
            mock_cfg(&rec, vec![("MOCK_AUTH_ERROR".to_string(), "1".to_string())]),
            None,
            msg("oneshot-auth-probe"),
            Duration::from_secs(120),
        )
        .await;
        let SyncTurnOutcome::Failed(reason) = outcome else {
            panic!("expected Failed, got {outcome:?}");
        };
        assert!(reason.contains("认证失效"), "失败原因文案: {reason}");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "终态失败必须快速 surfacing（不得挂到预算）: {:?}",
            started.elapsed()
        );
    }

    /// 挂起的 agent：预算触发 Timeout + cancel 记录在案 + 拆栈有界。
    #[tokio::test]
    async fn oneshot_timeout_cancels_and_teardown_bounded() {
        let rec = record_file("hang");
        let started = std::time::Instant::now();
        let outcome = oneshot_turn(
            mock_cfg(
                &rec,
                vec![("MOCK_HANG_PROMPT".to_string(), "1".to_string())],
            ),
            None,
            msg("oneshot-hang-probe"),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(outcome, SyncTurnOutcome::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "Timeout 后拆栈（cancel 排水 5s + 杀进程组 5s + 余量）必须有界: {:?}",
            started.elapsed()
        );
        let events = read_records(&rec);
        assert!(
            events.iter().any(|e| e["event"] == "cancel"),
            "超时后应向在途回合发 cancel: {events:?}"
        );
    }

    /// 预算与回合上限的关系锁：调用方预算应小于 MAX_TURN_DURATION（否则硬上限
    /// 死信先落地，oneshot 只会收到 Failed 而非 Timeout）——本测试只钉常量顺序。
    #[test]
    fn oneshot_budget_below_hard_cap_doc() {
        assert!(TEARDOWN_BUDGET < MAX_TURN_DURATION);
        assert!(CANCEL_CMD_BUDGET < TEARDOWN_BUDGET);
    }
}
