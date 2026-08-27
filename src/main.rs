// ABB — Rust + Slint 单二进制双模式
//   agent-bridge            → 托盘控制器（Slint GUI）
//   agent-bridge --service  → 无头桥守护进程（纯 tokio，LaunchAgent 跑）

// Windows：托盘 GUI 程序，不带控制台窗口（stdout/stderr 仍可被重定向到文件/管道）。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod agent;
mod agents_md;
mod attachments;
mod botstatus;
mod bridge;
mod ccswitch;
mod config;
mod ctxcompress;
mod deliver;
mod deps;
mod dingtalk;
mod feishu;
mod guard;
mod history;
mod install;
mod larkskills;
mod messenger;
mod msgstore;
mod outbox;
mod pending;
mod permreq;
mod platform;
mod proto;
mod schedule;
mod service;
mod session_gc;
mod session_import;
mod session_manage;
mod session_state;
mod sessions;
mod single_instance;
mod tasks;
mod teambuilder;
mod tidy;
mod trash;
mod ui;
mod unread;
mod updater;
mod virtualbot;
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

/// 原子写敏感文本文件：uuid 唯一 tmp + rename + 落盘前收紧 0o600（unix）。
/// 与 `atomic_write_text` 的差异：唯一 tmp 防并发写方（如 CLI 与 service）互踩同一 tmp
/// 文件；0o600 对齐 config.json 的敏感工件权限（历史/迁移标记等对话内容）。
/// 失败时清理残留 tmp（历史.rs 的 write_entries/set_marker 与 sessions.rs save_locked
/// 原先各自手写此模式，收敛为共享实现）。
pub fn atomic_write_sensitive(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
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

    /// 当前 UTC 时间的 RFC3339（"YYYY-MM-DDTHH:MM:SSZ" 形态，字典序可比较）。
    /// 与 now() 不同：UTC 不加本地偏移。
    pub fn rfc3339_now() -> String {
        let (y, mo, d, h, mi, s) = epoch_to_ymd(unix_secs());
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    }
    /// 距 UNIX 纪元秒数的本地时区日历拆解（y, mo, d, h, mi, s）。
    /// #74 历史页/提醒弹窗的时间显示（MM-DD HH:MM）复用同一套 UTC+8 口径。
    pub fn epoch_to_ymd(t: u64) -> (u64, u64, u64, u64, u64, u64) {
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
    // 定位收敛（2026-08）：GitHub 协作整体迁出本产品——存量配置中的 kind=github
    // bot 在此移除（幂等；GUI/service/CLI 谁先启动谁迁移，两进程并发原子写无破坏）。
    crate::config::Config::migrate_strip_github();

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

    // guard-check：claude PreToolUse hook 的决策子进程（授权者受限会话的强制闸）。
    // claude 以 `"$ABB_BIN" guard-check` 调用，stdin 收 hook 事件 JSON，stdout 出决策 JSON。
    if args.len() >= 2 && args[1] == "guard-check" {
        std::process::exit(guard::guard_check_main());
    }

    // 跨会话投递 CLI：供 claude 用 Bash 调用（也可人用）。
    //   agent-bridge deliver --bot <目标bot key> --chat <目标chat_id> --text "内容"
    //   [--source-bot <来源bot key> --source-chat <来源chat_id>]（缺省取桥注入的 env）
    // 总开关：Config.cross_delivery_enabled（设置 → 「跨会话投递」勾选），关闭时拒绝。
    if args.len() >= 2 && args[1] == "deliver" {
        std::process::exit(run_deliver_cli(&args[2..]));
    }

    // 会话管理 CLI（#23）：供 claude 用 Bash 调用（也可人用）。
    //   agent-bridge session reset <chat_id>
    // bot 从 AGENT_BRIDGE_BOT_KEY env（桥注入）解析；chat_id 缺省取 AGENT_BRIDGE_CHAT_ID。
    if args.len() >= 2 && args[1] == "session" {
        std::process::exit(run_session_cli(&args[2..]));
    }

    // 删除保护回收站 CLI（#88）：供 owner 手动恢复/清理（也可被桥 /trash 指令调用）。
    //   agent-bridge trash list [--bot <key>] [--pending]
    //   agent-bridge trash restore <id> [--bot <key>]
    //   agent-bridge trash purge [--bot <key>] [--all]
    //   agent-bridge trash confirm <path> [--bot <key>]
    // bot 缺省从 AGENT_BRIDGE_BOT_KEY env 解析（手动调用无 env 时报错提示）。
    if args.len() >= 2 && args[1] == "trash" {
        std::process::exit(run_trash_cli(&args[2..]));
    }

    // 一键创建团队 CLI（#100，P0）：LLM 按提示词生成团队方案（预览确认对象）。
    //   agent-bridge team generate "<团队目标>" [--members "小王,steven"] [--backend codex] [--template 软件产品团队]
    // 输出：校验后的团队方案 JSON（stdout），供上层预览确认/建群。
    if args.len() >= 2 && args[1] == "team" {
        std::process::exit(run_team_cli(&args[2..]));
    }

    // 历史会话迁移（#33）：agent-bridge session-import [--bot <key>] [--dry-run]。
    // 把后端私有 session 文件（claude/codex/pi）里的对话导入 ABB 的 history.rs，
    // 让 #49 之前的老历史参与注入接续。幂等（已导入来源跳过），可重跑。
    // --dry-run 只统计不写入；退出码 0=全部成功 1=有失败/跳过。
    if args.len() >= 2 && args[1] == "session-import" {
        std::process::exit(run_session_import_cli(&args[2..]));
    }

    // 一键安装全部缺失依赖（#60）：agent-bridge deps-install。
    // 终端/脚本可用；逐行进度 + 汇总，退出码 0=全部装好 1=有失败/跳过。
    if args.len() >= 2 && args[1] == "deps-install" {
        // 审查 Minor：拒绝尾随参数（--help 等不该真跑安装）
        if args.len() > 2 {
            println!("用法：agent-bridge deps-install（无参数，安装全部缺失依赖）");
            std::process::exit(2);
        }
        std::process::exit(run_deps_install_cli());
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
            let mut targets: Vec<schedule::JobTarget> = Vec::new();
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
                    // 多投递目标（#21）：可重复，`bot_key:chat_id` 跨 bot，裸 `chat_id` 本 bot
                    "--to" => {
                        let raw = match val {
                            Some(v) => v.to_string(),
                            None => {
                                eprintln!("--to 缺少值（格式：bot_key:chat_id 或 chat_id）");
                                return 1;
                            }
                        };
                        match schedule::parse_job_target(&raw) {
                            Ok(t) => targets.push(t),
                            Err(e) => {
                                eprintln!("{e:#}");
                                return 1;
                            }
                        }
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
            // 创建者角色：agent 会话 spawn 时注入 env（桥 → claude/codex → $ABB_BIN）。
            // 授权者建的任务落 granted，执行时走受限分支——否则可借 owner 全权限跑
            // 「读敏感文件」任务绕过隔离。手动跑 CLI 无 env → Owner（与现状一致）。
            let role = config::SenderRole::from_env();
            match schedule::job_from_parsed(
                kind,
                time_arg.as_deref(),
                cron_arg.as_deref(),
                &prompt,
                &chat_id,
                &note,
                targets,
                role,
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
                "用法：\n  agent-bridge job list\n  agent-bridge job del <id前缀>\n  agent-bridge job add (--once \"YYYY-MM-DD HH:MM\" | --cron \"分 时 日 月 周\") --prompt \"做什么\" [--note \"原句\"] [--to bot_key:chat_id]…（--to 可重复，跨 bot 多目标）"
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
    // @角色名寻址（#75 虚拟 Bot）：--chat @后端开发 → 查登记表解析成 chat_id；
    // 找不到报错并列出该 bot 可用角色。登记表与 service 注入判定共用同一份。
    let roles = crate::virtualbot::VirtualBotStore::new();
    let item = match deliver::parse_deliver_args_with_store(args, &env_bot, &env_chat, &roles) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}\n用法：agent-bridge deliver --bot <目标bot key> --chat <目标chat_id|@角色名> --text \"内容\" [--file <本地路径>]…");
            return 1;
        }
    };
    // 自环防护（消息循环防护 #21）：同 bot 同会话转发给自己没有意义且是循环温床。
    if deliver::is_self_loop(&item) {
        eprintln!("不能投递回当前会话（来源与目标相同），已拒绝。");
        return 1;
    }
    // 授权者（受限会话）纵深防御：--file 只能投递工作区内文件。
    // guard hook 是第一道（已在调用链上校验命令），这里 CLI 侧再兜一层——
    // 即使 hook 配置被绕过/误配，受限会话的 deliver 也投不出工作区外文件内容。
    if config::SenderRole::from_env() == config::SenderRole::Granted {
        // 工作区先 canonicalize：a.path 已按真实路径规范化，若 ~/.agent-bridge
        // 含符号链接组件（数据目录挪盘等），原始路径比较会误拒所有合法投递。
        let ws = std::fs::canonicalize(crate::workspace_dir(&env_bot))
            .unwrap_or_else(|_| crate::workspace_dir(&env_bot));
        for a in &item.attachments {
            if !guard::canonical_in_workspace(&a.path, &ws) {
                eprintln!("受限会话不能投递工作区外文件（已拒绝）：{}", a.path);
                return 1;
            }
        }
    }
    // 目标 bot 必须存在且启用、凭证就绪（与 service 路由表同源：config.bots[].key()）。
    let target_ok = cfg
        .bots
        .iter()
        .any(|b| b.key() == item.target_bot && b.enabled && b.credentials_ready());
    if !target_ok {
        let keys: Vec<String> = cfg
            .bots
            .iter()
            .filter(|b| b.enabled && b.credentials_ready())
            .map(|b| b.key())
            .collect();
        eprintln!(
            "目标 bot「{}」不存在、已停用或凭证未就绪。当前可用 bot：{}",
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

/// #60 一键安装全部缺失依赖 CLI（deps-install）：逐行进度 + 如实汇总。
/// 退出码 0=全部装好；1=有失败或跳过。
fn run_deps_install_cli() -> i32 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let outcome = rt.block_on(crate::deps::install_all_missing(|evt| {
        println!("[deps] [{}/{}] 安装 {} …", evt.idx, evt.total, evt.label);
    }));
    for id in &outcome.ok {
        println!("[deps] OK   {id}");
    }
    for (id, e) in &outcome.failed {
        println!("[deps] FAIL {id}: {e}");
    }
    for (id, e) in &outcome.skipped {
        println!("[deps] SKIP {id}: {e}");
    }
    println!("{}", crate::deps::format_all_summary(&outcome));
    #[cfg(target_os = "windows")]
    if !outcome.failed.is_empty() {
        println!("（Windows：若为权限错误，可右键以管理员身份运行，或在 GUI 环境页点「以管理员重启」。）");
    }
    if outcome.failed.is_empty() && outcome.skipped.is_empty() {
        0
    } else {
        1
    }
}

/// 历史会话迁移 CLI（#33）。退出码 0=全部成功 1=有失败/跳过。
fn run_session_import_cli(args: &[String]) -> i32 {
    let mut bot_key: Option<String> = None;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bot" => {
                i += 1;
                if i >= args.len() {
                    println!("用法：agent-bridge session-import [--bot <key>] [--dry-run]");
                    return 2;
                }
                bot_key = Some(args[i].clone());
            }
            "--dry-run" => dry_run = true,
            other => {
                println!("未知参数：{other}（用法：agent-bridge session-import [--bot <key>] [--dry-run]）");
                return 2;
            }
        }
        i += 1;
    }
    // 枚举 bot：--bot 指定单个；否则全部 enabled 的 bot
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    let keys: Vec<String> = match &bot_key {
        Some(k) => vec![k.clone()],
        None => cfg.bots.iter().map(|b| b.key()).collect(),
    };
    let mut any_issue = false;
    let mut found = false;
    for key in keys {
        found = true;
        let report = crate::session_import::import_bot(&key, dry_run);
        if dry_run {
            println!("[dry-run] bot={key}");
        } else {
            println!("bot={key}");
        }
        for cr in &report.chats {
            println!(
                "  chat={} 导入 {} 条{}",
                cr.chat,
                cr.imported,
                if cr.skipped.is_empty() {
                    String::new()
                } else {
                    format!("；跳过: {}", cr.skipped.join("; "))
                }
            );
        }
        if report.chats.is_empty() {
            println!("  （无可导入的会话）");
        }
        any_issue |= report.chats.iter().any(|c| !c.skipped.is_empty());
    }
    if !found {
        eprintln!(
            "找不到该 bot（--bot 拼写？可用：agent-bridge session-import --dry-run 列出全部）"
        );
        return 2;
    }
    if dry_run {
        println!("（dry-run：未写入任何内容）");
    }
    if any_issue {
        1
    } else {
        0
    }
}

