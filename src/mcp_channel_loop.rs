//! MCP 渠道的**事件循环**（总线 S2d）：把 S2/S2b/S2c 三段串起来。
//!
//! ## 它做的事
//!
//! ```text
//! spawn client → initialize → tools/list + resources/list → negotiate
//!   ↓ 循环
//!   有订阅：等通知 → resources/read → 消息
//!   无订阅：调 poll → 消息
//!   ↓
//!   deliver_batch（去重/LRU）→ mcp_adapter::to_ev → bridge.handle(ev)
//! ```
//!
//! ## 为什么这段单独成文件而不是塞进 `service::run_bot`
//!
//! `run_bot` 里已经有 ACP 句柄、工作区版本管理、回合投递三块，再塞一个
//! 「连外部进程 + 循环 + 协议协商」会让那 40 行变成 200 行且不可单测。
//! 这里只依赖三个**已经测过**的模块（`mcp_client`/`mcp_inbound`/`mcp_adapter`）
//! 加一个投递闭包，于是「循环本身」也能用假 client 单测。
//!
//! ## 状态上报
//!
//! 进循环前报「连接中」，成功取到一轮报「在线」，退出报「清空」。
//! **不能只报「连接中」然后就不管**：那样渠道死了托盘点还看得到「在线」，
//! 而本期最常见的故障就是 MCP 服务退出。
//!
//! ## 重连
//!
//! client 死掉（stdout 关闭）⇒ 退避重连，而不是让 bot 任务退出。
//! 但**连续失败超过上限就放弃**（否则一个永远起不来的配置会无限刷日志）。
//!
//! ## 阻塞风险
//!
//! `mcp_client` 是**同步**的（`std::process` + `read_until`），而本循环在 tokio 里。
//! 直接调用会阻塞 worker 线程 ⇒ 全部放到 `spawn_blocking` 里跑，
//! 用通道把消息送回 async 侧投递。这是本模块最容易写错的地方，有测试钉着。

use std::sync::Arc;

use crate::bridge::Bridge;
use crate::mcp_adapter::to_ev;
use crate::mcp_bus::ChannelMessage;
use crate::mcp_inbound_loop::{deliver_batch, InboundSink, InboundState};

/// 重连退避的起始与上限。
pub const BACKOFF_START: std::time::Duration = std::time::Duration::from_secs(2);
/// 退避封顶。**问过 Jev**：`cap_15` p=0.54 / `keep_60` p=0.45
/// （conf=0.31——**它自己也不确定，基本是抛硬币**）。选 15s 的实际理由：
/// 封顶即「服务恢复后最多等这么久才重新取消息」，IM 场景 60s 太迟；
/// 代价只是长故障期间每分钟 4 次请求而不是 1 次（子进程本地调用，可忽略）。
pub const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(15);

/// 连续失败多少次后放弃。**必须有上限**：一个永远起不来的配置
/// 无限重连会刷爆日志，而日志是排障唯一线索。
///
/// **问过 Jev**：`keep`(10 次) p=0.58 / conf=0.44——**它自己也不确定**
/// （`raise_to_30` 有 0.21）。保持 10 的好处是配合 15s 封顶，
/// 最坏约 2 分钟内暴露配置错误，而不是让用户等半天才发现连不上。
pub const MAX_CONSECUTIVE_FAILURES: u32 = 10;

/// 下一轮退避时长（指数增长，封顶）。
///
/// 抽成纯函数是为了能直接测边界（0 次、1 次、很多次），不必等真实时间。
pub fn backoff(failures: u32) -> std::time::Duration {
    if failures == 0 {
        return BACKOFF_START;
    }
    let factor = 1u32 << failures.min(5); // 2^5=32，够了
    let d = BACKOFF_START.saturating_mul(factor);
    d.min(BACKOFF_MAX)
}

