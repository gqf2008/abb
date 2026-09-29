//! ABB 的库目标（A1 第一批引入）。
//!
//! 存在的唯一理由：让**主程序**与**独立二进制**共享同一份逻辑——提权出口
//! [`elev`] 既要给 `src/bin/abb-elev-helper.rs` 用，将来也要给主程序用，而
//! `src/main.rs` 的模块树只属于 `agent-bridge` 这一个 bin，不能跨 bin 复用。
//!
//! ⚠️ 这里**不要** `mod` 主程序现有的那些模块（`agent` / `service` / `ui` …）：
//! 那会让它们被编译两份（lib + bin），重复的静态初始化与符号会带来难以察觉的漂移。
//! 本 lib 只暴露真正需要跨目标复用的东西。
//!
//! 例外（2026-09-28，批 `abb-svc-persist-password-gate`）：[`spawn`] 与 [`svc_task`] 两个
//! **纯叶子模块**（无静态状态、无副作用、只有常量与纯函数）也声明在这里 —— 提权 helper
//! 侧的 `elev::win` 要跑 `schtasks`（控制台程序，必须走统一入口抑制黑框）并要用计划任务的
//! XML 构建器，而 helper 是独立 bin、只能看到本 lib。它们没有静态初始化，双份编译不会带来
//! 上面那种漂移；控制台抑制的护栏（`spawn::tests::creation_flags_only_in_this_module`）
//! 在两个 crate 的测试里都会跑，覆盖面反而更大。
//!
//! 本批**不接线** GUI/CLI：`elev` 目前没有调用点，只有单测与 helper 二进制在使用它。

pub mod elev;
pub mod spawn;
pub mod svc_task;
