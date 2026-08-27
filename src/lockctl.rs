//! #129 锁屏控制 —— 特权助手（abb-helper）客户端。
//!
//! 职责：运维 root 特权助手（安装/卸载/状态）＋ 向助手发解锁指令（密码瞬态转发）。
//!
//! 安全设计（fail-closed，与 issue #129 对齐）：
//! - 安装 = 显式弹管理员授权框（osascript `with administrator privileges`），用户不点
//!   同意则不装任何东西；卸载完整移除（binary + launchd plist + socket）。
//! - 解锁前置两道闸：全局开关 `config.lock_screen_control`（默认关）＋ 助手侧对等校验
//!   （socket 0600 + peer uid + 进程路径 + 签名，见 src/bin/abb-helper.rs）。
//! - 密码只在本进程内存瞬态存在：序列化进请求后立即覆写清零；不落盘、不进日志、
//!   不参与事件溯源、不跨会话投递。
//!
//! 未装/未运行助手时所有操作明确报错（不静默）。

#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// launchd 服务名（root daemon）。
pub const HELPER_LABEL: &str = "com.sqb.abb-helper";
/// 助手二进制落点（/Library/PrivilegedHelperTools 是 Apple 特权助手惯例目录）。
pub const HELPER_TOOL_PATH: &str = "/Library/PrivilegedHelperTools/com.sqb.abb-helper";
/// launchd daemon plist 落点。
pub const HELPER_PLIST_PATH: &str = "/Library/LaunchDaemons/com.sqb.abb-helper.plist";
/// 助手监听的本机 unix socket（root 创建，0600 + chown 安装用户）。
pub const SOCKET_PATH: &str = "/var/run/com.sqb.abb-helper.sock";

