//! --service 入口：无头桥守护进程（纯 tokio，无 GUI）。
//! 组装 config → messenger（按 bot.kind：飞书 WS / 微信长轮询）→ bridge → 事件循环。
//! 处理 SIGTERM/SIGINT 优雅退出。

use crate::bridge::Bridge;
use crate::config::Config;
use crate::messenger;
use std::collections::HashMap;
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
    // #174：一次性迁移旧 key（name）目录/登记 → 新 key（app_id/wx_user_id 优先）。
    // 幂等（新目录存在跳过）；失败只 log 不阻塞（数据仍在旧目录，日志指明）。
    cfg.migrate_keys();
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

    // #200：mini-relay —— ABB 内嵌 Nostr relay（NIP-01/42/98 子集），buzz-acp 可连接
    // 订阅虚拟 Bot 频道触发 buzz-agent 执行。开关默认关（config buzz_relay_enabled）。
    // 状态（密钥/sqlite/频道表）在 bot 循环前**同步**初始化：Bridge 构造即拿到
    // RelayState（或 None），不存在「后台任务异步填共享句柄、bot 先读为 None」的
    // 启动竞态。WS 服务 / buzz-acp 子进程 / 回复回流仍在后台任务，等 messengers
    // 就绪后再 spawn（回复回流要按频道 uuid 路由到归属 bot，见下方）。
    let mut buzz_init: Option<BuzzRelayInit> = if cfg.buzz_relay_enabled {
        // 启动关键路径加期限（#184 教训：常态等待不设限、启动步骤要有界）：
        // sqlite 打开/种子事件在挂起介质上不得卡死全部 bot 启动。
        match tokio::time::timeout(std::time::Duration::from_secs(10), init_buzz_relay(&cfg)).await
        {
            Ok(Ok(init)) => Some(init),
            Ok(Err(e)) => {
                crate::log!("[mini-relay] ⚠️ 初始化失败: {e}（buzz 后端不可用，CLI 后端不受影响）");
                None
            }
            Err(_) => {
                crate::log!("[mini-relay] ⚠️ 初始化超时 10s（buzz 后端不可用）");
                None
            }
        }
    } else {
        None
    };
    // buzz 后端 bot + relay 不可用（开关关/初始化失败）= dispatch 必然丢弃：
    // 配置不对齐没有运行期补救，启动即告警（消息侧另有用户可见报错，这里给运维看）。
    if buzz_init.is_none() {
        let buzz_bots: Vec<String> = cfg
            .bots
            .iter()
            .filter(|b| {
                b.enabled
                    && crate::agent::Backend::parse(b.effective_backend(&cfg.default_backend))
                        == crate::agent::Backend::Buzz
            })
            .map(|b| b.key())
            .collect();
        if !buzz_bots.is_empty() {
            crate::log!(
                "[mini-relay] ⚠️ buzz_relay_enabled=false 或初始化失败，但 bot {buzz_bots:?} 后端为 buzz：消息将被丢弃并报错"
            );
        }
    }

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
    // #191：select 同时监听关停令牌——自发起关停（无信号，如全部 bot 自行退出）时
    // shutdown_wait 会 cancel 令牌，本任务若仍 parked 在等信号，tracker.wait() 永等
    // 它 → 收尾恒烧满 20s 总期限强退。令牌已取消即让位退出（不记信号日志，cancel
    // 幂等无害）。代价：收尾窗口内再来的 SIGTERM 走默认处置立即终止进程——与
    // systemd「第二次信号=立即杀」惯例一致，且强退本就是该窗口的兜底终点。
    crate::tasks::tasks().spawn_forever("signal", async {
        let _cancel_guard = crate::tasks::CancelOnShutdown::default();
        if wait_exit_signal_or_shutdown(crate::tasks::shutdown_token()).await {
            crate::log!("[service] 收到退出信号");
        }
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
    // #200 胶水：mini-relay 服务期任务（状态初始化成功才启动）：
    // ① buzz-acp 子进程（含存活巡检重拉）；② 回复回流 → 按频道 uuid 路由到归属 bot
    // 的 messenger → send_text 发回聊天平台；③ WS 服务（绑定失败退避重试）。
    // 必须在 messenger 就绪后 spawn（回流路由要查 messengers），且在 Router::new
    // 消费 messengers 之前建好路由表（messengers 此后被 move）。
    // 表达式值 = 注入各 Bridge 的 relay 状态（bot 循环构造时按值传入，无竞态）。
    let buzz_relay_state: Option<std::sync::Arc<crate::buzzrelay::RelayState>> = if let Some(init) =
        buzz_init.take()
    {
        let BuzzRelayInit {
            state,
            mut reply_rx,
            agent_secret,
            vbs,
        } = init;
        // 回复回流路由表：频道 uuid → messenger（uuid 含 bot_key，两 bot 同群不串线；
        // 回流事件本身带 channel_uuid）。vbs 用 init 的同一份快照（频道集与路由表
        // 绝不同源漂移）。运行期新登记的群不参与 buzz（重启生效，与频道集一致）。
        let senders: Arc<HashMap<String, Arc<dyn crate::messenger::Messenger>>> = {
            let mut map = HashMap::new();
            for vb in &vbs {
                if let Some(m) = messengers.get(&vb.bot_key) {
                    map.insert(
                        crate::buzzrelay::channel_uuid(&vb.bot_key, &vb.chat_id),
                        Arc::clone(m),
                    );
                }
            }
            Arc::new(map)
        };
        let relay_state_for_bots = Arc::clone(&state);
        // buzz-acp 作者门的合法 owner = 桥身份公钥（用户消息由桥签名，见 spawn_buzz_acp）
        let bridge_pubkey = state.bridge_pubkey();
        let cfg = Arc::clone(&cfg);
        crate::tasks::tasks().spawn_forever("mini-relay", async move {
            // ① buzz-acp 子进程 + 存活巡检。句柄必须被长期持有：Child 置了
            // kill_on_drop(true)，句柄一 drop 子进程即被 SIGKILL（审查 #205 P0——
            // 原实现 match Ok(_) 即丢，「已拉起」日志后微秒级被杀，功能整体失效）。
            // 巡检任务持有句柄：30s 检查存活，退出即重拉（父进程责任：buzz-acp 是
            // agent 池化执行的唯一入口，死了全部 buzz 会话静默停摆）。
            crate::tasks::tasks().spawn_forever("mini-relay-acp", {
                let cfg = Arc::clone(&cfg);
                let bridge_pubkey = bridge_pubkey.clone();
                async move {
                    let mut acp = spawn_buzz_acp(&cfg, &agent_secret, &bridge_pubkey);
                    // I5（buzz docs/remote-agents.md「Intentional termination is
                    // final」）：**主动退出（exit 0，owner !shutdown / auto-stop 到点）
                    // 对自动重拉是终态**——巡检不得复活操作者有意停掉的实例；崩溃
                    // （非零/探测失败）才重拉。「owner 可再发起一次 Start」的路径 =
                    // 重启 ABB 服务（本任务随进程重建），不是这里的自动复活。
                    let mut terminal = false;
                    let stop = crate::tasks::shutdown_token();
                    loop {
                        // 关停可中断的 30s 节拍（服务期常态等待，不设总期限）
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                            _ = stop.cancelled() => break,
                        }
                        if terminal {
                            continue;
                        }
                        match acp.as_mut().map(|p| p.try_exit()) {
                            None => {
                                // 从未拉起成功（缺二进制等）：按可自愈路径继续重拉
                                crate::log!("[mini-relay] ⚠️ buzz-acp 未在运行（上次拉起失败），重试");
                                acp = spawn_buzz_acp(&cfg, &agent_secret, &bridge_pubkey);
                            }
                            Some(None) => {} // 仍在跑
                            Some(Some(Ok(st))) if st.success() => {
                                crate::log!(
                                    "[mini-relay] buzz-acp 主动退出（intentional）——按 I5 不再自动重拉；需再启请重启本服务"
                                );
                                terminal = true;
                            }
                            Some(Some(Ok(st))) => {
                                crate::log!(
                                    "[mini-relay] ⚠️ buzz-acp 崩溃退出 code={:?}，重拉",
                                    st.code()
                                );
                                acp = spawn_buzz_acp(&cfg, &agent_secret, &bridge_pubkey);
                            }
                            Some(Some(Err(e))) => {
                                crate::log!("[mini-relay] ⚠️ buzz-acp 状态探测失败: {e}，重拉");
                                acp = spawn_buzz_acp(&cfg, &agent_secret, &bridge_pubkey);
                            }
                        }
                    }
                    // 句柄随本任务 drop → kill_on_drop 收子进程（关停语义即如此）
                }
            });

            // ② 回复回流（独立任务——下面的 run_server 才是本任务的常态驻留点）
            crate::tasks::tasks().spawn_forever("mini-relay-replies", async move {
                while let Some(reply) = reply_rx.recv().await {
                    match senders.get(&reply.channel_uuid) {
                        Some(msgr) => {
                            if let Err(e) = msgr.send_text(&reply.chat_id, &reply.content).await {
                                crate::log!(
                                    "[mini-relay] 回复发送失败 chat={}: {e:#}",
                                    crate::agent::truncate(&reply.chat_id, 16)
                                );
                            }
                        }
                        None => crate::log!(
                            "[mini-relay] agent 回复无对应 messenger（登记晚于启动？）chat={} len={}",
                            crate::agent::truncate(&reply.chat_id, 16),
                            reply.content.chars().count()
                        ),
                    }
                }
            });

            // ③ WS 服务。绑定失败（端口占用等）退避重试：relay 死了而 dispatch 活着
            // = 静默黑洞（bridge 照常入库/摘 pending 却无人消费），服务期持续自愈。
            let stop = crate::tasks::shutdown_token();
            loop {
                crate::log!(
                    "[mini-relay] 监听 127.0.0.1:{}（{} 个虚拟 Bot 频道）",
                    cfg.buzz_relay_port,
                    vbs.len()
                );
                match crate::buzzrelay::run_server(state.clone(), cfg.buzz_relay_port).await {
                    Ok(()) => break,
                    Err(e) => {
                        crate::log!("[mini-relay] ❌ relay 服务异常退出: {e:#}（2s 后重绑）")
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                    _ = stop.cancelled() => break,
                }
            }
        });
        Some(relay_state_for_bots)
    } else {
        None
    };

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
        // #200：每 bot 一份 relay 状态（None=未启用/初始化失败）
        let buzz_relay = buzz_relay_state.clone();
        // 任务名带 bot key（Box::leak：每次进程启动每 bot 一行小字符串，换取
        // errors/panic 告警可定位到具体 bot——审查 Minor 3）
        let name: &'static str = Box::leak(format!("bot:{}", bot.key()).into_boxed_str());
        handles.push(crate::tasks::tasks().spawn_forever(name, async move {
            run_bot(bot, cfg, msgr, router, stop, buzz_relay).await;
        }));
    }
    // 等所有 bot 循环结束。⚠️ 服务期的常态就是等在这里（等关停广播），**绝不能包超时**：
    // 健康的 bot 循环不会在固定时限内结束，超时必然触发——「启动后 30s×bot 数自动关机
    // → 看门狗按崩溃重拉」的无限重启风暴（#189 真机诊断：2 bot 60s、实验室 1 bot 30s
    // 分毫不差；#184 初版 22s 风暴与 v2.21.3「启动 61s 后二停挂死占锁」同为它的形态）。
    // #179 的 30s 关闭时限只作用于**关闭路径**（关停广播已到仍不退出的卡死 bot）：
    // 先无期限等服务期结束（关停广播或全部 bot 自行退出），进入收尾后才逐 handle 限时。
    wait_bots_or_shutdown(
        &mut handles,
        crate::tasks::shutdown_token(),
        std::time::Duration::from_secs(30),
    )
    .await;
    // #184：收尾总期限——从「bot 循环全部结束」**之后**起算，包住 shutdown_wait
    //（等 recover/session-import/larkskills 等启动期短命任务收尾；网络安装/磁盘
    // 挂死会永久挂起——2026-08-29 真机：v2.21.3 启动 61s 后二停挂死占锁）。
    // ⚠️ 期限绝不能包住上面的等 handle：服务期的常态就是「等 handle 等到关停广播」，
    // 期限从启动起算会在 20s 后把健康服务强杀成每 20s 一循环的重启风暴
    //（#184 初版真机事故，2026-08-29 同日修正）。
    // 到期 process::exit 强退：进程退出即释放 flock，看门 2s 内拉起新实例；
    // 残留任务随进程消亡（block_on 返回后 runtime drop 也会等残留任务，不能依赖它收尾）。
    if crate::tasks::tasks()
        .shutdown_wait_bounded(std::time::Duration::from_secs(20))
        .await
        .is_err()
    {
        crate::log!("[service] ⚠️ 优雅关闭超时（收尾 20s），强制退出（防挂死占锁）");
        std::process::exit(1);
    }
    crate::log!("[service] 已退出");
}

