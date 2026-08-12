//! 桥 —— 事件过滤 + 粘性后端路由 + per-chat 串行 + 表情生命周期。
//! 通道无关：通过 `Messenger` trait 收发（飞书 / 微信 / 钉钉），飞书事件解析在 on_payload，
//! 微信消息在 on_weixin，钉钉消息在 on_dingtalk。零 regex（路由/@_user_ 手工解析）。

use crate::agent::{self, AgentRunner, Backend};
use crate::config::{BotConfig, Config};
use crate::messenger::Messenger;
use crate::outbox::{OutboxItem, OutboxStore};
use crate::pending::{PendingItem, PendingStore};
use crate::schedule::JobStore;
use crate::sessions::SessionStore;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub struct Bridge {
    pub msgr: Arc<dyn Messenger>,
    pub sessions: SessionStore,
    pub jobs: JobStore,
    /// 本 bot 的配置（app_id/bot_name/bot_open_id/primary_chat_id/wx_*…）
    pub bot: BotConfig,
    /// 全局：只响应这个 owner（飞书 open_id）
    pub owner_open_id: Mutex<String>,
    /// 全局：默认后端（SessionStore 已按它初始化；字段保留以便将来逐 bot 覆盖）
    #[allow(dead_code)]
    pub default_backend: String,
    seen: Mutex<HashSet<String>>,
    /// per-chat 串行锁：同一 chat 的并发消息在此**排队**等前一条处理完再跑（而非丢弃）。
    /// 每个 chat_id 一把 tokio 异步锁；锁 Arc 从 std Mutex 的 HashMap 取出后再 await，
    /// 不跨 await 持有 std 锁。
    chat_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 在跑任务的打断标志：chat_id → AtomicBool。「停止词」到达时置 true 叫停该 chat 正在跑的任务。
    /// （per-chat 同一时刻只有一个在跑任务，故每 chat 至多一个标志。）
    cancel_flags: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    /// 微信待发积压（pending_outbox）：主动推送被微信拒绝（ret=-2 token stale）时落盘，
    /// 等用户下一条入站刷新 context_token 后补发。非微信 bot 空置。
    outbox: OutboxStore,
    /// 待处理消息持久化（#25 重启恢复）：进入 agent 前落盘、完成后删除；
    /// service 重启后 recover_pending 自动重放，续跑上次未完成的消息/会话。
    pending: PendingStore,
    /// Agent 执行器（#23 测试可测性）：仿 `msgr` 的 trait 注入——生产用 RealAgentRunner
    /// 转发 spawn 子进程，测试注入挡板以驱动「任务运行中」时序（详见 agent::AgentRunner）。
    agent_runner: Arc<dyn AgentRunner>,
}

#[derive(Debug)]
pub struct Ev {
    pub mid: String,
    pub chat_id: String,
    /// 会话类型：飞书 p2p/group，微信 dm（主会话候选判定用）
    pub chat_type: String,
    /// 飞书话题 ID（omt_ 开头）；空=非话题消息。微信/钉钉恒为空。
    pub thread_id: String,
    /// 被引用消息的内容（引用/回复场景：用户引用上一条消息再 @ bot 时，
    /// 把被引用文本 + 附件带进 agent prompt）。空=无引用。
    pub quoted: crate::messenger::QuotedContent,
    pub text: String,
    /// 已下载到工作区的附件元数据（#12）。纯附件消息 text 为空但 attachments 非空。
    pub attachments: Vec<crate::attachments::AttachmentMeta>,
}

impl Ev {
    /// 会话隔离 key：话题消息用 `{chat_id}:{thread_id}`（#14），非话题就是 chat_id。
    /// sessions / chat_lock / cancel_flags 全部按这个 key 隔离——同一群不同话题
    /// 各自独立 agent 会话、互不带上下文；老 sessions.json 键不变（话题是新键）。
    pub fn key(&self) -> String {
        if self.thread_id.is_empty() {
            self.chat_id.clone()
        } else {
            format!("{}:{}", self.chat_id, self.thread_id)
        }
    }
}

impl Bridge {
    pub fn new(msgr: Arc<dyn Messenger>, bot: BotConfig, cfg: &Config) -> Bridge {
        Self::build(msgr, bot, cfg, Arc::new(agent::RealAgentRunner))
    }

    /// 实际构造器：生产（`new` 用真实 `RealAgentRunner`）与测试（注入挡板 `AgentRunner`
    /// 驱动「任务运行中」时序）共用。字段初始化集中在此。
    fn build(
        msgr: Arc<dyn Messenger>,
        bot: BotConfig,
        cfg: &Config,
        agent_runner: Arc<dyn AgentRunner>,
    ) -> Bridge {
        // 后端跟着 bot 走：用该 bot 的生效后端（自身 backend 非空优先，否则回落全局默认）。
        let effective = bot.effective_backend(&cfg.default_backend).to_string();
        let key = bot.key();
        let sessions = SessionStore::new(&effective, &key);
        Bridge {
            msgr,
            sessions,
            jobs: JobStore::new(&bot.key()),
            // owner 也是 per-bot（飞书 bot 各自配，微信用 wx_user_id 不走这）；空则回落全局 owner_open_id。
            owner_open_id: Mutex::new(bot.effective_owner(&cfg.owner_open_id).to_string()),
            default_backend: effective,
            bot,
            seen: Mutex::new(HashSet::new()),
            chat_locks: Mutex::new(HashMap::new()),
            cancel_flags: Mutex::new(HashMap::new()),
            outbox: OutboxStore::new(&key),
            pending: PendingStore::new(&key),
            agent_runner,
        }
    }

