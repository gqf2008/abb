// ABB — Rust + Slint 单二进制双模式
//   agent-bridge            → 托盘控制器（Slint GUI）
//   agent-bridge --service  → 无头桥守护进程（纯 tokio，LaunchAgent 跑）

// Windows：托盘 GUI 程序，不带控制台窗口（stdout/stderr 仍可被重定向到文件/管道）。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod agent;
mod attachments;
mod botstatus;
mod bridge;
mod ccswitch;
mod config;
mod deliver;
mod deps;
mod dingtalk;
mod feishu;
mod install;
mod larkskills;
mod messenger;
mod outbox;
mod permreq;
mod platform;
mod proto;
mod schedule;
mod service;
mod sessions;
mod single_instance;
mod ui;
mod wechat;
mod ws;

/// 运行时数据目录：~/.agent-bridge（隐藏目录，与 ~/.claude 同款）。
/// 老路径 ~/feishu-bridge 由 platform::migrate_to_agent_bridge() 一次性迁移过来（main 启动时跑）。
pub fn bridge_dir() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-bridge")
}

/// 某 bot 的工作目录：~/.agent-bridge/workspaces/<bot_key>/。约定 agent 只在此读写。
/// 多 bot 相互隔离——每个 bot 独立工作目录。
pub fn workspace_dir(bot_key: &str) -> std::path::PathBuf {
    bridge_dir().join("workspaces").join(bot_key)
}

/// 原子写文本文件（tmp + rename）。config/sessions/jobs/botstatus 共用，避免崩溃留半截。
pub fn atomic_write_text(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 统一日志到 stdout（带时间戳，与 Python 版一致，落 logs/bridge.out）。
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        // windows_subsystem=windows 下无控制台时 stdout 句柄无效：println! 会 panic，
        // 这里用 write_fmt + 忽略错误，日志在无控制台时静默丢弃，重定向时照常落盘。
        let _ = std::io::Write::write_fmt(
            &mut std::io::stdout(),
            format_args!("[{}] {}\n", $crate::chrono_lite::now(), format!($($arg)*)),
        );
    }};
}

