//! --service 入口：无头桥守护进程（纯 tokio，无 GUI）。
//! 组装 config → messenger（按 bot.kind：飞书 WS / 微信长轮询）→ bridge → 事件循环。
//! 处理 SIGTERM/SIGINT 优雅退出。

use crate::bridge::Bridge;
use crate::config::Config;
use crate::messenger;
use std::sync::Arc;
use tokio::sync::watch;

pub async fn run() {
    crate::log!("=== ABB 启动（Rust 内置 WS 版 · 多 bot）===");
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            crate::log!("[service] config.json 读取失败: {e:#}");
            // 配置不可用 → 清「期望运行」标记，看门狗停止自动重拉（否则每 2s 拉起即退死循环）
            crate::install::set_desired(false);
            std::process::exit(1);
        }
    };
    let missing = cfg.missing();
    if !missing.is_empty() {
        crate::log!(
            "[service] ⚠️ config.json 缺少必填项: {}（{}）。请填好后重启。",
            missing.join(", "),
            Config::path().display()
        );
        // 配置不可用 → 清「期望运行」标记，看门狗停止自动重拉
        crate::install::set_desired(false);
        std::process::exit(1);
    }
    crate::log!(
        "[service] 只响应: {}  默认后端: {}  bot数: {}",
        cfg.owner_open_id,
        cfg.default_backend,
        cfg.bots.len()
    );

    // 启动时以「本进程身份（launchd 责任进程）」实测六项系统权限，打日志。
    // 诊断价值：终端里跑 --dump-perms 会向上追踪责任进程到 Terminal，看到的是终端的授权
    // 不是本服务的；只有从服务进程内部查才是真实状态。
    for p in crate::deps::detect_permissions() {
        crate::log!("[perm] 检测 {}: {:?}", p.id, p.state);
    }

    let cfg = Arc::new(cfg);

    // 接入飞书 bot → 后台自动装 lark-cli + lark-* 技能（幂等/best-effort，绝不阻塞 bot 启动）。
    // fire-and-forget：装不上只 log 警告，不影响本进程。只对飞书 bot 触发（微信/钉钉不需要）。
    if cfg.bots.iter().any(|b| b.enabled && b.kind == "feishu") {
        tokio::spawn(async { crate::larkskills::ensure_lark_setup().await });
    }

    // stop 信号通道：SIGTERM/SIGINT → true（所有 bot 共享）
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).ok();
            let mut int = signal(SignalKind::interrupt()).ok();
            tokio::select! {
                _ = async { if let Some(t)=term.as_mut(){t.recv().await} else {std::future::pending().await} } => {}
                _ = async { if let Some(t)=int.as_mut(){t.recv().await} else {std::future::pending().await} } => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            // Windows 无 SIGTERM/SIGINT 语义：Ctrl+C/关闭控制台即优雅退出
            let _ = tokio::signal::ctrl_c().await;
        }
        crate::log!("[service] 收到退出信号");
        let _ = stop_tx.send(true);
    });

    // 每个 bot 一个事件循环 + Bridge + 定时调度，并行跑。
    // 第一遍先构建所有 Messenger 并登记到跨会话投递路由表（#21）：投递循环要能看到
    // 全部已启用目标；Bridge 与路由表共享同一 Arc<dyn Messenger> 实例。
    let mut messengers: std::collections::HashMap<
        String,
        std::sync::Arc<dyn crate::messenger::Messenger>,
    > = std::collections::HashMap::new();
    let mut bot_cfgs: std::collections::HashMap<String, crate::config::BotConfig> =
        std::collections::HashMap::new();
    let mut ready: Vec<(
        crate::config::BotConfig,
        std::sync::Arc<dyn crate::messenger::Messenger>,
    )> = Vec::new();
    for bot in &cfg.bots {
        // 用户停用的 bot：不启动，但上报「已停用」让托盘显灰，并保留在设置窗可重启用。
        if !bot.enabled {
            crate::log!("[service] 跳过已停用的 bot「{}」", bot.key());
            crate::botstatus::report(&bot.key(), &bot.kind, &bot.bot_name, "已停用");
            continue;
        }
        // 跳过凭证不齐的（与 Config::missing 同一事实源）
        if !bot.credentials_ready() {
            crate::log!(
                "[service] 跳过未就绪的 bot「{}」（kind={}，缺凭证/登录）",
                bot.key(),
                bot.kind
            );
            continue;
        }
        let msgr = match messenger::build(bot) {
            Ok(m) => m,
            Err(e) => {
                crate::log!("[bot:{}] 构造 messenger 失败: {e:#}", bot.key());
                continue;
            }
        };
        messengers.insert(bot.key(), msgr.clone());
        bot_cfgs.insert(bot.key(), bot.clone());
        ready.push((bot.clone(), msgr));
    }
    if ready.is_empty() {
        crate::log!("[service] 没有可用 bot，退出");
        // 无可跑 bot（全部停用/凭证不齐）→ 清「期望运行」标记，看门狗停止自动重拉
        crate::install::set_desired(false);
        std::process::exit(1);
    }
    let router = std::sync::Arc::new(crate::deliver::Router::new(
        cfg.cross_delivery_enabled,
        messengers,
        bot_cfgs,
        None,
    ));
    // 跨会话投递消费循环（独立于 bot 循环，共享 stop）
    {
        let router = router.clone();
        let mut stop = stop_rx.clone();
        tokio::spawn(async move {
            deliver_loop(router, &mut stop).await;
        });
    }
    let mut handles = Vec::new();
    for (bot, msgr) in ready {
        let cfg = cfg.clone();
        let stop = stop_rx.clone();
        let router = router.clone();
        handles.push(tokio::spawn(async move {
            run_bot(bot, cfg, msgr, router, stop).await;
        }));
    }
    // 等所有 bot 循环结束（正常只有 stop 才会结束）
    for h in handles {
        let _ = h.await;
    }
    crate::log!("[service] 已退出");
}

