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
mod buzz;
mod config;
mod deliver;
mod deps;
mod dingtalk;
mod feishu;
mod guard;
mod history;
mod install;
mod larkskills;
mod lockctl;
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
mod task_proc;
mod task_run;
mod task_store;
mod tasks;
mod teambuilder;
mod teamflow;
mod teamreg;
mod tidy;
mod trash;
mod ui;
mod unread;
mod updater;
mod virtualbot;
mod wechat;
mod ws;
mod wsver;

/// 运行数据目录的环境变量覆盖；空字符串按未设置处理。
pub(crate) fn bridge_home_override() -> Option<std::path::PathBuf> {
    std::env::var_os("AGENT_BRIDGE_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

/// 运行时数据目录：默认 ~/.agent-bridge（隐藏目录，与 ~/.claude 同款）；
/// `AGENT_BRIDGE_HOME` 非空时直接作为整个数据目录。
/// 老路径 ~/feishu-bridge 由 platform::migrate_to_agent_bridge() 一次性迁移过来（main 启动时跑）。
pub fn bridge_dir() -> std::path::PathBuf {
    bridge_home_override()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".agent-bridge"))
}

/// 某 bot 的工作目录：~/.agent-bridge/workspaces/<bot_key>/。约定 agent 只在此读写。
/// 多 bot 相互隔离——每个 bot 独立工作目录。
pub fn workspace_dir(bot_key: &str) -> std::path::PathBuf {
    bridge_dir().join("workspaces").join(bot_key)
}

/// Windows：rename 覆盖目标被另一进程短暂占用（打开方无 FILE_SHARE_DELETE，如杀毒
/// 扫描、并发写方换名瞬间）→ MoveFileExW 报 ERROR_ACCESS_DENIED（os error 5）/
/// ERROR_SHARING_VIOLATION（32），#170 实测的「拒绝访问」。短暂退避重试后仍失败
/// 才上报；unix rename 无此占用语义，直接原样转发。
#[cfg(windows)]
fn rename_replace_retry(tmp: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    let mut last_err = None;
    for _ in 0..3 {
        match std::fs::rename(tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if matches!(e.raw_os_error(), Some(5) | Some(32)) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("rename 重试耗尽")))
}

#[cfg(not(windows))]
fn rename_replace_retry(tmp: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(tmp, path)
}

/// 原子写文本文件（tmp + rename）。config/sessions/jobs/botstatus 共用，避免崩溃留半截。
/// #137：唯一 tmp 名防并发写方互踩——固定名时进程 A rename 后，进程 B 的 rename
/// 拿不到 tmp → ENOENT（定时任务并发触发 guard 文件生成竞争失败）。与
/// `atomic_write_sensitive` 同款：uuid 唯一 tmp + rename，失败清理残留。
/// #170：rename 覆盖目标被占用时（os error 5）走 `rename_replace_retry` 短暂重试。
pub fn atomic_write_text(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, text)?;
    match rename_replace_retry(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 内容相同跳过写盘（#170）：静态配置文件（如 guard settings.json、workspace 引导
/// 文档）每次调用都原子重写，会在并发（定时任务 + 聊天消息并行）时 rename 目标被
/// 另一进程占用 → 拒绝访问（os error 5）。相同内容不写，消除稳态并发竞争窗口；
/// 内容真变化（如 exe 路径变更）才走 `atomic_write_text`（其内部 rename 仍有
/// `rename_replace_retry` 兜底首次写/变更瞬间的残留窗口）。
pub fn atomic_write_text_if_changed(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == text {
            return Ok(());
        }
    }
    atomic_write_text(path, text)
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
    match rename_replace_retry(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 统一日志到 stdout（带时间戳，与 Python 版一致，落 logs/bridge.out）。
pub fn write_log(writer: &mut dyn std::io::Write, args: std::fmt::Arguments<'_>) {
    let _ = std::io::Write::write_fmt(
        writer,
        format_args!("[{}] {}\n", crate::chrono_lite::now(), args),
    );
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        // windows_subsystem=windows 下无控制台时 stdout 句柄无效：println! 会 panic，
        // 这里用 write_fmt + 忽略错误，日志在无控制台时静默丢弃，重定向时照常落盘。
        $crate::write_log(&mut std::io::stdout(), format_args!($($arg)*));
    }};
}

