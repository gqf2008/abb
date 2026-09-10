//! Bridge 子模块：虚拟 Bot 处理（#80 按功能面拆分，impl Bridge 分散到子模块——
//! 子模块是父模块后代，可访问 mod.rs 私有字段，无需改可见性）。

use super::*;
use uuid::Uuid;

impl Bridge {
    /// 按消息/事件角色选 ACP 实例（单后端化 P2.2）：granted 受限会话路由 granted
    /// 实例（强制受限剖面 + 进程级 NO_HINTS）；其余走 normal 实例（bot 配置档）。
    /// 判据 `restrict_granted`（role==Granted && 开关热读）与 prompt 受限说明/agent
    /// 受限判定同源，防语义漂移。None = 句柄对未装配（测试挡板/job 内部路径）。
    fn acp_handle_for_role(
        &self,
        role: crate::config::SenderRole,
    ) -> Option<std::sync::Arc<crate::buzz::harness::BuzzHandle>> {
        self.acp_handles.as_ref().map(|hs| {
            if crate::config::restrict_granted(role, &self.bot.key()) {
                hs.granted.clone()
            } else {
                hs.normal.clone()
            }
        })
    }
}

/// #206：buzz 通道资格预检的失败面（dispatch 与 /cancel 共用判据、各自表述
/// 文案——判据只有一处，杜绝两处预检漂移）。P2.2 起角色不再是拒绝理由而是实例
/// 选择器（granted → 受限实例）；/cancel 与 dispatch 均按角色选实例，判据一致。
#[derive(Debug, PartialEq, Eq)]
enum BuzzPrecheckFail {
    /// 句柄对未装配（生产常驻；None = 测试挡板/job 内部路径，生产不可达——防御）
    BuzzDisabled,
    /// agent 进程不可用（未拉起/崩溃退避中——新消息会被预检拒绝，避免「以为
    /// 已受理」的静默排队）
    AgentDown,
    /// 未配置模型供应商（生效供应商解析为 None——buzz agent 进程共用一组凭证，
    /// 与 CLI 后端硬闸同源语义；区别仅在 buzz 闸在预检而非 build_injection）
    NoProvider,
    /// granted 受限会话的执行能力已判为不支持（P2.3 硬闸）：随包 fork 已起且
    /// initialize 未声明 `_meta.abbSandbox` ⇒ 提前拒答——绝不允许「发不出档位就
    /// 默认 FullAccess」的静默降级（新 ABB + 旧随包 fork 是唯一 Critical 风险）。
    /// 未启动（Unknown）不在此拒——真闸在 session 创建处（pool），此处只做
    /// 「已知不行」的早拒，避免首条 granted 消息因懒启动被误拒。
    SandboxUnsupported,
}