/// 跨会话投递消费循环：轮询 deliveries.json（agent 的 deliver CLI 落盘），逐条经路由表投递。
/// 失败项不回盘（避免死循环重投），由 Router 负责回源报错 / 微信 outbox 兜底。
async fn deliver_loop(
    router: std::sync::Arc<crate::deliver::Router>,
    stop_rx: &mut watch::Receiver<bool>,
) {
    crate::log!(
        "[deliver] 投递循环启动（跨会话投递开关={}）",
        router.enabled
    );
    let store = crate::deliver::DeliveryStore::new();
    loop {
        if interruptible_sleep(std::time::Duration::from_secs(1), stop_rx).await {
            break;
        }
        // at-least-once：先取（不移除）→ 投递（成功或已兜底）→ ack。
        // 投递中崩溃/退出 → 未 ack 的项下次启动重投，Router 防循环去重兜底。
        let items = store.pending();
        for item in items {
            router.deliver(&item).await;
            store.ack(&[item.id]);
        }
    }
    crate::log!("[deliver] 投递循环退出");
}

/// 单个 bot 的全套：事件循环（飞书 WS / 微信长轮询）+ 定时任务调度循环。
/// msgr 由 run() 构建并登记进跨会话投递路由表后传入（Bridge 与路由表共享同一实例）。
async fn run_bot(
    bot: crate::config::BotConfig,
    cfg: Arc<Config>,
    msgr: std::sync::Arc<dyn crate::messenger::Messenger>,
    router: std::sync::Arc<crate::deliver::Router>,
    stop_rx: watch::Receiver<bool>,
) {
    let key = bot.key();
    crate::log!(
        "[bot:{key}] 启动（kind={} name={}）",
        bot.kind,
        bot.bot_name
    );
    let bridge = Arc::new(Bridge::new(msgr, bot.clone(), &cfg));

    // 连接态初始上报：进入事件循环前是「连接中」；之后由各事件循环在状态迁移时更新。
    // （别用独立心跳任务只报「活着」——那测的是上报线程不是通道连通，会话死了托盘还在线。）
    crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "连接中");

    // #25 重启恢复：上次崩溃/退出时未处理完的消息 → 自动续跑（异步进行，不阻塞事件循环
    // 启动；per-chat 串行锁保证重放与实时消息不乱序）。
    {
        let bridge = bridge.clone();
        tokio::spawn(async move {
            bridge.recover_pending().await;
        });
    }

    // 定时任务调度循环（独立于事件循环，共享 stop）
    {
        let bridge = bridge.clone();
        let key = key.clone();
        let mut stop = stop_rx.clone();
        tokio::spawn(async move {
            crate::log!("[bot:{key}] 调度循环启动");
            let mut last_min: Option<String> = None;
            // 在跑任务集合：cron 周期短于任务耗时时，跳过重叠的新一轮（防同任务并发堆积、
            // 多个 claude 抢同一资源/互相踩工作区）。
            let running = Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::<String>::new(),
            ));
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(20), &mut stop).await {
                    break;
                }
                let now = crate::schedule::DateTime::now();
                let min_key = format!(
                    "{}-{:02}-{:02} {:02}:{:02}",
                    now.year, now.month, now.day, now.hour, now.minute
                );
                if last_min.as_deref() == Some(&min_key) {
                    continue;
                }
                last_min = Some(min_key);
                let due = bridge.jobs.due_jobs(&now);
                for job in due {
                    {
                        let mut r = running.lock().unwrap();
                        if r.contains(&job.id) {
                            crate::log!(
                                "[bot:{key}] 任务 {} 上一轮还没跑完，跳过本轮触发",
                                &job.id[..job.id.len().min(8)]
                            );
                            continue;
                        }
                        r.insert(job.id.clone());
                    }
                    let bridge = bridge.clone();
                    let router = router.clone();
                    let running = running.clone();
                    let jid = job.id.clone();
                    tokio::spawn(async move {
                        run_job(bridge, router, job).await;
                        running.lock().unwrap().remove(&jid);
                    });
                }
            }
            crate::log!("[bot:{key}] 调度循环退出");
        });
    }

    // GitHub watch 循环（附挂在既有 IM bot 上的能力，非新 bot kind）：
    // 新 issue 只通知、不自动处理（自动分析留 Phase 2）；通知走该 bot 自己的 messenger。
    // 与主事件循环并行：bot 的主循环仍是 IM 通道。
    if bot.is_github_capable() {
        let bot = bot.clone();
        let bridge = bridge.clone();
        let key = key.clone();
        let mut stop = stop_rx.clone();
        tokio::spawn(async move {
            crate::log!("[bot:{key}] GitHub watch 任务启动");
            github_watch_loop(bot, bridge, &mut stop).await;
            crate::log!("[bot:{key}] GitHub watch 任务退出");
        });
    }

    // 事件循环：按通道分派
    if bot.is_dingtalk() {
        crate::dingtalk::stream_loop(bot.app_id.clone(), bot.app_secret.clone(), bridge, stop_rx)
            .await;
        crate::botstatus::clear(&key);
        crate::log!("[bot:{key}] 钉钉 Stream 循环退出");
    } else if bot.is_wechat() {
        weixin_loop(bot, bridge, stop_rx).await; // weixin_loop 退出时已 clear
        crate::log!("[bot:{key}] 微信长轮询循环退出");
    } else {
        crate::ws::ws_loop(bot.app_id.clone(), bot.app_secret.clone(), bridge, stop_rx).await;
        crate::botstatus::clear(&key);
        crate::log!("[bot:{key}] WS 循环退出");
    }
}

