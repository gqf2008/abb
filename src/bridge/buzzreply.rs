//! Bridge 子模块：#206 回复侧记账——buzz 后端回复的回流处理与启动对账
//!（#80 按功能面拆分，impl Bridge 分散到子模块——子模块是父模块后代，可访问
//! mod.rs 私有字段，无需改可见性）。
//!
//! 与 CLI 路径的对位：CLI 的回复在 handle 内的 per-chat 串行锁里「写历史 + 发送」
//! 原子完成；buzz 的回复经 mini-relay **异步**回流（dispatch 的串行锁早已释放），
//! 落在串行锁/代际锁之外——所以这里显式重建两道锁语义：
//! - **代际闸**：dispatch 在 history_lock 内快照 epoch 登记进账本（awaiting）；
//!   回复到达时在 history_lock 内比对 epoch——失配 = /new 已清历史，回复属已作废
//!   会话，只发不写（与 CLI same_session=false 同语义，风险④）。
//! - **发送串行化**：chat_lock(key) 内 send_text，与实时 handle 的发送不交错（话题
//!   消息 key=chat:thread 落同一 key，风险③——账本登记的是 dispatch 时的 key）。
//! - **去重/补发**：账本 claim（锁内原子）防实时回流与启动对账双发；发送失败
//!   unclaim 留待下次启动对账补发（at-least-once，与 pending.rs W2 同口径）。

use super::*;

impl Bridge {
    /// buzz 回复回流入口（service 的 mini-relay-replies 任务按 uuid→bot 路由调用）。
    /// 逐条 await（调用方循环不 spawn）——per-chat 发送顺序由 chat_lock + 逐条处理
    /// 共同保证（风险⑦）。
    pub async fn handle_buzz_reply(&self, reply: crate::buzzrelay::AgentReply) {
        self.deliver_buzz_reply(reply).await;
    }

    /// 启动对账：遍历账本 awaiting——relay db 里已有该轮的 agent 回复（e-tag 命中）
    /// 但账本无 sent 记录 = 崩溃窗口「已入库未发送/已发送未记账」→ 补发 + 补历史。
    /// 未命中的条目不动（留给 buzz-acp 重连水位回放重跑整轮：重放产新回复事件、
    /// e-tag 指向同一用户事件，实时回流再认领——风险②，与现状同为 at-least-once）。
    /// 加载时已把 in-flight 残留归一化为未发（见 replyledger），所以这里没有第三态。
    pub async fn recover_buzz_replies(&self, stop: &tokio_util::sync::CancellationToken) {
        let Some(relay) = &self.buzz_relay_state else {
            return; // buzz 未启用：无账本可对了（账本是空的，直接返回）
        };
        let awaiting = self.reply_ledger.awaiting_snapshot();
        if awaiting.is_empty() {
            return;
        }
        crate::log!(
            "[bot:{}] buzz 回复启动对账：{} 条 awaiting 登记待核对",
            self.bot.key(),
            awaiting.len()
        );
        for (user_event_id, _entry) in awaiting {
            if stop.is_cancelled() {
                crate::log!(
                    "[bot:{}] buzz 回复对账被关停打断（剩余条目留账，下次启动续对）",
                    self.bot.key()
                );
                break;
            }
            for reply in relay.find_agent_replies_to(&user_event_id).await {
                // claim 在 deliver 内部做：已 sent / 实时回流正在发 → Duplicate 跳过，
                // 绝不双发（风险⑤）。
                self.deliver_buzz_reply(reply).await;
            }
        }
    }

