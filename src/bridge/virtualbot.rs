//! Bridge 子模块：虚拟 Bot 处理（#80 按功能面拆分，impl Bridge 分散到子模块——
//! 子模块是父模块后代，可访问 mod.rs 私有字段，无需改可见性）。

use super::*;

impl Bridge {
    /// 虚拟 Bot 注入数据（#75）：仅登记过的群聊返回 (群名, 群介绍)。
    /// 判定条件：chat_type=group + chat_id 在登记表（快照 mtime 懒刷新）。
    pub(super) async fn virtual_role_for(&self, ev: &Ev) -> Option<(String, String)> {
        if ev.chat_type != "group" {
            return None;
        }
        self.refresh_virtual_bots();
        let registered = {
            let bots = self.virtual_bots.lock().unwrap();
            bots.iter()
                .any(|v| v.bot_key == self.bot.key() && v.chat_id == ev.chat_id)
        };
        if !registered {
            return None;
        }
        // 取舍留痕（审查跟进）：cache.get 在缓存过期时会在**per-chat 串行锁内**发起
        // 异步网络拉群资料（仅登记群、每 5 分钟至多一次、reqwest 30s 超时）——最坏
        // 阻塞同 chat 消息队列 30s。可接受：频率极低 + best-effort（失败只 log），
        // 且把预取挪到锁外会引入「锁外异步态」的复杂度，收益不抵（不重构）。
        self.chat_info_cache
            .get(&ev.chat_id, self.msgr.as_ref())
            .await
    }

    /// 群被解散事件（im.chat.deleted_v1）：虚拟 Bot 登记自动移除——平台侧解散后 ABB
    /// 不残留幽灵登记（deliver @角色名不再指向死群、GUI 列表不再显示无效项）。
    /// 事件体 `{"chat_id": "oc_…"}`。写登记表与 GUI 并发的 last-writer-wins 取舍
    /// 见 virtualbot.rs 模块注释（低频人工操作 + 事件驱动，原子重写读侧永远完整）。
    pub(super) async fn on_chat_deleted(&self, event: &serde_json::Value) {
        let chat_id = event["chat_id"].as_str().unwrap_or("");
        if chat_id.is_empty() {
            return;
        }
        if self.vb_store.remove(&self.bot.key(), chat_id) {
            crate::log!(
                "[bridge] 群被解散（im.chat.deleted_v1），已自动移除虚拟 Bot 登记 chat={}",
                trunc(chat_id, 12)
            );
            // 会话历史归档（用户决策：解散后不删除，移入工作区 archive/）
            crate::virtualbot::VirtualBotStore::archive_chat_history(&self.bot.key(), chat_id);
        } else {
            crate::log!(
                "[bridge] 群被解散 chat={}（非本 bot 的虚拟 Bot 登记，忽略）",
                trunc(chat_id, 12)
            );
        }
    }

    /// 登记快照懒刷新：文件 (mtime, 长度) 变了才重读（GUI 登记/取消登记后下一条消息
    /// 即生效；文件极小，未变时只付一次 stat 成本）。长度进签名：防同 mtime 粒度内
    /// 两次连续写入（文件系统时间戳 tick 相同）漏刷新。
    pub(super) fn refresh_virtual_bots(&self) {
        use std::time::SystemTime;
        let sig = std::fs::metadata(crate::bridge_dir().join("virtual-bots.json"))
            .ok()
            .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        let mut cached = self.virtual_bots_mtime.lock().unwrap();
        if *cached != sig {
            *self.virtual_bots.lock().unwrap() = crate::virtualbot::VirtualBotStore::new().load();
            *cached = sig;
        }
    }

