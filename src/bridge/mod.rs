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

mod access;
mod outbox;
mod recover;
mod teamflow;
mod virtualbot;

pub struct Bridge {
    pub msgr: Arc<dyn Messenger>,
    pub sessions: SessionStore,
    /// #200 Phase 2：mini-relay 状态（buzz 后端的消息经它注入 buzz-acp）。
    /// None = 未启用（buzz_relay_enabled=false）或初始化失败。service 启动时同步
    /// 注入（bot 循环前 relay 已就绪），无「bot 先于 relay 初始化」竞态。
    pub buzz_relay_state: Option<Arc<crate::buzzrelay::RelayState>>,
    /// #194：虚拟 Bot 群的独立会话存储（per-chat 缓存；键=chat_id）。
    vb_sessions: Mutex<HashMap<String, SessionStore>>,
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
    /// 一键创建团队·聊天入口的会话态（#124 P1）：按 chat key 持久化
    /// `workspaces/<bot>/teamflow.json`，重启不丢（确认/改/取消可跨重启）。
    team_flows: crate::teamflow::TeamFlowStore,
    /// 团队方案生成器（#124 测试可测性）：生产 RealTeamPlanGenerator 转发 teambuilder；
    /// 测试注入挡板返回固定方案/错误（仿 agent_runner 设计）。
    team_gen: Arc<dyn crate::teamflow::TeamPlanGenerator>,
    /// Agent 执行器（#23 测试可测性）：仿 `msgr` 的 trait 注入——生产用 RealAgentRunner
    /// 转发 spawn 子进程，测试注入挡板以驱动「任务运行中」时序（详见 agent::AgentRunner）。
    /// pub(crate)：session_gc::run_once 经它走归纳调用（与聊天/job 同源，可挡板测试）。
    pub(crate) agent_runner: Arc<dyn AgentRunner>,
    /// 虚拟 Bot 登记快照（#75）：启动时加载；注入判定前按文件 mtime 懒刷新
    /// （GUI 登记/取消登记即时生效，无需重启 service）。读-改由 VirtualBotStore 原子
    /// 整文件重写，这里只读快照——跨进程无锁安全。
    virtual_bots: Mutex<Vec<crate::virtualbot::VirtualBot>>,
    /// 登记表 (mtime, 长度) 签名（懒刷新判定：没变就不重读）。
    virtual_bots_mtime: Mutex<Option<(std::time::SystemTime, u64)>>,
    /// 群资料缓存（#75，5 分钟 TTL）：事件群名 + API 群介绍。「改群介绍即时生效」
    /// 的载体——每次注入前查缓存，缓存过期自然刷新，不做变更推送。
    chat_info_cache: crate::virtualbot::ChatInfoCache,
    /// #74 授权者私聊历史库。生产 = ~/.agent-bridge/messages.sqlite（MsgStore::production）；
    /// 测试注入临时路径——handle 内的落库绝不能碰真实用户消息库。
    pub msgstore: crate::msgstore::MsgStore,
    /// #74 未读提醒队列（logs/unread.json 写句柄）。同上：测试注入临时路径。
    pub unread: crate::unread::UnreadStore,
    /// 虚拟 Bot 登记表（#75）：事件驱动移除（im.chat.deleted_v1 群被解散）用它写；
    /// 生产 = ~/.agent-bridge/virtual-bots.json，测试注入临时路径（同 msgstore/unread）。
    pub vb_store: crate::virtualbot::VirtualBotStore,
    /// #87 会话管控状态（暂停/恢复）。生产 = ~/.agent-bridge/session_state.json，
    /// 热重载——CLI pause/resume 落盘后无需重启即生效（测试注入临时路径）。
    pub session_state: crate::session_state::SessionState,
    /// 三级 AGENTS.md 注入根目录（abb 级文件所在）。生产 = ~/.agent-bridge；测试注入
    /// temp 根——现有测试断言 prompt 精确相等（如 on_payload_owner_gets_full_session），
    /// 真实 ~/.agent-bridge/AGENTS.md 若存在会破坏它们（同 mention_snapshot 的
    /// 「测试快照回落」模式：测试不碰真实配置/数据）。
    /// 三级 AGENTS.md / 会话摘要的注入根（生产 = `~/.agent-bridge`；测试注入 temp 根，
    /// 不碰真实 ~/.agent-bridge/AGENTS.md）。session_gc::run_once 与 bridge 注入点共用。
    pub agents_md_root: std::path::PathBuf,
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
    /// 发送者 id（飞书 open_id / 微信 ilink user_id / 钉钉 staffId）。
    /// #74 历史记录（msgstore）与未读提醒（unread.json）的发送者标识。
    pub sender_id: String,
    /// 消息事件时间（unix 秒；#74 历史排序/提醒时间显示）。
    /// 各平台事件时间：飞书 header.create_time（毫秒）、微信 create_time_ms；
    /// 钉钉事件体无时间字段 → 当前时间（见各入口注释）。
    pub ts: i64,
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
    /// 注册/摘除一个 cancel 标志（聊天任务与定时任务共用）：
    /// 定时任务（run_job）也注册到目标 chat_id，用户在该会话发「停止」即可打断后台任务。
    /// 返回的 flag 传给 agent::run 的 cancel 参数；任务结束（含错误/取消路径）必须 remove。
    pub(crate) fn register_cancel_flag(&self, key: &str) -> Arc<std::sync::atomic::AtomicBool> {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.cancel_flags
            .lock()
            .unwrap()
            .insert(key.to_string(), flag.clone());
        flag
    }