/// 零依赖时间戳（避免引入 chrono）
pub mod chrono_lite {
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 距 UNIX 纪元的秒数（botstatus 心跳等用）。
    pub fn unix_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    pub fn now() -> String {
        let secs = unix_secs();
        // 本地时区偏移（macOS 默认 Asia/Shanghai UTC+8 由系统 localtime 决定；
        // 这里简化用 UTC+8，够日志用。要精确可调 libc localtime，但不引依赖。）
        let local = secs + 8 * 3600;
        let (y, mo, d, h, mi, s) = epoch_to_ymd(local);
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
    }
    fn epoch_to_ymd(t: u64) -> (u64, u64, u64, u64, u64, u64) {
        let s = t % 60;
        let mi = (t / 60) % 60;
        let h = (t / 3600) % 24;
        let mut days = t / 86400;
        // 从 1970-01-01 起算
        let mut y = 1970;
        loop {
            let dy = if is_leap(y) { 366 } else { 365 };
            if days >= dy {
                days -= dy;
                y += 1;
            } else {
                break;
            }
        }
        let mdays = [
            31u64,
            if is_leap(y) { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        let mut mo = 1;
        for md in mdays {
            if days >= md {
                days -= md;
                mo += 1;
            } else {
                break;
            }
        }
        (y, mo, days + 1, h, mi, s)
    }
    fn is_leap(y: u64) -> bool {
        (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
    }
}

fn main() {
    // 改名一次性迁移（feishu-bridge → agent-bridge，~/feishu-bridge → ~/.agent-bridge）。
    // 必须在最顶：args 解析、单实例加锁、job CLI 读 config/jobs 都依赖数据已在新位置。幂等。
    platform::migrate_to_agent_bridge();

    let args: Vec<String> = std::env::args().collect();

    // 权限请求（GUI「请求权限」按钮拉起）：逐项触发屏幕录制/摄像头/麦克风授权弹框。
    // 独立子进程跑（不阻塞托盘），逐行打日志，GUI 逐行读进设置窗状态区。
    #[cfg(target_os = "macos")]
    if args.iter().any(|a| a == "--request-permissions") {
        permreq::request_media_permissions();
        return;
    }

    // 隐藏诊断：打印当前二进制六项系统权限的真实检测态（验证 API 检测是否反映系统设置）。
    if args.iter().any(|a| a == "--dump-perms") {
        for p in deps::detect_permissions() {
            println!("{}\t{:?}", p.id, p.state);
        }
        return;
    }

    if args.iter().any(|a| a == "--service") {
        // 单实例：已有一个 --service 在跑就直接退出（flock 拿不到锁）
        let _guard = match single_instance::SingleInstance::acquire("service") {
            Ok(g) => g,
            Err(e) => {
                crate::log!("{e:#}");
                std::process::exit(0); // 优雅退出，不报错（避免 launchd KeepAlive 刷屏重试日志）
            }
        };
        // 守护进程：纯 tokio
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(service::run());
        return;
    }

    // 定时任务 CLI：供 claude 用 Bash 调用（也可人用）。
    //   agent-bridge job list
    //   agent-bridge job del <id前缀>
    //   agent-bridge job add (--once "YYYY-MM-DD HH:MM" | --cron "分 时 日 月 周") --prompt "做什么" [--note "原句"]
    // chat_id 从 AGENT_BRIDGE_CHAT_ID env 读（桥 spawn claude 时注入），缺省回落主会话。
    if args.len() >= 2 && args[1] == "job" {
        std::process::exit(run_job_cli(&args[2..]));
    }

    // 跨会话投递 CLI：供 claude 用 Bash 调用（也可人用）。
    //   agent-bridge deliver --bot <目标bot key> --chat <目标chat_id> --text "内容"
    //   [--source-bot <来源bot key> --source-chat <来源chat_id>]（缺省取桥注入的 env）
    // 总开关：Config.cross_delivery_enabled（设置 → 「跨会话投递」勾选），关闭时拒绝。
    if args.len() >= 2 && args[1] == "deliver" {
        std::process::exit(run_deliver_cli(&args[2..]));
    }

    // 隐藏调试 flag：--wx-qr-test（冒烟：真拉一次微信登录二维码，验证协议端点）
    if args.iter().any(|a| a == "--wx-qr-test") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            match wechat::fetch_qrcode().await {
                Ok((qr, img)) => {
                    println!(
                        "OK qrcode_len={} qrcode_head={:?}",
                        qr.len(),
                        &qr[..qr.len().min(24)]
                    );
                    println!(
                        "img_len={} img_head={:?}",
                        img.len(),
                        &img[..img.len().min(80)]
                    );
                    match wechat::save_qrcode_image("smoke", &img) {
                        Ok(p) => println!("saved → {}", p.display()),
                        Err(e) => println!("save err: {e:#}"),
                    }
                }
                Err(e) => println!("ERR: {e:#}"),
            }
        });
        return;
    }

    // 隐藏调试 flag：--dump-config / --fetch-bot-info（P2/P3 验证用）
    if args.iter().any(|a| a == "--dump-config") {
        match config::Config::load() {
            Ok(c) => {
                println!(
                    "owner={} default_backend={} bot数={} missing={:?}",
                    c.owner_open_id,
                    c.default_backend,
                    c.bots.len(),
                    c.missing()
                );
                for b in &c.bots {
                    println!(
                        "  bot[{}] app_id={} name={} open_id={} primary={}",
                        b.key(),
                        b.app_id,
                        b.bot_name,
                        b.bot_open_id,
                        b.primary_chat_id
                    );
                }
            }
            Err(e) => crate::log!("config 读取失败: {e:#}"),
        }
        return;
    }
    if args.iter().any(|a| a == "--fetch-bot-info") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            match config::Config::load() {
                Ok(c) => {
                    for b in &c.bots {
                        let fs = feishu::FeishuClient::new(&b.app_id, &b.app_secret);
                        match fs.bot_info().await {
                            Ok((name, oid)) => {
                                println!("[{}] bot_name={name} bot_open_id={oid}", b.key())
                            }
                            Err(e) => crate::log!("[{}] bot_info 失败: {e:#}", b.key()),
                        }
                    }
                }
                Err(e) => crate::log!("config 读取失败: {e:#}"),
            }
        });
        return;
    }

    // GUI：托盘控制器（单实例：已在跑就不再开一个托盘）
    if args.iter().any(|a| a == "--diag-tray") {
        diag_tray_image();
        return;
    }
    let _gui_guard = match single_instance::SingleInstance::acquire("gui") {
        Ok(g) => g,
        Err(e) => {
            crate::log!("{e:#}");
            std::process::exit(0);
        }
    };
    if let Err(e) = ui::run_gui() {
        crate::log!("GUI 启动失败: {e:#}");
        std::process::exit(1);
    }
}

