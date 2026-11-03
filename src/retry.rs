//! 渠道重连退避的**唯一事实源**。
//!
//! ## 为什么抽出来
//!
//! 三条内置渠道（微信 / 钉钉）与 MCP 渠道各自有一份重连循环，2026-11-03 之前
//! 它们的策略不一致：
//!
//! | 渠道 | 原策略 | 长期故障一天日志 |
//! |---|---|---|
//! | MCP | 10 次后退出（后来改成 30s 永不放弃） | — |
//! | 微信 | **固定 3s** | ~28800 条 |
//! | 钉钉 | **固定 3s + 抖动** | ~28800 条 |
//! | 飞书 WS | 服务端下发（默认 90s） | ~960 条 |
//!
//! **固定 3s 的问题不是「重连太勤」，而是「日志刷爆」**——日志是排障唯一线索，
//! 一天 28800 条等于把它毁了。而日志量依赖的是间隔，不是重连意愿。
//!
//! 所以统一成：**快速起步（3s）+ 指数增长 + 30s 封顶 + 永不放弃**。
//! - 偶发抖动：第一次重试仍是 3s，用户无感
//! - 长期故障：稳定在 30s，日志约 2880 条/天（可接受）
//! - **永不放弃**：owner 明确要求（MCP 那次），这里是同一口径
//!
//! ## 与 `mcp_channel_loop::backoff` 的关系
//!
//! 那个函数多一个「10 次以内 / 超过」的两段语义（因为 owner 指定了 MCP 的 30s）。
//! 本模块是**通用**版本：纯指数 + 封顶，没有阈值分段——内置渠道没有那个特殊要求。

use std::time::Duration;

/// 首次重试间隔。与原来的固定 3s 一致，保证偶发抖动的体验不变。
pub const START: Duration = Duration::from_secs(3);

/// 封顶间隔。30s = 一天约 2880 条日志，且服务恢复后最多等 30s。
pub const MAX: Duration = Duration::from_secs(30);

/// 第 `fails` 次连续失败之后该等多久。
///
/// `fails == 0` 返回 [`START`]（调用方不该在 0 次失败时等，但保持返回值有意义便于测试）。
/// 指数增长到 [`MAX`] 封顶。
///
/// **不会溢出**：位移前先 `min(4)`（2^4=16，3s×16=48s > 30s 已封顶），
/// 所以 `fails` 再大也只走同一条路径。
pub fn backoff(fails: u32) -> Duration {
    if fails == 0 {
        return START;
    }
    let factor = 1u32 << fails.saturating_sub(1).min(4);
    START.saturating_mul(factor).min(MAX)
}

/// 是否该放弃重连。
///
/// **恒为 `false`**——owner 2026-11-03 明确要求「不要退出」。
///
/// 保留这个函数而不是删掉调用点，是为了让「要不要放弃」这个决定只出现在**一处**：
/// 将来若要重新引入上限（比如加 `max_retry_minutes` 配置），改这里就够。
pub fn should_give_up(_fails: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_then_caps_at_thirty_seconds() {
        assert_eq!(backoff(0), START);
        assert_eq!(backoff(1), Duration::from_secs(3));
        assert_eq!(backoff(2), Duration::from_secs(6));
        assert_eq!(backoff(3), Duration::from_secs(12));
        assert_eq!(backoff(4), Duration::from_secs(24));
        // 第 5 次指数上是 48s，被 30s 封顶
        assert_eq!(backoff(5), MAX);
        // 再多次也不涨
        assert_eq!(backoff(100), MAX);
    }

    /// **不溢出**：`fails` 会无限增长（只要不放弃），而这里有位移运算。
    #[test]
    fn never_overflows_on_huge_failure_counts() {
        assert_eq!(backoff(u32::MAX), MAX);
        assert_eq!(backoff(u32::MAX - 1), MAX);
    }

    /// owner 决定：**永不放弃**。
    #[test]
    fn never_gives_up() {
        for n in [0, 1, 3, 10, 100, 10_000, u32::MAX] {
            assert!(
                !should_give_up(n),
                "连续失败 {n} 次也不得放弃——owner 2026-11-03：不要退出"
            );
        }
    }

    /// 封顶值是「日志量」与「恢复延迟」的折中，两条都要成立。
    #[test]
    fn cap_is_a_reasonable_compromise() {
        // 恢复延迟上限：一次长时间故障后，最多等 MAX 就重新取消息
        assert!(MAX <= Duration::from_secs(30));
        // 日志量上限：一天不超过 3000 条
        let per_day = 86_400 / MAX.as_secs();
        assert!(per_day <= 3000, "一天 {per_day} 条日志太多");
    }
}