    pub(crate) async fn handle(&self, ev: Ev) {
        let t0 = std::time::Instant::now();
        crate::log!(
            "[bridge] 收到消息 bot={} chat={} mid={} text={:?}",
            self.bot.key(),
            trunc(&ev.chat_id, 12),
            trunc(&ev.mid, 12),
            crate::agent::truncate(&ev.text, 40)
        );
        // mid 去重
        {
            let mut seen = self.seen.lock().unwrap();
            if seen.contains(&ev.mid) {
                crate::log!("[bridge] 重复消息跳过（mid={}）", trunc(&ev.mid, 12));
                return;
            }
            seen.insert(ev.mid.clone());
            if seen.len() > 5000 {
                let keep: Vec<String> = seen.iter().skip(2500).cloned().collect();
                *seen = keep.into_iter().collect();
            }
        }

        // 剥群聊 @_user_N 提及标签
        let text = strip_mentions(&ev.text).trim().to_string();
        // #12：纯附件消息（text 空但 attachments 非空）也进 agent，不丢
        if text.is_empty() && ev.attachments.is_empty() {
            crate::log!("[bridge] chat {} 跳过空消息", trunc(&ev.chat_id, 10));
            return;
        }

        // 会话隔离 key：话题消息 = {chat_id}:{thread_id}，非话题 = chat_id（#14）。
        // 打断/串行/会话/发送全部按 key 走——同一群不同话题互不串线。
        let key = ev.key();

        // #87 暂停拦截：会话被 pause 后，新消息仍落消息库（可查）但不触发 agent、
        // 不回复；话题消息回落 chat 前缀判定（暂停整个群 = 群内所有话题一并静音）。
        // 暂停期消息不进 history——恢复后 agent 上下文不被暂停期内容污染，也不补发/重放。
        if self.session_state.is_paused(&self.bot.key(), &key)
            || self.session_state.is_paused(&self.bot.key(), &ev.chat_id)
        {
            self.msgstore.insert(
                &self.bot.key(),
                &ev.chat_id,
                &ev.mid,
                "user",
                &ev.sender_id,
                "",
                &text,
                ev.ts,
            );
            crate::log!(
                "[bridge] 会话已暂停（#87），消息入库不回复 bot={} chat={} mid={}",
                self.bot.key(),
                trunc(&key, 16),
                trunc(&ev.mid, 12)
            );
            return;
        }

        // 打断拦截：停止词 → 叫停该 chat 正在跑的任务。必须在拿串行锁**之前**判断，
        // 否则会被排到运行中任务之后，等任务跑完才处理（那时打断就没意义了）。
        // 显式命令 /cancel /stop：有任务 → 打断；无任务 → 明确回复（不透传给 agent，避免
        // 被当普通问题回答）。自然停止词（停/停止/取消/stop/cancel）→ 有任务打断、无任务透传
        // （对话语境下不该硬拦，例如「别取消，先继续」）。
        if is_cancel_command(&text) {
            if let Some(flag) = self.cancel_flags.lock().unwrap().get(&key).cloned() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                crate::log!("[bridge] 收到停止指令 chat={}", trunc(&key, 16));
                // 「⏹ 已停止」由被叫停的任务自己发（它确认真停了才发）；这里不回话避免重复。
                return;
            }
            // #124 团队创建流程进行中：/cancel 也中止（WaitingGoal/WaitingConfirm 通用），
            // 避免出现「/cancel 却说没有任务在跑」的割裂。
            if self.team_flows.get(&key).is_some() {
                self.team_flows.remove(&key);
                if let Err(e) = self
                    .send_reply(&ev, "已取消团队创建，随时可以再发起。")
                    .await
                {
                    crate::log!("[bridge] /cancel 团队中止确认发送失败: {e:#}");
                }
                return;
            }
            // 无在跑任务 → 命令化反馈，不喂给 agent
            if let Err(e) = self.send_reply(&ev, "✅ 当前没有正在运行的任务。").await {
                crate::log!("[bridge] /cancel 确认发送失败: {e:#}");
            }
            return;
        }
        if is_cancel_keyword(&text) {
            if let Some(flag) = self.cancel_flags.lock().unwrap().get(&key).cloned() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                crate::log!("[bridge] 收到停止指令 chat={}", trunc(&key, 16));
                // 「⏹ 已停止」由被叫停的任务自己发（它确认真停了才发）；这里不回话避免重复。
                return;
            }
            // 无在跑任务 → 停止词当普通消息透传给 agent
        }

        // 记录本 bot 主会话（私聊）：定时任务会话失效时的回落目标 + job CLI 缺省回发处
        // 飞书私聊 chat_type="p2p"；微信私聊用 "dm"。放在 /new 分支之前——新用户首条
        // 消息就是 /new 时主会话也要落盘（审查 Minor）。
        if ev.chat_type == "p2p" || ev.chat_type == "dm" {
            crate::config::Config::save_primary_chat(&self.bot.key(), &ev.chat_id);
        }

        // /new 会话新建（#23）：拦截在透传 agent 之前、拿串行锁之前（不被运行中任务阻塞）。
        // reset 按会话隔离 key（话题=chat:thread，#14）执行，只影响目标会话。
        // 运行中并发由 mark_started_if 兜底：旧任务完成时若槽位已被换走（/new 或 CLI reset），
        // 不会把新槽位 mark 回 started=true（审查修复——替代原 pending_new 标记，后者
        // 覆盖不了 CLI 跨进程 reset，且存在 insert 晚于 reset 的 TOCTOU）。
        if is_new_command(&text) {
            // #49：/new = 用户明确要求全新会话 → 连对话历史与迁移标记一起清
            // （切换注入的历史随之失效，不会泄进新会话）。代际自增使交错窗口内
            // 串行锁里的旧写盘全部失效（审查 I-2：clear 与锁内写无锁互斥的 TOCTOU）。
            // 顺序：先清历史再换会话——clear 失败则中止重置（否则旧历史泄进新会话，
            // 审查 I-2 读侧）；崩溃窗口从「新会话读到旧历史」变成「reset 未生效」
            //（用户可见的失败，无静默泄漏）。
            if self.history_reset(&key) {
                // #56/#57：/new 后旧 sid 的 pi 会话文件永久失效（pi 按 sid 续聊，新 sid
                // 不再触碰旧文件）——顺手清掉：.pi-sessions 是 #56 探针的唯一信号源，
                // 残留文件只增不减会拖慢每轮探针扫描并堆积磁盘。CLI `session reset`
                // 同样只轮换 sid，但不走本分支（旧文件由探针按 mtime 忽略）。
                let old_sid = self.sessions.ensure_with_started(&key).0;
                let new_sid = self.sessions.reset_session(&key);
                let ws = crate::workspace_dir(&self.bot.key());
                if Backend::parse(self.bot.effective_backend(&self.default_backend)) == Backend::Pi
                {
                    // #56/#57：/new 后旧 sid 的 pi 会话文件永久失效（pi 按 sid 续聊，新 sid
                    // 不再触碰旧文件）——顺手清掉：.pi-sessions 是 #56 探针的唯一信号源，
                    // 残留文件只增不减会拖慢每轮探针扫描并堆积磁盘。CLI `session reset`
                    // 同样只轮换 sid，但不走本分支（旧文件由探针按 mtime 忽略）。
                    // 无 mtime 护栏（fresh_secs=None）：被轮换的旧 sid 不可能再被使用。
                    let sids = std::collections::HashSet::from([old_sid.clone()]);
                    crate::agent::remove_pi_transcripts(
                        &ws,
                        &sids,
                        crate::agent::SidMatch::InSet,
                        None,
                    );
                }
                crate::log!(
                    "[bridge] /new 新建会话 bot={} key={} sid={}",
                    self.bot.key(),
                    trunc(&key, 16),
                    trunc(&new_sid, 8)
                );
                if let Err(e) = self
                    .send_reply(&ev, "✅ 已新建会话，下一条消息开始全新上下文。")
                    .await
                {
                    crate::log!("[bridge] /new 确认发送失败: {e:#}");
                }
            } else {
                crate::log!(
                    "[bridge] ⚠️ /new 历史清理失败，会话未重置 bot={} key={}",
                    self.bot.key(),
                    trunc(&key, 16)
                );
                let _ = self
                    .send_reply(&ev, "⚠️ 新建会话失败：历史清理未完成，请稍后重试。")
                    .await;
            }
            return;
        }

        // /mention 免 @ 群聊开关（#51）：位置在 /new 之后、GitHub 指令之前——与 /new 同为
        // 即时控制指令，不进 agent、不落盘 pending。仅顶层群聊可切换（私聊无 @ 门槛，
        // 话题内本就免 @——不落盘、只提示）；配置写入 config.json（热读即时生效，
        // 重启保持）。飞书/钉钉群聊共用（钉钉 Ev 的 chat_type 同为 "group"）。
        if let Some(cmd) = parse_mention_cmd(&text) {
            let reply = if ev.chat_type == "group" && ev.thread_id.is_empty() {
                // 开关是管理动作（用户拍板 2026-08-15）：仅 owner 可切换。私有模式下
                // 授权者也能到 handle 但收到拒绝；open_access 模式下陌生人 @ 到机器人
                // 同样被拒——@ 门槛是公开群唯一的防洪闸，不能让陌生人关掉。
                // Show（只看状态）对能到 handle 的人开放。
                let switching = matches!(cmd, MentionCmd::On | MentionCmd::Off);
                if switching && ev.role != crate::config::SenderRole::Owner {
                    "⚠️ 免 @ 开关仅管理员（owner）可切换。".to_string()
                } else {
                    match cmd {
                        MentionCmd::Show => {
                            if self.mention_mode(&key).as_deref() == Some("off") {
                                MENTION_OFF_MSG.to_string()
                            } else {
                                "本群需要 @ 本机器人 才会响应（默认）。/mention off 可开启免 @。"
                                    .to_string()
                            }
                        }
                        MentionCmd::On => {
                            // 恢复默认 = 删除条目（"on" 值与缺省语义等价，不落盘死条目）
                            if self.set_mention_mode(&key, None) {
                                "已恢复：需要 @ 本机器人 才会响应。".to_string()
                            } else {
                                MENTION_SAVE_FAIL_MSG.to_string()
                            }
                        }
                        MentionCmd::Off => {
                            if self.set_mention_mode(&key, Some("off")) {
                                MENTION_OFF_MSG.to_string()
                            } else {
                                MENTION_SAVE_FAIL_MSG.to_string()
                            }
                        }
                    }
                }
            } else {
                "⚠️ 免 @ 开关仅顶层群聊可用（私聊与话题内本就无需 @，本开关只影响顶层群消息）。"
                    .to_string()
            };
            if let Err(e) = self.send_reply(&ev, &reply).await {
                crate::log!("[bridge] /mention 确认发送失败: {e:#}");
            }
            return;
        }

        // /trash 删除保护指令（#88）：管理回收站（list/restore/purge）与确认危险删除
        //（confirm）。即时控制指令，不进 agent、不落盘 pending。仅 owner 可用（删除是
        // 管理动作，与 /mention 开关同口径）。三渠道共用（on_payload/on_weixin/on_dingtalk
        // 都汇入 handle）。
        if let Some(tc) = parse_trash_cmd(&text) {
            let reply = if ev.role != crate::config::SenderRole::Owner {
                "⚠️ 回收站管理仅管理员（owner）可用。".to_string()
            } else {
                self.trash_reply(&ev, tc).await
            };
            if let Err(e) = self.send_reply(&ev, &reply).await {
                crate::log!("[bridge] /trash 确认发送失败: {e:#}");
            }
            return;
        }

        // #124 一键创建团队·聊天入口（P1 后端）：触发词 → 方案预览 → 确认/修改/取消 → 建群。
        // 拦截在 pending 落盘之前（命中即短路返回，不落盘、不进 agent）；未命中原样透传。
        // 仅 owner 可触发（建群是管理动作）；内部持 per-chat 串行锁与 agent 路径互斥。
        if let Some(reply) = self.team_chat_reply(&ev, &text).await {
            if let Err(e) = self.send_reply(&ev, &reply).await {
                crate::log!("[bridge] 团队流程回复发送失败: {e:#}");
            }
            return;
        }

        // #25 重启恢复：进入 agent 处理前落盘 pending（已排除 /new、停止词等控制指令），
        // service 崩溃/重启后由 recover_pending 自动重放续跑。重放时同 mid 再次 add
        // 会按 mid 去重，不会产生重复条目。
        self.pending.add(PendingItem {
            mid: ev.mid.clone(),
            chat_id: ev.chat_id.clone(),
            chat_type: ev.chat_type.clone(),
            thread_id: ev.thread_id.clone(),
            text: text.clone(),
            quoted: ev.quoted.clone(),
            attachments: ev.attachments.clone(),
            role: ev.role, // 落盘角色：重启重放时按原角色走受限/全权限分支
            sender_id: ev.sender_id.clone(), // #74 重放落库时保持原发送者标识
            ts: ev.ts,     // #74 重放落库时保持原事件时间
            created_at: crate::chrono_lite::unix_secs(),
            reply: None, // 回复产出后由 set_reply 落盘（阶段 1：W2 补发）
        });

        // 后端只认 per-bot 配置（app 里改），聊天里不再有 /codex /claude 切换——
        // 斜杠前缀原样透传给 agent（claude/codex 有自己的 slash 命令，不该被桥拦截）。
        let backend = Backend::parse(self.bot.effective_backend(&self.default_backend));
        // prompt = 用户文本 + 附件元数据（agent 按本地路径读文件）+ 链接清单（可选能力）。
        // 附件元数据行带路径/mime/sha256，agent 可直接读取工作区文件内容。
        let has_text = !text.is_empty();
        let urls = if has_text {
            crate::attachments::extract_urls(&text)
        } else {
            Vec::new()
        };
        // 引用/回复上下文：把被引用消息内容（文本 + 附件）放在用户文本之前，
        // agent 先读到「上面被引用的内容」。附件行格式与普通附件一致（本地路径/mime/sha）。
        let mut prompt = String::new();
        if !ev.quoted.text.is_empty() || !ev.quoted.attachments.is_empty() {
            prompt.push_str("[引用消息]\n");
            if !ev.quoted.text.is_empty() {
                prompt.push_str(&ev.quoted.text);
                if !ev.quoted.attachments.is_empty() {
                    prompt.push('\n'); // 文本后跟附件时让 [引用附件] 独占一行（与 [附件] 约定一致）
                }
            }
            if !ev.quoted.attachments.is_empty() {
                prompt.push_str("[引用附件]");
                for a in &ev.quoted.attachments {
                    prompt.push('\n');
                    prompt.push_str(&a.to_prompt_line());
                }
            }
            prompt.push_str("\n\n");
        }
        prompt.push_str(&text);
        if !ev.attachments.is_empty() {
            prompt.push_str("\n\n[附件]");
            for a in &ev.attachments {
                prompt.push('\n');
                prompt.push_str(&a.to_prompt_line());
            }
        }
        if !urls.is_empty() {
            prompt.push_str("\n\n[链接]");
            for u in urls {
                prompt.push('\n');
                prompt.push_str(&u);
            }
        }
        // 虚拟 Bot 角色注入（#75）：登记过的群聊消息，在 prompt 前置 [群角色] 块——
        // 群名=角色名、群介绍=system prompt（平台群资料为准，改群介绍即时生效：注入前
        // 查 5 分钟缓存，缓存过期自然刷新）。判定：chat_type=group + chat_id 在登记表
        // （mtime 懒刷新快照）。best-effort：群名/群介绍都拿不到（事件无群名 + API 查
        // 不到）→ 跳过注入，只 log，不阻塞消息处理。群聊 @ 门槛保持不变（本条消息能
        // 走到这里就已满足 @/话题/免 @ 之一，注入不改变准入语义）。
        // 顺序说明：历史注入、AGENTS.md 指令文件（下方 insert_str(0)）与受限说明
        // （再下方 insert_str(0)）都比这里后插入，最终顺序 =
        // 受限说明 > [指令文件] > 历史 > 群角色 > 用户文本——受限说明必须保持最外层
        // （它的注释是硬约束）；指令文件（行为指导）紧随受限说明、压在历史之上；
        // 群角色紧随历史之后即可（角色=你是谁，历史=之前聊过什么，都在用户新消息之前）。
        if let Some((vb_name, vb_desc)) = self.virtual_role_for(&ev).await {
            prompt.insert_str(0, &crate::virtualbot::role_block(&vb_name, &vb_desc));
            crate::log!(
                "[bridge] 注入群角色 bot={} chat={} 名={} 介绍长度={}",
                self.bot.key(),
                trunc(&ev.chat_id, 12),
                vb_name.chars().count(),
                vb_desc.chars().count()
            );
        }
        // 受限会话（授权者）：prompt 开头前置受限说明。CLAUDE.md 是 owner/授权者共享的
        // 同一份指引，不能靠它区分——prompt 注入才是按角色区分的正确载体（硬闸在 guard hook）。
        // 判定与 agent::run 的 restrict 一致（role==Granted && 开关热读）——owner 关掉
        // 隔离开关后，granted 会话实际是全权限，prompt 不得再谎称受限（否则模型自我设限、
        // 或把不存在的拦截声明当承诺）。读不到 config 按安全默认 true。
        // （insert 挪到锁内历史注入之后——受限说明必须保持最外层。）
        let restrict_prompt = crate::config::restrict_granted(ev.role, &self.bot.key());

        // #74：是否落历史库 + 未读提醒（granted 私聊）。覆盖飞书 p2p / 钉钉单聊(dm)；
        // owner 自己（role==Owner）排除；微信无授权者概念（on_weixin 恒 Owner）自然排除。
        // 提醒是纯本地 UI（托盘红点 + 弹窗），绝不主动向任何 IM 发消息（授权边界规则）。
        let record_granted = ev.role == crate::config::SenderRole::Granted
            && (ev.chat_type == "p2p" || ev.chat_type == "dm");
        // 发送者展示名：**锁外解析**（API 反查是 await——在代际锁/串行锁内 await 会让
        // std MutexGuard 跨 await → future 非 Send，见审查）。本地名单优先，未授权 API。
        // 未授权私聊的名字在 on_payload 未授权分支解析（不走 handle），这里只管 granted。
        let granted_uname = if record_granted {
            self.resolve_sender_name(&ev.sender_id).await
        } else {
            String::new()
        };

        // per-chat 串行：同一 key（话题=chat:thread）的并发消息排队等前一条处理完（不丢弃）。
        // 先从 std Mutex 取出该 key 的锁 Arc（短持 std 锁），再 await 异步锁。
        let chat_lock = self.chat_lock(&key);
        let _serial_guard = chat_lock.lock().await;
        if t0.elapsed().as_millis() > 50 {
            crate::log!(
                "[bridge] 排队等待处理 {}ms（bot={} chat={}）",
                t0.elapsed().as_millis(),
                self.bot.key(),
                trunc(&ev.chat_id, 12)
            );
        }

        // 会话快照必须在**拿到锁之后**取：锁外取的话，首轮 agent 还在跑时到达的第二条消息
        // 会读到过期的 started=false —— claude 侧对同一 UUID 再 --session-id 报「already in use」，
        // codex 侧新建 thread 覆盖掉首轮的 → 首轮上下文永久丢失。锁内取则前一轮必已 mark_started。
        // 一次锁内原子取 session_id + started：避免 ensure_session 与 is_started 两次
        // refresh 之间被外部改盘读到中间态（审查 P3-1a）。
        let (mut session_id, mut resume) = self.sessions.ensure_with_started(&key);

        // #49 后端切换上下文迁移：新会话首轮（!resume）且历史尚未注入过该会话 →
        // 把最近几轮对话注入 prompt 开头（切后端/会话丢失后新后端由此接续上下文）。
        // marker 按 session_id 判定：/new、CLI reset 使 marker 失效或失配（新会话允许
        // 再注入）。三层闸防重复注入：per-chat 串行锁（前一轮完整结束才轮到本条）+
        // !resume（started=true 的正常消息直接 resume 不注入）+ marker。#54：自愈重建
        // 会话带 pending 标记（同 sid）→ resume 轮也放行一次注入。
        let hist = crate::history::History::open(&self.bot.key(), &key);
        // 三级 AGENTS.md 指令文件（abb → bot → session）：读的是工作区指令文件
        //（用户手动维护，/new 与 session_gc 清理都不触碰它们）——放代际锁外读，
        // 锁临界区不扩大为磁盘 I/O（审查修复：原来每轮消息持 std Mutex 做 3 次
        // 文件读，同 key 的 /new 清盘与 session_gc 清理被拖长）。
        let agents_block =
            crate::agents_md::collect_block_at(&self.agents_md_root, &self.bot.key(), &key);
        // 代际锁（per-key，见 history_epochs 字段注释）：注入读 + 用户轮写盘在锁内与
        // /new 清盘互斥——新会话首轮不可能读到未清盘的旧历史（审查 I-2 读侧闭环）。
        // 锁持于块作用域内（std MutexGuard 非 Send 不能跨 await）：块结束即释放，
        // agent 运行期间不持锁（/new 不被运行中任务阻塞）。
        // #130：记录本轮实际注入的历史块原文（重试替换用；空 = 未注入历史）。
        let mut injected_block: Option<String> = None;
        let (hist_epoch_lock, hist_epoch, injected_rounds) = {
            let lock = self.history_lock(&key);
            let lock_ret = lock.clone(); // guard 借用 lock，返回值需独立 Arc
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let epoch = *guard;
            // 锁内复核槽位：session_gc 清理（同样持代际锁）可能在本轮
            // ensure_with_started（锁前，见上）之后删掉该 chat 槽位——复核到槽位没了
            // 则重建全新会话（本轮以新 sid 跑，mark_started_if 才能命中，回复与历史
            // 才不丢；gc 已写摘要 → 下方摘要兜底注入衔接上下文。审查修复）。/new /
            // CLI reset 换走的槽位仍在（新 sid），复核不触发——保持原语义（旧轮丢弃）。
            if self.sessions.chat_entry(&key).is_none() {
                let (sid2, res2) = self.sessions.ensure_with_started(&key);
                crate::log!(
                    "[bridge] 会话槽位被清理（session_gc），重建全新会话 bot={} key={} sid={}",
                    self.bot.key(),
                    trunc(&key, 16),
                    trunc(&sid2, 12)
                );
                session_id = sid2;
                resume = res2;
            }
            // 注入闸（锁内读 marker/entries：与 /new 的 clear 互斥，杜绝读侧交错）：
            // - !resume（新会话首轮）：marker 缺失或 sid 失配 → 注入（#49 后端切换迁移）。            //   pi 例外（#56 同一探针，两个 !resume 臂都参与）：文件存在即续聊——被打断/
            //   失败的 pi 轮次文件已在盘上（pi 会话创建即落盘），文件存在时再注入会把
            //   同一历史块二次写进 pi transcript；文件缺失（且 marker 命中）才是真丢失。
            // - resume（既有会话）：pending 命中（#54 自愈重建/换 UUID 后待补注入）
            //   → 放行恰好一次，注入成功后桥回写 pending=false（复位）；
            //   或 pi 会话文件丢失/损坏（#56：pi 对不可续聊的文件同 sid 静默新建空
            //   会话——文件被删或损坏均实测如此——无错误可检）→ 本轮直接注入
            //   （run 前即可探明，比 pending 早一轮）。**不设 marker 防重复护栏**：pi run
            //   成功必落会话文件（核心功能），文件持续不可续聊 = 每轮都是新会话，重注入是
            //   正确行为；用「marker 匹配即已注入过」拦截会把「迁移后文件才丢失」的
            //   真丢失误判为已注入（静默永久无上下文，恰是本功能要杀的症状）——布局
            //   误报的代价是可见噪音（提示从首轮起可见；误报持续时注入块按轮累积进
            //   pi transcript，每轮 ≤6000 字符），可接受。
            //
            // 架构（#56/#57 定论）：丢失检测**分层是本质而非债**——
            // - claude/codex 有错误文本（no rollout found / No conversation found），事后
            //   分类（agent.rs run）→ rebuilt + pending 迁移标记补注入；
            // - pi 无错误信号（静默新建），只能事前探查（本闸的探针）→ 本轮直接注入。
            let marker = hist.marker();
            let session_file_lost = || {
                backend == Backend::Pi
                    && !crate::agent::pi_session_exists(
                        &crate::workspace_dir(&self.bot.key()),
                        &session_id,
                    )
            };
            let session_file_alive = || {
                backend == Backend::Pi
                    && crate::agent::pi_session_exists(
                        &crate::workspace_dir(&self.bot.key()),
                        &session_id,
                    )
            };
            let should_inject = if !resume {
                match &marker {
                    Some(m) => m.session_id != session_id || session_file_lost(),
                    None => !session_file_alive(),
                }
            } else {
                matches!(&marker, Some(m) if m.pending && m.session_id == session_id)
                    || session_file_lost()
            };
            let injected_rounds = if should_inject {
                // #130：该会话已被上下文压缩（ctxsum 存在）→ 注入压缩块（旧摘要 + 近期
                // 原文）而非全量历史——压缩后全量历史仍会超长，注入必须切到压缩源。
                let workspace = crate::workspace_dir(&self.bot.key());
                let (block, n) = match crate::contextsum::ctxsum_block_at(&workspace, &key) {
                    Some(sum) => {
                        let rounds = sum.matches("用户: ").count();
                        (sum, rounds.max(1))
                    }
                    None => hist.inject_block(&ev.mid, crate::history::INJECT_CHARS_DEFAULT),
                };
                if n > 0 {
                    injected_block = Some(block.clone());
                    prompt.insert_str(0, &block);
                    Some(n)
                } else {
                    // 历史为空（/new 或会话归纳清理后）→ 兜底注入归档摘要（若有），
                    // 让新会话仍能衔接旧上下文。复用 should_inject 闸、无新 marker；
                    // 用 Some(0) 标记「摘要注入」：下游 set_marker(false) 防下轮重复
                    // 注入，toast 提示显示「已携带会话摘要」而非「0 轮上下文」。
                    match crate::session_gc::summary_block_at(
                        &self.agents_md_root.join("workspaces").join(self.bot.key()),
                        &key,
                    ) {
                        Some(summary) => {
                            prompt.insert_str(0, &summary);
                            Some(0)
                        }
                        None => None,
                    }
                }
            } else {
                None
            };
            // 三级 AGENTS.md 指令文件每轮全量注入：内容进 prompt 即「必读」——不依赖
            // 后端 CLI 的 cwd 自动加载（那是 bot 级指引的兜底通道）。文件读取已在锁外
            // 完成（collect_block_at，见上）；此处只做字符串拼接。位置：受限说明之后
            //（受限说明必须最外层，指令文件里的任何话术不得盖过安全约束）、历史注入
            // 之后（历史=事实背景，指令文件=行为指导，后者更靠顶部）。
            if !agents_block.is_empty() {
                prompt.insert_str(0, &agents_block);
            }
            // 受限说明后插（insert_str(0) 后进者更靠前）→ 保持在最外层
            if restrict_prompt {
                prompt.insert_str(0, crate::config::RESTRICT_PREAMBLE);
            }
            // 历史/消息库用同一份「用户轮文本」（显示与落库一致；只算一次——审查清理）
            let user_text = history_user_text(&text, &ev);
            // 当前用户轮落历史（锁内，与助手轮严格按真实顺序交替；重放由 (mid,user) 去重兜底）。
            // 锁内写与 /new 的 clear 互斥。
            hist.append_user(&ev.mid, backend.name(), &user_text);
            // #74：授权者（granted）私聊消息 → 落消息库 + 未读提醒（条件见 record_granted）。
            // 与 hist 同处锁内写：插入快、失败只 log，不阻塞主链路。
            // 落库与提醒联动（审查跟进）：insert 返回是否真正插入——重放（崩溃恢复
            // 续跑 handle）同 mid 再插会被 UNIQUE(mid,direction) 挡住 → false → 不
            // 重复提醒（弹窗/红点以「这条消息提醒过没」为准，不以收到几次为准）。
            if record_granted {
                // 展示名：锁外已解析（granted_uname；本地名单优先/API 反查）——
                // 历史/提醒都显示名字（8-20 用户反馈，不显示 open_id）
                let inserted = self.msgstore.insert(
                    &self.bot.key(),
                    &ev.chat_id,
                    &ev.mid,
                    "user",
                    &ev.sender_id,
                    &granted_uname,
                    &user_text,
                    ev.ts,
                );
                if inserted {
                    // 未读提醒：只记发送者 id + 名字 + 摘要（40 字符预览）。
                    // insert 返回真正插入才提醒——重放同 mid 被 UNIQUE 挡住 → 不重复
                    self.unread.report(
                        &self.bot.key(),
                        &ev.sender_id,
                        &granted_uname,
                        &crate::agent::truncate(&user_text, 40),
                        ev.ts,
                    );
                }
            }
            (lock_ret, epoch, injected_rounds)
        };

        let typing_rid = self.msgr.typing(&ev.mid).await;

        // cancel flag 注册进 cancel_flags，供该 chat 后续「停止词」消息叫停。
        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.cancel_flags
            .lock()
            .unwrap()
            .insert(key.clone(), cancel_flag.clone());

        let bot_key = self.bot.key();
        // clone Arc 再调：async_trait 的 method future 会借用 receiver，先取出独立 runner
        // 避免 future 跨 await 持有 `&self.agent_runner`，与 select 内 `&self` 的其它字段
        // 借用冲突（保持原自由函数调用「future 只持有 &self.sessions」的借用形态）。
        let runner = self.agent_runner.clone();
        let mut result = self
            .run_agent_with_progress(
                &runner,
                backend,
                &prompt,
                &session_id,
                resume,
                &ev.chat_id,
                &key,
                &bot_key,
                ev.role,
                &cancel_flag,
            )
            .await;

        // #130：上下文超长错误 → 自动分段压缩 + 换新会话重试一次（每会话只压缩一次）。
        // 开关默认开（config context_compress_enabled）；ctxsum 已存在 = 已压缩过 → 不
        // 再压缩（防循环），错误照常返回并带提示。压缩失败（LLM 摘要不可用/写盘失败）
        // 也不阻塞主链路：回落原错误，用户可见 hint 不误导。
        let mut compress_note: Option<String> = None;
        if let Err(e) = &result {
            let enabled = crate::config::Config::load()
                .map(|c| c.context_compress_enabled)
                .unwrap_or(true);
            if enabled && crate::contextsum::is_context_too_long(e) {
                let workspace = crate::workspace_dir(&bot_key);
                if crate::contextsum::ctxsum_block_at(&workspace, &key).is_none() {
                    crate::log!(
                        "[bridge] #130 上下文超长，自动压缩 chat={} err_len={}",
                        trunc(&ev.chat_id, 10),
                        e.chars().count()
                    );
                    match crate::contextsum::compress(
                        &workspace,
                        &key,
                        backend,
                        &ev.chat_id,
                        &bot_key,
                        ev.role,
                        runner.as_ref(),
                        Some(cancel_flag.clone()),
                    )
                    .await
                    {
                        Ok(report) => {
                            // 换新会话（reset_session：新 UUID + started=false）＋重建
                            // prompt：压缩块替换原历史块（未注入历史则前置压缩块）。
                            let new_sid = self.sessions.reset_session(&key);
                            let ctxsum_block = crate::contextsum::ctxsum_block_at(&workspace, &key)
                                .unwrap_or_default();
                            let mut prompt2 = prompt.clone();
                            match &injected_block {
                                Some(hb) if !hb.is_empty() => {
                                    prompt2 = prompt2.replacen(hb, &ctxsum_block, 1);
                                }
                                _ => {
                                    if !ctxsum_block.is_empty() {
                                        prompt2.insert_str(0, &ctxsum_block);
                                    }
                                }
                            }
                            compress_note = Some(format!(
                                "💡 历史过长，已自动压缩 {} 条旧消息为摘要（{} 段），保留最近 {} 条原文，已换新会话重试本条…",
                                report.compressed, report.summaries, report.kept
                            ));
                            crate::log!(
                                "[bridge] #130 压缩完成 chat={} 压缩={} 保留={} 段={} llm={} → 新会话重试",
                                trunc(&ev.chat_id, 10),
                                report.compressed,
                                report.kept,
                                report.summaries,
                                report.used_llm
                            );
                            result = self
                                .run_agent_with_progress(
                                    &runner,
                                    backend,
                                    &prompt2,
                                    &new_sid,
                                    false,
                                    &ev.chat_id,
                                    &key,
                                    &bot_key,
                                    ev.role,
                                    &cancel_flag,
                                )
                                .await;
                        }
                        Err(ce) => {
                            crate::log!("[bridge] ⚠️ #130 上下文压缩失败: {ce}");
                        }
                    }
                } else {
                    crate::log!(
                        "[bridge] #130 已压缩过仍超长 chat={}（不重复压缩，提示用户）",
                        trunc(&ev.chat_id, 10)
                    );
                }
            }
        }
        // 任务结束 → 摘掉打断标志（后续停止词将按普通消息处理）
        self.cancel_flags.lock().unwrap().remove(&key);

        // 统一只发最终结果一条（中途进度已在 select 循环丢弃）。
        match result {
            Ok(agent::RunOutcome::Reply {
                reply,
                session_id: final_sid,
                rebuilt,
            }) => {
                // #130：压缩重试成功 → 回复前置系统提示（用户可见压缩动作；历史落盘
                // 与发送均用同一份 reply，保持显示一致）。
                let reply = match &compress_note {
                    Some(note) => format!("{note}\n\n{reply}"),
                    None => reply,
                };
                // agent 成功即标记 started（会话状态只跟 agent 跑没跑成有关，与投递无关）。
                // #23：仅当当前槽位仍是本次任务的会话时才 mark——运行中被 /new 或
                // CLI `session reset` 换走时跳过（旧任务完成不得把新槽位置回 started=true）。
                // #49：同一道闸决定历史落盘——换走后不写孤儿助手条目、不写迁移标记
                // （历史已被 /new 清空，旧任务的回复不得写回去）。
                let same_session = self.sessions.mark_started_if(&key, &final_sid);
                if same_session {
                    // 代际闸：/new 恰好落在 mark 与写盘之间（亚毫秒窗口）也不残留孤儿条目
                    let guard = hist_epoch_lock.lock().unwrap_or_else(|e| e.into_inner());
                    if *guard == hist_epoch {
                        hist.append_assistant(&ev.mid, backend.name(), &reply);
                        // #54 会话自愈后的历史补注入：
                        // - 注入轮成功 → 写非 pending 标记（复位）
                        // - 同 sid 重建轮（rebuilt，必为 resume 轮）→ pending 标记，下一条注入
                        // - claude already-in-use 自愈（run 内 reset_session 换 UUID，
                        //   final_sid != 入口 session_id 且本轮未注入）→ 同样 pending 标记：
                        //   换 UUID 虽使旧 marker「失效」，但让失效生效的 !resume 闸
                        //   永远不会再触发（started 已被 mark 回 true）——必须显式补
                        //   pending，否则新会话与 #54 同症状永久无上下文（审查 Important）。
                        //   限定 resume 轮：!resume 轮的注入闸本轮已评估过（marker 失配
                        //   即已注入），首轮治愈没有旧上下文可丢——再写 pending 只会让
                        //   下一条把本轮自身重复注入一遍（新会话原生已含该轮）。
                        // 注一：pending 写入与 run 返回之间存在毫秒级崩溃窗口（pending.json
                        // 已 remove 后、标记未写前）——崩溃则该会话永久无注入；窗口极小，
                        // 与既有 at-least-once 语义同类，接受（写入后崩溃则标记已在盘上）。
                        // 注二：注入轮失败（Err/Cancelled）同样**不清** pending——下一条
                        // 重注入。对「提示从未送达模型」的失败轮这是必要的兜底；代价是
                        // 已送达但失败的轮次会在对端 transcript 里多一份注入块，可接受。
                        if injected_rounds.is_some() {
                            hist.set_marker(&final_sid, backend.name(), false);
                        } else if rebuilt
                            || (backend == Backend::Claude && resume && final_sid != session_id)
                        {
                            hist.set_marker(&final_sid, backend.name(), true);
                        }
                    }
                }
                // 注入提示随最终回复一条发出（不独立发消息，打字机已下线纪律）。
                // n==0 是摘要兜底注入（见 handle 注入点），文案不能显示「0 轮上下文」。
                let history_note = injected_rounds.map(|n| {
                    if n == 0 {
                        "\n\n（已携带会话摘要）".to_string()
                    } else {
                        format!("\n\n（已携带最近 {n} 轮上下文）")
                    }
                });
                // 普通回复全文发送。发送结果必须留痕：回复丢了
                // （token 失效/会话失效等）时不能谎报成功。
                // #49：注入提示附在全文尾部（若本轮做过历史注入）。
                let sent_text = match &history_note {
                    Some(note) => format!("{reply}{note}"),
                    None => reply.clone(),
                };
                // 阶段 1（W2 窗口修复）：回复产出后先把最终文本落盘到 pending 条目——
                // 「发送前崩溃」的恢复据此**补发而非重跑**（原 remove 在发送前，
                // 此窗口崩溃 = 回复静默丢失）。发送完成后才 remove（send 成功但
                // remove 前崩溃 = 重启补发一条重复回复，at-least-once 仅重发文本，
                // 严格优于重跑；发送失败也 remove——用户在场可重发，恢复路径的无人
                // 值守补发不适用此场景，避免重启后陈旧回复）。
                // 审查跟进：same_session=false（运行中被 /new / CLI reset 换走）时
                // **不落盘 reply**——该回复属于已作废会话，崩溃后补发会把旧答案送进
                // 用户明确重置过的新会话（历史已清、提示已过期）；此窗口退回 W1 重跑，
                // 与基线一致（mark_started/历史已被上方同闸跳过）。
                // 注（再审 Minor）：门控只覆盖「/new 在 Reply 臂评估前发生」的窗口——
                // /new 绕过串行锁，可落在 mark_started_if=true 与 remove 之间（此时
                // set_reply 已落盘），崩溃后仍会补发旧答案；该残余窗口属文档接受的
                // at-least-once 语义，不额外处理。
                if same_session {
                    self.pending.set_reply(&ev.mid, &sent_text);
                }
                let send_result = self.send_reply(&ev, &sent_text).await;
                // remove 统一一处（审查：原 Ok/Err 两臂各自复制——未来只改一臂会
                // 破坏「发送后摘 pending」的 W2 不变式）
                self.pending.remove(&ev.mid);
                match send_result {
                    Ok(()) => {
                        // #74：bot 回复落历史库（与用户轮同条件：granted 私聊，见 record_granted）。
                        // 回复 mid 复用用户轮 mid（history.rs 一消息一回复语义），由
                        // UNIQUE(mid, direction) 幂等区分；时间用发送时刻。发的是纯回复
                        // （不含注入提示 history_note——那是迁移期瞬态，不进历史）。
                        if record_granted {
                            self.msgstore.insert(
                                &self.bot.key(),
                                &ev.chat_id,
                                &ev.mid,
                                "assistant",
                                &ev.sender_id,
                                "", // assistant 行 GUI 显示 bot 名（direction 区分）
                                &reply,
                                crate::chrono_lite::unix_secs() as i64,
                            );
                        }
                        crate::log!(
                            "[bridge] 已回复 chat={} 长度={}",
                            trunc(&ev.chat_id, 10),
                            reply.chars().count()
                        )
                    }
                    Err(e) => crate::log!(
                        "[bridge] ⚠️ 回复发送失败 chat={}: {e:#}",
                        trunc(&ev.chat_id, 10)
                    ),
                }
            }
            Ok(agent::RunOutcome::Cancelled) => {
                crate::log!("[bridge] 任务被打断 chat={}", trunc(&ev.chat_id, 10));
                // 先摘 pending 再发停止通知（审查：remove 若在发送后，「发送期间/后
                // remove 前」崩溃会让已叫停的任务以 reply=None 残留 → 重启被普通重放
                // **续跑**，违背叫停语义；停止通知本身丢失可接受——用户已在场叫停）。
                self.pending.remove(&ev.mid);
                // 只发最终结果：「⏹ 已停止」一条。失败也留痕（审查：原 `let _` 全静默——
                // pending 已摘、无重试路径，连日志都没有，用户与运维都无从得知）。
                if let Err(e) = self.send_reply(&ev, "⏹ 已停止").await {
                    crate::log!(
                        "[bridge] ⚠️ 停止通知发送失败 chat={}: {e:#}",
                        trunc(&ev.chat_id, 10)
                    );
                }
                // 不 mark_started：被打断的轮次不算完成
            }
            Err(e) => {
                // #130：压缩重试仍失败 → 错误文案前置压缩提示（用户知道已自动处理过，
                // 不再静默重试）。
                let e = match &compress_note {
                    Some(note) => format!("{note}\n\n{e}"),
                    None => e,
                };
                // 错误文案作为最终回复发出（用户可见原因），同样留痕。
                // 先摘 pending（任务已结束；错误文案发送失败不重跑，与基线一致——
                // remove 若在发送后，崩溃窗口会让失败任务被重启重放续跑）。
                self.pending.remove(&ev.mid);
                match self.send_reply(&ev, &e).await {
                    Ok(()) => crate::log!(
                        "[bridge] 已回复错误 chat={} 长度={}",
                        trunc(&ev.chat_id, 10),
                        e.chars().count()
                    ),
                    Err(se) => crate::log!(
                        "[bridge] ⚠️ 错误回复发送失败 chat={}: {se:#}",
                        trunc(&ev.chat_id, 10)
                    ),
                }
            }
        }

        self.msgr.del_typing(&ev.mid, typing_rid).await;
        self.msgr.done(&ev.mid).await;
        // _serial_guard 在此函数末尾 drop，释放 per-chat 锁，排队的下一条开始处理。
    }

    /// 执行一轮 agent 任务（统一中途进度排空：打字机已下线，中途输出丢弃不回，
    /// 任务结束只发最终结果一条）。#130 压缩重试也走本方法（每轮独立 progress 通道）。
    #[allow(clippy::too_many_arguments)] // 与 agent::run 同款参数集（11 参）
    async fn run_agent_with_progress(
        &self,
        runner: &std::sync::Arc<dyn crate::agent::AgentRunner>,
        backend: crate::agent::Backend,
        prompt: &str,
        session_id: &str,
        resume: bool,
        chat_id: &str,
        key: &str,
        bot_key: &str,
        role: crate::config::SenderRole,
        cancel_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<crate::agent::RunOutcome, String> {
        let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let run_fut = runner.run(
            backend,
            prompt,
            session_id,
            resume,
            chat_id,
            key, // 会话隔离 key（话题=chat:thread，#14）：session 存储按 key 记账，回存须同 key
            bot_key,
            role, // 发送者角色：granted 走受限分支（restrict 判定在 agent::run 内热读）
            Some(&self.sessions),
            Some(ptx),
            Some(cancel_flag.clone()),
        );
        tokio::pin!(run_fut);
        // 中途输出只计数不逐条留日志：编码 agent 一轮任务可推数百条进度，逐条写盘会让
        // 日志量随任务时长无界增长。统一只发最终结果：丢弃并计数，收尾汇总成一行日志。
        let mut dropped_progress = 0usize;
        let result = loop {
            tokio::select! {
                Some(_p) = prx.recv() => {
                    dropped_progress += 1;
                }
                r = &mut run_fut => { break r; }
            }
        };
        // run 完成时通道里可能还有刚入队未消费的中途输出（select 双就绪随机 break）——
        // 全部排空丢弃（agent 侧 unbounded send 不阻塞），不留残留。
        while let Ok(_p) = prx.try_recv() {
            dropped_progress += 1;
        }
        if dropped_progress > 0 {
            crate::log!(
                "[bridge] 丢弃中途进度 {} 条 chat={}（统一只发最终结果）",
                dropped_progress,
                trunc(chat_id, 10)
            );
        }
        result
    }

    /// /trash 子命令处理（#88）。owner 已由调用方过滤；这里只产出回复文案。
    async fn trash_reply(&self, _ev: &Ev, tc: TrashCmd) -> String {
        let workspace = crate::workspace_dir(&self.bot.key());
        match tc {
            TrashCmd::List => {
                let items = crate::trash::list(&workspace);
                let pending = crate::guard::list_pending(&self.bot.key());
                if items.is_empty() && pending.is_empty() {
                    return "🗑 回收站为空".into();
                }
                let mut lines = Vec::new();
                for it in items.iter().take(10) {
                    let remain = it
                        .trashed_at
                        .saturating_add((self.bot.trash_ttl_days.max(1) as u64) * 86400)
                        .saturating_sub(crate::chrono_lite::unix_secs())
                        / 86400;
                    lines.push(format!(
                        "{} | {}（{} 天后过期）{}",
                        &it.id[..it.id.len().min(8)],
                        crate::trash::pretty_path(std::path::Path::new(&it.orig)),
                        remain,
                        if it.dangerous { " ⚠️" } else { "" }
                    ));
                }
                if items.len() > 10 {
                    lines.push(format!(
                        "…共 {} 条（更多用 trash list 命令查看）",
                        items.len()
                    ));
                }
                if !pending.is_empty() {
                    lines.push("\n待确认危险删除（/trash confirm <路径>）：".into());
                    for (p, _) in pending {
                        lines.push(format!("  {p}"));
                    }
                }
                lines.join("\n")
            }
            TrashCmd::Restore(id) => match crate::trash::restore(&workspace, &id) {
                Ok(it) => format!("♻️ 已恢复：{} → {}", &it.id[..it.id.len().min(8)], it.orig),
                Err(e) => format!("⚠️ 恢复失败：{e}"),
            },
            TrashCmd::Purge(all) => {
                let n = if all {
                    crate::trash::purge_all(&workspace)
                } else {
                    crate::trash::purge_expired(&workspace, self.bot.trash_ttl_days.max(1))
                };
                format!(
                    "🧹 已清理回收站条目 {n} 条{}",
                    if all { "（全部）" } else { "（过期）" }
                )
            }
            TrashCmd::Confirm(path) => {
                match crate::guard::confirm_dangerous_delete(&self.bot.key(), &workspace, &path) {
                    Ok(it) => format!(
                        "✅ 已确认并移入回收站：{}（{} 天内可恢复；/trash restore 可撤回）",
                        it.orig,
                        self.bot.trash_ttl_days.max(1)
                    ),
                    Err(e) => format!("⚠️ 确认失败：{e}"),
                }
            }
        }
    }
}