/// 微信事件循环：HTTP 长轮询 getupdates（游标增量）→ bridge.on_weixin。
/// 断线/出错退避重试；errcode -14（会话超时）报「会话过期」后退出（需重新扫码）。
async fn weixin_loop(
    bot: crate::config::BotConfig,
    bridge: Arc<Bridge>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let key = bot.key();
    let base = if bot.wx_base_url.is_empty() {
        crate::wechat::FIXED_BASE_URL
    } else {
        bot.wx_base_url.as_str()
    };
    let cdn = if bot.wx_cdn_base_url.is_empty() {
        crate::wechat::DEFAULT_CDN_BASE_URL
    } else {
        bot.wx_cdn_base_url.as_str()
    };
    let client = crate::wechat::WeixinClient::new(base, &bot.wx_token, cdn);
    let mut cursor = String::new();
    let mut timeout_ms: u64 = 35_000;
    crate::log!("[bot:{key}] 微信长轮询启动 base={base}");
    // 每次 poll 成功都重报一次「在线」（绑真实连通，非独立心跳）；snapshot 僵尸阈值已放宽到 180s。
    let mut last_online = std::time::Instant::now();
    // 连续客户端超时计数：偶发超时是长轮询常态；连续 ≥3 次（≈2 分钟无任何成功响应）= 通道疑似
    // 半开假死（与 WS 看门狗同理：发得出去≠通，收得到才算通），降级「重连中」并留痕。
    let mut consec_timeouts = 0u32;
    loop {
        if *stop_rx.borrow() {
            break;
        }
        let next = client.get_updates(&cursor, timeout_ms);
        tokio::select! {
            _ = stop_rx.changed() => { break; }
            res = next => {
                match res {
                    Ok((msgs, new_cursor, new_timeout)) => {
                        if !msgs.is_empty() {
                            crate::log!("[bot:{key}] getupdates 返回 {} 条消息", msgs.len());
                        }
                        cursor = new_cursor;
                        timeout_ms = new_timeout.max(5_000);
                        consec_timeouts = 0; // 成功响应 = 通道真通，复位假死计数
                        // 每次成功轮询都视为「在线」，但至多 10s 重报一次续命（对抗僵尸过滤）
                        if last_online.elapsed() > std::time::Duration::from_secs(10) {
                            crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "在线");
                            last_online = std::time::Instant::now();
                        }
                        // 每条消息 spawn 独立处理（对齐飞书 ws_loop）：per-chat 串行由 bridge 的
                        // chat_lock 保证，而「停止词」等控制消息须能与运行中的任务**并发**处理，
                        // 若在此串行 await，长任务会把它（及其后的新消息）全部堵到跑完为止。
                        for msg in msgs {
                            let b = bridge.clone();
                            tokio::spawn(async move {
                                b.on_weixin(msg).await;
                            });
                        }
                    }
                    Err(crate::wechat::WxError::SessionExpired) => {
                        crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "会话过期");
                        crate::log!("[bot:{key}] 微信会话已超时（-14），需重新扫码登录");
                        break;
                    }
                    Err(crate::wechat::WxError::PollTimeout) => {
                        consec_timeouts += 1;
                        if consec_timeouts == 3 {
                            crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "重连中");
                            crate::log!(
                                "[bot:{key}] ⚠️ 连续 3 次长轮询超时（约 2 分钟无成功响应），疑似通道假死，持续重试中"
                            );
                        }
                        // 超时后立刻下一轮长轮询即可（服务端无新消息本就靠超时驱动），不额外 sleep
                    }
                    Err(e) => {
                        crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "重连中");
                        crate::log!("[bot:{key}] getupdates 出错，3s 后重试：{e}");
                        if interruptible_sleep(std::time::Duration::from_secs(3), &mut stop_rx).await {
                            break;
                        }
                    }
                }
            }
        }
    }
    crate::botstatus::clear(&key);
}

