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
    /// 本 bot 的配置（app_id/bot_name/bot_open_id/primary_chat_id/wx_*…）；
    /// 访问控制（owner/授权者/对话权限）也以它为准——生产每次消息从 config.json 热读覆盖
    /// 判断（授权/取消即时生效），config 读不到（单测）时用它当快照。
    pub bot: BotConfig,
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
    /// #49 历史代际（per-key 锁）：同一 key 的注入读、历史写盘、/new 清盘互斥。
    /// /new 不拿 per-chat 锁（不能被运行中任务阻塞），其 clear 与锁内写盘存在交错窗口
    /// （审查 I-2）——写方持本锁校验代际并写盘，/new 持同一锁自增代际并清盘，
    /// 二者互斥：旧代际的写盘要么发生在 clear 之前（被清掉）、要么被闸拦下，不会残留。
    /// per-key（原为全局单锁）：任一 chat 的磁盘写不再阻塞其它 chat 的 /new 与写盘
    /// （审查：全局锁跨文件 I/O 是全 bot 的串行点）。锁内只有快速文件 I/O，
    /// agent 运行期间不持锁（/new 不被运行中任务阻塞的语义保留）。
    history_epochs: Mutex<HashMap<String, Arc<std::sync::Mutex<u64>>>>,
    /// #51 免 @ 开关的测试快照：config.json 里有该 bot 时全走 config 热读/热写；
    /// 读不到该 bot（单测随机 key）→ 回落此内存快照（仿 access_and_role 的
    /// 「load+find+快照回落」模式，保证测试不碰真实 config.json）。
    mention_snapshot: Mutex<HashMap<String, String>>,
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
    /// 发送者角色（owner=全权限 / granted=受限）：入口准入闸推导，agent 调用处
    /// 按此选受限分支；pending 重放路径从 PendingItem.role 恢复。
    pub role: crate::config::SenderRole,
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

    /// 实际构造器：生产（`new` 用真实 `RealAgentRunner`）与测试（注入 `AgentRunner` 挡板
    /// 驱动时序）共用。字段初始化集中在此。
    fn build(
        msgr: Arc<dyn Messenger>,
        bot: BotConfig,
        cfg: &Config,
        agent_runner: Arc<dyn AgentRunner>,
    ) -> Bridge {
        // 后端跟着 bot 走：用该 bot 的生效后端（自身 backend 非空优先，否则回落全局默认）。
        let effective = bot.effective_backend(&cfg.default_backend).to_string();
        let key = bot.key();
        // I2：快照与 access 快照（self.bot）同源——bot 就是 build 时从 config 复制的那份，
        // 直接用它的 mention_modes 种子化，无需再扫 cfg.bots（两份来源可能漂移）。
        let mention_seed = bot.mention_modes.clone();
        let sessions = SessionStore::new(&effective, &key);
        Bridge {
            msgr,
            sessions,
            jobs: JobStore::new(&bot.key()),
            default_backend: effective,
            bot,
            seen: Mutex::new(HashSet::new()),
            chat_locks: Mutex::new(HashMap::new()),
            cancel_flags: Mutex::new(HashMap::new()),
            history_epochs: Mutex::new(HashMap::new()),
            mention_snapshot: Mutex::new(mention_seed),
            outbox: OutboxStore::new(&key),
            pending: PendingStore::new(&key),
            agent_runner,
        }
    }

    /// config 读不到本 bot（单测注入）时，用构造时的访问控制快照判定放行。
    /// 快照 = self.bot（build 时从 config 复制，含 kind 与全部访问字段）。
    fn access_snapshot_allows(&self, sender_id: &str) -> bool {
        self.bot.access_allows(sender_id)
    }

    /// #51 免 @ 开关的「config 优先、快照回落」读取（见 mention_snapshot 字段注释）。
    /// 判定键 = config.json 里有没有该 bot：有 → 生产路径（热读，重启保持）；
    /// 没有（单测随机 key / bot 被改名）或 config 读失败 → 内存快照（最后已知状态）。
    /// 快照与 config 写入路径写穿同步（见 set_mention_mode），config 临时读失败时
    /// 回落的是「最后一次生效的开关状态」而非启动时状态（fail-closed 语义成立）。
    fn mention_mode(&self, chat_id: &str) -> Option<String> {
        match crate::config::Config::load() {
            Ok(c) => match c.bots.into_iter().find(|b| b.key() == self.bot.key()) {
                Some(b) => b.mention_modes.get(chat_id).cloned(),
                None => self.mention_snapshot.lock().unwrap().get(chat_id).cloned(),
            },
            Err(_) => self.mention_snapshot.lock().unwrap().get(chat_id).cloned(),
        }
    }

    /// 写穿快照：无论走 config 还是快照分支，内存快照都同步到本次目标状态。
    fn write_snapshot(&self, chat_id: &str, mode: Option<&str>) {
        let mut m = self.mention_snapshot.lock().unwrap();
        match mode {
            Some(v) => {
                m.insert(chat_id.to_string(), v.to_string());
            }
            None => {
                m.remove(chat_id);
            }
        }
    }

    /// #51 写入开关。返回是否成功（false = config 加载/保存失败，未持久化——
    /// 调用方必须如实回显失败；此时快照仍同步写入，本次运行内行为一致，重启后不保留）。
    /// 成功路径写穿快照：config 与「最后已知状态」同源，config 临时读失败时
    /// 门槛回落的不是启动时快照而是最后一次生效的开关状态（审查 I2 fail-closed 承诺）。
    fn set_mention_mode(&self, chat_id: &str, mode: Option<&str>) -> bool {
        // 写穿先行：任何路径下快照都代表「最后一次尝试的开关状态」
        self.write_snapshot(chat_id, mode);
        match crate::config::Config::set_mention_mode(&self.bot.key(), chat_id, mode) {
            crate::config::MentionModeSave::Saved | crate::config::MentionModeSave::BotNotFound => {
                true
            }
            crate::config::MentionModeSave::Failed => false,
        }
    }

    /// 热读 config 推导（准入, 发送者角色）——on_payload / on_dingtalk 共用同一份
    /// load+find+快照回落，避免两个入口各写一份导致准入与角色推导漂移
    /// （同一发送者在不同通道被推导成不同角色 = 授权者拿到 owner 权限或反之）。
    /// 第三个返回值 = 该 bot 的 mention_modes（config 路径成功时 Some，含空 map——
    /// 空 map 同样是权威判定；config 无该 bot / 读失败时 None，由调用方回落快照）。
    /// 门槛与准入共用这一次 load：未 @ 的顶层群消息不必再整份读一次 config.json。
    fn access_and_role(
        &self,
        sender_id: &str,
    ) -> (
        bool,
        crate::config::SenderRole,
        Option<std::collections::HashMap<String, String>>,
    ) {
        match crate::config::Config::load() {
            Ok(c) => match c.bots.into_iter().find(|b| b.key() == self.bot.key()) {
                Some(b) => (
                    b.access_allows(sender_id),
                    b.sender_role(sender_id),
                    Some(b.mention_modes),
                ),
                None => (
                    self.access_snapshot_allows(sender_id),
                    self.bot.sender_role(sender_id),
                    None,
                ),
            },
            Err(_) => (
                self.access_snapshot_allows(sender_id),
                self.bot.sender_role(sender_id),
                None,
            ),
        }
    }

    /// #51 门槛判定：config 路径有该 bot → 以 config 的 map 为准（无条目 = 需要 @）；
    /// 否则（单测随机 key / 读失败）→ 回落内存快照。
    fn mention_off(
        &self,
        mention: &Option<std::collections::HashMap<String, String>>,
        chat_id: &str,
    ) -> bool {
        match mention {
            Some(m) => m.get(chat_id).map(String::as_str) == Some("off"),
            None => {
                self.mention_snapshot
                    .lock()
                    .unwrap()
                    .get(chat_id)
                    .map(String::as_str)
                    == Some("off")
            }
        }
    }

    /// 尝试把一条消息当作授权码处理（owner 生成后给到对方）。仅 p2p 接受（飞书 p2p / 钉钉单聊，
    /// 群里发码太公开防抢注）；文本精确匹配 pending 码 → 消费并把发送者加入对应白名单
    /// （管理员码→owner / 普通码→授权者，按 bot kind 落位到 open_id 或 staffId 字段）、回发结果。
    /// 返回 true = 授权码消息已消费/回复，调用方应 return（不再进 agent）。
    async fn try_consume_owner_code(
        &self,
        sender_id: &str,
        chat_id: &str,
        is_p2p: bool,
        text: &str,
    ) -> bool {
        let text = text.trim();
        if !is_p2p || text.is_empty() {
            return false;
        }
        use crate::config::OwnerCodeResult as R;
        // 先查发送者展示名（best-effort，查不到用 id 兜底）：随授权一起落盘，GUI 授权列表
        // 能显示「谁」。查名放授权前：失败不阻塞授权。
        let name = self
            .msgr
            .user_display_name(sender_id)
            .await
            .unwrap_or_default();
        let r = crate::config::Config::consume_owner_code(&self.bot.key(), text, sender_id, &name);
        let reply = match r {
            R::Granted => Some("✅ 授权成功，你现在可以在这个 bot 里对话了。"),
            R::Expired => Some("❌ 授权码已过期，请联系管理员重新生成。"),
            R::NotFound => None, // 不是授权码 → 按未授权消息忽略
        };
        let Some(txt) = reply else {
            return false;
        };
        if let Err(e) = self.msgr.send_text(chat_id, txt).await {
            crate::log!(
                "[bridge] 授权码回复发送失败 chat={}: {e:#}",
                trunc(chat_id, 10)
            );
        }
        crate::log!(
            "[bridge] 授权码消息处理完成（bot={} sender={} result={:?}）",
            self.bot.key(),
            sender_id,
            r
        );
        true
    }

    /// 取（或新建）某 chat 的串行锁 Arc。同一 chat 的并发任务拿同一把锁 → 排队等前一条处理完。
    fn chat_lock(&self, chat_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.chat_locks.lock().unwrap();
        locks
            .entry(chat_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// #49：取该 key 的历史代际锁（get-or-create）。同一 key 的注入读、用户轮写盘、
    /// 助手轮写盘、/new 清盘全部持此锁串行；不同 key 互不阻塞（审查后收敛：原全局锁
    /// 会让任一 chat 的磁盘写阻塞所有 chat 的 /new 与代际快照）。
    fn history_lock(&self, key: &str) -> Arc<std::sync::Mutex<u64>> {
        self.history_epochs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(0)))
            .clone()
    }

    /// #49：/new 的历史重置——自增代际 + 清历史/标记，在 per-key 锁内完成（与注入读/写盘
    /// 互斥，见 history_epochs 字段注释）。返回 clear 是否成功：失败时调用方必须中止会话
    /// 重置——否则旧历史/标记文件仍在，新会话首轮注入会读到被用户要求清除的对话
    /// （审查 I-2 读侧：clear 失败 + 注入读未按代际闸）。
    fn history_reset(&self, key: &str) -> bool {
        let lock = self.history_lock(key);
        let mut epoch = lock.lock().unwrap_or_else(|e| e.into_inner());
        *epoch += 1;
        // bump + clear 在同一锁持内：写方要么在 clear 前写完（被清掉）、要么在 clear 后
        // 看到新代际被拦，不会残留（审查 I-2；清盘不放锁外，否则 post-/new 写盘会落进
        // 尚未清掉的旧文件再被清掉——该轮历史静默丢失）。
        crate::history::History::open(&self.bot.key(), key).clear()
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

        // should_respond（访问控制，默认私有）：公开开关开 → 放行所有人；否则只放行 owner
        // （管理员）∪ 授权者（授权码添加）白名单。未授权者只能通过授权码激活。
        // 每次消息从 config.json 热读最新访问控制（授权/取消/改开关即时生效，不依赖启动快照）；
        // config 读不到该 bot（单测注入）→ 回落构造时的快照。判定统一走 BotConfig::access_allows。
        // 同一份热读顺路推导发送者角色（owner=全权限 / granted=受限），随 Ev 传给 agent 调用处。
        let (allowed, sender_role, mention_map) = self.access_and_role(sender_id);
        if !allowed {
            // 未授权用户可能在发授权码：仅 p2p 接受；文本精确匹配 pending 码 → 消费并
            // 把发送者加入白名单（管理员码→owner / 普通码→授权者）、回发结果；
            // 不匹配 → 按未授权消息忽略（不进入 agent，含 /new 等指令）。
            if chat_type == "p2p" {
                let raw = message["content"].as_str().unwrap_or("");
                let text = crate::feishu::parse_content(raw).text;
                let chat_id = message["chat_id"].as_str().unwrap_or("").to_string();
                if self
                    .try_consume_owner_code(sender_id, &chat_id, true, &text)
                    .await
                {
                    return;
                }
            }
            crate::log!(
                "[bridge] 忽略非 owner 消息（bot={} sender={} chat_type={}）",
                self.bot.key(),
                sender_id,
                chat_type
            );
            return;
        }
        // 群聊只有 @ 了本机器人（或话题内回复）的消息才处理：
        // 话题（thread）内用户回复机器人的消息不需要再次 @——这是「用户回复」的主流交互；
        // 顶层群消息仍要求 @（避免整个群的消息都进 agent）。
        // #51：该群设了免 @（mention_modes off）则顶层消息也进 agent——热读即时生效。
        // 门槛判定复用 access_and_role 同一次 config load（mention_off），
        // 已 @ 则短路不付门槛判定（已 @ 的消息本就无需门槛）。
        let chat_id = message["chat_id"].as_str().unwrap_or("");
        if chat_type == "group"
            && thread_id.is_empty()
            && !self.bot_is_mentioned(&mentions)
            && !self.mention_off(&mention_map, chat_id)
        {
            crate::log!(
                "[bridge] 群聊未 @ 机器人，忽略（bot={} chat={} sender={}）",
                self.bot.key(),
                chat_id,
                sender_id
            );
            return;
        }

        // content 是 JSON 字符串：文本 / 图片 / 文件 / 音视频 / 富文本（#12）。
        // 非文本消息解析出资源引用后下载附件，text 保持空（不再把 raw JSON 当文本透传）。
        // 富文本多图：下载全部图片（与引用路径一致，避免直接发 3 图只收 1 张的割裂）。
        let raw = message["content"].as_str().unwrap_or("");
        let parsed = crate::feishu::parse_content(raw);
        let text = parsed.text.trim().to_string();
        let mid = message["message_id"].as_str().unwrap_or("").to_string();
        let mut attachments = Vec::new();
        for (i, res) in parsed.resources.into_iter().enumerate() {
            let desc = crate::attachments::AttachmentDesc::Feishu {
                message_id: mid.clone(),
                file_key: res.file_key,
                kind: res.kind.clone(),
                file_name: res.file_name,
            };
            if let Some(meta) = self
                .msgr
                .download_attachment(&self.bot.key(), &mid, i, &desc)
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
            role: sender_role,
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
                match Backend::parse(self.bot.effective_backend(&self.default_backend)) {
                    // #56/#57：/new 后旧 sid 的 pi 会话文件永久失效（pi 按 sid 续聊，新 sid
                    // 不再触碰旧文件）——顺手清掉：.pi-sessions 是 #56 探针的唯一信号源，
                    // 残留文件只增不减会拖慢每轮探针扫描并堆积磁盘。CLI `session reset`
                    // 同样只轮换 sid，但不走本分支（旧文件由探针按 mtime 忽略）。
                    Backend::Pi => {
                        let pi_dir = crate::workspace_dir(&self.bot.key()).join(".pi-sessions");
                        if let Ok(entries) = std::fs::read_dir(&pi_dir) {
                            for e in entries.flatten() {
                                if e.file_name().to_string_lossy().contains(&old_sid) {
                                    let _ = std::fs::remove_file(e.path());
                                }
                            }
                        }
                    }
                    // #67：prime 会话文件名是 ULID（不含会话 id），无法按文件名过滤——
                    // 按内容判定：删「首行 id 不属于任何存活槽位」的文件。
                    // 覆盖三类：本聊天 reset 后的旧会话（出槽）、失败轮孤儿（id 从未
                    // 回存进槽位）、损坏文件（首行 id 不可解析，不可能是存活会话）。
                    // **不得**像 pi 那样简单清目录：.prime-sessions 是 per-bot 目录而
                    // 槽位是 per-chat——直接清空会把同 bot 其它聊天的活跃会话连带删掉
                    // （审查 Important）。10 分钟 mtime 护栏：另一个聊天正在跑的首轮
                    // 任务（新会话 id 尚未回存进槽位）不属存活集，但文件正被追加写
                    // （mtime 新鲜）——不得误删，留待下次 /new 时已过期回收。
                    Backend::PrimeAgent => {
                        let live: std::collections::HashSet<String> = self
                            .sessions
                            .live_session_ids("prime-agent")
                            .into_iter()
                            .collect();
                        let dir = crate::workspace_dir(&self.bot.key()).join(".prime-sessions");
                        let cutoff =
                            std::time::SystemTime::now() - std::time::Duration::from_secs(600);
                        if let Ok(entries) = std::fs::read_dir(&dir) {
                            for e in entries.flatten() {
                                // 读不到 mtime 按新鲜处理——宁留不删（留的代价是孤儿
                                // 堆积，删错是别人丢上下文）
                                let fresh = e
                                    .metadata()
                                    .and_then(|m| m.modified())
                                    .map(|t| t > cutoff)
                                    .unwrap_or(true);
                                if fresh {
                                    continue;
                                }
                                let remove = match crate::agent::session_file_id(&e.path()) {
                                    None => true,
                                    Some(id) => !live.contains(&id),
                                };
                                if remove {
                                    let _ = std::fs::remove_file(e.path());
                                }
                            }
                        }
                    }
                    _ => {}
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
        // 受限会话（授权者）：prompt 开头前置受限说明。CLAUDE.md 是 owner/授权者共享的
        // 同一份指引，不能靠它区分——prompt 注入才是按角色区分的正确载体（硬闸在 guard hook）。
        // 判定与 agent::run 的 restrict 一致（role==Granted && 开关热读）——owner 关掉
        // 隔离开关后，granted 会话实际是全权限，prompt 不得再谎称受限（否则模型自我设限、
        // 或把不存在的拦截声明当承诺）。读不到 config 按安全默认 true。
        // （insert 挪到锁内历史注入之后——受限说明必须保持最外层。）
        let restrict_prompt = crate::config::restrict_granted(ev.role, &self.bot.key());

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

        // #49 后端切换上下文迁移：新会话首轮（!resume）且历史尚未注入过该会话 →
        // 把最近几轮对话注入 prompt 开头（切后端/会话丢失后新后端由此接续上下文）。
        // marker 按 session_id 判定：/new、CLI reset 使 marker 失效或失配（新会话允许
        // 再注入）。三层闸防重复注入：per-chat 串行锁（前一轮完整结束才轮到本条）+
        // !resume（started=true 的正常消息直接 resume 不注入）+ marker。#54：自愈重建
        // 会话带 pending 标记（同 sid）→ resume 轮也放行一次注入。
        let hist = crate::history::History::open(&self.bot.key(), &key);
        // 代际锁（per-key，见 history_epochs 字段注释）：注入读 + 用户轮写盘在锁内与
        // /new 清盘互斥——新会话首轮不可能读到未清盘的旧历史（审查 I-2 读侧闭环）。
        // 锁持于块作用域内（std MutexGuard 非 Send 不能跨 await）：块结束即释放，
        // agent 运行期间不持锁（/new 不被运行中任务阻塞）。
        let (hist_epoch_lock, hist_epoch, injected_rounds) = {
            let lock = self.history_lock(&key);
            let lock_ret = lock.clone(); // guard 借用 lock，返回值需独立 Arc
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let epoch = *guard;
            // 注入闸（锁内读 marker/entries：与 /new 的 clear 互斥，杜绝读侧交错）：
            // - !resume（新会话首轮）：marker 缺失或 sid 失配 → 注入（#49 后端切换迁移）。
            //   pi 例外（#56 同一探针，两个 !resume 臂都参与）：文件存在即续聊——被打断/
            //   失败的 pi 轮次文件已在盘上（pi 会话创建即落盘），文件存在时再注入会把
            //   同一历史块二次写进 pi transcript；文件缺失（且 marker 命中）才是真丢失。
            // - resume（既有会话）：pending 命中（#54 自愈重建/换 UUID 后待补注入）
            //   → 放行恰好一次，注入成功后桥回写 pending=false（复位）；
            //   或 pi/prime-agent 会话文件丢失/损坏（#56/#67：pi 对不可续聊的文件同 sid
            //   静默新建空会话——文件被删或损坏均实测如此——无错误可检）→ 本轮直接注入
            //   （run 前即可探明，比 pending 早一轮）。**不设 marker 防重复护栏**：pi run
            //   成功必落会话文件（核心功能），文件持续不可续聊 = 每轮都是新会话，重注入是
            //   正确行为；用「marker 匹配即已注入过」拦截会把「迁移后文件才丢失」的
            //   真丢失误判为已注入（静默永久无上下文，恰是本功能要杀的症状）——布局
            //   误报的代价是可见噪音（提示从首轮起可见；误报持续时注入块按轮累积进
            //   pi transcript，每轮 ≤6000 字符），可接受。
            //
            // 架构（#56/#57/#67 定论）：丢失检测**分层是本质而非债**——
            // - claude/codex 有错误文本（no rollout found / No conversation found），事后
            //   分类（agent.rs run）→ rebuilt + pending 迁移标记补注入；
            // - pi 无错误信号（静默新建），只能事前探查（本闸的探针）→ 本轮直接注入；
            // - prime-agent 两种信号都有（--resume 不存在 → exit 1 + "No session found"，
            //   可走后述重建；会话文件经 --session-dir 落盘，可走探针）——探针先行
            //   （早一轮注入），run 失败后 run() 的 session_lost 分支兜底重建。
            let marker = hist.marker();
            let session_file_lost = || {
                (backend == Backend::Pi
                    && !crate::agent::pi_session_exists(
                        &crate::workspace_dir(&self.bot.key()),
                        &session_id,
                    ))
                    || (backend == Backend::PrimeAgent
                        && !crate::agent::prime_session_exists(
                            &crate::workspace_dir(&self.bot.key()),
                            &session_id,
                        ))
            };
            let session_file_alive = || {
                (backend == Backend::Pi
                    && crate::agent::pi_session_exists(
                        &crate::workspace_dir(&self.bot.key()),
                        &session_id,
                    ))
                    || (backend == Backend::PrimeAgent
                        && crate::agent::prime_session_exists(
                            &crate::workspace_dir(&self.bot.key()),
                            &session_id,
                        ))
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
                let (block, n) = hist.inject_block(&ev.mid, crate::history::INJECT_CHARS_DEFAULT);
                if n > 0 {
                    prompt.insert_str(0, &block);
                    Some(n)
                } else {
                    None
                }
            } else {
                None
            };
            // 受限说明后插（insert_str(0) 后进者更靠前）→ 保持在最外层
            if restrict_prompt {
                prompt.insert_str(
                    0,
                    "[受限模式] 你是受限会话：只能读/写本工作区（当前 bot 目录）内的文件；\
你的记忆文件是 GRANTED.md（跨轮次保存信息用它，可读写）；\
可用命令仅限 $ABB_BIN（定时任务/投递）与只读 git；不可联网、不可访问工作区外任何路径；\
越界操作会被系统拦截并记录。\n\n",
                );
            }
            // 当前用户轮落历史（锁内，与助手轮严格按真实顺序交替；重放由 (mid,user) 去重兜底）。
            // 锁内写与 /new 的 clear 互斥。
            hist.append_user(&ev.mid, backend.name(), &history_user_text(&text, &ev));
            (lock_ret, epoch, injected_rounds)
        };

        let typing_rid = self.msgr.typing(&ev.mid).await;

        // agent 边跑边把中途完整消息推进 progress 通道（agent.rs 现状不变）；
        // 打字机已下线：中途处理过程消息一律丢弃不回，任务结束只发最终结果一条。
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
            &key, // 会话隔离 key（话题=chat:thread，#14）：session 存储按 key 记账，回存须同 key
            &bot_key,
            ev.role, // 发送者角色：granted 走受限分支（restrict 判定在 agent::run 内热读）
            Some(&self.sessions),
            Some(ptx),
            Some(cancel_flag.clone()),
        );
        tokio::pin!(run_fut);

        // 中途输出只计数不逐条留日志：编码 agent 一轮任务可推数百条进度，逐条写盘会让
        // 日志量随任务时长无界增长（打字机路径原有 500ms 节流，微信/关停路径静默丢弃）。
        // 统一只发最终结果：丢弃并计数，收尾汇总成一行日志，信息不减、日志量有界。
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
                trunc(&ev.chat_id, 10)
            );
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
                let history_note =
                    injected_rounds.map(|n| format!("\n\n（已携带最近 {n} 轮上下文）"));
                // 普通回复全文发送。发送结果必须留痕：回复丢了
                // （token 失效/会话失效等）时不能谎报成功。
                // #49：注入提示附在全文尾部（若本轮做过历史注入）。
                let sent_text = match &history_note {
                    Some(note) => format!("{reply}{note}"),
                    None => reply.clone(),
                };
                // 阶段 1（W2 窗口修复）：回复产出后先把最终文本落盘到 pending 条目——
                // 「发送前崩溃」的恢复据此**补发而非重跑**（原 remove 在发送前，
                // 此窗口崩溃 = 回复静默丢失）。发送成功后才 remove（send 成功但
                // remove 前崩溃 = 重启补发一条重复回复，at-least-once 仅重发文本，
                // 严格优于重跑）。发送失败 → remove + 日志（用户在场可重发；恢复
                // 路径的无人值守补发不适用此场景，避免重启后陈旧回复）。
                self.pending.set_reply(&ev.mid, &sent_text);
                match self.send_reply(&ev, &sent_text).await {
                    Ok(()) => {
                        self.pending.remove(&ev.mid);
                        crate::log!(
                            "[bridge] 已回复 chat={} 长度={}",
                            trunc(&ev.chat_id, 10),
                            reply.chars().count()
                        );
                    }
                    Err(e) => {
                        self.pending.remove(&ev.mid);
                        crate::log!(
                            "[bridge] ⚠️ 回复发送失败 chat={}: {e:#}",
                            trunc(&ev.chat_id, 10)
                        );
                    }
                }
            }
            Ok(agent::RunOutcome::Cancelled) => {
                crate::log!("[bridge] 任务被打断 chat={}", trunc(&ev.chat_id, 10));
                // 先摘 pending 再发停止通知（审查：remove 若在发送后，「发送期间/后
                // remove 前」崩溃会让已叫停的任务以 reply=None 残留 → 重启被普通重放
                // **续跑**，违背叫停语义；停止通知本身丢失可接受——用户已在场叫停）。
                self.pending.remove(&ev.mid);
                // 只发最终结果：「⏹ 已停止」一条。
                let _ = self.send_reply(&ev, "⏹ 已停止").await;
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
            if parse_mention_cmd(&item.text).is_some() {
                crate::log!(
                    "[bot:{}] 跳过 pending 重放中的控制指令（/mention 为升级前残留）mid={}",
                    self.bot.key(),
                    trunc(&item.mid, 12)
                );
                self.pending.remove(&item.mid);
                continue;
            }
            // 阶段 1（W2 窗口修复）：回复已产出但未确认发出（崩溃在 set_reply 与
            // remove 之间）→ **直接补发，不重跑 agent**（原语义此窗口回复静默丢失）。
            // 补发成功才 remove；失败留盘，下次启动再试（不重跑）。
            // 注：与实时路径的 send_reply 一样无 per-chat 锁（recover 与事件循环并行，
            // 与「🔄 正在恢复」提示同模式）——仅消息顺序可能交错，无正确性问题。
            if let Some(reply) = &item.reply {
                let ev = Ev {
                    mid: item.mid.clone(),
                    chat_id: item.chat_id.clone(),
                    chat_type: item.chat_type.clone(),
                    thread_id: item.thread_id.clone(),
                    quoted: crate::messenger::QuotedContent::default(),
                    text: item.text.clone(),
                    attachments: Vec::new(),
                    role: item.role,
                };
                crate::log!(
                    "[bot:{}] 补发上次已产出的回复 chat={} mid={}",
                    self.bot.key(),
                    trunc(&ev.chat_id, 12),
                    trunc(&ev.mid, 12)
                );
                match self.send_reply(&ev, reply).await {
                    Ok(()) => self.pending.remove(&item.mid),
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
            role: crate::config::SenderRole::Owner, // 微信只有 owner（on_weixin 已按 wx_user_id 过滤）
        };
        self.handle(ev).await;
    }

    /// 钉钉入站消息入口（service 的钉钉 Stream 循环调用）。
    /// msg=解析好的机器人消息；先记群聊最近发送者（回复时 @），过滤后走统一 handle。
    pub async fn on_dingtalk(&self, msg: crate::dingtalk::DingtalkMessage) {
        // 访问控制（与飞书同套，staffId 标识）：公开开关开 → 放行所有人；否则只放行 owner ∪
        // 授权者白名单。每次热读 config（授权/取消/改开关即时生效）；config 读不到（单测）回落快照。
        // 同一份热读顺路推导发送者角色（owner=全权限 / granted=受限），随 Ev 传给 agent。
        let (allowed, sender_role, mention_map) = self.access_and_role(&msg.sender_staff_id);
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
            crate::log!(
                "[dingtalk] 忽略非 owner 消息 from={}",
                trunc(&msg.sender_staff_id, 10)
            );
            return;
        }
        // 群聊只有 @ 了本机器人（或配置了「@ 才推送」）的消息才处理；单聊直接处理。
        // #51：该群设了免 @（mention_modes off）则无需 @ 也进 agent（与飞书同开关）。
        // 门槛判定复用 access_and_role 同一次 config load；已 @ 则短路不付门槛判定。
        if msg.is_group() && !msg.mentioned && !self.mention_off(&mention_map, &msg.chat_id()) {
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
            role: sender_role,
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

/// #49 历史条目的用户轮文本：用户文本 + （引用）被引用文本 + （附件）元数据行。
/// 单条 300 字截断在 history 层做——这里只负责按重要性排布（用户文本最前，
/// 截断丢的是尾巴=次要信息）。与 prompt 的 [引用消息]/[附件] 段同源同格式。
/// 有意省略 [链接] 段（审查 M-4）：链接本身就在用户文本里，无需重复落历史。
fn history_user_text(text: &str, ev: &Ev) -> String {
    let mut t = String::from(text);
    if !ev.quoted.text.is_empty() {
        t.push_str("\n（引用）");
        t.push_str(&ev.quoted.text);
    }
    if !ev.attachments.is_empty() || !ev.quoted.attachments.is_empty() {
        t.push_str("\n（附件）");
        for a in ev.attachments.iter().chain(ev.quoted.attachments.iter()) {
            t.push_str(&format!("\n{}", a.to_prompt_line()));
        }
    }
    t
}

/// 识别「/new」会话新建指令（#23）：trim 后精确匹配，大小写不敏感。
/// 只在 handle 里拦截（在透传 agent 之前），其它斜杠命令仍原样透传。
fn is_new_command(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("/new")
}

/// #51 免 @ 群聊开关指令。
enum MentionCmd {
    /// 无参：显示当前群状态
    Show,
    On,
    Off,
}

/// 识别 /mention 指令（#51）：精确匹配 `/mention`、`/mention on`、`/mention off`
/// （trim + 大小写不敏感，仿 /new）。返回 None = 不是该指令（原样透传 agent）。
fn parse_mention_cmd(text: &str) -> Option<MentionCmd> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("/mention") {
        Some(MentionCmd::Show)
    } else if t.eq_ignore_ascii_case("/mention on") {
        Some(MentionCmd::On)
    } else if t.eq_ignore_ascii_case("/mention off") {
        Some(MentionCmd::Off)
    } else {
        None
    }
}

/// /mention 免 @ 确认文案（Show-off 与 Off 共用，防止两份文案漂移）。
const MENTION_OFF_MSG: &str = "已开启免 @：本群授权用户的消息无需 @ 直接进入 agent。\
多用户群共享同一会话、可能有上下文串扰；/mention on 可恢复。";

/// 开关写入失败（config 加载/保存出错）时的如实回显。
const MENTION_SAVE_FAIL_MSG: &str =
    "⚠️ 开关保存失败（config.json 写入出错），本次设置未持久化，重启后不保留。";

/// 识别「打断」关键词（整句精确匹配，大小写不敏感）。仅当该 chat 有任务在跑时才生效
/// （由 handle 判断）；否则原样透传给 agent，避免误吞用户正常词汇。
fn is_cancel_keyword(text: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "停", "停止", "停下", "取消", "stop", "cancel", "/stop", "/cancel",
    ];
    let t = text.trim().to_ascii_lowercase();
    KEYWORDS.iter().any(|k| *k == t)
}

