//! --service 入口：无头桥守护进程（纯 tokio，无 GUI）。
//! 组装 config → messenger（按 bot.kind：飞书 WS / 微信长轮询）→ bridge → 事件循环。
//! 处理 SIGTERM/SIGINT 优雅退出。

use crate::bridge::Bridge;
use crate::config::Config;
use crate::messenger;
use std::sync::Arc;
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
    // #69 审计：短命任务（装完即收尾），登记进治理（panic/指标可见）；装不上只 log 警告。
    if cfg.bots.iter().any(|b| b.enabled && b.kind == "feishu") {
        crate::tasks::tasks().spawn("larkskills", async {
            crate::larkskills::ensure_lark_setup().await;
        });
    }

    // #69：退出信号 → 全局取消令牌广播（原 watch::channel(stop) 职责，统一进治理）。
    // 长驻任务（活到关停），登记 spawn_forever——关停前意外退出会被计 errors_total。
    // CancelOnShutdown 守卫：本任务任何提前退出路径（含 panic 被捕获）都补一次 cancel
    // （幂等），保持原 watch 实现的 fail-open 语义（循环退出 → 进程优雅退出 → 看门狗重启），
    // 绝不出现「signal 死了所有循环永久挂起」的 fail-closed 死挂。
    crate::tasks::tasks().spawn_forever("signal", async {
        let _cancel_guard = crate::tasks::CancelOnShutdown::default();
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
        crate::tasks::shutdown_token().cancel();
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
        messengers, // 此后无其它消费方（github bot 循环已移除），直接 move
        bot_cfgs,
        None,
    ));
    // 跨会话投递消费循环（独立于 bot 循环，共享关停令牌）。#69：长驻，登记 spawn_forever。
    {
        let router = router.clone();
        let stop = crate::tasks::shutdown_token();
        crate::tasks::tasks().spawn_forever("deliver", async move {
            deliver_loop(router, stop).await;
        });
    }
    // #74 消息历史维护（长驻）：每 2s 消费 GUI 命令文件（手动清除 / 弹窗已读），
    // 每 24h 按保留期清理 messages.sqlite（启动即跑一次）。
    {
        let stop = crate::tasks::shutdown_token();
        crate::tasks::tasks().spawn_forever("history-gc", async move {
            history_gc_loop(stop).await;
        });
    }
    let mut handles = Vec::new();
    for (bot, msgr) in ready {
        let cfg = cfg.clone();
        let stop = crate::tasks::shutdown_token();
        let router = router.clone();
        // 任务名带 bot key（Box::leak：每次进程启动每 bot 一行小字符串，换取
        // errors/panic 告警可定位到具体 bot——审查 Minor 3）
        let name: &'static str = Box::leak(format!("bot:{}", bot.key()).into_boxed_str());
        handles.push(crate::tasks::tasks().spawn_forever(name, async move {
            run_bot(bot, cfg, msgr, router, stop).await;
        }));
    }
    // 等所有 bot 循环结束（正常只有关停广播才会结束）
    for h in handles {
        let _ = h.await;
    }
    // #69 优雅关闭：close（拒绝新登记）→ cancel（幂等，信号任务可能已广播）→ wait
    // 等全部在册任务收尾；指标汇总一行日志（shutdown_wait_ms 等）。
    let _ = crate::tasks::tasks().shutdown_wait().await;
    crate::log!("[service] 已退出");
}