// 隐藏调试 flag：诊断托盘图像数据
#[allow(dead_code)]
fn diag_tray_image() {
    for f in ["tray-dark.png", "tray-light.png"] {
        let p = std::path::Path::new("/Users/sqb/feishu-bridge-rs/app-assets").join(f);
        match slint::Image::load_from_path(&p) {
            Ok(img) => match img.to_rgba8() {
                Some(b) => {
                    let bytes = b.as_bytes();
                    let nonzero = bytes.chunks(4).filter(|c| c[3] > 0).count();
                    println!(
                        "{f}: {}x{} bytes={} 期望={} 不透明像素={}",
                        b.width(),
                        b.height(),
                        bytes.len(),
                        b.width() * b.height() * 4,
                        nonzero
                    );
                }
                None => println!("{f}: to_rgba8 None"),
            },
            Err(e) => println!("{f}: load err {e:?}"),
        }
    }
}

/// 定时任务 CLI（供 claude 用 Bash 调用，也可人用）。退出码 0=成功 1=失败。
/// bot 解析：AGENT_BRIDGE_BOT_KEY env（桥 spawn claude 时注入）→ 唯一 bot → 报错。
/// chat_id 解析：AGENT_BRIDGE_CHAT_ID env → 该 bot 主会话。
fn run_job_cli(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    // 确定目标 bot
    let bot_key = match resolve_bot_key() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let store = schedule::JobStore::new(&bot_key);
    match sub {
        "list" => {
            let jobs = store.list();
            if jobs.is_empty() {
                println!("（还没有定时任务）");
            } else {
                for j in &jobs {
                    println!("{}", j.describe());
                }
            }
            0
        }
        "del" => {
            let prefix = args.get(1).map(|s| s.trim()).unwrap_or("");
            if prefix.is_empty() {
                eprintln!("用法：agent-bridge job del <id前缀>（用 job list 查看 id）");
                return 1;
            }
            let jobs = store.list();
            let hit: Vec<_> = jobs.iter().filter(|j| j.id.starts_with(prefix)).collect();
            match hit.len() {
                0 => {
                    eprintln!("没找到 id 以「{prefix}」开头的任务");
                    1
                }
                1 => {
                    let desc = hit[0].describe();
                    store.remove(&hit[0].id);
                    println!("已删除：{desc}");
                    0
                }
                n => {
                    eprintln!("「{prefix}」匹配到 {n} 个任务，请给更长的 id 前缀");
                    1
                }
            }
        }
        "add" => {
            let mut once: Option<String> = None;
            let mut cron: Option<String> = None;
            let mut prompt: Option<String> = None;
            let mut note: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                let flag = args[i].as_str();
                let val = args.get(i + 1).map(|s| s.as_str());
                match flag {
                    "--once" => {
                        once = val.map(|s| s.to_string());
                        i += 2;
                    }
                    "--cron" => {
                        cron = val.map(|s| s.to_string());
                        i += 2;
                    }
                    "--prompt" => {
                        prompt = val.map(|s| s.to_string());
                        i += 2;
                    }
                    "--note" => {
                        note = val.map(|s| s.to_string());
                        i += 2;
                    }
                    other => {
                        eprintln!("未知参数：{other}");
                        return 1;
                    }
                }
            }
            let prompt = match prompt {
                Some(p) if !p.trim().is_empty() => p,
                _ => {
                    eprintln!("缺 --prompt（到点要做什么）");
                    return 1;
                }
            };
            let (kind, time_arg, cron_arg) = match (once, cron) {
                (Some(t), None) => ("once", Some(t), None),
                (None, Some(c)) => ("cron", None, Some(c)),
                _ => {
                    eprintln!("--once 和 --cron 必须二选一（且只给一个）");
                    return 1;
                }
            };
            // chat_id：优先 env（桥注入），否则回落该 bot 主会话
            let chat_id = std::env::var("AGENT_BRIDGE_CHAT_ID")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| config::Config::primary_chat(&bot_key));
            if chat_id.is_empty() {
                eprintln!("无法确定 chat_id：AGENT_BRIDGE_CHAT_ID 为空且主会话未建立（先在飞书私聊 bot 发一句话）");
                return 1;
            }
            let note = note.unwrap_or_else(|| prompt.clone());
            match schedule::job_from_parsed(
                kind,
                time_arg.as_deref(),
                cron_arg.as_deref(),
                &prompt,
                &chat_id,
                &note,
            ) {
                Ok(job) => {
                    let desc = job.describe();
                    store.add(job);
                    println!("⏰ 定时任务已创建：{desc}");
                    0
                }
                Err(e) => {
                    eprintln!("没建成定时任务：{e:#}");
                    1
                }
            }
        }
        _ => {
            eprintln!(
                "用法：\n  agent-bridge job list\n  agent-bridge job del <id前缀>\n  agent-bridge job add (--once \"YYYY-MM-DD HH:MM\" | --cron \"分 时 日 月 周\") --prompt \"做什么\" [--note \"原句\"]"
            );
            1
        }
    }
}