/// 一轮评论批处理结果：游标推进 + 自动处理触发点。
pub(crate) struct CommentBatch {
    /// 成功处理（含无提及/无映射项）的评论 id → 进 seen。
    pub seen_extra: Vec<u64>,
    /// 私信发送失败的评论 updated_at → 游标回退重试。
    pub failed: Vec<String>,
    /// 触发自动处理的 (issue 号, 评论 id)——调用方构造合成 Ev 交给 handle()。
    pub triggers: Vec<(u64, u64)>,
    /// 本轮最新评论 updated_at；**None = 空批 → 调用方必须保持原游标**
    /// （写空串会把游标清掉，下一轮走静默基线 → 窗口内评论永久丢失，评审 C1）。
    pub new_since: Option<String>,
}

/// 处理一批新评论（逻辑集中、可注入 api/msgr，可测）：@提及 私信 + @bot 触发判定。
/// - 私信侧：排除 bot 自身 login 与评论作者自提及（GitHub 自身也抑制自通知）；作者无
///   其他过滤——bot 自己的回复常引用 @login，被引用者需要知道；私信不产生 GitHub
///   活动，无回环；无映射项 → 静默跳过（映射是 opt-in 门，不群发骚扰）；
/// - 触发侧（护栏 b）：作者 ≠ bot + 词位 @bot 才判定；PR 评论跳过（归批次 2.3）；
///   is_collaborator（护栏 a）：true → triggers；false → 日志跳过；Err → failed 重试；
/// - 私信失败/协作者校验失败 → 该评论进 failed（游标回退，下轮重试）。
#[allow(clippy::too_many_arguments)] // 注入面全（api/msgr/评论/seen/映射/身份），与 MockAgentRunner::run 同款
pub(crate) async fn process_comment_batch(
    api: &dyn crate::github::GithubApi,
    msgr: &dyn crate::messenger::Messenger,
    comments: &[crate::github::GhComment],
    seen: &[u64],
    mention_map: &[(String, String)],
    bot_login: &str,
    repo: &str,
    owner: &str,
) -> CommentBatch {
    let mut out = CommentBatch {
        seen_extra: Vec::new(),
        failed: Vec::new(),
        triggers: Vec::new(),
        new_since: comments.last().map(|c| c.updated_at.clone()),
    };
    for c in comments {
        if seen.contains(&c.id) {
            continue;
        }
        let mut ok = true;
        // 1) @提及 私信：映射内 login 才发；评论 → issue/PR 号取自评论自身 html_url
        let mentions = crate::github::extract_mentions(&c.body, bot_login)
            .into_iter()
            .filter(|l| !l.eq_ignore_ascii_case(&c.user.login)) // 自提及不通知（同 GitHub）
            .collect::<Vec<_>>();
        for login in mentions {
            if let Some((_, chat)) = mention_map
                .iter()
                .find(|(l, _)| l.eq_ignore_ascii_case(&login))
            {
                if let Some((number, _)) = crate::github::comment_issue_ref(c) {
                    if let Err(e) = msgr
                        .send_text(chat, &crate::github::mention_notify_text(repo, number, c))
                        .await
                    {
                        crate::log!("[github] ⚠️ 提及私信失败 {repo} 目标={chat}: {e:#}");
                        ok = false;
                    }
                }
            }
        }
        // 2) @bot 自动处理触发（护栏 b：作者回声 + 词位已由 should_auto_process 把关）
        if crate::github::should_auto_process(&c.body, &c.user.login, bot_login) {
            match crate::github::comment_issue_ref(c) {
                Some((_n, true)) => { /* PR 评论自动处理归批次 2.3 */ }
                Some((number, false)) => {
                    match api.is_collaborator(owner, repo, &c.user.login).await {
                        Ok(true) => out.triggers.push((number, c.id)),
                        Ok(false) => crate::log!(
                            "[github] 跳过 @bot 触发（{repo} 非协作者 @{}）",
                            c.user.login
                        ),
                        Err(e) => {
                            crate::log!(
                                "[github] ⚠️ 协作者校验失败 {repo} @{}: {e:#}",
                                c.user.login
                            );
                            ok = false;
                        }
                    }
                }
                None => { /* 评论 URL 解析失败 → 不触发 */ }
            }
        }
        if ok {
            out.seen_extra.push(c.id);
        } else {
            out.failed.push(c.updated_at.clone());
        }
    }
    out
}

