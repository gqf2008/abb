//! service 后台任务统一治理（#69）：登记、优雅关闭、任务指标。
//!
//! - `TaskGovernance`：Tokio `TaskTracker` 统一登记 + `CancellationToken` 广播关闭 +
//!   指标计数。生产用全局单例 `tasks()`；测试各自构造独立实例（互不污染，可并行）。
//! - **登记口径**：service 侧长驻任务（信号/投递/事件/调度/轮询循环）与有明确收尾的
//!   短任务（启动恢复、技能安装）走 `spawn` / `spawn_forever`。bridge 侧 per-message
//!   spawn（agent 运行/typing/发送，51+ 处）**不登记**——短命/中命、有 owner（per-chat
//!   串行锁 + cancel_flags + pending.json 重启恢复），关停语义靠进程退出兜底（agent
//!   子进程由 agent-pids.json 在下次启动时清理）。登记它们会让每条消息都进 wait 图，
//!   点击停止要等所有 in-flight agent 跑完，与「停止=立即生效」的既有语义冲突。
//! - **指标**：spawned_total / running（tracker 当前在册）/ max_running /
//!   errors_total（长驻任务在关停前提前退出=异常终止）/ panics_total（catch_unwind
//!   捕获计数；panic 原文仍由默认 hook 打印，不吞诊断）/ shutdown_wait_ms。
//! - 关闭序列：`close()`（进入关闭态——在册任务清零即 wait 完成；之后新 spawn 仍被
//!   追踪，实测 tokio-util 0.7.18）→ `cancel()`（广播退出）→ `wait()`（等全部收尾）。
//!   cancel 幂等：信号任务先 cancel、shutdown_wait 再 cancel 一次是刻意冗余——
//!   无论谁先到达，广播都恰好发生一次。

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// 内部计数（Arc 共享给每个任务 future——'static 要求，不能借用 self）。
struct Counters {
    spawned: AtomicU64,
    /// 独立 running 计数（spawn 时 +1、任务收尾 -1）——比 tracker.len() 精确：
    /// 登记即计入（不等任务被调度），并发 spawn 的峰值经 fetch_max 逐次推进不错过。
    running: AtomicU64,
    errors: AtomicU64,
    panics: AtomicU64,
    max_running: AtomicU64,
}

/// 指标快照（日志/测试用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    pub spawned_total: u64,
    pub running: u64,
    pub max_running: u64,
    pub errors_total: u64,
    pub panics_total: u64,
}

pub struct TaskGovernance {
    tracker: TaskTracker,
    shutdown: CancellationToken,
    counters: Arc<Counters>,
}

impl Default for TaskGovernance {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskGovernance {
    pub fn new() -> Self {
        TaskGovernance {
            tracker: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            counters: Arc::new(Counters {
                spawned: AtomicU64::new(0),
                running: AtomicU64::new(0),
                errors: AtomicU64::new(0),
                panics: AtomicU64::new(0),
                max_running: AtomicU64::new(0),
            }),
        }
    }

    /// 关闭广播令牌（长驻循环 select 它退出）。
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// 登记并 spawn 一个**短命**任务（有明确收尾、预期在关停前自然结束）。
    /// 返回 JoinHandle 供调用方按需 await（Tracker 本身也会追踪其完成）。
    pub fn spawn(
        &self,
        name: &'static str,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        self.tracker.spawn(self.wrapped(name, false, fut))
    }

    /// 登记并 spawn 一个**长驻**任务（循环，预期活到关停）。关停广播前提前结束 =
    /// 异常终止 → errors_total + 1 + 告警日志（长驻循环中途死掉正是要治理的泄漏）。
    /// 注：设计内终态（如微信会话过期主动退出等人工介入场景）**也计** errors——
    /// 语义是「该任务没能活到关停」，宁可计错不可漏计（漏掉的才是静默泄漏）。
    pub fn spawn_forever(
        &self,
        name: &'static str,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        self.tracker.spawn(self.wrapped(name, true, fut))
    }

