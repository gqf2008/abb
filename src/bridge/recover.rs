//! Bridge 子模块：待办恢复/其它平台入口（#80 按功能面拆分，impl Bridge 分散到子模块——
//! 子模块是父模块后代，可访问 mod.rs 私有字段，无需改可见性）。

use super::*;

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
        let items = self.pending.snapshot();
        crate::log!(
            "[bot:{}] 检测到 {} 条上次未完成的消息，自动恢复续跑（先清理孤儿 agent 进程）",
            self.bot.key(),
            self.pending.len()
        );
        crate::agent::kill_stale_agents(&self.bot.key());
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
                            );
                        }
                    }
                    Err(e) => {
                        crate::log!("[bot:{}] 补发失败（留盘下次再试）: {e:#}", self.bot.key())
                    }
                }
                continue;
            }
            let ev = Ev {
                mid: item.mid,
                chat_id: item.chat_id,
                chat_type: item.chat_type,
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

    /// 微信入站消息入口（service 的微信长轮询循环调用）。
    /// msg=入站微信消息；先记 context_token，过滤后走统一 handle。
    pub async fn on_weixin(&self, msg: crate::wechat::WeixinMessage) {
        // 只处理用户消息（message_type==1 是 USER；2 是 BOT 自己）
        if msg.message_type != 1 {
            return;
        }
        let from = msg.from_user_id.trim().to_string();
        if from.is_empty() {
            return;
        }
        // #118 fail-closed：微信 owner 判据是登录拿到的 ilink_user_id（不是飞书 open_id）。
        // wx_user_id 未配置（owner 为空）→ 拒绝所有人（此前是放行旁路）。
        let owner = self.bot.wx_owner();
        if owner.is_empty() || from != owner {
            crate::log!("[weixin] 忽略未授权消息 from={}", trunc(from, 10));
            return;
        }
        // 回复必须回显该用户最新 context_token
        self.msgr.note_context(&from, &msg.context_token);
        // token 已刷新 → 顺带补发该会话积压的任务报告（主动推送曾被微信拒绝的，不静默丢失）
        self.flush_outbox(&from).await;
        let text = msg.text().trim().to_string();
        // 微信 message_id 可能为空；用 session_id+时间戳凑一个去重键
        let mid = if msg.message_id.is_empty() {
            format!("{}:{}", msg.session_id, msg.create_time_ms)
        } else {
            msg.message_id.clone()
        };
        // #12：图片/语音/文件/视频 → 下载保存（CDN AES 解密），纯附件消息 text 空也能进 agent
        let mut attachments = Vec::new();
        for (i, media) in msg.media_items().iter().enumerate() {
            let desc = crate::attachments::AttachmentDesc::Wechat(media.clone());
            if let Some(meta) = self
                .msgr
                .download_attachment(&self.bot.key(), &mid, i, &desc)
                .await
            {
                attachments.push(meta);
            }
        }
        if text.is_empty() && attachments.is_empty() {
            crate::log!("[weixin] 丢弃：text 为空且无附件");
            return;
        }
        // 引用/回复：ref_msg 里的被引用文本 + 媒体（图片/文件/音视频）下载成附件元数据。
        let mut quoted = crate::messenger::QuotedContent {
            text: msg.quoted_text(),
            attachments: Vec::new(),
        };
        for (i, media) in msg.quoted_media().into_iter().enumerate() {
            let desc = crate::attachments::AttachmentDesc::Wechat(media);
            if let Some(meta) = self
                .msgr
                .download_attachment(&self.bot.key(), &mid, 100 + i, &desc)
                .await
            {
                quoted.attachments.push(meta);
            }
        }
        let ev = Ev {
            mid,
            chat_id: from.clone(),       // 微信会话标识 = 对方 ilink_user_id
            chat_type: "dm".to_string(), // 微信私聊当 dm（主会话候选）
            thread_id: String::new(),    // 微信无话题
            quoted,
            text,
            attachments,
            role: crate::config::SenderRole::Owner, // 微信只有 owner（on_weixin 已按 wx_user_id 过滤）
            sender_id: from,
            // #74 事件时间：create_time_ms 是毫秒；缺失/为 0 回落当前时间
            ts: if msg.create_time_ms > 0 {
                msg.create_time_ms / 1000
            } else {
                crate::chrono_lite::unix_secs() as i64
            },
        };
        self.handle(ev).await;
    }

    /// 钉钉入站消息入口（service 的钉钉 Stream 循环调用）。
    /// msg=解析好的机器人消息；先记群聊最近发送者（回复时 @），过滤后走统一 handle。
    pub async fn on_dingtalk(&self, msg: crate::dingtalk::DingtalkMessage) {
        // 访问控制（与飞书同套，staffId 标识）：公开开关开 → 放行所有人；否则只放行 owner ∪
        // 授权者白名单。每次热读 config（授权/取消/改开关即时生效）；config 读不到（单测）回落快照。
        // 同一份热读顺路推导发送者角色（owner=全权限 / granted=受限），随 Ev 传给 agent。
        // #118：无公开开关、无群聊豁免——群里任何人未经授权也拦截。
        let (allowed, sender_role, mention_map, mention_default) =
            self.access_and_role(&msg.sender_staff_id);
        if !allowed {
            // 未授权用户可能在发授权码：仅单聊（chat_id=staffId，非 cid 开头）接受，群里发码防抢注
            let chat_id = msg.chat_id().to_string();
            let is_p2p = !chat_id.starts_with("cid");
            if self
                .try_consume_owner_code(&msg.sender_staff_id, &chat_id, is_p2p, &msg.text)
                .await
            {
                return;
            }
            // #118：未授权一律拦截且**无提示文案**；记录历史——单聊保留 #74 提醒+落历史；
            // 群聊落历史、不提醒。钉钉事件无时间字段 → 当前秒；授权码消费成功（上面 return）的不提醒
            if !msg.mid.is_empty() {
                // 展示名：未授权用户不在本地名单，API 反查（best-effort）
                let uname = self
                    .msgr
                    .user_display_name(&msg.sender_staff_id)
                    .await
                    .unwrap_or_default();
                self.msgstore.insert(
                    &self.bot.key(),
                    &chat_id,
                    &msg.mid,
                    "user",
                    &msg.sender_staff_id,
                    &uname,
                    &msg.text,
                    crate::chrono_lite::unix_secs() as i64,
                );
                if is_p2p {
                    self.unread.report(
                        &self.bot.key(),
                        &msg.sender_staff_id,
                        &uname,
                        &crate::agent::truncate(&msg.text, 40),
                        crate::chrono_lite::unix_secs() as i64,
                    );
                }
            }
            crate::log!(
                "[dingtalk] 忽略未授权消息 from={}",
                trunc(&msg.sender_staff_id, 10)
            );
            return;
        }
        // #118：granted + pi 后端 + 隔离开 → 接入层静默拦截（落历史不回复，不暴露配置）
        let backend = self.bot.effective_backend(&self.default_backend);
        if crate::config::granted_pi_unusable(sender_role, &self.bot.key(), backend) {
            crate::log!(
                "[dingtalk] granted+pi 会话静默拦截 from={}",
                trunc(&msg.sender_staff_id, 10)
            );
            if !msg.mid.is_empty() {
                let uname = self
                    .msgr
                    .user_display_name(&msg.sender_staff_id)
                    .await
                    .unwrap_or_default();
                self.msgstore.insert(
                    &self.bot.key(),
                    &msg.chat_id(),
                    &msg.mid,
                    "user",
                    &msg.sender_staff_id,
                    &uname,
                    &msg.text,
                    crate::chrono_lite::unix_secs() as i64,
                );
                if !msg.is_group() {
                    self.unread.report(
                        &self.bot.key(),
                        &msg.sender_staff_id,
                        &uname,
                        &crate::agent::truncate(&msg.text, 40),
                        crate::chrono_lite::unix_secs() as i64,
                    );
                }
            }
            return;
        }
        // 群聊只有 @ 了本机器人（或配置了「@ 才推送」）的消息才处理；单聊直接处理。
        // #51：该群设了免 @（mention_modes off）则无需 @ 也进 agent（与飞书同开关）。
        // 门槛判定复用 access_and_role 同一次 config load；已 @ 则短路不付门槛判定。
        if msg.is_group()
            && !msg.mentioned
            && !self.mention_off(&mention_map, &msg.chat_id(), mention_default)
        {
            crate::log!(
                "[dingtalk] 忽略群聊未 @ 机器人的消息 chat={}",
                trunc(msg.chat_id(), 10)
            );
            return;
        }
        let chat_id = msg.chat_id();
        if chat_id.is_empty() || msg.mid.is_empty() {
            return;
        }
        // 虚拟 Bot #75：事件群名（conversationTitle）入缓存——只对群聊；
        // 单聊事件无该字段，note 内部过滤空名
        if msg.is_group() {
            self.chat_info_cache
                .note_event_name(&chat_id, &msg.conversation_title);
        }
        // 群聊回复需要 @ 提问者 → 记最近 sender（单聊 chat_id==sender，无意义但无害）
        self.msgr.note_sender(&chat_id, &msg.sender_staff_id);

        // #12：图片/文件/语音/视频（含富文本里的图）→ 下载保存；纯附件消息 text 空也能进 agent
        let mut attachments = Vec::new();
        for (i, a) in msg.attachments.iter().enumerate() {
            let desc = crate::attachments::AttachmentDesc::Dingtalk {
                download_code: a.download_code.clone(),
                robot_code: msg.robot_code.clone(),
                kind: a.kind.clone(),
                file_name: a.file_name.clone(),
                voice_text: a.voice_text.clone(),
            };
            if let Some(meta) = self
                .msgr
                .download_attachment(&self.bot.key(), &msg.mid, i, &desc)
                .await
            {
                attachments.push(meta);
            }
        }

        // 剥群聊文本里的 "@机器人名" 前缀（钉钉推给机器人的内容会带上），只剥一次
        let is_group = msg.is_group();
        let mut text = msg.text;
        if is_group {
            text = strip_bot_mention(&text, &self.bot.bot_name);
        }
        // 引用/回复：repliedMsg 里的被引用文本 + 附件（图片/文件/音视频）下载成附件元数据。
        let mut quoted = crate::messenger::QuotedContent {
            text: msg.quoted_text,
            attachments: Vec::new(),
        };
        for (i, a) in msg.quoted_attachments.iter().enumerate() {
            let desc = crate::attachments::AttachmentDesc::Dingtalk {
                download_code: a.download_code.clone(),
                robot_code: msg.robot_code.clone(),
                kind: a.kind.clone(),
                file_name: a.file_name.clone(),
                voice_text: a.voice_text.clone(),
            };
            if let Some(meta) = self
                .msgr
                .download_attachment(&self.bot.key(), &msg.mid, 100 + i, &desc)
                .await
            {
                quoted.attachments.push(meta);
            }
        }
        let ev = Ev {
            mid: msg.mid,
            chat_id,
            chat_type: if is_group {
                "group".to_string()
            } else {
                "dm".to_string()
            },
            thread_id: String::new(), // 钉钉无话题
            quoted,
            text,
            attachments,
            role: sender_role,
            sender_id: msg.sender_staff_id,
            // #74 事件时间：钉钉 Stream 事件体无时间字段 → 当前 unix 秒
            // （历史排序/提醒时间显示够用）。
            ts: crate::chrono_lite::unix_secs() as i64,
        };
        self.handle(ev).await;
    }
}
