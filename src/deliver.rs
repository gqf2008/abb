//! 跨会话投递（#21）—— agent 通过 `$ABB_BIN deliver` CLI 把消息投递到其它 bot 的会话。
//!
//! 进程模型：agent 是 bridge spawn 的子进程，拿不到 service 内存里的 Messenger；
//! 因此 CLI 把投递请求落盘到 `~/.agent-bridge/deliveries.json`，service 侧轮询消费。
//! 文件极小，每次操作直接读盘（天然跨进程可见，无 mtime 竞态）；写盘持
//! `deliveries.lock` 独占文件锁（fs2：Unix flock / Windows LockFileEx）+ 唯一 tmp rename，
//! CLI 入队与 service ack 并发安全不丢项。投递目标 = bot_key + chat_id。
//!
//! 总开关：`Config.cross_delivery_enabled`（默认关）。CLI 提交前拒绝 + service 消费侧双保险，
//! 防止开关在提交后被关掉仍把队列里的项发出去。

use crate::config::BotConfig;
use crate::messenger::Messenger;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 一条待投递消息。来源（source_bot/source_chat）用于投递失败时回源报错，不静默丢失。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryItem {
    /// 幂等键（uuid；同 id 不重复入队）。
    pub id: String,
    /// 目标 bot key（config bots[].key()）。
    pub target_bot: String,
    /// 目标会话 id（chat_id）。
    pub target_chat: String,
    /// 完整消息文本。
    pub text: String,
    /// 来源 bot key（agent 工作区所在 bot；人工 CLI 可为空）。
    #[serde(default)]
    pub source_bot: String,
    /// 来源会话 id（投递失败时回源报错用）。
    #[serde(default)]
    pub source_chat: String,
    #[serde(default)]
    pub created_at: u64,
    /// 附件元数据（#21 附件跨投递）：转发本地路径/元数据，接收端按能力处理。
    #[serde(default)]
    pub attachments: Vec<crate::attachments::AttachmentMeta>,
    /// 来源定时任务 id（#21）：非空 = 定时任务投递，跳过防循环去重（周期任务是合法重复），
    /// 也豁免 Router 自环拒绝（任务回发本 bot 原会话是既有行为）。
    #[serde(default)]
    pub job_id: String,
    /// 「发给当前会话」显式声明（CLI `--to-current`）：目标恒等于来源，语义是**发送**
    /// 而不是**投递**——给当前会话回附件是正当需求（桥的回复通道只发文本，agent
    /// 生成的附件必须显式发一次）。
    ///
    /// 与自环保护的关系：`is_self_loop` 这条硬规则**不变**，只有本字段为真**且**来源
    /// 确实等于目标时才豁免；手改队列塞 `in_session` 却把来源写成别的会话 → 仍按普通
    /// 投递走自环判定。结构上不可能自循环：没有跨会话对，且 bot 自己的出站消息在三个
    /// 平台都被桥丢弃（微信 `message_type!=1`、飞书 `sender_type=app/bot`、钉钉回调
    /// 只在被 @ 时触发），不会回灌成新的用户输入。
    ///
    /// **依赖差异（审查 P3-2）**：微信/飞书是**代码级**丢弃（本仓有过滤与测试）；
    /// 钉钉 `bridge::recover::on_dingtalk` **没有** sender 判别，靠的是「平台不会把 bot
    /// 自己发的消息推回」。那天若变了，需在 `on_dingtalk` 补一条与飞书同款的丢弃。
    #[serde(default)]
    pub in_session: bool,
}

/// 投递请求落盘队列（CLI 写、service 消费）。
///
/// 并发模型：不缓存、不加锁——每次操作直接读盘（文件极小）；写盘用 CAS：
/// 先比对磁盘当前内容与自己读到的一致，再用**唯一 tmp 文件名** rename 替换。
/// 内容不一致说明有并发写（另一进程），重读重试。这样 CLI add 与 service ack
/// 并发时既不覆盖新项也不丢已投递项（at-least-once：崩溃未 ack → 下次重投，
/// 由 Router 防循环去重兜底）。
pub struct DeliveryStore {
    /// deliveries.json（队列本体）。
    path: PathBuf,
    /// deliveries.lock（跨进程独占锁，永不复名——rename 队列文件不影响锁的 inode）。
    lock_path: PathBuf,
}

impl DeliveryStore {
    pub fn new() -> DeliveryStore {
        DeliveryStore::new_at(crate::bridge_dir().join("deliveries.json"))
    }

    fn new_at(path: PathBuf) -> DeliveryStore {
        let lock_path = path.with_extension("lock");
        DeliveryStore { path, lock_path }
    }