/// 等待退出信号或关停广播（#191）。返回 true=收到真实信号（调用方记「收到退出信号」
/// 日志）；false=关停序列已开始（令牌被 shutdown_wait cancel——自发起关停，signal
/// 任务让位退出，让 tracker.wait() 能清零收尾，不再恒烧满 20s 总期限强退）。
/// 信号等待抽成独立 future（跨 cfg 统一 select，避免 cfg 块语句丢值）；term/int
/// 注册失败该路永久 pending（与原实现一致，不 panic）；ctrl_c 注册失败会以 Err
/// 立即完成（原实现即如此，实践不可达）。可单测。
async fn wait_exit_signal_or_shutdown(stop: tokio_util::sync::CancellationToken) -> bool {
    #[cfg(unix)]
    let signal_fired = wait_unix_signals();
    #[cfg(not(unix))]
    // Windows 无 SIGTERM/SIGINT 语义：Ctrl+C/关闭控制台即优雅退出
    let signal_fired = tokio::signal::ctrl_c();
    tokio::select! {
        _ = signal_fired => true,
        _ = stop.cancelled() => false,
    }
}

/// unix 退出信号（TERM/INT，ctrl_c 复用 INT 注册）任一到达即完成。
#[cfg(unix)]
async fn wait_unix_signals() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).ok();
    let mut int = signal(SignalKind::interrupt()).ok();
    tokio::select! {
        _ = async { if let Some(t)=term.as_mut(){t.recv().await} else {std::future::pending().await} } => {}
        _ = async { if let Some(t)=int.as_mut(){t.recv().await} else {std::future::pending().await} } => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// 服务期等待 bot 循环：等「关停广播」或「全部 bot 循环结束」哪个先到，**无期限**。
/// 期限（per_bot_shutdown_bound）只在进入收尾（广播已到 / bot 自行退出）后才逐
/// handle 生效——#179 卡死兜底语义保留，但绝不再作用于服务期（#189：服务期加超时
/// = 健康服务 30s×bot 数后自关机 → 看门狗按崩溃重拉的重启风暴根源）。
/// 抽成独立函数 + 令牌/期限可注入，便于单测（run() 收尾是 process::exit，进程内不可测）。
async fn wait_bots_or_shutdown(
    handles: &mut [tokio::task::JoinHandle<()>],
    stop: tokio_util::sync::CancellationToken,
    per_bot_shutdown_bound: std::time::Duration,
) {
    // 服务期：逐个等 bot 循环结束，任一步可被关停广播打断。无期限。
    // 索引推进（完成即跳过）+ is_finished 守卫：JoinHandle 返回 Ready（完成性
    // poll）后再 poll 会 panic，必须保证每个 handle 至多一次完成性 poll
    //（Pending 态可多次 poll，不受限）。
    let mut i = 0;
    while i < handles.len() {
        if handles[i].is_finished() {
            i += 1;
            continue;
        }
        tokio::select! {
            _ = &mut handles[i] => { i += 1; } // 该 bot 自行退出，继续等下一个
            _ = stop.cancelled() => break,     // 关停广播 → 进入限时收尾
        }
    }
    // 进入收尾：关停广播已到（或全部 bot 自行退出，fail-open 语义不变）。
    // #179：卡死的 bot 循环（如 WS 发送挂起，历史缺陷已由 send_with_timeout 根治，
    // 这里兜底）不能无限等：逐个限时后强制继续退出流程。
    for h in handles.iter_mut() {
        if h.is_finished() {
            continue;
        }
        let _ = tokio::time::timeout(per_bot_shutdown_bound, h).await;
    }
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

/// #200：mini-relay 初始化产物（run() 分发给 dispatch/回流/子进程管理三份消费者）。
struct BuzzRelayInit {
    state: std::sync::Arc<crate::buzzrelay::RelayState>,
    reply_rx: tokio::sync::mpsc::UnboundedReceiver<crate::buzzrelay::AgentReply>,
    /// **实际生效**的 agent 私钥（config 值或生成的 buzz-agent-key）——spawn buzz-acp
    /// 必须传它而非 config 原值：config 空时传空 → buzz-acp 身份与种子成员事件（#p）
    /// 对不上 → discover_channels 找不到频道，dispatch 全链路静默失效（审查 #205）。
    agent_secret: String,
    /// 频道集来源快照（回流路由表复用同一份，两处绝不同源漂移）。
    vbs: Vec<crate::virtualbot::VirtualBot>,
}

/// 拉起 buzz-acp（含缺省路径解析）。失败返回 None（缺二进制等）只告警——
/// relay 仍运行，消息继续入库；调用方巡检任务会周期性重拉。
/// 私钥/owner 都传**生效值**（agent_secret / bridge_pubkey）：owner 必须是发布
/// 用户消息的桥身份公钥——buzz-acp respond-to=owner-only 缺 owner 即 fail-closed
/// 丢弃全部事件（审查 #205 后追问 buzz 配置面时发现的第二个断链）。
fn spawn_buzz_acp(
    cfg: &Config,
    agent_secret: &str,
    bridge_pubkey: &str,
) -> Option<crate::buzzacp::BuzzAcpProcess> {
    // I1（buzz docs/remote-agents.md「Identity fail-closed」）：**空私钥/空 owner
    // 一律拒启**——装配 harness 环境的任何一方（桌面/provider/脚本/本桥）都 MUST
    // refuse rather than launch identityless。空身份的进程是「活着但收得到全丢」的
    // 哑实例，比不启动更糟；这里宁可不拉，巡检 30s 后再试（配置修好自愈）。
    if agent_secret.trim().is_empty() || bridge_pubkey.trim().is_empty() {
        crate::log!("[mini-relay] ❌ 拒绝拉起 buzz-acp：agent 私钥或 owner 为空（I1 fail-closed）");
        return None;
    }
    let acp_exe = if cfg.buzz_acp_exe.is_empty() {
        crate::bridge_dir()
            .join("bin/buzz-acp")
            .to_string_lossy()
            .into_owned()
    } else {
        cfg.buzz_acp_exe.clone()
    };
    let agent_exe = if cfg.buzz_agent_exe.is_empty() {
        crate::bridge_dir()
            .join("bin/buzz-agent")
            .to_string_lossy()
            .into_owned()
    } else {
        cfg.buzz_agent_exe.clone()
    };
    let relay_url = format!("ws://127.0.0.1:{}", cfg.buzz_relay_port);
    match crate::buzzacp::BuzzAcpProcess::spawn(
        &acp_exe,
        &relay_url,
        agent_secret,
        &agent_exe,
        bridge_pubkey, // 作者门 owner = 桥身份公钥（见 buzzacp::acp_env）
    ) {
        Ok(p) => {
            crate::log!("[mini-relay] buzz-acp 子进程已拉起（{acp_exe} → {relay_url}）");
            Some(p)
        }
        Err(e) => {
            crate::log!(
                "[mini-relay] ⚠️ buzz-acp spawn 失败: {e}（agent 回复不可用，relay 仍运行）"
            );
            None
        }
    }
}

/// #200：mini-relay 状态初始化（buzz_relay_enabled=true 时在 bot 循环前同步执行）。
/// 密钥准备（桥身份 buzz-bridge-key / agent 身份 buzz-agent-key，缺则生成并持久化）
/// → 登记表频道集 → buzz-relay.db 打开 → RelayState（含回复回流通道）→ 种子频道事件。
/// Err = relay 不可用（磁盘/权限等）：buzz 后端随之不可用，CLI 后端不受影响。
async fn init_buzz_relay(cfg: &Config) -> Result<BuzzRelayInit, String> {
    // 桥身份密钥：存 ~/.agent-bridge/buzz-bridge-key（hex；首次生成后持久化）
    let key_path = crate::bridge_dir().join("buzz-bridge-key");
    let bridge_secret = load_or_generate_key(&key_path);
    let bridge_keys = nostr::prelude::Keys::parse(&bridge_secret).unwrap_or_else(|_| {
        // 文件损坏/非 hex：重生成并回写（写回的才是本次生效值——身份跨重启稳定）
        crate::log!("[mini-relay] ⚠️ buzz-bridge-key 解析失败，重新生成");
        let fresh = load_or_generate_key(&key_path);
        nostr::prelude::Keys::parse(&fresh).expect("新生成密钥必可解析")
    });
    // agent 身份私钥：config 指定（合法 hex）优先；缺省/非法 → 落盘文件
    // buzz-agent-key（0o600）。非法 config 值不再「每次启动临时重生成」——那会让
    // 种子成员事件跨启动堆积互相矛盾的 agent 身份（审查 #205）；告警后回落文件
    // 身份，生效值回传给 spawn（两边必须同一身份）。
    let agent_key_path = crate::bridge_dir().join("buzz-agent-key");
    let agent_secret = if !cfg.buzz_agent_private_key.is_empty()
        && nostr::prelude::Keys::parse(cfg.buzz_agent_private_key.trim()).is_ok()
    {
        cfg.buzz_agent_private_key.trim().to_string()
    } else {
        if !cfg.buzz_agent_private_key.is_empty() {
            crate::log!("[mini-relay] ⚠️ config buzz_agent_private_key 非法（非 hex 私钥），回落文件身份 buzz-agent-key");
        }
        load_or_generate_key(&agent_key_path)
    };
    let agent_keys = nostr::prelude::Keys::parse(&agent_secret).unwrap_or_else(|_| {
        // 走到这里 = 文件内容损坏（config 分支已验 isValid）→ 重生成 + 回写。
        crate::log!("[mini-relay] ⚠️ agent 密钥解析失败，重新生成");
        let fresh = load_or_generate_key(&agent_key_path);
        nostr::prelude::Keys::parse(&fresh).expect("新生成密钥必可解析")
    });
    let agent_pubkey = agent_keys.public_key().to_hex();
    // 频道集：登记的虚拟 Bot 群（uuid 派生 + chat_id + 角色名/描述）
    let vbs = crate::virtualbot::VirtualBotStore::new().load();
    let channels: Vec<crate::buzzrelay::Channel> = vbs
        .iter()
        .map(|v| crate::buzzrelay::Channel {
            uuid: crate::buzzrelay::channel_uuid(&v.bot_key, &v.chat_id),
            chat_id: v.chat_id.clone(),
            name: v.role_name.clone(),
            about: String::new(),
        })
        .collect();
    let n_channels = channels.len();
    let bridge_pubkey = bridge_keys.public_key().to_hex();
    let store = crate::buzzrelay::EventStore::open(&crate::bridge_dir().join("buzz-relay.db"))
        .await
        .map_err(|e| format!("buzz-relay.db 打开失败: {e}"))?;
    let (state, reply_rx) =
        crate::buzzrelay::RelayState::new(store, bridge_keys, agent_pubkey.clone());
    state.set_channels(channels);
    state.seed_channel_events().await;
    crate::log!(
        "[mini-relay] 初始化完成（{n_channels} 个虚拟 Bot 频道，agent={agent_pubkey:.16}…，owner 门=桥身份 {bridge_pubkey:.16}…）"
    );
    Ok(BuzzRelayInit {
        state,
        reply_rx,
        agent_secret,
        vbs,
    })
}

/// #200：读取/生成本地 hex 私钥。敏感工件统一走 atomic_write_sensitive
/// （0o600 + tmp+rename 原子替换）——裸 fs::write 会留世界可读密钥，崩溃半写
/// 直接换掉身份（仓库口径：config.json/history/sessions 同款，不再新增手写副本）。
fn load_or_generate_key(path: &std::path::Path) -> String {
    if let Ok(s) = std::fs::read_to_string(path) {
        let t = s.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    let hex = nostr::prelude::Keys::generate()
        .secret_key()
        .display_secret()
        .to_string();
    if let Err(e) = crate::atomic_write_sensitive(path, &hex) {
        crate::log!(
            "[mini-relay] ⚠️ 私钥持久化失败 {}: {e}（本次身份不落盘，重启会换）",
            path.display()
        );
    }
    hex
}

/// 单个 bot 的全套：事件循环（飞书 WS / 微信长轮询）+ 定时任务调度循环。
/// msgr 由 run() 构建并登记进跨会话投递路由表后传入（Bridge 与路由表共享同一实例）。
async fn run_bot(
    bot: crate::config::BotConfig,
    cfg: Arc<Config>,
    msgr: std::sync::Arc<dyn crate::messenger::Messenger>,
    router: std::sync::Arc<crate::deliver::Router>,
    stop: tokio_util::sync::CancellationToken,
    buzz_relay: Option<std::sync::Arc<crate::buzzrelay::RelayState>>, // #200：buzz dispatch 用
) {
    let key = bot.key();
    crate::log!(
        "[bot:{key}] 启动（kind={} name={}）",
        bot.kind,
        bot.bot_name
    );
    let bridge = {
        let mut b = Bridge::new(msgr, bot.clone(), &cfg);
        b.buzz_relay_state = buzz_relay; // #200：buzz dispatch 用；未启用/初始化失败 = None
        Arc::new(b)
    };

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
    // （claude/codex/pi）导入 history.rs——老历史参与注入接续，无需手动跑
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

    // 每日工作目录整理循环（per-bot 开关 tidy_enabled，默认关）：24h 门 + 配置热读。
    // 纯文件操作（无 agent 调用），与调度循环独立——调度跑 agent（慢）不该阻塞整理。
    // 首轮延迟 24h + jitter（盐与 session_gc 不同，错开两循环触发分钟）：
    // 启动即跑会对每个 bot 全量扫描写盘 + git 操作，不值得。
    {
        let bridge = bridge.clone();
        let key = key.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("tidy:{key}").into_boxed_str());
        crate::tasks::tasks().spawn_forever(name, async move {
            crate::log!("[tidy:{key}] 工作目录整理循环启动");
            let workspace = crate::workspace_dir(&key);
            // 上次运行标记（重启不丢 24h 门）：损坏/缺失 → 回退内存门
            let marker = workspace.join(".abb-tidy-last");
            let jitter = (fnv(&key) ^ 0xC0FFEE) % 3600;
            // 内存门起点：落盘标记优先（重启不丢 24h 门——若以 now+jitter 起门，
            // daily_due 恒取新内存门，标记形同虚设：每次重启都把下次运行推迟
            // 24h+jitter，日重启的机器维护永不触发——审查修复）；无标记（首轮）
            // → now+jitter 首轮延迟（盐与 session_gc 不同，错开两循环触发分钟）。
            let mut gate = boot_gate(read_run_marker(&marker), crate::chrono_lite::unix_secs(), jitter);
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
                    break;
                }
                let now = crate::chrono_lite::unix_secs();
                // 门已由标记种子化并随 due/运行推进（唯一写者是本任务），无需每 tick 读盘
                let due = daily_due(gate, now);
                if !due {
                    continue;
                }
                // 到点才推进门并热读配置（原来每 2s tick 全量读盘解析，改每 24h 门一次）。
                // 门推进与开关无关：关着也推进，下次到点再读——运行中打开 → 最迟 24h 跑首轮
                gate = Some(now);
                let cfg = match Config::load() {
                    Ok(c) => c,
                    Err(_) => continue, // 配置损坏 → 本周期跳过，下个门再试
                };
                let enabled = cfg
                    .bots
                    .iter()
                    .find(|b| b.key() == key)
                    .map(|b| b.tidy_enabled)
                    .unwrap_or(false);
                if !enabled {
                    continue;
                }
                let days = cfg.history_retention_days.max(1);
                // 孤儿判定依赖 live 集：现取（SessionStore::new 轻量读盘）
                let live: std::collections::HashSet<String> = {
                    let store = crate::sessions::SessionStore::new(
                        &bridge.default_backend,
                        &key,
                    );
                    store.live_session_ids("pi").into_iter().collect()
                };
                let report = crate::tidy::run_once(&workspace, now, days, &live);
                write_run_marker(&marker, now);
                crate::log!(
                    "[tidy:{key}] 整理完成：临时文件 {}，孤儿会话 {}，历史截断 {} 条，归档 {}，空目录 {}",
                    report.temp_removed,
                    report.orphan_removed,
                    report.history_truncated,
                    report.archived,
                    report.emptied_dirs
                );
                match crate::tidy::git_commit(&workspace).await {
                    Ok(crate::tidy::GitOutcome::Committed(h)) => {
                        crate::log!("[tidy:{key}] git 留痕 commit {h}")
                    }
                    Ok(crate::tidy::GitOutcome::NothingToCommit) => {
                        crate::log!("[tidy:{key}] git 无变更")
                    }
                    Ok(crate::tidy::GitOutcome::Skipped(r)) | Err(r) => {
                        crate::log!("[tidy:{key}] ⚠️ git 留痕跳过：{r}")
                    }
                }
            }
            crate::log!("[tidy:{key}] 工作目录整理循环退出");
        });
    }

    // 回收站 TTL 清理循环（#88 删除保护）：24h 门 + jitter（盐与 tidy/session_gc 错开）。
    // delete_protect 默认开 → 回收站必须定期清，否则磁盘只进不出。纯文件操作，
    // 与调度/整理循环独立；配置损坏只跳过本轮。
    {
        let key = key.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("trash-gc:{key}").into_boxed_str());
        crate::tasks::tasks().spawn_forever(name, async move {
            crate::log!("[trash-gc:{key}] 回收站 TTL 清理循环启动");
            let workspace = crate::workspace_dir(&key);
            // 上次运行标记（重启不丢 24h 门，同 tidy）
            let marker = workspace.join(".abb-trash-gc-last");
            let jitter = (fnv(&key) ^ 0x5EED) % 3600;
            let mut gate = boot_gate(
                read_run_marker(&marker),
                crate::chrono_lite::unix_secs(),
                jitter,
            );
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
                    break;
                }
                let now = crate::chrono_lite::unix_secs();
                let due = daily_due(gate, now);
                if !due {
                    continue;
                }
                // 到点才推进门并热读配置（同 tidy）
                gate = Some(now);
                let cfg = match Config::load() {
                    Ok(c) => c,
                    Err(_) => continue, // 配置损坏 → 本周期跳过，下个门再试
                };
                let settings = cfg
                    .bots
                    .iter()
                    .find(|b| b.key() == key)
                    .map(crate::trash::TrashSettings::from_bot)
                    .unwrap_or_else(crate::trash::TrashSettings::defaults);
                if !settings.enabled {
                    continue;
                }
                let purged = crate::trash::purge_expired(&workspace, settings.ttl_days);
                write_run_marker(&marker, now);
                crate::log!("[trash-gc:{key}] 回收站 TTL 清理：过期 {} 条", purged);
                if purged > 0 {
                    // 清理也是变更：git 留痕（工作区有 .git 时；tidy::git_commit 兜底跳过）
                    match crate::tidy::git_commit(&workspace).await {
                        Ok(crate::tidy::GitOutcome::Committed(h)) => {
                            crate::log!("[trash-gc:{key}] git 留痕 commit {h}")
                        }
                        Ok(crate::tidy::GitOutcome::NothingToCommit) => {}
                        Ok(crate::tidy::GitOutcome::Skipped(r)) | Err(r) => {
                            crate::log!("[trash-gc:{key}] ⚠️ git 留痕跳过：{r}")
                        }
                    }
                }
            }
            crate::log!("[trash-gc:{key}] 回收站 TTL 清理循环退出");
        });
    }

    // 每日会话归纳清理循环（全局开关 session_gc_enabled，默认关）：24h 门 + 配置热读。
    // 过期会话（最后活跃超 session_gc_days）交 bot 后端 agent 归纳成摘要存档
    //（summaries/），再清理工作区内历史/后端会话文件（绝不触碰 ~/.claude 等后端
    // 私有目录），摘要下次会话注入衔接上下文。破坏性 + 每会话一次 LLM 调用，故
    // 默认关、首轮延迟 24h+jitter（盐与 tidy 不同——tidy 用 ^0xC0FFEE，错开两循环
    // 触发分钟，避免同刻全 bot 全量扫描 + 归纳同时开跑）。
    {
        let bridge = bridge.clone();
        let key = key.clone();
        let stop = stop.clone();
        let name: &'static str = Box::leak(format!("session-gc:{key}").into_boxed_str());
        crate::tasks::tasks().spawn_forever(name, async move {
            crate::log!("[session-gc:{key}] 会话归纳清理循环启动");
            let workspace = crate::workspace_dir(&key);
            // 上次运行标记（重启不丢 24h 门）：损坏/缺失 → 回退内存门
            let marker = workspace.join(".abb-session-gc-last");
            let jitter = fnv(&key) % 3600;
            // 内存门起点：落盘标记优先（重启不丢 24h 门，同上 tidy 循环——审查修复）；
            // 无标记（首轮）→ now+jitter 首轮延迟（盐与 tidy 不同——tidy 用 ^0xC0FFEE，
            // 错开两循环触发分钟，避免同刻全 bot 全量扫描 + 归纳同时开跑）
            let mut gate = boot_gate(
                read_run_marker(&marker),
                crate::chrono_lite::unix_secs(),
                jitter,
            );
            loop {
                if interruptible_sleep(std::time::Duration::from_secs(2), &stop).await {
                    break;
                }
                let now = crate::chrono_lite::unix_secs();
                // 门已由标记种子化并随 due/运行推进（唯一写者是本任务），无需每 tick 读盘
                let due = daily_due(gate, now);
                if !due {
                    continue;
                }
                // 到点才推进门并热读配置（原来每 2s tick 全量读盘解析，改每 24h 门一次）；
                // 门推进与开关无关——关着也推进，运行中打开 → 最迟 24h 跑首轮
                gate = Some(now);
                let cfg = match Config::load() {
                    Ok(c) => c,
                    Err(_) => continue, // 配置损坏 → 本周期跳过，下个门再试
                };
                if !cfg.session_gc_enabled {
                    continue;
                }
                // 跑前再确认关停：每会话一次 LLM 调用，可能很慢（run_once 内部每会话也查）
                if stop.is_cancelled() {
                    break;
                }
                let report = crate::session_gc::run_once(&bridge, &stop).await;
                // 归纳被关停打断（run_once 内每 chat 前检查 stop）→ 不写运行标记：
                // 中断轮不得记成完成轮，否则未归纳的会话要再等一个 24h 门才重试
                //（审查修复）；完整跑完（含提前 break 的 stop 已在 run_once 内兜住）才落盘
                if !stop.is_cancelled() {
                    write_run_marker(&marker, now);
                }
                crate::log!(
                    "[session-gc:{key}] 归纳完成：成功 {}，失败 {}，跳过 {}",
                    report.summarized,
                    report.failed,
                    report.skipped
                );
            }
            crate::log!("[session-gc:{key}] 会话归纳清理循环退出");
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

/// FNV-1a 64 位哈希：per-bot 维护循环（tidy/session_gc）的首轮 jitter 用。
/// 进程内一致即可，无需跨进程稳定（std hash 不保证跨版本稳定，自写最稳）。
fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 维护循环 24h 门判定：自上次运行（门=落盘标记种子）满 24h 即到点。
/// 门缺失（仅理论：boot_gate 恒有值）→ 到点。
fn daily_due(gate: Option<u64>, now: u64) -> bool {
    match gate {
        Some(t) => now.saturating_sub(t) >= 24 * 3600,
        None => true,
    }
}

/// 维护循环内存门起点：落盘标记优先（重启不丢 24h 门——门以「上次实际运行」为起点，
/// 重启/停用期间照常推进，不会每 tick 判定到点 + 全量读配置）；无标记（首轮）→
/// now+jitter 首轮延迟。调用方每次到点把门推进到 now（与落盘标记同步）。
fn boot_gate(marker_ts: Option<u64>, now: u64, jitter: u64) -> Option<u64> {
    marker_ts.or(Some(now + jitter))
}

/// 维护循环 24h 门落盘标记：读上次运行时间戳（unix_secs 十进制），进程重启不丢进度。
/// 缺失/损坏 → None（调用方回退内存门，首轮仍延迟 24h+jitter）。
fn read_run_marker(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 维护循环运行标记落盘（best-effort：失败只 log——重启后 24h 门重算，
/// run_once 幂等，重复跑一轮无害）。
fn write_run_marker(path: &std::path::Path, ts: u64) {
    if let Err(e) = std::fs::write(path, ts.to_string()) {
        crate::log!("⚠️ 写运行标记 {} 失败：{e}", path.display());
    }
}

/// 组装定时任务的 prompt：三级 AGENTS.md 指令文件块（abb → bot → session，session_key
/// 用 job.chat_id，与下方 agent::run 的 session_key 一致）+ 受限分支前置受限说明。
/// 抽成纯函数便于测试（run_job 的 agent 调用不可测，prompt 组装可测）。
fn job_prompt(job: &crate::schedule::Job, bot_key: &str) -> String {
    let agents_block = crate::agents_md::collect_block(bot_key, &job.chat_id);
    // 授权者建的任务在受限分支执行：prompt 前置与聊天路径一致的受限说明
    //（否则模型不知道自己被闸着，越界被拒后瞎试）。与 agent::run 同源公共判定
    // config::restrict_granted（role==Granted && 开关热读）。受限说明必须最外层。
    let body = if agents_block.is_empty() {
        job.prompt.clone()
    } else {
        format!("{agents_block}\n\n{}", job.prompt)
    };
    if crate::config::restrict_granted(job.role, bot_key) {
        // 受限说明与聊天路径同源（config::RESTRICT_PREAMBLE，文本/次序单一来源）
        format!("{}{body}", crate::config::RESTRICT_PREAMBLE)
    } else {
        body
    }
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
    // #87 暂停拦截：任务目标会话已暂停 → 跳过本轮（不消耗 LLM）。
    // 多目标任务的目标侧由 Router::deliver 的暂停检查兜底（回源提示）；
    // 这里拦的是旧路径「先发原会话」——原会话已暂停则本轮无意义。
    if bridge.session_state.is_paused(&bot_key, &job.chat_id) {
        crate::log!(
            "[bot:{bot_key}] 任务 {} 跳过：目标会话已暂停（#87）chat={}",
            &job.id[..job.id.len().min(8)],
            &job.chat_id[..job.chat_id.len().min(16)]
        );
        crate::session_state::audit("job.skip", &bot_key, &job.chat_id, "scheduler", "paused");
        if job.kind == crate::schedule::JobKind::Once {
            bridge.jobs.remove(&job.id);
        }
        return;
    }
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
    // 授权者建的任务在受限分支执行：prompt 前置与聊天路径一致的受限说明 + 三级
    // AGENTS.md 指令文件块（组装抽成 job_prompt 纯函数，可测）。
    let prompt = job_prompt(&job, &bot_key);
    // 定时任务可被「停止词」打断（#卡死修复）：注册到目标会话的 cancel 标志，
    // 用户在该会话发 停/停止/cancel 即可终止正在跑的后台任务；
    // 与聊天任务共用同一 key（chat_id）——同一 chat 同一时刻只有一个在跑任务。
    let cancel_flag = bridge.register_cancel_flag(&job.chat_id);
    let reply = match crate::agent::run(
        backend,
        &prompt,
        &uuid::Uuid::new_v4().to_string(), // 每次全新 session，不带聊天上下文
        false,
        &job.chat_id,
        &job.chat_id, // session_key：sessions=None 时仅回存分支用不到，占位保持一致
        &bot_key,
        job.role,                  // 按创建者角色执行：授权者建的任务走受限分支
        None, // claude/pi 无需回存 thread_id（只有 codex 要回存真实 thread_id）
        None, // 定时任务不推中间进度（统一只发最终结果）
        Some(cancel_flag.clone()), // 定时任务可被用户停止词打断
    )
    .await
    {
        Ok(crate::agent::RunOutcome::Reply { reply, .. }) => reply,
        Ok(crate::agent::RunOutcome::Cancelled) => "⏰ 任务被中断".to_string(),
        Err(e) => format!("⏰ 定时任务执行失败：{e}"),
    };
    bridge.unregister_cancel_flag(&job.chat_id);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #189 回归护栏（服务期无期限）：健康 bot 循环（不自行结束、无关停广播）期间，
    /// 等待绝不能提前返回进入关闭路径——旧实现把 30s 逐 handle 时限放在服务期，
    /// 健康服务 30s×bot 数后必然自关停（真机 2 bot=60s、实验室 1 bot=30s 重启风暴）。
    /// 探针期内不返回即「无期限」。
    #[tokio::test]
    async fn wait_bots_or_shutdown_never_times_out_on_healthy_bots() {
        let mut handles = vec![tokio::spawn(async {
            // 健康 bot：正常服务期不结束（300s 远超探针）
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        })];
        let stop = tokio_util::sync::CancellationToken::new(); // 无广播
        let r = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            wait_bots_or_shutdown(&mut handles, stop, std::time::Duration::from_millis(50)),
        )
        .await;
        assert!(
            r.is_err(),
            "健康 bot 期间服务期等待必须无期限（探针超时=正确，提前返回=重启风暴回归）"
        );
    }

    /// #189 回归护栏（关闭路径限时）：关停广播到达后，卡死的 bot 循环仍被逐 handle
    /// 时限兜底（#179 语义保留）——总耗时受 bound 支配，不得无限等。
    #[tokio::test]
    async fn wait_bots_or_shutdown_bounds_stuck_bot_after_cancel() {
        let mut handles = vec![tokio::spawn(async {
            // 卡死 bot：关停广播也不退出
            std::future::pending::<()>().await;
        })];
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel(); // 关停广播已到
        let t = std::time::Instant::now();
        wait_bots_or_shutdown(&mut handles, stop, std::time::Duration::from_millis(100)).await;
        let elapsed = t.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "关停后卡死 bot 必须被逐 handle 时限兜底，实际 {elapsed:?}"
        );
    }

    /// #189 回归护栏（fail-open 保留）：全部 bot 循环自行退出（如微信会话过期 -14）
    /// 时，即使无关停广播也进入收尾——与 v2.21.2 原语义一致（循环退出 → 进程优雅
    /// 退出 → 看门狗重启）。
    #[tokio::test]
    async fn wait_bots_or_shutdown_returns_when_all_bots_exit() {
        let mut handles: Vec<tokio::task::JoinHandle<()>> = vec![tokio::spawn(async {})]; // bot 立即退出
        let stop = tokio_util::sync::CancellationToken::new(); // 无广播
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            wait_bots_or_shutdown(&mut handles, stop, std::time::Duration::from_secs(30)),
        )
        .await;
        assert!(
            r.is_ok(),
            "全部 bot 自行退出后必须进入收尾（fail-open），不得无限等广播"
        );
    }

    /// #189 补充（独立审查 P3）：多 bot 顺序推进 + 迟到广播的混合形态——已完成
    /// handle 在服务期被 select 消费后，广播到达、尾段必须跳过它（不二次 poll
    /// panic），同时对未完成 handle 限时。删尾段 is_finished 守卫此测试必红。
    #[tokio::test]
    async fn wait_bots_or_shutdown_mixed_bots_with_late_cancel() {
        let mut handles = vec![
            tokio::spawn(async {}), // 立即退出的 bot：服务期被完成性消费
            tokio::spawn(async {
                // 卡死 bot：广播后由尾段限时兜底
                std::future::pending::<()>().await;
            }),
        ];
        let stop = tokio_util::sync::CancellationToken::new();
        {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                stop.cancel();
            });
        }
        let t = std::time::Instant::now();
        wait_bots_or_shutdown(&mut handles, stop, std::time::Duration::from_millis(100)).await;
        let elapsed = t.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "混合形态（已完成+卡死+迟到广播）不得 panic 且须受限时支配，实际 {elapsed:?}"
        );
    }

    /// 三个信号测试的进程内串行锁（独立审查 F1）：SIGTERM 发给整个进程，并行时
    /// 可能被其他信号测试 helper 的注册/parked 窗口消费致偶发 flake，串行化消除。
    static SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// #191 回归护栏（信号路径）：SIGTERM 到达返回 true（真实信号，调用方记日志）。
    /// 给测试进程自己发 SIGTERM——tokio 信号处理器已捕获，进程不死，仅本测试的
    /// helper 收到事件。三个信号测试共享 SIGNAL_TEST_LOCK 串行化（独立审查 F1）：
    /// 并行时发出的 SIGTERM 可能被别的测试 helper 的注册/parked 窗口消费致偶发 flake。
    /// 仅 unix：libc::kill 是 unix API（Windows CI 编译炸，#192 教训；false 路径
    /// 两测跨平台仍跑）。
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 串行锁必须横跨注册→发信号全程（F1），lint 会让位于测试目的
    async fn wait_exit_signal_or_shutdown_returns_true_on_sigterm() {
        let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stop = tokio_util::sync::CancellationToken::new(); // 无关停广播
        let task = tokio::spawn(wait_exit_signal_or_shutdown(stop));
        // 等 helper 完成信号注册（首次 poll 安装处理器），再给进程发 SIGTERM
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
            task.await.unwrap()
        })
        .await;
        assert!(r == Ok(true), "SIGTERM 必须返回 true（实际 {r:?}）");
    }

    /// #191 回归护栏（让位路径·先取消）：关停令牌已取消 → 立即返回 false
    ///（自发起关停，不记信号日志）。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 串行锁必须横跨注册→发信号全程（F1），lint 会让位于测试目的
    async fn wait_exit_signal_or_shutdown_returns_false_when_already_cancelled() {
        let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            wait_exit_signal_or_shutdown(stop),
        )
        .await;
        assert!(
            r == Ok(false),
            "已取消的令牌必须立即返回 false（实际 {r:?}）"
        );
    }

    /// #191 回归护栏（让位路径·后取消）：helper 先 parked 等信号，关停令牌后到仍能
    /// 唤醒返回 false——这正是自发起关停时 tracker.wait() 不再永等的前提
    ///（旧实现无此分支，parked 至烧满 20s 总期限强退）。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 串行锁必须横跨注册→发信号全程（F1），lint 会让位于测试目的
    async fn wait_exit_signal_or_shutdown_returns_false_on_late_cancel() {
        let _serial = SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stop = tokio_util::sync::CancellationToken::new();
        {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                stop.cancel();
            });
        }
        let t = std::time::Instant::now();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            wait_exit_signal_or_shutdown(stop),
        )
        .await;
        assert!(r == Ok(false), "迟到取消必须返回 false（实际 {r:?}）");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(1),
            "取消后必须立即唤醒，实际 {:?}",
            t.elapsed()
        );
    }

    fn test_job(
        prompt: &str,
        chat_id: &str,
        role: crate::config::SenderRole,
    ) -> crate::schedule::Job {
        crate::schedule::Job {
            id: uuid::Uuid::new_v4().to_string(),
            kind: crate::schedule::JobKind::Once,
            schedule: String::new(),
            prompt: prompt.to_string(),
            chat_id: chat_id.to_string(),
            note: String::new(),
            targets: Vec::new(),
            role,
        }
    }

    #[test]
    fn daily_due_gate_semantics() {
        // 门语义：满 24h 即到点；门缺失 → 到点（理论路径，boot_gate 恒有值）
        let now = 1_000_000_000u64;
        let h24 = 24 * 3600u64;
        assert!(daily_due(None, now));
        // 门未满 24h → 未到点（首轮延迟）
        assert!(!daily_due(Some(now - 1000), now));
        assert!(daily_due(Some(now - h24), now));
        // 恰好满 24h 边界
        assert!(daily_due(Some(now - h24 - 1), now));
        assert!(!daily_due(Some(now - h24 + 1), now));
        // 功能关闭期间门照常推进（due 分支置 gate=now）→ 关着也每 24h 才醒一次，不 spin
        assert!(!daily_due(Some(now), now));
    }

    #[test]
    fn boot_gate_restart_keeps_marker_cadence() {
        // 回归护栏（审查修复）：原实现在循环里以 now+jitter 起内存门、daily_due 取
        // 标记与门的较晚者——重启后门（未来时间戳）恒胜标记，每次重启都把下次运行
        // 推迟 24h+jitter，日重启的机器维护永不触发。修复后门以落盘标记为种子。
        let now = 1_000_000_000u64;
        let jitter = 1800u64;
        let h24 = 24 * 3600u64;
        // 重启时距上次运行不足 24h → 门=标记 → 未到点（重启不丢 24h 门）
        let gate = boot_gate(Some(now - 3600), now, jitter);
        assert!(!daily_due(gate, now), "重启后 1h 未到点");
        // 重启时已超 24h（停机超过一天）→ 门=标记 → 到点（补跑维护）
        let gate = boot_gate(Some(now - 50 * 3600), now, jitter);
        assert!(daily_due(gate, now), "停机两天重启即补跑");
        // 无标记（首轮）→ now+jitter 首轮延迟
        let gate = boot_gate(None, now, jitter);
        assert!(!daily_due(gate, now));
        assert!(
            daily_due(gate, now + h24 + jitter),
            "首轮满 24h+jitter 到点"
        );
        // 标记损坏/缺失 → 回退首轮延迟（与无标记一致）
        assert_eq!(boot_gate(None, now, jitter), boot_gate(None, now, jitter));
        let gate = boot_gate(None, now, 0);
        assert_eq!(gate, Some(now), "jitter=0 时门=now");
    }

    #[test]
    fn job_prompt_restricted_preamble_then_agents_md() {
        // 受限任务：受限说明必须最外层，其后是三级 AGENTS.md 块（bot 级按唯一 key 可控）
        let bot_key = format!("abb-jobtest-{}", uuid::Uuid::new_v4());
        let ws = crate::workspace_dir(&bot_key);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "定时任务 bot 规则").unwrap();
        let job = test_job("提醒我喝水", "oc_job", crate::config::SenderRole::Granted);
        let p = job_prompt(&job, &bot_key);
        assert!(p.starts_with("[受限模式]"), "受限说明必须最外层: {p}");
        let i_a = p.find("[指令文件]").unwrap();
        let i_b = p.find("定时任务 bot 规则").unwrap();
        let i_t = p.find("提醒我喝水").unwrap();
        assert!(
            i_a < i_b && i_b < i_t,
            "顺序：受限 > 指令文件 > 用户 prompt"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn job_prompt_owner_includes_agents_md_if_present() {
        // 非受限：有指令文件（bot 级）→ [指令文件] 块在用户 prompt 之前、无受限说明。
        // abb 级读真实 ~/.agent-bridge/AGENTS.md（用户机器上可能已建本功能文件），
        // 故不依赖它不存在——只断言结构，不断言块头精确位置。
        let bot_key = format!("abb-jobtest-{}", uuid::Uuid::new_v4());
        let ws = crate::workspace_dir(&bot_key);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "bot 规则").unwrap();
        let job = test_job("简单提醒", "oc_job2", crate::config::SenderRole::Owner);
        let p = job_prompt(&job, &bot_key);
        assert!(!p.starts_with("[受限模式]"), "owner 任务无受限说明");
        let i_a = p.find("[指令文件]");
        let i_t = p.find("简单提醒").unwrap();
        assert!(i_a.is_some(), "bot 级文件存在应注入块");
        assert!(i_a.unwrap() < i_t, "指令文件在用户 prompt 之前");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn job_prompt_owner_without_files_keeps_prompt() {
        // 非受限 + 唯一 key 无 bot/会话级文件（abb 级不可控：可能已存在）：
        // 只断言用户 prompt 出现、无受限说明；块的有无取决于 abb 级文件，不强断。
        let bot_key = format!("abb-jobtest-{}", uuid::Uuid::new_v4());
        let job = test_job("简单提醒", "oc_job3", crate::config::SenderRole::Owner);
        let p = job_prompt(&job, &bot_key);
        assert!(p.contains("简单提醒"));
        assert!(!p.starts_with("[受限模式]"));
    }
}
