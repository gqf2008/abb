//! Bridge 子模块：待办恢复/其它平台入口（#80 按功能面拆分，impl Bridge 分散到子模块——
//! 子模块是父模块后代，可访问 mod.rs 私有字段，无需改可见性）。

use super::*;

/// #164 恢复冻结阈值：同一消息「异常退出后残留待恢复」累计次数达到此值 → 不再自动
/// 恢复（通知 owner 停止循环）。2 次的语义：第 1 次异常 → 给一次重试机会（正常崩溃
/// 恢复 #25 不回退）；重试仍异常 → 判定坏消息（如 agent 杀宿主）→ 冻结。
/// 与 watchdog 常量同款护栏测试兜底（见 tests::resume_max_attempts_is_sane）。
const RESUME_MAX_ATTEMPTS: u32 = 2;

impl Bridge {
    /// 补发路径的 Ev 重建（崩溃残留 PendingItem → Ev）：quoted/attachments 已随原轮
    /// 消费（原消息已被处理过），补发只重发文本，字段按 send_reply 所需最小集。
    /// 与重放路径直接消费 item 字段不同（那里要带全上下文）——两处差异在此显式化，
    /// 防止未来改一处漏一处（审查 R2）。
    pub(super) fn redeliver_ev(item: &PendingItem) -> Ev {
        Ev {
            mid: item.mid.clone(),
            chat_id: item.chat_id.clone(),
            chat_type: item.chat_type.clone(),
            chat_name: String::new(), // 补发最小集：PendingItem 未落群名
            thread_id: item.thread_id.clone(),
            quoted: crate::messenger::QuotedContent::default(),
            text: item.text.clone(),
            attachments: Vec::new(),
            role: item.role,
            sender_id: item.sender_id.clone(),
            ts: item.ts,
        }
    }