/// 跨会话投递消费循环：轮询 deliveries.json（agent 的 deliver CLI 落盘），逐条经路由表投递。
/// 失败项不回盘（避免死循环重投），由 Router 负责回源报错 / 微信 outbox 兜底。
async fn deliver_loop(
    router: std::sync::Arc<crate::deliver::Router>,
    stop: tokio_util::sync::CancellationToken,
) {
    crate::log!(
        "[deliver] 投递循环启动（跨会话投递开关={}）",
        router.enabled
    );
    let store = crate::deliver::DeliveryStore::new();
    loop {
        if interruptible_sleep(std::time::Duration::from_secs(1), &stop).await {
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
    stop: tokio_util::sync::CancellationToken,
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
    // #69 审计：短命任务，登记进治理，**token 逐条检查**（审查 Important）——恢复会跑
    // 完整 agent 管线（单条可达数分钟），关停广播后立即停止重放（未重放条目留盘
    // pending.json，下次启动续跑——正是 #25 的恢复语义），不让 shutdown_wait 无界等它。
    {
        let bridge = bridge.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("recover:{}", key).into_boxed_str());
        crate::tasks::tasks().spawn(name, async move {
            bridge.recover_pending(&stop).await;
        });
    }

    // #33 历史会话迁移**自动触发**：每次启动把后端私有 session 里尚未导入的对话
    // （claude/codex/pi/prime）导入 history.rs——老历史参与注入接续，无需手动跑
    // `session-import`。幂等：imported.json 来源键标记，已导入来源秒跳过；追加式
    // 写入不触碰现有 history。spawn_blocking（fs 密集：读后端文件 + 提取 + 写）；
    // 失败仅 log 警告，绝不阻塞 bot 启动。
    {
        let key = key.clone();
        let name: &'static str = Box::leak(format!("session-import:{}", key).into_boxed_str());
        crate::tasks::tasks().spawn(name, async move {
            let kb = key.clone(); // 闭包内另持一份（日志用）
            let report =
                tokio::task::spawn_blocking(move || crate::session_import::import_bot(&kb, false))
                    .await
                    .unwrap_or_default();
            if report.total > 0 {
                crate::log!(
                    "[bot:{key}] 历史会话自动迁移完成：导入 {} 条（{} 个 chat）",
                    report.total,
                    report.chats.len()
                );
            } else if !report.chats.is_empty() {
                crate::log!(
                    "[bot:{key}] 历史会话迁移扫描完成：无新内容（{} 个 chat 有跳过项）",
                    report.chats.len()
                );
            }
        });
    }

    // 定时任务调度循环（独立于事件循环，共享关停令牌）。#69：长驻，登记 spawn_forever。
    {
        let bridge = bridge.clone();
        let key = key.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("schedule:{}", key).into_boxed_str());
        crate::tasks::tasks().spawn_forever(name, async move {
            crate::log!("[bot:{key}] 调度循环启动");
            let mut last_min: Option<String> = None;
            // 在跑任务集合：cron 周期短于任务耗时时，跳过重叠的新一轮（防同任务并发堆积、
            // 多个 claude 抢同一资源/互相踩工作区）。
            let running = Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::<String>::new(),
            ));
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(20), &stop).await {
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
                    // #69 审计：中命任务（一次 agent 运行），有 owner（调度循环在跑集合 +
                    // pending.json 重启恢复）。**不登记**：关停时等 agent 跑完会让「停止」
                    // 语义退化成最长任务时长；进程退出兜底 + 下次启动 agent-pids 清理。
                    tokio::spawn(async move {
                        run_job(bridge, router, job).await;
                        running.lock().unwrap().remove(&jid);
                    });
                }
            }
            crate::log!("[bot:{key}] 调度循环退出");
        });
    }

    // 事件循环：按通道分派
    if bot.is_dingtalk() {
        crate::dingtalk::stream_loop(bot.app_id.clone(), bot.app_secret.clone(), bridge, stop)
            .await;
        crate::botstatus::clear(&key);
        crate::log!("[bot:{key}] 钉钉 Stream 循环退出");
    } else if bot.is_wechat() {
        weixin_loop(bot, bridge, stop).await; // weixin_loop 退出时已 clear
        crate::log!("[bot:{key}] 微信长轮询循环退出");
    } else {
        crate::ws::ws_loop(bot.app_id.clone(), bot.app_secret.clone(), bridge, stop).await;
        crate::botstatus::clear(&key);
        crate::log!("[bot:{key}] WS 循环退出");
    }
}