/// 构造 @bot 自动处理的合成事件（复用 handle() 的指令门全套机制）：
/// - mid "gh:{repo}:{issue}:{comment_id}"：天然唯一（去重 + pending 键）；
/// - chat_id = 通知群、chat_type = "group"：不触发 save_primary_chat；
/// - thread_id 留空：send_reply 走普通发送（非空会进 send_thread_reply 到假话题，必须空）；
/// - text = "分析 <链接>"：命中既有门 → 白名单 → 拉取注入 → agent → 双写，零新机制。
///
/// repo/number 由评论自身 html_url 推导（调用方传入），评论者无法重定向分析目标。
fn auto_ev(repo: &str, number: u64, comment_id: u64, notify_chat: &str) -> crate::bridge::Ev {
    crate::bridge::Ev {
        mid: format!("gh:{repo}:{number}:{comment_id}"),
        chat_id: notify_chat.to_string(),
        chat_type: "group".into(),
        thread_id: String::new(),
        quoted: Default::default(),
        text: format!("分析 https://github.com/{repo}/issues/{number}"),
        attachments: Vec::new(),
    }
}

/// 睡眠 dur，但可被 stop 信号打断。返回 true=收到停止信号（调用方应 break）。
async fn interruptible_sleep(dur: std::time::Duration, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = stop.changed() => true,
    }
}

