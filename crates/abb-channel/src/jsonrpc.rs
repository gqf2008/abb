//! 三个渠道共用的运行时辅助。
//!
//! # 为什么值得单独一个模块
//!
//! 合并前三个 crate 各自有一份**逐字相同**的 `rt()`（用 `md5` 比对确认过：
//! 三份哈希一致）。重复本身不会报错，但它是「改了 A 忘了改 B」的温床——
//! 比如哪天要给 runtime 加 `thread_name`，漏一处就变成「只有微信的线程名不同」。
//!
//! 这里只放**确实逐字相同**的东西。`base64_decode` 三份实现有差异（错误处理与
//! 填充细节不同），合并会改变至少一个渠道的行为，所以这次**不合并**——
//! 体积收益在 TLS 那一层，不在这些几十行的函数上；为省几百字节去动协议解析的
//! 容错边界，是拿正确性换体积的坏交易。

/// 建一个当前线程的 tokio runtime。
///
/// 单线程足够：每次工具调用都是「一次 HTTP 往返」，没有并发需求；入站长连接
/// （飞书/钉钉的 WS）自己另起线程与 runtime，不共用这个。
pub fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建 tokio runtime 失败")
}

#[cfg(test)]
mod tests {
    /// runtime 必须真能跑起来并执行任务（只 `build()` 成功不算）。
    #[test]
    fn rt_can_block_on_a_task() {
        let r = super::rt();
        assert_eq!(r.block_on(async { 1 + 1 }), 2);
    }
}
