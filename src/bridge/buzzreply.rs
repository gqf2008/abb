//! Bridge 子模块：#206 同步投递——buzz 后端回合回复（harness TurnOutput）的桥侧
//! 处理（#80 按功能面拆分，impl Bridge 分散到子模块——子模块是父模块后代，可访问
//! mod.rs 私有字段，无需改可见性）。
//!
//! 单进程架构（#200 Phase 3）下 buzz 回复不再异步回流：harness 在回合结束时捕获
//! agent 文本，经出站通道送回桥，由本模块**同步投递**（写历史 + 发送）。与 CLI
//! 路径的差异只在「回复何时产出」——CLI 的回复在 handle 的 per-chat 串行锁内
//! 「写历史 + 发送」原子完成；buzz 的回复在 dispatch 返回之后才到（回合在 agent
//! 侧跑），落在串行锁之外——所以这里显式重建两道锁语义：
//! - **代际闸**：dispatch 在 history_lock 内快照 epoch 登记进回合表（见
//!   [`super::Bridge::turn_registry`]）；回复到达时在 history_lock 内比对
//!   epoch——失配 = /new 已清历史，回复属已作废会话，只发不写（与 CLI
//!   same_session=false 同语义，风险④）。
//! - **发送串行化**：chat_lock(key) 内发送，与实时 handle 的发送不交错（话题
//!   消息 key=chat:thread 落同一 key，风险③——回合登记的是 dispatch 时的 key）。
//! - **去重**：账本删除后无持久去重面——同步形态下「一回合至多一条回复、且回合
//!   表登记与 TurnOutput 一一配对」，崩溃语义降级为 at-most-once（回合中途进程
//!   死 = 回复丢失，重启重放用户消息才会重跑；文档接受，见 PR 风险节）。

use super::*;

/// 回合登记（dispatch 时写入，TurnOutput 消费时摘除）。per-channel 单槽：频道
/// 串行 + 单 agent，同一频道同一时刻至多一个在跑回合，后到的 dispatch 必然发生
/// 在前一回合结束后（前者的 TurnOutput 已消费或随进程死丢失）。
#[derive(Debug, Clone)]
pub(crate) struct TurnEntry {
    /// 用户消息 mid（历史助手轮复用同一 mid，一消息一回复）。
    pub mid: String,
    /// 会话隔离 key（chat 或 chat:thread）——历史/串行锁必须与 dispatch 同 key，
    /// 否则用户轮与助手轮落进不同历史文件（风险③）。
    pub key: String,
    /// dispatch 时历史代际快照（history_lock 内取）。回复到达时代际已变 = /new
    /// 清过历史 → 只发不写（孤儿闸，风险④；与 CLI same_session=false 语义一致）。
    pub epoch: u64,
}