/// GitHub watch 循环（附挂能力，Phase 1）：每 60s 增量轮询白名单仓库的 open issues，
/// 新 issue → 发「🔔 新 issue …」到 gh_notify_chat；自己（gh_username）发的不回显。
/// 游标落盘 workspaces/<bot>/github_cursor.json（原子写）：崩溃/重启后增量续跑不重发。
/// 状态上报只写「重连中」迁移（连续 3 轮失败）——botstatus 槽位归 IM 事件循环，
/// watch 循环不报在线（无条件写在线会覆盖 IM 的重连中迁移）；退出不清 botstatus。
async fn github_watch_loop(
    bot: crate::config::BotConfig,
    bridge: Arc<Bridge>,
    stop_rx: &mut watch::Receiver<bool>,
) {
    let key = bot.key();
    let repos = bot.gh_repo_list();
    let notify_chat = bot.gh_notify_chat.clone();
    let echo = bot.gh_username.clone();
    let mention_map = bot.gh_mention_map_list();
    crate::log!(
        "[bot:{key}] GitHub watch 循环启动 repos={} 通知={} 提及映射={}",
        repos.len(),
        if notify_chat.is_empty() {
            "（未配置通知群）"
        } else {
            "已配置"
        },
        mention_map.len()
    );
    let mut cursor = crate::github::GhCursor::load(&key);
    let mut consec_errs = 0u32;
    loop {
        if interruptible_sleep(std::time::Duration::from_secs(60), stop_rx).await {
            break;
        }
        // 通知目标与提及映射都空 → 轮询无意义；两者任一配置即跑（评审 I1：
        // 只配 gh_mention_map 的「仅私信不群发」配置不该被 notify_chat 拦截）。
        if notify_chat.is_empty() && mention_map.is_empty() {
            continue;
        }
        // 静默基线用当前时刻（评审 L2）：since 为空不调 API，游标直接置 now——
        // 否则 >100 条存量时游标落在列表中间，下一轮把中段存量误当新条目。
        let now_rfc = crate::chrono_lite::rfc3339_now();
        let mut sweep_failed = false;
        for (owner, name) in crate::github::watch_entries(&repos) {
            let repo = format!("{owner}/{name}");
            let cur = cursor.repo_cursor(&repo);
            // ── issue 侧：新 issue 通知（Phase 1）──
            if cur.since.is_empty() {
                cursor.update(&repo, &now_rfc, Vec::new()); // 静默基线
            } else {
                match bridge
                    .github_client
                    .list_issues_since(&owner, &name, &cur.since)
                    .await
                {
                    Ok(issues) => {
                        let (fresh, new_since) =
                            crate::github::new_issues(&issues, &cur.since, &cur.seen, &echo);
                        let mut seen_extra = Vec::new();
                        // 通知失败不推进游标到该 issue 之后：retry_at 记下**所有**失败 issue 的
                        // created_at 最小值（逐个覆盖取最后失败者会丢 created 更早者，见
                        // github::retry_since），下一轮以它为 since 重新浮现（已通知的都在 seen 里，
                        // 不会重复）。
                        let mut failed: Vec<&crate::github::GhIssue> = Vec::new();
                        for iss in &fresh {
                            let text = crate::github::notify_text(&repo, iss);
                            match bridge.msgr.send_text(&notify_chat, &text).await {
                                Ok(()) => {
                                    seen_extra.push(iss.id);
                                    crate::log!(
                                        "[bot:{key}] 新 issue 已通知 #{}({})",
                                        iss.number,
                                        repo
                                    );
                                }
                                Err(e) => {
                                    failed.push(iss);
                                    crate::log!(
                                        "[bot:{key}] ⚠️ 新 issue 通知失败 repo={repo}: {e:#}"
                                    );
                                }
                            }
                        }
                        let effective_since = crate::github::retry_since(&failed)
                            .unwrap_or_else(|| new_since.clone());
                        cursor.update(&repo, &effective_since, seen_extra);
                    }
                    Err(e) => {
                        sweep_failed = true;
                        crate::log!("[bot:{key}] ⚠️ 拉取 {repo} issues 失败: {e:#}");
                    }
                }
            }
            // ── 评论侧：@提及 私信（Phase 2 批次 2.1；@bot 触发在 2.2）──
            if cur.comment_since.is_empty() {
                cursor.comment_update(&repo, &now_rfc, Vec::new()); // 静默基线（存量评论不私信）
            } else {
                match bridge
                    .github_client
                    .list_comments_since(&owner, &name, &cur.comment_since)
                    .await
                {
                    Ok(comments) => {
                        let batch = crate::service::process_comment_batch(
                            bridge.github_client.as_ref(),
                            bridge.msgr.as_ref(),
                            &comments,
                            &cur.comment_seen,
                            &mention_map,
                            &echo,
                            &repo,
                            &owner,
                        )
                        .await;
                        // 私信失败 → 游标回退到最早失败评论（同 retry_since 语义，下轮重试）。
                        // 边界假设：GitHub since 按 updated_at **严格大于** 过滤（"updated after"），
                        // updated_at == since 的失败评论不会自然重浮——评论被编辑或依赖
                        // 后续重试窗口时才会重取；Phase 1 issue 侧同款假设（created_at 回退），
                        // 已知取舍，注释声明（评审 M3）。
                        // 空批（new_since=None）→ 保持原游标，绝不能写空串清掉（评审 C1）。
                        let effective = batch.failed.iter().min().cloned().or(batch.new_since);
                        if let Some(e) = effective {
                            cursor.comment_update(&repo, &e, batch.seen_extra);
                        }
                        // @bot 自动处理：合成 Ev 走既有指令门（白名单→拉取→agent→双写）。
                        // 群 key 串行：与群消息共用通知群锁，长任务与手动「分析」同语义。
                        for (number, comment_id) in batch.triggers {
                            let bridge = bridge.clone();
                            let ev = auto_ev(&repo, number, comment_id, &notify_chat);
                            tokio::spawn(async move {
                                bridge.handle(ev).await;
                            });
                        }
                    }
                    Err(e) => {
                        sweep_failed = true;
                        crate::log!("[bot:{key}] ⚠️ 拉取 {repo} 评论失败: {e:#}");
                    }
                }
            }
        }
        cursor.save(&key); // 每轮原子落盘（60s 一次小写盘，崩溃至多丢一个窗口）
                           // 状态上报：**不报在线**——botstatus 槽位归 IM 事件循环（它每 10s 上报一次在线），
                           // watch 循环无条件写在线会覆盖 IM 的「重连中」迁移（上次写者赢）。这里只在
                           // GitHub 侧连续 3 轮失败时标「重连中」（失败迁移也只会被 IM 循环的在线覆盖，
                           // 而 IM 在线是真实的——GitHub 故障期间 IM 正常时槽位显示在线属可接受偏差）。
        if sweep_failed {
            consec_errs += 1;
            if consec_errs == 3 {
                crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "重连中");
            }
        } else {
            consec_errs = 0;
        }
    }
    // 注意：这里不清 botstatus——槽位归 IM 事件循环（weixin_loop 退出时 clear），
    // github 是附挂能力，退场不该把 IM 的状态一起抹掉。
}