/// 与 `log!` 同一格式与时间戳，但允许测试注入 writer 验证真实发射路径。
#[macro_export]
macro_rules! log_to {
    ($writer:expr, $($arg:tt)*) => {{
        $crate::write_log($writer, format_args!($($arg)*));
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

    // 权限请求（GUI「请求权限」按钮拉起）：逐项触发屏幕录制/摄像头/麦克风授权弹框，
    // #129 追加辅助功能/输入监控（锁屏控制前置）。独立子进程跑（不阻塞托盘），
    // 逐行打日志，GUI 逐行读进设置窗状态区。
    #[cfg(target_os = "macos")]
    if args.iter().any(|a| a == "--request-permissions") {
        permreq::request_media_permissions();
        permreq::request_lock_permissions();
        return;
    }

    // #129 锁屏控制特权助手运维：status / install / uninstall（client 侧）。
    // 安装弹显式管理员授权框（用户不点不同意则不装任何东西）；卸载完整移除。
    #[cfg(target_os = "macos")]
    if let Some(pos) = args.iter().position(|a| a == "--lockctl") {
        let sub = args.get(pos + 1).map(String::as_str).unwrap_or("status");
        match sub {
            "status" => println!("{}", lockctl::status()),
            "install" => match lockctl::install() {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            },
            "uninstall" => match lockctl::uninstall() {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            },
            other => {
                eprintln!("用法: --lockctl status|install|uninstall（未知子命令: {other}）");
                std::process::exit(2);
            }
        }
        return;
    }

    // 隐藏诊断：打印当前二进制六项系统权限的真实检测态（验证 API 检测是否反映系统设置）。
    if args.iter().any(|a| a == "--dump-perms") {
        for p in deps::detect_permissions() {
            println!("{}\t{:?}", p.id, p.state);
        }
        return;
    }

    // #305 Step 0：相机探测。**必须**用
    //   open -n -a /Applications/ABB.app --args --camera-probe 0 /tmp/abb-camera-probe.jpg
    // 触发——让 LaunchServices 把新实例的 responsible process 设成 ABB.app，
    // 这样它派生的 ffmpeg 才落在 ABB 的 TCC 责任面内（裸二进制从终端跑会把归属
    // 算到终端，实验无效）。判定标准见 permreq::camera_probe 的文档。
    #[cfg(target_os = "macos")]
    if let Some(pos) = args.iter().position(|a| a == "--camera-probe") {
        // 参数边界：下一 token 若以 `-` 开头视为「没给」（否则 `--camera-probe --service`
        // 会把 `--service` 当设备号传给 ffmpeg）。
        let arg_after = |n: usize| -> Option<&str> {
            args.get(pos + n)
                .map(String::as_str)
                .filter(|v| !v.starts_with('-'))
        };
        let index = arg_after(1).unwrap_or("0");
        let out = arg_after(2).unwrap_or("/tmp/abb-camera-probe.jpg");
        match permreq::camera_probe(index, out) {
            // 报告已落盘（open -n 起的实例 stdout/stderr 是 /dev/null，println 只是终端直跑时的
            // best-effort）；抓不到帧返回非 0，脚本可据此判定。
            Ok((report, ok)) => {
                print!("{report}");
                if !ok {
                    std::process::exit(2);
                }
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // 隐藏诊断：确认随包工具与宿主 PATH 的最终解析结果。
    if args.iter().any(|a| a == "--dump-tools") {
        let strict = args.iter().any(|a| a == "--require-bundled-tools");
        let mut not_bundled = false;
        for tool in deps::BUNDLED_TOOLS {
            let (source, path) = deps::bundled_tool_status(tool);
            if source != "bundled" {
                not_bundled = true;
            }
            println!(
                "{tool}\t{source}\t{}",
                path.as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".to_string())
            );
        }
        if strict && not_bundled {
            std::process::exit(1);
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

    // 任务 CLI（#326 / #306）：`agent-bridge task <子命令>`。
    //   task add --prompt "做什么" [--name 名字] [--cwd 路径] [--timeout-secs N]
    //   task list | task status <id前缀> | task logs <id前缀> [--tail N] [--all] | task rm <id前缀>
    // 与 job 的差别：任务**不等时刻**——登记后由 service 的 task worker 立刻认领执行
    // （trigger=now 的 agent 任务就是「后台子代理」，跑在自己的 handle 上、不占聊天 slot）。
    if args.len() >= 2 && args[1] == "task" {
        std::process::exit(run_task_cli(&args[2..]));
    }

    // guard-check：claude PreToolUse hook 的决策子进程（授权者受限会话的强制闸）。
    // claude 以 `"$ABB_BIN" guard-check` 调用，stdin 收 hook 事件 JSON，stdout 出决策 JSON。
    if args.len() >= 2 && args[1] == "guard-check" {
        std::process::exit(guard::guard_check_main());
    }

    // 跨会话投递 CLI：供 claude 用 Bash 调用（也可人用）。
    //   agent-bridge deliver --text "内容" [--file <路径>]…
    //     （不给目标 = 缺省发回创建者会话，等价 --to-current；见 parse_deliver_args）
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

    // 工作区版本管理 CLI（#209 批次 4/5）：快照历史 / 按快照恢复 / 保护状态一览。
    //   agent-bridge wsver log [--bot <key>] [-n 10]
    //   agent-bridge wsver restore <commit> <path> [--bot <key>]
    //   agent-bridge wsver status [--bot <key>]
    // bot 缺省从 AGENT_BRIDGE_BOT_KEY env 解析（同 trash）。
    if args.len() >= 2 && args[1] == "wsver" {
        std::process::exit(run_wsver_cli(&args[2..]));
    }

    // 一键创建团队 CLI（#100，P0）：LLM 按提示词生成团队方案（预览确认对象）。
    //   agent-bridge team generate "<团队目标>" [--members "小王,steven"] [--template 软件产品团队]
    // （--backend 已废弃：单后端化 P3.4 后统一由随包 buzz-agent 执行，传入仅警告并忽略）
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
    // 自启漂移自愈（macOS；其它平台是空函数）：LaunchAgent plist 在、但登记的二进制
    // 已不存在或指向旧副本时，按当前二进制重建并 launchctl reload。App 被移动过
    // （build.sh 装 ~/Applications、正式包拖进 /Applications）后 launchd 首次 exec
    // 即判 EX_CONFIG(78) 并静默停手（实测不会重试、二进制补回也不拉），而托盘仍显示
    // 「开」——在这里收敛回真值。
    crate::platform::heal_autostart();
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
                            Ok(t) => {
                                if crate::buzz::keys::looks_like_channel_uuid(t.chat_id.trim()) {
                                    eprintln!(
                                        "--to 的 chat_id「{}」形如 buzz 频道 UUID，不是平台可用的 receive_id（直发会被平台拒，如飞书 230001）；请填真实 chat_id（飞书 oc_…／微信 wxid…／钉钉 cid…）",
                                        t.chat_id.trim()
                                    );
                                    return 1;
                                }
                                targets.push(t);
                            }
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
            // chat_id：优先 env（桥注入），否则回落该 bot 主会话。桥注入值若形如 buzz
            // 频道 UUID（非平台 receive_id），同样回落主会话并 loud 提示——否则任务
            // 「跑完也发不出」（见 resolve_injected_chat）。
            let chat_id = resolve_injected_chat(&bot_key);
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
    let raw = std::env::var("AGENT_BRIDGE_BOT_KEY").ok();
    let cfg = config::Config::load().map_err(|e| format!("读 config 失败: {e:#}"))?;
    resolve_env_bot_key_from(&cfg, raw.as_deref())
}

/// env→唯一 bot 的纯解析实现；凡把 env 值当目录/隔离键的入口都应复用这里。
fn resolve_env_bot_key_from(cfg: &config::Config, raw: Option<&str>) -> Result<String, String> {
    if let Some(raw) = raw.filter(|v| !v.trim().is_empty()) {
        if cfg.bots.is_empty() {
            let shown = raw.trim();
            return Err(format!(
                "无法校验 AGENT_BRIDGE_BOT_KEY={shown:?}：config.json 里没有任何 bot——\n\
                 该 key 对应的目录不会被任何 service 扫描（任务会永远停在 Pending）。\n\
                 请先在设置窗添加 bot，或在 bot 会话里调用本命令。"
            ));
        }
        // **必须收敛成规范 key**：这个值会被直接当目录名用（tasks/<key>/、sessions、
        // workspaces、outbox…），service 只扫规范 key。
        return cfg.resolve_bot_key(raw);
    }
    match cfg.bots.len() {
        0 => Err("config.json 没有配置任何 bot".into()),
        1 => cfg.resolve_bot_key(&cfg.bots[0].key()),
        n => Err(format!(
            "有 {n} 个 bot 但未指定目标（桥正常调用会注入 AGENT_BRIDGE_BOT_KEY；手动用请把该环境变量设成某个 bot 的 **key**，同名 bot 会带 -2 后缀）\n可用：{}",
            cfg.bot_key_list()
        )),
    }
}

/// 解析「桥注入的当前会话」chat_id（`job add` / `task add` 缺省回创建者会话走这条路）。
///
/// 返回 `AGENT_BRIDGE_CHAT_ID`；为空时回落该 bot 主会话；**形如 buzz 频道 UUID** 时
/// 同样回落主会话并 loud 提示。背景：ACP 架构下 agent 是每 bot 长驻进程，桥无法按频道
/// 注入 `AGENT_BRIDGE_CHAT_ID`；外部启动器喂进来的可能是 buzz 频道 UUID——它不是平台
/// receive_id，直发必被平台拒（飞书 230001 invalid receive_id），不校正则任务
/// 「跑完但没人看得到」。
fn resolve_injected_chat(bot_key: &str) -> String {
    let raw = std::env::var("AGENT_BRIDGE_CHAT_ID").unwrap_or_default();
    let primary = config::Config::primary_chat(bot_key);
    if raw.is_empty() {
        return primary;
    }
    let (chat, fell_back) = deliver::correct_injected_chat(&raw, &primary);
    if fell_back {
        eprintln!(
            "⚠️ AGENT_BRIDGE_CHAT_ID「{raw}」形如 buzz 频道 UUID，不是平台 chat_id，已回落到该 bot 主会话「{chat}」（否则会被平台判定 invalid receive_id，如飞书 230001）；要在指定群/话题回投，请显式指定真实 chat_id"
        );
    }
    chat
}

/// 读取任务日志：先按 `all` 决定是否拼接轮转历史，再对完整内容取末 `tail` 行。
///
/// 读取/拼接由 [`task_store::read_task_logs`] 负责；这里只做 CLI 层的 `--tail`
/// 截断，便于直接单测「先合并、后截断」的语义。
fn read_task_logs(
    paths: &task_store::TaskPaths,
    id: &str,
    all: bool,
    tail: usize,
) -> Result<String, String> {
    let body = task_store::read_task_logs(paths, id, all).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.len().saturating_sub(tail);
    Ok(lines[start..].join("\n"))
}

/// `--cmd` 后全部参数原样作为 argv。单独抽出是为了锁住“不拼 shell 串、不丢 quoted
/// 参数边界”的契约；返回值中的每个元素仍是一个独立 argv 元素。
fn take_proc_cmd(args: &[String], cmd_index: usize) -> Result<Vec<String>, String> {
    if args.get(cmd_index + 1).is_none() {
        return Err("--cmd 缺 argv（--cmd 之后的所有参数都会原样作为 argv）".to_string());
    }
    Ok(args[cmd_index + 1..].to_vec())
}

/// 任务 CLI（#326 / #306）。退出码 0=成功 1=失败。
///
/// 与 `job` 的分工：`job` 是「到点唤起一个回合」，本命令是「**立刻**登记一个后台任务」——
/// 登记后不阻塞调用方，由 service 的 task worker 认领执行，结果回创建者会话。
fn run_task_cli(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    // 帮助请求**先于** `resolve_bot_key()`（#312 审查）：`task --help` 是通用习惯，也是
    // 工作区指引写给 agent 的那条命令；落到下面的 `other` 臂会先多打一行「不认识的
    // 子命令」，而放到解析 bot 之后又会让「还没配 bot」的环境连帮助都看不了
    // （`task --help` → 「config.json 没有配置任何 bot」，实测）。退出码 0（不是错误）。
    if matches!(sub, "-h" | "--help" | "help") {
        eprintln!("{TASK_CLI_HELP}");
        return 0;
    }
    let bot_key = match resolve_bot_key() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let store = task_store::TaskStore::new(&bot_key);
    let states = task_store::TaskStateStore::new(&bot_key);
    match sub {
        "list" => {
            let tasks = store.list();
            if tasks.is_empty() {
                println!("（还没有任务）");
                return 0;
            }
            for t in &tasks {
                let rt = states.get(&t.id);
                println!(
                    "{}  {}  [{:?}]  {}",
                    t.id,
                    t.display_name(),
                    rt.kind,
                    describe_trigger(&t.trigger)
                );
            }
            0
        }
        "status" => {
            let Some(t) = resolve_task(&store, args.get(1)) else {
                return 1;
            };
            let rt = states.get(&t.id);
            println!("id        = {}", t.id);
            println!("name      = {}", t.display_name());
            println!("bot       = {}", t.bot_key);
            println!("载荷      = {:?}", t.payload.kind);
            println!("触发      = {}", describe_trigger(&t.trigger));
            println!(
                "创建者    = {} / {}（角色 {}）",
                t.created_by.bot_key,
                t.created_by.chat_id,
                t.created_by.role.as_str()
            );
            println!("运行态    = {:?}", rt.kind);
            if let Some(s) = rt.started_at {
                println!("开始      = {s}");
            }
            if let Some(f) = rt.finished_at {
                println!("结束      = {f}");
            }
            if let Some(c) = rt.last_exit_code {
                println!("退出码    = {c}");
            }
            // 重跑次数（#326 审查：中断后自动归位重跑是有上界的，得让人看得到用了几次）
            println!("重跑次数  = {}", rt.restarts);
            if !rt.last_error.is_empty() {
                println!("最近错误  = {}", rt.last_error);
            }
            let log = task_store::TaskPaths::for_bot(&bot_key).log_file(&t.id);
            if log.exists() {
                println!("日志      = {}", log.display());
            }
            0
        }
        "logs" => {
            let Some(t) = resolve_task(&store, args.get(1)) else {
                return 1;
            };
            // --tail N（缺省 200 行）；--all 先拼接轮转历史，再对完整内容取末 N 行。
            let mut tail = 200usize;
            let mut all = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--tail" => {
                        match args.get(i + 1).and_then(|v| v.parse::<usize>().ok()) {
                            Some(n) => tail = n,
                            None => {
                                eprintln!("--tail 需要一个数字");
                                return 1;
                            }
                        }
                        i += 2;
                    }
                    "--all" => {
                        all = true;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            let paths = task_store::TaskPaths::for_bot(&bot_key);
            // 默认语义（只读当前文件、取末 N 行）与历史完全一致；`--all` 只是先把轮转
            // 历史拼进来，再对完整内容取末 N 行。
            match read_task_logs(&paths, &t.id, all, tail) {
                Ok(body) => {
                    for l in body.lines() {
                        println!("{l}");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        // Q3 `task cancel`：**CLI 只投取消请求**，运行态由 service 侧的 task worker
        // 消费（单写者）。所以这里既不改 tasks-state.json，也不假装「已取消」——
        // 只保证请求已落盘，真正的结局由 worker 写。
        "cancel" | "stop" => {
            let Some(t) = resolve_task(&store, args.get(1)) else {
                return 1;
            };
            let rt = states.get(&t.id);
            use task_store::TaskStateKind as K;
            // 重复档（cron/interval）跑完一轮是 Succeeded/Failed，但**不是终态**：下一分钟/
            // 下一个间隔还会再跑，所以这里必须继续投取消请求（否则用户看到「已结束，无需
            // 取消」而任务照跑——审查实测）。只有 Cancelled、或一次性档的
            // Succeeded/Failed 才算真结束。
            let repeating = t.trigger.kind.is_repeating();
            let terminal = matches!(rt.kind, K::Cancelled)
                || (matches!(rt.kind, K::Succeeded | K::Failed) && !repeating);
            if terminal {
                println!("任务 {} 已结束（{:?}），无需取消", t.id, rt.kind);
                return 0;
            }
            if rt.kind == K::Running {
                println!(
                    "任务 {} 正在运行，已请求终止（数秒内生效、结果不再投递；若该轮刚好已收尾则本条无效）",
                    t.id
                );
            } else if repeating && matches!(rt.kind, K::Succeeded | K::Failed) {
                println!(
                    "任务 {} 是周期任务（当前空闲，{:?}）：已请求取消，后续不再触发",
                    t.id, rt.kind
                );
            } else {
                println!(
                    "任务 {} 尚未开跑，已请求取消（不会再开跑，也不会投递结果）",
                    t.id
                );
            }
            let paths = task_store::TaskPaths::for_bot(&bot_key);
            if let Err(e) = std::fs::create_dir_all(paths.cancel_requests_dir()) {
                eprintln!(
                    "写取消请求失败（{}）：{e}",
                    paths.cancel_requests_dir().display()
                );
                return 1;
            }
            let req = paths.cancel_file(&t.id);
            let body = serde_json::json!({
                "task_id": t.id,
                "requested_at": chrono_lite::unix_secs(),
                "requested_by": bot_key,
            });
            if let Err(e) = std::fs::write(&req, body.to_string()) {
                eprintln!("写取消请求失败（{}）：{e}", req.display());
                return 1;
            }
            if !crate::install::status().running {
                eprintln!(
                    "⚠️ 未检测到运行中的 service：取消请求已落盘，service 下次启动时会立即消费它"
                );
            }
            0
        }
        // del 是 rm 的别名（job 用的是 del，两边都认，减少踩空）
        "rm" | "del" => {
            let Some(t) = resolve_task(&store, args.get(1)) else {
                return 1;
            };
            // 运行中的任务不允许直接删定义：worker 还在跑，删了定义会让结局无处落
            // （状态文件会变成孤儿）。先 cancel 或等它跑完。
            let rt = states.get(&t.id);
            if rt.kind == task_store::TaskStateKind::Running {
                eprintln!(
                    "任务 {} 正在运行，不能直接删除（等它跑完，或先让 service 停止）",
                    t.id
                );
                return 1;
            }
            store.remove(&t.id);
            let _ = states.remove(&t.id);
            // 当前日志 + 轮转历史（.1/.2）一起删——只删当前文件会把轮转历史留成永久孤儿
            // （状态行已删，孤儿回收再也枚举不到它）。
            task_store::remove_task_logs(&task_store::TaskPaths::for_bot(&bot_key), &t.id);
            println!("已删除任务 {}", t.id);
            0
        }
        "add" => {
            let mut prompt: Option<String> = None;
            let mut proc_mode = false;
            let mut cmd: Vec<String> = Vec::new();
            let mut env = std::collections::BTreeMap::new();
            let mut name = String::new();
            let mut cwd = String::new();
            let mut timeout_secs = task_store::DEFAULT_TIMEOUT_SECS;
            let mut max_restarts = task_store::DEFAULT_MAX_RESTARTS;
            let mut grace_secs = task_store::DEFAULT_GRACE_SECS;
            // `--to bot_key:chat_id`（bot_key 可省 = 本 bot）/ `--to-current`。
            let mut to_target: Option<String> = None;
            let mut to_current = false;
            // P2b-C 触发档：三者互斥，缺省 = 立即（now）
            let mut once: Option<String> = None;
            let mut cron: Option<String> = None;
            let mut every: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--prompt" | "--text" => {
                        let Some(v) = args.get(i + 1) else {
                            eprintln!("--prompt 缺内容");
                            return 1;
                        };
                        prompt = Some(v.clone());
                        i += 2;
                    }
                    "--proc" => {
                        proc_mode = true;
                        i += 1;
                    }
                    "--cmd" => {
                        // `--cmd` 必须是选项链的最后一站：命令参数常见 `--foo`，继续解析会把
                        // 它们误认成 task add 的选项。这里只收集，不拼 shell 串、不做引号解释。
                        cmd = match take_proc_cmd(args, i) {
                            Ok(v) => v,
                            Err(e) => {
                                eprintln!("{e}");
                                return 1;
                            }
                        };
                        i = args.len();
                    }
                    "--env" => {
                        let Some(v) = args.get(i + 1) else {
                            eprintln!("--env 缺 KEY=VALUE");
                            return 1;
                        };
                        let Some((k, value)) = v.split_once('=') else {
                            eprintln!("--env 需要 KEY=VALUE 形式（收到 {v:?}）");
                            return 1;
                        };
                        env.insert(k.to_string(), value.to_string());
                        i += 2;
                    }
                    "--name" => {
                        let Some(v) = args.get(i + 1) else {
                            eprintln!("--name 缺内容");
                            return 1;
                        };
                        name = v.clone();
                        i += 2;
                    }
                    "--cwd" => {
                        let Some(v) = args.get(i + 1) else {
                            eprintln!("--cwd 缺路径");
                            return 1;
                        };
                        cwd = v.clone();
                        i += 2;
                    }
                    "--timeout-secs" => {
                        match args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                            Some(n) => timeout_secs = n,
                            None => {
                                eprintln!("--timeout-secs 需要一个数字（0 = 不限）");
                                return 1;
                            }
                        }
                        i += 2;
                    }
                    "--max-restarts" => {
                        match args.get(i + 1).and_then(|v| v.parse::<u32>().ok()) {
                            Some(n) => max_restarts = n,
                            None => {
                                eprintln!("--max-restarts 需要一个非负整数");
                                return 1;
                            }
                        }
                        i += 2;
                    }
                    "--grace-secs" => {
                        match args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                            Some(n) => grace_secs = n,
                            None => {
                                eprintln!(
                                    "--grace-secs 需要一个数字（合法范围 {}..={}）",
                                    task_store::MIN_GRACE_SECS,
                                    task_store::MAX_GRACE_SECS
                                );
                                return 1;
                            }
                        }
                        i += 2;
                    }
                    "--once" => {
                        let Some(v) = args.get(i + 1) else {
                            eprintln!("--once 缺时间点（形如 \"2026-09-20 09:00\"）");
                            return 1;
                        };
                        once = Some(v.clone());
                        i += 2;
                    }
                    "--cron" => {
                        let Some(v) = args.get(i + 1) else {
                            eprintln!("--cron 缺表达式（5 段：分 时 日 月 周，如 \"30 9 * * *\"）");
                            return 1;
                        };
                        cron = Some(v.clone());
                        i += 2;
                    }
                    "--every" => {
                        let Some(v) = args.get(i + 1) else {
                            eprintln!("--every 缺间隔（如 30s / 5m / 2h / 1d）");
                            return 1;
                        };
                        every = Some(v.clone());
                        i += 2;
                    }
                    "--to" => {
                        let Some(v) = args.get(i + 1) else {
                            eprintln!("--to 缺目标（形如 bot_key:chat_id，bot_key 可省）");
                            return 1;
                        };
                        to_target = Some(v.clone());
                        i += 2;
                    }
                    "--to-current" => {
                        to_current = true;
                        i += 1;
                    }
                    other => {
                        eprintln!("task add 不认识的参数：{other}");
                        return 1;
                    }
                }
            }
            if proc_mode {
                if let Some(reason) = task_proc::platform_error() {
                    eprintln!("{reason}");
                    return 1;
                }
                if prompt.is_some() {
                    eprintln!("--proc 与 --prompt 互斥：proc 只接受 --cmd argv");
                    return 1;
                }
                if cmd.is_empty() {
                    eprintln!("--proc 需要 --cmd <argv…>（arg0 必填，不接 shell 串）");
                    eprintln!("{TASK_ADD_USAGE}");
                    return 1;
                }
            } else {
                if !cmd.is_empty() || !env.is_empty() {
                    eprintln!("--cmd/--env 只能与 --proc 一起使用");
                    return 1;
                }
                if prompt.is_none() {
                    eprintln!("{TASK_ADD_USAGE}");
                    return 1;
                }
            }
            if to_current && to_target.is_some() {
                eprintln!("--to-current 与 --to 互斥（前者 = 显式发回创建者会话）");
                return 1;
            }
            // 触发档三选一：全不给 = 立即跑（#306 的后台子代理语义）
            let chosen = [
                once.as_ref().map(|v| (task_store::TriggerKind::Once, v)),
                cron.as_ref().map(|v| (task_store::TriggerKind::Cron, v)),
                every
                    .as_ref()
                    .map(|v| (task_store::TriggerKind::Interval, v)),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            if chosen.len() > 1 {
                eprintln!("--once / --cron / --every 三者只能给一个");
                return 1;
            }
            let trigger = match chosen.first() {
                Some((kind, expr)) => task_store::TaskTrigger {
                    kind: *kind,
                    expr: (*expr).clone(),
                    timezone: String::new(),
                },
                None => task_store::TaskTrigger {
                    kind: task_store::TriggerKind::Now,
                    expr: String::new(),
                    timezone: String::new(),
                },
            };
            // 创建者会话：优先桥注入的 env；手动 CLI 回落该 bot 的主会话（与 job add 同款）。
            // 审查 B2：这里若留空，任务会「跑完但没人看得到」——而 CLI 却印着「结果回创建者会话」。
            // 所以两条路都给不出目标时**直接拒绝登记**，不接受一个永远发不出结果的任务。
            // 桥注入值若形如 buzz 频道 UUID（非平台 receive_id），回落主会话并 loud 提示。
            let chat_id = resolve_injected_chat(&bot_key);
            if chat_id.is_empty() {
                eprintln!(
                    "无法确定结果投递目标：AGENT_BRIDGE_CHAT_ID 为空且该 bot 主会话未建立\n\
                     （先在对应 IM 私聊该 bot 发一句话，或从 bot 会话内调用本命令）"
                );
                return 1;
            }
            // 显式 `--to`：`bot_key:chat_id`（bot_key 可省 = 本 bot）。`--to-current`
            // 与缺省同义，都落成「无 targets ⇒ 回创建者会话」，这里只做互斥校验。
            let targets = match to_target {
                Some(raw) => {
                    let (tbot, tchat) = parse_task_to(&raw);
                    if tchat.trim().is_empty() {
                        eprintln!("--to 的 chat_id 不能为空：{raw}");
                        return 1;
                    }
                    if crate::buzz::keys::looks_like_channel_uuid(tchat.trim()) {
                        eprintln!(
                            "--to 的 chat_id「{}」形如 buzz 频道 UUID，不是平台可用的 receive_id（直发会被平台拒，如飞书 230001）；请填真实 chat_id（飞书 oc_…／微信 wxid…／钉钉 cid…）",
                            tchat.trim()
                        );
                        return 1;
                    }
                    vec![task_store::TaskTarget {
                        bot_key: tbot,
                        chat_id: tchat,
                    }]
                }
                None => Vec::new(),
            };
            let targets = if to_current { Vec::new() } else { targets };
            let task = task_store::Task {
                schema_version: task_store::TASK_SCHEMA_VERSION,
                id: task_store::new_id(chrono_lite::unix_secs()),
                name,
                bot_key: bot_key.clone(),
                created_by: task_store::CreatedBy {
                    role: config::SenderRole::from_env(),
                    bot_key: bot_key.clone(),
                    chat_id: chat_id.clone(),
                },
                payload: task_store::TaskPayload {
                    kind: if proc_mode {
                        task_store::PayloadKind::Proc
                    } else {
                        task_store::PayloadKind::Agent
                    },
                    prompt: prompt.unwrap_or_default(),
                    cwd,
                    cmd,
                    env,
                },
                trigger,
                delivery: task_store::TaskDelivery {
                    targets,
                    ..Default::default()
                },
                limits: task_store::TaskLimits {
                    timeout_secs,
                    max_restarts,
                    grace_secs,
                    ..Default::default()
                },
            };
            match store.add(task.clone()) {
                Ok(()) => {
                    // 运行态显式落 Pending：worker 只认「定义存在 + 状态 Pending」，
                    // 没有状态行时 get() 也返回默认 Pending，两处等价、不靠隐式。
                    let _ = states.set(&task.id, task_store::TaskRuntime::default());
                    println!(
                        "🤖 后台任务已登记：{}（{}）\n   执行状态：agent-bridge task status {}\n   结果投递：{}（若 service 未运行则不会开跑）",
                        task.id,
                        task.display_name(),
                        task.id,
                        describe_task_delivery(&task),
                    );
                    0
                }
                Err(e) => {
                    eprintln!("登记失败：{e}");
                    1
                }
            }
        }
        other => {
            if !other.is_empty() {
                eprintln!("task 不认识的子命令：{other}");
            }
            eprintln!("{TASK_CLI_HELP}");
            1
        }
    }
}

/// `task add` 的用法行（错误提示与总帮助共用，避免两处漂移）。
const TASK_ADD_USAGE: &str = "用法：agent-bridge task add --prompt \"做什么\" [--name 名字] [--cwd 路径] [--timeout-secs N] [--max-restarts N] [--to bot_key:chat_id | --to-current] [--once \"YYYY-MM-DD HH:MM\" | --cron \"分 时 日 月 周\" | --every 5m]\n     agent-bridge task add --proc [上述公共选项] [--env KEY=VALUE] [--grace-secs 1..=300] --cmd <argv…>（--cmd 必须最后，后续参数原样作为 argv）";

/// `task` 的总帮助（**单一真源**：#312 的指引 v7 逐字内嵌它，防文档漂移——
/// 改了分派分支/参数就必须同步改这里，`agent::tests` 有一条断言锁住两边一致）。
pub(crate) const TASK_CLI_HELP: &str = "用法：agent-bridge task <list|status|logs|add|cancel|rm> …\n\
    \n  task add --prompt \"做什么\" [--name 名字] [--cwd 路径] [--timeout-secs N] [--max-restarts N] [--to bot_key:chat_id | --to-current] [--once \"YYYY-MM-DD HH:MM\" | --cron \"分 时 日 月 周\" | --every 5m]\n\
     \n  task add --proc [公共选项] [--env KEY=VALUE] [--grace-secs 1..=300] --cmd <argv…>  人工/GUI 专用；argv 不经过 shell，--cmd 必须放最后\n\
     \n  task list                         列出本 bot 的任务\n\
     \n  task status <id前缀>              看一条任务的详情与运行态\n\
     \n  task logs <id前缀> [--tail N] [--all]  看任务日志（缺省末 200 行；--all 先按 .2→.1→当前拼接轮转历史再取末 N 行）\n\
     \n  task cancel <id前缀>              取消任务（在跑的中止且不投递结果；未开跑的不再开跑）\n\
     \n  task rm <id前缀>                  删除任务（运行中不允许）";

/// 解析 `--to` 的值：`bot_key:chat_id`（`bot_key` 可省）。只按**第一个**冒号切，
/// 留空段按语义校验（chat_id 必须非空，bot_key 空 = 本 bot）。
fn parse_task_to(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((b, c)) => (b.trim().to_string(), c.trim().to_string()),
        None => (String::new(), raw.trim().to_string()),
    }
}

/// 人话描述这条任务的结果去向（登记成功回执用）。
fn describe_task_delivery(t: &task_store::Task) -> String {
    match t.delivery.targets.first() {
        None => format!("回创建者会话（{}）", t.created_by.chat_id),
        Some(tg) if tg.bot_key.trim().is_empty() => format!("显式指定 {}", tg.chat_id),
        Some(tg) => format!("显式指定 {}:{}", tg.bot_key, tg.chat_id),
    }
}

/// 把触发档渲染成人话（列表/详情共用）。
fn describe_trigger(t: &task_store::TaskTrigger) -> String {
    match t.kind {
        task_store::TriggerKind::Now => "立即".to_string(),
        task_store::TriggerKind::Once => format!("一次性 {}", t.expr),
        task_store::TriggerKind::Cron => format!("周期 {}", t.expr),
        task_store::TriggerKind::Interval => match task_store::parse_interval_secs(&t.expr) {
            Some(secs) => format!("每 {}", task_store::human_interval(secs)),
            None => format!("每 {}（表达式无法解析）", t.expr),
        },
        task_store::TriggerKind::Keepalive => "常驻".to_string(),
    }
}

/// 按 id 前缀解析出唯一一条任务（沿用 job 的前缀语义：0 条/多条都报错）。
fn resolve_task(
    store: &task_store::TaskStore,
    prefix: Option<&String>,
) -> Option<task_store::Task> {
    let prefix = prefix.map(|s| s.trim()).unwrap_or("");
    if prefix.is_empty() {
        eprintln!("用法：agent-bridge task <status|logs|rm> <id前缀>（用 task list 查看 id）");
        return None;
    }
    let hit: Vec<_> = store
        .list()
        .into_iter()
        .filter(|t| t.id.starts_with(prefix))
        .collect();
    match hit.len() {
        0 => {
            eprintln!("没找到 id 以「{prefix}」开头的任务");
            None
        }
        1 => Some(hit.into_iter().next().unwrap()),
        n => {
            eprintln!("「{prefix}」匹配到 {n} 个任务，请给更长的 id 前缀");
            None
        }
    }
}

/// parser 已按既有优先级选出最终来源后，再收敛来源 bot；显式 --source-bot 因此可以
/// 覆盖坏 env，而真正采用 env 时仍在入队前失败。env chat 若形如 buzz UUID，也按
/// 最终来源 bot 的主会话校正（仅当 parser 确实采用了 env chat）。
fn converge_deliver_source(
    cfg: &config::Config,
    item: &mut deliver::DeliveryItem,
    env_chat: &str,
) -> Result<(), String> {
    if item.source_bot.is_empty() {
        return Ok(());
    }
    item.source_bot = cfg.resolve_bot_key(&item.source_bot)?;
    if item.in_session {
        item.target_bot = item.source_bot.clone();
    }
    if !env_chat.is_empty() && item.source_chat == env_chat {
        let primary = cfg
            .bots
            .iter()
            .find(|b| b.key() == item.source_bot)
            .map(|b| b.primary_chat_id.clone())
            .unwrap_or_default();
        let (chat, fell_back) = deliver::correct_injected_chat(env_chat, &primary);
        if fell_back {
            eprintln!(
                "⚠️ AGENT_BRIDGE_CHAT_ID「{env_chat}」形如 buzz 频道 UUID，不是平台 chat_id，已回落到该 bot 主会话「{chat}」（否则会被平台判定 invalid receive_id，如飞书 230001）"
            );
        }
        item.source_chat = chat.clone();
        if item.in_session {
            item.target_chat = chat;
        }
    }
    Ok(())
}

/// 跨会话投递 CLI（供 claude 用 Bash 调用，也可人用）。退出码 0=已入队 1=失败。
/// 来源缺省取 AGENT_BRIDGE_BOT_KEY / AGENT_BRIDGE_CHAT_ID（桥 spawn agent 时注入）。
/// 目标缺省 = 创建者会话（不给 --bot/--chat/--to-current 时，等价 --to-current）。
/// 投递是异步的：CLI 只负责校验 + 入队，service 侧投递循环实际发送。
fn run_deliver_cli(args: &[String]) -> i32 {
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    let env_bot_raw = std::env::var("AGENT_BRIDGE_BOT_KEY").unwrap_or_default();
    let env_chat_raw = std::env::var("AGENT_BRIDGE_CHAT_ID").unwrap_or_default();
    // @角色名寻址（#75 虚拟 Bot）：--chat @后端开发 → 查登记表解析成 chat_id；
    // 找不到报错并列出该 bot 可用角色。登记表与 service 注入判定共用同一份。
    let roles = crate::virtualbot::VirtualBotStore::new();
    let mut item = match deliver::parse_deliver_args_with_store(
        args,
        &env_bot_raw,
        &env_chat_raw,
        &roles,
    ) {
        Ok(i) => i,
        Err(e) => {
            eprintln!(
                "{e}\n用法：agent-bridge deliver --text \"内容\" [--file <本地路径>]…（缺省发回创建者会话）\n      agent-bridge deliver --to-current --text \"内容\" …（同上，显式写法）\n      agent-bridge deliver --bot <目标bot key> --chat <目标chat_id|@角色名> --text \"内容\" [--file <本地路径>]…（跨会话）"
            );
            return 1;
        }
    };
    if let Err(e) = converge_deliver_source(&cfg, &mut item, &env_chat_raw) {
        eprintln!("{e}");
        return 1;
    }
    // 「跨会话投递」开关只管控**跨会话**：`--to-current` 是发给当前会话（等价于
    // 「回复带附件」），不跨会话、也不是新风险面，不该逼用户为一个"给自己发文件"
    // 去打开跨会话开关（冒烟测试暴露：原先开关检查在解析之前，把 in_session 一起挡了）。
    if !item.in_session && !cfg.cross_delivery_enabled {
        eprintln!("跨会话投递未开启：请在 ABB 设置里勾选「跨会话投递」后重试（保存即重启服务）。");
        return 1;
    }
    // 自环防护（消息循环防护 #21）：同 bot 同会话转发给自己没有意义且是循环温床。
    // 例外只有一条：`--to-current` 显式声明的"发给当前会话"。它**不是**放宽这条规则——
    // 目标的来源必须确实等于目标（解析时已强制目标=来源=桥注入的当前会话），语义是
    // "发送"而非"投递"，没有跨会话对；且 bot 自己的出站消息在三个平台都被桥丢弃
    //（微信 message_type!=1、飞书 sender_type=app/bot、钉钉回调只在被 @ 时触发），
    // 不会回灌成新的用户输入，结构上不可能自循环。
    if deliver::is_self_loop(&item) && !item.in_session {
        eprintln!(
            "不能投递回当前会话（来源与目标相同），已拒绝。\n\
             要把内容/文件发到当前会话，请用 --to-current（显式声明）。"
        );
        return 1;
    }
    // in_session 的前提是"来源确实等于目标"：不满足 = 参数拼错或来源被改写，直接拒。
    if item.in_session && !deliver::is_self_loop(&item) {
        eprintln!("--to-current 只能在 bot 会话内使用（来源与目标必须一致），已拒绝。");
        return 1;
    }
    // 授权者（受限会话）纵深防御：--file 只能投递**桥注入身份**的工作区内文件。
    // 显式 --source-bot 在 owner 会话仍是合法覆盖；granted 会话不能用它切换附件边界。
    let sender_role = config::SenderRole::from_env();
    let session_bot = if sender_role == config::SenderRole::Granted {
        let raw = env_bot_raw.trim();
        if raw.is_empty() {
            eprintln!("受限会话缺少 AGENT_BRIDGE_BOT_KEY，无法确认附件工作区边界，已拒绝");
            return 1;
        }
        match cfg.resolve_bot_key(raw) {
            Ok(k) => Some(k),
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    } else {
        None
    };
    if sender_role == config::SenderRole::Granted {
        let session_bot = session_bot.as_deref().expect("granted session bot");
        if item.source_bot != session_bot {
            eprintln!(
                "受限会话不能修改来源身份（env={session_bot:?}，实际 source_bot={:?}），已拒绝",
                item.source_bot
            );
            return 1;
        }
        // 工作区先 canonicalize：a.path 已按真实路径规范化，若 ~/.agent-bridge
        // 含符号链接组件（数据目录挪盘等），原始路径比较会误拒所有合法投递。
        let ws = std::fs::canonicalize(crate::workspace_dir(session_bot))
            .unwrap_or_else(|_| crate::workspace_dir(session_bot));
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

/// session-import 的 bot 列表收敛：显式 --bot 走唯一解析点；否则枚举所有规范 key。
fn session_import_bot_keys(
    cfg: &config::Config,
    explicit: Option<&str>,
) -> Result<Vec<String>, String> {
    match explicit {
        Some(key) => Ok(vec![cfg.resolve_bot_key(key)?]),
        None => cfg
            .bots
            .iter()
            .map(|b| cfg.resolve_bot_key(&b.key()))
            .collect(),
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
    // 枚举 bot：--bot 指定单个；否则全部 bot。显式值必须在任何 import 副作用前收敛。
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读 config 失败: {e:#}");
            return 1;
        }
    };
    let keys = match session_import_bot_keys(&cfg, bot_key.as_deref()) {
        Ok(keys) => keys,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
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
            let env_chat = std::env::var("AGENT_BRIDGE_CHAT_ID").unwrap_or_default();
            let chat = match session_reset_chat_id(&args[1..], &env_chat) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            // #194：虚拟 Bot 群的 reset 路由到独立工作区的 sessions.json
            let store = sessions::SessionStore::store_for_chat(&bot_key, &chat);
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

/// 工作区版本管理 CLI（#209 批次 4/5）：快照历史 / 按快照恢复 / 保护状态一览。
/// bot 缺省从 AGENT_BRIDGE_BOT_KEY env 解析（同 trash）。
fn run_wsver_cli(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        eprintln!("用法：agent-bridge wsver log|restore <commit> <path>|status [--bot <key>]");
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
    match sub {
        "log" => {
            // 无仓库给可操作文案而非裸 libgit2 错误（审查 P3-4；restore 同）
            if !crate::wsver::repo_status(&workspace).has_repo {
                eprintln!("工作区还没有 git 仓库：启用 config workspace_git_enabled 后，bot 下次启动自动 init");
                return 1;
            }
            // -n <N>：条数（默认 10）
            let limit = args
                .iter()
                .position(|a| a == "-n")
                .and_then(|i| args.get(i + 1))
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(10);
            match crate::wsver::log(&workspace, limit) {
                Ok(entries) if entries.is_empty() => {
                    println!("无快照（bot 下次启动自动 init 工作区仓库）");
                    0
                }
                Ok(entries) => {
                    println!("工作区 {bot_key} 快照历史（新→旧）：");
                    for (h, t, msg) in entries {
                        println!("{h} | {t} | {msg}");
                    }
                    println!("恢复：agent-bridge wsver restore <hash> <路径>");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "restore" => {
            if !crate::wsver::repo_status(&workspace).has_repo {
                eprintln!("工作区还没有 git 仓库：启用 config workspace_git_enabled 后，bot 下次启动自动 init");
                return 1;
            }
            let (rev, path) = match (args.get(1), args.get(2)) {
                (Some(r), Some(p)) => (r.as_str(), p.as_str()),
                _ => {
                    eprintln!(
                        "用法：agent-bridge wsver restore <commit> <path> [--bot <key>]\n（路径为工作区内相对路径；先自动打「恢复前快照」，恢复错了还能退回）"
                    );
                    return 2;
                }
            };
            match crate::wsver::restore_path(&workspace, rev, path) {
                Ok(n) => {
                    println!("已从快照 {rev} 恢复 {path}（{n} 个文件）；恢复前状态已自动快照（wsver log 查看）");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "status" => {
            let git_enabled = crate::config::Config::load()
                .map(|c| c.workspace_git_enabled)
                .unwrap_or(true);
            let s = crate::wsver::repo_status(&workspace);
            println!(
                "工作区版本管理：{}",
                if git_enabled {
                    "已启用"
                } else {
                    "已关闭（config workspace_git_enabled）"
                }
            );
            if !s.has_repo {
                println!("仓库：无（启用状态下 bot 下次启动自动 init）");
            } else {
                println!(
                    "仓库：有（HEAD {}，共 {} 个快照）",
                    s.head.as_deref().unwrap_or("(空仓)"),
                    s.commit_count
                );
            }
            println!(
                "回收站：{} 条（/trash list 查看）",
                crate::trash::list(&workspace).len()
            );
            0
        }
        other => {
            eprintln!("未知 wsver 子命令：{other}");
            2
        }
    }
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
                    // 恢复点列（批次 5）：git:<hash> = 快照可恢复；git:- = 仅回收站
                    let snap = match &it.snapshot {
                        Some(h) => format!("git:{h}"),
                        None => "git:-".to_string(),
                    };
                    println!(
                        "{} | {} | {} MB | {} 天前 | {}{} | {}",
                        it.id,
                        crate::trash::pretty_path(std::path::Path::new(&it.orig)),
                        it.size / (1024 * 1024),
                        days_ago,
                        it.reason,
                        if it.dangerous { " | ⚠️危险" } else { "" },
                        snap
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
                "已永久清理回收站条目 {} 条{}（不可恢复；git 快照中的历史版本不受影响）",
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
                    // 批次 5：如实展示恢复路径（快照恢复点 / 仅回收站；忽略类路径
                    // 不入快照时明示——审查 P3-2，与 guard 回执同口径）
                    let rels: Vec<&std::path::Path> = std::path::Path::new(&it.orig)
                        .strip_prefix(&workspace)
                        .ok()
                        .map(|p| vec![p])
                        .unwrap_or_default();
                    let prot = crate::wsver::prot_phrase(
                        &workspace,
                        it.snapshot.as_deref(),
                        settings.git_enabled,
                        &rels,
                    );
                    println!(
                        "已确认并移入回收站：{}（{} 天内可恢复，{prot}）",
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
/// 用法：agent-bridge team generate "<目标>" [--members "小王,steven"] [--template 软件产品团队]
/// （--backend 已废弃：单后端化 P3.4 后统一由随包 buzz-agent 执行，传入仅警告并忽略）
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
            eprintln!("用法：agent-bridge team generate \"<团队目标>\" [--members \"小王,steven\"] [--template 软件产品团队]\n       agent-bridge team templates");
            return 2;
        }
    }
    let mut goal = String::new();
    let mut members: Vec<String> = Vec::new();
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
                // 单后端化 P3.4：仍解析该 flag（兼容旧脚本），但接受并忽略——
                // 统一由随包 buzz-agent 执行。
                i += 1;
                if args.get(i).is_some() {
                    eprintln!("⚠️ --backend 已忽略：单后端化后统一由随包 buzz-agent 执行");
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
    let cfg = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读取配置失败：{e:#}");
            return 1;
        }
    };
    // bot 选择（保留旧 env 契约）：AGENT_BRIDGE_BOT_KEY 非空且命中 → 该 bot；
    // env **设了但未命中** → 明确报错（供应商硬闸：旧路径此场景经 build_injection
    // (Codex, None) 拒答，绝不允许显式指定的供应商路由被静默替换为另一 bot 的
    // 供应商——审查 P1；与 job CLI 的 env→唯一 bot→报错 契约同款）；env 未设/空
    // → 第一个 enabled bot；都没有 → 引导配置。
    let bot = match std::env::var("AGENT_BRIDGE_BOT_KEY") {
        Ok(bk) if !bk.is_empty() => match cfg.bots.iter().find(|b| b.key() == bk) {
            Some(b) => Some(b.clone()),
            None => {
                eprintln!("AGENT_BRIDGE_BOT_KEY（{bk}）未命中任何 bot（供应商硬闸：不做静默回落，请修正 env 或在 GUI 配置该 bot）");
                return 1;
            }
        },
        _ => cfg.bots.iter().find(|b| b.enabled).cloned(),
    };
    let Some(bot) = bot else {
        eprintln!("请先在 GUI 配置 bot");
        return 1;
    };
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("运行时创建失败：{e}");
            return 1;
        }
    };
    match rt.block_on(crate::teambuilder::generate_team_plan(
        &bot,
        &cfg,
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
    let cfg = config::Config::load().map_err(|e| format!("读 config 失败: {e:#}"))?;
    let env = std::env::var("AGENT_BRIDGE_BOT_KEY").ok();
    trash_bot_key_with(args, env.as_deref(), &cfg)
}

/// trash/wsver 共用的纯解析实现（env/cfg 可注入，单测不碰真实 HOME）。
fn trash_bot_key_with(
    args: &[String],
    env: Option<&str>,
    cfg: &config::Config,
) -> Result<String, String> {
    if let Some(i) = args.iter().position(|a| a == "--bot") {
        let value = args.get(i + 1).ok_or_else(|| "--bot 缺少值".to_string())?;
        return cfg.resolve_bot_key(value);
    }
    if let Some(value) = env.filter(|v| !v.trim().is_empty()) {
        return cfg.resolve_bot_key(value);
    }
    Err(
        "缺少 bot key：请用 --bot <key> 指定，或在桥注入环境（AGENT_BRIDGE_BOT_KEY）下调用"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        describe_trigger, parse_task_to, read_task_logs, session_reset_chat_id, take_proc_cmd,
        TASK_ADD_USAGE,
    };

    /// #312 审查：`task --help` / `-h` / `help` 必须**真**走帮助臂——落到 `other` 会先
    /// 多打一行「不认识的子命令」，而放到 `resolve_bot_key()` 之后又会让未配置 bot 的
    /// 环境连帮助都看不了。
    ///
    /// **真调 `run_task_cli`**（不是重写一遍 `matches!`）：这条断言在删掉帮助臂时必红
    /// ——那正是上一版「空转假绿」被审查否掉的原因（把臂删掉，自证式断言照样绿）。
    #[test]
    fn task_help_returns_zero_without_configured_bot() {
        // 帮助必须在解析 bot **之前**返回：这里不设任何 env、也不碰磁盘。
        for a in ["-h", "--help", "help"] {
            assert_eq!(
                super::run_task_cli(&[a.to_string()]),
                0,
                "task {a} 应打印帮助并 exit 0（且不得要求先配好 bot）"
            );
        }
        // 反面：不存在的子命令仍是失败（1）——证明上面那条不是「恒返回 0」。
        assert_ne!(
            super::run_task_cli(&["definitely-not-a-subcommand".to_string()]),
            0,
            "未知子命令不应伪装成功"
        );
    }

    #[test]
    fn proc_cmd_keeps_argv_elements_without_shell_joining() {
        let args = vec![
            "--cmd".to_string(),
            "/bin/echo".to_string(),
            "hello world".to_string(),
            "--literal-option".to_string(),
        ];
        assert_eq!(
            take_proc_cmd(&args, 0).unwrap(),
            vec![
                "/bin/echo".to_string(),
                "hello world".to_string(),
                "--literal-option".to_string()
            ],
            "每个元素必须保持独立 argv，不能拼成 shell 串"
        );
        assert!(take_proc_cmd(&["--cmd".to_string()], 0).is_err());
        assert!(
            TASK_ADD_USAGE.contains("--proc") && TASK_ADD_USAGE.contains("--cmd <argv…>"),
            "usage 必须暴露人工/GUI proc 入口"
        );
    }

    /// P4：`task logs --all` 从 `.2` 到当前按最老→最新拼接，`--tail` 在完整
    /// 拼接结果上截末尾；默认仍只读当前 `.log`。
    #[test]
    fn task_logs_all_orders_rotated_files_and_tails_merged_output() {
        let root = std::env::temp_dir().join(format!("abb-task-logs-all-{}", uuid::Uuid::new_v4()));
        let paths = crate::task_store::TaskPaths::with_root(&root, "bot");
        std::fs::create_dir_all(paths.logs_dir()).unwrap();
        let current = paths.log_file("tk_log");
        std::fs::write(&current, "current-1\ncurrent-2\n").unwrap();
        std::fs::write(format!("{}.1", current.display()), "middle-1\nmiddle-2\n").unwrap();
        std::fs::write(format!("{}.2", current.display()), "old-1\nold-2\n").unwrap();

        assert_eq!(
            read_task_logs(&paths, "tk_log", false, 200).unwrap(),
            "current-1\ncurrent-2"
        );
        assert_eq!(
            read_task_logs(&paths, "tk_log", true, 200).unwrap(),
            "old-1\nold-2\nmiddle-1\nmiddle-2\ncurrent-1\ncurrent-2"
        );
        assert_eq!(
            read_task_logs(&paths, "tk_log", true, 3).unwrap(),
            "middle-2\ncurrent-1\ncurrent-2"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// P2b-C：`task list/status` 的触发档描述（interval 要说人话）。
    #[test]
    fn describe_trigger_renders_all_kinds() {
        use crate::task_store::{TaskTrigger, TriggerKind};
        let mk = |kind, expr: &str| TaskTrigger {
            kind,
            expr: expr.to_string(),
            ..Default::default()
        };
        assert_eq!(describe_trigger(&mk(TriggerKind::Now, "")), "立即");
        assert_eq!(
            describe_trigger(&mk(TriggerKind::Once, "2026-09-20 09:00")),
            "一次性 2026-09-20 09:00"
        );
        assert_eq!(
            describe_trigger(&mk(TriggerKind::Cron, "30 9 * * *")),
            "周期 30 9 * * *"
        );
        assert_eq!(
            describe_trigger(&mk(TriggerKind::Interval, "5m")),
            "每 5 分钟",
            "interval 要渲染成人话而不是裸 expr"
        );
        assert_eq!(describe_trigger(&mk(TriggerKind::Keepalive, "")), "常驻");
    }

    /// #306：`task add --to` 的值解析只按**第一个**冒号切，缺 bot_key 段 = 本 bot；
    /// 空 chat 段留给调用方报错（这里把语义钉死，别让 CLI 与指引各写一套）。
    #[test]
    fn task_to_parses_bot_and_chat() {
        assert_eq!(
            parse_task_to("wx_bot:oc_abc"),
            ("wx_bot".to_string(), "oc_abc".to_string())
        );
        // 只给 chat_id：bot_key 空 = 本 bot
        assert_eq!(
            parse_task_to("oc_abc"),
            (String::new(), "oc_abc".to_string())
        );
        // `:oc_abc` 同义（显式留空 bot_key）
        assert_eq!(
            parse_task_to(":oc_abc"),
            (String::new(), "oc_abc".to_string())
        );
        // chat_id 里再出现冒号不当作分隔（只按第一个切）
        assert_eq!(parse_task_to("b:c:d"), ("b".to_string(), "c:d".to_string()));
        assert_eq!(parse_task_to("b:"), ("b".to_string(), String::new()));
    }

    #[test]
    fn atomic_write_text_concurrent_no_race() {
        // #137：并发写同一目标（定时任务多触发 / CLI 与 service 并存）不因固定 tmp 名
        // 竞争失败——旧实现固定 `path.with_extension("tmp")`，A rename 后 B 的
        // rename 拿不到 tmp → ENOENT。新实现唯一 tmp，并发全部成功且无残留。
        let dir = std::env::temp_dir().join(format!("abb-atomic-race-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("guard/settings.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();

        // 模拟两进程并发：每个写方各写 20 次（固定名下必然撞车）
        let mut handles = Vec::new();
        for w in 0..2 {
            let t = target.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    super::atomic_write_text(&t, &format!("writer{w}-{i}")).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // 最终文件内容完整（最后一次写入的某个 writer 的值，非空即对）
        let final_text = std::fs::read_to_string(&target).unwrap();
        assert!(
            final_text.starts_with("writer"),
            "文件内容应完整：{final_text}"
        );
        // 无 tmp 残留
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应有 tmp 残留: {:?}", leftovers);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