/// 是否该放弃。
pub fn should_give_up(consecutive: u32) -> bool {
    consecutive >= MAX_CONSECUTIVE_FAILURES
}

/// 把消息投给 `Bridge` 的 sink。
///
/// 单独抽出来（而不是在循环里内联）是为了能测「翻译失败怎么办」：
/// `to_ev` 失败的消息**不入桥**，但要记日志——不能静默吞。
pub struct BridgeSink {
    bridge: Arc<Bridge>,
}

impl BridgeSink {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl InboundSink for BridgeSink {
    async fn deliver(&self, msg: &ChannelMessage) -> Result<(), String> {
        let ev = match to_ev(msg) {
            Ok(ev) => ev,
            Err(e) => {
                // 翻译失败 ⇒ 这条进不了桥。**记日志但不让整批失败**：
                // 一条畸形消息不该阻断同批其它消息。
                crate::log!(
                    "[mcp] 消息无法进桥（chat={} mid={}）：{e}",
                    msg.chat_id,
                    msg.message_id
                );
                return Err(e);
            }
        };
        self.bridge.handle(ev).await;
        Ok(())
    }
}

/// 一次拉取的抽象：让循环本身能单测（真实现走 `mcp_inbound`）。
///
/// 用 trait 而不是直接调 `mcp_inbound`：那段要真子进程，测不了「循环的重连逻辑」。
#[async_trait::async_trait]
///
/// 只需要 `Send` 不需要 `Sync`：真实现内部持有 `StdioChannelClient`，而它含有
/// `mpsc::Receiver`（**不是 `Sync`**）。这不是妥协——`fetch(&mut self)` 本来就是
/// 独占语义，多线程共享它没有意义。
pub trait InboundSource: Send {
    /// 取一批消息。`Err` 表示这一轮失败（调用方据此退避/计数）。
    async fn fetch(&mut self) -> Result<Vec<ChannelMessage>, String>;
}

/// MCP 渠道的**主循环**（S2d 接线点）。
///
/// 起 client → 握手 → 能力协商 → 循环取消息投给 Bridge。
/// client 死掉就退避重连；连续失败超上限则退出（不无限刷日志）。
pub async fn run(
    bot_key: &str,
    name: &str,
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    bridge: std::sync::Arc<Bridge>,
    stop: tokio_util::sync::CancellationToken,
) {
    let sink = BridgeSink::new(bridge);
    let mut consecutive: u32 = 0;
    loop {
        if stop.is_cancelled() {
            return;
        }
        crate::botstatus::report(bot_key, "mcp", name, "连接中");

        // 建连 + 协商（同步，放阻塞线程）。
        let name_owned = name.to_string();
        let program_owned = program.to_string();
        let args_owned: Vec<String> = args.to_vec();
        let envs_owned: Vec<(String, String)> = envs.to_vec();
        let built = tokio::task::spawn_blocking(move || {
            build_source(&name_owned, &program_owned, &args_owned, &envs_owned)
        })
        .await;

        let mut source = match built {
            Ok(Ok(src)) => {
                consecutive = 0;
                crate::botstatus::report(bot_key, "mcp", name, "在线");
                crate::log!("[mcp:{bot_key}] 渠道 `{name}` 已连上");
                src
            }
            Ok(Err(e)) => {
                consecutive += 1;
                crate::log!("[mcp:{bot_key}] 连接渠道 `{name}` 失败（第 {consecutive} 次）：{e}");
                if should_give_up(consecutive) {
                    crate::log!(
                        "[mcp:{bot_key}] 连续失败 {consecutive} 次，放弃重连（检查 mcp_channel 配置）"
                    );
                    crate::botstatus::report(bot_key, "mcp", name, "连接失败");
                    return;
                }
                crate::botstatus::report(bot_key, "mcp", name, "重连中");
                if wait_or_cancel(&stop, backoff(consecutive)).await {
                    return;
                }
                continue;
            }
            Err(e) => {
                crate::log!("[mcp:{bot_key}] 建连任务 join 失败：{e}");
                return;
            }
        };

        // 取消息循环。一次 fetch 失败不退出：可能是临时故障，退避后接着跑。
        let mut state = crate::mcp_inbound_loop::InboundState::new(4096);
        loop {
            if stop.is_cancelled() {
                return;
            }
            match tick(&mut state, source.as_mut(), &sink).await {
                Ok(report) => {
                    if report.delivered > 0 {
                        consecutive = 0;
                        crate::botstatus::report(bot_key, "mcp", name, "在线");
                    }
                    let delay = crate::mcp_inbound_loop::next_delay(&report);
                    if wait_or_cancel(&stop, delay).await {
                        return;
                    }
                }
                Err(e) => {
                    consecutive += 1;
                    crate::log!("[mcp:{bot_key}] 取消息失败（第 {consecutive} 次）：{e}");
                    crate::botstatus::report(bot_key, "mcp", name, "重连中");
                    if should_give_up(consecutive) {
                        crate::log!("[mcp:{bot_key}] 连续失败 {consecutive} 次，退出渠道");
                        return;
                    }
                    if wait_or_cancel(&stop, backoff(consecutive)).await {
                        return;
                    }
                    // 断开重连：丢掉当前 source，回外层重新建连。
                    break;
                }
            }
        }
    }
}

/// 等一段时间；被取消则返回 `true`（调用方据此退出）。
async fn wait_or_cancel(
    stop: &tokio_util::sync::CancellationToken,
    d: std::time::Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = stop.cancelled() => true,
    }
}