impl Bridge {
    /// #206 话题隔离：dispatch 预检（话题感知，**纯判据无副作用**——预检
    /// 「不过 = 无副作用」invariant：被拒的 dispatch 不得污染频道注册表，
    /// 审查 #214 P2-1。话题频道登记（注册 + 锚点）由
    /// [`Self::buzz_ensure_topic_channel`] 在全闸通过之后执行）。
    /// 判据顺序：按角色选实例 → agent 可用 → 供应商闸 →（granted）能力协商闸。
    /// - 群根消息：**无「已登记」门槛**——bot 能收到消息的群都该能回（提及/授权
    ///   原则由平台与桥的 @ 门槛保证）；未登记的群由
    ///   [`Self::buzz_ensure_group_channel`] 闸后自动登记（adhoc，巡检豁免清理）。
    /// - 话题消息（thread_id 非空）：话题从属于群——群根频道自动登记后话题
    ///   频道同样闸后登记；agent 可用性判据沿用（acp 就绪才有轮次可跑，与群根
    ///   一致——单 slot 无「话题订阅在途」中间态，话题消息与群根消息共享同一判据）。
    ///
    /// 返回 Ok(群根频道名)：优先虚拟 Bot 登记表的角色名；未登记群回退
    /// 「群聊·<chat_id 前缀>」——闸后登记话题频道时作话题频道名来源，不另查
    /// harness 频道表，防两份来源漂移。
    fn buzz_dispatch_precheck(&self, ev: &Ev) -> Result<String, BuzzPrecheckFail> {
        // P2.2：按角色选实例——granted 会话路由 granted 实例（受限剖面）。
        let handle = self
            .acp_handle_for_role(ev.role)
            .ok_or(BuzzPrecheckFail::BuzzDisabled)?;
        let granted = crate::config::restrict_granted(ev.role, &self.bot.key());
        // p2p（私聊）：免登记——频道由 dispatch 全闸通过后即时注册（upsert 幂等），
        // 频道名 = 对方展示名。私聊无「群登记」概念（用户私聊 bot 是合理路径）。
        if ev.chat_type == "p2p" || ev.chat_type == "dm" {
            if !handle.is_agent_available() {
                return Err(BuzzPrecheckFail::AgentDown);
            }
            if !crate::agent::provider_ready(
                crate::config::Config::provider_for_bot_key_of(&self.cfg_snapshot, &self.bot.key())
                    .as_ref(),
            ) {
                return Err(BuzzPrecheckFail::NoProvider);
            }
            // P2.3 granted 能力硬闸（绝不静默降级 FullAccess）：仅在能力位已判为
            // Unsupported（agent 已起、initialize 未声明）时提前拒答。Unknown
            //（懒启动未起）放行——真闸在 session 创建处（pool，与本 agent 的
            // initialize 结果同源，无竞态），否则首条 granted 消息必被误拒。
            if granted
                && handle.sandbox_support() == crate::buzz::harness::SandboxSupport::Unsupported
            {
                return Err(BuzzPrecheckFail::SandboxUnsupported);
            }
            let cfg = crate::config::Config::load().unwrap_or_default();
            let (peer, _) = crate::ui::resolve_display(&cfg, &self.bot.key(), &ev.sender_id);
            let name = if peer.is_empty() {
                format!("私聊·{}", trunc(&ev.sender_id, 10))
            } else {
                peer
            };
            return Ok(name);
        }
        // 无「已登记」门槛：bot 能收到消息的群都该能回（自动登记见
        // buzz_ensure_group_channel，闸后执行——预检无副作用 invariant）。
        if !handle.is_agent_available() {
            return Err(BuzzPrecheckFail::AgentDown);
        }
        // 供应商硬闸（与 CLI 后端 build_injection None 臂同源）：生效供应商为
        // None 时 agent 无凭证可用，拒答并引导配置（每消息热读，与 agent.rs 同款成本）。
        if !crate::agent::provider_ready(
            crate::config::Config::provider_for_bot_key_of(&self.cfg_snapshot, &self.bot.key())
                .as_ref(),
        ) {
            return Err(BuzzPrecheckFail::NoProvider);
        }
        // P2.3 granted 能力硬闸（同 p2p 分支：仅 Unsupported 提前拒答，Unknown 放行）
        if granted && handle.sandbox_support() == crate::buzz::harness::SandboxSupport::Unsupported
        {
            return Err(BuzzPrecheckFail::SandboxUnsupported);
        }
        // 群根频道名：优先虚拟 Bot 登记快照的角色名（mtime 懒刷新，与注入判定
        // 同源）；未登记的群回退「群聊·<chat_id 前缀>」（自动登记语义，不拒答）。
        self.refresh_virtual_bots();
        {
            let bots = self.virtual_bots.lock().unwrap();
            let vb_name = bots
                .iter()
                .find(|v| v.bot_key == self.bot.key() && v.chat_id == ev.chat_id)
                .map(|v| v.role_name.clone());
            if let Some(name) = vb_name {
                return Ok(name);
            }
        }
        Ok(format!("群聊·{}", trunc(&ev.chat_id, 10)))
    }

    /// 未登记群的自动登记（用户决策：bot 能收到消息的群都该能回，提及/授权
    /// 原则由平台与桥的 @ 门槛保证）。仅当频道未登记时 upsert（已登记的 vb
    /// 群不被覆盖——巡检登记的角色名优先）。adhoc 标记让巡检 diff 豁免清理
    ///（不在登记表是常态而非消失）。
    fn buzz_ensure_group_channel(&self, ev: &Ev, group_name: &str) {
        let Some(handle) = self.acp_handle_for_role(ev.role) else {
            return; // 预检已过则 handle 必在；防御性早退
        };
        let uuid = Uuid::parse_str(&crate::buzz::keys::channel_uuid(
            &self.bot.key(),
            &ev.chat_id,
        ))
        .expect("channel_uuid output must parse as Uuid");
        if handle.channel_registered(&uuid) {
            return;
        }
        handle.upsert_channel(
            uuid,
            crate::buzz::harness::ChannelMeta {
                bot_key: self.bot.key(),
                chat_id: ev.chat_id.clone(),
                chat_type: "group".to_string(),
                thread_id: None,
                workspace: Some(self.workspace_for(&ev.chat_id).display().to_string()),
                name: group_name.to_string(),
                anchor_mid: None,
                adhoc: true,
            },
        );
    }