    /// 取（或新建）某 chat 的串行锁 Arc。同一 chat 的并发任务拿同一把锁 → 排队等前一条处理完。
    fn chat_lock(&self, chat_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.chat_locks.lock().unwrap();
        locks
            .entry(chat_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 发一条回复：话题消息走 reply 接口（落在原话题），非话题走普通发送。
    async fn send_reply(&self, ev: &Ev, text: &str) -> anyhow::Result<()> {
        if ev.thread_id.is_empty() {
            self.msgr.send_text(&ev.chat_id, text).await
        } else {
            self.msgr
                .send_thread_reply(&ev.chat_id, &ev.mid, text)
                .await
        }
    }

    /// 把一条待发消息落盘积压（仅微信：主动推送被拒时缓存，等下次入站补发）。
    /// 其它通道主动推送不受 token 活跃度约束，继续走既有「失败回落主会话」路径，不入队。
    pub fn queue_outbox(&self, chat_id: &str, text: &str, job_id: &str) {
        if !self.bot.is_wechat() || chat_id.is_empty() || text.is_empty() {
            return;
        }
        self.outbox.add(OutboxItem {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            text: text.to_string(),
            created_at: crate::chrono_lite::unix_secs(),
            attempts: 0,
            job_id: job_id.to_string(),
        });
        crate::log!(
            "[bot:{}] [outbox] 任务报告写入待发积压 chat={} 长度={}（当前积压 {} 条）",
            self.bot.key(),
            trunc(chat_id, 10),
            text.chars().count(),
            self.outbox.len()
        );
    }

    /// 微信入站刷新 context_token 后调用：把该 chat 的积压消息一次性补发。
    /// 与 handle 共用 per-chat 串行锁，避免补发与消息处理交错；失败的项保留待下次入站再试。
    pub async fn flush_outbox(&self, chat_id: &str) {
        if !self.bot.is_wechat() {
            return;
        }
        let lock = self.chat_lock(chat_id);
        let _guard = lock.lock().await;
        crate::outbox::flush_pending(self.msgr.as_ref(), &self.outbox, chat_id).await;
    }

    /// 群消息 mentions 里是否 @了本机器人（name/app_id/open_id 三重冗余）。
    fn bot_is_mentioned(&self, mentions: &[serde_json::Value]) -> bool {
        for m in mentions {
            // name 命中
            if m.get("name").and_then(|x| x.as_str()) == Some(self.bot.bot_name.as_str())
                && !self.bot.bot_name.is_empty()
            {
                return true;
            }
            // id.app_id / id.open_id 命中
            if let Some(id) = m.get("id") {
                if id.get("app_id").and_then(|x| x.as_str()) == Some(self.bot.app_id.as_str())
                    && !self.bot.app_id.is_empty()
                {
                    return true;
                }
                if id.get("open_id").and_then(|x| x.as_str()) == Some(self.bot.bot_open_id.as_str())
                    && !self.bot.bot_open_id.is_empty()
                {
                    return true;
                }
            }
            // 顶层 open_id 命中
            if m.get("open_id").and_then(|x| x.as_str()) == Some(self.bot.bot_open_id.as_str())
                && !self.bot.bot_open_id.is_empty()
            {
                return true;
            }
        }
        false
    }

    /// DATA 帧 payload 入口：解析 v2 事件 → 过滤 → handle。
    pub async fn on_payload(&self, payload: &[u8]) {
        let body: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(_) => return,
        };
        let event_type = body["header"]["event_type"].as_str().unwrap_or("");
        if event_type != "im.message.receive_v1" {
            return; // 只处理消息接收事件，其它（reaction/task…）忽略
        }
        let event = &body["event"];
        let sender = &event["sender"];
        let message = &event["message"];

        let sender_id = sender["sender_id"]["open_id"].as_str().unwrap_or("");
        let sender_type = sender["sender_type"].as_str().unwrap_or("");

        // 忽略机器人/应用发的消息（防自我回复死循环）：
        // 飞书事件里应用（含本 bot）发的消息 sender_type="app"（不是 "bot"），用户消息才是 "user"；
        // 只判 "bot" 会把 app 消息当用户输入透传——bot 自己的回复 echo 回来会再触发一轮 agent
        // （自说自话），owner 未配置时还会把 app open_id 误设为 owner 导致真用户被忽略。
        // 双保险：sender open_id 与本 bot 相同也一律丢弃（即使 sender_type 缺失/变化，见
        // openclaw #90559/#90572 同类自回声问题）。
        let is_self = !self.bot.bot_open_id.is_empty() && sender_id == self.bot.bot_open_id;
        if matches!(sender_type, "app" | "bot") || is_self {
            crate::log!(
                "[bridge] 忽略应用/bot 消息（bot={} sender_type={:?} sender={} is_self={}）",
                self.bot.key(),
                sender_type,
                trunc(sender_id, 12),
                is_self
            );
            return;
        }
        let chat_type = message["chat_type"].as_str().unwrap_or("");
        let thread_id = message["thread_id"].as_str().unwrap_or("");
        let mentions: Vec<serde_json::Value> =
            message["mentions"].as_array().cloned().unwrap_or_default();

        if chat_type == "group" {
            crate::log!("[群] bot@={}", self.bot_is_mentioned(&mentions));
        }

        // should_respond：owner 未配置时，把第一条私聊（p2p）的发送者自动设为 owner 并落盘，
        // 之后只响应 ta（群聊不自动学习，避免随便一个群成员认领）。
        {
            let mut owner = self.owner_open_id.lock().unwrap();
            if owner.is_empty() && chat_type == "p2p" && !sender_id.is_empty() {
                *owner = sender_id.to_string();
                crate::log!(
                    "[bridge] 未配置 owner，自动把首个私聊用户设为 owner（bot={}）: {}",
                    self.bot.key(),
                    sender_id
                );
                crate::config::Config::save_owner(&self.bot.key(), sender_id);
            }
            if sender_id != *owner {
                if !owner.is_empty() {
                    crate::log!(
                        "[bridge] 忽略非 owner 消息（bot={} sender={} chat_type={}）",
                        self.bot.key(),
                        sender_id,
                        chat_type
                    );
                } else if chat_type != "p2p" {
                    crate::log!(
                        "[bridge] owner 未配置且非私聊，忽略（bot={} chat_type={} sender={}）；请先私聊 bot 自动设置 owner",
                        self.bot.key(),
                        chat_type,
                        sender_id
                    );
                }
                return;
            }
        }
        // 群聊只有 @ 了本机器人（或话题内回复）的消息才处理：
        // 话题（thread）内用户回复机器人的消息不需要再次 @——这是「用户回复」的主流交互；
        // 顶层群消息仍要求 @（避免整个群的消息都进 agent）。
        if chat_type == "group" && thread_id.is_empty() && !self.bot_is_mentioned(&mentions) {
            crate::log!(
                "[bridge] 群聊未 @ 机器人，忽略（bot={} chat={} sender={}）",
                self.bot.key(),
                message["chat_id"].as_str().unwrap_or(""),
                sender_id
            );
            return;
        }

        // content 是 JSON 字符串：文本 / 图片 / 文件 / 音视频 / 富文本（#12）。
        // 非文本消息解析出资源引用后下载附件，text 保持空（不再把 raw JSON 当文本透传）。
        let raw = message["content"].as_str().unwrap_or("");
        let parsed = crate::feishu::parse_content(raw);
        let text = parsed.text.trim().to_string();
        let mid = message["message_id"].as_str().unwrap_or("").to_string();
        let mut attachments = Vec::new();
        if let Some(res) = parsed.resource {
            let desc = crate::attachments::AttachmentDesc::Feishu {
                message_id: mid.clone(),
                file_key: res.file_key,
                kind: res.kind.clone(),
                file_name: res.file_name,
            };
            if let Some(meta) = self
                .msgr
                .download_attachment(&self.bot.key(), &mid, 0, &desc)
                .await
            {
                attachments.push(meta);
            }
        }

        let mut ev = Ev {
            mid,
            chat_id: message["chat_id"].as_str().unwrap_or("").to_string(),
            chat_type: chat_type.to_string(),
            // 话题消息事件体带 thread_id（omt_ 开头）；非话题不返回该字段 → 空
            thread_id: thread_id.to_string(),
            quoted: crate::messenger::QuotedContent::default(),
            text,
            attachments,
        };
        if ev.mid.is_empty() || ev.chat_id.is_empty() {
            crate::log!(
                "[bridge] 消息缺 mid/chat_id，忽略（bot={} mid={} chat={}）",
                self.bot.key(),
                ev.mid,
                ev.chat_id
            );
            return;
        }

        // 引用/回复场景：parent_id 是被引用（回复）的消息 id。事件体不带被引用内容，
        // 需按 id 拉取（飞书 API，文本 + 资源引用），再下载引用附件；best-effort——
        // 拉取/下载失败只记日志，不阻塞本条回复。
        let parent_id = message["parent_id"].as_str().unwrap_or("");
        if !parent_id.is_empty() && parent_id != ev.mid {
            match self.msgr.get_quoted_message(parent_id).await {
                Some(q) => {
                    let mut quoted = crate::messenger::QuotedContent {
                        text: q.text.trim().to_string(),
                        attachments: Vec::new(),
                    };
                    for (i, desc) in q.attachments.into_iter().enumerate() {
                        if let Some(meta) = self
                            .msgr
                            .download_attachment(&self.bot.key(), &ev.mid, 100 + i, &desc)
                            .await
                        {
                            quoted.attachments.push(meta);
                        }
                    }
                    if !quoted.text.is_empty() || !quoted.attachments.is_empty() {
                        ev.quoted = quoted;
                        crate::log!(
                            "[bridge] 引用消息 parent={} 文本长度={} 附件数={}",
                            trunc(parent_id, 12),
                            ev.quoted.text.chars().count(),
                            ev.quoted.attachments.len()
                        );
                    }
                }
                None => crate::log!(
                    "[bridge] 拉取引用消息失败/无内容 parent={}",
                    trunc(parent_id, 12)
                ),
            }
        }
        self.handle(ev).await;
    }

    async fn handle(&self, ev: Ev) {
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

        // 打断拦截：停止词 → 叫停该 chat 正在跑的任务。必须在拿串行锁**之前**判断，
        // 否则会被排到运行中任务之后，等任务跑完才处理（那时打断就没意义了）。
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
            let new_sid = self.sessions.reset_session(&key);
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
            created_at: crate::chrono_lite::unix_secs(),
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
        let (session_id, resume) = self.sessions.ensure_with_started(&key);

        let typing_rid = self.msgr.typing(&ev.mid).await;

        // 流式执行：agent 边跑边把中途完整消息推进 progress 通道，这里即时转发到聊天（不等跑完）；
        // cancel flag 注册进 cancel_flags，供该 chat 后续「停止词」消息叫停。
        let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<String>();
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
        let run_fut = runner.run(
            backend,
            &prompt,
            &session_id,
            resume,
            &ev.chat_id,
            &bot_key,
            Some(&self.sessions),
            Some(ptx),
            Some(cancel_flag.clone()),
        );
        tokio::pin!(run_fut);
        let result = loop {
            tokio::select! {
                Some(p) = prx.recv() => {
                    if let Err(e) = self.send_reply(&ev, &p).await {
                        crate::log!(
                            "[bridge] ⚠️ 中途进度发送失败 chat={}: {e:#}",
                            trunc(&ev.chat_id, 10)
                        );
                    }
                }
                r = &mut run_fut => { break r; }
            }
        };
        // 任务结束 → 摘掉打断标志（后续停止词将按普通消息处理）
        self.cancel_flags.lock().unwrap().remove(&key);

        // #25：agent 已返回（Reply/Cancelled/Err 均视为「任务完成」）→ 摘掉 pending，
        // 避免重启后重复执行已完成的任务；回复发送失败仍走既有路径（日志/outbox），不重跑。
        self.pending.remove(&ev.mid);

        match result {
            Ok(agent::RunOutcome::Reply { reply, session_id }) => {
                // agent 成功即标记 started（会话状态只跟 agent 跑没跑成有关，与投递无关）。
                // #23：仅当当前槽位仍是本次任务的会话时才 mark——运行中被 /new 或
                // CLI `session reset` 换走时跳过（旧任务完成不得把新槽位置回 started=true）。
                self.sessions.mark_started_if(&key, &session_id);
                // 发送结果必须留痕：回复丢了（token 失效/会话失效等）时不能谎报成功。
                match self.send_reply(&ev, &reply).await {
                    Ok(()) => crate::log!(
                        "[bridge] 已回复 chat={} 长度={}",
                        trunc(&ev.chat_id, 10),
                        reply.chars().count()
                    ),
                    Err(e) => crate::log!(
                        "[bridge] ⚠️ 回复发送失败 chat={}: {e:#}",
                        trunc(&ev.chat_id, 10)
                    ),
                }
            }
            Ok(agent::RunOutcome::Cancelled) => {
                crate::log!("[bridge] 任务被打断 chat={}", trunc(&ev.chat_id, 10));
                let _ = self.send_reply(&ev, "⏹ 已停止").await;
                // 不 mark_started：被打断的轮次不算完成
            }
            Err(e) => {
                // 错误文案作为回复发出（用户可见原因），同样留痕
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

    /// 启动恢复（#25）：扫描 pending.json 残留（=上次崩溃/重启时未完成的消息），
    /// 自动重放进 handle 续跑——sessions.json 已 mark_started 的会话会 resume 原上下文，
    /// 未完成的重新执行；先清理孤儿 agent 子进程避免 resume 撞 already in use。
    /// 恢复是异步任务，不阻塞事件循环启动（多 chat 并发由 per-chat 串行锁保证不乱序）。
    pub async fn recover_pending(&self) {
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
            let ev = Ev {
                mid: item.mid,
                chat_id: item.chat_id,
                chat_type: item.chat_type,
                thread_id: item.thread_id,
                quoted: item.quoted,
                text: item.text,
                attachments: item.attachments,
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
        // should_respond：微信侧 owner 判据是登录拿到的 ilink_user_id（不是飞书 open_id）
        let owner = self.bot.wx_owner();
        if !owner.is_empty() && from != owner {
            crate::log!("[weixin] 忽略非 owner 消息 from={}", trunc(from, 10));
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
            chat_id: from,               // 微信会话标识 = 对方 ilink_user_id
            chat_type: "dm".to_string(), // 微信私聊当 dm（主会话候选）
            thread_id: String::new(),    // 微信无话题
            quoted,
            text,
            attachments,
        };
        self.handle(ev).await;
    }

    /// 钉钉入站消息入口（service 的钉钉 Stream 循环调用）。
    /// msg=解析好的机器人消息；先记群聊最近发送者（回复时 @），过滤后走统一 handle。
    pub async fn on_dingtalk(&self, msg: crate::dingtalk::DingtalkMessage) {
        // should_respond：钉钉 owner 判据是允许的 staffId（ding_user_id；空=不设限）
        let owner = self.bot.ding_owner();
        if !owner.is_empty() && msg.sender_staff_id != owner {
            crate::log!(
                "[dingtalk] 忽略非 owner 消息 from={}",
                trunc(&msg.sender_staff_id, 10)
            );
            return;
        }
        // 群聊只有 @ 了本机器人（或配置了「@ 才推送」）的消息才处理；单聊直接处理
        if msg.is_group() && !msg.mentioned {
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
        };
        self.handle(ev).await;
    }
}

/// 剥掉 "@_user_<数字>" 提及标签（等价 Python 的 re.sub(r"@_user_\d+", "", ...)）。
/// 注意：只有后面跟了至少一个数字才算提及标签；`@_user_`（无数字）原样保留（与 \d+ 一致）。
pub fn strip_mentions(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("@_user_") {
            let mut j = i + "@_user_".len();
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + "@_user_".len() {
                i = j; // 后面确有数字：跳过整个 @_user_N
                continue;
            }
            // 无数字：不是有效提及，原样保留 "@_user_"
            out.push_str("@_user_");
            i += "@_user_".len();
        } else {
            // 按 char 推进（UTF-8 安全）
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 剥掉钉钉群聊文本开头的 "@机器人名"（钉钉推给机器人的群消息 content 会带 @ 前缀；
/// 机器人名未知/没匹配到则原样返回）。只剥一次，避免误伤正文里的同名 @。
pub fn strip_bot_mention(text: &str, bot_name: &str) -> String {
    if bot_name.is_empty() {
        return text.to_string();
    }
    let t = text.trim_start();
    let mention = format!("@{bot_name}");
    if let Some(rest) = t.strip_prefix(mention.as_str()) {
        // 剥掉后面可能跟的空格（钉钉常见 " @机器人名 内容"）
        return rest.trim_start().to_string();
    }
    text.to_string()
}

/// 按字符截断（日志预览用）。`&s[..n]` 按字节切会在多字节 UTF-8 边界 panic——
/// key/chat_id 可能含非 ASCII（话题、群名等），日志一律走字符级。
fn trunc(s: impl AsRef<str>, n: usize) -> String {
    s.as_ref().chars().take(n).collect()
}

/// 识别「/new」会话新建指令（#23）：trim 后精确匹配，大小写不敏感。
/// 只在 handle 里拦截（在透传 agent 之前），其它斜杠命令仍原样透传。
fn is_new_command(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("/new")
}

/// 识别「打断」关键词（整句精确匹配，大小写不敏感）。仅当该 chat 有任务在跑时才生效
/// （由 handle 判断）；否则原样透传给 agent，避免误吞用户正常词汇。
fn is_cancel_keyword(text: &str) -> bool {
    const KEYWORDS: &[&str] = &["停", "停止", "停下", "取消", "stop", "cancel", "/stop", "/cancel"];
    let t = text.trim().to_ascii_lowercase();
    KEYWORDS.iter().any(|k| *k == t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_user_mentions() {
        assert_eq!(strip_mentions("@_user_1 你好"), " 你好");
        assert_eq!(strip_mentions("@_user_123@_user_456 hi"), " hi");
        assert_eq!(strip_mentions("没有提及"), "没有提及");
        assert_eq!(strip_mentions("@_user_ 不完整"), "@_user_ 不完整");
    }

    #[test]
    fn strip_bot_mention_works() {
        assert_eq!(strip_bot_mention("@庆小丰 你好", "庆小丰"), "你好");
        assert_eq!(strip_bot_mention("  @庆小丰  你好", "庆小丰"), "你好");
        // 名字未知/没匹配 → 原样
        assert_eq!(strip_bot_mention("@别人 你好", "庆小丰"), "@别人 你好");
        assert_eq!(strip_bot_mention("@庆小丰 你好", ""), "@庆小丰 你好");
        // 正文中间的同名 @ 不剥
        assert_eq!(strip_bot_mention("你好 @庆小丰", "庆小丰"), "你好 @庆小丰");
    }

    #[test]
    fn ev_key_is_chat_id_for_non_thread() {
        // 非话题消息：key 就是 chat_id（老行为不变，#14 兼容）
        let ev = Ev {
            mid: "om_1".into(),
            chat_id: "oc_group".into(),
            chat_type: "group".into(),
            thread_id: String::new(),
            quoted: crate::messenger::QuotedContent::default(),
            text: "hi".into(),
            attachments: vec![],
        };
        assert_eq!(ev.key(), "oc_group");
    }

    #[test]
    fn ev_key_isolates_threads() {
        // 同一群两个不同话题 → 不同 key（各自独立会话/锁/打断）
        let base = |thread: &str| Ev {
            mid: "om_1".into(),
            chat_id: "oc_group".into(),
            chat_type: "group".into(),
            thread_id: thread.into(),
            quoted: crate::messenger::QuotedContent::default(),
            text: "hi".into(),
            attachments: vec![],
        };
        let a = base("omt_aaa");
        let b = base("omt_bbb");
        assert_eq!(a.key(), "oc_group:omt_aaa");
        assert_eq!(b.key(), "oc_group:omt_bbb");
        assert_ne!(a.key(), b.key(), "不同话题必须不同 key");
    }

    #[test]
    fn new_command_matches_exactly() {
        assert!(is_new_command("/new"));
        assert!(is_new_command(" /new "));
        assert!(is_new_command("/NEW"));
        // 全角空格 U+3000 也属 Unicode White_Space，trim() 会去掉（审查 P4-2 复核：
        // trim 已处理，撤回原先「/new 不识别全角空格」的判断）。
        assert!(is_new_command("\u{3000}/new\u{3000}"));
        assert!(!is_new_command("/new 参数"));
        assert!(!is_new_command("/news"));
        assert!(!is_new_command("new"));
        assert!(!is_new_command(""));
    }

    #[test]
    fn cancel_keywords_match() {
        for k in ["停", "停止", "取消", "stop", "Stop", "STOP", "/stop", "cancel", "/cancel", " 停 "] {
            assert!(is_cancel_keyword(k), "应为停止词: {k:?}");
        }
        for k in ["停下来聊聊", "stop it", "别停", "/stopit", "取消订阅这个服务", ""] {
            assert!(!is_cancel_keyword(k), "不应为停止词: {k:?}");
        }
    }

    // ---- #23 pending_new 竞态测试 ----
    // bridge 的 handle 编排（/new 拦截 + 任务结束跳过 mark_started）跨多个 await 点，
    // 必须靠可注入的 AgentRunner 挡板才能驱动「任务运行中」时序。MockMessenger 收 send_text，
    // MockAgentRunner 用 Notify 控制 run 何时返回。每个测试用唯一 bot key 隔离 ~/.agent-bridge
    // 工作目录、末尾清理（不设 HOME：多测试并行 set_var 是 UB——见 LESSON）。

    use async_trait::async_trait;
    use tokio::sync::Notify;

    /// 收集 send_text 调用，供断言回复内容；get_quoted_message 用 map 按 message_id 返回
    /// 被引用内容（飞书引用/回复场景测试用）；download_attachment 直接返回占位元数据
    /// （不落盘，测试与工作区解耦）。
    struct MockMessenger {
        sent: Mutex<Vec<String>>,
        quoted: Mutex<std::collections::HashMap<String, crate::messenger::QuotedMessage>>,
    }
    impl MockMessenger {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                quoted: Mutex::new(std::collections::HashMap::new()),
            }
        }
        fn set_quoted(&self, message_id: &str, text: &str) {
            self.quoted.lock().unwrap().insert(
                message_id.to_string(),
                crate::messenger::QuotedMessage {
                    text: text.to_string(),
                    attachments: Vec::new(),
                },
            );
        }
        fn set_quoted_msg(&self, message_id: &str, q: crate::messenger::QuotedMessage) {
            self.quoted
                .lock()
                .unwrap()
                .insert(message_id.to_string(), q);
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl Messenger for MockMessenger {
        async fn send_text(&self, _chat_id: &str, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }
        async fn get_quoted_message(&self, message_id: &str) -> Option<crate::messenger::QuotedMessage> {
            self.quoted.lock().unwrap().get(message_id).cloned()
        }
        async fn download_attachment(
            &self,
            _bot_key: &str,
            _mid: &str,
            _seq: usize,
            desc: &crate::attachments::AttachmentDesc,
        ) -> Option<crate::attachments::AttachmentMeta> {
            let (kind, file_name) = match desc {
                crate::attachments::AttachmentDesc::Feishu { kind, file_name, .. } => {
                    (kind.clone(), file_name.clone())
                }
                crate::attachments::AttachmentDesc::Dingtalk { kind, file_name, .. } => {
                    (kind.clone(), file_name.clone())
                }
                crate::attachments::AttachmentDesc::Wechat(m) => (m.kind.clone(), m.file_name.clone()),
            };
            Some(crate::attachments::AttachmentMeta {
                kind,
                source: "mock".into(),
                file_name,
                mime: "application/octet-stream".into(),
                size: 1,
                path: "/tmp/mock-attachment.bin".into(),
                sha256: "abc".into(),
                note: String::new(),
            })
        }
    }

    /// 挡板 agent：run 进入即 `started.notify_one()`（让测试知道「任务在跑」）；
    /// `block=true` 等 `release.notified()` 才返回（用于「任务运行中穿插 /new」），
    /// `block=false` 立即返回（对照组）。
    struct MockAgentRunner {
        started: Notify,
        release: Notify,
        block: bool,
        reply: String,
        prompts: Mutex<Vec<String>>,
    }
    impl MockAgentRunner {
        fn blocking(reply: &str) -> Self {
            Self {
                started: Notify::new(),
                release: Notify::new(),
                block: true,
                reply: reply.into(),
                prompts: Mutex::new(Vec::new()),
            }
        }
        fn immediate(reply: &str) -> Self {
            Self {
                started: Notify::new(),
                release: Notify::new(),
                block: false,
                reply: reply.into(),
                prompts: Mutex::new(Vec::new()),
            }
        }
        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl AgentRunner for MockAgentRunner {
        #[allow(clippy::too_many_arguments)]
        async fn run(
            &self,
            _backend: Backend,
            prompt: &str,
            session_id: &str,
            _resume: bool,
            _chat_id: &str,
            _bot_key: &str,
            _sessions: Option<&SessionStore>,
            _progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
            _cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
        ) -> Result<agent::RunOutcome, String> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.started.notify_one();
            if self.block {
                self.release.notified().await;
            }
            // 返回本次运行使用的 session_id——bridge 据此做 mark_started_if 身份校验
            Ok(agent::RunOutcome::Reply {
                reply: self.reply.clone(),
                session_id: session_id.to_string(),
            })
        }
    }

    fn test_ev(mid: &str, chat_id: &str, text: &str) -> Ev {
        Ev {
            mid: mid.into(),
            chat_id: chat_id.into(),
            chat_type: "group".into(), // 非 p2p/dm：跳过 save_primary_chat 写盘
            thread_id: String::new(),
            quoted: crate::messenger::QuotedContent::default(),
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    /// 构造带唯一 bot key 的 Bridge（隔离 ~/.agent-bridge/workspaces/<key>/），返回 bridge +
    /// messenger 供断言。调用方负责 `cleanup_bridge(&bridge)`。
    fn build_test_bridge(runner: Arc<dyn AgentRunner>) -> (Arc<Bridge>, Arc<MockMessenger>) {
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        };
        build_test_bridge_with_bot(runner, bot)
    }

    /// 构造带指定 BotConfig 的 Bridge（飞书 on_payload 测试需要 bot_name/bot_open_id/owner）。
    fn build_test_bridge_with_bot(
        runner: Arc<dyn AgentRunner>,
        bot: BotConfig,
    ) -> (Arc<Bridge>, Arc<MockMessenger>) {
        let msgr = Arc::new(MockMessenger::new());
        let bridge = Arc::new(Bridge::build(msgr.clone(), bot, &Config::default(), runner));
        (bridge, msgr)
    }

    /// 构造飞书 receive_v1 事件 payload（content 按官方格式传 JSON 字符串）。
    /// 测试 helper，参数多是构造 JSON 字段所需；与 MockAgentRunner::run 同类允许。
    #[allow(clippy::too_many_arguments)]
    fn feishu_payload(
        mid: &str,
        chat_id: &str,
        chat_type: &str,
        thread_id: &str,
        parent_id: &str,
        sender_type: &str,
        sender_id: &str,
        mentions: &[(&str, &str)], // (name, open_id)
        text: &str,
    ) -> String {
        let content = serde_json::json!({"text": text});
        serde_json::json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {
                    "sender_type": sender_type,
                    "sender_id": {"open_id": sender_id}
                },
                "message": {
                    "message_id": mid,
                    "chat_id": chat_id,
                    "chat_type": chat_type,
                    "thread_id": thread_id,
                    "parent_id": parent_id,
                    "content": content.to_string(),
                    "mentions": mentions.iter().map(|(name, open_id)| {
                        serde_json::json!({"name": name, "id": {"open_id": open_id}})
                    }).collect::<Vec<_>>()
                }
            }
        })
        .to_string()
    }

    fn cleanup_bridge(bridge: &Bridge) {
        // 跑完删整个工作目录：sessions.json / jobs.json / outbox 一并清理
        let _ = std::fs::remove_dir_all(crate::workspace_dir(&bridge.bot.key()));
    }

    #[tokio::test]
    async fn new_during_run_skips_mark_started() {
        // #23 核心不变式：任务运行中发 /new → 槽位换成新 UUID；旧任务完成时
        // mark_started_if(旧 session_id) 与当前槽位不匹配 → 跳过 mark，
        // 新槽位 started 保持 false（否则下一条会误 resume 一个从未运行的新 UUID）。
        // 若把 handle 里的 mark_started_if 改回无条件 mark_started，此测试必红。
        let runner = Arc::new(MockAgentRunner::blocking("done"));
        let (bridge, _msgr) = build_test_bridge(runner.clone());

        // 先建槽位拿到旧 session_id（任务本身会 ensure 同一槽位）
        let sid_before = bridge.sessions.ensure_with_started("oc_x").0;

        // 普通消息触发 agent（挡板挂住等 release）
        let b1 = bridge.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m1", "oc_x", "hello")).await });

        runner.started.notified().await; // agent 已进入「运行中」、持串行锁
        assert!(!bridge.sessions.is_started("oc_x"), "首轮 agent 还没 mark");

        // 任务运行中发 /new（在拿串行锁之前拦截，立即执行，不被运行中任务阻塞）
        bridge.handle(test_ev("m2", "oc_x", "/new")).await;
        let sid_after = bridge.sessions.ensure_with_started("oc_x").0;
        assert_ne!(
            sid_before, sid_after,
            "/new 必须真的换了新 UUID（防测试恒真）"
        );
        assert!(
            !bridge.sessions.is_started("oc_x"),
            "/new 后 started 应为 false"
        );

        // 放行旧任务 → Ok(Reply) 带旧 session_id → mark_started_if 不匹配 → 跳过 mark
        runner.release.notify_one();
        task.await.unwrap();

        assert!(
            !bridge.sessions.is_started("oc_x"),
            "任务运行中发 /new：旧任务完成不得把新槽位 mark 回 started=true"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn cli_reset_during_run_skips_mark_started() {
        // #23 审查修复：CLI `session reset`（跨进程，等效直接改 sessions.json）发生在任务
        // 运行中 → 旧任务完成时 mark_started_if 不匹配 → 不得把新槽位 mark 回 started=true。
        let runner = Arc::new(MockAgentRunner::blocking("done"));
        let (bridge, _msgr) = build_test_bridge(runner.clone());

        let b1 = bridge.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m1", "oc_r", "hello")).await });
        runner.started.notified().await;
        let sid_before = bridge.sessions.ensure_with_started("oc_r").0;

        // 模拟 CLI 在另一进程 reset（服务内存不知道，但 refresh 会读到新文件；这里直接
        // 调同款 reset_session 等价于外部写盘后的状态）
        let sid_after = bridge.sessions.reset_session("oc_r");
        assert_ne!(sid_before, sid_after);

        runner.release.notify_one();
        task.await.unwrap();

        assert!(
            !bridge.sessions.is_started("oc_r"),
            "CLI reset 运行中：旧任务完成不得把新槽位 mark 回 started=true"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn task_complete_without_new_marks_started() {
        // 对照组：无 /new 时任务完成应正常 mark_started。验证挡板基础设施正确，
        // 防 new_during_run_skips_mark_started 因别的原因恒真（RULE_修复规范：守卫要见过红）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, msgr) = build_test_bridge(runner);

        bridge.handle(test_ev("m1", "oc_y", "hello")).await;

        assert!(
            bridge.sessions.is_started("oc_y"),
            "无 /new 时任务完成应 mark_started"
        );
        assert!(
            msgr.sent().iter().any(|t| t == "done"),
            "agent 回复应经 Messenger 发出"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn new_when_idle_does_not_break_next_mark() {
        // /new 在无任务时发出 → 换新 UUID；下一条消息用新 session_id 跑 → 正常 mark_started
        // （mark_started_if 按会话身份匹配，无需标记清理路径）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, _msgr) = build_test_bridge(runner);

        bridge.handle(test_ev("m1", "oc_z", "/new")).await; // 无任务 → reset 换新 UUID
        assert!(!bridge.sessions.is_started("oc_z"));

        bridge.handle(test_ev("m2", "oc_z", "hello")).await; // 新会话 → 跑 → mark
        assert!(
            bridge.sessions.is_started("oc_z"),
            "新会话完成应正常 mark_started"
        );
        cleanup_bridge(&bridge);
    }

    // ---- #25 重启恢复（in-flight 消息持久化 + 自动重放）----

    #[tokio::test]
    async fn handle_persists_pending_while_running_and_removes_after() {
        // 消息进入 agent 处理时 pending.json 有该条；agent 返回后摘除（不重复执行）。
        let runner = Arc::new(MockAgentRunner::blocking("done"));
        let (bridge, _msgr) = build_test_bridge(runner.clone());

        let b1 = bridge.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m1", "oc_p1", "hello")).await });
        runner.started.notified().await; // agent 在跑 → pending 应已落盘
        assert_eq!(bridge.pending.len(), 1, "任务运行中 pending 应有 1 条");
        assert_eq!(bridge.pending.snapshot()[0].mid, "m1");

        runner.release.notify_one();
        task.await.unwrap();
        assert!(bridge.pending.is_empty(), "任务完成 pending 应清空");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn control_commands_not_persisted() {
        // /new 与停止词是即时控制指令，不落盘：崩溃后重放不会把停止词当普通消息透传。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, _msgr) = build_test_bridge(runner);

        bridge.handle(test_ev("m1", "oc_p2", "/new")).await;
        assert!(bridge.pending.is_empty(), "/new 不应落盘");

        // 有任务在跑时发停止词 → 不新增 pending（原任务那条还在）
        let runner2 = Arc::new(MockAgentRunner::blocking("done"));
        let (bridge2, _msgr2) = build_test_bridge(runner2.clone());
        let b1 = bridge2.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m2", "oc_p3", "hello")).await });
        runner2.started.notified().await;
        assert_eq!(bridge2.pending.len(), 1);
        bridge2.handle(test_ev("m3", "oc_p3", "停止")).await;
        assert_eq!(bridge2.pending.len(), 1, "停止词不应新增 pending");
        runner2.release.notify_one();
        task.await.unwrap();
        assert!(bridge2.pending.is_empty());
        cleanup_bridge(&bridge2);
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn recover_pending_replays_and_clears() {
        // 模拟崩溃残留：pending.json 有两条未完成消息 → recover_pending 按时间顺序
        // 重放进 handle 自动续跑（runner 收到两个 prompt、回复发出、pending 清空）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, msgr) = build_test_bridge(runner.clone());
        bridge.pending.add(crate::pending::PendingItem {
            mid: "r1".into(),
            chat_id: "oc_r1".into(),
            chat_type: "group".into(),
            thread_id: String::new(),
            text: "第一条".into(),
            quoted: crate::messenger::QuotedContent::default(),
            attachments: Vec::new(),
            created_at: 10,
        });
        bridge.pending.add(crate::pending::PendingItem {
            mid: "r2".into(),
            chat_id: "oc_r2".into(),
            chat_type: "group".into(),
            thread_id: String::new(),
            text: "第二条".into(),
            quoted: crate::messenger::QuotedContent::default(),
            attachments: Vec::new(),
            created_at: 20,
        });

        bridge.recover_pending().await;

        assert_eq!(runner.prompts(), ["第一条", "第二条"], "应按入队顺序重放");
        assert!(
            msgr.sent().iter().any(|t| t.contains("正在恢复")),
            "重放前应发恢复提示"
        );
        assert_eq!(
            msgr.sent().iter().filter(|t| *t == "done").count(),
            2,
            "两条消息都应重新处理并回复"
        );
        assert!(bridge.pending.is_empty(), "恢复完成后 pending 应清空");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn recover_pending_empty_is_noop() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, msgr) = build_test_bridge(runner.clone());
        bridge.recover_pending().await;
        assert!(runner.prompts().is_empty(), "无残留不应触发 agent");
        assert!(!msgr.sent().iter().any(|t| t.contains("正在恢复")));
        cleanup_bridge(&bridge);
    }

    // ---- on_payload 过滤（飞书 receive_v1）----

    #[tokio::test]
    async fn on_payload_ignores_app_and_bot_senders() {
        // 飞书事件里应用/bot 发的消息 sender_type 是 "app"/"bot"，不是用户——必须丢弃，
        // 否则 bot 自己的回复 echo 会被当用户输入再跑一轮 agent（自说自话死循环）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        for sender_type in ["app", "bot"] {
            let payload = feishu_payload(
                &format!("om_app_{sender_type}"),
                "oc_p2p",
                "p2p",
                "",
                "",
                sender_type,
                "ou_app",
                &[],
                "你好",
            );
            bridge.on_payload(payload.as_bytes()).await;
        }

        assert!(runner.prompts().is_empty(), "应用/bot 消息不应触发 agent");
        assert!(msgr.sent().is_empty(), "应用/bot 消息不应有回复");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_ignores_self_open_id() {
        // 双保险：即使 sender_type 不是 app/bot（缺失或变化），sender open_id 等于
        // 本 bot 的 open_id 也一律丢弃（openclaw #90559/#90572 同类自回声问题）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let payload = feishu_payload(
            "om_self",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user", // 防御：sender_type 意外是 user 也要靠 open_id 拦住
            "ou_bot",
            &[],
            "你好",
        );
        bridge.on_payload(payload.as_bytes()).await;

        assert!(runner.prompts().is_empty(), "自回声不应触发 agent");
        assert!(msgr.sent().is_empty());
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_group_thread_reply_without_mention_is_handled() {
        // 用户回复机器人的话题消息不需要再次 @：thread_id 非空时跳过群聊 @ 校验
        // （顶层群消息仍要求 @，见下个测试）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let payload = feishu_payload(
            "om_thread",
            "oc_group",
            "group",
            "omt_abc",
            "",
            "user",
            "ou_owner",
            &[], // 话题内回复不 @ 也能收到事件
            "回复一下",
        );
        bridge.on_payload(payload.as_bytes()).await;

        assert_eq!(runner.prompts(), ["回复一下"], "话题内回复应进 agent");
        assert!(msgr.sent().iter().any(|t| t == "done"));
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_group_top_level_without_mention_is_ignored() {
        // 顶层群消息不 @ 机器人仍忽略（老行为不变）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let payload = feishu_payload(
            "om_top",
            "oc_group",
            "group",
            "",
            "",
            "user",
            "ou_owner",
            &[],
            "在吗",
        );
        bridge.on_payload(payload.as_bytes()).await;

        assert!(
            runner.prompts().is_empty(),
            "未 @ 的顶层群消息不应触发 agent"
        );
        assert!(msgr.sent().is_empty());
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_p2p_user_message_is_handled() {
        // 对照组：owner 私聊消息正常进 agent（on_payload 不应误伤用户消息）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let payload = feishu_payload(
            "om_p2p",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_owner",
            &[],
            "在吗",
        );
        bridge.on_payload(payload.as_bytes()).await;

        assert_eq!(runner.prompts(), ["在吗"], "owner 私聊消息应进 agent");
        assert!(msgr.sent().iter().any(|t| t == "done"));
        cleanup_bridge(&bridge);
    }

    // ---- 引用/回复：被引用消息内容进 agent prompt ----

    #[tokio::test]
    async fn on_payload_fetches_quoted_parent() {
        // 飞书引用/回复：事件带 parent_id，on_payload 按 id 拉取被引用内容并拼进 prompt。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        msgr.set_quoted("om_parent", "上面那条被引用的消息");

        let payload = feishu_payload(
            "om_reply",
            "oc_p2p",
            "p2p",
            "",
            "om_parent", // parent_id = 被引用消息
            "user",
            "ou_owner",
            &[],
            "回复内容",
        );
        bridge.on_payload(payload.as_bytes()).await;

        assert_eq!(
            runner.prompts(),
            ["[引用消息]\n上面那条被引用的消息\n\n回复内容"],
            "prompt 应带引用上下文"
        );
        assert!(msgr.sent().iter().any(|t| t == "done"));
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn handle_prompt_prepends_quoted() {
        // 核心拼装：Ev.quoted 非空 → prompt = [引用消息]\n引用内容\n\n用户文本。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, _msgr) = build_test_bridge(runner.clone());
        let mut ev = test_ev("m1", "oc_q", "回复内容");
        ev.quoted = crate::messenger::QuotedContent {
            text: "被引用的原消息".to_string(),
            attachments: Vec::new(),
        };
        bridge.handle(ev).await;
        assert_eq!(
            runner.prompts(),
            ["[引用消息]\n被引用的原消息\n\n回复内容"]
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_weixin_quoted_text_in_prompt() {
        // 微信引用/回复：ref_msg 内容随事件携带，进 prompt。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "wechat".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let msg = crate::wechat::WeixinMessage {
            from_user_id: "u1".into(),
            message_type: 1,
            message_id: "7491".into(),
            item_list: vec![crate::wechat::MessageItem {
                item_type: 1,
                text_item: Some(crate::wechat::TextItem {
                    text: "回复内容".into(),
                }),
                ref_msg: Some(crate::wechat::RefMessage {
                    title: "摘要".into(),
                    message_item: Some(Box::new(crate::wechat::MessageItem {
                        item_type: 1,
                        text_item: Some(crate::wechat::TextItem {
                            text: "被引用的原消息".into(),
                        }),
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        bridge.on_weixin(msg).await;

        assert_eq!(
            runner.prompts(),
            ["[引用消息]\n摘要 | 被引用的原消息\n\n回复内容"]
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_dingtalk_quoted_text_in_prompt() {
        // 钉钉引用/回复：repliedMsg 内容随事件携带，进 prompt。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "dingtalk".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let msg = crate::dingtalk::DingtalkMessage {
            mid: "msg1".into(),
            sender_staff_id: "u1".into(),
            conversation_id: "u1".into(), // 单聊：chat_id = sender
            conversation_type: "1".into(),
            text: "回复内容".into(),
            mentioned: false,
            robot_code: "r".into(),
            quoted_text: "被引用的原消息".into(),
            quoted_attachments: Vec::new(),
            attachments: Vec::new(),
        };
        bridge.on_dingtalk(msg).await;

        assert_eq!(
            runner.prompts(),
            ["[引用消息]\n被引用的原消息\n\n回复内容"]
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_fetches_quoted_attachment() {
        // 飞书引用附件：get_quoted_message 返回资源 desc → 下载成元数据 → [引用附件] 进 prompt。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        msgr.set_quoted_msg(
            "om_parent",
            crate::messenger::QuotedMessage {
                text: "引用的文字".into(),
                attachments: vec![crate::attachments::AttachmentDesc::Feishu {
                    message_id: "om_parent".into(),
                    file_key: "img_1".into(),
                    kind: "image".into(),
                    file_name: "截图.png".into(),
                }],
            },
        );

        let payload = feishu_payload(
            "om_reply",
            "oc_p2p",
            "p2p",
            "",
            "om_parent",
            "user",
            "ou_owner",
            &[],
            "回复内容",
        );
        bridge.on_payload(payload.as_bytes()).await;

        let prompt = &runner.prompts()[0];
        assert!(prompt.contains("[引用消息]"), "应有引用消息段");
        assert!(prompt.contains("引用的文字"));
        assert!(prompt.contains("[引用附件]"), "应有引用附件段");
        assert!(prompt.contains("截图.png"), "附件元数据应带文件名");
        assert!(prompt.contains("本地路径=/tmp/mock-attachment.bin"));
        assert!(prompt.contains("回复内容"));
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_weixin_quoted_media_in_prompt() {
        // 微信引用图片：ref_msg.message_item 媒体 → 下载 → [引用附件] 进 prompt。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "wechat".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let msg = crate::wechat::WeixinMessage {
            from_user_id: "u1".into(),
            message_type: 1,
            message_id: "7491".into(),
            item_list: vec![crate::wechat::MessageItem {
                item_type: 1,
                text_item: Some(crate::wechat::TextItem {
                    text: "回复内容".into(),
                }),
                ref_msg: Some(crate::wechat::RefMessage {
                    title: "摘要".into(),
                    message_item: Some(Box::new(crate::wechat::MessageItem {
                        item_type: 2,
                        image_item: Some(crate::wechat::ImageItem {
                            media: Some(crate::wechat::CDNMedia {
                                encrypt_query_param: "enc".into(),
                                aes_key: "a2V5".into(),
                                full_url: String::new(),
                                ..Default::default()
                            }),
                            aeskey: "00112233445566778899aabbccddeeff".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        bridge.on_weixin(msg).await;

        let prompt = &runner.prompts()[0];
        assert!(prompt.contains("[引用消息]"));
        assert!(prompt.contains("[引用附件]"));
        assert!(prompt.contains("image"));
        assert!(prompt.contains("回复内容"));
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_dingtalk_quoted_attachment_in_prompt() {
        // 钉钉引用图片：repliedMsg 附件 → 下载 → [引用附件] 进 prompt。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "dingtalk".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let msg = crate::dingtalk::DingtalkMessage {
            mid: "msg1".into(),
            sender_staff_id: "u1".into(),
            conversation_id: "u1".into(),
            conversation_type: "1".into(),
            text: "回复内容".into(),
            mentioned: false,
            robot_code: "r".into(),
            quoted_text: String::new(),
            quoted_attachments: vec![crate::dingtalk::DingtalkAttachment {
                kind: "image".into(),
                download_code: "dc_pic".into(),
                file_name: String::new(),
                voice_text: String::new(),
            }],
            attachments: Vec::new(),
        };
        bridge.on_dingtalk(msg).await;

        let prompt = &runner.prompts()[0];
        assert!(prompt.contains("[引用消息]"));
        assert!(prompt.contains("[引用附件]"));
        assert!(prompt.contains("image"));
        assert!(prompt.contains("回复内容"));
        cleanup_bridge(&bridge);
    }
}