/// 会话管理 CLI（#23）。退出码 0=成功 1=失败。
fn run_session_cli(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        // #87 会话可观察/可管控（list/show/pause/resume/delete）由独立模块实现。
        // 原有 reset（#23）保持原逻辑不变。
        "list" | "show" | "pause" | "resume" | "delete" => crate::session_manage::run(args),
        "reset" => {
            let bot_key = match resolve_bot_key() {
                Ok(k) => k,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let cfg = match config::Config::load() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("读 config 失败: {e:#}");
                    return 1;
                }
            };
            // 后端跟 bot 走（与聊天/定时任务一致），决定 reset 哪个后端槽位
            let backend = cfg
                .bots
                .iter()
                .find(|b| b.key() == bot_key)
                .map(|b| b.effective_backend(&cfg.default_backend).to_string())
                .unwrap_or_else(|| cfg.default_backend.clone());
            let env_chat = std::env::var("AGENT_BRIDGE_CHAT_ID").unwrap_or_default();
            let chat = match session_reset_chat_id(&args[1..], &env_chat) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let store = sessions::SessionStore::new(&backend, &bot_key);
            let sid = store.reset_session(&chat);
            // 打印完整 UUID：后续要拿它做 --session-id / resume 时截断会误导
            println!(
                "✅ 已新建会话 bot={} chat={} session={sid}（service 热重载，无需重启）",
                bot_key, chat
            );
            0
        }
        _ => {
            eprintln!(
                "用法：\n  agent-bridge session list [--bot <名>] [--state active|paused|gc-pending] [--active-days N] [--paused]\n  agent-bridge session show <chat_id> [--last N] [--since YYYY-MM-DD] [--bot <名>]\n  agent-bridge session pause <chat_id> [--bot <名>]\n  agent-bridge session resume <chat_id> [--bot <名>]\n  agent-bridge session delete <chat_id> [--purge] [--yes] [--bot <名>]\n  agent-bridge session reset <chat_id>（bot 取 AGENT_BRIDGE_BOT_KEY，chat 缺省取 AGENT_BRIDGE_CHAT_ID）"
            );
            1
        }
    }
}

