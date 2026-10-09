//! agent spawn 的**进程保护**：全局限速 + 有界指数退避 + 熔断（owner 2026-10-04 硬要求：
//! 「agent_spawn 需要有进程保护（需要退避机制，不能出现死循环）」）。
//!
//! ## 为什么需要它（与既有退避的分工）
//! buzz/harness.rs 已有**每 agent 回路**的崩溃退避（crash_backoff + respawn_delay）——
//! 那是「回合失败后，多久再拉一次」的策略。本模块管的是更底层的**spawn 动作本身**，覆盖
//! **所有**调用方（聊天 harness、oneshot_turn（角色/团队生成、会话归纳、定时任务）、
//! 以及任何将来的调用点）。它们各自都可能写出「失败就重试」的循环，而它们**不共享**退避账
//! ⇒ 一个坏掉的 agent（例如起不来的配置）能被不同调用方各自打出 spawn 风暴。
//!
//! ## 三条机制
//! 1. **最小间隔**（MIN_INTERVAL，按 key）：同一 key 的两次 spawn 至少间隔这么久。
//! 2. **有界指数退避**（FAIL_BASE → FAIL_MAX）：连续失败后第 n 次 spawn 要等
//!    min(FAIL_BASE * 2^(n-1), FAIL_MAX) —— **有上限**，不会越等越离谱，也不会退化成 0。
//! 3. **熔断**（BREAK_AFTER 次连续失败 ⇒ 打开 BREAK_OPEN）：打开期间**拒绝**新 spawn
//!    （返回 Denied::BreakerOpen），冷却后自动半开（放一次试探；成功即复位）。
//!
//! ## 为什么「不可能死循环」是**接口保证**而不是约定
//! 唯一的入口是 async fn acquire()：它内部 sleep 掉需要等的时间**才返回**；拒绝时返回
//! Err（调用方必须处理）。调用方**没有**「立刻再 spawn」这条路径 —— 想连打也只能靠 await
//! 排队。对应单测 min_interval_is_enforced / breaker_opens_then_half_opens_and_recovers。
//!
//! 时间由参数注入（now: Instant），所以上面每条都能用假时钟做**确定性**单测，不靠 sleep。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 同一 key 两次 spawn 的最小间隔（防抖 / 防风暴）。
pub const MIN_INTERVAL: Duration = Duration::from_millis(500);
/// 退避基准：第 1 次失败后的等待。
pub const FAIL_BASE: Duration = Duration::from_secs(1);
/// 退避上限：无论连续失败多少次，等待都不超过它。
pub const FAIL_MAX: Duration = Duration::from_secs(60);
/// 静默期：连续失败计数在这么久没有新失败后归零（避免「几天前失败过」一直压着退避）。
pub const FAIL_DECAY_AFTER: Duration = Duration::from_secs(120);
/// 连续失败达到多少次就熔断。
pub const BREAK_AFTER: u32 = 5;
/// 熔断打开多久（之后半开，放一次试探）。
pub const BREAK_OPEN: Duration = Duration::from_secs(60);

/// 拒绝本次 spawn 的原因（调用方必须给出可行动提示，不许静默重试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// 熔断打开：最近连续失败过多，冷却后再试。
    BreakerOpen {
        /// 还要等多久才允许再试。
        retry_after: Duration,
        /// 触发熔断时的连续失败次数。
        failures: u32,
    },
}

/// ACP agent 的守卫 key。
///
/// 刻意**全局共用一个 key**（而不是按 bot）：熔断要防的是「这个 agent 二进制起不来」这类
/// 与 bot 无关的故障 —— 全局 key 让所有调用方（聊天/oneshot/定时任务）共享同一本退避账，
/// 这才是「无论谁重试都不会形成风暴」的保证。
pub const AGENT_KEY: &str = "acp-agent";

impl Denied {
    /// 给用户看的一句话（进死信回复，必须可行动）。
    pub fn message(&self) -> String {
        match self {
            Denied::BreakerOpen {
                retry_after,
                failures,
            } => format!(
                "agent spawn 熔断：连续失败 {} 次，约 {} 秒后可再试",
                failures,
                retry_after.as_secs().max(1)
            ),
        }
    }
}