    /// 独占锁内执行闭包（CLI 入队 / service ack 读-改-写原子化）。
    /// 锁获取失败（本地盘极罕见）时降级为不加锁执行——宁可偶发竞态也不静默丢请求。
    fn with_exclusive<T>(&self, f: impl FnOnce(&Self) -> T) -> T {
        use fs2::FileExt;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock_path)
            .expect("open deliveries.lock");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.lock_path, fs::Permissions::from_mode(0o600));
        }
        match lock.lock_exclusive() {
            Ok(()) => {
                let out = f(self);
                let _ = lock.unlock();
                out
            }
            Err(e) => {
                crate::log!("[deliver] 拿不到 deliveries.lock（{e:#}），降级不加锁执行");
                f(self)
            }
        }
    }

    /// 写盘：唯一 tmp（固定 tmp 名会被并发进程互相覆盖）+ rename 原子替换 + 0600。
    fn write(&self, data: &[DeliveryItem]) {
        let new_text = serde_json::to_string_pretty(data).unwrap_or_default();
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
        if fs::write(&tmp, &new_text).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 队列含消息文本/本地路径，随 config 同级落 0600（等同凭证文件）
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        if fs::rename(&tmp, &self.path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    fn read(&self) -> Vec<DeliveryItem> {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// 入队（幂等：同 id 已存在则跳过）。持锁读-改-写，不覆盖并发写入。
    pub fn add(&self, item: DeliveryItem) {
        self.with_exclusive(|me| {
            let mut cur = me.read();
            if cur.iter().any(|x| x.id == item.id) {
                return; // 幂等
            }
            cur.push(item.clone());
            me.write(&cur);
        });
    }

    /// 取当前全部待投递项（不移除；投递完成后由 service 调 ack 逐项确认）。
    pub fn pending(&self) -> Vec<DeliveryItem> {
        self.read()
    }

    /// 投递完成后确认移除指定 id（持锁；同一批里已被并发 ack 的项直接跳过）。
    pub fn ack(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        self.with_exclusive(|me| {
            let cur = me.read();
            let next: Vec<DeliveryItem> = cur
                .iter()
                .filter(|x| !ids.contains(&x.id))
                .cloned()
                .collect();
            if next.len() != cur.len() {
                me.write(&next);
            }
        });
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
/// 跨会话投递路由：service 启动时按所有已启用 bot 构建，投递循环持引用消费队列。
pub struct Router {
    /// 总开关（Config.cross_delivery_enabled）。false 时任何投递直接拒绝（不发出）。
    pub enabled: bool,
    /// bot_key → Messenger（与各 Bridge 共享同一实例）。
    pub messengers: HashMap<String, Arc<dyn Messenger>>,
    /// bot_key → 配置（判断微信通道走 outbox 兜底；供日志/回源提示）。
    pub bots: HashMap<String, BotConfig>,
    /// outbox 落盘目录覆盖（测试注入用；生产 None = 按目标 bot 工作区默认路径）。
    pub outbox_dir: Option<PathBuf>,
    /// 最近投递指纹（防循环 #21）：`(时间, 来源+目标+文本指纹)`，窗口内重复投递直接跳过。
    recent: Mutex<VecDeque<(u64, String)>>,
}

/// 防循环去重窗口（秒）：同一 (来源 bot/会话, 目标 bot/会话, 文本) 在窗口内只投一次。
const RECENT_WINDOW_SECS: u64 = 600;

impl Router {
    pub fn new(
        enabled: bool,
        messengers: HashMap<String, Arc<dyn Messenger>>,
        bots: HashMap<String, BotConfig>,
        outbox_dir: Option<PathBuf>,
    ) -> Router {
        Router {
            enabled,
            messengers,
            bots,
            outbox_dir,
            recent: Mutex::new(VecDeque::new()),
        }
    }

    /// 投递一条消息。成功只留日志；失败：微信目标落其 outbox 等下次入站补发，
    /// 同时回源 bot/会话报错（best-effort，不静默丢失）。未知目标/开关关闭也回源提示。
    /// 防循环：非定时任务的同指纹（来源/目标/文本+附件 sha256）在窗口内重复投递会跳过并回源提示；
    /// 定时任务（job_id 非空）是合法重复，不走去重，也豁免自环拒绝（任务回发本 bot 原会话是既有行为）。
    pub async fn deliver(&self, item: &DeliveryItem) {
        // 日志截断必须按字符（agent::truncate）：bot key/chat_id 可含中文，按字节切会 panic，
        // 而 deliver_loop 在 tokio::spawn 里，panic = 投递消费线程静默死亡（#21 审查 Critical）。
        let tb = crate::agent::truncate(&item.target_bot, 16);
        let tc = crate::agent::truncate(&item.target_chat, 16);
        let tid = &item.id[..item.id.len().min(8)]; // uuid 恒 ASCII，按字节切安全
                                                    // `in_session` 的豁免判据**只此一处**：必须是「显式声明」**且**来源确实等于目标。
                                                    // 后面的开关 / 自环 / 去重 / 指纹登记四处都用它，别各自去读裸 flag——否则伪造一条
                                                    // `in_session:true` 但来源≠目标的项（手改 deliveries.json，或 owner 会话用
                                                    // `AGENT_BRIDGE_BOT_KEY=… CHAT_ID=…` 覆盖 env 后调 --to-current）就能把
                                                    // `cross_delivery_enabled` 关着的跨会话投递和 10 分钟防循环窗口一起绕掉（审查 P2）。
        let in_session_ok = item.in_session && is_self_loop(item);
        // 开关只管跨会话；`in_session` 等价于「回复带附件」，不受它限制（CLI 侧同判）。
        if !self.enabled && !in_session_ok {
            crate::log!(
                "[deliver] 跳过投递：跨会话投递未开启（bot={} chat={} id={}）",
                tb,
                tc,
                tid
            );
            self.notify_source(
                item,
                "⚠️ 跨会话投递未开启：请在 ABB 设置里打开「跨会话投递」开关后重试。",
            )
            .await;
            return;
        }
        // 自环防护（Router 级兜底，防手改队列绕过 CLI）：来源==目标 且非定时任务 → 拒绝。
        // 两处豁免：① 定时任务（既有）；② `in_session`（CLI `--to-current` 显式声明的
        // 「发给当前会话」）——但它**只在来源确实等于目标时**才构成豁免；手改队列塞
        // in_session 却把来源写成别的会话，下面这条判定照旧拒（自环硬规则不退化）。
        if !in_session_ok && item.job_id.is_empty() && is_self_loop(item) {
            crate::log!(
                "[deliver] 跳过投递：自环（来源==目标）bot={} chat={} id={}",
                tb,
                tc,
                tid
            );
            self.notify_source(item, "⚠️ 跨会话投递失败：来源与目标相同（自环），已拒绝。")
                .await;
            return;
        }
        // 防循环（消息循环防护 #21）：非定时任务同指纹窗口内重复 → 跳过 + 回源说明。
        // 注意：MutexGuard 必须在任何 await 前 drop（std 锁不是 Send，跨 await 会编译失败）。
        // `in_session` 也豁免去重：10 分钟内说两次「再发我一次」应该都能发出去
        //（直发语义，不是跨会话中继）。
        if item.job_id.is_empty() && !in_session_ok && self.is_duplicate(item) {
            crate::log!(
                "[deliver] 跳过重复投递（防循环）bot={} chat={} id={}",
                tb,
                tc,
                tid
            );
            self.notify_source(
                item,
                "⚠️ 相同内容刚投递过，已跳过（防循环）。如需再次投递请稍后（10 分钟内抑制重复）。",
            )
            .await;
            return;
        }
        let Some(msgr) = self.messengers.get(&item.target_bot) else {
            crate::log!(
                "[deliver] 跳过投递：目标 bot 不存在或未启用（{}）",
                item.target_bot
            );
            self.notify_source(
                item,
                &format!(
                    "⚠️ 跨会话投递失败：目标 bot「{}」不存在或未启用。",
                    item.target_bot
                ),
            )
            .await;
            return;
        };
        // #87 暂停拦截：目标会话被 pause → 拒绝投递并回源提示（不静默丢）。
        // 覆盖 deliver CLI 与定时任务多目标（job_target_items）两条路径；
        // 暂停期消息不入队不发送，源会话收到明确原因。
        if crate::session_state::SessionState::production()
            .is_paused(&item.target_bot, &item.target_chat)
        {
            crate::log!(
                "[deliver] 拒绝投递：目标会话已暂停（#87）bot={} chat={} id={}",
                tb,
                tc,
                tid
            );
            self.notify_source(
                item,
                &format!(
                    "⚠️ 目标会话「{}」（bot {}）已暂停，投递被拒绝。恢复后再投（session resume）。",
                    item.target_chat, item.target_bot
                ),
            )
            .await;
            return;
        }
        // 先发文本，再逐个发附件。**每个附件独立成败**：某个失败要上报但绝不
        // 吞掉后面的附件（审查 #254：真发送引入了多种新失败模式——超大/格式
        // 不支持/能力边界，旧「首败即 return」会把一次投递里其余本可送达的
        // 附件连坐丢弃）。全部成功才进防循环登记（见 mark_delivered）。
        if !item.text.is_empty() {
            if let Err(e) = msgr.send_text(&item.target_chat, &item.text).await {
                self.fail_text(item, &e).await;
                return;
            }
        }
        let mut failed: Vec<String> = Vec::new();
        for meta in &item.attachments {
            if let Err(e) = msgr.send_attachment(&item.target_chat, meta).await {
                crate::log!(
                    "[deliver] 附件投递失败 bot={} chat={} 文件名={}: {e:#}",
                    item.target_bot,
                    tc,
                    meta.file_name
                );
                // 单条错误串也截断：平台返回的超长错误体若原样带上，3 条一样会顶爆
                // 平台单条文本上限（审查 #254 复核 N4）。按字符截断（可能含中文），
                // 被截时补省略号，免得文案看起来像正常结束。完整错误已进上面的日志。
                let detail = format!("{e:#}");
                let brief = crate::agent::truncate(&detail, 160);
                let brief = if brief.chars().count() < detail.chars().count() {
                    format!("{brief}…")
                } else {
                    brief
                };
                failed.push(format!(
                    "{}（{brief}）",
                    if meta.file_name.is_empty() {
                        "未命名附件"
                    } else {
                        meta.file_name.as_str()
                    }
                ));
            }
        }
        if !failed.is_empty() {
            // 只列前 3 条：附件多时全量拼接会顶爆平台单条文本上限，导致这条
            // **失败提示本身**发不出去，用户反而什么都收不到（审查 #254 P3-5）。
            let mut detail = failed
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("；");
            let rest = failed.len().saturating_sub(3);
            if rest > 0 {
                detail.push_str(&format!(" 等 {rest} 个"));
            }
            self.notify_source(
                item,
                &format!(
                    "⚠️ 跨会话投递到「{}」：{}/{} 个附件发送失败：{}。其余内容已送达。",
                    item.target_bot,
                    failed.len(),
                    item.attachments.len(),
                    detail
                ),
            )
            .await;
        }
        // 防循环指纹只在**完全成功**时登记：部分失败也记会让 10 分钟内的合法
        // 重试被「防循环」挡掉，失败的那件永远补不回来（审查 #254）。
        if !failed.is_empty() {
            return;
        }
        crate::log!(
            "[deliver] 已投递 bot={} chat={} id={} 文本长度={} 附件数={}",
            item.target_bot,
            tc,
            tid,
            item.text.chars().count(),
            item.attachments.len()
        );
        if item.job_id.is_empty() && !in_session_ok {
            self.mark_delivered(item);
        }
    }

    /// 防循环去重：同（来源 bot/会话, 目标 bot/会话, 文本, 附件 sha256 列表）在窗口内只投一次。
    /// 附件 sha256 进指纹：纯附件投递（文本为空）或同文本不同附件不应被误判重复。
    /// 投递指纹（防循环键）。
    fn dup_key(item: &DeliveryItem) -> String {
        let mut sha: Vec<&str> = item.attachments.iter().map(|a| a.sha256.as_str()).collect();
        sha.sort_unstable();
        format!(
            "{}|{}|{}|{}|{}|{}",
            item.source_bot,
            item.source_chat,
            item.target_bot,
            item.target_chat,
            item.text,
            sha.join(",")
        )
    }

    /// 查指纹是否命中近窗（**只查不记**——审查 #254：旧实现在检查时就把指纹写进
    /// recent，发送失败后 10 分钟内的合法重试会被「防循环」挡掉，槽位被失败的
    /// 投递污染）。记录移到 `mark_delivered`，仅在真正投递成功后调用。
    fn is_duplicate(&self, item: &DeliveryItem) -> bool {
        let dup_key = Self::dup_key(item);
        let mut recent = self.recent.lock().unwrap();
        let now = crate::chrono_lite::unix_secs();
        while recent
            .front()
            .map(|(t, _)| *t + RECENT_WINDOW_SECS < now)
            .unwrap_or(false)
        {
            recent.pop_front();
        }
        recent.iter().any(|(_, k)| k == &dup_key)
    }

    /// 投递成功后登记指纹（进防循环窗口）。
    fn mark_delivered(&self, item: &DeliveryItem) {
        let dup_key = Self::dup_key(item);
        let now = crate::chrono_lite::unix_secs();
        self.recent.lock().unwrap().push_back((now, dup_key));
    }

    /// 文本投递失败：微信目标落其 outbox 等下次入站补发（#9 模式），同时回源报错。
    async fn fail_text(&self, item: &DeliveryItem, e: &anyhow::Error) {
        crate::log!(
            "[deliver] 投递失败 bot={} chat={} id={}: {e:#}",
            item.target_bot,
            crate::agent::truncate(&item.target_chat, 16),
            &item.id[..item.id.len().min(8)]
        );
        if self
            .bots
            .get(&item.target_bot)
            .map(|b| b.is_wechat())
            .unwrap_or(false)
        {
            let store = match &self.outbox_dir {
                Some(dir) => crate::outbox::OutboxStore::new_at(dir.join("pending_outbox.json")),
                None => crate::outbox::OutboxStore::new(&item.target_bot),
            };
            store.add(crate::outbox::OutboxItem {
                id: item.id.clone(),
                chat_id: item.target_chat.clone(),
                text: item.text.clone(),
                created_at: item.created_at,
                attempts: 0,
                job_id: String::new(),
            });
            crate::log!(
                "[deliver] 已落 bot={} 的 outbox 等待入站补发",
                item.target_bot
            );
        }
        self.notify_source(
            item,
            &format!("⚠️ 跨会话投递到「{}」失败：{e:#}", item.target_bot),
        )
        .await;
    }

    /// 回源报错：投递失败要让人知道，不能静默丢。来源 bot/会话空（人工 CLI）则只留日志。
    async fn notify_source(&self, item: &DeliveryItem, msg: &str) {
        if item.source_bot.is_empty() || item.source_chat.is_empty() {
            return;
        }
        let Some(msgr) = self.messengers.get(&item.source_bot) else {
            crate::log!(
                "[deliver] 回源失败：来源 bot {} 不存在或未启用",
                item.source_bot
            );
            return;
        };
        if let Err(e) = msgr.send_text(&item.source_chat, msg).await {
            crate::log!(
                "[deliver] 回源报错发送失败 bot={} chat={}: {e:#}",
                item.source_bot,
                crate::agent::truncate(&item.source_chat, 16)
            );
        }
    }
}

/// 解析 `deliver` CLI 参数（main.rs run_deliver_cli 用；拆出便于单测）。
/// env_bot / env_chat 是桥 spawn agent 时注入的 AGENT_BRIDGE_BOT_KEY / AGENT_BRIDGE_CHAT_ID，
/// 作为来源（回源报错）缺省值；人工 CLI 可显式 --source-bot/--source-chat 覆盖。
pub fn parse_deliver_args(
    args: &[String],
    env_bot: &str,
    env_chat: &str,
) -> Result<DeliveryItem, String> {
    let mut target_bot: Option<String> = None;
    let mut target_chat: Option<String> = None;
    let mut text: Option<String> = None;
    let mut source_bot: Option<String> = None;
    let mut source_chat: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut in_session = false;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let val = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .map(|s| s.to_string())
                .ok_or_else(|| format!("参数 {} 缺少值", flag))
        };
        match flag {
            // 无值开关：把目标钉在当前会话（= 桥注入的来源），见 DeliveryItem.in_session。
            "--to-current" => in_session = true,
            "--bot" => target_bot = Some(val(&mut i)?),
            "--chat" => target_chat = Some(val(&mut i)?),
            "--text" => text = Some(val(&mut i)?),
            "--file" => files.push(val(&mut i)?),
            "--source-bot" => source_bot = Some(val(&mut i)?),
            "--source-chat" => source_chat = Some(val(&mut i)?),
            _ => return Err(format!("未知参数：{flag}")),
        }
        i += 1;
    }
    // --to-current：目标恒为当前会话（桥注入的 bot/chat），不接受显式目标与来源——
    // 来源必须**确实**等于目标，这是 in_session 豁免自环保护的前提。
    if in_session {
        if env_bot.is_empty() || env_chat.is_empty() {
            return Err(
                "--to-current 只能在 bot 会话里使用（缺 AGENT_BRIDGE_BOT_KEY/CHAT_ID 环境变量）"
                    .to_string(),
            );
        }
        if target_bot.is_some() || target_chat.is_some() {
            return Err("--to-current 的目标就是当前会话，不要再给 --bot/--chat".to_string());
        }
        if source_bot.is_some() || source_chat.is_some() {
            return Err(
                "--to-current 的来源恒为当前会话，不要再给 --source-bot/--source-chat".to_string(),
            );
        }
        target_bot = Some(env_bot.to_string());
        target_chat = Some(env_chat.to_string());
    }
    let target_bot = target_bot.ok_or_else(|| "缺少 --bot <目标 bot key>".to_string())?;
    let target_chat = target_chat.ok_or_else(|| "缺少 --chat <目标 chat_id>".to_string())?;
    let text = text.unwrap_or_default();
    if target_bot.is_empty() {
        return Err("--bot 不能为空".to_string());
    }
    if target_chat.is_empty() {
        return Err("--chat 不能为空".to_string());
    }
    if text.trim().is_empty() && files.is_empty() {
        return Err("--text 和 --file 至少给一个".to_string());
    }
    // 纯附件投递：全空白 text 归一为空（避免给目标发一条空白消息）；
    // 非空白时保留原始文本（不 trim，避免吃掉代码块等有意义的首尾空白）。
    let text = if text.trim().is_empty() {
        String::new()
    } else {
        text
    };
    let mut attachments = Vec::with_capacity(files.len());
    for f in &files {
        attachments.push(attachment_meta_from_path(f)?);
    }
    Ok(DeliveryItem {
        id: uuid::Uuid::new_v4().to_string(),
        target_bot,
        target_chat,
        text,
        source_bot: source_bot
            .filter(|s| !s.is_empty())
            .or_else(|| (!env_bot.is_empty()).then(|| env_bot.to_string()))
            .unwrap_or_default(),
        source_chat: source_chat
            .filter(|s| !s.is_empty())
            .or_else(|| (!env_chat.is_empty()).then(|| env_chat.to_string()))
            .unwrap_or_default(),
        created_at: crate::chrono_lite::unix_secs(),
        attachments,
        job_id: String::new(),
        in_session,
    })
}

/// 解析 + 虚拟 Bot @角色名寻址（#75）：`--chat @角色名` 时查登记表
/// （同 bot 上下文，role_name 匹配）→ 替换成 chat_id；找不到报错并列出可用角色。
/// `roles` 参数化登记表：生产 = VirtualBotStore::new()（默认路径），单测注入临时 store，
/// 不碰真实登记表。main.rs run_deliver_cli 用这个，旧 parse_deliver_args 保持纯解析。
pub fn parse_deliver_args_with_store(
    args: &[String],
    env_bot: &str,
    env_chat: &str,
    roles: &crate::virtualbot::VirtualBotStore,
) -> Result<DeliveryItem, String> {
    let mut item = parse_deliver_args(args, env_bot, env_chat)?;
    if let Some(role) = item.target_chat.strip_prefix('@') {
        if role.is_empty() {
            return Err("--chat 以 @ 开头时需跟角色名（如 --chat @后端开发）".to_string());
        }
        match roles.resolve(&item.target_bot, role) {
            Some(chat_id) => item.target_chat = chat_id,
            None => {
                let available = roles.roles_for(&item.target_bot);
                let list = if available.is_empty() {
                    "该 bot 暂无已登记的虚拟 Bot（GUI 里「虚拟 Bot → ＋ 创建」）。".to_string()
                } else {
                    format!(
                        "已登记角色：{}。",
                        available
                            .iter()
                            .map(|r| format!("@{r}"))
                            .collect::<Vec<_>>()
                            .join("、")
                    )
                };
                return Err(format!(
                    "未找到虚拟 Bot「@{role}」（bot={}）。{list}",
                    item.target_bot
                ));
            }
        }
    }
    Ok(item)
}

/// 把本地文件转成附件元数据（deliver --file 用）。跨 bot 同机运行，接收端可直接读本地路径。
pub fn attachment_meta_from_path(path: &str) -> Result<crate::attachments::AttachmentMeta, String> {
    let p = std::path::Path::new(path);
    let file_name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let bytes = std::fs::read(p).map_err(|e| format!("读取附件失败 {path}: {e}"))?;
    if bytes.is_empty() {
        return Err(format!("附件为空：{path}"));
    }
    if bytes.len() > crate::attachments::MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "附件超过大小上限（{}MB）：{path}",
            crate::attachments::MAX_ATTACHMENT_BYTES / 1024 / 1024
        ));
    }
    let kind = crate::attachments::kind_from_name(&file_name);
    Ok(crate::attachments::AttachmentMeta {
        kind: kind.clone(),
        source: "deliver".to_string(),
        file_name: file_name.clone(),
        mime: crate::attachments::mime_from_name(&file_name, &kind),
        size: bytes.len() as u64,
        path: std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string(),
        sha256: crate::attachments::sha256_hex(&bytes),
        note: String::new(),
    })
}