    pub(crate) fn unregister_cancel_flag(&self, key: &str) {
        self.cancel_flags.lock().unwrap().remove(key);
    }
    /// #118：未授权 / granted-pi 拦截统一落历史（接入层静默拦截，无提示文案）。
    /// p2p：落历史 + #74 未读提醒（owner 可见谁在找 bot）；group：落历史、不提醒。
    /// mid 为空（事件缺 id）时跳过——与主路径缺 mid 直接忽略的口径一致。
    async fn record_intercepted(
        &self,
        bot_key: &str,
        message: &serde_json::Value,
        body: &serde_json::Value,
        sender_id: &str,
        chat_type: &str,
    ) {
        let mid = message["message_id"].as_str().unwrap_or("").to_string();
        if mid.is_empty() {
            return;
        }
        let ts = body["header"]["create_time"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| body["header"]["create_time"].as_i64())
            .map(|ms| ms / 1000)
            .unwrap_or_else(|| crate::chrono_lite::unix_secs() as i64);
        let chat_id = message["chat_id"].as_str().unwrap_or("").to_string();
        let text = crate::feishu::parse_content(message["content"].as_str().unwrap_or(""))
            .text
            .trim()
            .to_string();
        // 展示名：拦截用户不在本地名单，API 反查（best-effort；失败空串由 GUI 回落 id）
        let uname = self
            .msgr
            .user_display_name(sender_id)
            .await
            .unwrap_or_default();
        self.msgstore.insert(
            bot_key, &chat_id, &mid, "user", sender_id, &uname, &text, ts,
        );
        if chat_type == "p2p" {
            self.unread.report(
                bot_key,
                sender_id,
                &uname,
                &crate::agent::truncate(&text, 40),
                ts,
            );
        }
    }

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
            buzz_relay_state: None, // #200：service 启动后由 buzz_relay_enabled 分支注入
            vb_sessions: Mutex::new(HashMap::new()),
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
            team_flows: crate::teamflow::TeamFlowStore::new(&key),
            team_gen: Arc::new(crate::teamflow::RealTeamPlanGenerator),
            agent_runner,
            // #75：启动即加载登记快照 + 记录文件签名（后续由 refresh_virtual_bots 懒刷新）
            virtual_bots: Mutex::new(crate::virtualbot::VirtualBotStore::new().load()),
            virtual_bots_mtime: Mutex::new(
                std::fs::metadata(crate::bridge_dir().join("virtual-bots.json"))
                    .ok()
                    .map(|m| {
                        (
                            m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                            m.len(),
                        )
                    }),
            ),
            chat_info_cache: crate::virtualbot::ChatInfoCache::new(),
            msgstore: crate::msgstore::MsgStore::production(),
            unread: crate::unread::UnreadStore::production(),
            vb_store: crate::virtualbot::VirtualBotStore::new(),
            session_state: crate::session_state::SessionState::production(),
            agents_md_root: crate::bridge_dir(),
        }
    }

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
    pub(crate) fn history_lock(&self, key: &str) -> Arc<std::sync::Mutex<u64>> {
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
    /// DATA 帧 payload 入口：解析 v2 事件 → 过滤 → handle。
    pub async fn on_payload(&self, payload: &[u8]) {
        let body: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(_) => return,
        };
        let event_type = body["header"]["event_type"].as_str().unwrap_or("");
        if event_type == "im.chat.deleted_v1" {
            // 虚拟 Bot：群被平台解散 → 自动移除登记（8-20 用户决策——ABB 不残留
            // 幽灵登记：deliver @角色名不再指向死群、GUI 列表不再显示无效项）
            self.on_chat_deleted(&body["event"]).await;
            return;
        }
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
        // 虚拟 Bot #75：事件自带的群名（取不到为空）。比 API 查询新——平台改群名，
        // 下一条消息注入的就是新名（入库见下方 chat_id 处，只对群聊记）。
        let chat_name = message["chat_name"].as_str().unwrap_or("").to_string();
        let mentions: Vec<serde_json::Value> =
            message["mentions"].as_array().cloned().unwrap_or_default();

        if chat_type == "group" {
            crate::log!("[群] bot@={}", self.bot_is_mentioned(&mentions));
        }

        // should_respond（访问控制，默认私有，#118 收紧）：只放行 owner（管理员）∪ 授权者
        // （授权码添加）白名单。**无公开开关、无群聊豁免**——群里任何人未经授权也拦截。
        // 未授权者只能通过授权码激活。每次消息从 config.json 热读最新访问控制（授权/取消
        // 即时生效，不依赖启动快照）；config 读不到该 bot（单测注入）→ 回落构造时的快照。
        let (allowed, sender_role, mention_map, mention_default) = self.access_and_role(sender_id);
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
                "[bridge] 忽略未授权消息（bot={} sender={} chat_type={}）",
                self.bot.key(),
                sender_id,
                chat_type
            );
            // #118：未授权一律拦截且**无提示文案**；记录历史——p2p 保留 #74 提醒+落历史；
            // 群聊落历史、不提醒（不再完全忽略）。
            self.record_intercepted(&self.bot.key(), message, &body, sender_id, chat_type)
                .await;
            return;
        }
        // #118：granted + pi 后端 + 隔离开 → 接入层静默拦截（落历史不回复，
        // 不再出现「后端是 pi」提示文案外泄；job 路径保留 agent::run 防御兜底）。
        let backend = self.bot.effective_backend(&self.default_backend);
        if crate::config::granted_pi_unusable(sender_role, &self.bot.key(), backend) {
            crate::log!(
                "[bridge] granted+pi 会话静默拦截（bot={} sender={}）",
                self.bot.key(),
                sender_id
            );
            self.record_intercepted(&self.bot.key(), message, &body, sender_id, chat_type)
                .await;
            return;
        }
        // 群聊只有 @ 了本机器人（或话题内回复）的消息才处理：
        // 话题（thread）内用户回复机器人的消息不需要再次 @——这是「用户回复」的主流交互；
        // 顶层群消息仍要求 @（避免整个群的消息都进 agent）。
        // #51：该群设了免 @（mention_modes off）则顶层消息也进 agent——热读即时生效。
        // 门槛判定复用 access_and_role 同一次 config load（mention_off），
        // 已 @ 则短路不付门槛判定（已 @ 的消息本就无需门槛）。
        let chat_id = message["chat_id"].as_str().unwrap_or("");
        // #75：事件群名入缓存（只对群聊；非群事件该字段恒空，note 内部过滤）
        if chat_type == "group" {
            self.chat_info_cache.note_event_name(chat_id, &chat_name);
        }
        if chat_type == "group"
            && thread_id.is_empty()
            && !self.bot_is_mentioned(&mentions)
            && !self.mention_off(&mention_map, chat_id, mention_default)
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

        // #74 事件时间：飞书事件头 create_time 是毫秒字符串；缺失/解析失败回落当前时间
        // （历史排序/提醒时间显示用，精确到秒足够）。
        let ts = body["header"]["create_time"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| body["header"]["create_time"].as_i64())
            .map(|ms| ms / 1000)
            .unwrap_or_else(|| crate::chrono_lite::unix_secs() as i64);

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
            sender_id: sender_id.to_string(),
            ts,
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
}

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
/// 存储层保真（事件派生，仅 2 万保险上限）——这里只负责按重要性排布（用户文本最前，
/// 注入时超预算才切，切的是尾巴=次要信息）。与 prompt 的 [引用消息]/[附件] 段同源同格式。
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
    use crate::teambuilder::{TeamPlan, TeamRole};

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
            sender_id: String::new(),
            ts: 0,
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
            sender_id: String::new(),
            ts: 0,
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
        /// done 回执收集（W2 补发补 DONE 断言用；实时路径 handle 尾部也会调）。
        done: Mutex<Vec<String>>,
        /// 群资料（#75 注入测试）：None=查不到（默认）。
        chat_info: Mutex<Option<(String, String)>>,
        /// #124 一键创建团队：已建群名（create_chat 成功记录）。
        created: Mutex<Vec<String>>,
        /// #124 建群失败注入：命中角色名 → Err（测部分失败清单）。
        fail_create: Mutex<Option<String>>,
    }
    impl MockMessenger {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                sent_chats: Mutex::new(Vec::new()),
                quoted: Mutex::new(std::collections::HashMap::new()),
                fail_chat: Mutex::new(None),
                done: Mutex::new(Vec::new()),
                chat_info: Mutex::new(None),
                created: Mutex::new(Vec::new()),
                fail_create: Mutex::new(None),
            }
        }
        fn set_fail_create(&self, role: &str) {
            *self.fail_create.lock().unwrap() = Some(role.to_string());
        }
        fn clear_fail_create(&self) {
            *self.fail_create.lock().unwrap() = None;
        }
        fn created(&self) -> Vec<String> {
            self.created.lock().unwrap().clone()
        }
        fn set_chat_info(&self, name: &str, desc: &str) {
            *self.chat_info.lock().unwrap() = Some((name.to_string(), desc.to_string()));
        }
        fn done_calls(&self) -> Vec<String> {
            self.done.lock().unwrap().clone()
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
        async fn done(&self, message_id: &str) {
            self.done.lock().unwrap().push(message_id.to_string());
        }
        async fn get_quoted_message(
            &self,
            message_id: &str,
        ) -> Option<crate::messenger::QuotedMessage> {
            self.quoted.lock().unwrap().get(message_id).cloned()
        }
        async fn get_chat_info(&self, _chat_id: &str) -> Option<(String, String)> {
            self.chat_info.lock().unwrap().clone()
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
        async fn create_chat(
            &self,
            name: &str,
            _description: &str,
            _owner_user_id: &str,
        ) -> Result<String, String> {
            if let Some(f) = self.fail_create.lock().unwrap().clone() {
                if f == name {
                    return Err(format!("模拟建群失败：{name}"));
                }
            }
            let n = self.created.lock().unwrap().len();
            self.created.lock().unwrap().push(name.to_string());
            Ok(format!("oc_new_{n}"))
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
        /// #130：前 N 次 run 返回 Err(err)，随后正常 Reply（上下文压缩重试测试用）。
        fail_then_reply: Mutex<Option<(usize, String)>>,
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
                fail_then_reply: Mutex::new(None),
            }
        }
        /// #130：前 n 次调用返回 Err(err)，随后正常 Reply。
        fn fail_then_reply(&self, n: usize, err: &str) {
            *self.fail_then_reply.lock().unwrap() = Some((n, err.to_string()));
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
            if backend == Backend::Pi {
                let _ = write_pi_session_file(bot_key, session_id);
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
            // #130：前 N 次失败后正常（fail_then_reply 优先于 outcome）
            if let Some((n, err)) = self.fail_then_reply.lock().unwrap().as_mut() {
                if *n > 0 {
                    *n -= 1;
                    return Err(err.clone());
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

    /// #124 团队方案生成挡板：固定返回 2 角色方案 / 指定错误；记录收到的 goal
    /// （断言「改：xxx」把调整要求合并进目标）。不碰真实 LLM。
    struct MockTeamPlanGenerator {
        plan: Mutex<Option<TeamPlan>>,
        err: Mutex<Option<String>>,
        goals: Mutex<Vec<String>>,
    }
    impl MockTeamPlanGenerator {
        fn ok() -> Self {
            Self {
                plan: Mutex::new(Some(mock_team_plan())),
                err: Mutex::new(None),
                goals: Mutex::new(Vec::new()),
            }
        }
        fn fail(msg: &str) -> Self {
            Self {
                plan: Mutex::new(None),
                err: Mutex::new(Some(msg.to_string())),
                goals: Mutex::new(Vec::new()),
            }
        }
        fn goals(&self) -> Vec<String> {
            self.goals.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl crate::teamflow::TeamPlanGenerator for MockTeamPlanGenerator {
        async fn generate(
            &self,
            _backend: Backend,
            goal: &str,
            _members: &[String],
            _template: Option<&str>,
        ) -> Result<TeamPlan, String> {
            self.goals.lock().unwrap().push(goal.to_string());
            if let Some(e) = self.err.lock().unwrap().clone() {
                return Err(e);
            }
            Ok(self.plan.lock().unwrap().clone().expect("mock plan"))
        }
    }

    /// #124 测试用固定方案：2 角色（开发/测试），全部待任命。
    fn mock_team_plan() -> TeamPlan {
        TeamPlan {
            team_name: "测试团队".into(),
            roles: vec![
                TeamRole {
                    role_name: "开发".into(),
                    member_name: None,
                    system_prompt: "负责开发".into(),
                },
                TeamRole {
                    role_name: "测试".into(),
                    member_name: None,
                    system_prompt: "负责测试".into(),
                },
            ],
            collab: None,
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
            sender_id: String::new(),
            ts: 0,
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
    /// #74：msgstore/unread 注入临时路径——handle 内的落库/提醒测试不得碰真实
    /// ~/.agent-bridge 数据（消息库是全局文件，不像 workspaces 可按 bot key 隔离）。
    fn build_test_bridge_with_bot(
        runner: Arc<dyn AgentRunner>,
        bot: BotConfig,
    ) -> (Arc<Bridge>, Arc<MockMessenger>) {
        let msgr = Arc::new(MockMessenger::new());
        let mut bridge = Bridge::build(msgr.clone(), bot, &Config::default(), runner);
        // 按 bot key 命名（key 本身唯一），cleanup_bridge 可按 key 回收
        let key = bridge.bot.key();
        bridge.msgstore = crate::msgstore::MsgStore::at(
            std::env::temp_dir().join(format!("abb-msgstore-test-{key}")),
        );
        bridge.unread = crate::unread::UnreadStore::at(
            std::env::temp_dir().join(format!("abb-unread-test-{key}")),
        );
        bridge.vb_store = crate::virtualbot::VirtualBotStore::new_at(
            std::env::temp_dir().join(format!("abb-vb-test-{key}.json")),
        );
        // #87 会话管控状态注入临时路径：不碰真实 ~/.agent-bridge/session_state.json
        bridge.session_state = crate::session_state::SessionState::at(
            std::env::temp_dir().join(format!("abb-sessstate-test-{key}.json")),
        );
        // 三级 AGENTS.md 注入根注入临时目录：不碰真实 ~/.agent-bridge/AGENTS.md
        //（现有测试断言 prompt 精确相等，真实 abb 级文件会破坏它们）
        bridge.agents_md_root = std::env::temp_dir().join(format!("abb-agentsmd-root-{key}"));
        (Arc::new(bridge), msgr)
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
        // #74：回收测试注入的临时消息库/未读文件（按 bot key 命名 + WAL 伴生文件，
        // 见 build_test_bridge_with_bot）
        let key = bridge.bot.key();
        for (prefix, suffix) in [
            ("abb-msgstore-test-", ""),
            ("abb-msgstore-test-", "-wal"),
            ("abb-msgstore-test-", "-shm"),
            ("abb-unread-test-", ""),
            ("abb-vb-test-", ".json"),
            ("abb-agentsmd-root-", ""),
        ] {
            let _ =
                std::fs::remove_file(std::env::temp_dir().join(format!("{prefix}{key}{suffix}")));
        }
        // agents_md_root 是目录（workspaces/<key>/ 含子目录），整树删除
        let _ =
            std::fs::remove_dir_all(std::env::temp_dir().join(format!("abb-agentsmd-root-{key}")));
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

    /// #130：上下文超长 → 自动压缩（旧段摘要 + 近期原文）→ 换新会话重试一次，
    /// 回复前置压缩提示；原历史保留不删；二次超长（已压缩过）不重复压缩。
    #[tokio::test]
    async fn context_too_long_auto_compresses_and_retries() {
        let runner = Arc::new(MockAgentRunner::immediate("压缩后重试成功"));
        // 首次 run 失败（超长），随后的摘要任务/重试均正常
        runner.fail_then_reply(1, "claude: prompt is too long (context window exceeded)");
        let bot = backend_bot("claude");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        // 预写 14 轮历史（> keep_recent=10，触发压缩）
        let hist = crate::history::History::open(&bot.key(), "oc_x");
        for i in 0..14 {
            hist.append_user(&format!("u{i}"), "claude", &format!("第{i}轮用户问题"));
            hist.append_assistant(&format!("u{i}"), "claude", &format!("第{i}轮回答"));
        }
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "继续聊")).await })
            .await
            .unwrap();

        let prompts = runner.prompts();
        assert!(
            prompts.len() >= 2,
            "首轮失败 + 压缩重试（+摘要任务），实际 {} 次",
            prompts.len()
        );
        // 首轮注入全量历史；重试轮注入压缩块 + 保留当前消息
        assert!(
            prompts[0].contains("[历史上下文]"),
            "首轮全量历史: {}",
            prompts[0]
        );
        let last = prompts.last().unwrap();
        assert!(
            last.contains("[历史上下文·压缩版]"),
            "重试轮注入压缩块: {last}"
        );
        assert!(last.contains("继续聊"), "当前消息保留: {last}");
        // 用户可见压缩提示 + 重试回复
        assert!(
            msgr.sent()[0].contains("已自动压缩"),
            "回复带压缩提示: {}",
            msgr.sent()[0]
        );
        assert!(msgr.sent()[0].contains("压缩后重试成功"));
        // ctxsum 落盘（压缩块）+ 原历史保留
        let workspace = crate::workspace_dir(&bot.key());
        let sum = crate::contextsum::ctxsum_block_at(&workspace, "oc_x").expect("ctxsum 已写");
        assert!(sum.contains("## 旧对话摘要"));
        assert!(sum.contains("## 近期原文"));
        assert_eq!(
            crate::history::History::open(&bot.key(), "oc_x")
                .entries()
                .len(),
            30, // 14 轮 + 本轮用户轮 + 重试成功的助手轮
            "原历史保留不删"
        );
        cleanup_bridge(&bridge);
    }

    /// #130：已压缩过（ctxsum 存在）再超长 → 不重复压缩，错误原样返回（防循环）。
    #[tokio::test]
    async fn context_too_long_no_second_compression() {
        let runner = Arc::new(MockAgentRunner::immediate("不应走到"));
        runner.fail_then_reply(2, "codex: context length exceeded");
        let bot = backend_bot("codex");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        let hist = crate::history::History::open(&bot.key(), "oc_x");
        for i in 0..14 {
            hist.append_user(&format!("u{i}"), "codex", &format!("第{i}轮用户问题"));
            hist.append_assistant(&format!("u{i}"), "codex", &format!("第{i}轮回答"));
        }
        // 预置 ctxsum（模拟已压缩过）
        let workspace = crate::workspace_dir(&bot.key());
        std::fs::write(
            crate::contextsum::ctxsum_path(&workspace, "oc_x"),
            "[历史上下文·压缩版]\n已压缩",
        )
        .unwrap();
        let b = bridge.clone();
        tokio::spawn(async move { b.handle(test_ev("m1", "oc_x", "继续")).await })
            .await
            .unwrap();
        // 只跑一次（不压缩不重试）；错误原样返回
        assert_eq!(runner.prompts().len(), 1, "已压缩过不重试");
        assert!(msgr.sent()[0].contains("context length exceeded"));
        assert!(!msgr.sent()[0].contains("已自动压缩"), "无压缩提示");
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
    /// 新后端能接续「刚才分析过什么」。助手条目记 reply 本体（存储保真，注入时
    /// 按预算切）。

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

    // ---- #200 Phase 2：buzz 后端 dispatch（mini-relay 短路）----

    #[tokio::test]
    async fn buzz_zero_subscriber_fails_fast() {
        // 第三轮审查 finding 3：buzz-acp 未连（未装/崩溃窗口/I5 终态）时，入库只是
        // 「等它重连背充」，用户侧是无限等待——必须当场可见失败，不静默悬挂。
        let bot = backend_bot("buzz");
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let (mut bridge, msgr) = build_test_bridge_with_bot(runner, bot.clone());
        let chat = "oc_ns";
        let db_path = crate::buzzrelay::test_db("abb-buzz-nosub");
        let store = crate::buzzrelay::EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = crate::buzzrelay::RelayState::new(
            store,
            nostr::prelude::Keys::generate(),
            nostr::prelude::Keys::generate().public_key().to_hex(),
        );
        // 频道已登记，但**没有任何 WS 连接**
        state.set_channels([crate::buzzrelay::Channel {
            uuid: crate::buzzrelay::channel_uuid(&bot.key(), chat),
            chat_id: chat.into(),
            name: "角色".into(),
            about: String::new(),
        }]);
        Arc::get_mut(&mut bridge)
            .expect("测试期唯一引用")
            .buzz_relay_state = Some(state.clone());
        let all: nostr::prelude::Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();

        bridge.handle(test_ev("m1", chat, "你好")).await;
        assert!(
            msgr.sent().iter().any(|t| t.contains("未订阅")),
            "无消费者要当场报错，不能让用户无限等待"
        );
        assert!(
            state.query(std::slice::from_ref(&all)).await.is_empty(),
            "预检未过不得入库（也不消耗会话闸）"
        );
        assert!(!bridge.sessions.is_started(chat));
        assert!(bridge.pending.is_empty());
        cleanup_bridge(&bridge);
        drop(bridge);
        drop(state);
        drop(_rx);
        crate::buzzrelay::remove_test_db(&db_path);
    }

    #[tokio::test]
    async fn buzz_backend_publishes_to_relay_without_cli_spawn() {
        // buzz 后端：消息写入 mini-relay（kind 9 事件含用户文本），不走 CLI spawn
        //（mock runner 若被调用会发出「不应被调用」，据此断言）；无同步回复。
        // 预置旧后端历史（#49 迁移场景）：首轮 prompt 带旧历史并落 marker；第二轮不再
        // 注入（resume 主闸 + marker 副闸——buzz 侧 channel→session 自带上下文）。
        let bot = backend_bot("buzz");
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let (mut bridge, msgr) = build_test_bridge_with_bot(runner, bot.clone());
        let chat = "oc_buzz";
        {
            let hist = crate::history::History::open(&bot.key(), chat);
            hist.append_user("old1", "claude", "旧背景");
            hist.append_assistant("old1", "claude", "旧答复");
        }
        // mini-relay 状态：临时库 + 登记一个频道（uuid 按 (bot_key, chat_id) 派生）
        let db_path = crate::buzzrelay::test_db("abb-buzz-bridge");
        let store = crate::buzzrelay::EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = crate::buzzrelay::RelayState::new(
            store,
            nostr::prelude::Keys::generate(),
            nostr::prelude::Keys::generate().public_key().to_hex(),
        );
        state.set_channels([crate::buzzrelay::Channel {
            uuid: crate::buzzrelay::channel_uuid(&bot.key(), chat),
            chat_id: chat.into(),
            name: "角色".into(),
            about: String::new(),
        }]);
        Arc::get_mut(&mut bridge)
            .expect("测试期唯一引用")
            .buzz_relay_state = Some(state.clone());
        // dispatch 预检要求「有 WS 订阅者」——登记一个假连接（不为测试放宽预检）
        let _sub = state.test_attach_subscriber();

        bridge.handle(test_ev("m1", chat, "第一问")).await;

        // 第一轮：kind 9 入库（用户文本 + 每轮必注入的指令块 + 迁移注入的旧历史）；
        // 无 CLI 调用、无同步回复；pending 摘除、mark_started、marker 落盘。
        let all: nostr::prelude::Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        let evs = state.query(std::slice::from_ref(&all)).await;
        assert_eq!(evs.len(), 1, "消息应写入 mini-relay");
        assert!(evs[0].content.contains("第一问"));
        assert!(
            evs[0].content.contains("旧背景"),
            "首轮应带 #49 迁移注入的旧历史"
        );
        assert!(msgr.sent().is_empty(), "buzz 路径无同步回复");
        assert!(bridge.pending.is_empty(), "发布即处理完毕，pending 应摘除");
        assert!(
            bridge.sessions.is_started(chat),
            "buzz 轮完成应 mark_started"
        );
        let sid = bridge.sessions.ensure_with_started(chat).0;
        let marker = crate::history::History::open(&bot.key(), chat).marker();
        assert!(
            matches!(&marker, Some(m) if m.session_id == sid && !m.pending),
            "注入轮应落非 pending marker（去掉 marker 写入逻辑此断言必红）"
        );

        // 第二轮：不再注入历史（旧背景不得再进 prompt）。
        bridge.handle(test_ev("m2", chat, "第二问")).await;
        let evs = state.query(&[all]).await;
        assert_eq!(evs.len(), 2);
        assert!(evs[1].content.contains("第二问"));
        assert!(
            !evs[1].content.contains("旧背景"),
            "第二轮不应重复注入历史（buzz 会话上下文由 channel→session 持有）"
        );
        cleanup_bridge(&bridge);
        drop(bridge);
        drop(state);
        drop(_rx);
        crate::buzzrelay::remove_test_db(&db_path);
    }

    #[tokio::test]
    async fn buzz_undelivered_replies_error_and_keeps_gate() {
        // buzz dispatch 失败面：① relay None（未启用/初始化失败）；② relay Some 但
        // chat 无频道（未登记/登记晚于启动）。两种都必须用户可见「无法处理」报错、
        // 摘 pending、且不 mark_started——一次性迁移注入闸不能被没发生的轮次消耗。
        let bot = backend_bot("buzz");
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let (mut bridge, msgr) = build_test_bridge_with_bot(runner, bot.clone());

        // ① relay None
        bridge.handle(test_ev("m1", "oc_n", "你好")).await;
        assert!(
            msgr.sent().iter().any(|t| t.contains("无法处理")),
            "relay 不可用必须可见报错，不做静默黑洞"
        );
        assert!(bridge.pending.is_empty());
        assert!(
            !bridge.sessions.is_started("oc_n"),
            "未送达轮不得 mark_started（迁移注入闸保留）"
        );

        // ② relay Some 但该 chat 未登记为频道
        let db_path = crate::buzzrelay::test_db("abb-buzz-neg");
        let store = crate::buzzrelay::EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = crate::buzzrelay::RelayState::new(
            store,
            nostr::prelude::Keys::generate(),
            nostr::prelude::Keys::generate().public_key().to_hex(),
        );
        Arc::get_mut(&mut bridge)
            .expect("测试期唯一引用")
            .buzz_relay_state = Some(state);
        bridge.handle(test_ev("m2", "oc_x", "再试")).await;
        assert!(
            msgr.sent()
                .iter()
                .filter(|t| t.contains("无法处理"))
                .count()
                >= 2,
            "无频道会话同样要可见报错"
        );
        assert!(bridge.pending.is_empty());
        assert!(!bridge.sessions.is_started("oc_x"));
        cleanup_bridge(&bridge);
        drop(bridge);
        crate::buzzrelay::remove_test_db(&db_path);
    }

    /// 安全回归（审查 #205r2）：**受限会话（授权者 Granted）不得走 buzz dispatch**。
    /// CLI 路径对 Granted 的硬闸是 agent::run 内挂的 guard hook/沙箱；buzz 跑在
    /// acp 侧自己的工具权限下（permission-mode 默认 bypass），RESTRICT_PREAMBLE 只是
    /// 提示文本——guard→request_permission 映射未接线前放行 = 静默提权。
    #[tokio::test]
    async fn buzz_refuses_restricted_granted_session() {
        let bot = backend_bot("buzz");
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let (mut bridge, msgr) = build_test_bridge_with_bot(runner, bot.clone());
        let chat = "oc_g";
        let db_path = crate::buzzrelay::test_db("abb-buzz-grant");
        let store = crate::buzzrelay::EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = crate::buzzrelay::RelayState::new(
            store,
            nostr::prelude::Keys::generate(),
            nostr::prelude::Keys::generate().public_key().to_hex(),
        );
        // 频道**已登记**：唯一拒绝理由必须是「受限会话」，不能是资格不足
        state.set_channels([crate::buzzrelay::Channel {
            uuid: crate::buzzrelay::channel_uuid(&bot.key(), chat),
            chat_id: chat.into(),
            name: "角色".into(),
            about: String::new(),
        }]);
        let all: nostr::prelude::Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        Arc::get_mut(&mut bridge)
            .expect("测试期唯一引用")
            .buzz_relay_state = Some(state.clone());
        // 假订阅者：预检顺序是「资格 → 策略」，没有订阅者会先撞上「未连接」，
        // 那样本测试断言的就不是受限拒绝了（第三轮审查 finding 3 引入的顺序）。
        let _sub = state.test_attach_subscriber();

        let mut ev = test_ev("m1", chat, "帮我改代码");
        ev.role = crate::config::SenderRole::Granted;
        bridge.handle(ev).await;

        assert!(
            msgr.sent().iter().any(|t| t.contains("受限")),
            "Granted 会话必须被拒绝并说明原因"
        );
        assert!(
            state.query(std::slice::from_ref(&all)).await.is_empty(),
            "安全回归：受限会话的内容不得进入 buzz 频道（那里没有 guard 硬闸）"
        );
        assert!(bridge.pending.is_empty());
        assert!(!bridge.sessions.is_started(chat));
        // 同群 owner 角色对照：owner 可正常 dispatch（拒绝只针对受限会话）
        bridge.handle(test_ev("m2", chat, "owner 正常消息")).await;
        assert_eq!(state.query(&[all]).await.len(), 1);
        cleanup_bridge(&bridge);
        drop(bridge);
        drop(state);
        drop(_rx);
        crate::buzzrelay::remove_test_db(&db_path);
    }

    // ---- #206：buzz 轮次叫停（/cancel → owner 控制命令 "!cancel"）----

    /// 从事件 tags 提取 #h 首值（buzzrelay::h_tag_of 是私有的，测试侧就地复刻）。
    fn test_h_tag(e: &nostr::prelude::Event) -> Option<String> {
        e.tags.iter().find_map(|t| {
            let s = t.as_slice();
            s.first()
                .is_some_and(|k| k.as_str() == "h")
                .then(|| s.get(1).map(|v| v.as_str().to_string()))
                .flatten()
        })
    }

    /// buzz bot /cancel → 发布恰好一条 owner 控制命令事件（content 精确 "!cancel"，
    /// #h 命中本 chat 频道）+ 诚实回执（只说「已发送」，附无轮次 no-op 说明）；
    /// pending 不残留、不消耗会话闸（不 mark_started）。
    #[tokio::test]
    async fn buzz_cancel_publishes_control_command() {
        let bot = backend_bot("buzz");
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let (mut bridge, msgr) = build_test_bridge_with_bot(runner, bot.clone());
        let chat = "oc_cancel";
        let db_path = crate::buzzrelay::test_db("abb-buzz-cancel");
        let store = crate::buzzrelay::EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = crate::buzzrelay::RelayState::new(
            store,
            nostr::prelude::Keys::generate(),
            nostr::prelude::Keys::generate().public_key().to_hex(),
        );
        let uuid = crate::buzzrelay::channel_uuid(&bot.key(), chat);
        state.set_channels([crate::buzzrelay::Channel {
            uuid: uuid.clone(),
            chat_id: chat.into(),
            name: "角色".into(),
            about: String::new(),
        }]);
        Arc::get_mut(&mut bridge)
            .expect("测试期唯一引用")
            .buzz_relay_state = Some(state.clone());
        // 预检判据为真：必须有连接 REQ 订阅了本频道（与 dispatch 同判据）
        let _sub = state.test_attach_subscriber();
        let all: nostr::prelude::Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();

        bridge.handle(test_ev("m1", chat, "/cancel")).await;

        let evs = state.query(std::slice::from_ref(&all)).await;
        assert_eq!(evs.len(), 1, "/cancel 应发布恰好一条控制事件");
        // 协议钉扎（buzz-acp is_owner_control_command lib.rs:3552-3562 @ c3132c3）：
        // content.trim()=="!cancel" 精确比对——钉死精确相等，防日后被加上下文前缀
        //（加了 acp 就把它当普通消息喂给 agent，叫停静默失效）。
        assert_eq!(evs[0].content, "!cancel");
        assert_eq!(test_h_tag(&evs[0]).as_deref(), Some(uuid.as_str()));
        // 回执诚实：只说「已发送」+ 无轮次 no-op 说明，绝不谎称「已停止」
        //（acp 对无 in-flight 的 !cancel 仅 warn no-op，桥无轮次记账可确知）
        let sent = msgr.sent();
        assert_eq!(sent.len(), 1, "buzz /cancel 应回一条回执");
        assert!(
            sent[0].contains("已向 buzz 发送叫停指令"),
            "回执: {}",
            sent[0]
        );
        assert!(
            sent[0].contains("无轮次在跑时本指令无效果"),
            "回执: {}",
            sent[0]
        );
        assert!(!sent[0].contains("已停止"), "不得谎称已停止: {}", sent[0]);
        // 即时控制指令：不落 pending、不消耗会话闸（迁移注入闸保留）
        assert!(bridge.pending.is_empty());
        assert!(!bridge.sessions.is_started(chat));
        cleanup_bridge(&bridge);
        drop(bridge);
        drop(state);
        drop(_rx);
        crate::buzzrelay::remove_test_db(&db_path);
    }

    /// buzz /cancel 三失败面各自的可见文案（预检判据与 dispatch 共用）：
    /// ① relay None；② 频道未登记；③ 无订阅者。都不发布控制事件、不落 pending。
    #[tokio::test]
    async fn buzz_cancel_preflight_failures_are_visible() {
        let bot = backend_bot("buzz");
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let (mut bridge, msgr) = build_test_bridge_with_bot(runner, bot.clone());
        let all: nostr::prelude::Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();

        // ① relay None（未启用/初始化失败）
        bridge.handle(test_ev("m1", "oc_n", "/cancel")).await;
        assert!(
            msgr.sent().iter().any(|t| t.contains("mini-relay 未运行")),
            "relay 不在要可见报错: {:?}",
            msgr.sent()
        );
        assert!(bridge.pending.is_empty());

        // ② relay 在但频道未登记
        let db_path = crate::buzzrelay::test_db("abb-buzz-cancel-neg");
        let store = crate::buzzrelay::EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = crate::buzzrelay::RelayState::new(
            store,
            nostr::prelude::Keys::generate(),
            nostr::prelude::Keys::generate().public_key().to_hex(),
        );
        Arc::get_mut(&mut bridge)
            .expect("测试期唯一引用")
            .buzz_relay_state = Some(state.clone());
        bridge.handle(test_ev("m2", "oc_x", "/cancel")).await;
        assert!(
            msgr.sent()
                .iter()
                .any(|t| t.contains("不是已登记的虚拟 Bot 群")),
            "未登记频道要可见报错: {:?}",
            msgr.sent()
        );
        assert!(state.query(std::slice::from_ref(&all)).await.is_empty());

        // ③ 频道已登记但无订阅者（buzz-acp 未连/未就绪）
        let chat = "oc_nosub";
        state.set_channels([crate::buzzrelay::Channel {
            uuid: crate::buzzrelay::channel_uuid(&bot.key(), chat),
            chat_id: chat.into(),
            name: "角色".into(),
            about: String::new(),
        }]);
        bridge.handle(test_ev("m3", chat, "/cancel")).await;
        assert!(
            msgr.sent().iter().any(|t| t.contains("未订阅本频道")),
            "无订阅者要可见报错: {:?}",
            msgr.sent()
        );
        assert!(
            state.query(std::slice::from_ref(&all)).await.is_empty(),
            "预检未过不得发布控制事件"
        );
        assert!(bridge.pending.is_empty());
        cleanup_bridge(&bridge);
        drop(bridge);
        drop(state);
        drop(_rx);
        crate::buzzrelay::remove_test_db(&db_path);
    }

    /// 自然停止词「停」透传不回归：buzz 路径不注册 cancel flag，无在跑任务时
    /// 停止词按普通消息进频道（事件 content 是原文而非 "!cancel"）。
    /// 已知边界（#206）：透传的停止词在 acp 侧实际触发 steer 模式 cancel+merge
    /// 重跑一轮（BUZZ_ACP_MULTIPLE_EVENT_HANDLING=steer 已显式钉住），与 CLI
    /// 「无任务透传」语义不同——对齐需桥侧轮次记账，本项不含。
    #[tokio::test]
    async fn buzz_natural_stop_keyword_passes_through() {
        let bot = backend_bot("buzz");
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let (mut bridge, msgr) = build_test_bridge_with_bot(runner, bot.clone());
        let chat = "oc_kw";
        let db_path = crate::buzzrelay::test_db("abb-buzz-kw");
        let store = crate::buzzrelay::EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = crate::buzzrelay::RelayState::new(
            store,
            nostr::prelude::Keys::generate(),
            nostr::prelude::Keys::generate().public_key().to_hex(),
        );
        state.set_channels([crate::buzzrelay::Channel {
            uuid: crate::buzzrelay::channel_uuid(&bot.key(), chat),
            chat_id: chat.into(),
            name: "角色".into(),
            about: String::new(),
        }]);
        Arc::get_mut(&mut bridge)
            .expect("测试期唯一引用")
            .buzz_relay_state = Some(state.clone());
        let _sub = state.test_attach_subscriber();

        bridge.handle(test_ev("m1", chat, "停")).await;

        let all: nostr::prelude::Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        let evs = state.query(std::slice::from_ref(&all)).await;
        assert_eq!(evs.len(), 1, "停止词应按普通消息透传进频道");
        assert!(
            evs[0].content.contains('停'),
            "透传原文: {}",
            evs[0].content
        );
        assert_ne!(evs[0].content, "!cancel", "自然停止词不得被翻译成控制命令");
        assert!(msgr.sent().is_empty(), "透传轮无同步回复");
        cleanup_bridge(&bridge);
        drop(bridge);
        drop(state);
        drop(_rx);
        crate::buzzrelay::remove_test_db(&db_path);
    }

    /// 粒度语义钉扎：话题消息（thread_id 非空）的 /cancel 仍发布到**群频道**——
    /// buzz 叫停是频道级=群级（同群各话题共用 channel，#206 已知边界），与 CLI
    /// 按 chat:thread key 隔离不同。
    #[tokio::test]
    async fn buzz_cancel_from_thread_publishes_to_group_channel() {
        let bot = backend_bot("buzz");
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let (mut bridge, msgr) = build_test_bridge_with_bot(runner, bot.clone());
        let chat = "oc_thr";
        let db_path = crate::buzzrelay::test_db("abb-buzz-thr");
        let store = crate::buzzrelay::EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = crate::buzzrelay::RelayState::new(
            store,
            nostr::prelude::Keys::generate(),
            nostr::prelude::Keys::generate().public_key().to_hex(),
        );
        let uuid = crate::buzzrelay::channel_uuid(&bot.key(), chat);
        state.set_channels([crate::buzzrelay::Channel {
            uuid: uuid.clone(),
            chat_id: chat.into(),
            name: "角色".into(),
            about: String::new(),
        }]);
        Arc::get_mut(&mut bridge)
            .expect("测试期唯一引用")
            .buzz_relay_state = Some(state.clone());
        let _sub = state.test_attach_subscriber();

        let mut ev = test_ev("m1", chat, "/cancel");
        ev.thread_id = "omt_topic1".into();
        bridge.handle(ev).await;

        let all: nostr::prelude::Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        let evs = state.query(std::slice::from_ref(&all)).await;
        assert_eq!(evs.len(), 1, "话题 /cancel 同样发布控制事件");
        assert_eq!(evs[0].content, "!cancel");
        assert_eq!(
            test_h_tag(&evs[0]).as_deref(),
            Some(uuid.as_str()),
            "话题 /cancel 路由到群频道（频道级叫停）"
        );
        assert!(
            msgr.sent().iter().any(|t| t.contains("叫停指令")),
            "话题内也要收到回执: {:?}",
            msgr.sent()
        );
        cleanup_bridge(&bridge);
        drop(bridge);
        drop(state);
        drop(_rx);
        crate::buzzrelay::remove_test_db(&db_path);
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
            sender_id: String::new(),
            ts: 0,
            created_at: 10,
            reply: None,
            resume_attempts: 0, // #164 新消息从 0 计
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
            sender_id: String::new(),
            ts: 0,
            created_at: 20,
            resume_attempts: 0, // #164 新消息从 0 计
            reply: None,
        });

        let stop = tokio_util::sync::CancellationToken::new();
        bridge.recover_pending(&stop).await;

        let prompts = runner.prompts();
        assert_eq!(prompts.len(), 2, "应按入队顺序重放");
        assert!(
            prompts[0].ends_with("第一条"),
            "prompt 尾部应为用户文本: {}",
            prompts[0]
        );
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
            sender_id: String::new(),
            ts: 0,
            resume_attempts: 0, // #164 新消息从 0 计
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
        assert!(
            msgr.done_calls().iter().any(|m| m == "w2"),
            "补发成功后补 DONE 回执（崩溃窗口里 handle 尾部的 done 未执行）"
        );
        assert!(bridge.pending.is_empty(), "补发成功清盘");
        cleanup_bridge(&bridge);
    }

    /// #74 审查跟进：W2 补发成功后落 assistant 历史（granted 私聊）——实时 handle 的
    /// 发送成功分支没跑到，不补的话这条回复在历史页永久缺失（消息+回复不完整）。
    #[tokio::test]
    async fn recover_redelivery_records_assistant_history_for_granted_p2p() {
        let runner = Arc::new(MockAgentRunner::immediate("不应被重跑"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss".into(),
            granted_ids: "ou_friend".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        // 模拟崩溃残留：granted 私聊的用户轮已落库（原跑过 handle），回复已产出
        // （set_reply 落盘）但发送确认前进程没了
        let mid = "w2h";
        assert!(
            bridge.msgstore.insert(
                &bridge.bot.key(),
                "oc_p2p",
                mid,
                "user",
                "ou_friend",
                "",
                "帮我查一下",
                1000
            ),
            "前置：用户轮应已落库"
        );
        bridge.pending.add(crate::pending::PendingItem {
            mid: mid.into(),
            chat_id: "oc_p2p".into(),
            chat_type: "p2p".into(),
            thread_id: String::new(),
            text: "帮我查一下".into(),
            quoted: crate::messenger::QuotedContent::default(),
            attachments: Vec::new(),
            role: crate::config::SenderRole::Granted,
            sender_id: "ou_friend".into(),
            resume_attempts: 0, // #164 新消息从 0 计
            ts: 1000,
            created_at: 10,
            reply: None,
        });
        bridge.pending.set_reply(mid, "补发的回复");

        let stop = tokio_util::sync::CancellationToken::new();
        bridge.recover_pending(&stop).await;

        assert!(
            runner.prompts().is_empty(),
            "有 reply 的条目不得重跑 agent（prompts 应为空）"
        );
        assert!(
            msgr.sent().iter().any(|t| t == "补发的回复"),
            "直接补发已产出回复"
        );
        let rows = bridge.msgstore.list_recent(10);
        assert_eq!(rows.len(), 2, "用户轮 + 补发的回复都要在历史库");
        assert_eq!(rows[0].direction, "assistant", "最新是回复");
        assert_eq!(rows[0].mid, mid, "回复复用用户轮 mid");
        assert_eq!(rows[0].text, "补发的回复");
        assert_eq!(rows[0].sender_id, "ou_friend");
        // 幂等：同 mid 同 direction 再插被忽略（重复补发/重启重放安全）
        assert!(
            !bridge.msgstore.insert(
                &bridge.bot.key(),
                "oc_p2p",
                mid,
                "assistant",
                "ou_friend",
                "",
                "补发的回复",
                2000
            ),
            "UNIQUE(mid,direction) 应挡住重复 assistant 行"
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
            resume_attempts: 0, // #164 新消息从 0 计
            sender_id: String::new(),
            ts: 0,
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
            resume_attempts: 0, // #164 新消息从 0 计
            role: crate::config::SenderRole::Owner,
            sender_id: String::new(),
            ts: 0,
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
    async fn group_access_exempt_but_private_still_gated() {
        // 8-20 用户决策：群聊不要求授权（未授权者 @ bot 被服务，@ 门槛是唯一防洪闸）；
        // 私聊授权仍生效（未授权私聊只提醒不触发 agent）
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
        // #118：无群聊豁免——未授权 sender + 免 @ 群 → 仍被拦截（不触发 agent）
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
            "#118 无群聊豁免：未授权群聊消息也被拦截（静默）"
        );
        // 未授权私聊 → 同样被拒（不触发 agent，只提醒/落历史）
        bridge
            .on_payload(
                feishu_payload(
                    "m2",
                    "oc_p2p",
                    "p2p",
                    "",
                    "",
                    "user",
                    "ou_stranger",
                    &[],
                    "私聊陌生人消息",
                )
                .as_bytes(),
            )
            .await;
        assert_eq!(runner.prompts().len(), 0, "未授权私聊不触发 agent（拦截）");
        cleanup_bridge(&bridge);
    }

    /// 开关是管理动作（用户拍板）。#118 公开开关失效：陌生人即使 @ 也被接入层拦截，
    /// 更不可能切换 /mention——未授权静默拦截，无提示文案。
    #[tokio::test]
    async fn open_access_stranger_is_intercepted() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_owner".into(),
            open_access: true, // #118：字段保留但不再被读（无公开开关）
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        // 陌生人 @ 机器人发 /mention off → 接入层静默拦截：无任何回复、开关不落
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
            msgr.sent().is_empty(),
            "未授权静默拦截：无任何提示文案: {:?}",
            msgr.sent()
        );
        assert!(runner.prompts().is_empty(), "未授权不触发 agent");
        assert!(bridge.mention_mode("oc_g").is_none(), "开关未被陌生人改动");
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
        assert!(
            runner.prompts()[0].ends_with("你好"),
            "白名单成员应触发 agent: {}",
            runner.prompts()[0]
        );
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
        assert!(
            prompts[0].ends_with("随便聊聊"),
            "owner 会话 prompt 不应带受限说明: {}",
            prompts[0]
        );
        assert_eq!(
            runner.roles(),
            [crate::config::SenderRole::Owner],
            "owner 消息以 Owner 角色进入 agent 调用"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn prompt_injects_agents_md_block_in_order() {
        // 三级 AGENTS.md：bot 级 + session 级文件 → 每轮全量注入，abb 先于 bot 先于
        // session（abb 级在测试 temp 根，一并覆盖）；prompt 以 [指令文件] 开头。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        let bot_key = bridge.bot.key();
        let root = bridge.agents_md_root.clone();
        let write = |p: &std::path::Path, c: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        };
        write(&root.join("AGENTS.md"), "# abb 级\n- 全局规则\n");
        write(
            &root.join("workspaces").join(&bot_key).join("AGENTS.md"),
            "# bot 级\n- bot 规则\n",
        );
        write(
            &root
                .join("workspaces")
                .join(&bot_key)
                .join("sessions")
                .join("oc_p2p.AGENTS.md"),
            "# session 级\n- 会话规则\n",
        );

        let payload = feishu_payload(
            "om_a",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_boss",
            &[],
            "执行任务",
        );
        bridge.on_payload(payload.as_bytes()).await;

        let prompts = runner.prompts();
        assert_eq!(prompts.len(), 1);
        let p = &prompts[0];
        assert!(p.starts_with("[指令文件]"), "全量注入块头: {p}");
        let i_abb = p.find("── abb 级").unwrap();
        let i_bot = p.find("── bot 级").unwrap();
        let i_ses = p.find("── session 级").unwrap();
        assert!(i_abb < i_bot && i_bot < i_ses, "顺序 abb → bot → session");
        assert!(p.contains("# abb 级") && p.contains("# bot 级") && p.contains("# session 级"));
        assert!(p.contains("执行任务"), "用户文本在块后");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn agents_md_block_sits_inside_restricted_preamble() {
        // 受限说明必须最外层：granted 会话中 [受限模式] 在 [指令文件] 之前
        //（AGENTS.md 里的任何话术不得盖过安全约束）
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
        let bot_key = bridge.bot.key();
        let root = bridge.agents_md_root.clone();
        std::fs::create_dir_all(root.join("workspaces").join(&bot_key)).unwrap();
        std::fs::write(
            root.join("workspaces").join(&bot_key).join("AGENTS.md"),
            "bot 规则",
        )
        .unwrap();

        let payload = feishu_payload(
            "om_b",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_friend",
            &[],
            "帮忙",
        );
        bridge.on_payload(payload.as_bytes()).await;

        let prompts = runner.prompts();
        let p = &prompts[0];
        assert!(p.starts_with("[受限模式]"), "受限说明必须最外层: {p}");
        let i_restrict = p.find("[受限模式]").unwrap();
        let i_agents = p.find("[指令文件]").unwrap();
        let i_text = p.find("帮忙").unwrap();
        assert!(
            i_restrict < i_agents && i_agents < i_text,
            "顺序：受限说明 > [指令文件] > 用户文本"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn missing_agents_md_skips_block() {
        // 三级文件全缺 → 整个块不注入（现有断言 prompt 精确相等的测试不破坏）
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let payload = feishu_payload(
            "om_c",
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
        assert!(
            prompts[0].contains("宿主护栏"),
            "无文件也应注入宿主护栏（#164 无条件）: {}",
            &prompts[0][..prompts[0].len().min(60)]
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn new_session_injects_summary_block() {
        // 历史为空（/new 或会话归纳清理后）+ 有归档摘要 → 兜底注入 [会话摘要] 块；
        // 提示文案显示「已携带会话摘要」而非「0 轮上下文」。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        let bot_key = bridge.bot.key();
        let ws = bridge.agents_md_root.join("workspaces").join(&bot_key);
        std::fs::create_dir_all(ws.join("summaries")).unwrap();
        std::fs::write(
            ws.join("summaries").join("oc_p2p.md"),
            "> ABB 会话归纳自动生成（2026-01-01）\n主题：写周报\n关键结论：无\n",
        )
        .unwrap();

        let payload = feishu_payload(
            "om_s",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_boss",
            &[],
            "你好",
        );
        bridge.on_payload(payload.as_bytes()).await;

        let prompts = runner.prompts();
        assert_eq!(prompts.len(), 1);
        let p = &prompts[0];
        assert!(p.contains("[会话摘要]"), "摘要块兜底注入: {p}");
        assert!(p.contains("主题：写周报"), "摘要内容在 prompt 中");
        assert!(
            p.find("[会话摘要]").unwrap() < p.find("你好").unwrap(),
            "摘要块在用户文本前"
        );
        assert!(
            p.find("[指令文件]").unwrap() < p.find("[会话摘要]").unwrap(),
            "指令文件块在摘要块前"
        );
        // 回复带「已携带会话摘要」提示（而非 0 轮上下文）
        let sent = msgr.sent();
        assert!(
            sent.iter().any(|t| t.contains("已携带会话摘要")),
            "提示文案: {sent:?}"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn granted_p2p_message_records_msgstore_and_unread() {
        // #74：授权者私聊消息 → 落消息库（user 轮）+ 未读提醒；agent 回复发送成功后
        // 再落 assistant 轮（同 mid，direction 区分，UNIQUE(mid,direction) 幂等）。
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
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        let payload = feishu_payload(
            "om_rec",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_friend",
            &[],
            "帮我写个周报",
        );
        bridge.on_payload(payload.as_bytes()).await;

        let rows = bridge.msgstore.list_recent(10);
        assert_eq!(rows.len(), 2, "granted 私聊应落 user + assistant 两条");
        assert_eq!(rows[0].direction, "assistant", "最新是回复");
        assert_eq!(rows[0].mid, "om_rec", "回复复用用户轮 mid");
        assert_eq!(rows[0].sender_id, "ou_friend");
        assert_eq!(rows[1].direction, "user");
        assert_eq!(rows[1].text, "帮我写个周报");
        assert_eq!(rows[1].sender_id, "ou_friend");

        let unread = bridge.unread.snapshot().unwrap();
        assert_eq!(unread.len(), 1, "未读提醒一条");
        assert_eq!(unread[0].bot_key, bridge.bot.key());
        assert_eq!(unread[0].sender, "ou_friend");
        assert_eq!(unread[0].preview, "帮我写个周报");
        // 提醒是纯本地 UI：messenger 只发出 agent 回复，绝不主动向任何 IM 发提醒消息
        assert_eq!(msgr.sent().len(), 1, "外发只有 agent 回复一条");
        assert_eq!(msgr.sent()[0], "done");
        // 审查跟进：重放同 mid（崩溃恢复续跑 handle；seen 去重是进程内的，重启后
        // 清空）→ 落库被 UNIQUE(mid,direction) 幂等挡住、未读不得重复提醒
        // （弹窗/红点以「这条消息提醒过没」为准，不以收到几次为准）。
        bridge.seen.lock().unwrap().clear(); // 模拟重启后的全新进程
        bridge.on_payload(payload.as_bytes()).await;
        assert_eq!(
            bridge.msgstore.list_recent(10).len(),
            2,
            "重放不重复落库（user+assistant 仍各一行）"
        );
        assert_eq!(
            bridge.unread.snapshot().unwrap().len(),
            1,
            "重放同 mid 只提醒一次"
        );
        assert_eq!(
            msgr.sent().iter().filter(|t| *t == "done").count(),
            2,
            "W1 重放语义：agent 重跑并再发一条回复（at-least-once）"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn owner_p2p_message_skips_msgstore_and_unread() {
        // #74：owner 自己排除——owner 私聊不落历史库、不产生未读提醒
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
            "om_own",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_boss",
            &[],
            "在吗",
        );
        bridge.on_payload(payload.as_bytes()).await;
        assert!(
            bridge.msgstore.list_recent(10).is_empty(),
            "owner 消息不得落库"
        );
        assert!(
            bridge.unread.snapshot().unwrap_or_default().is_empty(),
            "owner 消息不得产生未读"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn unauthorized_p2p_message_records_unread_and_history() {
        // #74 扩展（8-20 用户实测反馈）：未授权用户私聊也提醒 + 落历史（owner 能看到
        // 谁在找 bot）；授权码消费成功的不提醒（那是激活流程）
        let runner = Arc::new(MockAgentRunner::immediate("不应被调用"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "feishu".into(),
            bot_name: "庆小丰".into(),
            bot_open_id: "ou_bot".into(),
            owner_open_id: "ou_boss".into(),
            granted_ids: "ou_friend".into(),
            ..Default::default()
        };
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        // 陌生人（非 owner 非授权者）私聊：应落 user 历史 + 未读提醒；agent 不被触发
        let payload = feishu_payload(
            "om_stranger",
            "oc_p2p",
            "p2p",
            "",
            "",
            "user",
            "ou_stranger",
            &[],
            "你好，我想用一下这个机器人",
        );
        bridge.on_payload(payload.as_bytes()).await;
        let rows = bridge.msgstore.list_recent(10);
        assert_eq!(rows.len(), 1, "未授权私聊应落 1 条 user 历史");
        assert_eq!(rows[0].direction, "user");
        assert_eq!(rows[0].sender_id, "ou_stranger");
        assert_eq!(rows[0].text, "你好，我想用一下这个机器人");
        let unread = bridge.unread.snapshot().unwrap();
        assert_eq!(unread.len(), 1, "未授权私聊应产生未读提醒");
        assert_eq!(unread[0].sender, "ou_stranger");
        assert!(
            msgr.sent().is_empty(),
            "未授权消息不触发 agent、不回复（提醒是纯本地）"
        );
        // 未授权群聊：不提醒（提醒范围=私聊）
        let payload2 = feishu_payload(
            "om_stranger_g",
            "oc_group",
            "group",
            "",
            "",
            "user",
            "ou_stranger",
            &[],
            "@_user_1 你好",
        );
        bridge.on_payload(payload2.as_bytes()).await;
        assert_eq!(
            bridge.unread.snapshot().unwrap().len(),
            1,
            "未授权群聊不产生新提醒"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn granted_group_message_skips_msgstore_and_unread() {
        // #74：提醒/历史只覆盖私聊（p2p/dm）——群里授权者 @ 消息不落历史库、不提醒
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
            "om_g_grp",
            "oc_grp",
            "group",
            "",
            "",
            "user",
            "ou_friend",
            &[("庆小丰", "ou_bot")],
            "大家早",
        );
        bridge.on_payload(payload.as_bytes()).await;
        assert!(bridge.msgstore.list_recent(10).is_empty(), "群消息不得落库");
        assert!(
            bridge.unread.snapshot().unwrap_or_default().is_empty(),
            "群消息不得产生未读"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn dingtalk_granted_dm_records_msgstore_and_unread() {
        // #74：钉钉授权者单聊（chat_type=dm）同样落库 + 提醒（dm 分支覆盖）
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "dingtalk".into(),
            ding_owner_ids: "u_boss".into(),
            ding_granted_ids: "u_friend".into(),
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);
        let msg = crate::dingtalk::DingtalkMessage {
            mid: "dt1".into(),
            sender_staff_id: "u_friend".into(),
            conversation_id: "u_friend".into(), // 单聊：chat_id = sender
            conversation_type: "1".into(),
            conversation_title: String::new(),
            text: "钉钉上找你".into(),
            mentioned: false,
            robot_code: "r".into(),
            quoted_text: String::new(),
            quoted_attachments: Vec::new(),
            attachments: Vec::new(),
        };
        bridge.on_dingtalk(msg).await;

        let rows = bridge.msgstore.list_recent(10);
        assert_eq!(rows.len(), 2, "granted 钉钉单聊应落 user + assistant 两条");
        assert_eq!(rows[0].direction, "assistant");
        assert_eq!(rows[1].direction, "user");
        assert_eq!(rows[1].text, "钉钉上找你");
        assert_eq!(rows[1].sender_id, "u_friend");
        let unread = bridge.unread.snapshot().unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].sender, "u_friend");
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

        assert!(
            runner.prompts()[0].ends_with("回复一下"),
            "话题内回复应进 agent: {}",
            runner.prompts()[0]
        );
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

        assert!(
            runner.prompts()[0].ends_with("在吗"),
            "owner 私聊消息应进 agent: {}",
            runner.prompts()[0]
        );
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

        assert!(
            runner.prompts()[0].ends_with("[引用消息]\n上面那条被引用的消息\n\n回复内容"),
            "prompt 应带引用上下文（引用块在尾部）: {}",
            runner.prompts()[0]
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
        assert_eq!(runner.prompts().len(), 1);
        assert!(
            runner.prompts()[0].ends_with("[引用消息]\n被引用的原消息\n\n回复内容"),
            "引用块应在 prompt 尾部: {}",
            runner.prompts()[0]
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
            wx_user_id: "wx_owner".into(), // #118 fail-closed：wx_user_id 未配置则拒绝所有人
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let msg = crate::wechat::WeixinMessage {
            from_user_id: "wx_owner".into(),
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

        assert!(
            runner.prompts()[0].ends_with("[引用消息]\n摘要 | 被引用的原消息\n\n回复内容"),
            "引用块应在 prompt 尾部: {}",
            runner.prompts()[0]
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
            conversation_title: String::new(),
            text: "回复内容".into(),
            mentioned: false,
            robot_code: "r".into(),
            quoted_text: "被引用的原消息".into(),
            quoted_attachments: Vec::new(),
            attachments: Vec::new(),
        };
        bridge.on_dingtalk(msg).await;

        assert_eq!(runner.prompts().len(), 1);
        assert!(
            runner.prompts()[0].ends_with("[引用消息]\n被引用的原消息\n\n回复内容"),
            "引用块应在 prompt 尾部: {}",
            runner.prompts()[0]
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

        // 文本+附件同时存在：整段精确断言，钉死「[引用附件] 独占一行」的格式
        assert!(runner.prompts()[0].ends_with(
            "[引用消息]\n引用的文字\n[引用附件]\n[image] 来源=mock 文件名=截图.png mime=application/octet-stream 大小=1 本地路径=/tmp/mock-attachment.bin sha256=abc\n\n回复内容"
        ), "引用+附件块应在 prompt 尾部: {}", runner.prompts()[0]);
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn on_weixin_quoted_media_in_prompt() {
        // 微信引用图片：ref_msg.message_item 媒体 → 下载 → [引用附件] 进 prompt。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            kind: "wechat".into(),
            wx_user_id: "wx_owner".into(), // #118 fail-closed：wx_user_id 未配置则拒绝所有人
            ..Default::default()
        };
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot);

        let msg = crate::wechat::WeixinMessage {
            from_user_id: "wx_owner".into(),
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
            conversation_title: String::new(),
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

    #[test]
    fn register_unregister_cancel_flag_roundtrip() {
        let (bridge, _msgr) = build_test_bridge(Arc::new(MockAgentRunner::immediate("x")));
        // 注册 → 可被「停止词」发现并置 true
        let flag = bridge.register_cancel_flag("oc_jobchat");
        assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
        // 模拟停止词到达（virtualbot 里对该 key 的 flag 置 true）
        let got = bridge
            .cancel_flags
            .lock()
            .unwrap()
            .get("oc_jobchat")
            .cloned();
        let got = got.expect("注册后应可查到");
        got.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            flag.load(std::sync::atomic::Ordering::Relaxed),
            "同一 Arc，置 true 应互相可见"
        );
        // 摘除 → 停止词找不到（后续按普通消息处理）
        bridge.unregister_cancel_flag("oc_jobchat");
        assert!(
            bridge
                .cancel_flags
                .lock()
                .unwrap()
                .get("oc_jobchat")
                .is_none(),
            "摘除后不应再可查"
        );
        // 未注册的 key 摘除是 no-op（不 panic）
        bridge.unregister_cancel_flag("oc_none");
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
                resume_attempts: 0, // #164 新消息从 0 计
                attachments: Vec::new(),
                role: crate::config::SenderRole::Owner,
                sender_id: String::new(),
                ts: 0,
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

    // ─── 虚拟 Bot 角色注入（#75）───

    /// 给 bridge 的内存登记快照塞一条登记（不走真实登记表文件）。
    fn register_virtual_bot(bridge: &Bridge, chat_id: &str, role: &str) {
        bridge
            .virtual_bots
            .lock()
            .unwrap()
            .push(crate::virtualbot::VirtualBot {
                bot_key: bridge.bot.key(),
                chat_id: chat_id.into(),
                role_name: role.into(),
                created_at: 1,
            });
    }

    #[tokio::test]
    async fn virtual_bot_role_injected_for_registered_group() {
        // 登记过的群聊：prompt 前置 [群角色] 块。群名优先事件名（比 API 新），
        // 群介绍来自 messenger 的 get_chat_info（best-effort）。
        let runner = Arc::new(MockAgentRunner::immediate("好的"));
        let bot = backend_bot("claude");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        register_virtual_bot(&bridge, "oc_vb_1", "后端开发");
        bridge
            .chat_info_cache
            .note_event_name("oc_vb_1", "后端开发");
        msgr.set_chat_info("后端开发", "你是后端开发工程师。");

        bridge
            .handle(test_ev("m1", "oc_vb_1", "帮我评审这个 API 设计"))
            .await;
        let prompts = runner.prompts();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains("[群角色]\n群名：后端开发\n群介绍：你是后端开发工程师。\n\n"),
            "群角色块应注入（在指令文件块之后）: {:?}",
            &prompts[0][..prompts[0].len().min(120)]
        );
        assert!(
            prompts[0].find("[群角色]").unwrap() > prompts[0].find("[指令文件]").unwrap(),
            "指令文件块应在群角色块前"
        );
        assert!(prompts[0].ends_with("帮我评审这个 API 设计"));
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn virtual_bot_uses_event_name_and_skips_when_no_info() {
        // 事件名比 API 名新（平台改名后）：注入用事件名；API 查不到群介绍 → 只注入名
        let runner = Arc::new(MockAgentRunner::immediate("好的"));
        let bot = backend_bot("claude");
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        register_virtual_bot(&bridge, "oc_vb_1", "旧角色名");
        bridge
            .chat_info_cache
            .note_event_name("oc_vb_1", "新角色名");

        bridge.handle(test_ev("m1", "oc_vb_1", "hi")).await;
        let prompts = runner.prompts();
        assert!(
            prompts[0].contains("[群角色]\n群名：新角色名\n群介绍：\n\n"),
            "事件名优先: {:?}",
            &prompts[0][..prompts[0].len().min(120)]
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn virtual_bot_no_injection_for_unregistered_or_dm() {
        let runner = Arc::new(MockAgentRunner::immediate("好的"));
        let bot = backend_bot("claude");
        let (bridge, msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        msgr.set_chat_info("后端开发", "你是后端开发工程师。");

        // 未登记群 → 不注入
        bridge.handle(test_ev("m1", "oc_other", "你好")).await;
        assert!(
            runner.prompts()[0].ends_with("你好"),
            "prompt 尾部应为用户文本: {}",
            runner.prompts()[0]
        );
        // 登记了但消息是私聊（chat_type != group）→ 不注入
        register_virtual_bot(&bridge, "oc_vb_2", "产品经理");
        bridge
            .chat_info_cache
            .note_event_name("oc_vb_2", "产品经理");
        let mut ev = test_ev("m2", "oc_vb_2", "私聊你好");
        ev.chat_type = "dm".into();
        bridge.handle(ev).await;
        assert!(
            runner.prompts()[1].ends_with("私聊你好"),
            "prompt 尾部应为用户文本: {}",
            runner.prompts()[1]
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn chat_deleted_event_removes_registration() {
        // #75 事件驱动：im.chat.deleted_v1（平台解散群）→ 自动移除该群登记
        let runner = Arc::new(MockAgentRunner::immediate("好的"));
        let bot = backend_bot("claude");
        let (bridge, _msgr) = build_test_bridge_with_bot(runner.clone(), bot.clone());
        // 登记一条（写临时 vb_store）
        bridge
            .vb_store
            .add(crate::virtualbot::VirtualBot {
                bot_key: bridge.bot.key(),
                chat_id: "oc_vb_del".into(),
                role_name: "后端开发".into(),
                created_at: 1,
            })
            .unwrap();
        // 模拟飞书推送群被解散事件
        let payload = br#"{"schema":"2.0","header":{"event_type":"im.chat.deleted_v1"},"event":{"chat_id":"oc_vb_del"}}"#;
        bridge.on_payload(payload).await;
        assert!(
            bridge.vb_store.load().is_empty(),
            "群被解散后登记应自动移除"
        );
        // 未登记的群被解散：不报错、不动其它登记
        bridge
            .vb_store
            .add(crate::virtualbot::VirtualBot {
                bot_key: bridge.bot.key(),
                chat_id: "oc_vb_keep".into(),
                role_name: "产品经理".into(),
                created_at: 2,
            })
            .unwrap();
        let payload2 = br#"{"schema":"2.0","header":{"event_type":"im.chat.deleted_v1"},"event":{"chat_id":"oc_other"}}"#;
        bridge.on_payload(payload2).await;
        assert_eq!(bridge.vb_store.load().len(), 1, "未登记群不影响登记表");
        cleanup_bridge(&bridge);
    }

    // ─── #87 会话暂停/恢复（消息仍入库、不触发 agent、恢复即生效）──────────

    #[tokio::test]
    async fn paused_chat_stores_message_but_does_not_run_agent() {
        let runner = Arc::new(MockAgentRunner::immediate("不该出现的回复"));
        let (bridge, msgr) = build_test_bridge(runner.clone());
        // 暂停该会话（写入注入的临时 session_state）
        bridge
            .session_state
            .pause(&bridge.bot.key(), "oc_x", "test");
        bridge.handle(test_ev("m1", "oc_x", "暂停期间的消息")).await;
        // 不触发 agent、不回复
        assert!(
            msgr.sent().is_empty(),
            "暂停会话不应回复: {:?}",
            msgr.sent()
        );
        assert_eq!(runner.prompts().len(), 0, "暂停会话不应触发 agent");
        // 消息仍入库（可查）
        let rows = bridge.msgstore.list_recent(10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "暂停期间的消息");
        assert_eq!(rows[0].direction, "user");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn resumed_chat_runs_agent_again() {
        let runner = Arc::new(MockAgentRunner::immediate("恢复后的回复"));
        let (bridge, msgr) = build_test_bridge(runner.clone());
        bridge
            .session_state
            .pause(&bridge.bot.key(), "oc_x", "test");
        bridge.session_state.resume(&bridge.bot.key(), "oc_x");
        bridge.handle(test_ev("m1", "oc_x", "恢复后的消息")).await;
        assert_eq!(runner.prompts().len(), 1, "恢复后应正常触发 agent");
        assert!(!msgr.sent().is_empty(), "恢复后应正常回复");
        cleanup_bridge(&bridge);
    }

    // ─── #124 一键创建团队·聊天入口（P1 后端）──────────────────────────────

    #[tokio::test]
    async fn team_flow_trigger_generates_preview_without_agent() {
        // 触发词 → 加载提示 + 方案预览；不进 agent、不落 pending（消息被消费）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, msgr) = build_test_bridge(runner.clone());
        Arc::get_mut(&mut bridge).unwrap().team_gen = Arc::new(MockTeamPlanGenerator::ok());
        bridge
            .handle(test_ev("m1", "oc_team1", "帮我建个团队做测试"))
            .await;
        let sent = msgr.sent();
        assert!(
            sent.iter().any(|t| t.contains("⏳ 正在为你组建团队")),
            "应先发加载提示: {:?}",
            sent
        );
        assert!(
            sent.iter()
                .any(|t| t.contains("📋 团队「测试团队」方案预览")),
            "应发方案预览: {:?}",
            sent
        );
        assert!(
            sent.iter().any(|t| t.contains("✅ 回复「确认」创建团队")),
            "预览应含操作提示"
        );
        assert!(runner.prompts().is_empty(), "团队流程不应进 agent");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn team_flow_confirm_creates_groups_and_registers() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, msgr) = build_test_bridge(runner.clone());
        Arc::get_mut(&mut bridge).unwrap().bot.owner_open_id = "ou_owner".into();
        Arc::get_mut(&mut bridge).unwrap().team_gen = Arc::new(MockTeamPlanGenerator::ok());
        bridge
            .handle(test_ev("m1", "oc_team2", "帮我建个团队做测试"))
            .await;
        bridge.handle(test_ev("m2", "oc_team2", "确认")).await;
        let sent = msgr.sent();
        assert!(
            sent.iter()
                .any(|t| t.contains("✅ 团队「测试团队」已创建（2 个角色群）")),
            "确认应建群并回清单: {:?}",
            sent
        );
        assert_eq!(
            msgr.created(),
            vec![
                "测试团队-开发-待任命".to_string(),
                "测试团队-测试-待任命".to_string()
            ]
        );
        assert_eq!(bridge.vb_store.load().len(), 2, "建群后应登记虚拟 Bot");
        // 流程结束：后续普通消息应进 agent（不再被团队流程消费）
        bridge.handle(test_ev("m3", "oc_team2", "你好")).await;
        assert_eq!(runner.prompts().len(), 1);
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn team_flow_modify_regenerates_with_revision_merged() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, msgr) = build_test_bridge(runner.clone());
        let gen = Arc::new(MockTeamPlanGenerator::ok());
        Arc::get_mut(&mut bridge).unwrap().team_gen = gen.clone();
        bridge
            .handle(test_ev("m1", "oc_team3", "帮我建个团队做测试"))
            .await;
        bridge
            .handle(test_ev("m2", "oc_team3", "改：把测试改成运营"))
            .await;
        let sent = msgr.sent();
        let previews = sent
            .iter()
            .filter(|t| t.contains("📋 团队「测试团队」方案预览"))
            .count();
        assert_eq!(previews, 2, "改：应重新生成并替换预览（新预览消息）");
        let goals = gen.goals();
        assert_eq!(goals.len(), 2);
        assert!(
            goals[1].contains("调整要求：把测试改成运营"),
            "调整要求应合并进生成目标: {:?}",
            goals
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn team_flow_cancel_aborts_and_clears() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, msgr) = build_test_bridge(runner.clone());
        Arc::get_mut(&mut bridge).unwrap().team_gen = Arc::new(MockTeamPlanGenerator::ok());
        bridge
            .handle(test_ev("m1", "oc_team4", "帮我建个团队做测试"))
            .await;
        bridge.handle(test_ev("m2", "oc_team4", "取消")).await;
        assert!(
            msgr.sent().iter().any(|t| t.contains("已取消团队创建")),
            "取消应回复中止确认"
        );
        bridge.handle(test_ev("m3", "oc_team4", "你好")).await;
        assert_eq!(runner.prompts().len(), 1, "取消后普通消息应进 agent");
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn team_flow_cancel_command_aborts() {
        // /cancel 显式命令：团队流程进行中也要能中止（不能出现「/cancel 却说没任务」）。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, msgr) = build_test_bridge(runner.clone());
        Arc::get_mut(&mut bridge).unwrap().team_gen = Arc::new(MockTeamPlanGenerator::ok());
        bridge
            .handle(test_ev("m1", "oc_teamC", "帮我建个团队做测试"))
            .await;
        bridge.handle(test_ev("m2", "oc_teamC", "/cancel")).await;
        assert!(
            msgr.sent().iter().any(|t| t.contains("已取消团队创建")),
            "/cancel 应中止团队流程: {:?}",
            msgr.sent()
        );
        bridge.handle(test_ev("m3", "oc_teamC", "你好")).await;
        assert_eq!(runner.prompts().len(), 1);
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn team_flow_trigger_without_goal_asks_then_generates() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, msgr) = build_test_bridge(runner.clone());
        Arc::get_mut(&mut bridge).unwrap().team_gen = Arc::new(MockTeamPlanGenerator::ok());
        bridge.handle(test_ev("m1", "oc_team5", "创建团队")).await;
        assert!(
            msgr.sent()
                .iter()
                .any(|t| t.contains("想建一个什么样的团队")),
            "缺目标应追问"
        );
        assert!(runner.prompts().is_empty());
        bridge
            .handle(test_ev("m2", "oc_team5", "帮我们建一个做运营的团队"))
            .await;
        assert!(
            msgr.sent()
                .iter()
                .any(|t| t.contains("📋 团队「测试团队」方案预览")),
            "补目标后应生成预览"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn team_flow_generation_failure_is_retryable() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, msgr) = build_test_bridge(runner.clone());
        Arc::get_mut(&mut bridge).unwrap().team_gen =
            Arc::new(MockTeamPlanGenerator::fail("模型超时"));
        bridge
            .handle(test_ev("m1", "oc_team6", "帮我建个团队做测试"))
            .await;
        assert!(
            msgr.sent().iter().any(|t| {
                t.contains("⚠️ 团队方案生成失败") && t.contains("模型超时") && t.contains("重试")
            }),
            "失败应明确提示可重试: {:?}",
            msgr.sent()
        );
        // 失败不残留流程：下一条普通消息进 agent
        bridge.handle(test_ev("m2", "oc_team6", "普通问题")).await;
        assert_eq!(runner.prompts().len(), 1);
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn team_flow_partial_failure_keeps_flow_and_retry_is_idempotent() {
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, msgr) = build_test_bridge(runner.clone());
        Arc::get_mut(&mut bridge).unwrap().bot.owner_open_id = "ou_owner".into();
        Arc::get_mut(&mut bridge).unwrap().team_gen = Arc::new(MockTeamPlanGenerator::ok());
        bridge
            .handle(test_ev("m1", "oc_team8", "帮我建个团队做测试"))
            .await;
        msgr.set_fail_create("测试团队-测试-待任命");
        bridge.handle(test_ev("m2", "oc_team8", "确认")).await;
        assert!(
            msgr.sent().iter().any(|t| t.contains("部分成功（1/2）")),
            "部分失败应分开展示成功/失败: {:?}",
            msgr.sent()
        );
        assert!(
            msgr.sent()
                .iter()
                .any(|t| t.contains("❌ 测试团队-测试-待任命")),
            "失败项应列出原因"
        );
        // 重试：已成功的「开发」跳过（不重复建），失败的「测试」补建
        msgr.clear_fail_create();
        bridge.handle(test_ev("m3", "oc_team8", "确认")).await;
        assert_eq!(
            msgr.created(),
            vec![
                "测试团队-开发-待任命".to_string(),
                "测试团队-测试-待任命".to_string()
            ],
            "重试不应重复建已成功的群"
        );
        cleanup_bridge(&bridge);
    }

    #[tokio::test]
    async fn team_flow_granted_role_passes_through_to_agent() {
        // 建群是管理动作：granted（授权者）不触发团队流程，原样进 agent。
        let runner = Arc::new(MockAgentRunner::immediate("done"));
        let (mut bridge, _msgr) = build_test_bridge(runner.clone());
        Arc::get_mut(&mut bridge).unwrap().team_gen = Arc::new(MockTeamPlanGenerator::ok());
        let mut ev = test_ev("m1", "oc_team9", "帮我建个团队做测试");
        ev.role = crate::config::SenderRole::Granted;
        bridge.handle(ev).await;
        assert_eq!(runner.prompts().len(), 1, "granted 应透传 agent");
        cleanup_bridge(&bridge);
    }
}