    fn wrapped(
        &self,
        name: &'static str,
        forever: bool,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> impl Future<Output = ()> {
        self.counters.spawned.fetch_add(1, Ordering::Relaxed);
        // running 精确峰值：每次 +1 后把新值推给 max（并发 spawn 各自推进，
        // 最后一个 +1 者看到真实峰值——不漏不重）。
        let running = self.counters.running.fetch_add(1, Ordering::Relaxed) + 1;
        self.counters
            .max_running
            .fetch_max(running, Ordering::Relaxed);
        let counters = self.counters.clone();
        let shutdown = self.shutdown.clone();
        async move {
            // catch_unwind：任务 panic 只终结自身（追踪下无人 await JoinHandle，panic
            // 会被静默吞进 join error）——捕获计数，其余任务不受影响；panic 原文仍由
            // 默认 hook 打印，不丢诊断。
            let caught =
                futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await;
            counters.running.fetch_sub(1, Ordering::Relaxed);
            if caught.is_err() {
                counters.panics.fetch_add(1, Ordering::Relaxed);
                crate::log!("[tasks] 任务「{name}」panic（已捕获隔离，其余任务不受影响）");
            }
            if forever && !shutdown.is_cancelled() {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                crate::log!("[tasks] 长驻任务「{name}」在关停前提前退出（errors_total+1）");
            }
        }
    }

    /// 优雅关闭：close（进入关闭态）→ cancel（广播退出）→ wait（等全部在册任务收尾）。
    /// 返回 wait 耗时毫秒，并打一行指标汇总日志（含 close 时在册任务数——wait 后
    /// running 恒 0，关停前快照才是 running 指标的唯一可见形态）。
    pub async fn shutdown_wait(&self) -> u64 {
        let t0 = Instant::now();
        self.tracker.close();
        let running_at_close = self.counters.running.load(Ordering::Relaxed);
        self.shutdown.cancel();
        self.tracker.wait().await;
        let ms = t0.elapsed().as_millis() as u64;
        let m = self.metrics();
        crate::log!(
            "[tasks] 关闭完成 shutdown_wait_ms={ms} spawned_total={} running_at_close={running_at_close} errors_total={} panics_total={} max_running={}",
            m.spawned_total,
            m.errors_total,
            m.panics_total,
            m.max_running
        );
        ms
    }

    /// 指标快照。
    pub fn metrics(&self) -> Metrics {
        Metrics {
            spawned_total: self.counters.spawned.load(Ordering::Relaxed),
            running: self.counters.running.load(Ordering::Relaxed),
            max_running: self.counters.max_running.load(Ordering::Relaxed),
            errors_total: self.counters.errors.load(Ordering::Relaxed),
            panics_total: self.counters.panics.load(Ordering::Relaxed),
        }
    }
}

/// 生产全局单例。
static TASKS: OnceLock<TaskGovernance> = OnceLock::new();

pub fn tasks() -> &'static TaskGovernance {
    TASKS.get_or_init(TaskGovernance::new)
}

/// 全局关闭令牌的便捷入口（长驻循环 select `token.cancelled()` 退出）。
pub fn shutdown_token() -> CancellationToken {
    tasks().shutdown_token()
}

/// Drop 时补一次 cancel（幂等）的守卫。signal 任务专用（#69 审查 Important）：
/// 任务内 panic 被 wrapped 的 catch_unwind 捕获后 future 提前结束——若无守卫，
/// 令牌永不取消、全部循环在 `cancelled().await` 永久挂起（fail-closed）。有守卫则
/// 任何提前退出路径都触发关停广播，回到原 watch 实现的 fail-open 语义（进程优雅
/// 退出、看门狗重启）。正常路径守卫 drop 再 cancel 一次，幂等无害。
pub struct CancelOnShutdown(CancellationToken);

impl Default for CancelOnShutdown {
    fn default() -> Self {
        CancelOnShutdown(shutdown_token())
    }
}

impl Drop for CancelOnShutdown {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn spawn_metrics_count() {
        let g = TaskGovernance::new();
        assert_eq!(g.metrics().spawned_total, 0);
        let h = g.spawn("t", async {});
        h.await.unwrap();
        assert_eq!(g.metrics().spawned_total, 1);
        assert!(g.metrics().max_running >= 1, "spawn 时在册数至少含自身");
        // 任务结束后不在册
        assert_eq!(g.metrics().running, 0);
    }