/// 建连 + 协商入站方式（同步；由调用方放进 `spawn_blocking`）。
/// 测试可见的建连入口（端到端全链路用）。
///
/// 与生产 [`run`] 走同一个 `build_source`，避免"测试走另一条路"的假绿。
#[cfg(test)]
pub fn build_source_for_test(
    name: &str,
    program: &str,
    args: &[String],
    envs: &[(String, String)],
) -> Result<Box<dyn InboundSource>, String> {
    build_source(name, program, args, envs)
}

fn build_source(
    name: &str,
    program: &str,
    args: &[String],
    envs: &[(String, String)],
) -> Result<Box<dyn InboundSource>, String> {
    let mut client = crate::mcp_client::StdioChannelClient::spawn(name, program, args, envs)
        .map_err(|e| format!("起不来渠道服务：{e}"))?;
    // 握手失败不致命（tools/list 才是真判据），与 `McpChannelMessenger::connect` 同口径。
    let _ = client.call(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "abb", "version": env!("CARGO_PKG_VERSION")},
        }),
    );
    let tools = client
        .call("tools/list", serde_json::json!({}))
        .map_err(|e| format!("tools/list 失败：{e}"))?;
    let resources = client
        .call("resources/list", serde_json::json!({}))
        .unwrap_or(serde_json::Value::Null);
    let caps = crate::mcp_bus::capabilities_from(&tools, &resources)
        .map_err(|e| format!("能力清单形状不对：{e:?}"))?;
    let inbound = crate::mcp_inbound::negotiate(&mut client, &caps).ok_or_else(|| {
        "既没有可订阅的 inbox 资源，也没有 poll 工具 ⇒ 该渠道收不到消息".to_string()
    })?;
    Ok(match inbound {
        crate::mcp_inbound::ChannelInbound::Subscribe { .. } => {
            Box::new(SubscribeSource::new(client, inbound, DEFAULT_FETCH_LIMIT))
        }
        crate::mcp_inbound::ChannelInbound::Poll => {
            Box::new(PollSource::new(client, DEFAULT_FETCH_LIMIT))
        }
    })
}

/// 单次取消息上限。
pub const DEFAULT_FETCH_LIMIT: usize = 100;