impl Bridge {
    /// buzz 回合投递入口（service 的 buzz-turn-consumer 任务按 TurnOutput 的
    /// meta.bot_key → Bridge 路由调用）。回合表登记（定位 key/epoch/mid）→
    /// 写历史（代际闸）→ 发送（chat 串行锁）。顺序「先历史后发送」沿用旧
    /// deliver_buzz_reply：发送失败只是用户侧可见缺失（同步形态无补发路径），
    /// 历史写入不得因发送失败而跳过。
    pub async fn deliver_turn_reply(&self, out: crate::buzz::harness::TurnOutput) {
        // TurnOutput 自带频道登记快照：None = 回合途中频道被移除（SyncRoots
        // diff）/登记异常——无法路由（chat_id/thread_id 都无从取），记日志丢弃。
        let Some(meta) = out.meta else {
            crate::log!(
                "[bridge] buzz 回合输出缺频道登记（频道已移除/登记异常），丢弃 uuid={} len={}",
                out.channel_id,
                out.text.chars().count()
            );
            return;
        };
        let channel_id = out.channel_id;
        let text = out.text;
        // 回合登记消费（与 dispatch 的登记一一配对；无登记 = 频道从未 dispatch/
        // 登记被 /new 前序消费，按 chat 兜底关联——与旧账本被剪的软关联降级同口径）。
        let entry = self.turn_registry.lock().unwrap().remove(&channel_id);
        let key = match &entry {
            Some(e) => e.key.clone(),
            None => {
                let derived = if meta.thread_id.is_none() {
                    meta.chat_id.clone()
                } else {
                    format!(
                        "{}:{}",
                        meta.chat_id,
                        meta.thread_id.as_deref().unwrap_or_default()
                    )
                };
                crate::log!(
                    "[bridge] ⚠️ buzz 回合输出无 dispatch 登记（未登记频道/登记缺失？），按会话键兜底 key={} uuid={}",
                    crate::agent::truncate(&derived, 24),
                    channel_id
                );
                derived
            }
        };
        // 发送串行化：与实时 handle 共用 per-chat 锁，回复不与消息处理交错。
        let chat_lock = self.chat_lock(&key);
        let _serial = chat_lock.lock().await;
        // 写历史（代际闸）：epoch 失配 = 回复到达前 /new 已清历史——只发不写，
        // 与 CLI same_session=false 同语义（风险④）。兜底路径无 epoch 可比
        //（登记缺失），锁内直写：与 /new 的 clear 互斥，写在 clear 前则被清、
        // 后则归新会话——best-effort，已在上方留日志。
        let mid = entry.as_ref().map(|e| e.mid.as_str()).unwrap_or("");
        {
            let lock = self.history_lock(&key);
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let generation_ok = entry.as_ref().is_none_or(|e| *guard == e.epoch);
            if generation_ok {
                // mid = 用户消息 mid（一消息一回复；与 CLI 成功路径同口径）。
                // (mid,user) 去重只认用户轮，助手条目无重复风险（同步形态下每次
                // dispatch 至多一条 TurnOutput）。
                crate::history::History::open(&self.bot.key(), &key).append_assistant(
                    mid,
                    Backend::Buzz.name(),
                    &text,
                );
            } else {
                crate::log!(
                    "[bridge] buzz 回合回复到达时会话已 /new（代际失配），跳过历史写入 chat={} uuid={}",
                    crate::agent::truncate(&key, 16),
                    channel_id
                );
            }
        }
        // 发送：#206 话题隔离——回复来源是话题频道 → send_thread_reply 落回原话题
        //（飞书 reply_in_thread；钉钉/微信无话题概念，平台实现回落普通发送）。
        // 锚点是频道登记表里的最近话题用户 mid（dispatch 时更新）——同一话题的
        // 迟到回复可能锚到更新的消息，飞书侧仍落同一话题（可接受的近似）。
        // 话题频道但锚点为空 = 异常态（upsert 必设）——如实回落 send_text + 日志。
        // 审查 #214 P1-1：send_thread_reply 失败（锚点消息被删/撤回 → 飞书 reply
        // 永久报错）必须回落 send_text——回复已写历史，无回落则永远投递不出。
        // 两级都败才留日志（同步形态下无账本可重试，如实告知）。
        let send_result = if meta.thread_id.is_none() {
            self.msgr.send_text(&meta.chat_id, &text).await
        } else if meta.anchor_mid.is_none() {
            crate::log!(
                "[bridge] ⚠️ 话题回复缺锚点 mid（话题频道登记异常？），回落普通发送 chat={} uuid={}",
                crate::agent::truncate(&meta.chat_id, 12),
                channel_id
            );
            self.msgr.send_text(&meta.chat_id, &text).await
        } else {
            match self
                .msgr
                .send_thread_reply(
                    &meta.chat_id,
                    meta.anchor_mid.as_deref().unwrap_or_default(),
                    &text,
                )
                .await
            {
                Ok(()) => Ok(()),
                Err(e) => {
                    crate::log!(
                        "[bridge] ⚠️ 话题回复发送失败（锚点消息可能已删/撤回），回落普通发送 chat={} uuid={}: {e:#}",
                        crate::agent::truncate(&meta.chat_id, 12),
                        channel_id
                    );
                    self.msgr.send_text(&meta.chat_id, &text).await
                }
            }
        };
        match send_result {
            Ok(()) => {
                crate::log!(
                    "[bridge] buzz 回复已投递{} chat={} 长度={}",
                    if entry.is_none() {
                        "（无 dispatch 登记兜底关联）"
                    } else {
                        ""
                    },
                    crate::agent::truncate(&meta.chat_id, 12),
                    text.chars().count()
                );
            }
            Err(e) => {
                // 发送失败：同步形态无账本补发路径（at-most-once）——如实留痕，
                // 用户在场可重发。
                crate::log!(
                    "[bridge] ⚠️ buzz 回复发送失败（两级都败，无补发路径）chat={}: {e:#}",
                    crate::agent::truncate(&meta.chat_id, 12)
                );
            }
        }
    }
}