    /// p2p 私聊的群根频道即时注册：dispatch 全闸通过后调用（upsert 幂等——首条
    /// 消息注册，后续消息刷新 meta）。巡检对 adhoc 频道豁免清理，不会被登记表
    /// diff 误清。频道名 = 对方展示名（agent prompt 上下文）。
    fn buzz_ensure_p2p_channel(&self, ev: &Ev, peer_name: &str) {
        let Some(handle) = self.acp_handle_for_role(ev.role) else {
            return;
        };
        let uuid = Uuid::parse_str(&crate::buzz::keys::channel_uuid(
            &self.bot.key(),
            &ev.chat_id,
        ))
        .expect("channel_uuid output must parse as Uuid");
        handle.upsert_channel(
            uuid,
            crate::buzz::harness::ChannelMeta {
                bot_key: self.bot.key(),
                chat_id: ev.chat_id.clone(),
                chat_type: "p2p".to_string(),
                thread_id: None,
                workspace: Some(self.workspace_for(&ev.chat_id).display().to_string()),
                name: peer_name.to_string(),
                anchor_mid: None,
                adhoc: true,
            },
        );
    }

    /// #206 话题隔离：话题频道登记——dispatch 全闸通过之后调用（审查 #214 P2-1：
    /// 被拒 dispatch 不得登记频道）。幂等（upsert 覆盖
    /// 同频道）；每次 dispatch 都刷新锚点（= 本条用户消息 mid，话题回复
    /// send_thread_reply 的落点）。
    fn buzz_ensure_topic_channel(&self, ev: &Ev, group_name: &str) {
        let Some(handle) = self.acp_handle_for_role(ev.role) else {
            return; // 预检已过则 handle 必在；防御性早退
        };
        let uuid = Uuid::parse_str(&crate::buzz::keys::topic_channel_uuid(
            &self.bot.key(),
            &ev.chat_id,
            &ev.thread_id,
        ))
        .expect("channel_uuid output must parse as Uuid");
        handle.upsert_channel(
            uuid,
            crate::buzz::harness::ChannelMeta {
                bot_key: self.bot.key(),
                chat_id: ev.chat_id.clone(),
                chat_type: "group".to_string(),
                thread_id: Some(ev.thread_id.clone()),
                workspace: Some(self.workspace_for(&ev.chat_id).display().to_string()),
                name: group_name.to_string(),
                anchor_mid: Some(ev.mid.clone()),
                adhoc: false,
            },
        );
        crate::log!(
            "[bridge] 话题频道已登记/刷新锚点 bot={} chat={} thread={}",
            self.bot.key(),
            trunc(&ev.chat_id, 12),
            trunc(&ev.thread_id, 16)
        );
    }

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
            // #147 双向一致：团队条目对应角色 chat_id 清空（状态转「部分失败」）
            crate::teamreg::TeamStore::new().clear_chat(&self.bot.key(), chat_id);
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
                &ev.chat_type,
                &ev.chat_name,
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
            // 无在跑任务 → 命令化反馈，不喂给 agent。口径与 dispatch 一致：harness
            // 已装配（生产恒真）→ 真实叫停——发布 owner 控制命令 "!cancel" 走
            // harness.cancel（协议与语义见 buzz_cancel_reply）；未装配（测试挡板）
            // → 静态文案（测试不碰 harness）。
            let msg = if self.acp_handles.is_some() {
                self.buzz_cancel_reply(&ev).await
            } else {
                "✅ 当前没有正在运行的任务。".to_string()
            };
            if let Err(e) = self.send_reply(&ev, &msg).await {
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
        // #194：虚拟 Bot 群的会话存储与工作目录（vb/<uuid>/，含存量迁移）；非虚拟零变化。
        let sessions = self.sessions_for(&ev.chat_id);
        if is_new_command(&text) {
            // #49：/new = 用户明确要求全新会话 → 连对话历史与迁移标记一起清
            // （切换注入的历史随之失效，不会泄进新会话）。代际自增使交错窗口内
            // 串行锁里的旧写盘全部失效（审查 I-2：clear 与锁内写无锁互斥的 TOCTOU）。
            // 顺序：先清历史再换会话——clear 失败则中止重置（否则旧历史泄进新会话，
            // 审查 I-2 读侧）；崩溃窗口从「新会话读到旧历史」变成「reset 未生效」
            //（用户可见的失败，无静默泄漏）。
            if self.history_reset(&key) {
                // （P4.1 删除：/new 顺手清旧 sid 的 .pi-sessions 文件——pi 后端已退役，
                // legacy 转录清理由 session_gc / tidy 的孤儿清理兜底。）
                let new_sid = sessions.reset_session(&key);
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
            reply: None,        // 回复产出后由 set_reply 落盘（阶段 1：W2 补发）
            resume_attempts: 0, // #164 新消息首次入队从 0 计（异常退出恢复才递增）
        });

        // 单后端化（P4.1）：执行层恒为 buzz harness——老配置 backend/default_backend
        // 字段直接忽略（serde 天然容错），历史/marker 的 backend 字段恒记 "buzz"
        //（出处标注，不参与任何闸判定）。
        // #200 Phase 2：buzz 路径资格预检——必须在 **prompt 组装与历史落盘之前**
        // （审查 #205r2）：预检不过 = 这一轮根本不会发生，既不该白烧迁移历史读/指令块
        // 拼接，更不该往 ABB 历史里写一条「有去无回」的用户轮（单边历史会在日后
        // buzz→CLI 切换时被当上下文注入）。资格（全部由 buzz_dispatch_precheck 承载）：
        // ① harness 句柄已装配（P2.1 起句柄对按 bot 常驻构造；None = 测试挡板/
        //    job 内部路径，本分支在生产不可达——防御）；
        // ② 供应商硬闸（NoProvider）——无「已登记」门槛：bot 能收到消息的群都该能回，
        //    未登记群闸后自动登记（buzz_ensure_group_channel，adhoc 标记巡检豁免；
        //    p2p 同语义即时注册）；
        // ③ agent 进程可用（启动失败/崩溃退避中 = 不可用——此时 push 只是排队
        //    进 dead queue，无 agent 可跑，用户侧是无限等待，#205r4 同型）；
        // ④ granted 能力协商闸（SandboxUnsupported，P2.3）——受限会话必须走受限
        //    实例，随包 fork 未声明 `_meta.abbSandbox` 一律拒答（绝不静默降级）。
        // ACP 单轨：harness 句柄对已装配（生产常驻）→ 按角色选实例 dispatch 异步回合；
        // 未装配（测试挡板/job 内部路径）→ 回落 spawn 同步路径。
        if self.acp_handles.is_some() {
            // 预检话题感知（话题频道缺失不再拒绝——登记在全闸通过后做）。
            let precheck = self.buzz_dispatch_precheck(&ev);
            let reason = match &precheck {
                Err(BuzzPrecheckFail::BuzzDisabled) => Some("服务未装配 agent（重启服务）"),
                // agent 不可用 = 启动失败/崩溃退避中：当场报错优于静默排队
                //（失败批次会随重拉重试，死信积压见 harness queue 语义）。
                Err(BuzzPrecheckFail::AgentDown) => {
                    Some("agent 未就绪（启动失败/崩溃退避中），本轮无法执行")
                }
                Err(BuzzPrecheckFail::NoProvider) => {
                    Some("未配置模型供应商或未填 API Key：请在 ABB 设置「模型供应商」页补全并保存")
                }
                Err(BuzzPrecheckFail::SandboxUnsupported) => Some(
                    "受限（授权者）会话需要 ABB 随包 agent 具备受限执行能力——请升级 ABB 后重试",
                ),
                Ok(_) => None,
            };
            if let Some(why) = reason {
                crate::log!(
                    "[bridge] ⚠️ buzz 路径预检未通过 chat={}: {why}",
                    trunc(&ev.chat_id, 12)
                );
                self.pending.remove(&ev.mid);
                if let Err(e) = self
                    .send_reply(&ev, &format!("⚠️ 无法处理本条消息：{why}。"))
                    .await
                {
                    crate::log!(
                        "[bridge] ⚠️ buzz 预检失败提示发送失败 chat={}: {e:#}",
                        trunc(&ev.chat_id, 10)
                    );
                }
                return;
            }
            // 全闸（①②③④）通过后才登记频道（审查 #214 P2-1：
            // 被拒的 dispatch 不得污染频道注册表——预检「不过=无副作用」）。
            // 话题消息：登记话题频道（群根已登记）；p2p 私聊：即时注册群根频道
            //（免登记语义，频道名 = 对方名）。
            if !ev.thread_id.is_empty() {
                if let Ok(group_name) = &precheck {
                    self.buzz_ensure_topic_channel(&ev, group_name);
                }
            } else if ev.chat_type != "group" {
                if let Ok(peer_name) = &precheck {
                    self.buzz_ensure_p2p_channel(&ev, peer_name);
                }
            } else if let Ok(group_name) = &precheck {
                // 群根：未登记的群自动登记（用户决策——bot 能收到消息的群都该
                // 能回；已登记的 vb 群不被覆盖）。
                self.buzz_ensure_group_channel(&ev, group_name);
            }
        }
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
        let (mut session_id, mut resume) = sessions.ensure_with_started(&key);

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
            if sessions.chat_entry(&key).is_none() {
                let (sid2, res2) = sessions.ensure_with_started(&key);
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
            // - !resume（新会话首轮）：marker 缺失或 sid 失配 → 注入（#49 后端切换迁移）。
            // - resume（既有会话）：pending 命中（#54 自愈重建/换 UUID 后待补注入）
            //   → 放行恰好一次，注入成功后桥回写 pending=false（复位）。
            //
            // 单后端化（P4.1）：CLI 后端随 run 一并删除，注入闸的 pi 探针臂（#56 事前
            // 探查 pi 会话文件丢失/损坏）随之退役——buzz harness 会话不落盘续聊
            // （服务启动即清槽，见 sessions.rs reset_slots_for_service_start），
            // 丢失场景只剩「新会话首轮注入」与「pending 自愈补注入」两条既有闸臂。
            let marker = hist.marker();
            let should_inject = if !resume {
                match &marker {
                    Some(m) => m.session_id != session_id,
                    None => true,
                }
            } else {
                matches!(&marker, Some(m) if m.pending && m.session_id == session_id)
            };
            let injected_rounds = if should_inject {
                // #194：workspace 已按 chat 路由——虚拟 Bot 群用 vb/<uuid>/。
                //（P4.1：contextsum 压缩模块已删——单执行层后生产不可达的死码，
                // 注入只走全量历史块一条路径。）
                let (block, n) = hist.inject_block(&ev.mid, crate::history::INJECT_CHARS_DEFAULT);
                if n > 0 {
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
            hist.append_user(&ev.mid, "buzz", &user_text);
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
                    &ev.chat_type,
                    &ev.chat_name,
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

        // #200 Phase 3：buzz 后端短路——消息 push 进 harness 队列（harness 单
        // slot + per-channel 串行锁：同一频道不会有两轮并发，在跑轮次期间到达
        // 的消息按 Queue 语义入队、回合结束时随 steer 合并进下一轮），回合结束
        // 捕获 agent 文本同步投递回聊天平台。单后端化（P4.1）后 CLI spawn 已整体
        // 删除，下方 spawn 同步路径只剩测试挡板/job 内部兜底（生产 Runner 是
        // SpawnRetiredRunner 诚实报错桩）。buzz 路径语义：
        // - 会话上下文由 harness 的 channel→session 持有：ABB 侧首轮迁移注入照常
        //   （注入闸照跑），push 成功后 mark_started + 落 marker，防后续每轮重复
        //   注入（CLI 路径这步在 run 返回后做；buzz 无同步轮次，就地补齐）。
        // - 中断：buzz 轮次叫停已接线（#206）——/cancel 走 harness 的 cancel
        //   信号（见 buzz_cancel_reply；话题内 /cancel 停话题频道在跑轮次，粒度
        //   与 CLI 的 chat:thread key 对齐）；桥侧仍无轮次记账，本路径不注册
        //   cancel flag（桥无从知道轮次何时在跑）。自然停止词（停/取消/…）维持
        //   按普通消息透传：队列语义下它并入在跑轮次后、随下一轮 steered prompt
        //   一起交 agent（pi-acp 无原生 steer，走 cancel+merge 重跑一轮重提示）。
        // 表情语义与 CLI 对齐：收到打「处理中」（上方），回复投递时撤销+✅。
        // ACP 单轨：句柄对已装配 → 按角色 push 进对应实例（P2.2：granted→受限实例，
        // 其余→normal 实例）异步回合后返回；未装配（测试挡板/job 内部）落到下方
        // spawn 同步路径。
        if let Some(handle) = self.acp_handle_for_role(ev.role) {
            crate::log!(
                "[bridge] buzz 路径：push 进 harness chat={} len={}",
                trunc(&ev.chat_id, 12),
                prompt.chars().count()
            );
            // 频道 uuid 派生（root/话题与 harness 登记同源——root 由 service 巡检
            // 登记、话题由 buzz_ensure_topic_channel 登记，keys 命名空间见 keys.rs）。
            let channel_id = Uuid::parse_str(&if ev.thread_id.is_empty() {
                crate::buzz::keys::channel_uuid(&self.bot.key(), &ev.chat_id)
            } else {
                crate::buzz::keys::topic_channel_uuid(&self.bot.key(), &ev.chat_id, &ev.thread_id)
            })
            .expect("channel_uuid output must parse as Uuid");
            // 「处理中」表情（对齐 CLI 语义）：**一个在途回合一个 typing**——该
            // 频道回合表已有登记（上一条消息已打「处理中」，回合尚未结束）时
            // 复用其 typing_mid/typing_rid、不再打新表情；回合完成时撤的就是
            // 首条消息上的表情。爆发消息各打一个 typing、完成只撤最后一条 =
            // 表情泄漏（实机复现：三条连发，第一条/第二条 typing 永久挂着）。
            let (typing_mid, typing_rid) = {
                let reuse = {
                    let reg = self.turn_registry.lock().unwrap();
                    reg.get(&channel_id)
                        .map(|e| (e.typing_mid.clone(), e.typing_rid.clone()))
                };
                match reuse {
                    Some((tm, tr)) => (tm, tr),
                    None => {
                        let rid = self.msgr.typing(&ev.mid).await;
                        (Some(ev.mid.clone()), rid)
                    }
                }
            };
            // push 即处理完毕：摘 pending（重启不重放；push-摘除间崩溃 = 重启重放
            // 重复 prompt，at-least-once 语义，可接受）。
            self.pending.remove(&ev.mid);
            if !handle.push_message(
                channel_id,
                crate::buzz::queue::InboundMsg {
                    id_hex: ev.mid.clone(),
                    author_role: ev.role.as_str().to_string(),
                    text: prompt.clone(),
                    ts_secs: ev.ts,
                    // ABB 会话面扁平：无话题维；话题隔离走频道维（topic_channel_uuid）。
                    prompt_tag: "channel_message".to_string(),
                },
            ) {
                if let Err(e) = self
                    .send_reply(
                        &ev,
                        "⚠️ buzz 后端消息入队失败（harness 已关闭，详见服务日志），请重发。",
                    )
                    .await
                {
                    crate::log!(
                        "[bridge] ⚠️ buzz 未送达报错发送失败 chat={}: {e:#}",
                        trunc(&ev.chat_id, 10)
                    );
                }
                return;
            }
            // 回合登记（TurnOutput 投递时按它定位会话 key/代际快照/历史 mid）。
            // epoch 取 dispatch 时 history_lock 内的快照（与用户轮写盘同一代际）
            // ——此后任何 /new 都会 bump 代际，回复到达时按失配「只发不写历史」
            // （孤儿闸，与 CLI same_session=false 同语义，见 bridge::buzzreply）。
            // push-登记间崩溃 = 回复走 chat 兜底关联（回合表无登记，仍发仍写）。
            self.turn_registry.lock().unwrap().insert(
                channel_id,
                crate::bridge::buzzreply::TurnEntry {
                    mid: ev.mid.clone(),
                    key: key.clone(),
                    epoch: hist_epoch,
                    typing_rid,
                    typing_mid,
                },
            );
            // mark_started 是防重复注入的主闸（resume 轮不再注入）；marker 与 CLI
            // 成功路径同形（注入轮的迁移标记）。mark_started_if 的槽位校验天然挡
            // 与 /new 的竞态；marker 错写旧 sid 也会因失配下轮重注入（自愈）。
            if sessions.mark_started_if(&key, &session_id) && injected_rounds.is_some() {
                hist.set_marker(&session_id, "buzz", false);
            }
            return;
        }
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
        // 会话存储与工作目录已在函数前部按 chat 路由（#194：vb/<uuid>/ 或 bot 级）。
        let runner = self.agent_runner.clone();
        let result = self
            .run_agent_with_progress(
                &runner,
                &prompt,
                &session_id,
                resume,
                &ev.chat_id,
                &key,
                &bot_key,
                ev.role,
                &sessions,
                &cancel_flag,
            )
            .await;
        //（P4.1：#130 自动上下文压缩重试臂已随 contextsum 模块一并删除——单执行层后
        // 生产不可达的死码；上下文超长错误照旧返回给上层。）
        // 任务结束 → 摘掉打断标志（后续停止词将按普通消息处理）
        self.cancel_flags.lock().unwrap().remove(&key);

        // 统一只发最终结果一条（中途进度已在 select 循环丢弃）。
        match result {
            Ok(agent::RunOutcome::Reply {
                reply,
                session_id: final_sid,
                rebuilt,
            }) => {
                // agent 成功即标记 started（会话状态只跟 agent 跑没跑成有关，与投递无关）。
                // #23：仅当当前槽位仍是本次任务的会话时才 mark——运行中被 /new 或
                // CLI `session reset` 换走时跳过（旧任务完成不得把新槽位置回 started=true）。
                // #49：同一道闸决定历史落盘——换走后不写孤儿助手条目、不写迁移标记
                // （历史已被 /new 清空，旧任务的回复不得写回去）。
                let same_session = sessions.mark_started_if(&key, &final_sid);
                if same_session {
                    // 代际闸：/new 恰好落在 mark 与写盘之间（亚毫秒窗口）也不残留孤儿条目
                    let guard = hist_epoch_lock.lock().unwrap_or_else(|e| e.into_inner());
                    if *guard == hist_epoch {
                        hist.append_assistant(&ev.mid, "buzz", &reply);
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
                            hist.set_marker(&final_sid, "buzz", false);
                        } else if rebuilt {
                            hist.set_marker(&final_sid, "buzz", true);
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
                                &ev.chat_type,
                                &ev.chat_name,
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

    /// #194：chat 的会话存储——虚拟 Bot 群用其独立工作区的 sessions.json（按 chat
    /// 缓存实例），其余会话用 bot 级存储。按值返回（SessionStore 手写 Clone=句柄
    /// 拷贝，文件是唯一事实源，新实例首次使用 refresh 从盘加载）。
    fn sessions_for(&self, chat_id: &str) -> SessionStore {
        let Some(dir) = crate::virtualbot::ensure_vb_dir(&self.bot.key(), chat_id) else {
            return self.sessions.clone();
        };
        let mut cache = self.vb_sessions.lock().unwrap();
        cache
            .entry(chat_id.to_string())
            .or_insert_with(|| {
                let store = crate::sessions::SessionStore::at(dir.join("sessions.json"));
                // 进程内首建 = 服务启动后的首次使用：同 bot 级存储，复位槽位
                // 让本进程首轮走注入闸（harness 会话不跨进程存活）。
                store.reset_slots_for_service_start();
                store
            })
            .clone()
    }

    /// #194：chat 的工作目录——虚拟 Bot 群 = vb/<uuid>/，其余 = bot 工作区。
    fn workspace_for(&self, chat_id: &str) -> std::path::PathBuf {
        crate::virtualbot::ensure_vb_dir(&self.bot.key(), chat_id)
            .unwrap_or_else(|| crate::workspace_dir(&self.bot.key()))
    }

    /// #206：buzz /cancel——预检（频道已登记；buzz 未启用则拒——与 dispatch 同
    /// 判据的前两条）→ 向 harness 的**目标频道**发 Cancel 信号（同步回执）。
    ///
    /// 叫停粒度（#206 话题隔离后）：与 CLI 的 chat:thread key 对齐——话题内的
    /// /cancel 停**话题频道**的在跑轮次（话题消息已 dispatch 进独立话题频道，
    /// 群根频道的 cancel 停不到话题频道的在跑轮次）；群顶层的 /cancel 只停群根
    /// 频道轮次。话题频道从未登记 = 该话题从未 dispatch 过 = 无轮次可停 → 如实回
    /// 「没有正在运行的任务」。
    ///
    /// 回执必须诚实：cancel 是**同步信号 + 同步回执**（harness 主循环在命令通道
    /// 内应答）——`Ok(true)` = 已向在跑任务发 Cancel 信号；`Ok(false)` = 没有
    /// 在跑任务（队列里等着的消息不在取消范围——取消只停正在跑的那一轮，不吞
    /// 用户还没被受理的消息）。桥侧无轮次记账，无法在「信号已发」之外再确认
    /// agent 是否真停了——文案只如实说「叫停指令已发送」。
    ///
    /// 明示后果：有在跑轮次 → 向 agent 发 cancel（pi-acp 无原生 steer，回合终止
    /// 走 cancel+merge 回退：该轮文本作废不投递、频道 session 失效），下条消息
    /// 以新会话续跑。被叫停轮次不重发、不回复。
    ///
    /// 与 CLI 一致无角色门槛：任何能发言的 IM 用户都可叫停。!shutdown/!rotate
    /// 等 harness 无此面（cancel 是唯一暴露给聊天的控制信号）。
    async fn buzz_cancel_reply(&self, ev: &Ev) -> String {
        let Some(handle) = self.acp_handle_for_role(ev.role) else {
            return "⚠️ 叫停未送达：harness 未装配（检查服务状态/重启）。".to_string();
        };
        let channel_id = Uuid::parse_str(&if ev.thread_id.is_empty() {
            crate::buzz::keys::channel_uuid(&self.bot.key(), &ev.chat_id)
        } else {
            crate::buzz::keys::topic_channel_uuid(&self.bot.key(), &ev.chat_id, &ev.thread_id)
        })
        .expect("channel_uuid output must parse as Uuid");
        // 频道从未登记 = 从未 dispatch 过 = 无轮次可停（话题与群根同判据——
        // 登记先于 dispatch 发生，见 buzz_ensure_topic_channel / 巡检登记）。
        if !handle.channel_registered(&channel_id) {
            return "✅ 当前没有正在运行的任务。".to_string();
        }
        match handle.cancel(channel_id).await {
            // Ok(false) = 无在跑任务（消息还在队列/空闲）；如实回执，不空发 no-op
            Some(false) => "✅ 当前没有正在运行的任务。".to_string(),
            Some(true) => {
                let scope = if ev.thread_id.is_empty() {
                    "本群顶层频道"
                } else {
                    "本话题"
                };
                format!(
                    "⏹ 已向 buzz 发送叫停指令（{scope}生效）。\
                     有轮次在跑时将在数秒内停止——被叫停的轮次不会回复也不会自动重发，\
                     会话将重新开始；无轮次在跑时本指令无效果。"
                )
            }
            None => "⚠️ buzz 叫停未送达（harness 已关闭），请重试。".to_string(),
        }
    }

    /// 执行一轮 agent 任务（统一中途进度排空：打字机已下线，中途输出丢弃不回，
    /// 任务结束只发最终结果一条）。
    #[allow(clippy::too_many_arguments)] // 与 AgentRunner::run 同款参数集（10 参）
    async fn run_agent_with_progress(
        &self,
        runner: &std::sync::Arc<dyn crate::agent::AgentRunner>,
        prompt: &str,
        session_id: &str,
        resume: bool,
        chat_id: &str,
        key: &str,
        bot_key: &str,
        role: crate::config::SenderRole,
        sessions: &SessionStore,
        cancel_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<crate::agent::RunOutcome, String> {
        let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let run_fut = runner.run(
            prompt,
            session_id,
            resume,
            chat_id,
            key, // 会话隔离 key（话题=chat:thread，#14）：session 存储按 key 记账，回存须同 key
            bot_key,
            role, // 发送者角色：granted 走受限分支（restrict 判定在 AgentRunner::run 内热读）
            Some(sessions),
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
                // 确认 = git 快照 + 移入回收站（libgit2 首轮全量 add 可能秒级），
                // spawn_blocking 让出 tokio worker（审查 P2-1；与 service/tidy 同口径）
                let key = self.bot.key();
                let ws = workspace.clone();
                match tokio::task::spawn_blocking(move || {
                    crate::guard::confirm_dangerous_delete(&key, &ws, &path)
                })
                .await
                {
                    Ok(Ok(it)) => {
                        // 批次 5 回执（审查 P3-3）：与 guard/CLI 同口径如实展示恢复途径
                        let git_enabled = crate::config::Config::load()
                            .map(|c| c.workspace_git_enabled)
                            .unwrap_or(true);
                        let rels: Vec<&std::path::Path> = std::path::Path::new(&it.orig)
                            .strip_prefix(&workspace)
                            .ok()
                            .map(|p| vec![p])
                            .unwrap_or_default();
                        let prot = crate::wsver::prot_phrase(
                            &workspace,
                            it.snapshot.as_deref(),
                            git_enabled,
                            &rels,
                        );
                        format!(
                            "✅ 已确认并移入回收站：{}（{} 天内可恢复，{prot}；/trash restore 可撤回）",
                            it.orig,
                            self.bot.trash_ttl_days.max(1)
                        )
                    }
                    Ok(Err(e)) => format!("⚠️ 确认失败：{e}"),
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