fn backoff(failures: u32) -> Duration {
    // 2^(n-1)，n 先夹住：避免 1u64 << 64 这类溢出/恐慌。
    const CAP_SHIFT: u32 = 20;
    if failures == 0 {
        return Duration::ZERO;
    }
    let shift = failures.saturating_sub(1).min(CAP_SHIFT);
    let millis = FAIL_BASE.as_millis() as u64;
    let want = millis.saturating_mul(1u64 << shift);
    let cap = FAIL_MAX.as_millis() as u64;
    Duration::from_millis(want.min(cap))
}

#[derive(Debug, Default, Clone, Copy)]
struct KeyState {
    /// 已**预约**的下一个可用时刻（含并发排队，见 check 注释）。
    next_slot: Option<Instant>,
    /// 连续失败次数。
    failures: u32,
    /// 最近一次失败的时刻（用于静默衰减）。
    last_failure: Option<Instant>,
    /// 熔断打开到什么时候。
    open_until: Option<Instant>,
}

/// 全局 spawn 守卫（进程内单例）。
///
/// **测试进程里禁用**（`#[cfg(test)]` 返回 no-op 守卫）：大量测试故意造「agent 超时/崩溃」
/// 场景，若共享生产熔断/限速，全量并行时会被计满 `BREAK_AFTER` 触发熔断，波及其它无辜
/// 测试的 spawn（实测：`oneshot_external_cancel` / `oneshot_timeout` 两条 flaky 就是被
/// 其它测试的故意失败打满熔断后连累）。进程保护防的是「真实部署上 agent 二进制起不来」
/// 的风暴，测试进程无此风险。
#[derive(Debug)]
pub struct SpawnGuard {
    keys: Mutex<HashMap<String, KeyState>>,
    /// true = 完全不禁（测试进程）：`check` 恒放行、失败/成功记录 no-op。
    unlimited: bool,
}

impl Default for SpawnGuard {
    fn default() -> Self {
        Self {
            keys: Mutex::new(HashMap::new()),
            unlimited: false,
        }
    }
}