/// 是否已注册为 launchd 服务（launchctl print system/<label> 成功即注册）。
pub fn helper_installed() -> bool {
    std::process::Command::new("/bin/launchctl")
        .args(["print", &format!("system/{HELPER_LABEL}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 向助手发一条命令（4 字节大端长度 + JSON），返回响应 JSON。
fn send_cmd(cmd: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut sock = UnixStream::connect(SOCKET_PATH).map_err(|e| {
        format!(
            "无法连接特权助手（{SOCKET_PATH}）：{e} —— 助手未安装或未运行，先执行 `--lockctl install`"
        )
    })?;
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    let payload = serde_json::to_vec(cmd).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    sock.write_all(&buf).map_err(|e| e.to_string())?;
    sock.flush().ok();
    let mut len_b = [0u8; 4];
    sock.read_exact(&mut len_b).map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(len_b) as usize;
    if len > 1 << 20 {
        return Err("助手响应过大".into());
    }
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

/// 状态摘要：not-installed | installed(ok|unreachable)。
pub fn status() -> String {
    if !helper_installed() {
        return format!("not-installed（特权助手未安装，可执行 `--lockctl install`）");
    }
    match send_cmd(&serde_json::json!({ "cmd": "status" })) {
        Ok(v) => {
            let pid = v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0);
            if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                format!("installed; daemon=ok (pid={pid})")
            } else {
                format!(
                    "installed; daemon=error ({})",
                    v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown")
                )
            }
        }
        Err(e) => format!("installed; daemon=unreachable ({e})"),
    }
}

/// 安装特权助手：复制二进制 + 写 launchd plist + bootstrap，全部经一次显式管理员授权。
/// 用户取消授权 → osascript 报错 → 返回 Err，**不装任何东西**（fail-closed）。
pub fn install() -> Result<String, String> {
    if helper_installed() {
        return Ok("privileged helper already installed".to_string());
    }
    let helper_src = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("abb-helper")))
        .ok_or_else(|| "无法定位当前可执行文件目录".to_string())?;
    if !helper_src.exists() {
        return Err(format!(
            "找不到 abb-helper 二进制（{}）——打包时必须把 abb-helper 放在主程序同目录",
            helper_src.display()
        ));
    }
    let uid = unsafe { libc::getuid() };
    let plist = build_plist(uid)?;
    // 先把 plist 写到用户可写的临时文件（管理员脚本只负责复制 + 装载，不生成内容）。
    let tmp_plist = std::env::temp_dir().join(format!("abb-helper-{}.plist", std::process::id()));
    std::fs::write(&tmp_plist, &plist).map_err(|e| format!("写临时 plist 失败: {e}"))?;

    let sh = format!(
        "install -m 755 {hs} {ht} && install -m 644 {tp} {hp} && (launchctl bootout system/{label} 2>/dev/null || true) && launchctl bootstrap system {hp}",
        hs = shq(&helper_src),
        ht = shq(Path::new(HELPER_TOOL_PATH)),
        tp = shq(&tmp_plist),
        hp = shq(Path::new(HELPER_PLIST_PATH)),
        label = HELPER_LABEL,
    );
    let r = run_admin(&sh);
    let _ = std::fs::remove_file(&tmp_plist); // 清理临时 plist（不敏感，仍删）
    match r {
        Ok(out) => {
            if helper_installed() {
                Ok("privileged helper installed（launchd daemon 已注册，socket 就绪）".to_string())
            } else {
                Err(format!("安装脚本执行完成但助手未注册：{out}"))
            }
        }
        Err(e) => Err(format!("安装被取消或失败（未安装任何东西）：{e}")),
    }
}

/// 卸载特权助手：bootout + 删 binary/plist/socket。同样经显式管理员授权。
pub fn uninstall() -> Result<String, String> {
    if !helper_installed() {
        // 未注册也把残留文件清一遍（幂等）
        let sh = format!(
            "rm -f {ht} {hp} {sock}",
            ht = shq(Path::new(HELPER_TOOL_PATH)),
            hp = shq(Path::new(HELPER_PLIST_PATH)),
            sock = shq(Path::new(SOCKET_PATH)),
        );
        run_admin(&sh)?;
        return Ok("not installed; residual files cleaned".to_string());
    }
    let sh = format!(
        "(launchctl bootout system/{label} 2>/dev/null || true) && rm -f {ht} {hp} {sock}",
        label = HELPER_LABEL,
        ht = shq(Path::new(HELPER_TOOL_PATH)),
        hp = shq(Path::new(HELPER_PLIST_PATH)),
        sock = shq(Path::new(SOCKET_PATH)),
    );
    run_admin(&sh)?;
    if helper_installed() {
        Err("卸载后助手仍注册（launchctl 未生效？请重启后重试）".to_string())
    } else {
        Ok("privileged helper removed".to_string())
    }
}

/// 解锁：把用户提供的密码按键瞬态注入 loginwindow。
/// 前置闸：config.lock_screen_control 必须为 true；密码非空且 ≤512 字符。
/// 失败/超时即丢弃，助手侧不重试（防暴力尝试）。密码在发送后立即覆写清零。
/// Stage 2：agent 集成（用户当前会话提供密码 → 调本入口）；当前 CLI/GUI 未接线。
#[allow(dead_code)] // Stage 2 agent 集成入口（#129 待真机验证项）
pub fn unlock(mut password: String) -> Result<(), String> {
    let cfg = crate::config::Config::load().map_err(|e| format!("读配置失败: {e}"))?;
    if !cfg.lock_screen_control {
        return Err(
            "锁屏控制开关未开启（config lock_screen_control=false）——请在 GUI 设置中显式开启"
                .into(),
        );
    }
    if password.is_empty() {
        return Err("密码为空".into());
    }
    if password.len() > 512 {
        wipe(&mut password);
        return Err("密码过长（>512 字符）".into());
    }
    if !helper_installed() {
        wipe(&mut password);
        return Err("特权助手未安装（`--lockctl install`）".into());
    }
    let cmd = serde_json::json!({
        "cmd": "unlock",
        "password": password,
        "timeout_ms": 8000,
    });
    let r = send_cmd(&cmd);
    wipe(&mut password); // 瞬态密码立即清零（不落盘/不进日志）
    match r {
        Ok(v) if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) => Ok(()),
        Ok(v) => Err(v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error")
            .to_string()),
        Err(e) => Err(e),
    }
}

/// 生成 launchd daemon plist：ProgramArguments + LOCKCTL_UID 环境变量（助手 chown socket 用）。
fn build_plist(uid: u32) -> Result<String, String> {
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{tool}</string>
    <string>--helper-daemon</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>LOCKCTL_UID</key><string>{uid}</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/var/log/{label}.log</string>
  <key>StandardErrorPath</key><string>/var/log/{label}.log</string>
</dict>
</plist>
"#,
        label = HELPER_LABEL,
        tool = HELPER_TOOL_PATH,
        uid = uid,
    );
    Ok(xml)
}

/// 单引号 shell 引用（路径防注入）。
fn shq(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

/// 以管理员权限执行 shell 脚本（弹系统管理员授权框；用户取消 → Err）。
fn run_admin(script: &str) -> Result<String, String> {
    let out = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!(
                "do shell script \"{}\" with administrator privileges",
                script.replace('"', "\\\"")
            ),
        ])
        .output()
        .map_err(|e| format!("osascript 启动失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(format!("{stderr}").trim().to_string())
    }
}

/// 内存覆写清零（瞬态密码用；不保证编译器不优化，但配合 move 语义足够 Stage 1）。
#[allow(dead_code)] // 仅 unlock 路径使用（Stage 2 接线后移除）
fn wipe(s: &mut String) {
    unsafe {
        for b in s.as_bytes_mut() {
            *b = 0;
        }
    }
    s.clear();
}
