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
    /// 微信待发积压（pending_outbox）：主动推送被微信拒绝（ret=-2 token stale）时落盘，
    /// 等用户下一条入站刷新 context_token 后补发。非微信 bot 空置。
    outbox: OutboxStore,
    /// 待处理消息持久化（#25 重启恢复）：进入 agent 前落盘、完成后删除；
    /// service 重启后 recover_pending 自动重放，续跑上次未完成的消息/会话。
    pending: PendingStore,
    /// Agent 执行器（#23 测试可测性）：仿 `msgr` 的 trait 注入——生产用 RealAgentRunner
    /// 转发 spawn 子进程，测试注入挡板以驱动「任务运行中」时序（详见 agent::AgentRunner）。
    agent_runner: Arc<dyn AgentRunner>,
    /// GitHub API（仿 agent_runner 的 trait 注入）：生产 = GithubClient（用 bot.gh_token），
    /// 测试 = MockGithub 挡板。handle 的 github 指令门与双写、watch 循环都走它。
    pub(crate) github_client: Arc<dyn crate::github::GithubApi>,
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

/// 一条 github 指令的处理结果。
enum GhOutcome {
    /// 注入 prompt 继续走 agent（最终回复双写）。
    Analyze(crate::github::GhContext),
    /// 直接 API 指令已完成并回复 → 调用方 return。
    Consumed,
    /// 白名单拒绝/参数不足，已带原因 → 调用方回复后 return。
    Rejected(String),
}

impl Bridge {
    pub fn new(msgr: Arc<dyn Messenger>, bot: BotConfig, cfg: &Config) -> Bridge {
        let gh_token = bot.gh_token.clone();
        Self::build(
            msgr,
            bot,
            cfg,
            Arc::new(agent::RealAgentRunner),
            Arc::new(crate::github::GithubClient::new(&gh_token)),
        )
    }