    #[tokio::test]
    async fn forever_task_early_exit_counts_error() {
        let g = TaskGovernance::new();
        g.spawn_forever("loop", async {}).await.unwrap();
        assert_eq!(g.metrics().errors_total, 1, "长驻任务提前退出计错误");
        assert_eq!(g.metrics().panics_total, 0);
    }

    #[tokio::test]
    async fn panic_is_caught_and_counted() {
        let g = TaskGovernance::new();
        let other_done = Arc::new(AtomicBool::new(false));
        let od = other_done.clone();
        // panic 被 wrapped 的 catch_unwind 捕获 → JoinHandle 正常完成（Err 被吞在任务内）
        g.spawn("panicky", async {
            panic!("boom");
        })
        .await
        .unwrap();
        g.spawn("healthy", async move {
            od.store(true, Ordering::Relaxed);
        })
        .await
        .unwrap();
        assert!(other_done.load(Ordering::Relaxed), "panic 不波及其它任务");
        assert_eq!(g.metrics().panics_total, 1);
        assert_eq!(g.metrics().errors_total, 0);
    }

    #[tokio::test]
    async fn shutdown_cancels_loops_and_waits() {
        let g = TaskGovernance::new();
        let exited = Arc::new(AtomicBool::new(false));
        let e = exited.clone();
        let token = g.shutdown_token();
        g.spawn_forever("loop", async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                }
            }
            e.store(true, Ordering::Relaxed);
        });
        // 循环应能在关停前一直活着（errors 不增）
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(g.metrics().errors_total, 0);
        let ms = g.shutdown_wait().await;
        assert!(exited.load(Ordering::Relaxed), "cancel 后循环退出");
        assert_eq!(g.metrics().running, 0, "wait 后无在册任务");
        assert_eq!(g.metrics().errors_total, 0, "正常关停不计错误");
        let _ = ms;
    }

    #[tokio::test]
    async fn running_counter_peaks_exactly_under_concurrency() {
        // running 在 spawn 调用时同步 +1（登记即计入，不等任务被调度）——并发 spawn
        // 的峰值经每次 +1 后 fetch_max 推进，不漏不重（#69 审查 Minor：原
        // tracker.len()+1 近似在并发下低估峰值）。
        let g = TaskGovernance::new();
        let mut handles = Vec::new();
        for _ in 0..3 {
            handles.push(g.spawn("w", async move {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }));
        }
        assert_eq!(g.metrics().running, 3, "spawn 即计入（不等调度）");
        assert!(g.metrics().max_running >= 3, "峰值不漏");
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(g.metrics().running, 0, "收尾后清零");
    }

    #[tokio::test]
    async fn cancel_on_shutdown_guard_cancels_on_drop() {
        // signal 任务提前退出（含 panic 被捕获后）→ 守卫 drop 补 cancel（fail-open）
        let g = TaskGovernance::new();
        let token = g.shutdown_token();
        assert!(!token.is_cancelled());
        {
            let _guard = CancelOnShutdown(token.clone());
        }
        assert!(token.is_cancelled(), "守卫 drop 后令牌已取消");
        // 幂等：再 cancel 不 panic、状态不变
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn spawn_after_close_still_tracked() {
        // 实测 tokio-util 0.7.18 语义：close() 只把 tracker 置为关闭态（在册清零即
        // wait 完成），之后 spawn 的任务**仍被追踪**（token 照常登记）——wait() 会等它。
        // 即 close 不是「拒绝新登记」，而是「进入关闭态」；对关停序列这是更稳的语义：
        // 关停窗口内新登记的任务不会被 wait 漏掉。
        let g = TaskGovernance::new();
        g.tracker.close();
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        let h = g.spawn("late", async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            r.store(true, Ordering::Relaxed);
        });
        g.tracker.wait().await;
        assert!(
            ran.load(Ordering::Relaxed),
            "close 后登记的任务仍被 wait 等待"
        );
        h.await.unwrap();
    }
}