/// 是否自环投递：来源 == 目标（同 bot 同会话转发给自己，无意义且是循环温床）。
pub fn is_self_loop(item: &DeliveryItem) -> bool {
    !item.source_bot.is_empty()
        && !item.source_chat.is_empty()
        && item.source_bot == item.target_bot
        && item.source_chat == item.target_chat
}

/// 定时任务多目标（#21）：把 Job.targets 展开成待投递项（bot_key 空 = 本 bot）。
/// 来源固定为任务所属 bot/原会话，投递失败由 Router 回源报错。
pub fn job_target_items(
    job_bot: &str,
    job_chat: &str,
    job_id: &str,
    targets: &[crate::schedule::JobTarget],
    text: &str,
) -> Vec<DeliveryItem> {
    targets
        .iter()
        .map(|t| DeliveryItem {
            id: uuid::Uuid::new_v4().to_string(),
            target_bot: if t.bot_key.is_empty() {
                job_bot.to_string()
            } else {
                t.bot_key.clone()
            },
            target_chat: t.chat_id.clone(),
            text: text.to_string(),
            source_bot: job_bot.to_string(),
            source_chat: job_chat.to_string(),
            created_at: crate::chrono_lite::unix_secs(),
            attachments: Vec::new(),
            job_id: job_id.to_string(),
            in_session: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 测试用临时目录：返回守卫（Drop 时清理）+ store。
    struct TmpDir(std::path::PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp_store(name: &str) -> (TmpDir, DeliveryStore) {
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-{name}-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let store = DeliveryStore::new_at(dir.join("deliveries.json"));
        (TmpDir(dir), store)
    }

    fn item(id: &str, target_bot: &str, target_chat: &str, text: &str) -> DeliveryItem {
        DeliveryItem {
            id: id.to_string(),
            target_bot: target_bot.to_string(),
            target_chat: target_chat.to_string(),
            text: text.to_string(),
            source_bot: String::new(),
            source_chat: String::new(),
            created_at: 1,
            attachments: Vec::new(),
            job_id: String::new(),
            in_session: false,
        }
    }

    #[test]
    fn store_add_dedupes_by_id() {
        let (_d, store) = tmp_store("dedupe");
        store.add(item("a", "wechat", "u1", "hi"));
        store.add(item("a", "wechat", "u1", "hi"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_pending_then_ack_removes_only_ids() {
        let (_d, store) = tmp_store("ack");
        store.add(item("a", "wechat", "u1", "hi"));
        store.add(item("b", "feishu", "c2", "yo"));
        let pending = store.pending();
        assert_eq!(pending.len(), 2);
        // ack 只移除指定 id
        store.ack(&["a".to_string()]);
        assert_eq!(store.pending().len(), 1);
        assert_eq!(store.pending()[0].id, "b");
        store.ack(&["b".to_string()]);
        assert!(store.is_empty());
    }

    #[test]
    fn store_pending_sees_external_write() {
        let (d, store) = tmp_store("reload");
        store.add(item("a", "wechat", "u1", "old"));
        // 模拟 CLI 进程在另一个进程直接覆盖文件（无缓存，天然可见）
        let path = d.0.join("deliveries.json");
        std::fs::write(
            &path,
            serde_json::to_string(&vec![item("b", "feishu", "c2", "new")]).unwrap(),
        )
        .unwrap();
        let pending = store.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "b");
    }

    #[test]
    fn store_concurrent_add_ack_no_loss() {
        // 用多个独立 store 实例指向同一文件（模拟 CLI/service 跨进程 CAS），
        // 并发 add 后 ack 一部分：不应丢任何未 ack 的项。
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-conc-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("deliveries.json");
        let stores: Vec<DeliveryStore> = (0..4)
            .map(|_| DeliveryStore::new_at(path.clone()))
            .collect();
        let mut handles = Vec::new();
        for (i, st) in stores.into_iter().enumerate() {
            handles.push(std::thread::spawn(move || {
                for j in 0..10 {
                    st.add(item(&format!("{i}-{j}"), "wechat", "u1", "hi"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let checker = DeliveryStore::new_at(path.clone());
        assert_eq!(checker.pending().len(), 40);
        let ids: Vec<String> = checker
            .pending()
            .iter()
            .take(10)
            .map(|x| x.id.clone())
            .collect();
        checker.ack(&ids);
        assert_eq!(checker.pending().len(), 30);
        let rest: Vec<String> = checker.pending().iter().map(|x| x.id.clone()).collect();
        checker.ack(&rest);
        assert!(checker.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_uses_env_defaults_for_source() {
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--text".to_string(),
            "hello".to_string(),
        ];
        let d = parse_deliver_args(&args, "wechat", "u1").unwrap();
        assert_eq!(d.target_bot, "feishu");
        assert_eq!(d.target_chat, "c1");
        assert_eq!(d.text, "hello");
        assert_eq!(d.source_bot, "wechat");
        assert_eq!(d.source_chat, "u1");
        assert!(!d.id.is_empty());
    }

    /// `--to-current`：目标钉在当前会话（= 桥注入的 env），并置 in_session。
    /// 这是 in_session 豁免自环保护的前提——来源必须**确实**等于目标。
    #[test]
    fn parse_to_current_pins_target_to_env_session() {
        let args = vec![
            "--to-current".to_string(),
            "--text".to_string(),
            "hi".to_string(),
        ];
        let d = parse_deliver_args(&args, "wechat", "u1").unwrap();
        assert_eq!(d.target_bot, "wechat");
        assert_eq!(d.target_chat, "u1");
        assert_eq!(d.source_bot, "wechat");
        assert_eq!(d.source_chat, "u1");
        assert!(d.in_session, "应标记为「发给当前会话」");
        assert!(is_self_loop(&d), "in_session 的项必须来源==目标");
    }

    /// `--to-current` 在 bot 会话之外（无 env）没有意义：拒绝，别猜目标。
    #[test]
    fn parse_to_current_requires_bot_session_env() {
        let args = vec![
            "--to-current".to_string(),
            "--text".to_string(),
            "hi".to_string(),
        ];
        for (b, c) in [("", ""), ("wechat", ""), ("", "u1")] {
            let e = parse_deliver_args(&args, b, c).unwrap_err();
            assert!(
                e.contains("只能在 bot 会话里使用"),
                "env=({b:?},{c:?}) → {e}"
            );
        }
    }

    /// `--to-current` 自己就是目标与来源，不接受重复指定（否则来源≠目标时
    /// in_session 的豁免前提被破坏）。
    #[test]
    fn parse_to_current_rejects_explicit_target_or_source() {
        let mk = |extra: &[&str]| {
            let mut v = vec!["--to-current", "--text", "hi"];
            v.extend_from_slice(extra);
            v.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        };
        for extra in [
            vec!["--bot", "feishu"],
            vec!["--chat", "c1"],
            vec!["--source-bot", "feishu"],
            vec!["--source-chat", "c1"],
        ] {
            let e = parse_deliver_args(&mk(&extra), "wechat", "u1").unwrap_err();
            assert!(
                e.contains("不要再给"),
                "extra={extra:?} 应被拒并被提示重复指定: {e}"
            );
        }
    }

    #[test]
    fn parse_requires_target_and_text() {
        assert!(parse_deliver_args(&[], "wechat", "u1").is_err());
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
        ];
        assert!(parse_deliver_args(&args, "wechat", "u1").is_err());
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--text".to_string(),
            "   ".to_string(),
        ];
        assert!(parse_deliver_args(&args, "wechat", "u1").is_err());
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let args = vec!["--nope".to_string()];
        assert!(parse_deliver_args(&args, "wechat", "u1").is_err());
    }

    // ─── 虚拟 Bot @角色名寻址（#75）───

    /// 临时登记表（不碰真实 ~/.agent-bridge/virtual-bots.json）。
    fn tmp_roles(name: &str) -> (std::path::PathBuf, crate::virtualbot::VirtualBotStore) {
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-roles-{name}-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let store = crate::virtualbot::VirtualBotStore::new_at(dir.join("virtual-bots.json"));
        (dir, store)
    }

    fn role_args(chat: &str) -> Vec<String> {
        vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            chat.to_string(),
            "--text".to_string(),
            "hello".to_string(),
        ]
    }

    #[test]
    fn parse_resolves_role_alias_to_chat_id() {
        let (dir, store) = tmp_roles("resolve");
        store
            .add(crate::virtualbot::VirtualBot {
                bot_key: "feishu".into(),
                chat_id: "oc_vb_1".into(),
                role_name: "后端开发".into(),
                created_at: 1,
            })
            .unwrap();
        let item =
            parse_deliver_args_with_store(&role_args("@后端开发"), "wechat", "u1", &store).unwrap();
        assert_eq!(item.target_chat, "oc_vb_1", "@角色名 应解析成 chat_id");
        assert_eq!(item.target_bot, "feishu");
        // 非 @ 目标原样透传
        let plain =
            parse_deliver_args_with_store(&role_args("oc_x"), "wechat", "u1", &store).unwrap();
        assert_eq!(plain.target_chat, "oc_x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_role_unknown_lists_available_roles() {
        let (dir, store) = tmp_roles("unknown");
        store
            .add(crate::virtualbot::VirtualBot {
                bot_key: "feishu".into(),
                chat_id: "oc_1".into(),
                role_name: "后端开发".into(),
                created_at: 1,
            })
            .unwrap();
        store
            .add(crate::virtualbot::VirtualBot {
                bot_key: "feishu".into(),
                chat_id: "oc_2".into(),
                role_name: "产品经理".into(),
                created_at: 1,
            })
            .unwrap();
        // 找不到的角色 → 报错 + 列出可用角色（寻址失败可见）
        let e = parse_deliver_args_with_store(&role_args("@不存在"), "wechat", "u1", &store)
            .unwrap_err();
        assert!(e.contains("不存在"), "{e}");
        assert!(e.contains("@后端开发"), "应列出可用角色: {e}");
        assert!(e.contains("@产品经理"), "{e}");
        // 空角色名 → 明确报错
        let e2 =
            parse_deliver_args_with_store(&role_args("@"), "wechat", "u1", &store).unwrap_err();
        assert!(e2.contains("@"), "{e2}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_role_scoped_by_bot() {
        let (dir, store) = tmp_roles("scoped");
        store
            .add(crate::virtualbot::VirtualBot {
                bot_key: "feishu".into(),
                chat_id: "oc_1".into(),
                role_name: "后端开发".into(),
                created_at: 1,
            })
            .unwrap();
        // 同角色名登记在别的 bot：寻址失败（同 bot 上下文匹配），报错列表也按 bot 过滤
        let e = parse_deliver_args_with_store(
            &[
                "--bot".to_string(),
                "dingtalk".to_string(),
                "--chat".to_string(),
                "@后端开发".to_string(),
                "--text".to_string(),
                "hi".to_string(),
            ],
            "wechat",
            "u1",
            &store,
        )
        .unwrap_err();
        assert!(e.contains("未找到"), "{e}");
        // 错误文案里会引用被查的角色名本身（「@后端开发」），所以不能断言「不含角色名」；
        // 应断言没有进入「已登记角色列表」分支（列表按 bot 过滤，同 bot 无登记 → 提示空）
        assert!(
            !e.contains("已登记角色"),
            "可用角色列表不该混入其它 bot: {e}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 测试假 messenger：记录发送，可配置失败。
    struct FakeMsgr {
        sent: Mutex<Vec<(String, String)>>,
        /// 真正走到 `send_attachment` 的附件（（chat_id, 文件名））。必须覆盖该
        /// 方法而不是依赖默认实现——逐件成败/指纹不污染这两条新语义只有能让
        /// 附件失败才测得出来（审查 #254 P2-1）。
        attachments: Mutex<Vec<(String, String)>>,
        fail: AtomicBool,
        /// 命中这些文件名的附件发送失败（空 = 全部成功）。
        fail_attachments: Mutex<Vec<String>>,
    }
    impl FakeMsgr {
        fn new() -> FakeMsgr {
            FakeMsgr {
                sent: Mutex::new(Vec::new()),
                attachments: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
                fail_attachments: Mutex::new(Vec::new()),
            }
        }
        /// 让指定文件名的附件发送失败。
        fn fail_attachment(&self, name: &str) {
            self.fail_attachments.lock().unwrap().push(name.to_string());
        }
    }
    #[async_trait]
    impl Messenger for FakeMsgr {
        async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
            if self.fail.load(Ordering::Relaxed) {
                anyhow::bail!("模拟发送失败");
            }
            self.sent
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
            Ok(())
        }
        async fn send_attachment(
            &self,
            chat_id: &str,
            meta: &crate::attachments::AttachmentMeta,
        ) -> Result<()> {
            if self
                .fail_attachments
                .lock()
                .unwrap()
                .iter()
                .any(|n| n == &meta.file_name)
            {
                anyhow::bail!("模拟附件发送失败: {}", meta.file_name);
            }
            self.attachments
                .lock()
                .unwrap()
                .push((chat_id.to_string(), meta.file_name.clone()));
            Ok(())
        }
    }

    /// 最小附件元数据（只有 send_attachment / 指纹关心的字段）。
    fn attach(name: &str) -> crate::attachments::AttachmentMeta {
        crate::attachments::AttachmentMeta {
            kind: "file".into(),
            source: "deliver".into(),
            file_name: name.into(),
            mime: "application/octet-stream".into(),
            size: 1,
            path: format!("/tmp/{name}"),
            sha256: format!("sha-{name}"),
            note: String::new(),
        }
    }

    fn router_with(
        enabled: bool,
        outbox_dir: Option<std::path::PathBuf>,
    ) -> (Router, Arc<FakeMsgr>, Arc<FakeMsgr>) {
        let target = Arc::new(FakeMsgr::new());
        let source = Arc::new(FakeMsgr::new());
        let mut bots = HashMap::new();
        bots.insert(
            "wechat".to_string(),
            BotConfig {
                name: "wechat".into(),
                kind: "wechat".into(),
                ..Default::default()
            },
        );
        bots.insert(
            "feishu".to_string(),
            BotConfig {
                name: "feishu".into(),
                kind: "feishu".into(),
                ..Default::default()
            },
        );
        let router = Router::new(
            enabled,
            HashMap::from([
                ("wechat".to_string(), target.clone() as Arc<dyn Messenger>),
                ("feishu".to_string(), source.clone() as Arc<dyn Messenger>),
            ]),
            bots,
            outbox_dir,
        );
        (router, target, source)
    }

    #[tokio::test]
    async fn router_disabled_rejects_and_notifies_source() {
        let (router, target, source) = router_with(false, None);
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        assert!(target.sent.lock().unwrap().is_empty());
        assert_eq!(source.sent.lock().unwrap().len(), 1);
        assert!(source.sent.lock().unwrap()[0].0 == "c1");
    }

    #[tokio::test]
    async fn router_sends_to_target() {
        let (router, target, source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        assert_eq!(target.sent.lock().unwrap().len(), 1);
        assert_eq!(
            target.sent.lock().unwrap()[0],
            ("u1".to_string(), "hi".to_string())
        );
        assert!(source.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn router_unknown_target_notifies_source() {
        let (router, target, source) = router_with(true, None);
        let mut d = item("a", "ghost", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        assert!(target.sent.lock().unwrap().is_empty());
        assert_eq!(source.sent.lock().unwrap().len(), 1);
        assert!(source.sent.lock().unwrap()[0].1.contains("ghost"));
    }

    #[tokio::test]
    async fn router_failure_queues_wechat_outbox_and_notifies_source() {
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-outbox-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let (router, target, source) = router_with(true, Some(dir.clone()));
        target.fail.store(true, Ordering::Relaxed);
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        assert!(target.sent.lock().unwrap().is_empty());
        // 回源报错
        assert_eq!(source.sent.lock().unwrap().len(), 1);
        assert!(source.sent.lock().unwrap()[0].1.contains("失败"));
        // 微信目标落 outbox（文件存在且可读出该项）
        let path = dir.join("pending_outbox.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"id\": \"a\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_accepts_file_attachments() {
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-file-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pic.png");
        std::fs::write(&path, b"fake-png-bytes").unwrap();
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--file".to_string(),
            path.display().to_string(),
        ];
        let d = parse_deliver_args(&args, "wechat", "u1").unwrap();
        assert_eq!(d.attachments.len(), 1);
        assert_eq!(d.attachments[0].kind, "image");
        assert_eq!(d.attachments[0].mime, "image/png");
        assert_eq!(d.attachments[0].size, 14);
        assert_eq!(d.attachments[0].sha256.len(), 64);
        assert!(d.attachments[0].path.contains("pic.png"));
        // 不存在的文件 → 报错
        let bad = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--file".to_string(),
            dir.join("nope.png").display().to_string(),
        ];
        assert!(parse_deliver_args(&bad, "wechat", "u1").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_allows_file_only() {
        // --text 空 + --file 有 → 允许（纯附件投递）
        let dir = std::env::temp_dir().join(format!(
            "abb-deliver-fileonly-{}",
            crate::chrono_lite::unix_secs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("a.txt");
        std::fs::write(&path, b"hi").unwrap();
        let args = vec![
            "--bot".to_string(),
            "feishu".to_string(),
            "--chat".to_string(),
            "c1".to_string(),
            "--text".to_string(),
            " ".to_string(),
            "--file".to_string(),
            path.display().to_string(),
        ];
        assert!(parse_deliver_args(&args, "wechat", "u1").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn self_loop_detected() {
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "wechat".into();
        d.source_chat = "u1".into();
        assert!(is_self_loop(&d));
        d.source_chat = "u2".into();
        assert!(!is_self_loop(&d));
        // 来源为空（人工 CLI）不算自环
        let d2 = item("b", "wechat", "u1", "hi");
        assert!(!is_self_loop(&d2));
    }

    #[test]
    fn job_target_items_expands_bot_key() {
        let targets = vec![
            crate::schedule::JobTarget {
                bot_key: String::new(),
                chat_id: "oc_own".into(),
            },
            crate::schedule::JobTarget {
                bot_key: "feishu".into(),
                chat_id: "oc_feishu".into(),
            },
        ];
        let items = job_target_items("wechat", "oc_src", "job-1", &targets, "结果");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].target_bot, "wechat"); // bot_key 空 → 本 bot
        assert_eq!(items[0].target_chat, "oc_own");
        assert_eq!(items[1].target_bot, "feishu");
        assert_eq!(items[1].source_bot, "wechat");
        assert_eq!(items[1].source_chat, "oc_src");
        assert_eq!(items[1].text, "结果");
    }

    #[tokio::test]
    async fn router_sends_attachments_through_send_attachment() {
        let (router, target, _source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "看附件");
        d.attachments.push(attach("pic.png"));
        router.deliver(&d).await;
        {
            let sent = target.sent.lock().unwrap();
            assert_eq!(sent.len(), 1, "文本照发");
            assert_eq!(sent[0], ("u1".to_string(), "看附件".to_string()));
        }
        assert_eq!(
            target.attachments.lock().unwrap().clone(),
            vec![("u1".to_string(), "pic.png".to_string())],
            "附件必须走 send_attachment 真发送，而非文本元数据降级"
        );
    }

    /// P2-1①：一件失败不得连坐吞掉后面的附件（旧「首败即 return」会把同一次
    /// 投递里其余本可送达的附件全丢）。
    #[tokio::test]
    async fn router_keeps_sending_after_one_attachment_fails() {
        let (router, target, source) = router_with(true, None);
        target.fail_attachment("bad.png");
        let mut d = item("a", "wechat", "u1", "");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        d.attachments = vec![attach("bad.png"), attach("ok.png")];
        router.deliver(&d).await;
        assert_eq!(
            target.attachments.lock().unwrap().clone(),
            vec![("u1".to_string(), "ok.png".to_string())],
            "首个失败后，后续附件仍须投递"
        );
        let src = source.sent.lock().unwrap();
        assert_eq!(src.len(), 1, "失败必须回源提示");
        assert!(
            src[0].1.contains("1/2 个附件发送失败") && src[0].1.contains("bad.png"),
            "回源要说明几件失败、哪件: {}",
            src[0].1
        );
    }

    /// P2-1②：部分失败**不登记**防循环指纹——否则 10 分钟内的合法重试会被
    /// 「防循环」挡掉，失败那件永远补不回来。
    #[tokio::test]
    async fn router_partial_failure_leaves_retry_window_open() {
        let (router, target, source) = router_with(true, None);
        target.fail_attachment("bad.png");
        let mut d = item("a", "wechat", "u1", "");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        d.attachments = vec![attach("bad.png")];
        router.deliver(&d).await;
        assert!(target.attachments.lock().unwrap().is_empty());
        // 用户重试（同来源/目标/文本/附件指纹）：必须真的再投一次
        target.fail_attachments.lock().unwrap().clear();
        router.deliver(&d).await;
        assert_eq!(
            target.attachments.lock().unwrap().len(),
            1,
            "部分失败后同指纹重试不得被防循环拦"
        );
        assert!(
            !source
                .sent
                .lock()
                .unwrap()
                .iter()
                .any(|(_, t)| t.contains("防循环")),
            "失败投递不得占用防循环槽位"
        );
    }

    /// P3-5（复核 N4）：失败件数 >3 时回源提示只列前 3 条 + 「等 N 个」——附件多
    /// 时全量拼接会顶爆平台单条文本上限，这条提示**自己**发不出去。
    #[tokio::test]
    async fn router_failure_notice_lists_only_first_three() {
        let (router, target, source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        d.attachments = (1..=5)
            .map(|i| {
                let name = format!("bad{i}.png");
                target.fail_attachment(&name);
                attach(&name)
            })
            .collect();
        router.deliver(&d).await;
        assert!(target.attachments.lock().unwrap().is_empty(), "5 件全失败");
        let src = source.sent.lock().unwrap();
        assert_eq!(src.len(), 1);
        let msg = &src[0].1;
        assert!(msg.contains("5/5 个附件发送失败"), "{msg}");
        assert!(msg.contains("bad3.png"), "前 3 条要列全: {msg}");
        assert!(msg.contains("等 2 个"), "超出部分折叠成计数: {msg}");
        assert!(
            !msg.contains("bad4.png") && !msg.contains("bad5.png"),
            "第 4/5 件不应出现在提示里: {msg}"
        );
    }

    /// P2-1③：全部成功才登记指纹——同指纹再来一次被防循环拦下。
    #[tokio::test]
    async fn router_all_success_marks_fingerprint() {
        let (router, target, source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        d.attachments = vec![attach("ok.png")];
        router.deliver(&d).await;
        router.deliver(&d).await;
        assert_eq!(
            target.attachments.lock().unwrap().len(),
            1,
            "全成功后同指纹应被防循环拦"
        );
        assert!(source.sent.lock().unwrap()[0].1.contains("防循环"));
    }

    #[tokio::test]
    async fn router_skips_duplicate_within_window() {
        let (router, target, source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await;
        router.deliver(&d).await;
        assert_eq!(target.sent.lock().unwrap().len(), 1);
        // 第二次被抑制并回源提示
        assert_eq!(source.sent.lock().unwrap().len(), 1);
        assert!(source.sent.lock().unwrap()[0].1.contains("防循环"));
    }

    #[tokio::test]
    async fn router_different_source_same_text_not_duplicate() {
        let (router, target, _source) = router_with(true, None);
        let mut d1 = item("a", "wechat", "u1", "hi");
        d1.source_bot = "feishu".into();
        d1.source_chat = "c1".into();
        let mut d2 = item("b", "wechat", "u1", "hi");
        d2.source_bot = "feishu".into();
        d2.source_chat = "c2".into();
        router.deliver(&d1).await;
        router.deliver(&d2).await;
        assert_eq!(target.sent.lock().unwrap().len(), 2); // 不同来源不算重复
    }

    #[tokio::test]
    async fn router_attachment_only_different_files_not_duplicate() {
        // 纯附件投递（文本为空）：不同文件（sha256 不同）不应被防循环误判为重复
        let (router, target, _source) = router_with(true, None);
        let meta = |sha: &str| crate::attachments::AttachmentMeta {
            kind: "image".into(),
            source: "deliver".into(),
            file_name: "a.png".into(),
            mime: "image/png".into(),
            size: 1,
            path: "/tmp/a.png".into(),
            sha256: sha.into(),
            note: String::new(),
        };
        let mut d1 = item("a", "wechat", "u1", "");
        d1.attachments.push(meta("sha1"));
        let mut d2 = item("b", "wechat", "u1", "");
        d2.attachments.push(meta("sha2"));
        router.deliver(&d1).await;
        router.deliver(&d2).await;
        assert_eq!(
            target.attachments.lock().unwrap().len(),
            2,
            "不同附件（sha256 不同）不得被防循环误判为重复"
        );
    }

    #[tokio::test]
    async fn router_job_items_skip_dedupe() {
        // 定时任务（job_id 非空）：同文本重复投递是合法周期任务，不去重
        let (router, target, source) = router_with(true, None);
        let mut d1 = item("a", "wechat", "u1", "hi");
        d1.job_id = "job-1".into();
        d1.source_bot = "feishu".into();
        d1.source_chat = "c1".into();
        let mut d2 = item("b", "wechat", "u1", "hi");
        d2.job_id = "job-1".into();
        d2.source_bot = "feishu".into();
        d2.source_chat = "c1".into();
        router.deliver(&d1).await;
        router.deliver(&d2).await;
        assert_eq!(target.sent.lock().unwrap().len(), 2);
        assert!(source.sent.lock().unwrap().is_empty());
    }

    /// `--to-current`（in_session）是唯一能在同会话投递附件/内容的通道：
    /// 来源确实等于目标时放行；来源≠目标（手改队列伪造 in_session）→ 仍按自环拒。
    /// 注意上面那条 `router_rejects_self_loop_but_exempts_jobs` 是**自环保护的回归锁**，
    /// 它必须继续绿——本 PR 没有放宽那条规则。
    #[tokio::test]
    async fn router_allows_in_session_but_only_when_source_equals_target() {
        let (router, target, _source) = router_with(true, None);
        // ① 正当的 in_session：来源==目标 → 真的发出去（不再拒）
        let mut d = item("a", "wechat", "u1", "报告好了");
        d.source_bot = "wechat".into();
        d.source_chat = "u1".into();
        d.in_session = true;
        router.deliver(&d).await;
        let sent = target.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "in_session 应放行: {sent:?}");
        assert_eq!(sent[0], ("u1".to_string(), "报告好了".to_string()));
        drop(sent);

        // ② in_session 只在「来源确实等于目标」时才构成豁免：来源不同 = **本来就不是自环**，
        //    此时它不作任何特殊处理，退化成一条普通投递（豁免逻辑不生效，也就没被滥用）。
        let mut cross = item("b", "wechat", "u1", "hi");
        cross.source_bot = "wechat".into();
        cross.source_chat = "OTHER_CHAT".into();
        cross.in_session = true;
        assert!(!is_self_loop(&cross), "来源≠目标根本不是自环");
        router.deliver(&cross).await;
        assert_eq!(
            target.sent.lock().unwrap().len(),
            2,
            "来源≠目标 + in_session = 普通投递照发（in_session 不产生额外豁免）"
        );
    }

    /// 「跨会话投递」开关只管控跨会话：in_session（发给当前会话）在开关关闭时**也要发**
    /// ——它等价于「回复带附件」，不该逼用户为这个去打开跨会话开关（冒烟测试发现的洞）。
    /// 对照组：同开关关闭下的普通跨会话投递仍被拒。
    #[tokio::test]
    async fn router_in_session_ignores_cross_delivery_switch() {
        let (router, target, source) = router_with(false, None);
        // ① 开关关 + in_session → 照发
        let mut d = item("a", "wechat", "u1", "报告好了");
        d.source_bot = "wechat".into();
        d.source_chat = "u1".into();
        d.in_session = true;
        router.deliver(&d).await;
        assert_eq!(
            target.sent.lock().unwrap().clone(),
            vec![("u1".to_string(), "报告好了".to_string())],
            "in_session 不受跨会话投递开关限制"
        );

        // ② 开关关 + 普通跨会话 → 仍拒（开关语义不退化）
        let mut cross = item("b", "wechat", "u2", "hi");
        cross.source_bot = "feishu".into();
        cross.source_chat = "c1".into();
        router.deliver(&cross).await;
        assert_eq!(target.sent.lock().unwrap().len(), 1, "跨会话仍被开关拒");
        assert_eq!(source.sent.lock().unwrap().len(), 1, "应回源提示未开启");
        assert!(source.sent.lock().unwrap()[0]
            .1
            .contains("跨会话投递未开启"));
    }

    /// in_session 不进防循环窗口：10 分钟内连着要两次同一个文件都应该发出去
    /// （直发语义，不是跨会话中继）。
    #[tokio::test]
    async fn router_in_session_is_exempt_from_dedup_window() {
        let (router, target, _source) = router_with(true, None);
        let mut d = item("a", "wechat", "u1", "");
        d.source_bot = "wechat".into();
        d.source_chat = "u1".into();
        d.in_session = true;
        d.attachments.push(attach("report.pdf"));
        router.deliver(&d).await;
        router.deliver(&d).await;
        assert_eq!(
            target.attachments.lock().unwrap().len(),
            2,
            "同一个文件的两次 in_session 发送都该出去（不被防循环压掉）"
        );
    }

    #[tokio::test]
    async fn router_rejects_self_loop_but_exempts_jobs() {
        let (router, target, source) = router_with(true, None);
        // 非定时任务自环 → Router 拒绝（防手改队列绕过 CLI）
        let mut d = item("a", "wechat", "u1", "hi");
        d.source_bot = "wechat".into();
        d.source_chat = "u1".into();
        router.deliver(&d).await;
        // 自环拒绝：来源==目标 bot，回源提示会发到同一 messenger（wechat）
        assert_eq!(target.sent.lock().unwrap().len(), 1);
        assert!(target.sent.lock().unwrap()[0].1.contains("自环"));
        assert!(source.sent.lock().unwrap().is_empty());
        // 定时任务回发本 bot 原会话（既有行为）→ 豁免
        let mut j = item("b", "wechat", "u1", "hi");
        j.job_id = "job-1".into();
        j.source_bot = "wechat".into();
        j.source_chat = "u1".into();
        router.deliver(&j).await;
        assert_eq!(target.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn router_long_chinese_names_no_panic() {
        // 审查 Critical：中文 bot key/chat 按字节切会 panic 杀死投递循环；这里回归验证
        let (router, _target, source) = router_with(true, None);
        let mut d = item(
            "a",
            "庆小丰的助手机器人",
            "oc_中文会话标识很长很长很长",
            "hi",
        );
        d.source_bot = "feishu".into();
        d.source_chat = "c1".into();
        router.deliver(&d).await; // 未知目标 → 回源提示，但绝不能 panic
        assert_eq!(source.sent.lock().unwrap().len(), 1);
    }
}