/// 解析 session reset 的目标 chat：显式参数优先，缺省回落 env（桥 spawn agent 时注入）。
fn session_reset_chat_id(args: &[String], env_chat: &str) -> Result<String, String> {
    if let Some(c) = args.first() {
        let c = c.trim();
        if !c.is_empty() {
            return Ok(c.to_string());
        }
    }
    if !env_chat.is_empty() {
        return Ok(env_chat.to_string());
    }
    Err("缺 chat_id：agent-bridge session reset <chat_id>（或用 AGENT_BRIDGE_CHAT_ID env）".into())
}

/// trash CLI 入口（#88 删除保护回收站）。bot 缺省从 AGENT_BRIDGE_BOT_KEY env 解析。
fn run_trash_cli(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        eprintln!("用法：agent-bridge trash list|restore <id>|purge|confirm <path> [--bot <key>]");
        return 2;
    };
    let bot_key = match trash_bot_key(args) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let workspace = crate::workspace_dir(&bot_key);
    // 热读 bot 实际配置（与 hook/service 同口径），避免 TTL 默认值提前清条目
    let settings = crate::guard::bot_trash_settings_for(&bot_key);
    match sub {
        "list" => {
            let items = crate::trash::list(&workspace);
            if items.is_empty() {
                println!("回收站为空");
            } else {
                for it in &items {
                    let days_ago =
                        crate::chrono_lite::unix_secs().saturating_sub(it.trashed_at) / 86400;
                    println!(
                        "{} | {} | {} MB | {} 天前 | {}{}",
                        it.id,
                        crate::trash::pretty_path(std::path::Path::new(&it.orig)),
                        it.size / (1024 * 1024),
                        days_ago,
                        it.reason,
                        if it.dangerous { " | ⚠️危险" } else { "" }
                    );
                }
            }
            let pending = crate::guard::list_pending(&bot_key);
            if !pending.is_empty() {
                println!("\n待确认的危险删除（/trash confirm <路径>）：");
                for (p, _) in pending {
                    println!("  {p}");
                }
            }
            0
        }
        "restore" => {
            let id = args.get(1).map(|s| s.as_str()).unwrap_or("");
            if id.is_empty() {
                eprintln!("用法：agent-bridge trash restore <id> [--bot <key>]");
                return 2;
            }
            match crate::trash::restore(&workspace, id) {
                Ok(it) => {
                    println!("已恢复：{} → {}", it.id, it.orig);
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "purge" => {
            let all = args.iter().any(|a| a == "--all");
            let n = if all {
                crate::trash::purge_all(&workspace)
            } else {
                crate::trash::purge_expired(&workspace, settings.ttl_days)
            };
            println!(
                "已清理回收站条目 {} 条{}",
                n,
                if all { "（全部）" } else { "（过期）" }
            );
            0
        }
        "confirm" => {
            let path = args.get(1).map(|s| s.as_str()).unwrap_or("");
            if path.is_empty() {
                eprintln!("用法：agent-bridge trash confirm <path> [--bot <key>]");
                return 2;
            }
            match crate::guard::confirm_dangerous_delete(&bot_key, &workspace, path) {
                Ok(it) => {
                    println!(
                        "已确认并移入回收站：{}（{} 天内可恢复）",
                        it.orig, settings.ttl_days
                    );
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        other => {
            eprintln!("未知 trash 子命令：{other}");
            2
        }
    }
}

/// 一键创建团队 CLI（#100 P0）：LLM 按提示词生成团队方案（预览确认对象）。
/// 用法：agent-bridge team generate "<目标>" [--members "小王,steven"] [--backend codex] [--template 软件产品团队]
/// 成功 → stdout 输出校验后的团队方案 JSON（缩进）；失败 → stderr 提示重试/手动编辑。
fn run_team_cli(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("templates") => {
            // 列出内置起手式模板（含说明），供用户选择
            for t in crate::teambuilder::builtin_team_templates() {
                println!("{} — {}", t.name, t.description);
            }
            return 0;
        }
        Some("generate") => {}
        _ => {
            eprintln!("用法：agent-bridge team generate \"<团队目标>\" [--members \"小王,steven\"] [--backend codex] [--template 软件产品团队]\n       agent-bridge team templates");
            return 2;
        }
    }
    let mut goal = String::new();
    let mut members: Vec<String> = Vec::new();
    let mut backend = crate::agent::Backend::Codex;
    let mut template: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--members" => {
                i += 1;
                if let Some(m) = args.get(i) {
                    members = m
                        .split([',', '，'])
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
            "--backend" => {
                i += 1;
                if let Some(b) = args.get(i) {
                    backend = crate::agent::Backend::parse(b);
                }
            }
            "--template" => {
                i += 1;
                template = args.get(i).cloned();
            }
            other => {
                if goal.is_empty() {
                    goal = other.to_string();
                } else {
                    goal.push(' ');
                    goal.push_str(other);
                }
            }
        }
        i += 1;
    }
    if goal.trim().is_empty() {
        eprintln!("缺少团队目标。用法：agent-bridge team generate \"<团队目标>\"");
        return 2;
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("运行时创建失败：{e}");
            return 1;
        }
    };
    match rt.block_on(crate::teambuilder::generate_team_plan(
        backend,
        &goal,
        &members,
        template.as_deref(),
    )) {
        Ok(plan) => {
            match serde_json::to_string_pretty(&plan) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("序列化失败：{e}"),
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// 解析 bot key：优先命令行 --bot，回落 AGENT_BRIDGE_BOT_KEY env。
fn trash_bot_key(args: &[String]) -> Result<String, String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--bot" {
            if let Some(v) = args.get(i + 1) {
                return Ok(v.clone());
            }
        }
        i += 1;
    }
    std::env::var("AGENT_BRIDGE_BOT_KEY").map_err(|_| {
        "缺少 bot key：请用 --bot <key> 指定，或在桥注入环境（AGENT_BRIDGE_BOT_KEY）下调用"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::session_reset_chat_id;

    #[test]
    fn rfc3339_now_utc_format() {
        // UTC "YYYY-MM-DDTHH:MM:SSZ" 形态，字典序可比较
        let s = super::chrono_lite::rfc3339_now();
        assert_eq!(s.len(), 20, "格式应为 YYYY-MM-DDTHH:MM:SSZ: {s}");
        assert!(s.ends_with('Z'));
        assert!(s.chars().nth(10) == Some('T'));
        // 与本地 now() 至少同日（UTC vs UTC+8 可能跨日，这里只验证可解析性）
        let _ = super::chrono_lite::unix_secs();
    }

    #[test]
    fn session_reset_chat_prefers_arg() {
        assert_eq!(
            session_reset_chat_id(&["oc_123".into()], "oc_env").unwrap(),
            "oc_123"
        );
    }

    #[test]
    fn session_reset_chat_falls_back_to_env() {
        assert_eq!(session_reset_chat_id(&[], "oc_env").unwrap(), "oc_env");
        assert_eq!(
            session_reset_chat_id(&["   ".into()], "oc_env").unwrap(),
            "oc_env"
        );
    }

    #[test]
    fn session_reset_chat_requires_target() {
        assert!(session_reset_chat_id(&[], "").is_err());
    }
}