/// 执行一个到点任务：跑该 bot 生效后端（全新会话，不带聊天上下文）→ 回发；once 任务执行后删除。
/// 回发优先发任务原会话；若该会话已失效（群解散/bot 被移出等），回落到主会话（私聊，必存在）。
/// 多目标（#21）：job.targets 非空时向每个目标各投一份（可跨 bot，经路由表投递 + 失败兜底）。
async fn run_job(
    bridge: Arc<Bridge>,
    router: std::sync::Arc<crate::deliver::Router>,
    job: crate::schedule::Job,
) {
    let bot_key = bridge.bot.key();
    let prompt_preview = crate::agent::truncate(&job.prompt, 40); // 按字符截断（含中文）
                                                                  // 后端跟 bot 走：bridge.default_backend 已在 Bridge::new 里取 bot.effective_backend(&cfg.default_backend)，
                                                                  // 与聊天消息同一后端，避免「聊天走 codex、定时任务却跑 claude」的割裂。
    let backend = crate::agent::Backend::parse(&bridge.default_backend);
    crate::log!(
        "[bot:{bot_key}] 触发任务 {} → {}（backend: {}）",
        &job.id[..8],
        prompt_preview,
        backend.name()
    );
    let reply = match crate::agent::run(
        backend,
        &job.prompt,
        &uuid::Uuid::new_v4().to_string(), // 每次全新 session，不带聊天上下文
        false,
        &job.chat_id,
        &bot_key,
        None, // claude/pi 无需回存 thread_id（只有 codex 要回存真实 thread_id）
        None, // 定时任务不推中间进度（统一只发最终结果）
        None, // 定时任务不可被聊天打断
    )
    .await
    {
        Ok(crate::agent::RunOutcome::Reply { reply, .. }) => reply,
        Ok(crate::agent::RunOutcome::Cancelled) => "⏰ 任务被中断".to_string(), // 定时任务不会触发
        Err(e) => format!("⏰ 定时任务执行失败：{e}"),
    };
    let header = match job.kind {
        crate::schedule::JobKind::Once => "⏰ 定时提醒",
        crate::schedule::JobKind::Cron => "⏰ 定时任务",
    };
    let text = format!("{header}\n\n{reply}");
    // 多目标（#21）：每个目标各投一份（跨 bot 走路由表；失败由 Router 回源报错 + 微信 outbox 兜底）
    if !job.targets.is_empty() {
        for item in
            crate::deliver::job_target_items(&bot_key, &job.chat_id, &job.id, &job.targets, &text)
        {
            router.deliver(&item).await;
        }
        if job.kind == crate::schedule::JobKind::Once {
            bridge.jobs.remove(&job.id);
        }
        return;
    }
    // 旧行为：先发原会话
    match bridge.msgr.send_text(&job.chat_id, &text).await {
        Ok(()) => crate::log!(
            "[bot:{bot_key}] 任务 {} 发送成功 chat={} 长度={}",
            &job.id[..8],
            &job.chat_id[..job.chat_id.len().min(10)],
            text.chars().count()
        ),
        Err(e) => {
            crate::log!(
                "[bot:{bot_key}] 任务 {} 原会话 {} 发送失败（{}）",
                &job.id[..8],
                &job.chat_id[..job.chat_id.len().min(10)],
                e
            );
            // 微信：主动推送受会话活跃度约束（ret=-2 = context_token stale），同 token 重试
            // 必然再失败 → 落盘积压，等用户下次发消息刷新 token 后补发，避免任务报告静默丢失。
            if bridge.bot.is_wechat() {
                bridge.queue_outbox(&job.chat_id, &text, &job.id);
            }
            // 原会话失效 → 回落本 bot 主会话（微信主会话==原会话时上面已积压，不再重复发）
            let primary = crate::config::Config::primary_chat(&bot_key);
            if !primary.is_empty() && primary != job.chat_id {
                let fallback_text = format!("{header}（原会话已失效，转发到主会话）\n\n{reply}");
                crate::log!(
                    "[bot:{bot_key}] 任务 {} 回落主会话 {}",
                    &job.id[..8],
                    &primary[..primary.len().min(10)]
                );
                match bridge.msgr.send_text(&primary, &fallback_text).await {
                    Ok(()) => crate::log!(
                        "[bot:{bot_key}] 任务 {} 回落发送成功 chat={} 长度={}",
                        &job.id[..8],
                        &primary[..primary.len().min(10)],
                        fallback_text.chars().count()
                    ),
                    Err(se) => {
                        crate::log!(
                            "[bot:{bot_key}] 任务 {} 回落发送也失败（{}）",
                            &job.id[..8],
                            se
                        );
                        if bridge.bot.is_wechat() {
                            bridge.queue_outbox(&primary, &fallback_text, &job.id);
                        }
                    }
                }
            }
        }
    }
    if job.kind == crate::schedule::JobKind::Once {
        bridge.jobs.remove(&job.id);
    }
}