/// /trash 回收站指令（#88）。
#[derive(Debug, PartialEq)]
enum TrashCmd {
    List,
    Restore(String),
    Purge(bool),
    Confirm(String),
}

/// 识别 /trash 指令：
///
/// - `/trash` / `/trash list` → List
/// - `/trash restore <id>` → Restore(id)
/// - `/trash purge` → Purge(false)；`/trash purge --all` → Purge(true)
/// - `/trash confirm <path>` → Confirm(path)
///
/// 其它形态（含 `/trash` 后跟未知子命令）返回 None（原样透传 agent）。
fn parse_trash_cmd(text: &str) -> Option<TrashCmd> {
    let mut parts = text.split_whitespace();
    if !parts.next().map(|p| p.eq_ignore_ascii_case("/trash"))? {
        return None;
    }
    let sub = parts.next().map(|s| s.to_ascii_lowercase());
    match sub.as_deref() {
        None | Some("list") => Some(TrashCmd::List),
        Some("restore") => parts.next().map(|id| TrashCmd::Restore(id.to_string())),
        Some("purge") => {
            let all = parts.any(|p| p.eq_ignore_ascii_case("--all"));
            Some(TrashCmd::Purge(all))
        }
        Some("confirm") => parts.next().map(|p| TrashCmd::Confirm(p.to_string())),
        _ => None,
    }
}