/// 跑一轮：取 → 去重投递。返回本轮报告（失败会体现为 `Err`）。
pub async fn tick(
    state: &mut InboundState,
    source: &mut dyn InboundSource,
    sink: &dyn InboundSink,
) -> Result<crate::mcp_inbound_loop::TickReport, String> {
    let batch = source.fetch().await?;
    Ok(deliver_batch(state, sink, &batch).await)
}

/// 真实现：一条长驻的订阅连接（A 方案）。
///
/// 每次 `fetch` 先排空通知通道，再对涉及的 URI 逐个 `resources/read`。
/// 没通知就返回空批次（**不是错误**：没新消息是常态）。
pub struct SubscribeSource {
    client: Option<crate::mcp_client::StdioChannelClient>,
    inbound: crate::mcp_inbound::ChannelInbound,
    limit: usize,
}

impl SubscribeSource {
    pub fn new(
        client: crate::mcp_client::StdioChannelClient,
        inbound: crate::mcp_inbound::ChannelInbound,
        limit: usize,
    ) -> Self {
        Self {
            client: Some(client),
            inbound,
            limit,
        }
    }
}

#[async_trait::async_trait]
impl InboundSource for SubscribeSource {
    async fn fetch(&mut self) -> Result<Vec<ChannelMessage>, String> {
        // 同步 client 放进 spawn_blocking —— 见模块注释「阻塞风险」。
        // `spawn_blocking` 要求 'static，所以把 client **移出**再移回（`&mut self` 独占，
        // 不会有人并发用）。用 `Option::take` 而不是 `mem::replace` + 占位进程：
        // 占位进程会真的 spawn 一个 /bin/true，白起进程还可能失败。
        let client = self
            .client
            .take()
            .ok_or_else(|| "内部错误：client 已被取走（上一次 fetch 没归还？）".to_string())?;
        let inbound = self.inbound.clone();
        let limit = self.limit;
        let res = tokio::task::spawn_blocking(move || {
            let mut c = client;
            let notes = c.drain_notifications();
            let out = if notes.is_empty() {
                Ok(vec![])
            } else {
                crate::mcp_inbound::messages_from_notifications(&mut c, &inbound, &notes, limit)
            };
            (c, out)
        })
        .await
        .map_err(|e| format!("读资源任务 join 失败：{e}"))?;
        let (client, out) = res;
        // **无论成功失败都归还**：否则下一轮 fetch 直接报「client 被取走」，
        // 一次偶发失败会变成渠道永久失效。
        self.client = Some(client);
        out
    }
}

/// 真实现：轮询（B 方案）。
pub struct PollSource {
    client: Option<crate::mcp_client::StdioChannelClient>,
    state_cursor: Option<String>,
    limit: usize,
}

impl PollSource {
    pub fn new(client: crate::mcp_client::StdioChannelClient, limit: usize) -> Self {
        Self {
            client: Some(client),
            state_cursor: None,
            limit,
        }
    }
}

#[async_trait::async_trait]
impl InboundSource for PollSource {
    async fn fetch(&mut self) -> Result<Vec<ChannelMessage>, String> {
        let client = self
            .client
            .take()
            .ok_or_else(|| "内部错误：client 已被取走".to_string())?;
        let cursor = self.state_cursor.clone();
        let limit = self.limit;
        let res = tokio::task::spawn_blocking(move || {
            let mut c = client;
            let out = crate::mcp_inbound::poll_once(&mut c, cursor.as_deref(), limit);
            (c, out)
        })
        .await
        .map_err(|e| format!("轮询任务 join 失败：{e}"))?;
        let (client, out) = res;
        self.client = Some(client);
        let batch = out?;
        // 游标只在成功后推进（S2b 的口径）——`out?` 已经把失败挡在上面了。
        self.state_cursor = batch.next_cursor;
        Ok(batch.messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_inbound_loop::TickReport;
    use std::sync::Mutex;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(0), BACKOFF_START);
        assert_eq!(backoff(1), BACKOFF_START * 2);
        assert_eq!(backoff(2), BACKOFF_START * 4);
        // 封顶：5 次以上不再增长
        assert_eq!(backoff(5), BACKOFF_MAX);
        assert_eq!(backoff(50), BACKOFF_MAX);
    }

