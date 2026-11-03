//! 渠道入站的**驱动循环**（总线 S2b）：把协议层（[`crate::mcp_inbound`]）接上投递侧。
//!
//! ## 为什么不复用 `Bridge::on_payload`
//!
//! 那个入口吃的是**飞书事件 JSON**（`header.event_type == "im.message.receive_v1"`、
//! `event.message.*`）。MCP 入站给的是契约 §3.3 的 [`ChannelMessage`]——硬塞进去
//! 就得伪造一份飞书 JSON，等于把「渠道差异」重新引进 ABB，正好违背总线目标。
//!
//! 所以这里定义一个**渠道无关**的投递抽象 [`InboundSink`]：循环只负责
//! 「什么时候取、怎么去重、出错怎么办」，投递侧怎么处理是它自己的事。
//! 这样本模块可单测（用假 sink），而真实接线（adapter → `Bridge`）是 S2c 的事。
//!
//! ## 三个必须做对的地方
//!
//! 1. **去重键用 `(chat_id, message_id)`**，不是 `message_id` 单独——不同渠道的
//!    message_id 可能撞车（尤其邮件线程键与 IM 消息 id 形态完全不同）。
//! 2. **出错不能停循环**。渠道服务偶发失败（网络抖动、服务重启）是常态；
//!    一次失败就退出会让「渠道静默失效」——托盘里看不到任何提示。
//!    但也**不能无限重试同一批**：失败的批次记下并继续，下一轮再取。
//! 3. **游标只在成功后推进**。失败却推进游标 = 那批消息永久丢失。
//!    （游标本身不在这里：它是 `PollSource` 的 `state_cursor`（`mcp_channel_loop.rs`）。
//!    本模块原来也有个 `cursor` 字段，但那是游标职责下沉前的残留——2026-11-03 实测
//!    生产代码零调用后删掉，连同两个只被它自己用的方法。）

// 这里曾有 `#![allow(dead_code)]`，注释写「还没接进 service::run_bot 的 bot 循环（S2c）」。
//
// **那个 allow 已经移除**（2026-11-03）：`mcp_channel_loop.rs` 现在真的在调
// `tick` / `deliver_batch` / `InboundState`，`service::run_bot` 也已调用
// `mcp_channel_loop::run`。摘掉后 clippy 只剩 `cursor` 相关的 3 处真死代码，已一并删除。
use std::collections::VecDeque;

use crate::mcp_bus::ChannelMessage;

/// 投递侧（真实接线是 S2c：把消息交给 bridge）。
///
/// `async_trait` 而不是裸 `async fn`：要能放进 `dyn`。
#[async_trait::async_trait]
pub trait InboundSink: Send + Sync {
    /// 投递一条消息。返回 `Err` 表示投递失败（驱动层会记下并继续，不丢消息）。
    async fn deliver(&self, msg: &ChannelMessage) -> Result<(), String>;
}

/// 去重 + 游标状态。**抽出来是因为它的正确性可以完全单测**，
/// 不需要真渠道也不需要真 bridge。
pub struct InboundState {
    /// 已见过的 `(chat_id, message_id)`，按到达顺序（满了从头丢）。
    seen: VecDeque<(String, String)>,
    seen_cap: usize,
    /// 累计投递失败条数（给状态行/日志看；不因失败而停）。
    pub failed: u64,
    /// 累计丢弃的重复条数。
    pub deduped: u64,
}

impl InboundState {
    pub fn new(seen_cap: usize) -> Self {
        Self {
            seen: VecDeque::new(),
            seen_cap: seen_cap.max(1),
            failed: 0,
            deduped: 0,
        }
    }

    /// 是否重复。**命中也会刷新它的位置**（LRU 语义）：
    /// 长会话里同一条被反复推送时，不该因为「很久以前见过」而被重新处理。
    pub fn is_duplicate(&mut self, msg: &ChannelMessage) -> bool {
        let key = (msg.chat_id.clone(), msg.message_id.clone());
        if let Some(pos) = self.seen.iter().position(|k| k == &key) {
            let k = self.seen.remove(pos).expect("刚找到的位置");
            self.seen.push_back(k);
            self.deduped += 1;
            return true;
        }
        self.seen.push_back(key);
        while self.seen.len() > self.seen_cap {
            self.seen.pop_front();
        }
        false
    }
}

/// 一轮的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// 成功投递条数。
    pub delivered: usize,
    /// 因重复被跳过条数。
    pub deduped: usize,
    /// 投递失败条数（**不会让循环停**）。
    pub failed: usize,
}

/// 处理一批消息：去重 → 投递 → 计数。
///
/// 单独抽出来是为了能直接单测「重复」「部分失败」这些组合，
/// 不需要起渠道、不需要真 sink。
pub async fn deliver_batch(
    state: &mut InboundState,
    sink: &dyn InboundSink,
    batch: &[ChannelMessage],
) -> TickReport {
    let mut r = TickReport::default();
    for msg in batch {
        if state.is_duplicate(msg) {
            r.deduped += 1;
            continue;
        }
        match sink.deliver(msg).await {
            Ok(()) => r.delivered += 1,
            Err(_) => {
                // **失败不中断**：一条坏消息不该让整个渠道停摆。
                // 也不重排队（那会让同一条永久堵在队首）——交给渠道下次推/下轮 poll。
                r.failed += 1;
                state.failed += 1;
            }
        }
    }
    r
}

/// 轮询间隔的两档：有消息时快、空闲时慢。
///
/// 为什么不一档：固定 1s 对空闲渠道是纯浪费（每渠道每秒一次 MCP 调用）；
/// 固定 30s 又让活跃会话明显迟滞。两档兼顾，且**不用自适应魔法数**——
/// 只要能区分「刚刚有活」和「一直没事」就够。
pub const POLL_ACTIVE: std::time::Duration = std::time::Duration::from_secs(2);
/// 空闲档。
pub const POLL_IDLE: std::time::Duration = std::time::Duration::from_secs(20);