/// 显式取消命令（/cancel、/stop）：与自然停止词不同——无任务在跑时也要明确回复、
/// 不透传给 agent（避免被当普通问题回答）。
fn is_cancel_command(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    t == "/cancel" || t == "/stop"
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
            role: crate::config::SenderRole::Owner,
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
            role: crate::config::SenderRole::Owner,
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
    fn cancel_command_is_exact_and_case_insensitive() {
        // 显式取消命令：/cancel /stop 精确匹配（含大小写/首尾空白），带参数不算
        for c in ["/cancel", "/stop", " /cancel ", "/CANCEL", "/Stop"] {
            assert!(is_cancel_command(c), "{c:?} 应是取消命令");
        }
        assert!(!is_cancel_command("/cancel 全部"));
        assert!(!is_cancel_command("/cancelled"));
        assert!(!is_cancel_command("取消")); // 自然词不是命令（保持透传语义）
        assert!(!is_cancel_command("stop"));
        assert!(!is_cancel_command(""));
        // 自然停止词仍是关键词（有任务时也能打断）
        assert!(is_cancel_keyword("取消"));
        assert!(is_cancel_keyword("/cancel"));
        assert!(is_cancel_keyword("STOP"));
    }

    #[test]
    fn cancel_keywords_match() {
        for k in [
            "停", "停止", "取消", "stop", "Stop", "STOP", "/stop", "cancel", "/cancel", " 停 ",
        ] {
            assert!(is_cancel_keyword(k), "应为停止词: {k:?}");
        }
        for k in [
            "停下来聊聊",
            "stop it",
            "别停",
            "/stopit",
            "取消订阅这个服务",
            "",
        ] {
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
        /// (chat_id, text) 全量记录：提及私信等按目标断言用（sent() 只留文本）。
        sent_chats: Mutex<Vec<(String, String)>>,
        quoted: Mutex<std::collections::HashMap<String, crate::messenger::QuotedMessage>>,
        /// 设置后：发给该 chat 的消息返回 Err（失败注入，测游标回退链路）。
        fail_chat: Mutex<Option<String>>,
    }
    impl MockMessenger {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                sent_chats: Mutex::new(Vec::new()),
                quoted: Mutex::new(std::collections::HashMap::new()),
                fail_chat: Mutex::new(None),
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
        async fn send_text(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
            if let Some(f) = self.fail_chat.lock().unwrap().clone() {
                if f == chat_id {
                    anyhow::bail!("模拟发送失败");
                }
            }
            self.sent.lock().unwrap().push(text.to_string());
            self.sent_chats
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
            Ok(())
        }
        async fn get_quoted_message(
            &self,
            message_id: &str,
        ) -> Option<crate::messenger::QuotedMessage> {
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
                crate::attachments::AttachmentDesc::Feishu {
                    kind, file_name, ..
                } => (kind.clone(), file_name.clone()),
                crate::attachments::AttachmentDesc::Dingtalk {
                    kind, file_name, ..
                } => (kind.clone(), file_name.clone()),
                crate::attachments::AttachmentDesc::Wechat(m) => {
                    (m.kind.clone(), m.file_name.clone())
                }
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

    /// run 的收尾形态（默认 Reply；Cancel 模拟被打断；Err 模拟后端报错）。
    enum MockOutcome {
        Reply,
        Cancel,
        Fail(String),
    }

    /// 挡板 agent：run 进入即 `started.notify_one()`（让测试知道「任务在跑」）；
    /// `block=true` 等 `release.notified()` 才返回（用于「任务运行中穿插 /new」），
    /// `block=false` 立即返回（对照组）。
    struct MockAgentRunner {
        started: Notify,
        release: Notify,
        block: bool,
        reply: String,
        progress_msgs: Vec<String>,
        outcome: MockOutcome,
        prompts: Mutex<Vec<String>>,
        /// 收到的发送者角色（授权者隔离断言用：granted → 受限分支）。
        roles: Mutex<Vec<crate::config::SenderRole>>,
        /// 前 N 次 run 返回 rebuilt=true（#54 自愈重建轮），随后回 false。
        rebuilt_left: std::sync::atomic::AtomicUsize,
        /// 一次性 claude already-in-use 自愈模拟：run 内把槽位 CAS 换成该 sid 并返回
        /// 之（等价 reset_session 换 UUID + started 复位后再 mark 的最终状态）。
        heal_to_sid: Mutex<Option<String>>,
    }
    impl MockAgentRunner {
        fn blocking(reply: &str) -> Self {
            Self {
                started: Notify::new(),
                release: Notify::new(),
                block: true,
                reply: reply.into(),
                progress_msgs: Vec::new(),
                outcome: MockOutcome::Reply,
                prompts: Mutex::new(Vec::new()),
                roles: Mutex::new(Vec::new()),
                rebuilt_left: std::sync::atomic::AtomicUsize::new(0),
                heal_to_sid: Mutex::new(None),
            }
        }
        fn set_rebuilt_rounds(&self, n: usize) {
            self.rebuilt_left
                .store(n, std::sync::atomic::Ordering::SeqCst);
        }
        fn set_heal_sid(&self, sid: &str) {
            *self.heal_to_sid.lock().unwrap() = Some(sid.to_string());
        }
        fn immediate(reply: &str) -> Self {
            Self {
                block: false,
                ..Self::blocking(reply)
            }
        }
        /// run 立即返回（block=false）+ 中途输出 —— 进度可能还压在通道里，
        /// 强制走收尾排空（try_recv drain）路径。
        fn with_progress_immediate(reply: &str, progress: &[&str]) -> Self {
            Self {
                progress_msgs: progress.iter().map(|s| s.to_string()).collect(),
                block: false,
                ..Self::blocking(reply)
            }
        }
        /// 中途输出推完后以 Err 收尾（模拟后端报错）；block=false 不等 release。
        fn with_progress_error(progress: &[&str], err: &str) -> Self {
            Self {
                progress_msgs: progress.iter().map(|s| s.to_string()).collect(),
                outcome: MockOutcome::Fail(err.into()),
                block: false,
                ..Self::blocking("（不会用到）")
            }
        }
        /// 中途输出推完后以 Cancelled 收尾（模拟被打断）；block=false 不等 release。
        fn with_progress_cancel(progress: &[&str]) -> Self {
            Self {
                progress_msgs: progress.iter().map(|s| s.to_string()).collect(),
                outcome: MockOutcome::Cancel,
                block: false,
                ..Self::blocking("（不会用到）")
            }
        }
        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }

        fn roles(&self) -> Vec<crate::config::SenderRole> {
            self.roles.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl AgentRunner for MockAgentRunner {
        #[allow(clippy::too_many_arguments)]
        async fn run(
            &self,
            backend: Backend,
            prompt: &str,
            session_id: &str,
            _resume: bool,
            _chat_id: &str,
            session_key: &str,
            bot_key: &str,
            role: crate::config::SenderRole,
            sessions: Option<&SessionStore>,
            progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
            cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
        ) -> Result<agent::RunOutcome, String> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.roles.lock().unwrap().push(role);
            self.started.notify_one();
            // #57 审查遗留修复：模拟 pi 的持久化时机——真实 pi 在**会话创建时**（run
            // 开始、LLM 工作之前，SessionManager.open）即落盘，失败/被打断的轮次同样
            // 留文件（文件里已有该轮的注入块与消息）；mock 原只在 Reply 写盘，导致
            // 「失败后文件存在 → 不重复注入」的生产路径测试不可表达，T5 反而锁定了
            // 与生产矛盾的语义。写在 run 入口 = 所有 outcome（Reply/Fail/Cancel）都留盘。
            // prime 同 pi（#67）：会话创建即落盘，mock 同等对待。
            match backend {
                Backend::Pi => {
                    let _ = write_pi_session_file(bot_key, session_id);
                }
                Backend::PrimeAgent => {
                    let _ = write_prime_session_file(bot_key, session_id);
                }
                _ => {}
            }
            // 先把中途输出推完（unbounded 即推即走），桥侧 select/收尾排空负责丢弃
            if let Some(tx) = &progress {
                for p in &self.progress_msgs {
                    let _ = tx.send(p.clone());
                }
            }
            if self.block {
                // 挂住期间响应 cancel（模拟真实 agent 被打断）：cancel 置 true → Cancelled
                tokio::select! {
                    _ = self.release.notified() => {}
                    _ = async {
                        let flag = cancel.clone().expect("blocking mock 需要 cancel flag");
                        loop {
                            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    } => {
                        return Ok(agent::RunOutcome::Cancelled);
                    }
                }
            }
            // 返回本次运行使用的 session_id——bridge 据此做 mark_started_if 身份校验
            match &self.outcome {
                MockOutcome::Reply => {
                    let rebuilt = self.rebuilt_left.load(std::sync::atomic::Ordering::SeqCst) > 0
                        && self
                            .rebuilt_left
                            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
                            > 0;
                    // 一次性 claude already-in-use 自愈模拟：槽位 CAS 换新 sid（等价 run 内
                    // reset_session 的最终状态），返回新 sid——桥据此写 pending 标记（#54 审查）
                    let final_sid = if let Some(new_sid) = self.heal_to_sid.lock().unwrap().take() {
                        if let Some(store) = sessions {
                            store.set_session_id_if(session_key, session_id, &new_sid);
                        }
                        new_sid
                    } else {
                        session_id.to_string()
                    };
                    // pi 会话文件已在 run 入口写过（见上）；claude 换 UUID 自愈轮的
                    // final_sid 若与入口 sid 不同——pi 不走 heal，无需补写。
                    Ok(agent::RunOutcome::Reply {
                        reply: self.reply.clone(),
                        session_id: final_sid,
                        rebuilt,
                    })
                }
                MockOutcome::Cancel => Ok(agent::RunOutcome::Cancelled),
                MockOutcome::Fail(e) => Err(e.clone()),
            }
        }
    }

    /// #56/#57：写一个形态正确的 pi 会话文件（探针按「首行 session 记录 id + 末行
    /// 完整 JSON」校验）。mock 与各 fixture 共用，格式契约只此一处。
    fn write_pi_session_file(bot_key: &str, sid: &str) -> std::io::Result<()> {
        let dir = crate::workspace_dir(bot_key).join(".pi-sessions");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(format!("2026-08-14T00-00-00-000Z_{sid}.jsonl")),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"timestamp\":\"2026-08-14T00:00:00.000Z\"}}\n"
            ),
        )
    }

    /// #67：写一个形态正确的 prime 会话文件——与 pi 的差异仅在目录与文件名
    /// （prime 文件名是 ULID，**不含**会话 id，探针按首行 id 匹配）。
    /// 返回文件路径（/new 清理测试需要按文件断言存在性/拨旧 mtime）。
    fn write_prime_session_file(bot_key: &str, sid: &str) -> std::io::Result<std::path::PathBuf> {
        let dir = crate::workspace_dir(bot_key).join(".prime-sessions");
        std::fs::create_dir_all(&dir)?;
        let compact = sid.replace('-', "");
        let stem = compact.get(..12).unwrap_or(&compact);
        let path = dir.join(format!("01a00a5d-fc0a-760e-b9dc-{stem}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"timestamp\":\"2026-08-14T00:00:00.000Z\"}}\n"
            ),
        )?;
        Ok(path)
    }

    /// 把文件 mtime 拨旧（1 小时前）——模拟不活跃会话/孤儿（/new 清理的 10 分钟护栏外）。
    fn set_mtime_old(path: &std::path::Path) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600)),
        )
        .unwrap();
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
            role: crate::config::SenderRole::Owner,
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

    // ─── #49 后端切换上下文迁移（历史日志 + 首轮注入）──────────────────────

    /// 构造指定后端的 bot（随机 key 隔离工作目录）。
    fn backend_bot(backend: &str) -> BotConfig {
        BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            backend: backend.into(),
            ..Default::default()
        }
    }

    /// 预置「已迁移过、可 resume」的既有会话槽位（started=true + marker 匹配同 sid）
    /// 并写入一轮旧历史。#54 注入闸测试共用的前置（seed 变了只需改这一处）。
    fn seed_migrated_session(
        bridge: &Bridge,
        bot: &BotConfig,
        key: &str,
    ) -> (crate::history::History, String) {
        let sid = bridge.sessions.ensure_with_started(key).0;
        assert!(bridge.sessions.mark_started_if(key, &sid));
        let hist = crate::history::History::open(&bot.key(), key);
        hist.append_user("old1", "claude", "旧背景");
        hist.set_marker(&sid, "claude", false);
        (hist, sid)
    }

    /// T1/T2：切后端首轮注入 [历史上下文]（旧→新、角色前缀、在当前消息之前）+
    /// 回复尾部带提示；第二轮同会话 resume → 不重复注入、无提示。
    #[tokio::test]
    async fn backend_switch_injects_history_once() {
        let runner = Arc::new(MockAgentRunner::immediate("pi 的回答"));
        let bot = backend_bot("pi"); // bot 现在用 pi（历史里是 claude 的轮次）
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        // 预写 claude 历史（模拟切换前在该 chat 的 2 轮对话）
        let hist = crate::history::History::open(&bot.key(), "oc_x");
        hist.append_user("old1", "claude", "项目背景是登录偶发 401");
        hist.append_assistant("old1", "claude", "根因是 token 缓存竞态");
        hist.append_user("old2", "claude", "先修缓存失效路径");

        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "继续修")).await })
            .await
            .unwrap();

        let p0 = &runner.prompts()[0];
        assert!(p0.contains("[历史上下文]"), "首轮注入历史段: {p0}");
        assert!(p0.contains("用户: 项目背景是登录偶发 401"));
        assert!(p0.contains("助手: 根因是 token 缓存竞态"));
        let hpos = p0.find("[历史上下文]").unwrap();
        let mpos = p0.find("继续修").unwrap();
        assert!(hpos < mpos, "历史段在当前消息之前");
        assert_eq!(msgr.sent().len(), 1);
        assert!(
            msgr.sent()[0].ends_with("（已携带最近 2 轮上下文）"),
            "回复带注入提示，实际: {}",
            msgr.sent()[0]
        );
        // marker 已写（绑定该 session）；助手轮落历史
        assert_eq!(hist.marker().map(|m| m.backend), Some("pi".into()));
        assert!(hist
            .entries()
            .iter()
            .any(|e| !e.user && e.text.contains("pi 的回答")));

        // 第二条：同会话（started=true → resume）不重复注入、无提示
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "下一步")).await })
            .await
            .unwrap();
        assert!(!runner.prompts()[1].contains("[历史上下文]"), "不重复注入");
        assert!(!msgr.sent()[1].contains("已携带"), "第二轮无提示");
        cleanup_bridge(&bridge);
    }

    /// T3：/new 清空历史与迁移标记——用户明确要全新会话，注入历史随之失效。
    #[tokio::test]
    async fn new_command_clears_history_and_marker() {
        let runner = Arc::new(MockAgentRunner::immediate("ok"));
        let bot = backend_bot("pi");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let hist = crate::history::History::open(&bot.key(), "oc_x");
        hist.append_user("old1", "claude", "旧背景");
        // 先跑一轮让 marker 落上（模拟：切到 pi 已注入过）
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "hi")).await })
            .await
            .unwrap();
        assert!(hist.marker().is_some(), "首轮注入后 marker 已写");

        // /new → 历史与 marker 全清
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("n1", "oc_x", "/new")).await })
            .await
            .unwrap();
        assert!(hist.entries().is_empty(), "历史已清空");
        assert!(hist.marker().is_none(), "marker 已清");

        // /new 后首条消息：全新会话、历史空 → 无注入
        let b3 = bridge.clone();
        tokio::spawn(async move { b3.handle(test_ev("m2", "oc_x", "重新开始")).await })
            .await
            .unwrap();
        assert!(!runner.prompts()[1].contains("[历史上下文]"));
        assert!(!msgr.sent()[1].contains("已携带"));
        cleanup_bridge(&bridge);
    }

    /// #67：/new 后 prime 会话文件清理（审查 Important 修复）：删「首行 id 不属于任何
    /// 存活槽位且 mtime 已过期」的文件——本聊天旧会话与失败轮孤儿回收；同 bot 其它
    /// 聊天的活跃会话与在途新会话（mtime 新鲜）不得误删。
    #[tokio::test]
    async fn new_clears_prime_session_files() {
        let runner = Arc::new(MockAgentRunner::immediate("ok"));
        let bot = backend_bot("prime-agent");
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let dir = crate::workspace_dir(&bot.key()).join(".prime-sessions");

        // 本聊天既有会话（mtime 拨旧 = 已不活跃）→ /new 后应删
        let sid = bridge.sessions.ensure_with_started("oc_x").0;
        assert!(bridge.sessions.mark_started_if("oc_x", &sid));
        let f_old = write_prime_session_file(&bot.key(), &sid).unwrap();
        set_mtime_old(&f_old);
        // 同 bot 其它聊天的活跃会话（存活槽位）→ 必须保留
        let other_sid = bridge.sessions.ensure_with_started("oc_other").0;
        let f_other = write_prime_session_file(&bot.key(), &other_sid).unwrap();
        // 失败轮孤儿（id 不在任何槽位，mtime 旧）→ 应回收
        let f_orphan = dir.join("01a00a5d-fc0a-760e-b9dc-orphan01.jsonl");
        std::fs::write(
            &f_orphan,
            "{\"type\":\"session\",\"version\":3,\"id\":\"orphan-dead-id\",\"timestamp\":\"2026-08-14T00:00:00.000Z\"}\n",
        )
        .unwrap();
        set_mtime_old(&f_orphan);
        // 在途新会话（id 未知、mtime 新鲜）→ 10 分钟护栏保留
        let f_inflight = dir.join("01a00a5d-fc0a-760e-b9dc-inflight1.jsonl");
        std::fs::write(
            &f_inflight,
            "{\"type\":\"session\",\"version\":3,\"id\":\"inflight-unknown\",\"timestamp\":\"2026-08-14T00:00:00.000Z\"}\n",
        )
        .unwrap();

        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("n1", "oc_x", "/new")).await })
            .await
            .unwrap();

        assert!(!f_old.exists(), "本聊天旧会话文件已清");
        assert!(f_other.exists(), "其它聊天活跃会话保留");
        assert!(!f_orphan.exists(), "失败轮孤儿回收");
        assert!(f_inflight.exists(), "在途（mtime 新鲜）不误删");
        cleanup_bridge(&bridge);
    }

    /// T4：会话已 started（切回旧后端场景）→ 直接 resume 原上下文，不注入。
    #[tokio::test]
    async fn resume_existing_session_skips_injection() {
        let runner = Arc::new(MockAgentRunner::immediate("接续回答"));
        let bot = backend_bot("pi");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        // 预置：pi 槽位已有完整会话（started=true，可 resume 自己的上下文）。
        // started=true 在真实世界必然伴随 pi 会话文件（run 成功即落盘）——fixture
        // 补齐文件，否则 #56 探针会判定「会话丢失」而注入。
        let sid = bridge.sessions.ensure_with_started("oc_x").0;
        assert!(bridge.sessions.mark_started_if("oc_x", &sid));
        write_pi_session_file(&bot.key(), &sid).unwrap();
        // 不注入的真正原因：pending 失配 + 探针命中文件（#56）——marker 是 None，
        // 不是「对不上」；旧注释「!resume 直接短路」不成立（该轮 resume=true）。
        let hist = crate::history::History::open(&bot.key(), "oc_x");
        hist.append_user("old1", "claude", "claude 时代的内容");

        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "继续")).await })
            .await
            .unwrap();
        assert!(
            !runner.prompts()[0].contains("[历史上下文]"),
            "resume 不注入"
        );
        assert!(!msgr.sent()[0].contains("已携带"));
        cleanup_bridge(&bridge);
    }

    /// T5：首轮 agent 失败（未 mark、未写 marker）→ 重发/下一条重新注入
    /// （上下文从未真正进入任何 session，重注入是对的）。
    #[tokio::test]
    async fn failed_round_reinjects_on_next_message() {
        // #49 通用语义（失败轮不写 marker/started → 下一条重注入）。用 claude：pi 的
        // 失败轮文件已在盘上（会话创建即落盘），下一条走的是「文件在 → 不重复注入」
        // 的另一条路——见 pi_failed_round_keeps_file_no_reinject。
        let runner = Arc::new(MockAgentRunner::with_progress_error(&[], "后端爆炸"));
        let bot = backend_bot("claude");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let hist = crate::history::History::open(&bot.key(), "oc_x");
        hist.append_user("old1", "claude", "旧背景");

        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "第一问")).await })
            .await
            .unwrap();
        assert!(runner.prompts()[0].contains("[历史上下文]"), "首轮注入");
        assert!(hist.marker().is_none(), "失败轮不写 marker");
        assert!(
            !hist.entries().iter().any(|e| !e.user),
            "失败轮不写助手条目"
        );

        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "再试")).await })
            .await
            .unwrap();
        assert!(
            runner.prompts()[1].contains("[历史上下文]"),
            "下一条重新注入"
        );
        let _ = msgr;
        cleanup_bridge(&bridge);
    }

    /// #57 审查遗留修复：pi 失败轮的**生产语义**——真实 pi 在会话创建时（run 开始）
    /// 即落盘，失败轮的文件里已含该轮注入块与消息 → 下一条探针判定存活、**不重复
    /// 注入**（mock 原只在 Reply 写盘，把这条路径测试成了反面）。
    #[tokio::test]
    async fn pi_failed_round_keeps_file_no_reinject() {
        let runner = Arc::new(MockAgentRunner::with_progress_error(&[], "pi LLM 报错"));
        let bot = backend_bot("pi");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let hist = crate::history::History::open(&bot.key(), "oc_x");
        hist.append_user("old1", "claude", "旧背景");

        // m1：fresh 首轮注入历史 → pi 失败（会话创建即落盘，注入块已在文件里）
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "第一问")).await })
            .await
            .unwrap();
        assert!(runner.prompts()[0].contains("[历史上下文]"), "首轮注入");
        assert!(hist.marker().is_none(), "失败轮不写 marker");

        // m2：文件在（mock 于 run 入口落盘）→ 探针判存活 → 不重复注入
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "再试")).await })
            .await
            .unwrap();
        assert!(
            !runner.prompts()[1].contains("[历史上下文]"),
            "失败轮文件已在（含注入块），不重复注入: {}",
            runner.prompts()[1]
        );
        // Err 轮发送的是错误文案（非 Reply）——断言它锁「失败可见」而非恒真的
        // 否定式「不含已携带」（Err 文案在任何实现下都不含该提示，审查 Minor）。
        assert_eq!(msgr.sent()[1], "pi LLM 报错", "m2 走失败可见路径");
        cleanup_bridge(&bridge);
    }

    /// #54：同 sid 会话自愈重建——重建轮本身无注入（判定发生在 run 前），但写 pending
    /// 迁移标记；下一条消息（resume=true）被 pending 放行注入历史并复位标记；之后不注入。
    #[tokio::test]
    async fn rebuilt_round_marks_pending_then_next_injects() {
        let runner = Arc::new(MockAgentRunner::immediate("重建后的回复"));
        runner.set_rebuilt_rounds(1); // 第一次 run 返回 rebuilt=true（模拟自愈重建）
                                      // claude/codex 的 rebuilt 语义（pi 的静默重建走 #56 探针直接注入，不走 pending）
        let bot = backend_bot("claude");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        // 既有会话：started=true + marker 匹配（模拟已迁移过的会话，正常消息不注入）
        let (hist, sid) = seed_migrated_session(&bridge, &bot, "oc_x");

        // 重建轮（后端对端丢会话，agent 以同 sid 重建）→ 无注入、写 pending 标记
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "恢复试试")).await })
            .await
            .unwrap();
        assert!(
            !runner.prompts()[0].contains("[历史上下文]"),
            "重建轮本身无注入（判定发生在 run 之前）"
        );
        let m = hist.marker().expect("重建轮应写 pending 标记");
        assert!(m.pending, "标记 pending");
        assert_eq!(m.session_id, sid, "同 sid 标记");
        assert!(!msgr.sent()[0].contains("已携带"), "重建轮回复无提示");

        // 下一条消息：pending 放行（resume=true 也注入）→ 历史补上，标记复位
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "继续")).await })
            .await
            .unwrap();
        assert!(
            runner.prompts()[1].contains("[历史上下文]"),
            "pending 放行注入: {}",
            runner.prompts()[1]
        );
        assert!(runner.prompts()[1].contains("旧背景"), "历史内容注入");
        assert!(
            msgr.sent()[1].contains("已携带"),
            "注入轮回复带提示: {}",
            msgr.sent()[1]
        );
        assert!(!hist.marker().unwrap().pending, "注入后标记复位");

        // 再下一条：恢复正常（不注入、无提示）
        let b3 = bridge.clone();
        tokio::spawn(async move { b3.handle(test_ev("m3", "oc_x", "再来")).await })
            .await
            .unwrap();
        assert!(!runner.prompts()[2].contains("[历史上下文]"));
        assert!(!msgr.sent()[2].contains("已携带"));
        cleanup_bridge(&bridge);
    }

    /// 审查 Important（#54 同类缺口）：claude already-in-use 自愈在 run 内换 UUID——
    /// 换 UUID 使旧 marker「失效」，但 !resume 闸永不触发（started 已被 mark 回 true）
    /// → 必须由「final_sid != 入口 sid」判定补写 pending，下一条消息才注入。
    #[tokio::test]
    async fn claude_heal_sid_change_marks_pending() {
        let runner = Arc::new(MockAgentRunner::immediate("自愈后的回复"));
        runner.set_heal_sid("new-heal-sid"); // 一次性：run 内槽位 CAS 换新 sid
        let bot = backend_bot("claude");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        // 既有会话：started=true + marker 匹配（旧 sid）
        let (hist, _sid) = seed_migrated_session(&bridge, &bot, "oc_x");

        // 自愈轮：resume=true、marker 匹配且非 pending → 不注入；run 内换 UUID（rebuilt=false）
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "还在吗")).await })
            .await
            .unwrap();
        assert!(
            !runner.prompts()[0].contains("[历史上下文]"),
            "自愈轮不注入"
        );
        let m = hist.marker().expect("换 UUID 自愈应写 pending 标记");
        assert!(m.pending, "pending 标记");
        assert_eq!(m.session_id, "new-heal-sid", "标记记最终 sid");
        assert!(!msgr.sent()[0].contains("已携带"));

        // 下一条：pending 命中（resume=true 也注入）→ 历史补上、标记复位
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "继续")).await })
            .await
            .unwrap();
        assert!(runner.prompts()[1].contains("[历史上下文]"), "下一条注入");
        assert!(runner.prompts()[1].contains("旧背景"));
        assert!(msgr.sent()[1].contains("已携带"));
        assert!(!hist.marker().unwrap().pending, "注入后复位");
        cleanup_bridge(&bridge);
    }

    /// #54 审查：claude 自愈换 UUID 发生在 !resume 首轮（空历史，没有旧上下文可丢）
    /// 时不得写 pending——重试已把本轮完整 prompt 交给新会话，再写 pending 只会让
    /// 下一条把本轮自身重复注入一遍（外加误导性的「已携带上下文」提示）。
    #[tokio::test]
    async fn claude_heal_on_first_round_writes_no_pending() {
        let runner = Arc::new(MockAgentRunner::immediate("首轮回复"));
        runner.set_heal_sid("new-heal-sid"); // 一次性：run 内槽位 CAS 换新 sid
        let bot = backend_bot("claude");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let hist = crate::history::History::open(&bot.key(), "oc_x");
        // 全新会话：无历史、无 marker、started=false（!resume 首轮）

        // 首轮自愈换 UUID：空历史无注入 → 不写 pending
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "你好")).await })
            .await
            .unwrap();
        assert!(
            hist.marker().map(|m| !m.pending).unwrap_or(true),
            "首轮自愈无上下文丢失，不得写 pending: {:?}",
            hist.marker()
        );

        // 下一条（resume=true）：无 pending → 不注入、无提示（修复前会把 m1 这轮
        // 重复注入进已原生含它的会话）
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "继续")).await })
            .await
            .unwrap();
        assert!(
            !runner.prompts()[1].contains("[历史上下文]"),
            "无 pending：下一条不注入: {}",
            runner.prompts()[1]
        );
        assert!(!msgr.sent()[1].contains("已携带"));
        cleanup_bridge(&bridge);
    }

    /// #56：pi 会话文件丢失（对不存在文件同 sid 静默新建空会话，无错误可检）→
    /// resume 轮**本轮直接注入**（run 前探明，比 pending 早一轮）；pi 落文件后
    /// 恢复正常不注入。不设 marker 防重复护栏——真丢失不得被「已注入过」误拦
    /// （见注入闸注释），文件持续缺失时每轮重注入是正确行为。
    #[tokio::test]
    async fn pi_session_loss_injects_directly() {
        let runner = Arc::new(MockAgentRunner::immediate("重建轮的回复"));
        let bot = backend_bot("pi");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        // 既有会话（started=true + marker 匹配非 pending）+ 一轮旧历史；
        // .pi-sessions 下无该 sid 文件 = 会话已丢失
        let (hist, sid) = seed_migrated_session(&bridge, &bot, "oc_x");

        // msg1：文件缺失 → 本轮直接注入 + 提示
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "还在吗")).await })
            .await
            .unwrap();
        assert!(
            runner.prompts()[0].contains("[历史上下文]"),
            "文件丢失本轮直接注入: {}",
            runner.prompts()[0]
        );
        assert!(runner.prompts()[0].contains("旧背景"));
        assert!(msgr.sent()[0].contains("已携带"));
        let m = hist.marker().unwrap();
        assert!(!m.pending && m.session_id == sid, "注入后 marker 复位");

        // msg2：mock 已模拟 pi 落盘（run 成功必写会话文件）→ 正常续聊不注入、无提示
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "继续")).await })
            .await
            .unwrap();
        assert!(!runner.prompts()[1].contains("[历史上下文]"));
        assert!(!msgr.sent()[1].contains("已携带"));
        cleanup_bridge(&bridge);
    }

    /// #67：prime-agent 会话文件丢失 → resume 轮本轮直接注入（与 pi 探针同语义；
    /// 差异只在探针目录/文件名——prime 文件名不含 sid，按首行 id 匹配）。
    /// prime 对不可续聊目标另有 exit 1 + "No session found" 错误信号（run 重建兜底），
    /// 探针先行可早一轮注入，两者互补。
    #[tokio::test]
    async fn prime_session_loss_injects_directly() {
        let runner = Arc::new(MockAgentRunner::immediate("重建轮的回复"));
        let bot = backend_bot("prime-agent");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let (hist, sid) = seed_migrated_session(&bridge, &bot, "oc_x");

        // msg1：.prime-sessions 下无该 sid 文件 = 会话已丢失 → 本轮直接注入 + 提示
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "还在吗")).await })
            .await
            .unwrap();
        assert!(
            runner.prompts()[0].contains("[历史上下文]"),
            "文件丢失本轮直接注入: {}",
            runner.prompts()[0]
        );
        assert!(runner.prompts()[0].contains("旧背景"));
        assert!(msgr.sent()[0].contains("已携带"));
        let m = hist.marker().unwrap();
        assert!(!m.pending && m.session_id == sid, "注入后 marker 复位");

        // msg2：mock 已模拟 prime 落盘（ULID 文件名、首行 id 命中）→ 正常续聊不注入
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "继续")).await })
            .await
            .unwrap();
        assert!(!runner.prompts()[1].contains("[历史上下文]"));
        assert!(!msgr.sent()[1].contains("已携带"));
        cleanup_bridge(&bridge);
    }

    /// T6：任务运行中 /new → 清历史后，旧任务完成不得写孤儿助手条目/标记
    ///（mark_started_if 身份校验同一道闸）。
    #[tokio::test]
    async fn new_during_run_writes_no_orphan_history() {
        let runner = Arc::new(MockAgentRunner::blocking("旧任务回复"));
        let bot = backend_bot("pi");
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let hist = crate::history::History::open(&bot.key(), "oc_x");

        let b1 = bridge.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m1", "oc_x", "hello")).await });
        runner.started.notified().await; // 持锁运行中（用户条目已落）

        // 运行中 /new：清历史（含 m1 的用户条目）
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("n1", "oc_x", "/new")).await })
            .await
            .unwrap();
        assert!(hist.entries().is_empty(), "/new 已清历史");

        runner.release.notify_one();
        task.await.unwrap();
        assert!(hist.entries().is_empty(), "旧任务完成不得写孤儿助手条目");
        assert!(hist.marker().is_none(), "无孤儿 marker");
        cleanup_bridge(&bridge);
    }

    /// 审查 I-2：/new 的 clear 与串行锁内历史写盘的交错窗口由代际闸互斥——
    /// 快照旧代际的写盘在 /new 之后到达必须被拦（不残留 /new 前的旧对话）。
    #[tokio::test]
    async fn history_epoch_guard_blocks_stale_writes() {
        let runner = Arc::new(MockAgentRunner::immediate("ok"));
        let bot = backend_bot("pi");
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let hist = crate::history::History::open(&bot.key(), "oc_x");

        // 旧代际写盘（模拟 m1 在锁内、/new 尚未到达的快照）
        let lock = bridge.history_lock("oc_x");
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let epoch0 = *guard;
        hist.append_user("m1", "pi", "/new 之前的旧消息");
        drop(guard);
        assert!(!hist.entries().is_empty(), "同代际写盘放行");

        // /new：代际自增 + 清盘（同锁内原子完成）
        assert!(bridge.history_reset("oc_x"), "clear 成功");
        assert!(hist.entries().is_empty(), "clear 清掉旧条目");

        // 旧代际（epoch0）的写盘在 /new 之后到达 → 代际不符必须被拦
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        assert_ne!(*guard, epoch0, "代际已自增");
        assert!(*guard == epoch0 + 1);
        drop(guard);
        assert!(hist.entries().is_empty(), "历史保持空");

        // 新代际（/new 之后的新消息）恢复正常写
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        hist.append_user("m2", "pi", "新消息");
        drop(guard);
        assert_eq!(hist.entries().len(), 1);
        cleanup_bridge(&bridge);
    }

    /// T7：话题隔离——thread key（chat:thread）与主 chat 各自独立历史文件。
    #[tokio::test]
    async fn history_thread_isolation() {
        let runner = Arc::new(MockAgentRunner::immediate("ok"));
        let bot = backend_bot("pi");
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let thread_key = format!("oc_x:{}", "omt_1");
        let hist_thread = crate::history::History::open(&bot.key(), &thread_key);
        hist_thread.append_user("old1", "claude", "话题里的旧背景");
        let hist_main = crate::history::History::open(&bot.key(), "oc_x");
        assert!(hist_main.entries().is_empty(), "主 chat 无历史");

        // 话题消息 → 注入话题历史
        let mut ev = test_ev("m1", "oc_x", "继续");
        ev.thread_id = "omt_1".into();
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(ev).await })
            .await
            .unwrap();
        assert!(runner.prompts()[0].contains("话题里的旧背景"), "话题注入");

        // 主 chat 消息 → 无历史可注入
        let b2 = bridge.clone();
        tokio::spawn(async move { b2.handle(test_ev("m2", "oc_x", "hi")).await })
            .await
            .unwrap();
        assert!(
            !runner.prompts()[1].contains("[历史上下文]"),
            "主 chat 不串话题历史"
        );
        assert!(hist_thread.entries().len() >= 2, "话题历史独立累计");
        cleanup_bridge(&bridge);
    }

    /// T8：GitHub 分析（含合成 Ev）同样落历史——它是该通知群的 agent 轮次，
    /// 新后端能接续「刚才分析过什么」。助手条目记 reply 本体（300 字截断在
    /// history 层，与群摘要的 200 字截断各自独立）。

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
            role: crate::config::SenderRole::Owner,
            created_at: 10,
            reply: None,
        });
        bridge.pending.add(crate::pending::PendingItem {
            mid: "r2".into(),
            chat_id: "oc_r2".into(),
            chat_type: "group".into(),
            thread_id: String::new(),
            text: "第二条".into(),
            quoted: crate::messenger::QuotedContent::default(),
            attachments: Vec::new(),
            role: crate::config::SenderRole::Granted,
            created_at: 20,
            reply: None,
        });

        let stop = tokio_util::sync::CancellationToken::new();
        bridge.recover_pending(&stop).await;

        let prompts = runner.prompts();
        assert_eq!(prompts.len(), 2, "应按入队顺序重放");
        assert_eq!(prompts[0], "第一条");
        // 第二条是 granted 角色：重放时按原角色走受限分支（prompt 前置受限说明）
        assert!(prompts[1].contains("第二条"), "受限说明在前，原文在后");
        assert!(prompts[1].starts_with("[受限模式]"));
        assert_eq!(
            runner.roles(),
            [
                crate::config::SenderRole::Owner,
                crate::config::SenderRole::Granted
            ],
            "重放必须携带原角色（granted 不得被提升为 owner）"
        );
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

    /// 阶段 1（W2 窗口修复）：崩溃在「回复已产出、发送确认前」→ 重启恢复时
    /// **直接补发回复，不重跑 agent**（runner 收到 0 个 prompt；原语义此窗口
    /// 回复静默丢失）。补发成功清盘；失败留盘下次再试。
    #[tokio::test]
    async fn recover_redelivers_produced_reply_without_rerun() {
        let runner = Arc::new(MockAgentRunner::immediate("不应被重跑"));
        let (bridge, msgr) = build_test_bridge(runner.clone());
        // 模拟崩溃残留：回复已产出（set_reply 落盘）但发送确认前进程没了
        bridge.pending.add(crate::pending::PendingItem {
            mid: "w2".into(),
            chat_id: "oc_w2".into(),
            chat_type: "group".into(),
            thread_id: String::new(),
            text: "问题".into(),
            quoted: crate::messenger::QuotedContent::default(),
            attachments: Vec::new(),
            role: crate::config::SenderRole::Owner,
            created_at: 10,
            reply: None,
        });
        bridge.pending.set_reply("w2", "已产出的最终回复");

        let stop = tokio_util::sync::CancellationToken::new();
        bridge.recover_pending(&stop).await;

        assert!(
            runner.prompts().is_empty(),
            "有 reply 的条目不得重跑 agent（prompts 应为空）"
        );
        assert!(
            msgr.sent().iter().any(|t| t == "已产出的最终回复"),
            "直接补发已产出回复: {:?}",
            msgr.sent()
        );
        assert!(
            !msgr.sent().iter().any(|t| t.contains("正在恢复")),
            "补发不走重放提示（不是重跑）"
        );
        assert_eq!(
            msgr.sent()
                .iter()
                .filter(|t| *t == "已产出的最终回复")
                .count(),
            1,
            "恰发一条（不重复）"
        );
        assert!(bridge.pending.is_empty(), "补发成功清盘");
        cleanup_bridge(&bridge);
    }

    /// 阶段 1：补发失败（发送通道暂不可用）→ 条目留盘，下次启动再试（不重跑、不丢）。
    #[tokio::test]
    async fn recover_redeliver_failure_keeps_item() {
        let runner = Arc::new(MockAgentRunner::immediate("不应被重跑"));
        let bot = backend_bot("pi");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        *msgr.fail_chat.lock().unwrap() = Some("oc_w3".into()); // 发送通道故障
        bridge.pending.add(crate::pending::PendingItem {
            mid: "w3".into(),
            chat_id: "oc_w3".into(),
            chat_type: "group".into(),
            thread_id: String::new(),
            text: "问题".into(),
            quoted: crate::messenger::QuotedContent::default(),
            attachments: Vec::new(),
            role: crate::config::SenderRole::Owner,
            created_at: 10,
            reply: None,
        });
        bridge.pending.set_reply("w3", "待补发的回复");

        let stop = tokio_util::sync::CancellationToken::new();
        bridge.recover_pending(&stop).await;

        assert!(runner.prompts().is_empty(), "不重跑");
        assert_eq!(bridge.pending.len(), 1, "补发失败留盘");
        assert_eq!(
            bridge.pending.snapshot()[0].reply.as_deref(),
            Some("待补发的回复"),
            "reply 保留供下次补发"
        );
        cleanup_bridge(&bridge);
    }

    /// 阶段 1：话题（thread）消息的补发走 send_thread_reply（mid 必须正确）。
    #[tokio::test]
    async fn recover_redeliver_thread_reply() {
        let runner = Arc::new(MockAgentRunner::immediate("不应被重跑"));
        let (bridge, msgr) = build_test_bridge(runner.clone());
        bridge.pending.add(crate::pending::PendingItem {
            mid: "w4".into(),
            chat_id: "oc_w4".into(),
            chat_type: "group".into(),
            thread_id: "omt_9".into(), // 话题消息：send_reply 走 thread 分支
            text: "问题".into(),
            quoted: crate::messenger::QuotedContent::default(),
            attachments: Vec::new(),
            role: crate::config::SenderRole::Owner,
            created_at: 10,
            reply: None,
        });
        bridge.pending.set_reply("w4", "话题回复");

        let stop = tokio_util::sync::CancellationToken::new();
        bridge.recover_pending(&stop).await;

        assert!(runner.prompts().is_empty(), "不重跑");
        // MockMessenger 的 thread 回复路径：send_thread_reply 默认回落 send_text
        assert!(
            msgr.sent().iter().any(|t| t == "话题回复"),
            "话题补发: {:?}",
            msgr.sent()
        );
        assert!(bridge.pending.is_empty());
        cleanup_bridge(&bridge);
    }

    /// 审查 Important：Cancelled 臂必须先摘 pending 再发停止通知——若 remove 在发送后，
    /// 「发送期间崩溃」会让已叫停任务以 reply=None 残留 → 重启被普通重放续跑。
    #[tokio::test]
    async fn cancelled_arm_removes_pending_before_send() {
        let runner = Arc::new(MockAgentRunner::with_progress_cancel(&[]));
        let (bridge, _msgr) = build_test_bridge(runner.clone());
        // 任务跑到一半被打断（Cancelled）→ 返回后 pending 必须已摘（不残留可重放条目）
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_c", "跑起来")).await })
            .await
            .unwrap();
        assert!(
            bridge.pending.is_empty(),
            "Cancelled 臂在发送前摘 pending（无 reply 残留）"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn recover_pending_empty_is_noop() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, msgr) = build_test_bridge(runner.clone());
        let stop = tokio_util::sync::CancellationToken::new();
        bridge.recover_pending(&stop).await;
        assert!(runner.prompts().is_empty(), "无残留不应触发 agent");
        assert!(!msgr.sent().iter().any(|t| t.contains("正在恢复")));
        cleanup_bridge(&bridge);
    }

    // ---- on_payload 过滤（飞书 receive_v1）----

    // ─── #51 免 @ 群聊开关 ─────────────────────────────────────────

    /// /mention 指令解析表：精确匹配 + 大小写不敏感 + 非指令透传。
    #[test]
    fn parse_mention_cmd_table() {
        assert!(matches!(
            parse_mention_cmd("/mention"),
            Some(MentionCmd::Show)
        ));
        assert!(matches!(
            parse_mention_cmd(" /mention "),
            Some(MentionCmd::Show)
        ));
        assert!(matches!(
            parse_mention_cmd("/MENTION"),
            Some(MentionCmd::Show)
        ));
        assert!(matches!(
            parse_mention_cmd("/mention on"),
            Some(MentionCmd::On)
        ));
        assert!(matches!(
            parse_mention_cmd("/mention OFF"),
            Some(MentionCmd::Off)
        ));
        assert!(
            parse_mention_cmd("/mention on!").is_none(),
            "非精确匹配透传"
        );
        assert!(parse_mention_cmd("mention off").is_none());
        assert!(parse_mention_cmd("/mention x").is_none());
    }

    /// 群聊 @ 门槛端到端（验收核心）：/mention off 后未 @ 的顶层消息进 agent；
    /// /mention on 恢复过滤；无参显示状态；per-群隔离。
    #[tokio::test]
    async fn mention_off_allows_unmentioned_then_on_restores() {
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

        // 基线：未 @ 的顶层群消息被过滤（不进 agent）
        bridge
            .on_payload(
                feishu_payload(
                    "m0",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[],
                    "没@我",
                )
                .as_bytes(),
            )
            .await;
        assert!(runner.prompts().is_empty(), "未 @ 默认忽略");

        // @ 了 bot 发 /mention off → 确认回复 + 快照落 off
        bridge
            .on_payload(
                feishu_payload(
                    "m1",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[("庆小丰", "ou_bot")],
                    "/mention off",
                )
                .as_bytes(),
            )
            .await;
        assert!(
            msgr.sent().iter().any(|s| s.contains("已开启免 @")),
            "off 确认回复: {:?}",
            msgr.sent()
        );

        // off 后：未 @ 的顶层消息进 agent
        bridge
            .on_payload(
                feishu_payload(
                    "m2",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[],
                    "免 @ 的第一条",
                )
                .as_bytes(),
            )
            .await;
        assert_eq!(runner.prompts().len(), 1, "免 @ 后进 agent");
        assert!(runner.prompts()[0].contains("免 @ 的第一条"));

        // 无参显示状态
        bridge
            .on_payload(
                feishu_payload(
                    "m3",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[("庆小丰", "ou_bot")],
                    "/mention",
                )
                .as_bytes(),
            )
            .await;
        assert!(
            msgr.sent().iter().any(|s| s.contains("已开启免 @")),
            "无参显示免 @ 状态"
        );

        // /mention on 恢复：未 @ 消息再次被过滤
        bridge
            .on_payload(
                feishu_payload(
                    "m4",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[("庆小丰", "ou_bot")],
                    "/mention on",
                )
                .as_bytes(),
            )
            .await;
        bridge
            .on_payload(
                feishu_payload(
                    "m5",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[],
                    "又没@",
                )
                .as_bytes(),
            )
            .await;
        assert_eq!(
            runner.prompts().len(),
            1,
            "on 后未 @ 消息不再进 agent（仍是免 @ 那一条）"
        );

        // per-群隔离：oc_g 的 off 不影响 oc_other
        bridge
            .on_payload(
                feishu_payload(
                    "m6",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[("庆小丰", "ou_bot")],
                    "/mention off",
                )
                .as_bytes(),
            )
            .await;
        bridge
            .on_payload(
                feishu_payload(
                    "m7",
                    "oc_other",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[],
                    "别的群没@",
                )
                .as_bytes(),
            )
            .await;
        assert_eq!(runner.prompts().len(), 1, "开关 per-群隔离");

        // oc_other 独立开关往返（M7）：off 生效 → 显式 on 恢复要求 @
        bridge.set_mention_mode("oc_other", Some("off"));
        bridge
            .on_payload(
                feishu_payload(
                    "m8",
                    "oc_other",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[],
                    "别的群开了免@",
                )
                .as_bytes(),
            )
            .await;
        assert_eq!(runner.prompts().len(), 2, "oc_other 独立 off 生效");
        bridge.set_mention_mode("oc_other", Some("on"));
        bridge
            .on_payload(
                feishu_payload(
                    "m9",
                    "oc_other",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[],
                    "显式 on 又要求@",
                )
                .as_bytes(),
            )
            .await;
        assert_eq!(runner.prompts().len(), 2, "显式 on 条目仍要求 @");
        cleanup_bridge(&bridge);
    }

    /// 最高优先级安全回归（审查 I1）：免 @ 只放宽 @ 过滤一层——off 状态下
    /// 未授权用户仍在访问控制层被拒，绝不能进 agent。
    #[tokio::test]
    async fn mention_off_does_not_bypass_access_control() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        bridge.set_mention_mode("oc_g", Some("off"));
        // 未授权 sender（非 owner/授权者、非公开模式）+ 免 @ 群 → 仍被拒
        bridge
            .on_payload(
                feishu_payload(
                    "m1",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_stranger",
                    &[],
                    "陌生人的消息",
                )
                .as_bytes(),
            )
            .await;
        assert!(
            runner.prompts().is_empty(),
            "off 不绕过访问控制（未授权者被拒）"
        );
        cleanup_bridge(&bridge);
    }

    /// 开关是管理动作（用户拍板）：open_access 模式下陌生人可 @ 机器人对话，
    /// 但切换 /mention off/on 被拒——@ 门槛是公开群唯一的防洪闸。
    #[tokio::test]
    async fn open_access_stranger_cannot_toggle_mention() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            open_access: true, // 公开模式：任何人都可对话
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        // 陌生人 @ 机器人发 /mention off → 拒绝回显、开关不落
        bridge
            .on_payload(
                feishu_payload(
                    "m1",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_stranger",
                    &[("庆小丰", "ou_bot")],
                    "/mention off",
                )
                .as_bytes(),
            )
            .await;
        assert!(
            msgr.sent().iter().any(|s| s.contains("仅管理员")),
            "陌生人切换被拒: {:?}",
            msgr.sent()
        );
        assert!(bridge.mention_mode("oc_g").is_none(), "开关未被陌生人改动");
        // 未 @ 消息仍被 @ 门槛过滤（闸门没被关掉）
        bridge
            .on_payload(
                feishu_payload(
                    "m2",
                    "oc_g",
                    "group",
                    "",
                    "",
                    "user",
                    "ou_stranger",
                    &[],
                    "陌生人的未@消息",
                )
                .as_bytes(),
            )
            .await;
        assert!(runner.prompts().is_empty(), "@ 门槛仍生效");
        cleanup_bridge(&bridge);
    }

    /// 私聊 /mention → 仅顶层群聊可用提示，不写开关。
    #[tokio::test]
    async fn mention_in_p2p_replies_group_only() {
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
        bridge
            .on_payload(
                feishu_payload(
                    "m1",
                    "oc_p",
                    "p2p",
                    "",
                    "",
                    "user",
                    "ou_owner",
                    &[],
                    "/mention off",
                )
                .as_bytes(),
            )
            .await;
        assert!(
            msgr.sent().iter().any(|s| s.contains("仅顶层群聊可用")),
            "私聊提示: {:?}",
            msgr.sent()
        );
        assert!(runner.prompts().is_empty(), "不进 agent");
        assert!(
            bridge.mention_mode("oc_p").is_none(),
            "私聊不写开关（快照为空）"
        );
        cleanup_bridge(&bridge);
    }

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
    async fn on_payload_unactivated_bot_ignores_commands_and_chat() {
        // owner 空 = 未授权（默认封闭）：普通消息和 /new 等指令都不进 agent、无回复——
        // 只有发匹配的授权码才能激活（授权码消费走真实 config，单测里 bot 不在 config → NotFound
        // 静默忽略，故这里只验「未激活时所有非授权码消息都被拦」）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "新bot".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "".into(), // 未授权
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        for (mid, text) in [("om_c1", "你好"), ("om_c2", "/new")] {
            let payload = feishu_payload(
                mid,
                "oc_p2p",
                "p2p",
                "",
                "",
                "user",
                "ou_stranger",
                &[],
                text,
            );
            bridge.on_payload(payload.as_bytes()).await;
        }

        assert!(
            runner.prompts().is_empty(),
            "未授权消息（含 /new）不应触发 agent"
        );
        assert!(_msgr.sent().is_empty(), "未授权消息不应有回复");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_owner_whitelist_allows_members_only() {
        // 多 owner 白名单（逗号分隔）：白名单内放行进 agent，名单外忽略
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss, ou_friend".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        // 名单外 → 忽略
        let payload = feishu_payload(
            "om_out",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_stranger",
            &[],
            "你好",
        );
        bridge.on_payload(payload.as_bytes()).await;
        assert!(runner.prompts().is_empty(), "名单外不应触发 agent");

        // 名单内（第二个 open_id）→ 放行
        let payload = feishu_payload(
            "om_in",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_friend",
            &[],
            "你好",
        );
        bridge.on_payload(payload.as_bytes()).await;
        assert_eq!(runner.prompts(), vec!["你好"], "白名单成员应触发 agent");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_granted_gets_restricted_session() {
        // 授权者（granted_ids 成员）消息：角色=Granted 传给 agent（受限分支），
        // prompt 前置受限说明（CLAUDE.md 共享，只能靠 prompt 区分角色）
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss".into(),
            granted_ids: "ou_friend".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let payload = feishu_payload(
            "om_g",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_friend",
            &[],
            "帮我分析附件",
        );
        bridge.on_payload(payload.as_bytes()).await;

        let prompts = runner.prompts();
        assert_eq!(prompts.len(), 1, "授权者应触发 agent");
        assert!(
            prompts[0].starts_with("[受限模式]"),
            "granted 会话 prompt 应前置受限说明"
        );
        assert!(prompts[0].contains("帮我分析附件"));
        assert_eq!(
            runner.roles(),
            [crate::config::SenderRole::Granted],
            "授权者消息必须以 Granted 角色进入 agent 调用"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_owner_gets_full_session() {
        // owner 消息：角色=Owner，prompt 不带受限说明（全权限会话不受影响）
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss".into(),
            granted_ids: "ou_friend".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let payload = feishu_payload(
            "om_o",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_boss",
            &[],
            "随便聊聊",
        );
        bridge.on_payload(payload.as_bytes()).await;

        let prompts = runner.prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0], "随便聊聊", "owner 会话 prompt 不应带受限说明");
        assert_eq!(
            runner.roles(),
            [crate::config::SenderRole::Owner],
            "owner 消息以 Owner 角色进入 agent 调用"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_cancel_without_task_replies_and_no_agent() {
        // /cancel 无任务在跑 → 明确回复「当前没有正在运行的任务」，不透传给 agent
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
            "om_cancel",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_owner",
            &[],
            "/cancel",
        );
        bridge.on_payload(payload.as_bytes()).await;

        assert!(runner.prompts().is_empty(), "/cancel 不应触发 agent");
        assert!(
            msgr.sent()
                .iter()
                .any(|t| t.contains("当前没有正在运行的任务")),
            "无任务时应给明确反馈"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_cancel_interrupts_running_task() {
        // /cancel 在任务运行中 → 打断（mock 返回 Cancelled）→ 回「⏹ 已停止」
        let runner = Arc::new(MockAgentRunner::blocking("done"));
        let (bridge, msgr) = build_test_bridge(runner.clone());

        // 普通消息触发 agent（挡板挂住等 release/cancel）
        let b1 = bridge.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m1", "oc_x", "hello")).await });
        runner.started.notified().await; // agent 已进入「运行中」、cancel_flags 已注册

        // 任务运行中发 /cancel → 打断
        bridge.handle(test_ev("m2", "oc_x", "/cancel")).await;
        task.await.unwrap();

        assert!(
            msgr.sent().iter().any(|t| t == "⏹ 已停止"),
            "被打断的任务应回「⏹ 已停止」"
        );
        // 打断不算完成：started 不应被 mark（Cancelled 路径不 mark）
        assert!(!bridge.sessions.is_started("oc_x"));
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
        assert_eq!(runner.prompts(), ["[引用消息]\n被引用的原消息\n\n回复内容"]);
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
            ding_owner_ids: "u1".into(), // 私有模式 + owner=u1（测试 sender）
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

        assert_eq!(runner.prompts(), ["[引用消息]\n被引用的原消息\n\n回复内容"]);
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

        // 文本+附件同时存在：整段精确断言，钉死「[引用附件] 独占一行」的格式
        assert_eq!(
            runner.prompts(),
            ["[引用消息]\n引用的文字\n[引用附件]\n[image] 来源=mock 文件名=截图.png mime=application/octet-stream 大小=1 本地路径=/tmp/mock-attachment.bin sha256=abc\n\n回复内容"]
        );
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
        assert!(prompt.contains("\n[引用附件]"), "[引用附件] 应独占一行");
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
            ding_owner_ids: "u1".into(), // 私有模式 + owner=u1（测试 sender）
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
        assert!(prompt.contains("\n[引用附件]"), "[引用附件] 应独占一行");
        assert!(prompt.contains("image"));
        assert!(prompt.contains("回复内容"));
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_payload_multi_image_post_downloads_all() {
        // 多图：飞书 post 多图经 on_payload 主路径应全部下载进 [附件]（#5 一致性）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        let content = serde_json::json!({
            "title": "多图",
            "content": [[{"tag":"img","image_key":"img_1"}],[{"tag":"img","image_key":"img_2"}],[{"tag":"img","image_key":"img_3"}]]
        });
        let payload = serde_json::json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_type":"user","sender_id":{"open_id":"ou_owner"}},
                "message": {"message_id":"om_multi","chat_id":"oc_p2p","chat_type":"p2p","content": content.to_string()}
            }
        })
        .to_string();
        bridge.on_payload(payload.as_bytes()).await;
        assert_eq!(
            runner.prompts()[0].matches("本地路径=").count(),
            3,
            "多图应全部下载"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn handle_multi_attachment_and_multilink() {
        // 多附件 + 多链接 + 引用多附件：handle 拼装各段多件正确性。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, _msgr) = build_test_bridge(runner.clone());
        let att = |kind: &str, name: &str, path: &str| crate::attachments::AttachmentMeta {
            kind: kind.into(),
            source: "mock".into(),
            file_name: name.into(),
            mime: "application/octet-stream".into(),
            size: 1,
            path: path.into(),
            sha256: "h".into(),
            note: String::new(),
        };
        let mut ev = test_ev(
            "m1",
            "oc_q",
            "看这个 https://a.com/x 和 https://b.com/y 如何",
        );
        ev.attachments = vec![
            att("image", "a.png", "/tmp/a.png"),
            att("file", "报告.pdf", "/tmp/r.pdf"),
            att("video", "v.mp4", "/tmp/v.mp4"),
        ];
        ev.quoted = crate::messenger::QuotedContent {
            text: "被引用".into(),
            attachments: vec![
                att("image", "q1.png", "/tmp/q1.png"),
                att("file", "q2.zip", "/tmp/q2.zip"),
            ],
        };
        bridge.handle(ev).await;
        let p = &runner.prompts()[0];
        assert_eq!(
            p.matches("本地路径=").count(),
            5,
            "引用 2 + 主 3 = 5 个附件行"
        );
        assert!(p.contains(
            "[引用附件]
"
        ));
        assert!(p.contains("报告.pdf") && p.contains("v.mp4"));
        assert!(p.contains(
            "[链接]
https://a.com/x
https://b.com/y"
        ));
        cleanup_bridge(&bridge);
    }

    /// 打字机下线：带中途输出的任务只发最终结果一条，中途处理过程消息一律不回。
    /// run 立即返回（block=false）→ 进度可能还压在通道里 → 同时覆盖 select 丢弃
    /// 与收尾排空（try_recv drain）两条丢弃路径（断言只看最终发送面，时序无关）。
    #[tokio::test]
    async fn final_only_drops_progress_sends_final_reply() {
        let runner = Arc::new(MockAgentRunner::with_progress_immediate(
            "最终结果",
            &["中途1", "中途2", "中途3"],
        ));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        };
        let msgr = Arc::new(MockMessenger::new());
        let bridge = Arc::new(Bridge::build(
            msgr.clone(),
            bot,
            &Config::default(),
            runner.clone(),
        ));

        let b1 = bridge.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m1", "oc_fin", "hi")).await });
        task.await.unwrap();

        assert_eq!(
            msgr.sent(),
            vec!["最终结果"],
            "中途进度不得发送，只发最终结果一条"
        );
        cleanup_bridge(&bridge);
    }

    /// 任务出错 → 错误文案作为最终回复恰好发一条（中途进度丢弃）。
    #[tokio::test]
    async fn final_only_error_sends_error_once() {
        let runner = Arc::new(MockAgentRunner::with_progress_error(
            &["部分输出"],
            "后端进程退出码 1",
        ));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        };
        let msgr = Arc::new(MockMessenger::new());
        let bridge = Arc::new(Bridge::build(
            msgr.clone(),
            bot,
            &Config::default(),
            runner.clone(),
        ));

        let b1 = bridge.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m1", "oc_err", "hi")).await });
        task.await.unwrap();

        assert_eq!(msgr.sent(), vec!["后端进程退出码 1"]);
        cleanup_bridge(&bridge);
    }

    /// 打字机下线：任务被打断时中途进度同样丢弃，只回「⏹ 已停止」一条。
    #[tokio::test]
    async fn cancel_drops_progress_sends_stopped_once() {
        let runner = Arc::new(MockAgentRunner::with_progress_cancel(&["中途输出"]));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        };
        let msgr = Arc::new(MockMessenger::new());
        let bridge = Arc::new(Bridge::build(
            msgr.clone(),
            bot,
            &Config::default(),
            runner.clone(),
        ));

        let b1 = bridge.clone();
        let task = tokio::spawn(async move { b1.handle(test_ev("m1", "oc_cx", "hi")).await });
        task.await.unwrap(); // block=false，无需 release

        assert_eq!(
            msgr.sent(),
            vec!["⏹ 已停止"],
            "中途进度不得发送，只回「⏹ 已停止」一条"
        );
        cleanup_bridge(&bridge);
    }

    /// #69 审查 Important：关停广播后恢复重放立即停止，未重放条目留盘 pending.json
    /// （下次启动续跑——恢复任务跑完整 agent 管线，不能让 shutdown_wait 无界等它）。
    #[tokio::test]
    async fn recover_pending_stops_on_cancel() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (bridge, _msgr) = build_test_bridge(runner.clone());
        for (i, mid) in ["r1", "r2"].iter().enumerate() {
            bridge.pending.add(crate::pending::PendingItem {
                mid: (*mid).into(),
                chat_id: format!("oc_{mid}"),
                chat_type: "group".into(),
                thread_id: String::new(),
                text: format!("第{i}条"),
                quoted: crate::messenger::QuotedContent::default(),
                attachments: Vec::new(),
                role: crate::config::SenderRole::Owner,
                created_at: i as u64,
                reply: None,
            });
        }
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        bridge.recover_pending(&stop).await;
        assert!(
            runner.prompts().is_empty(),
            "取消后不重放任何条目: {:?}",
            runner.prompts()
        );
        assert_eq!(
            bridge.pending.snapshot().len(),
            2,
            "未重放条目留盘，下次启动续跑"
        );
        cleanup_bridge(&bridge);
    }
}