    #[test]
    fn gives_up_after_limit() {
        assert!(!should_give_up(0));
        assert!(!should_give_up(MAX_CONSECUTIVE_FAILURES - 1));
        assert!(should_give_up(MAX_CONSECUTIVE_FAILURES));
        assert!(should_give_up(MAX_CONSECUTIVE_FAILURES + 5));
    }

    fn msg(id: &str) -> ChannelMessage {
        ChannelMessage {
            chat_id: "c".into(),
            message_id: id.into(),
            sender_id: String::new(),
            sender_name: String::new(),
            text: "hi".into(),
            ts: 1,
            attachments: vec![],
        }
    }

    /// 可配置返回：第 n 次失败 / 返回哪些消息。
    struct FakeSource {
        /// 依次返回：`Some(msgs)` = 成功，`None` = 失败。
        script: Mutex<Vec<Option<Vec<ChannelMessage>>>>,
    }

    impl FakeSource {
        fn new(script: Vec<Option<Vec<ChannelMessage>>>) -> Self {
            Self {
                script: Mutex::new(script),
            }
        }
    }

    #[async_trait::async_trait]
    impl InboundSource for FakeSource {
        async fn fetch(&mut self) -> Result<Vec<ChannelMessage>, String> {
            let mut s = self.script.lock().unwrap();
            if s.is_empty() {
                return Ok(vec![]);
            }
            match s.remove(0) {
                Some(m) => Ok(m),
                None => Err("拉取失败".into()),
            }
        }
    }

    struct CountSink {
        got: Mutex<Vec<String>>,
    }

    impl CountSink {
        fn new() -> Self {
            Self {
                got: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl InboundSink for CountSink {
        async fn deliver(&self, m: &ChannelMessage) -> Result<(), String> {
            self.got.lock().unwrap().push(m.message_id.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn tick_delivers_what_source_returns() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![Some(vec![msg("1"), msg("2")])]);
        let sink = CountSink::new();
        let r = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 2);
    }

    /// **拉取失败要冒泡成 Err**，不能当成「这轮没消息」——
    /// 后者会让循环永远以为渠道正常。
    #[tokio::test]
    async fn tick_propagates_fetch_failure() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![None]);
        let sink = CountSink::new();
        let err = tick(&mut st, &mut src, &sink).await.unwrap_err();
        assert!(err.contains("拉取失败"));
    }

    /// 失败的那一轮**不该影响下一轮**：状态要能继续用。
    #[tokio::test]
    async fn failure_round_does_not_poison_state() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![None, Some(vec![msg("1")])]);
        let sink = CountSink::new();
        assert!(tick(&mut st, &mut src, &sink).await.is_err());
        let r = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 1);
    }

    /// 同一批消息第二轮应全部去重（跨 tick 的状态要保留）。
    #[tokio::test]
    async fn dedupe_persists_across_ticks() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![Some(vec![msg("1")]), Some(vec![msg("1")])]);
        let sink = CountSink::new();
        let r1 = tick(&mut st, &mut src, &sink).await.unwrap();
        let r2 = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r1.delivered, 1);
        assert_eq!(r2.delivered, 0);
        assert_eq!(r2.deduped, 1);
    }

    /// 空批次是正常的（没新消息），不该算失败。
    #[tokio::test]
    async fn empty_batch_is_ok_not_error() {
        let mut st = InboundState::new(10);
        let mut src = FakeSource::new(vec![Some(vec![])]);
        let sink = CountSink::new();
        let r: TickReport = tick(&mut st, &mut src, &sink).await.unwrap();
        assert_eq!(r.delivered, 0);
    }
}
