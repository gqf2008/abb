//! 运行时日志的**唯一出口**（资源事件链路：丢弃 / 窗口汇总 / 孤儿更新 / 图片-drop 等）。
//!
//! # 为什么存在（评审 O2 / N3）
//!
//! 本仓的生产可见日志路径是 [`crate::log!`]（写 stdout，由 launchd/plist 落到 `bridge.out`）；
//! 而**根二进制从未安装 tracing subscriber**（`tracing` 只是空壳）——所以任何
//! `tracing::*` 在这种"丢失/丢弃/失败"语义的位置上都等于**没有日志**。
//!
//! 把出口收成一个薄函数有两个好处：
//! 1. "接线"可以被一条**有边界**的源码守卫钉住（见 `harness.rs` 的
//!    `resource_event_logs_wire_to_stdout_not_tracing`），而不是只扫某个函数的词法范围；
//! 2. 两侧（`harness` 的资源事件、`acp` 的资源分派）**共用同一个出口**，不会出现
//!    "harness 侧有守卫、acp 侧没有"的不对称（评审 N3 实测过那种不对称：
//!    把 acp 侧的 `crate::log!` 换回 `tracing::warn!`，当时全部用例仍绿）。
//!
//! 边界（如实写明）：这是**源码级**收敛，不是编译期强制——有人绕过它直接写 `crate::log!`/
//! `tracing::*` 时由守卫的文本断言兜（见守卫注释里实测的盲区：在 handler 内**并行**新增一条走
//! 函数体外 helper 的日志调用，守卫看不出来）。

/// 写一行运行时日志到 stdout（唯一出口）。
pub(crate) fn resource_line(line: &str) {
    crate::log!("{line}");
}