    /// 回流/对账共用的投递核：认领（去重）→ 写历史（代际闸）→ 发送（chat 串行锁）
    /// → 记账。顺序「先历史后发送」：崩溃窗口的恢复由账本兜底（未 sent 必补发，
    /// 历史 append 按 (回复事件 id, assistant) 幂等去重），严格优于「先发送后历史」
    /// 的「发了但没记账 → 历史缺轮」。
    async fn deliver_buzz_reply(&self, reply: crate::buzzrelay::AgentReply) {
        let claim = self
            .reply_ledger
            .claim(&reply.event_id, reply.in_reply_to.as_deref());
        let entry = match claim {
            crate::replyledger::Claim::Duplicate => {
                // 重连重放/对账与实时撞单：静默跳过是正确语义（首次投递已完成或
                // 正在进行），但留一行诊断日志便于核对「双发」投诉。
                crate::log!(
                    "[bridge] buzz 回复重复到达，按账本去重跳过 chat={} rev={:.12}",
                    trunc(&reply.chat_id, 12),
                    reply.event_id
                );
                return;
            }
            crate::replyledger::Claim::Fresh(entry) => entry,
        };
        // 关联归属：e-tag 命中 awaiting → 用 dispatch 登记的 key/epoch（话题消息
        // 落回 chat:thread 同一历史，风险③）；未命中（agent 未带 --reply-to / 登记
        // 被剪）→ key=chat_id 兜底 + 记日志（风险①：软关联是 best-effort，不阻塞发送）。
        let (key, epoch, via_fallback) = match &entry {
            Some(e) => (e.key.clone(), Some(e.epoch), false),
            None => {
                crate::log!(
                    "[bridge] ⚠️ buzz 回复无 e-tag 关联（agent 未带 --reply-to 或登记已剪枝），按 chat 兜底 chat={} rev={:.12}",
                    trunc(&reply.chat_id, 12),
                    reply.event_id
                );
                (reply.chat_id.clone(), None, true)
            }
        };
        // 发送串行化：与实时 handle 共用 per-chat 锁，补发/回流不与消息处理交错。
        let chat_lock = self.chat_lock(&key);
        let _serial = chat_lock.lock().await;
        // 写历史（代际闸）：epoch 失配 = 回复到达前 /new 已清历史——只发不写，
        // 与 CLI same_session=false 同语义（风险④）。兜底路径无 epoch 可比（登记
        // 缺失），锁内直写：与 /new 的 clear 互斥，写在 clear 前则被清、后则归新
        // 会话——best-effort，已在上方留日志。
        {
            let lock = self.history_lock(&key);
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let generation_ok = epoch.is_none_or(|e| *guard == e);
            if generation_ok {
                // mid = 回复事件 id：天然去重（重连重发同 id 不重记），且一轮多条
                // 回复各自落一条（(mid,user) 去重只认同 id，不吞同轮第二条）。
                crate::history::History::open(&self.bot.key(), &key).append_assistant(
                    &reply.event_id,
                    Backend::Buzz.name(),
                    &reply.content,
                );
            } else {
                crate::log!(
                    "[bridge] buzz 回复到达时会话已 /new（代际失配），跳过历史写入 chat={} rev={:.12}",
                    trunc(&key, 16),
                    reply.event_id
                );
            }
        }
        match self.msgr.send_text(&reply.chat_id, &reply.content).await {
            Ok(()) => {
                self.reply_ledger.mark_sent(&reply.event_id);
                // 回复时龄（事件 created_at → 投递）是回流链路健康度的直接读数：
                // 对账补发的条目时龄必然偏大，靠它区分实时投递与补发。
                let age = crate::chrono_lite::unix_secs().saturating_sub(reply.created_at);
                crate::log!(
                    "[bridge] buzz 回复已投递{} chat={} 长度={} 时龄={}s",
                    if via_fallback {
                        "（chat 兜底关联）"
                    } else {
                        ""
                    },
                    trunc(&reply.chat_id, 12),
                    reply.content.chars().count(),
                    age
                );
            }
            Err(e) => {
                // 发送失败：回滚认领（= 未发），下次启动对账补发（at-least-once）。
                self.reply_ledger.unclaim(&reply.event_id);
                crate::log!(
                    "[bridge] ⚠️ buzz 回复发送失败（留账待启动对账补发）chat={}: {e:#}",
                    trunc(&reply.chat_id, 12)
                );
            }
        }
    }
}