/// 下一轮该等多久。
pub fn next_delay(last: &TickReport) -> std::time::Duration {
    if last.delivered > 0 {
        POLL_ACTIVE
    } else {
        POLL_IDLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn msg(chat: &str, id: &str) -> ChannelMessage {
        ChannelMessage {
            chat_id: chat.into(),
            message_id: id.into(),
            sender_id: String::new(),
            sender_name: String::new(),
            text: format!("{chat}/{id}"),
            ts: 0,
            attachments: vec![],
            parent_id: String::new(),
        }
    }

    /// 记录所有投递，可配置某条失败。
    struct FakeSink {
        got: Mutex<Vec<String>>,
        fail_on: Vec<String>,
    }

    impl FakeSink {
        fn new(fail_on: &[&str]) -> Self {
            Self {
                got: Mutex::new(Vec::new()),
                fail_on: fail_on.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl InboundSink for FakeSink {
        async fn deliver(&self, m: &ChannelMessage) -> Result<(), String> {
            if self.fail_on.contains(&m.message_id) {
                return Err("boom".into());
            }
            self.got.lock().unwrap().push(m.message_id.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn delivers_all_and_counts() {
        let mut st = InboundState::new(10);
        let sink = FakeSink::new(&[]);
        let r = deliver_batch(&mut st, &sink, &[msg("c", "1"), msg("c", "2")]).await;
        assert_eq!(r.delivered, 2);
        assert_eq!(r.failed, 0);
        assert_eq!(*sink.got.lock().unwrap(), vec!["1", "2"]);
    }

    /// **不同渠道的 message_id 可能撞车**，所以去重键必须带 chat_id。
    #[tokio::test]
    async fn dedupe_key_includes_chat_id() {
        let mut st = InboundState::new(10);
        let sink = FakeSink::new(&[]);
        let r = deliver_batch(&mut st, &sink, &[msg("a", "1"), msg("b", "1")]).await;
        assert_eq!(r.delivered, 2, "不同会话的同名 message_id 不该被当成重复");
        assert_eq!(r.deduped, 0);
    }

    #[tokio::test]
    async fn same_message_twice_is_delivered_once() {
        let mut st = InboundState::new(10);
        let sink = FakeSink::new(&[]);
        deliver_batch(&mut st, &sink, &[msg("c", "1")]).await;
        let r = deliver_batch(&mut st, &sink, &[msg("c", "1")]).await;
        assert_eq!(r.delivered, 0);
        assert_eq!(r.deduped, 1);
        assert_eq!(sink.got.lock().unwrap().len(), 1);
    }

    /// **一条失败不中断其余**：坏消息不该让整个渠道停摆。
    #[tokio::test]
    async fn one_failure_does_not_stop_the_batch() {
        let mut st = InboundState::new(10);
        let sink = FakeSink::new(&["2"]);
        let r = deliver_batch(
            &mut st,
            &sink,
            &[msg("c", "1"), msg("c", "2"), msg("c", "3")],
        )
        .await;
        assert_eq!(r.delivered, 2);
        assert_eq!(r.failed, 1);
        assert_eq!(*sink.got.lock().unwrap(), vec!["1", "3"]);
        assert_eq!(st.failed, 1, "累计失败数要记着（给状态行看）");
    }

    /// 去重缓存**有上限**且按 LRU 淘汰：长会话里不能无限涨。
    #[tokio::test]
    async fn seen_cache_is_bounded_and_lru() {
        let mut st = InboundState::new(2);
        let sink = FakeSink::new(&[]);
        deliver_batch(&mut st, &sink, &[msg("c", "1"), msg("c", "2")]).await;
        // 推到上限，1 被挤出
        deliver_batch(&mut st, &sink, &[msg("c", "3")]).await;
        // 1 又来了 ⇒ 因为已被淘汰，应重新投递
        let r = deliver_batch(&mut st, &sink, &[msg("c", "1")]).await;
        assert_eq!(r.delivered, 1, "被淘汰的消息应重新处理");
        assert_eq!(r.deduped, 0);
    }

    /// 命中重复时刷新位置（LRU）：刚见过的不会被立刻淘汰。
    #[tokio::test]
    async fn duplicate_refreshes_recency() {
        let mut st = InboundState::new(2);
        let sink = FakeSink::new(&[]);
        deliver_batch(&mut st, &sink, &[msg("c", "1"), msg("c", "2")]).await;
        // 再来 1（重复）⇒ 1 应被刷成最新，2 变成最旧
        deliver_batch(&mut st, &sink, &[msg("c", "1")]).await;
        // 加 3 ⇒ 淘汰的是 2（最旧），1 还在
        deliver_batch(&mut st, &sink, &[msg("c", "3")]).await;
        let r = deliver_batch(&mut st, &sink, &[msg("c", "1")]).await;
        assert_eq!(r.deduped, 1, "1 刚被刷新过，不该被淘汰：{r:?}");
    }

    #[test]
    fn delay_is_active_only_when_something_was_delivered() {
        let empty = TickReport::default();
        assert_eq!(next_delay(&empty), POLL_IDLE);
        let busy = TickReport {
            delivered: 3,
            ..Default::default()
        };
        assert_eq!(next_delay(&busy), POLL_ACTIVE);
        // 全部重复（没有任何新投递）⇒ 空闲档，不该因为「取到了但都是旧的」就狂轮询
        let dup = TickReport {
            deduped: 5,
            ..Default::default()
        };
        assert_eq!(next_delay(&dup), POLL_IDLE);
    }
}