/// 微信事件循环：HTTP 长轮询 getupdates（游标增量）→ bridge.on_weixin。
/// 断线/出错退避重试；errcode -14（会话超时）报「会话过期」后退出（需重新扫码）。
async fn weixin_loop(
    bot: crate::config::BotConfig,
    bridge: Arc<Bridge>,
    stop: tokio_util::sync::CancellationToken,
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
        if stop.is_cancelled() {
            break;
        }
        let next = client.get_updates(&cursor, timeout_ms);
        tokio::select! {
            _ = stop.cancelled() => { break; }
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
                        // #69 审计：短/中命、有 owner（bridge chat_lock + pending.json 恢复），
                        // 不登记——关停语义靠进程退出兜底（见 tasks.rs 登记口径）。
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
                        if interruptible_sleep(std::time::Duration::from_secs(3), &stop).await {
                            break;
                        }
                    }
                }
            }
        }
    }
    crate::botstatus::clear(&key);
}
/// 睡眠 dur，但可被关停令牌打断。返回 true=收到关停广播（调用方应 break）。
async fn interruptible_sleep(
    dur: std::time::Duration,
    stop: &tokio_util::sync::CancellationToken,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = stop.cancelled() => true,
    }
}

/// #74 历史维护循环：命令文件消费（2s 轮询）+ 保留期 GC（24h，启动也跑一次）。
/// - 命令文件：GUI 跨进程写 msg-clear.command / msg-read.command（「手动清除」/
///   「弹窗已读」），存在即消费（deliveries.json 队列先例，详见 msgstore::consume_commands）；
///   2s 轮询保证手动清除的反馈延迟可感知地小（≈2s），不必等 24h 的 GC 周期。
/// - GC：保留期热读 config（改配置保存重启即生效）；启动立即跑一次清掉积压过期记录。
async fn history_gc_loop(stop: tokio_util::sync::CancellationToken) {
    crate::log!("[history-gc] 历史维护循环启动（消息库保留期清理 + GUI 命令消费）");
    let mut last_gc: Option<u64> = None;
    loop {
        if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
            break;
        }
        crate::msgstore::consume_commands();
        // 保留期 GC：启动即跑一次（last_gc=None → due），之后每 24h
        let now = crate::chrono_lite::unix_secs();
        let due = last_gc
            .map(|t| now.saturating_sub(t) >= 24 * 3600)
            .unwrap_or(true);
        if due {
            last_gc = Some(now);
            let days = Config::load()
                .map(|c| c.history_retention_days)
                .unwrap_or(30);
            let removed = crate::msgstore::MsgStore::production().gc(days);
            crate::log!("[history-gc] 保留期 {days} 天，清理 {removed} 条过期历史");
        }
    }
    crate::log!("[history-gc] 历史维护循环退出");
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
    // 授权者建的任务在受限分支执行：prompt 前置与聊天路径一致的受限说明
    //（否则模型不知道自己被闸着，越界被拒后瞎试）。与 agent::run 同源公共判定
    // config::restrict_granted（role==Granted && 开关热读）。
    let prompt = if crate::config::restrict_granted(job.role, &bot_key) {
        format!(
            "[受限模式] 你是受限会话：只能读/写本工作区（当前 bot 目录）内的文件；\
你的记忆文件是 GRANTED.md（跨轮次保存信息用它，可读写）；\
可用命令仅限 $ABB_BIN（定时任务/投递）与只读 git；不可联网、不可访问工作区外任何路径；\
越界操作会被系统拦截并记录。\n\n{}",
            job.prompt
        )
    } else {
        job.prompt.clone()
    };
    let reply = match crate::agent::run(
        backend,
        &prompt,
        &uuid::Uuid::new_v4().to_string(), // 每次全新 session，不带聊天上下文
        false,
        &job.chat_id,
        &job.chat_id, // session_key：sessions=None 时仅回存分支用不到，占位保持一致
        &bot_key,
        job.role, // 按创建者角色执行：授权者建的任务走受限分支
        None,     // claude/pi 无需回存 thread_id（只有 codex 要回存真实 thread_id）
        None,     // 定时任务不推中间进度（统一只发最终结果）
        None,     // 定时任务不可被聊天打断
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