impl SpawnGuard {
    /// 进程内单例。测试构建返回**无限制**守卫（见结构体注释），生产构建才是真守卫。
    pub fn global() -> &'static SpawnGuard {
        static G: OnceLock<SpawnGuard> = OnceLock::new();
        G.get_or_init(|| {
            #[cfg(test)]
            {
                SpawnGuard::unlimited()
            }
            #[cfg(not(test))]
            {
                SpawnGuard::default()
            }
        })
    }

    /// 测试用的无限制守卫：`check` 恒放行、失败/成功记录均为 no-op。
    #[cfg(test)]
    fn unlimited() -> Self {
        Self {
            keys: Mutex::new(HashMap::new()),
            unlimited: true,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, KeyState>> {
        // 毒锁也继续用：本模块内部只做算术，不会在持锁时 panic；宁可继续，不要连带崩掉 spawn。
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 判定 + **预约槽位**（纯函数式，now 注入 ⇒ 可确定性单测）。
    ///
    /// 返回 Ok(wait) = 允许，且需要先 sleep(wait)；返回 Err(Denied) = 拒绝（不许 spawn）。
    ///
    /// 预约语义：调用时就把它自己排到「next_slot + 退避」这个位置，**并发调用者会拿到各自的
    /// 槽位**（而不是同时通过检查、一起 spawn）。
    pub fn check(&self, key: &str, now: Instant) -> Result<Duration, Denied> {
        if self.unlimited {
            return Ok(Duration::ZERO);
        }
        let mut map = self.lock();
        let st = map.entry(key.to_string()).or_default();

        // 静默衰减：太久没失败过就把连续失败清零。
        if let Some(t) = st.last_failure {
            if now.saturating_duration_since(t) >= FAIL_DECAY_AFTER {
                st.failures = 0;
                st.open_until = None;
            }
        }

        // 熔断仍打开 ⇒ 直接拒绝（不预约，避免冷却期还被排队拉长）。
        if let Some(until) = st.open_until {
            if now < until {
                return Err(Denied::BreakerOpen {
                    retry_after: until.saturating_duration_since(now),
                    failures: st.failures,
                });
            }
            // 冷却结束 ⇒ 半开：这次放行（下面照常算槽位），成功会复位、失败会再次打开。
            st.open_until = None;
        }

        let base = st.next_slot.unwrap_or(now);
        let earliest = base.max(now);
        let extra = backoff(st.failures);
        let slot = earliest + extra;
        let wait = slot.saturating_duration_since(now);
        st.next_slot = Some(slot + MIN_INTERVAL);
        Ok(wait)
    }

    /// spawn 前调用：需要等就等（**这是唯一入口，调用方绕不过去**）。
    pub async fn acquire(&self, key: &str) -> Result<(), Denied> {
        let wait = self.check(key, Instant::now())?;
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(())
    }

    /// 报告一次 spawn 后**很快失败**（agent 起了就死 / 起不来）：累加连续失败，必要时熔断。
    pub fn record_failure(&self, key: &str, now: Instant) {
        if self.unlimited {
            return;
        }
        let mut map = self.lock();
        let st = map.entry(key.to_string()).or_default();
        st.failures = st.failures.saturating_add(1);
        st.last_failure = Some(now);
        if st.failures >= BREAK_AFTER {
            st.open_until = Some(now + BREAK_OPEN);
            crate::log!(
                "[spawn] 熔断打开：{key} 连续失败 {} 次，冷却 {}s 内不再 spawn",
                st.failures,
                BREAK_OPEN.as_secs()
            );
        }
    }

    /// 报告一次**健康**运行（回合成功 / 进程存活够久）：连续失败清零、熔断复位。
    pub fn record_success(&self, key: &str) {
        if self.unlimited {
            return;
        }
        let mut map = self.lock();
        if let Some(st) = map.get_mut(key) {
            st.failures = 0;
            st.last_failure = None;
            st.open_until = None;
        }
    }

    /// 只读快照（诊断用：日志/测试）。
    pub fn snapshot(&self, key: &str) -> (u32, Option<Duration>) {
        if self.unlimited {
            return (0, None);
        }
        let now = Instant::now();
        let map = self.lock();
        match map.get(key) {
            Some(st) => (
                st.failures,
                st.open_until.map(|u| u.saturating_duration_since(now)),
            ),
            None => (0, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> SpawnGuard {
        SpawnGuard::default()
    }

    /// 最小间隔：第二次允许但**必须等**，且等待 >= MIN_INTERVAL（这就是「不可能死循环」的判据）。
    #[test]
    fn min_interval_is_enforced() {
        let g = g();
        let t0 = Instant::now();
        assert_eq!(g.check("k", t0), Ok(Duration::ZERO), "首次立即可用");
        let wait = g.check("k", t0 + Duration::from_millis(100)).unwrap();
        assert!(
            wait >= MIN_INTERVAL - Duration::from_millis(100),
            "第二次必须等到槽位：{wait:?}"
        );
        assert!(wait <= MIN_INTERVAL, "不该多等：{wait:?}");
    }

    /// 并发调用者拿**不同槽位**（否则会一起 spawn 出去）。
    #[test]
    fn concurrent_checkers_get_distinct_slots() {
        let g = g();
        let t0 = Instant::now();
        let a = g.check("k", t0).unwrap();
        let b = g.check("k", t0).unwrap();
        let c = g.check("k", t0).unwrap();
        assert_eq!(a, Duration::ZERO);
        assert!(b >= MIN_INTERVAL, "b={b:?}");
        assert!(
            c >= b + MIN_INTERVAL - Duration::from_millis(1),
            "c={c:?} b={b:?}"
        );
    }

    /// 退避随连续失败递增，但**有上限**（不会无限增长，也不会归零）。
    #[test]
    fn backoff_grows_and_is_capped() {
        assert!(backoff(0).is_zero(), "0 次不该有退避");
        assert!(backoff(1) >= FAIL_BASE);
        assert!(backoff(2) > backoff(1));
        assert!(backoff(10) > backoff(5));
        assert_eq!(backoff(1_000_000), FAIL_MAX, "必须夹在上限");
        assert_eq!(backoff(u32::MAX), FAIL_MAX, "极端值也不许溢出/恐慌");
    }

    /// 连续失败到阈值 ⇒ 熔断；冷却后自动半开（放一次），成功即复位。
    #[test]
    fn breaker_opens_then_half_opens_and_recovers() {
        let g = g();
        let t0 = Instant::now();
        for i in 0..BREAK_AFTER {
            g.record_failure("k", t0 + Duration::from_millis(u64::from(i)));
        }
        match g.check("k", t0 + Duration::from_secs(10)) {
            Err(Denied::BreakerOpen {
                retry_after,
                failures,
            }) => {
                assert_eq!(failures, BREAK_AFTER);
                assert!(
                    retry_after <= BREAK_OPEN,
                    "冷却不该超过上限：{retry_after:?}"
                );
            }
            other => panic!("应熔断，实得 {other:?}"),
        }
        // 冷却结束 ⇒ 半开放行（仍然要排队，不是「立刻无限打」）。
        let after = t0 + BREAK_OPEN + Duration::from_secs(60);
        assert!(g.check("k", after).is_ok(), "冷却后应放行试探");
        g.record_success("k");
        assert_eq!(g.snapshot("k").0, 0, "成功必须复位失败计数");
        assert!(g.check("k", after).is_ok());
    }

    /// 健康运行复位：失败几次后成功 ⇒ 退避账清零。
    #[test]
    fn success_resets_failure_count() {
        let g = g();
        let t0 = Instant::now();
        g.record_failure("k", t0);
        g.record_failure("k", t0);
        assert_eq!(g.snapshot("k").0, 2);
        g.record_success("k");
        assert_eq!(g.snapshot("k").0, 0);
        let wait = g.check("k", t0 + Duration::from_secs(1)).unwrap();
        assert!(wait <= MIN_INTERVAL, "复位后只受最小间隔约束：{wait:?}");
    }

    /// 静默期衰减：很久没有再失败，就不该继续压着退避/熔断。
    #[test]
    fn failures_decay_after_quiet_period() {
        let g = g();
        let t0 = Instant::now();
        for i in 0..BREAK_AFTER {
            g.record_failure("k", t0 + Duration::from_millis(u64::from(i)));
        }
        let later = t0 + FAIL_DECAY_AFTER + BREAK_OPEN + Duration::from_secs(1);
        assert!(g.check("k", later).is_ok(), "久未失败应已衰减");
        assert_eq!(g.snapshot("k").0, 0);
    }

    /// 拒绝文案必须可行动（带「还要等多久」），不能只说「失败了」。
    #[test]
    fn denied_message_is_actionable() {
        let d = Denied::BreakerOpen {
            retry_after: Duration::from_secs(42),
            failures: 7,
        };
        let m = d.message();
        assert!(m.contains("熔断"), "{m}");
        assert!(m.contains('7'.to_string().as_str()), "{m}");
        assert!(m.contains("42"), "要给出等待时间：{m}");
    }

    /// key 之间互不影响（一个 bot 挂了不该拖住其它 bot 的 spawn）。
    #[test]
    fn keys_are_isolated() {
        let g = g();
        let t0 = Instant::now();
        for i in 0..BREAK_AFTER {
            g.record_failure("bot-a", t0 + Duration::from_millis(u64::from(i)));
        }
        assert!(matches!(
            g.check("bot-a", t0 + Duration::from_secs(5)),
            Err(Denied::BreakerOpen { .. })
        ));
        assert_eq!(
            g.check("bot-b", t0 + Duration::from_secs(5)),
            Ok(Duration::ZERO),
            "另一个 key 不该受影响"
        );
    }
}
