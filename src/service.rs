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

    // 每个 bot 一个事件循环 + Bridge + 定时调度，并行跑
    let mut handles = Vec::new();
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
        let bot = bot.clone();
        let cfg = cfg.clone();
        let stop = stop_rx.clone();
        handles.push(tokio::spawn(async move {
            run_bot(bot, cfg, stop).await;
        }));
    }
    if handles.is_empty() {
        crate::log!("[service] 没有可用 bot，退出");
        // 无可跑 bot（全部停用/凭证不齐）→ 清「期望运行」标记，看门狗停止自动重拉
        crate::install::set_desired(false);
        std::process::exit(1);
    }
    // 等所有 bot 循环结束（正常只有 stop 才会结束）
    for h in handles {
        let _ = h.await;
    }
    crate::log!("[service] 已退出");
}

/// 单个 bot 的全套：事件循环（飞书 WS / 微信长轮询）+ 定时任务调度循环。
async fn run_bot(bot: crate::config::BotConfig, cfg: Arc<Config>, stop_rx: watch::Receiver<bool>) {
    let key = bot.key();
    crate::log!(
        "[bot:{key}] 启动（kind={} name={}）",
        bot.kind,
        bot.bot_name
    );
    let msgr = match messenger::build(&bot) {
        Ok(m) => m,
        Err(e) => {
            crate::log!("[bot:{key}] 构造 messenger 失败: {e:#}");
            return;
        }
    };
    let bridge = Arc::new(Bridge::new(msgr, bot.clone(), &cfg));

    // 连接态初始上报：进入事件循环前是「连接中」；之后由各事件循环在状态迁移时更新。
    // （别用独立心跳任务只报「活着」——那测的是上报线程不是通道连通，会话死了托盘还在线。）
    crate::botstatus::report(&key, &bot.kind, &bot.bot_name, "连接中");

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
            let running = Arc::new(std::sync::Mutex::new(std::collections::HashSet::<String>::new()));
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
                    let running = running.clone();
                    let jid = job.id.clone();
                    tokio::spawn(async move {
                        run_job(bridge, job).await;
                        running.lock().unwrap().remove(&jid);
                    });
                }
            }
            crate::log!("[bot:{key}] 调度循环退出");
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
    let client = crate::wechat::WeixinClient::new(base, &bot.wx_token);
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

/// 睡眠 dur，但可被 stop 信号打断。返回 true=收到停止信号（调用方应 break）。
async fn interruptible_sleep(dur: std::time::Duration, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = stop.changed() => true,
    }
}

/// 执行一个到点任务：跑 claude（全新会话，不带聊天上下文）→ 回发飞书；once 任务执行后删除。
/// 回发优先发任务原会话；若该会话已失效（群解散/bot 被移出等），回落到主会话（私聊，必存在）。
async fn run_job(bridge: Arc<Bridge>, job: crate::schedule::Job) {
    let bot_key = bridge.bot.key();
    let prompt_preview = crate::agent::truncate(&job.prompt, 40); // 按字符截断（含中文）
    crate::log!(
        "[bot:{bot_key}] 触发任务 {} → {}",
        &job.id[..8],
        prompt_preview
    );
    let reply = match crate::agent::run(
        crate::agent::Backend::Claude,
        &job.prompt,
        &uuid::Uuid::new_v4().to_string(), // 每次全新 session，不带聊天上下文
        false,
        &job.chat_id,
        &bot_key,
        None, // claude 无需回存 thread_id
        None, // 定时任务不流式中间进度
        None, // 定时任务不可被聊天打断
    )
    .await
    {
        Ok(crate::agent::RunOutcome::Reply(r)) => r,
        Ok(crate::agent::RunOutcome::Cancelled) => "⏰ 任务被中断".to_string(), // 定时任务不会触发
        Err(e) => format!("⏰ 定时任务执行失败：{e}"),
    };
    let header = match job.kind {
        crate::schedule::JobKind::Once => "⏰ 定时提醒",
        crate::schedule::JobKind::Cron => "⏰ 定时任务",
    };
    // 先发原会话
    let sent = bridge
        .msgr
        .send_text(&job.chat_id, &format!("{header}\n\n{reply}"))
        .await;
    if let Err(e) = sent {
        // 原会话失效 → 回落本 bot 主会话
        let primary = crate::config::Config::primary_chat(&bot_key);
        crate::log!(
            "[bot:{bot_key}] 任务 {} 原会话 {} 发送失败（{}），回落主会话 {}",
            &job.id[..8],
            &job.chat_id[..job.chat_id.len().min(10)],
            e,
            &primary[..primary.len().min(10)]
        );
        if !primary.is_empty() && primary != job.chat_id {
            let _ = bridge
                .msgr
                .send_text(
                    &primary,
                    &format!("{header}（原会话已失效，转发到主会话）\n\n{reply}"),
                )
                .await;
        }
    }
    if job.kind == crate::schedule::JobKind::Once {
        bridge.jobs.remove(&job.id);
    }
}