    /// 启动恢复（#25）：扫描 pending.json 残留（=上次崩溃/重启时未完成的消息），
    /// 自动重放进 handle 续跑——sessions.json 已 mark_started 的会话会 resume 原上下文，
    /// 未完成的重新执行；先清理孤儿 agent 子进程避免 resume 撞 already in use。
    /// 恢复是异步任务，不阻塞事件循环启动（多 chat 并发由 per-chat 串行锁保证不乱序）。
    /// 重启恢复重放。stop：关停广播后立即停止（未重放的条目留盘 pending.json，
    /// 下次启动续跑——#69 审查 Important：恢复任务会跑完整 agent 管线，单条可达
    /// 数分钟，不可让 shutdown_wait 无界等它）。
    pub async fn recover_pending(&self, stop: &tokio_util::sync::CancellationToken) {
        if self.pending.is_empty() {
            return;
        }
        crate::log!(
            "[bot:{}] 检测到 {} 条上次未完成的消息，自动恢复续跑（先清理孤儿 agent 进程）",
            self.bot.key(),
            self.pending.len()
        );
        // #164 恢复失败计数：残留 agent pid = 上次进程异常退出（被强杀/崩溃）→ 本轮
        // 待恢复消息全部 +1（优雅关停不会残留，不计）。计数在清理**之后**（残留信号
        // 先被 kill_stale_agents 消费再清空文件），重新快照拿更新后的计数。
        let had_stale = crate::agent::kill_stale_agents(&self.bot.key());
        if had_stale {
            for item in self.pending.snapshot() {
                self.pending.bump_resume_attempt(&item.mid);
            }
        }
        let items = self.pending.snapshot();
        for item in items {
            if stop.is_cancelled() {
                crate::log!(
                    "[bot:{}] 恢复重放被关停打断（剩余条目留盘，下次启动续跑）",
                    self.bot.key()
                );
                break;
            }
            // #51 审查跟进：/mention 是升级后新增的控制指令，实时路径在 pending.add 之前
            // 就被拦截，正常不会落盘；但升级前落盘的旧条目若文本恰为 /mention 系列，
            // 重放进 handle 会被当作开关指令静默执行——重放只续跑业务消息，控制指令跳过。
            // 停止指令（/cancel /stop 及自然停止词）同理：实时路径在 pending.add 之前
            // 就被拦截；旧条目文本恰为停止词时，重放进 handle 的取消分支会回复
            // 「当前没有正在运行的任务」并 return（不 remove）——条目永久留盘，每次
            // 重启都对同一 chat 重发一次该回复（审查修复：与 /mention 同例跳过）。
            if parse_mention_cmd(&item.text).is_some()
                || is_cancel_command(&item.text)
                || is_cancel_keyword(&item.text)
            {
                crate::log!(
                    "[bot:{}] 跳过 pending 重放中的控制指令（/mention/停止词为升级前残留）mid={}",
                    self.bot.key(),
                    trunc(&item.mid, 12)
                );
                self.pending.remove(&item.mid);
                continue;
            }
            // 阶段 1（W2 窗口修复）：回复已产出但未确认发出（崩溃在 set_reply 与
            // remove 之间）→ **直接补发，不重跑 agent**（原语义此窗口回复静默丢失）。
            // 补发成功才 remove；失败留盘，下次启动再试（不重跑）。
            // 审查跟进：补发持与实时 handle 同款 per-chat 串行锁（key 与 Ev::key 一致），
            // 消除发送交错（原实现无锁，补发与事件循环的发送可中途交织）；锁消除了
            // 交错但不保证先后——若事件循环先拿锁，新消息应答在前、陈旧补发在后
            //（残余倒挂属「补发无 TTL」接受项，不额外处理；flush_outbox 取锁先例）。
            if let Some(reply) = &item.reply {
                let ev = Self::redeliver_ev(&item);
                crate::log!(
                    "[bot:{}] 补发上次已产出的回复 chat={} mid={}",
                    self.bot.key(),
                    trunc(&ev.chat_id, 12),
                    trunc(&ev.mid, 12)
                );
                let lock = self.chat_lock(&ev.key());
                let _guard = lock.lock().await;
                match self.send_reply(&ev, reply).await {
                    Ok(()) => {
                        self.pending.remove(&item.mid);
                        // 与实时路径一致：补发成功后给原消息补 DONE 回执（崩溃窗口里
                        // handle 尾部的 del_typing/done 未执行——补发即补上）。
                        self.msgr.done(&ev.mid).await;
                        // #74 审查跟进：补发成功也要落 assistant 历史——崩溃发生在
                        // 「回复已产出、发送未确认」窗口，实时 handle 的发送成功分支
                        // 没跑到，不补的话这条回复在历史页永久缺失（消息+回复不完整）。
                        // 条件与 handle 的 record_granted 同款（granted 私聊）；
                        // UNIQUE(mid,direction) 幂等，重复补发/重放安全。时间用发送时刻。
                        if item.role == crate::config::SenderRole::Granted
                            && (item.chat_type == "p2p" || item.chat_type == "dm")
                        {
                            self.msgstore.insert(
                                &self.bot.key(),
                                &item.chat_id,
                                &item.mid,
                                "assistant",
                                &item.sender_id,
                                "", // assistant 行 GUI 显示 bot 名
                                reply,
                                crate::chrono_lite::unix_secs() as i64,
                                &item.chat_type,
                                "", // p2p/dm 私聊无群名
                            );
                        }
                    }
                    Err(e) => {
                        crate::log!("[bot:{}] 补发失败（留盘下次再试）: {e:#}", self.bot.key())
                    }
                }
                continue;
            }
            // #164 冻结：同一消息连续异常退出恢复达到上限 → 不再自动重跑 agent
            //（补发分支不重跑 agent、控制指令不触发 agent，均不冻结——只有这里
            // 会再次调 agent，是「agent 杀宿主 → 重启 → 恢复 → 再杀」死循环的入口）。
            if item.resume_attempts >= RESUME_MAX_ATTEMPTS {
                crate::log!(
                    "[bot:{}] 消息 {} 连续 {} 次恢复失败（上次处理疑似导致进程异常退出），冻结不再自动恢复",
                    self.bot.key(),
                    trunc(&item.mid, 12),
                    item.resume_attempts
                );
                self.pending.remove(&item.mid);
                // 通知 owner（主会话，必存在）：让用户知道消息没丢但停了，可重发。
                let primary = crate::config::Config::primary_chat(&self.bot.key());
                if !primary.is_empty() {
                    let notice = format!(
                        "⚠️ 消息「{}」连续 {} 次恢复失败（处理期间疑似导致进程异常退出），已停止自动恢复。如需继续请重新发送。",
                        crate::agent::truncate(&item.text, 40),
                        item.resume_attempts
                    );
                    let _ = self.msgr.send_text(&primary, &notice).await;
                    crate::log!(
                        "[bot:{}] 已通知 owner 冻结消息 {}（chat={}）",
                        self.bot.key(),
                        trunc(&item.mid, 12),
                        trunc(&primary, 12)
                    );
                }
                continue;
            }
            let ev = Ev {
                mid: item.mid,
                chat_id: item.chat_id,
                chat_type: item.chat_type,
                chat_name: String::new(), // PendingItem 未落群名（重放轮少一栏，可接受）
                thread_id: item.thread_id,
                quoted: item.quoted,
                text: item.text,
                attachments: item.attachments,
                role: item.role, // 重放按原角色走受限/全权限分支（PendingItem 落盘字段）
                sender_id: item.sender_id, // #74 重放保持原发送者标识
                ts: item.ts,     // #74 重放保持原事件时间
            };
            crate::log!(
                "[bot:{}] 恢复消息 chat={} mid={} text={:?}",
                self.bot.key(),
                trunc(&ev.chat_id, 12),
                trunc(&ev.mid, 12),
                crate::agent::truncate(&ev.text, 40)
            );
            let _ = self
                .send_reply(&ev, "🔄 正在恢复上次中断的消息，请稍候…")
                .await;
            self.handle(ev).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_max_attempts_is_sane() {
        // 护栏（同 watchdog 常量护栏风格，assert_eq 锁精确值避免 clippy 常量断言）：
        // 阈值 = 2 = 一次异常 + 一次重试机会（正常崩溃恢复 #25 不回退）；改阈值必须
        // 同步本测试，防止「冻结形同虚设」或「一次异常就丢消息」的漂移。
        assert_eq!(
            RESUME_MAX_ATTEMPTS, 2,
            "冻结阈值语义：1 次异常 + 1 次重试机会，变更需审慎并同步测试"
        );
    }
}