/// 解析 job CLI 的目标 bot：AGENT_BRIDGE_BOT_KEY env → 唯一 bot → 报错提示。
fn resolve_bot_key() -> Result<String, String> {
    if let Ok(k) = std::env::var("AGENT_BRIDGE_BOT_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    let cfg = config::Config::load().map_err(|e| format!("读 config 失败: {e:#}"))?;
    match cfg.bots.len() {
        0 => Err("config.json 没有配置任何 bot".into()),
        1 => Ok(cfg.bots[0].key()),
        n => Err(format!(
            "有 {n} 个 bot 但未指定目标（桥正常调用会注入 AGENT_BRIDGE_BOT_KEY；手动用请设该环境变量为某个 bot 的 name）"
        )),
    }
}

/// 跨会话投递 CLI（供 claude 用 Bash 调用，也可人用）。退出码 0=已入队 1=失败。
/// 来源缺省取 AGENT_BRIDGE_BOT_KEY / AGENT_BRIDGE_CHAT_ID（桥 spawn agent 时注入）。
/// 投递是异步的：CLI 只负责校验 + 入队，service 侧投递循环实际发送。
fn run_deliver_cli(args: &[String]) -> i32 {
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    if !cfg.cross_delivery_enabled {
        eprintln!("跨会话投递未开启：请在 ABB 设置里勾选「跨会话投递」后重试（保存即重启服务）。");
        return 1;
    }
    let env_bot = std::env::var("AGENT_BRIDGE_BOT_KEY").unwrap_or_default();
    let env_chat = std::env::var("AGENT_BRIDGE_CHAT_ID").unwrap_or_default();
    let item = match deliver::parse_deliver_args(args, &env_bot, &env_chat) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}\n用法：agent-bridge deliver --bot <目标bot key> --chat <目标chat_id> --text \"内容\"");
            return 1;
        }
    };
    // 目标 bot 必须真实存在（与 service 路由表同源：config.bots[].key()）
    if !cfg.bots.iter().any(|b| b.key() == item.target_bot) {
        let keys: Vec<String> = cfg.bots.iter().map(|b| b.key()).collect();
        eprintln!(
            "目标 bot「{}」不存在。当前 bot：{}",
            item.target_bot,
            if keys.is_empty() {
                "（无）".to_string()
            } else {
                keys.join(", ")
            }
        );
        return 1;
    }
    let store = deliver::DeliveryStore::new();
    store.add(item);
    crate::log!("[deliver] CLI 已入队投递（service 异步发送）");
    println!("✅ 已入队跨会话投递（由 service 异步发送）。");
    0
}
