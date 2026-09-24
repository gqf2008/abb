//! ABB Windows 提权 helper（A1 第一批）。
//!
//! 由 `agent-bridge` 用 `ShellExecuteExW "runas"` 拉起，**处理一个请求即退出**——它是
//! 「用到时才提权、干完即退」的短命进程，不是常驻服务。
//!
//! ## 用法
//! ```text
//! abb-elev-helper --pipe \\.\pipe\abb-elev-helper-<调用方 pid>-<随机> --token <32 位十六进制>
//! ```
//!
//! ## 安全模型（fail-closed，细节见 `src/elev/mod.rs`）
//! - 管道 DACL 只放「当前用户 SID + SYSTEM + Administrators」；`--token` 是第二道
//!   （残余风险：提权进程的命令行同用户可读，令牌只防跨用户冒用）；
//! - 请求令牌不匹配 → **先记审计再拒绝**，不透露白名单、不碰系统调用；
//! - 操作白名单固定三个，表外名字在触碰系统调用之前就拒绝；
//! - 审计写失败 ⇒ 不执行；结果审计写失败 ⇒ 不回成功（三个操作都幂等，可安全重试）；
//! - 密码只在内存里传递：校验 → 交给注册表后端 → 立刻清零，绝不进日志/审计/响应；
//! - 120s 兜底超时：到点无条件退出，绝不常驻。
//!
//! ## 退出码
//! - `0`：正常处理完一个请求（**结果在响应体里**，成功与否都算 0；非 0 表示没走到
//!   「拿到请求」这一步，此时调用方不应相信任何响应）；
//! - `2`：命令行参数缺失/非法；
//! - `3`：看门线程超时兜底；
//! - `4`：管道创建/连接/收发失败；
//! - `5`：审计目录不可用（无法 fail-closed 记账）；
//! - `6`：审计通过但响应没能写出（调用方会看到管道断开）。
//!
//! 非 Windows 平台为占位 main（拒绝运行），对齐既有 `src/bin/abb-helper.rs` 的写法。

use std::process::ExitCode;

// 退出码只在 Windows 分支用到 → 必须 `cfg` 门控，否则 macOS/Linux 上会因
// `dead_code` 在 `-D warnings` 下直接红（对齐既有 `abb-helper` 的写法）。

/// helper 正常处理完一个请求。
#[cfg(target_os = "windows")]
const EXIT_OK: u8 = 0;
/// 命令行参数错误。
#[cfg(target_os = "windows")]
const EXIT_USAGE: u8 = 2;
/// 看门超时。
#[cfg(target_os = "windows")]
const EXIT_TIMEOUT: u8 = 3;
/// 管道相关失败。
#[cfg(target_os = "windows")]
const EXIT_PIPE: u8 = 4;
/// 审计目录不可用。
#[cfg(target_os = "windows")]
const EXIT_AUDIT: u8 = 5;
/// 响应写出失败。
#[cfg(target_os = "windows")]
const EXIT_REPLY: u8 = 6;

fn main() -> ExitCode {
    #[cfg(target_os = "windows")]
    {
        run()
    }
    #[cfg(not(target_os = "windows"))]
    {
        eprintln!("abb-elev-helper is Windows-only");
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "windows")]
fn run() -> ExitCode {
    use agent_bridge::elev::{self, win};

    let argv: Vec<String> = std::env::args().collect();
    if argv.iter().any(|a| a == "--version") {
        println!("abb-elev-helper {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::from(EXIT_OK);
    }
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("usage: abb-elev-helper --pipe <pipe-name> --token <32-hex>");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    // 兜底超时：到点无条件退出。任何一步卡住（例如没人来连管道）都不会让它常驻。
    let watchdog = elev::HELPER_TIMEOUT;
    std::thread::spawn(move || {
        std::thread::sleep(watchdog);
        eprintln!("[elev] 看门超时（{}s），退出", watchdog.as_secs());
        std::process::exit(EXIT_TIMEOUT.into());
    });

    let server = match win::PipeServer::create(&args.pipe) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("[elev] 创建管道失败：{e}");
            return ExitCode::from(EXIT_PIPE);
        }
    };
    if let Err(e) = server.accept() {
        eprintln!("[elev] 等客户端连接失败：{e}");
        return ExitCode::from(EXIT_PIPE);
    }
    // 来源 = 调用方进程 pid（取不到才退化为 helper 自己），审计里用它回答「谁提的」。
    let source = match server.client_pid() {
        Some(pid) => format!("pid:{pid}"),
        None => format!("pid:{}", std::process::id()),
    };

    let mut body = match server.read_frame(elev::MAX_BODY) {
        Ok(body) => body,
        Err(e) => {
            eprintln!("[elev] 读请求失败：{e}");
            return ExitCode::from(EXIT_PIPE);
        }
    };

    // 审计目录拿不到 = 无法记账 ⇒ 不执行任何操作（fail-closed），但仍回一个人话回应。
    let dir = match elev::audit_dir_from_env() {
        Ok(dir) => dir,
        Err(e) => {
            let resp = elev::resp_err(
                elev::codes::AUDIT_FAILED,
                "审计目录不可用，已拒绝执行（fail-closed）",
            );
            eprintln!("[elev] {e}");
            let _ = reply(&server, &resp);
            elev::wipe_bytes(&mut body);
            return ExitCode::from(EXIT_AUDIT);
        }
    };

    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let ctx = elev::AuditCtx {
        dir: &dir,
        ts: &ts,
        source: &source,
        pid: std::process::id(),
    };
    let response = elev::handle_request(&body, &args.token, &ctx, &win::WinlogonBackend);
    // 请求体（可能含密码）用完即擦。
    elev::wipe_bytes(&mut body);

    match reply(&server, &response) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(e) => {
            // 审计已经落盘，但调用方拿不到回应 → 非 0 退出，让调用方知道「这次不算数」。
            eprintln!("[elev] 写响应失败：{e}");
            ExitCode::from(EXIT_REPLY)
        }
    }
}

/// 命令行参数。
#[cfg(target_os = "windows")]
struct Args {
    pipe: String,
    token: String,
}

/// 只认 `--pipe` / `--token`（顺序无关，各一次）。缺项/多项/未知项都算用法错误。
#[cfg(target_os = "windows")]
fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut pipe: Option<String> = None;
    let mut token: Option<String> = None;
    let mut it = argv.iter().skip(1);
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--pipe" => &mut pipe,
            "--token" => &mut token,
            other => return Err(format!("未知参数：{other}")),
        };
        if slot.is_some() {
            return Err(format!("参数 {arg} 重复"));
        }
        match it.next() {
            Some(v) => *slot = Some(v.clone()),
            None => return Err(format!("参数 {arg} 缺少取值")),
        }
    }
    let pipe = pipe.ok_or_else(|| "缺少 --pipe".to_string())?;
    let token = token.ok_or_else(|| "缺少 --token".to_string())?;
    if pipe.is_empty() || token.is_empty() {
        return Err("--pipe / --token 不能为空".to_string());
    }
    Ok(Args { pipe, token })
}

/// 把响应序列化后发回调用方。
#[cfg(target_os = "windows")]
fn reply(
    server: &agent_bridge::elev::win::PipeServer,
    response: &agent_bridge::elev::Response,
) -> Result<(), String> {
    let payload = serde_json::to_vec(response).map_err(|e| format!("序列化响应失败：{e}"))?;
    server.write_frame(&payload)
}