    /// 实际构造器：生产（`new` 用真实 `RealAgentRunner` + `GithubClient`）与测试（注入挡板
    /// `AgentRunner` / `GithubApi` 驱动时序）共用。字段初始化集中在此。
    fn build(
        msgr: Arc<dyn Messenger>,
        bot: BotConfig,
        cfg: &Config,
        agent_runner: Arc<dyn AgentRunner>,
        github_client: Arc<dyn crate::github::GithubApi>,
    ) -> Bridge {
        // 后端跟着 bot 走：用该 bot 的生效后端（自身 backend 非空优先，否则回落全局默认）。
        let effective = bot.effective_backend(&cfg.default_backend).to_string();
        let key = bot.key();
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
            outbox: OutboxStore::new(&key),
            pending: PendingStore::new(&key),
            agent_runner,
            github_client,
        }
    }

    /// config 读不到本 bot（单测注入）时，用构造时的访问控制快照判定放行。
    /// 快照 = self.bot（build 时从 config 复制，含 kind 与全部访问字段）。
    fn access_snapshot_allows(&self, sender_id: &str) -> bool {
        self.bot.access_allows(sender_id)
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

    /// 解析建 issue 指令的缺省仓库：显式 owner/repo 直接用；未带仓库 → 白名单恰好
    /// 一项时用它（通配项 o/* 不是具体仓库，拒绝）；多项/空 → 明确拒绝（防猜错仓库）。
    fn resolve_create_repo(&self, owner: &str, repo: &str) -> Result<(String, String), String> {
        if !repo.is_empty() {
            return Ok((owner.to_string(), repo.to_string()));
        }
        let list = self.bot.gh_repo_list();
        if list.len() == 1 {
            let (o, r) = list[0].split_once('/').unwrap_or(("", list[0].as_str()));
            if r == "*" {
                return Err(
                    "❌ 白名单是整组织通配（owner/*），创建 issue 请带上具体仓库：`建 issue owner/repo 标题`。".to_string(),
                );
            }
            Ok((o.to_string(), r.to_string()))
        } else {
            Err(
                "❌ 创建 issue 请带上仓库：`建 issue owner/repo 标题`（白名单多于一项时不能省略）。".to_string(),
            )
        }
    }

    /// 写操作的白名单前置闸（评审 S1）：空白名单 = 全部放行只适用于**读**（分析）；
    /// 写操作（关闭/建）在未配置白名单时直接拒绝——token 授权范围可能覆盖用户所有
    /// 仓库，空名单放行写操作等于群里任何能 @bot 的人可对任意授权仓库做写操作。
    fn gh_write_guard(&self, action: &str) -> Option<String> {
        if self.bot.gh_repo_list().is_empty() {
            Some(format!(
                "❌ 未配置仓库白名单，{action}已禁用。请先在设置窗「GitHub 能力 → 仓库白名单」配置。"
            ))
        } else {
            None
        }
    }

    /// 执行一条 github 指令。白名单在每个动作前强制校验（写操作无一绕过）。
    async fn handle_github_cmd(&self, ev: &Ev, cmd: crate::github::GhCmd) -> GhOutcome {
        match cmd {
            crate::github::GhCmd::ConfirmClose {
                owner,
                repo,
                number,
            } => {
                if let Some(msg) = self.gh_write_guard("关闭") {
                    return GhOutcome::Rejected(msg);
                }
                let repo_full = format!("{owner}/{repo}");
                if !self.bot.gh_allows_repo(&repo_full) {
                    return GhOutcome::Rejected(format!(
                        "❌ 仓库 {repo_full} 不在白名单内，已拒绝关闭。"
                    ));
                }
                // 关闭是破坏性操作：先引导确认，用户回复「确认关闭 <链接>」才真正执行
                // （防闲聊里的「别关闭/为什么关闭了 <链接>」误触发写操作）。
                let _ = self
                    .send_reply(
                        ev,
                        &format!(
                            "⚠️ 关闭 {repo_full}#{number} 是破坏性操作。确认请回复：\n确认关闭 https://github.com/{repo_full}/issues/{number}"
                        ),
                    )
                    .await;
                GhOutcome::Consumed
            }
            crate::github::GhCmd::Close {
                owner,
                repo,
                number,
            } => {
                if let Some(msg) = self.gh_write_guard("关闭") {
                    return GhOutcome::Rejected(msg);
                }
                let repo_full = format!("{owner}/{repo}");
                if !self.bot.gh_allows_repo(&repo_full) {
                    return GhOutcome::Rejected(format!(
                        "❌ 仓库 {repo_full} 不在白名单内，已拒绝关闭。"
                    ));
                }
                match self.github_client.close_issue(&owner, &repo, number).await {
                    Ok(()) => {
                        crate::log!("[bridge] 已关闭 {owner}/{repo}#{number}");
                        let _ = self
                            .send_reply(ev, &format!("✅ 已关闭 {owner}/{repo}#{number}。"))
                            .await;
                        GhOutcome::Consumed
                    }
                    Err(e) => {
                        crate::log!("[bridge] ⚠️ 关闭 {owner}/{repo}#{number} 失败: {e:#}");
                        let _ = self.send_reply(ev, &format!("❌ 关闭失败：{e:#}")).await;
                        GhOutcome::Consumed
                    }
                }
            }
            crate::github::GhCmd::ConfirmCreate { owner, repo, title } => {
                if let Some(msg) = self.gh_write_guard("创建 issue") {
                    return GhOutcome::Rejected(msg);
                }
                // 创建是公开写操作：先预览（仓库 + 标题），用户回复「确认建 issue <标题>」才执行。
                // 解析缺省仓库与 Create 同逻辑（单项白名单/显式 owner/repo）。
                let (owner, repo) = match self.resolve_create_repo(&owner, &repo) {
                    Ok(v) => v,
                    Err(msg) => return GhOutcome::Rejected(msg),
                };
                let repo_full = format!("{owner}/{repo}");
                if !self.bot.gh_allows_repo(&repo_full) {
                    return GhOutcome::Rejected(format!(
                        "❌ 仓库 {repo_full} 不在白名单内，已拒绝创建。"
                    ));
                }
                let _ = self
                    .send_reply(
                        ev,
                        &format!(
                            "⚠️ 将创建 issue「{}」→ {repo_full}。确认请回复：\n确认建 issue {repo_full} {}",
                            crate::agent::truncate(&title, 60),
                            crate::agent::truncate(&title, 60)
                        ),
                    )
                    .await;
                GhOutcome::Consumed
            }

            crate::github::GhCmd::Create { owner, repo, title } => {
                if let Some(msg) = self.gh_write_guard("创建 issue") {
                    return GhOutcome::Rejected(msg);
                }
                let (owner, repo) = match self.resolve_create_repo(&owner, &repo) {
                    Ok(v) => v,
                    Err(msg) => return GhOutcome::Rejected(msg),
                };
                let repo_full = format!("{owner}/{repo}");
                if !self.bot.gh_allows_repo(&repo_full) {
                    return GhOutcome::Rejected(format!(
                        "❌ 仓库 {repo_full} 不在白名单内，已拒绝创建。"
                    ));
                }
                match self.github_client.create_issue(&owner, &repo, &title).await {
                    Ok(url) => {
                        crate::log!(
                            "[bridge] 已创建 {owner}/{repo} issue「{}」",
                            crate::agent::truncate(&title, 20)
                        );
                        let _ = self.send_reply(ev, &format!("✅ 已创建：{url}")).await;
                        GhOutcome::Consumed
                    }
                    Err(e) => {
                        crate::log!("[bridge] ⚠️ 创建 issue 失败: {e:#}");
                        let _ = self.send_reply(ev, &format!("❌ 创建失败：{e:#}")).await;
                        GhOutcome::Consumed
                    }
                }
            }
            crate::github::GhCmd::Analyze {
                owner,
                repo,
                number,
            } => {
                let repo_full = format!("{owner}/{repo}");
                if !self.bot.gh_allows_repo(&repo_full) {
                    return GhOutcome::Rejected(format!(
                        "❌ 仓库 {repo_full} 不在白名单内，已拒绝分析。"
                    ));
                }
                // 并行拉 issue 详情 + 评论（独立请求，一次往返）
                let (issue, comments) = match tokio::try_join!(
                    self.github_client.fetch_issue(&owner, &repo, number),
                    self.github_client.list_comments(&owner, &repo, number),
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        crate::log!("[bridge] ⚠️ 拉取 {owner}/{repo}#{number} 失败: {e:#}");
                        let _ = self
                            .send_reply(ev, &format!("❌ 拉取 issue 失败：{e:#}"))
                            .await;
                        return GhOutcome::Consumed;
                    }
                };
                crate::log!(
                    "[bridge] 分析指令 {owner}/{repo}#{number} 评论 {} 条",
                    comments.len()
                );
                GhOutcome::Analyze(crate::github::GhContext::new(
                    owner, repo, number, issue, comments,
                ))
            }
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
        {
            let allowed = match crate::config::Config::load() {
                Ok(c) => c
                    .bots
                    .into_iter()
                    .find(|b| b.key() == self.bot.key())
                    .map(|b| b.access_allows(sender_id))
                    .unwrap_or_else(|| self.access_snapshot_allows(sender_id)),
                Err(_) => self.access_snapshot_allows(sender_id),
            };
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

        // GitHub 指令门（附挂在既有 IM bot 的能力，非新 bot kind）。
        // 位置：/new 之后、pending 落盘之前。
        //  - 直接 API 指令（关闭/建 issue）→ 就地执行回复后 return：不进 agent、不落盘 pending
        //    （动作已完成，重启重放会重复操作）；
        //  - 分析指令 → 只注入 issue 上下文到 prompt 继续走 agent，最终回复双写
        //    （issue 评论留档全文 + 群截断摘要）。
        // 访问控制不重复做：群 @ 过滤（on_payload）+ access_allows 已在进 handle 前把关；
        // 仓库白名单在 handle_github_cmd 里每个动作前强制校验（单一关卡）。
        // Consumed/Rejected 分支调用 pending.remove 是**无操作兜底**：实时路径上这些
        // 分支在 pending.add 之前就 return 了；但 pending 重放（#25）时条目已存在——
        // 若重放时 fetch 瞬失败（限流/网络）导致 Consumed，不摘除会每次重启都重放刷屏。
        let mut gh_ctx: Option<crate::github::GhContext> = None;
        if self.bot.is_github_capable() {
            if let Some(cmd) = crate::github::parse_github_cmd(&text) {
                match self.handle_github_cmd(&ev, cmd).await {
                    GhOutcome::Analyze(ctx) => gh_ctx = Some(ctx),
                    GhOutcome::Consumed => {
                        self.pending.remove(&ev.mid);
                        return;
                    }
                    GhOutcome::Rejected(msg) => {
                        self.pending.remove(&ev.mid);
                        let _ = self.send_reply(&ev, &msg).await;
                        return;
                    }
                }
            }
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
        // GitHub 分析指令：issue 内容注入 prompt（原始链接已在 [链接] 段，这里带实际内容）
        if let Some(ctx) = &gh_ctx {
            prompt.push_str("\n\n[GitHub Issue]");
            prompt.push('\n');
            prompt.push_str(&ctx.render);
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
            &bot_key,
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

        // #25：agent 已返回（Reply/Cancelled/Err 均视为「任务完成」）→ 摘掉 pending，
        // 避免重启后重复执行已完成的任务；回复发送失败仍走既有路径（日志/outbox），不重跑。
        self.pending.remove(&ev.mid);

        // 统一只发最终结果一条（中途进度已在 select 循环丢弃）。
        match result {
            Ok(agent::RunOutcome::Reply { reply, session_id }) => {
                // agent 成功即标记 started（会话状态只跟 agent 跑没跑成有关，与投递无关）。
                // #23：仅当当前槽位仍是本次任务的会话时才 mark——运行中被 /new 或
                // CLI `session reset` 换走时跳过（旧任务完成不得把新槽位置回 started=true）。
                self.sessions.mark_started_if(&key, &session_id);
                // GitHub 分析双写：全文回写 issue 评论留档，群聊只回截断摘要——
                // 避免超长分析刷屏，完整内容去 issue 页看。
                // 注：pending 重放是 at-least-once——崩溃窗口可能重跑并双发评论（与既有
                // pending 恢复语义一致，接受）。
                if let Some(ctx) = &gh_ctx {
                    let full = format!("[agent-bridge 分析结果]\n\n{reply}");
                    // 留档成败要如实反映到回执：失败时摘要明说「未留档」，不能假装成功。
                    let mut archived = true;
                    match self
                        .github_client
                        .post_comment(&ctx.owner, &ctx.repo, ctx.number, &full)
                        .await
                    {
                        Ok(()) => crate::log!(
                            "[bridge] 已把分析结果回写 {}/{}#{} 评论",
                            ctx.owner,
                            ctx.repo,
                            ctx.number
                        ),
                        Err(e) => {
                            archived = false;
                            crate::log!(
                                "[bridge] ⚠️ issue 评论回写失败 {}/{}#{}: {e:#}",
                                ctx.owner,
                                ctx.repo,
                                ctx.number
                            );
                        }
                    }
                    let summary = crate::agent::truncate(&reply, 200);
                    let text = if archived {
                        format!(
                            "📝 已分析 {}/{}#{}「{}」\n\n```\n{}\n```\n\n（完整结果已留档到 issue 评论）",
                            ctx.owner, ctx.repo, ctx.number, ctx.title, summary
                        )
                    } else {
                        format!(
                            "📝 已分析 {}/{}#{}「{}」\n\n```\n{}\n```\n\n（⚠️ 评论留档失败，全文仅此可见，可稍后重发「分析」补档）",
                            ctx.owner, ctx.repo, ctx.number, ctx.title, summary
                        )
                    };
                    match self.send_reply(&ev, &text).await {
                        Ok(()) => crate::log!(
                            "[bridge] 已回复 github 摘要 chat={} 长度={}",
                            trunc(&ev.chat_id, 10),
                            text.chars().count()
                        ),
                        Err(e) => crate::log!(
                            "[bridge] ⚠️ github 摘要发送失败 chat={}: {e:#}",
                            trunc(&ev.chat_id, 10)
                        ),
                    }
                } else {
                    // 原路径：普通回复全文发送。发送结果必须留痕：回复丢了
                    // （token 失效/会话失效等）时不能谎报成功。
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
            }
            Ok(agent::RunOutcome::Cancelled) => {
                crate::log!("[bridge] 任务被打断 chat={}", trunc(&ev.chat_id, 10));
                // 只发最终结果：「⏹ 已停止」一条。
                let _ = self.send_reply(&ev, "⏹ 已停止").await;
                // 不 mark_started：被打断的轮次不算完成
            }
            Err(e) => {
                // 错误文案作为最终回复发出（用户可见原因），同样留痕。
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
        // 访问控制（与飞书同套，staffId 标识）：公开开关开 → 放行所有人；否则只放行 owner ∪
        // 授权者白名单。每次热读 config（授权/取消/改开关即时生效）；config 读不到（单测）回落快照。
        let allowed = match crate::config::Config::load() {
            Ok(c) => c
                .bots
                .into_iter()
                .find(|b| b.key() == self.bot.key())
                .map(|b| b.access_allows(&msg.sender_staff_id))
                .unwrap_or_else(|| self.access_snapshot_allows(&msg.sender_staff_id)),
            Err(_) => self.access_snapshot_allows(&msg.sender_staff_id),
        };
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

    /// GitHub 挡板：记录调用序列（calls），fetch 结果可编程（默认成功）。用于驱动
    /// 指令门时序——close/create 直接 API、analyze 注入 + 双写、白名单拒绝。
    struct MockGithub {
        calls: Mutex<Vec<String>>,
        issue: Mutex<crate::github::GhIssue>,
        comments: Mutex<Vec<crate::github::GhComment>>,
        fail_fetch: bool,
        fail_post: bool,
        /// is_collaborator 返回值（默认 Some(true)=协作者；None=权限不足）；
        /// fail_collab=true 时返回 Err。
        collab: Option<bool>,
        fail_collab: bool,
    }
    impl MockGithub {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                issue: Mutex::new(crate::github::GhIssue {
                    id: 1,
                    number: 42,
                    title: "登录偶发 401".into(),
                    state: "open".into(),
                    html_url: "https://github.com/o/r/issues/42".into(),
                    body: "token 缓存竞态导致偶发 401。".into(),
                    created_at: "2026-08-14T01:00:00Z".into(),
                    updated_at: "2026-08-14T02:00:00Z".into(),
                    user: crate::github::GhUser {
                        login: "alice".into(),
                    },
                    pull_request: None,
                }),
                comments: Mutex::new(vec![crate::github::GhComment {
                    id: 101,
                    body: "复现了，见日志。".into(),
                    user: crate::github::GhUser {
                        login: "bob".into(),
                    },
                    created_at: "2026-08-14T02:00:00Z".into(),
                    updated_at: "2026-08-14T02:05:00Z".into(),
                    html_url: "https://github.com/o/r/issues/42#issuecomment-101".into(),
                }]),
                fail_fetch: false,
                fail_post: false,
                collab: Some(true),
                fail_collab: false,
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn set_fail_fetch(&mut self) {
            self.fail_fetch = true;
        }
        fn set_fail_post(&mut self) {
            self.fail_post = true;
        }
        fn set_collab(&mut self, v: bool) {
            self.collab = Some(v);
        }
        fn set_collab_denied(&mut self) {
            self.collab = None; // 模拟 token 缺 Administration: Read
        }
        fn set_fail_collab(&mut self) {
            self.fail_collab = true;
        }
    }
    #[async_trait]
    impl crate::github::GithubApi for MockGithub {
        async fn fetch_issue(
            &self,
            owner: &str,
            repo: &str,
            number: u64,
        ) -> anyhow::Result<crate::github::GhIssue> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("fetch:{owner}/{repo}/{number}"));
            if self.fail_fetch {
                anyhow::bail!("模拟拉取失败");
            }
            Ok(self.issue.lock().unwrap().clone())
        }
        async fn list_comments(
            &self,
            owner: &str,
            repo: &str,
            number: u64,
        ) -> anyhow::Result<Vec<crate::github::GhComment>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("comments:{owner}/{repo}/{number}"));
            Ok(self.comments.lock().unwrap().clone())
        }
        async fn post_comment(
            &self,
            owner: &str,
            repo: &str,
            number: u64,
            body: &str,
        ) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!(
                "post:{owner}/{repo}/{number}:{}",
                crate::agent::truncate(body, 30)
            ));
            if self.fail_post {
                anyhow::bail!("模拟回写失败");
            }
            Ok(())
        }
        async fn close_issue(&self, owner: &str, repo: &str, number: u64) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("close:{owner}/{repo}/{number}"));
            Ok(())
        }
        async fn create_issue(
            &self,
            owner: &str,
            repo: &str,
            title: &str,
        ) -> anyhow::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create:{owner}/{repo}:{title}"));
            Ok(format!("https://github.com/{owner}/{repo}/issues/1"))
        }
        async fn list_issues_since(
            &self,
            owner: &str,
            repo: &str,
            since: &str,
        ) -> anyhow::Result<Vec<crate::github::GhIssue>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("list:{owner}/{repo}:{since}"));
            Ok(Vec::new())
        }
        async fn list_comments_since(
            &self,
            owner: &str,
            repo: &str,
            since: &str,
        ) -> anyhow::Result<Vec<crate::github::GhComment>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("comments:{owner}/{repo}:{since}"));
            Ok(self.comments.lock().unwrap().clone())
        }
        async fn is_collaborator(
            &self,
            owner: &str,
            repo: &str,
            login: &str,
        ) -> anyhow::Result<Option<bool>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("collab:{owner}/{repo}:{login}"));
            if self.fail_collab {
                anyhow::bail!("模拟协作者校验失败");
            }
            Ok(self.collab)
        }
    }

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
        fn sent_chats(&self) -> Vec<(String, String)> {
            self.sent_chats.lock().unwrap().clone()
        }
        fn set_fail_chat(&self, chat: &str) {
            *self.fail_chat.lock().unwrap() = Some(chat.to_string());
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
            }
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
            progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
            cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
        ) -> Result<agent::RunOutcome, String> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.started.notify_one();
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
                MockOutcome::Reply => Ok(agent::RunOutcome::Reply {
                    reply: self.reply.clone(),
                    session_id: session_id.to_string(),
                }),
                MockOutcome::Cancel => Ok(agent::RunOutcome::Cancelled),
                MockOutcome::Fail(e) => Err(e.clone()),
            }
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
        build_test_bridge_with_bot_gh(runner, bot, Arc::new(MockGithub::new()))
    }

    /// 同 build_test_bridge_with_bot，但可注入 MockGithub（github 指令门测试用）。
    fn build_test_bridge_with_bot_gh(
        runner: Arc<dyn AgentRunner>,
        bot: BotConfig,
        gh: Arc<dyn crate::github::GithubApi>,
    ) -> (Arc<Bridge>, Arc<MockMessenger>) {
        let msgr = Arc::new(MockMessenger::new());
        let bridge = Arc::new(Bridge::build(
            msgr.clone(),
            bot,
            &Config::default(),
            runner,
            gh,
        ));
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
            Arc::new(MockGithub::new()),
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
            Arc::new(MockGithub::new()),
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
            Arc::new(MockGithub::new()),
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

    /// GitHub 指令门：关闭是破坏性操作——裸「关闭」只回确认引导（不调 API），
    /// 「确认关闭」才真正执行（不进 agent、不进 pending），回执 ✅。
    #[tokio::test]
    async fn github_close_requires_confirmation_then_closes() {
        let runner = Arc::new(MockAgentRunner::blocking("不会用到"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "o/r".into(),
            ..Default::default()
        };
        // 第一句：裸「关闭」→ 确认引导，零 API 调用
        let gh = Arc::new(MockGithub::new());
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot.clone(), gh.clone());
        let b1 = bridge.clone();
        let task = tokio::spawn(async move {
            b1.handle(test_ev(
                "m1",
                "oc_gh",
                "关闭 https://github.com/o/r/issues/7",
            ))
            .await
        });
        task.await.unwrap();
        assert!(gh.calls().is_empty(), "裸关闭不得调 API");
        assert!(msgr.sent()[0].contains("破坏性操作"));
        assert!(msgr.sent()[0].contains("确认关闭"));
        cleanup_bridge(&bridge);

        // 第二句：「确认关闭」→ 真正执行
        let gh2 = Arc::new(MockGithub::new());
        let (bridge2, msgr2) = build_test_bridge_with_bot_gh(runner.clone(), bot, gh2.clone());
        let b2 = bridge2.clone();
        let task = tokio::spawn(async move {
            b2.handle(test_ev(
                "m1",
                "oc_gh",
                "确认关闭 https://github.com/o/r/issues/7",
            ))
            .await
        });
        task.await.unwrap();
        assert_eq!(gh2.calls(), vec!["close:o/r/7"]);
        assert_eq!(msgr2.sent(), vec!["✅ 已关闭 o/r#7。"]);
        assert!(runner.prompts().is_empty(), "关闭不进 agent");
        cleanup_bridge(&bridge2);
    }

    /// GitHub 指令门：创建是公开写操作——裸「建 issue」只回预览确认（不调 API），
    /// 「确认建 issue」才真正创建；白名单单项可省略仓库，多项省略 → 明确拒绝。
    #[tokio::test]
    async fn github_create_requires_confirmation_then_creates() {
        let runner = Arc::new(MockAgentRunner::blocking("不会用到"));
        // 第一句：裸「建 issue」→ 预览引导，零 API 调用
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "o/r".into(),
            ..Default::default()
        };
        let gh = Arc::new(MockGithub::new());
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot.clone(), gh.clone());
        let b1 = bridge.clone();
        let task = tokio::spawn(async move {
            b1.handle(test_ev("m1", "oc_gh2", "建 issue 修复登录 401"))
                .await
        });
        task.await.unwrap();
        assert!(gh.calls().is_empty(), "裸建 issue 不得调 API");
        assert!(msgr.sent()[0].contains("将创建 issue「修复登录 401」"));
        assert!(msgr.sent()[0].contains("确认建 issue o/r 修复登录 401"));
        cleanup_bridge(&bridge);

        // 第二句：「确认建 issue」→ 真正创建
        let gh2 = Arc::new(MockGithub::new());
        let (bridge2, msgr2) =
            build_test_bridge_with_bot_gh(runner.clone(), bot.clone(), gh2.clone());
        let b2 = bridge2.clone();
        let task = tokio::spawn(async move {
            b2.handle(test_ev("m1", "oc_gh2", "确认建 issue 修复登录 401"))
                .await
        });
        task.await.unwrap();
        assert_eq!(gh2.calls(), vec!["create:o/r:修复登录 401"]);
        assert_eq!(
            msgr2.sent(),
            vec!["✅ 已创建：https://github.com/o/r/issues/1"]
        );
        cleanup_bridge(&bridge2);

        // 多项白名单 + 省略仓库 → 预览阶段就拒绝并提示带仓库
        let bot3 = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "o/a, o/b".into(),
            ..Default::default()
        };
        let gh3 = Arc::new(MockGithub::new());
        let (bridge3, msgr3) = build_test_bridge_with_bot_gh(runner.clone(), bot3, gh3.clone());
        let b3 = bridge3.clone();
        let task = tokio::spawn(async move {
            b3.handle(test_ev("m1", "oc_gh3", "建 issue 修复登录 401"))
                .await
        });
        task.await.unwrap();
        assert!(gh3.calls().is_empty(), "未调 API");
        assert!(msgr3.sent()[0].contains("请带上仓库"));
        cleanup_bridge(&bridge3);
    }

    /// GitHub 指令门：仓库不在白名单 → 拒绝回复，零 API 调用。
    #[tokio::test]
    async fn github_whitelist_rejected() {
        let runner = Arc::new(MockAgentRunner::blocking("不会用到"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "o/r".into(),
            ..Default::default()
        };
        let gh = Arc::new(MockGithub::new());
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot, gh.clone());
        let b1 = bridge.clone();
        let task = tokio::spawn(async move {
            b1.handle(test_ev(
                "m1",
                "oc_gh4",
                "分析 https://github.com/x/y/issues/1",
            ))
            .await
        });
        task.await.unwrap();
        assert!(gh.calls().is_empty(), "白名单拒绝不得碰 API");
        assert!(msgr.sent()[0].contains("不在白名单"));
        assert!(runner.prompts().is_empty());
        cleanup_bridge(&bridge);
    }

    /// GitHub 分析：issue 上下文注入 prompt，agent 回复双写（评论留档 + 群摘要）。
    #[tokio::test]
    async fn github_analyze_injects_context_and_double_writes() {
        let runner = Arc::new(MockAgentRunner::immediate("根因是 token 缓存竞态。"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "o/r".into(),
            ..Default::default()
        };
        let gh = Arc::new(MockGithub::new());
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot, gh.clone());
        let b1 = bridge.clone();
        let task = tokio::spawn(async move {
            b1.handle(test_ev(
                "m1",
                "oc_gh5",
                "分析 https://github.com/o/r/issues/42",
            ))
            .await
        });
        task.await.unwrap();
        // issue + 评论 拉取，回复全文回写评论
        let calls = gh.calls();
        assert!(
            calls.iter().any(|c| c.starts_with("fetch:o/r/42")),
            "calls={calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.starts_with("comments:o/r/42")),
            "calls={calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.starts_with("post:o/r/42:")),
            "calls={calls:?}"
        );
        // prompt 注入 issue 内容
        let p = runner.prompts().join("\n");
        assert!(p.contains("[GitHub Issue]"), "prompt 应含注入段");
        assert!(p.contains("登录偶发 401"));
        assert!(p.contains("token 缓存竞态导致偶发 401。"));
        assert!(p.contains("复现了，见日志。"));
        // 群里只有截断摘要（≤200 字 + 📝 前缀），全文在 issue 评论
        assert_eq!(msgr.sent().len(), 1);
        assert!(msgr.sent()[0].starts_with("📝 已分析 o/r#42「登录偶发 401」"));
        assert!(msgr.sent()[0].contains("根因是 token 缓存竞态。"));
        assert!(msgr.sent()[0].chars().count() < 300);
        cleanup_bridge(&bridge);
    }

    /// GitHub 分析：拉取失败 → 回执 ❌，不进 agent。
    #[tokio::test]
    async fn github_analyze_fetch_error_replies() {
        let runner = Arc::new(MockAgentRunner::blocking("不会用到"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "o/r".into(),
            ..Default::default()
        };
        let mut gh = MockGithub::new();
        gh.set_fail_fetch();
        let gh = Arc::new(gh);
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot, gh.clone());
        let b1 = bridge.clone();
        let task = tokio::spawn(async move {
            b1.handle(test_ev(
                "m1",
                "oc_gh6",
                "分析 https://github.com/o/r/issues/42",
            ))
            .await
        });
        task.await.unwrap();
        assert!(msgr.sent()[0].contains("拉取 issue 失败"));
        assert!(runner.prompts().is_empty(), "拉取失败不进 agent");
        cleanup_bridge(&bridge);
    }

    /// GitHub 分析：评论回写失败 → 摘要如实提示「留档失败」，不假装已留档。
    #[tokio::test]
    async fn github_analyze_post_failure_receipt_is_honest() {
        let runner = Arc::new(MockAgentRunner::immediate("根因是 token 缓存竞态。"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "o/r".into(),
            ..Default::default()
        };
        let mut gh = MockGithub::new();
        gh.set_fail_post();
        let gh = Arc::new(gh);
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot, gh.clone());
        let b1 = bridge.clone();
        let task = tokio::spawn(async move {
            b1.handle(test_ev(
                "m1",
                "oc_gh8",
                "分析 https://github.com/o/r/issues/42",
            ))
            .await
        });
        task.await.unwrap();
        assert_eq!(msgr.sent().len(), 1);
        assert!(
            msgr.sent()[0].contains("留档失败"),
            "摘要应提示留档失败: {}",
            msgr.sent()[0]
        );
        assert!(
            !msgr.sent()[0].contains("已留档到"),
            "不得谎称已留档: {}",
            msgr.sent()[0]
        );
        cleanup_bridge(&bridge);
    }

    /// 评审 S1：空白名单 = 全放行只适用于读（分析）；写操作（关闭/建）未配置白名单时
    /// 直接拒绝，且零 API 调用；分析维持放行。
    #[tokio::test]
    async fn github_empty_whitelist_blocks_writes_allows_analyze() {
        let runner = Arc::new(MockAgentRunner::blocking("不会用到"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "".into(), // 空白名单
            ..Default::default()
        };
        let gh = Arc::new(MockGithub::new());
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot.clone(), gh.clone());
        let b1 = bridge.clone();
        let task = tokio::spawn(async move {
            b1.handle(test_ev(
                "m1",
                "oc_gh9",
                "确认关闭 https://github.com/o/r/issues/7",
            ))
            .await
        });
        task.await.unwrap();
        assert!(gh.calls().is_empty(), "空名单写操作零 API 调用");
        assert!(msgr.sent()[0].contains("未配置仓库白名单"));
        cleanup_bridge(&bridge);

        // 建 issue 同样拒绝
        let gh2 = Arc::new(MockGithub::new());
        let (bridge2, msgr2) =
            build_test_bridge_with_bot_gh(runner.clone(), bot.clone(), gh2.clone());
        let b2 = bridge2.clone();
        let task = tokio::spawn(async move {
            b2.handle(test_ev("m1", "oc_gh9", "确认建 issue 修复 bug"))
                .await
        });
        task.await.unwrap();
        assert!(gh2.calls().is_empty());
        assert!(msgr2.sent()[0].contains("未配置仓库白名单"));
        cleanup_bridge(&bridge2);

        // 分析（读）维持放行：正常注入 + 双写（空名单 = 全放行，读不设限）
        let runner2 = Arc::new(MockAgentRunner::immediate("根因分析。"));
        let gh3 = Arc::new(MockGithub::new());
        let (bridge3, msgr3) = build_test_bridge_with_bot_gh(runner2.clone(), bot, gh3.clone());
        let b3 = bridge3.clone();
        let task = tokio::spawn(async move {
            b3.handle(test_ev(
                "m1",
                "oc_gh9",
                "分析 https://github.com/o/r/issues/42",
            ))
            .await
        });
        task.await.unwrap();
        assert!(gh3.calls().iter().any(|c| c.starts_with("fetch:o/r/42")));
        assert_eq!(msgr3.sent().len(), 1);
        cleanup_bridge(&bridge3);
    }

    /// 未配置 github 能力：同样文本走普通 agent 流程（不注入、全文一次发送）。
    #[tokio::test]
    async fn github_not_capable_passthrough() {
        let runner = Arc::new(MockAgentRunner::immediate("这是普通回复"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        };
        let gh = Arc::new(MockGithub::new());
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot, gh.clone());
        let b1 = bridge.clone();
        let task = tokio::spawn(async move {
            b1.handle(test_ev(
                "m1",
                "oc_gh7",
                "分析 https://github.com/o/r/issues/42",
            ))
            .await
        });
        task.await.unwrap();
        assert!(gh.calls().is_empty(), "未配置能力不碰 GitHub API");
        assert_eq!(msgr.sent(), vec!["这是普通回复"]);
        let p = runner.prompts().join("\n");
        assert!(!p.contains("[GitHub Issue]"), "未配置不注入");
        cleanup_bridge(&bridge);
    }

    /// 评论批处理：@提及 私信通知（映射内 login → 对应 chat_id；无映射静默跳过；
    /// bot login 不私信；失败评论进 failed 游标回退）。
    #[tokio::test]
    async fn comment_batch_mentions_dm_targets() {
        let msgr = Arc::new(MockMessenger::new());
        let mk = |id: u64, body: &str, login: &str| crate::github::GhComment {
            id,
            body: body.into(),
            user: crate::github::GhUser {
                login: login.into(),
            },
            created_at: "2026-08-14T02:00:00Z".into(),
            updated_at: "2026-08-14T02:05:00Z".into(),
            html_url: "https://github.com/o/r/issues/42#issuecomment-1".into(),
        };
        let comments = vec![
            // 映射内 login → 私信；@bot 被 exclude（bot_login）排除
            mk(1, "@alice 看看这个 @bot", "bob"),
            // 无映射 login → 静默跳过
            mk(2, "@nobody 你好", "bob"),
            // 引用行内的 @ 不算
            mk(3, "> @alice 旧讨论\n新讨论", "carol"),
            // 围栏代码块内的 @ 不算（评审 M4）
            mk(
                5,
                "```rust\nlet x = \"@alice\";\n```\n@alice 真提及",
                "dave",
            ),
            // 已在 seen → 跳过
            mk(4, "@alice 已处理过", "bob"),
        ];
        let gh = Arc::new(MockGithub::new());
        let batch = crate::service::process_comment_batch(
            gh.as_ref(),
            msgr.as_ref(),
            &comments,
            &[4],
            &[("alice".to_string(), "oc_alice".to_string())],
            "bot",
            "o/r",
            "o",
            "oc_gh",
        )
        .await;
        // 私信目标断言：@alice 的评论 1 与 5（代码块内不算，但块后真提及算）发到 oc_alice；
        // 评论 1 的 @bot 被 exclude 排除
        let sent = msgr.sent_chats();
        assert_eq!(sent.len(), 2, "评论 1 和 5 触发私信: {sent:?}");
        assert!(sent.iter().all(|(c, _)| c == "oc_alice"));
        assert!(sent[0].1.contains("你在 o/r#42 被 @bob 提到了"));
        assert!(sent[1].1.contains("你在 o/r#42 被 @dave 提到了"));
        // seen 推进：1/2/3/5 成功，4 在 seen 里跳过
        assert_eq!(batch.seen_extra, vec![1, 2, 3, 5]);
        assert!(batch.failed.is_empty());
        assert_eq!(batch.new_since.as_deref(), Some("2026-08-14T02:05:00Z"));
    }

    /// 评审 C1：空批 → new_since=None（调用方保持原游标，不得写空串清掉）。
    #[tokio::test]
    async fn comment_batch_empty_keeps_cursor() {
        let msgr = Arc::new(MockMessenger::new());
        let gh = Arc::new(MockGithub::new());
        let batch = crate::service::process_comment_batch(
            gh.as_ref(),
            msgr.as_ref(),
            &[],
            &[],
            &[],
            "bot",
            "o/r",
            "o",
            "oc_gh",
        )
        .await;
        assert!(batch.seen_extra.is_empty());
        assert!(batch.failed.is_empty());
        assert_eq!(batch.new_since, None, "空批必须保持原游标");
    }

    /// 评审 M2：私信失败 → 该评论不进 seen、进 failed（游标回退重试）。
    #[tokio::test]
    async fn comment_batch_dm_failure_rewinds() {
        let msgr = Arc::new(MockMessenger::new());
        msgr.set_fail_chat("oc_alice"); // 对 alice 的私信失败
        let mk = |id: u64, body: &str, login: &str| crate::github::GhComment {
            id,
            body: body.into(),
            user: crate::github::GhUser {
                login: login.into(),
            },
            created_at: "2026-08-14T02:00:00Z".into(),
            updated_at: format!("2026-08-14T0{id}:05:00Z"),
            html_url: "https://github.com/o/r/issues/42#issuecomment-1".into(),
        };
        let comments = vec![
            mk(1, "@alice 失败", "bob"),    // updated T01:05 → failed
            mk(2, "@carol 成功", "bob"),    // updated 02:02:05 → seen
            mk(3, "@alice 再失败", "dave"), // updated 02:03:05 → failed
        ];
        let gh = Arc::new(MockGithub::new());
        let batch = crate::service::process_comment_batch(
            gh.as_ref(),
            msgr.as_ref(),
            &comments,
            &[],
            &[
                ("alice".to_string(), "oc_alice".to_string()),
                ("carol".to_string(), "oc_carol".to_string()),
            ],
            "bot",
            "o/r",
            "o",
            "oc_gh",
        )
        .await;
        assert_eq!(batch.seen_extra, vec![2], "成功评论进 seen");
        // 回退到最早失败评论的 updated_at（评审：取 min 而非最后失败者）；
        // failed 携带评论 id（供失败计数，评审 M2）
        assert_eq!(
            batch.failed,
            vec![
                (1, "2026-08-14T01:05:00Z".to_string()),
                (3, "2026-08-14T03:05:00Z".to_string())
            ]
        );
        assert_eq!(batch.new_since.as_deref(), Some("2026-08-14T03:05:00Z"));
    }
    /// 2.2 触发判定：协作者评论 @bot → triggers；非协作者 → 跳过；PR 评论 → 留 2.3；
    /// 作者 == bot（回声）→ 跳过。
    #[tokio::test]
    async fn comment_batch_trigger_and_collaborator_gate() {
        let mk = |id: u64, body: &str, login: &str, url: &str| crate::github::GhComment {
            id,
            body: body.into(),
            user: crate::github::GhUser {
                login: login.into(),
            },
            created_at: "2026-08-14T02:00:00Z".into(),
            updated_at: format!("2026-08-14T02:{id:02}:00Z"),
            html_url: url.into(),
        };
        let issue_url = "https://github.com/o/r/issues/42#issuecomment-1";
        let pr_url = "https://github.com/o/r/pull/5#issuecomment-1";
        let comments = vec![
            mk(1, "@bot 分析下这个", "alice", issue_url), // 协作者（默认）→ 触发
            mk(2, "@BOT 再看看", "bob", issue_url),       // 大小写不敏感
            mk(3, "@bot 分析", "bot", issue_url),         // 作者回声 → 不触发
            mk(4, "@bot 审查下 PR", "alice", pr_url),     // PR 评论 → 2.2 跳过
            mk(5, "xxbot 分析", "alice", issue_url),      // 词位不符 → 不触发
        ];
        let msgr = Arc::new(MockMessenger::new());
        let gh = Arc::new(MockGithub::new());
        let batch = crate::service::process_comment_batch(
            gh.as_ref(),
            msgr.as_ref(),
            &comments,
            &[],
            &[],
            "bot",
            "o/r",
            "o",
            "oc_gh",
        )
        .await;
        assert_eq!(
            batch.triggers,
            vec![(42, 1), (42, 2)],
            "评论 1/2 触发，3/4/5 不触发"
        );

        // 非协作者 → 跳过
        let mut gh2 = MockGithub::new();
        gh2.set_collab(false);
        let gh2 = Arc::new(gh2);
        let batch2 = crate::service::process_comment_batch(
            gh2.as_ref(),
            msgr.as_ref(),
            &comments[..1],
            &[],
            &[],
            "bot",
            "o/r",
            "o",
            "oc_gh",
        )
        .await;
        assert!(batch2.triggers.is_empty(), "非协作者不触发");
        assert_eq!(batch2.seen_extra, vec![1], "仍算处理过（不重试）");
    }

    /// 2.2 协作者校验失败 → 评论进 failed（游标回退重试）。
    #[tokio::test]
    async fn comment_batch_collaborator_error_rewinds() {
        let mk = |id: u64, body: &str, login: &str| crate::github::GhComment {
            id,
            body: body.into(),
            user: crate::github::GhUser {
                login: login.into(),
            },
            created_at: "2026-08-14T02:00:00Z".into(),
            updated_at: format!("2026-08-14T02:{id:02}:00Z"),
            html_url: "https://github.com/o/r/issues/42#issuecomment-1".into(),
        };
        let comments = vec![mk(1, "@bot 分析", "alice"), mk(2, "普通讨论", "bob")];
        let msgr = Arc::new(MockMessenger::new());
        let mut gh = MockGithub::new();
        gh.set_fail_collab();
        let gh = Arc::new(gh);
        let batch = crate::service::process_comment_batch(
            gh.as_ref(),
            msgr.as_ref(),
            &comments,
            &[],
            &[],
            "bot",
            "o/r",
            "o",
            "oc_gh",
        )
        .await;
        assert!(batch.triggers.is_empty());
        assert_eq!(batch.seen_extra, vec![2], "普通评论照常进 seen");
        assert_eq!(
            batch.failed,
            vec![(1, "2026-08-14T02:01:00Z".to_string())],
            "校验失败评论回退"
        );
    }

    /// 2.2 合成 Ev 复用 handle()：同 mid 两次 handle 只 post 一次（去重）。
    #[tokio::test]
    async fn auto_process_reuses_handle_and_dedupe_mid() {
        let runner = Arc::new(MockAgentRunner::immediate("根因是 token 缓存竞态。"));
        let bot = BotConfig {
            name: format!("abb-test-{}", uuid::Uuid::new_v4()),
            gh_token: "ghp_x".into(),
            gh_repos: "o/r".into(),
            gh_notify_chat: "oc_gh".into(),
            ..Default::default()
        };
        let gh = Arc::new(MockGithub::new());
        let (bridge, msgr) = build_test_bridge_with_bot_gh(runner.clone(), bot, gh.clone());
        let ev = |mid: &str| crate::bridge::Ev {
            mid: mid.into(),
            chat_id: "oc_gh".into(),
            chat_type: "group".into(),
            thread_id: String::new(),
            quoted: Default::default(),
            text: "分析 https://github.com/o/r/issues/42".into(),
            attachments: Vec::new(),
        };
        // 同一评论 id 的合成 Ev 触发两次 → mid 去重只处理一次
        let b1 = bridge.clone();
        let t1 = tokio::spawn(async move { b1.handle(ev("gh:o/r:42:1")).await });
        t1.await.unwrap();
        let b2 = bridge.clone();
        let t2 = tokio::spawn(async move { b2.handle(ev("gh:o/r:42:1")).await });
        t2.await.unwrap();
        let posts = gh
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("post:"))
            .count();
        assert_eq!(posts, 1, "同 mid 去重，只回写一次");
        // 群摘要恰一条
        assert_eq!(msgr.sent().len(), 1);
        assert!(msgr.sent()[0].starts_with("📝 已分析 o/r#42"));
        // prompt 注入 issue 内容（不可信包裹）
        let p = runner.prompts().join("\n");
        assert!(p.contains("[GitHub Issue]"));
        assert!(p.contains("不可信数据"));
        cleanup_bridge(&bridge);
    }
    /// 评审 I1：仅配置提及映射（不配通知群）时 @bot 触发不收集（合成 Ev chat_id 空会被
    /// handle 丢弃），日志说明而非静默丢失。
    #[tokio::test]
    async fn comment_batch_no_notify_chat_skips_triggers() {
        let mk = |id: u64, body: &str, login: &str| crate::github::GhComment {
            id,
            body: body.into(),
            user: crate::github::GhUser {
                login: login.into(),
            },
            created_at: "2026-08-14T02:00:00Z".into(),
            updated_at: "2026-08-14T02:05:00Z".into(),
            html_url: "https://github.com/o/r/issues/42#issuecomment-1".into(),
        };
        let comments = vec![mk(1, "@bot 分析下", "alice")];
        let msgr = Arc::new(MockMessenger::new());
        let gh = Arc::new(MockGithub::new());
        let batch = crate::service::process_comment_batch(
            gh.as_ref(),
            msgr.as_ref(),
            &comments,
            &[],
            &[],
            "bot",
            "o/r",
            "o",
            "", // notify_chat 空 = 仅映射配置
        )
        .await;
        assert!(batch.triggers.is_empty(), "无通知群不收集触发");
        assert_eq!(batch.seen_extra, vec![1], "评论仍算处理过");
    }

    /// 评审 M2：协作者检查权限不足（token 缺 Administration: Read）→ 跳过不重试（不进 failed）。
    #[tokio::test]
    async fn comment_batch_collab_denied_skips_without_rewind() {
        let mk = |id: u64, body: &str, login: &str| crate::github::GhComment {
            id,
            body: body.into(),
            user: crate::github::GhUser {
                login: login.into(),
            },
            created_at: "2026-08-14T02:00:00Z".into(),
            updated_at: "2026-08-14T02:05:00Z".into(),
            html_url: "https://github.com/o/r/issues/42#issuecomment-1".into(),
        };
        let comments = vec![mk(1, "@bot 分析", "alice")];
        let msgr = Arc::new(MockMessenger::new());
        let mut gh = MockGithub::new();
        gh.set_collab_denied();
        let gh = Arc::new(gh);
        let batch = crate::service::process_comment_batch(
            gh.as_ref(),
            msgr.as_ref(),
            &comments,
            &[],
            &[],
            "bot",
            "o/r",
            "o",
            "oc_gh",
        )
        .await;
        assert!(batch.triggers.is_empty());
        assert!(batch.failed.is_empty(), "权限不足不重试");
        assert_eq!(batch.seen_extra, vec![1]);
    }

    /// auto_ev 合成事件构造（评审 M3）：mid/chat_type/thread_id/text 字段契约。
    #[test]
    fn auto_ev_shape() {
        let ev = crate::service::auto_ev("o/r", 42, 7, "oc_gh");
        assert_eq!(ev.mid, "gh:o/r:42:7");
        assert_eq!(ev.chat_id, "oc_gh");
        assert_eq!(ev.chat_type, "group"); // 跳过 save_primary_chat
        assert!(ev.thread_id.is_empty(), "thread_id 必须空（防假话题回复）");
        assert_eq!(ev.text, "分析 https://github.com/o/r/issues/42");
    }
}
