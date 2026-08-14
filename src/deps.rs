//! 依赖检测与安装 —— claude / codex / pi / nodejs / python3 / lark-cli / dingtalk-cli。
//! 跨平台（win/mac/linux）：检测组 PATH 分平台（分隔符、PATHEXT、常见安装目录），
//! 安装命令按平台出（mac 用 brew/npm/curl 安装器，win 用 winget/npm，linux 用 apt/dnf/npm）。
//! 本轮只验证 mac 路径；win/linux 编译可用、不行则给「请手动安装」文案。
//!
//! 从 agent.rs 挪来 composed_path/find_in_path/is_executable（原 Unix-only），并扩展。

use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Windows：spawn 外部子进程时抑制控制台窗口（CREATE_NO_WINDOW）。
/// 桥/服务是 GUI 子系统，spawn reg/npm/cmd/taskkill 等控制台程序默认会弹黑框——
/// 所有「会 spawn 子进程」的地方统一调它（agent 子进程另在 run_once 里对 tokio Command
/// 设置，此处管 std::process::Command 的零散调用）。
#[cfg(windows)]
pub fn apply_no_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}

/// 组 PATH：claude 在 ~/.local/bin，codex/lark-cli 在 ~/.npm-global/bin；launchd 环境精简须显式带。
/// 分平台：分隔符 win 用 `;`、unix 用 `:`；常见安装目录各平台不同。
#[cfg(unix)]
pub fn composed_path() -> String {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let mut parts = vec![
        home.join(".local/bin").to_string_lossy().into_owned(),
        home.join(".npm-global/bin").to_string_lossy().into_owned(),
        home.join(".cargo/bin").to_string_lossy().into_owned(),
        "/opt/homebrew/bin".to_string(),
        "/opt/homebrew/sbin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ];
    // 系统级持久 PATH 源：/etc/paths 与 /etc/paths.d/*（GUI/launchd 启动不读 shell rc，
    // 但会读 /etc/paths.d——用户自定义安装目录常放在这）。
    parts.extend(unix_etc_paths());
    if let Ok(existing) = std::env::var("PATH") {
        parts.push(existing);
    }
    parts.join(":")
}

/// macOS 系统级持久 PATH：/etc/paths（每行一个）+ /etc/paths.d/*（每行一个，`/usr/bin:/bin` 等）。
/// GUI/登录项启动的进程继承 launchd PATH（不含 shell rc 的 export），但会读这些文件——
/// 用户往 /etc/paths.d 加自定义目录后，本桥 launchd 环境也能找到（Docker 用户常踩：
/// shell 里能 which，服务里找不到）。
#[cfg(unix)]
fn unix_etc_paths() -> Vec<String> {
    let mut out = Vec::new();
    for f in ["/etc/paths"] {
        if let Ok(text) = std::fs::read_to_string(f) {
            for line in text.lines() {
                let p = line.trim();
                if !p.is_empty() && !p.starts_with('#') {
                    out.push(p.to_string());
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir("/etc/paths.d") {
        for ent in rd.flatten() {
            if let Ok(text) = std::fs::read_to_string(ent.path()) {
                for line in text.lines() {
                    let p = line.trim();
                    if !p.is_empty() && !p.starts_with('#') {
                        out.push(p.to_string());
                    }
                }
            }
        }
    }
    out
}

#[cfg(windows)]
pub fn composed_path() -> String {
    let mut parts: Vec<String> = Vec::new();
    let push_env = |parts: &mut Vec<String>, var: &str, sub: &str| {
        if let Ok(base) = std::env::var(var) {
            parts.push(PathBuf::from(base).join(sub).to_string_lossy().into_owned());
        }
    };
    push_env(&mut parts, "APPDATA", "npm"); // npm 全局 bin（claude.cmd/codex.cmd/pi.cmd）
    push_env(&mut parts, "LOCALAPPDATA", "Programs\\nodejs");
    push_env(&mut parts, "USERPROFILE", ".local\\bin");
    push_env(&mut parts, "USERPROFILE", ".cargo\\bin");
    push_env(
        &mut parts,
        "USERPROFILE",
        "AppData\\Local\\Microsoft\\WinGet\\Links",
    );
    push_env(&mut parts, "USERPROFILE", "scoop\\shims"); // scoop 安装的 cli shim
                                                         // nvm-windows：npm 全局 bin 落在 nvm 当前版本目录（NVM_HOME 或 %APPDATA%\nvm）
    if let Ok(nvm) = std::env::var("NVM_HOME") {
        parts.push(PathBuf::from(nvm).to_string_lossy().into_owned());
    } else {
        push_env(&mut parts, "APPDATA", "nvm");
    }
    // 注册表持久 PATH（用户 + 系统）：GUI 由登录项/任务计划启动时可能没继承 shell 级配置；
    // 用户往「环境变量」里加的自定义目录（如 pi 装的自定义目录）在这里，显式合并。
    parts.extend(windows_registry_paths());
    // 当前进程 PATH（交互 shell 启动的 GUI/服务可能已带；桌面/开机自启场景精简则靠上面前缀）
    if let Ok(existing) = std::env::var("PATH") {
        parts.push(existing);
    }
    parts.join(";")
}

/// Windows 持久 PATH（注册表）：HKCU\Environment（用户级）与
/// HKLM\...\Session Manager\Environment（系统级）的 Path 值，分号分隔。
/// 通过 `reg query` 读取（零依赖；GUI 环境也能跑 reg.exe）。REG_EXPAND_SZ 里的
/// %VAR% 不展开——多数是绝对路径，够用；展开交给 find_in_path 的逐段探测。
#[cfg(windows)]
fn windows_registry_paths() -> Vec<String> {
    let mut out = Vec::new();
    for scope in [
        "HKCU\\Environment",
        "HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
    ] {
        let mut reg = std::process::Command::new("reg");
        reg.args(["query", scope, "/v", "Path"]);
        apply_no_window(&mut reg);
        let Ok(o) = reg.output() else {
            continue;
        };
        let text = String::from_utf8_lossy(&o.stdout);
        // reg query 行格式：`    Path    REG_EXPAND_SZ    C:\a;C:\b`
        if let Some(line) = text
            .lines()
            .find(|l| l.contains("REG_") && l.contains("Path"))
        {
            if let Some(value) = line
                .split_once("REG_")
                .and_then(|(_, v)| v.split_once(' ').map(|(_, r)| r))
            {
                for part in value.split(';') {
                    let p = part.trim();
                    if !p.is_empty() {
                        out.push(p.to_string());
                    }
                }
            }
        }
    }
    out
}

/// 在 composed PATH 各目录里找可执行文件（等价 `which`，但不依赖外部 which——launchd 环境不一定有）。
/// unix：跟随符号链接（npm 全局 bin 是指向 cli.js 的软链）+ 检查可执行位。
/// windows：按 PATHEXT 试 `name.exe/.cmd/.bat/.com`（npm 全局 shim 是 `claude.cmd`/`codex.cmd`）。
#[cfg(unix)]
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    for dir in composed_path().split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = PathBuf::from(dir).join(name);
        if p.is_file() && is_executable(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(windows)]
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let pathext: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    for dir in composed_path().split(';') {
        if dir.is_empty() {
            continue;
        }
        let base = PathBuf::from(dir);
        // 只按 PATHEXT 找 .com/.exe/.bat/.cmd，**不试无扩展名文件**：npm 在 Windows 会
        // 同时生成 `pi.cmd`（cmd 用）与无扩展 `pi`（Git Bash 用 shell 脚本），若先命中
        // 无扩展脚本直接 CreateProcess 会报 ERROR_BAD_EXE_FORMAT（193「不是有效的 Win32
        // 应用程序」）。可执行按 PATHEXT 顺序（.exe 优先于 .cmd）。
        for ext in &pathext {
            let cand = base.join(format!("{name}{}", ext.to_lowercase()));
            if cand.is_file() {
                return Some(cand);
            }
            let cand_up = base.join(format!("{name}{ext}"));
            if cand_up.is_file() {
                return Some(cand_up);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 单个依赖的检测结果。
#[derive(Debug, Clone)]
pub struct DepStatus {
    /// 机器键：claude | codex | pi | node | python3 | lark-cli | dingtalk-cli
    pub id: &'static str,
    /// 展示名。
    pub label: &'static str,
    pub found: bool,
    /// 找到时的可执行路径（未找到为空）。当前 UI 只显 found，路径留作排障/将来展示。
    #[allow(dead_code)]
    pub path: String,
}

/// 检测全部依赖。设置窗打开 + 「重新检测」时调。
/// node 探 `node`；python 先试 `python3` 再 `python`；lark-cli 用于技能引导门控。
pub fn detect_all() -> Vec<DepStatus> {
    let probe = |id: &'static str, label: &'static str, names: &[&str]| -> DepStatus {
        for n in names {
            if let Some(p) = find_in_path(n) {
                return DepStatus {
                    id,
                    label,
                    found: true,
                    path: p.to_string_lossy().into_owned(),
                };
            }
        }
        DepStatus {
            id,
            label,
            found: false,
            path: String::new(),
        }
    };
    vec![
        probe("claude", "Claude Code", &["claude"]),
        probe("codex", "Codex CLI", &["codex"]),
        // pi：npm 全局 bin（~/.npm-global/bin/pi，软链到 pi-coding-agent 的 cli.js）
        probe("pi", "Pi (pi-coding-agent)", &["pi"]),
        probe("node", "Node.js", &["node"]),
        probe("python3", "Python 3", &["python3", "python"]),
        probe("lark-cli", "lark-cli", &["lark-cli"]),
        // 钉钉 CLI（dingtalk-workspace-cli，命令名 dws）：接入钉钉 bot 后让 agent 调钉钉能力
        probe("dingtalk-cli", "dingtalk-cli (dws)", &["dws"]),
    ]
}

/// 按 id 查某项的检测状态。
pub fn detect_one(dep_id: &str) -> Option<DepStatus> {
    detect_all().into_iter().find(|d| d.id == dep_id)
}

/// 一个安装步骤：要么是可执行+参数（直接 spawn），要么是 shell 管道（unix 走 bash -c）。
struct InstallStep {
    /// 直执行：程序名（经 composed PATH 找）。
    program: String,
    args: Vec<String>,
    /// shell 管道（如 `curl ... | bash`）：给了它就忽略 program/args，unix 用 bash -c 跑。
    shell: Option<String>,
}

impl InstallStep {
    fn exec(program: &str, args: &[&str]) -> InstallStep {
        InstallStep {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            shell: None,
        }
    }
    // 仅 macOS/Linux 的安装计划用到（curl | sh）；Windows 全是 exec 直装
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    fn shell(cmd: &str) -> InstallStep {
        InstallStep {
            program: String::new(),
            args: Vec::new(),
            shell: Some(cmd.to_string()),
        }
    }
}

/// 出某依赖在当前平台的安装步骤序列（顺序执行，前一步失败即中止）。
/// 不存在的依赖 id / 该平台无法自动装 → Err（用户可见文案）。
/// （各平台是独立 #[cfg] 块，块内显式 return；clippy 的 needless_return 在此不适用。）
#[allow(clippy::needless_return)]
fn install_plan(dep_id: &str) -> Result<Vec<InstallStep>, String> {
    #[cfg(target_os = "macos")]
    {
        let plan = match dep_id {
            // claude 官方原生安装器（无需 node），落 ~/.local/bin；回落 npm。
            "claude" => vec![
                InstallStep::shell("curl -fsSL https://claude.ai/install.sh | bash"),
                InstallStep::exec("npm", &["install", "-g", "@anthropic-ai/claude-code"]),
            ],
            "codex" => vec![
                InstallStep::exec("npm", &["install", "-g", "@openai/codex"]),
                InstallStep::exec("brew", &["install", "codex"]),
            ],
            // pi：官方安装器（curl pi.dev/install.sh，无需 node）；回落 npm。
            "pi" => vec![
                InstallStep::shell("curl -fsSL https://pi.dev/install.sh | sh"),
                InstallStep::exec(
                    "npm",
                    &[
                        "install",
                        "-g",
                        "--ignore-scripts",
                        "@earendil-works/pi-coding-agent",
                    ],
                ),
            ],
            "node" => vec![InstallStep::exec("brew", &["install", "node"])],
            "python3" => vec![InstallStep::exec("brew", &["install", "python"])],
            "lark-cli" => vec![InstallStep::exec(
                "npm",
                &["install", "-g", "@larksuite/cli"],
            )],
            // 钉钉 CLI：npm 为主，回落 brew tap + install（官方同样提供 curl 一行安装器）
            "dingtalk-cli" => vec![
                InstallStep::exec("npm", &["install", "-g", "dingtalk-workspace-cli"]),
                InstallStep::exec(
                    "brew",
                    &[
                        "tap",
                        "DingTalk-Real-AI/dingtalk-workspace-cli",
                        "https://github.com/DingTalk-Real-AI/dingtalk-workspace-cli.git",
                    ],
                ),
                InstallStep::exec("brew", &["install", "dingtalk-workspace-cli"]),
            ],
            other => return Err(format!("未知依赖：{other}")),
        };
        return Ok(plan);
    }
    #[cfg(target_os = "windows")]
    {
        let plan = match dep_id {
            "claude" => vec![InstallStep::exec(
                "npm",
                &["install", "-g", "@anthropic-ai/claude-code"],
            )],
            "codex" => vec![InstallStep::exec(
                "npm",
                &["install", "-g", "@openai/codex"],
            )],
            "pi" => vec![InstallStep::exec(
                "npm",
                &[
                    "install",
                    "-g",
                    "--ignore-scripts",
                    "@earendil-works/pi-coding-agent",
                ],
            )],
            "node" => vec![InstallStep::exec("winget", &["install", "OpenJS.NodeJS"])],
            "python3" => vec![InstallStep::exec(
                "winget",
                &["install", "Python.Python.3.12"],
            )],
            "lark-cli" => vec![InstallStep::exec(
                "npm",
                &["install", "-g", "@larksuite/cli"],
            )],
            "dingtalk-cli" => vec![InstallStep::exec(
                "npm",
                &["install", "-g", "dingtalk-workspace-cli"],
            )],
            other => return Err(format!("未知依赖：{other}")),
        };
        return Ok(plan);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let plan = match dep_id {
            "claude" => vec![
                InstallStep::shell("curl -fsSL https://claude.ai/install.sh | bash"),
                InstallStep::exec("npm", &["install", "-g", "@anthropic-ai/claude-code"]),
            ],
            "codex" => vec![InstallStep::exec(
                "npm",
                &["install", "-g", "@openai/codex"],
            )],
            "pi" => vec![InstallStep::exec(
                "npm",
                &[
                    "install",
                    "-g",
                    "--ignore-scripts",
                    "@earendil-works/pi-coding-agent",
                ],
            )],
            // linux 包管理器按二进制探测：优先 apt-get，其次 dnf。
            "node" => vec![InstallStep::shell(
                "if command -v apt-get >/dev/null; then sudo apt-get install -y nodejs npm; \
                     elif command -v dnf >/dev/null; then sudo dnf install -y nodejs npm; \
                     else echo 'no-supported-pkg-mgr' >&2; exit 1; fi",
            )],
            "python3" => vec![InstallStep::shell(
                "if command -v apt-get >/dev/null; then sudo apt-get install -y python3; \
                     elif command -v dnf >/dev/null; then sudo dnf install -y python3; \
                     else echo 'no-supported-pkg-mgr' >&2; exit 1; fi",
            )],
            "lark-cli" => vec![InstallStep::exec(
                "npm",
                &["install", "-g", "@larksuite/cli"],
            )],
            "dingtalk-cli" => vec![InstallStep::exec(
                "npm",
                &["install", "-g", "dingtalk-workspace-cli"],
            )],
            other => return Err(format!("未知依赖：{other}")),
        };
        return Ok(plan);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = dep_id;
        Err("当前平台不支持自动安装，请手动安装".to_string())
    }
}

/// 跑某依赖的安装：顺序执行 install_plan 的每一步，任一步非零退出即尝试下一步
/// （同 dep 的多步是「主 + 回落」关系）；全部失败 → Err（最后一步的 stderr 尾）。
/// 成功（任一步 0 退出且之后能探到该依赖）→ Ok（安装输出尾部，用于状态行）。
/// 全程不涉密钥；只往日志写命令 + exit + stderr 尾（截断）。
pub async fn run_install(dep_id: &str) -> Result<String, String> {
    let steps = install_plan(dep_id)?;
    let mut last_err = String::new();
    for (i, step) in steps.iter().enumerate() {
        let desc = if let Some(sh) = &step.shell {
            format!("bash -c {:?}", crate::agent::truncate(sh, 60))
        } else {
            format!("{} {}", step.program, step.args.join(" "))
        };
        crate::log!("[deps] 安装 {dep_id} 步骤{}：{}", i + 1, desc);
        match run_step(step).await {
            Ok(tail) => {
                // 该步退出 0：再确认依赖真的可用了（有些安装器 0 退出但需重开 shell 才上 PATH）。
                if detect_one(dep_id).map(|d| d.found).unwrap_or(false) {
                    crate::log!("[deps] {dep_id} 安装成功（步骤{}）", i + 1);
                    return Ok(tail);
                }
                last_err = format!(
                    "步骤{}跑完但未在 PATH 找到 {dep_id}（可能需重开终端/刷新 PATH）",
                    i + 1
                );
                crate::log!(
                    "[deps] {dep_id} 步骤{} 退出0但检测不到，尝试回落步骤",
                    i + 1
                );
            }
            Err(e) => {
                crate::log!("[deps] {dep_id} 步骤{}失败：{}", i + 1, e);
                last_err = e;
            }
        }
    }
    Err(format!("{dep_id} 安装失败：{last_err}"))
}

/// 执行单个安装步骤，捕获 stdout/stderr 尾部。Ok=0 退出（返回输出尾），Err=非零/启动失败。
async fn run_step(step: &InstallStep) -> Result<String, String> {
    let mut cmd = if let Some(sh) = &step.shell {
        let mut c = Command::new("bash");
        c.arg("-c").arg(sh);
        c
    } else {
        let prog = find_in_path(&step.program)
            .ok_or_else(|| format!("找不到 {}（请先装它的前置依赖）", step.program))?;
        let mut c = Command::new(prog);
        c.args(&step.args);
        c
    };
    // Windows：安装依赖（npm.cmd 等）也抑制控制台窗口，别在 GUI 里弹黑框
    #[cfg(windows)]
    {
        apply_no_window(cmd.as_std_mut());
    }
    cmd.env("PATH", composed_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 防安装器交互卡住（npm/brew 遇到提示读 stdin 会 EOF 继续，不会死等）。
        .stdin(Stdio::null());

    let mut child = cmd.spawn().map_err(|e| format!("启动失败：{e}"))?;
    let out_tail = read_tail(child.stdout.take());
    let err_tail = read_tail(child.stderr.take());
    let status = child
        .wait()
        .await
        .map_err(|e| format!("等待退出失败：{e}"))?;
    let out = out_tail.await.unwrap_or_default();
    let err = err_tail.await.unwrap_or_default();
    if status.success() {
        Ok(if out.is_empty() { err } else { out })
    } else {
        Err(format!(
            "退出码 {:?}：{}",
            status.code(),
            if err.is_empty() { out } else { err }
        ))
    }
}

/// 读流，只保留尾部约 800 字符（安装输出可能很长，状态行/日志只要结尾）。
fn read_tail<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    r: Option<R>,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut keep = String::new();
        if let Some(r) = r {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                keep.push_str(&line);
                keep.push('\n');
                if keep.len() > 1600 {
                    // 按字节砍到一半，再对齐到 char 边界
                    let cut = keep.len() - 800;
                    let mut idx = cut;
                    while !keep.is_char_boundary(idx) {
                        idx += 1;
                    }
                    keep.drain(..idx);
                }
            }
        }
        keep.trim_end().to_string()
    })
}

// ═══════════════════════ macOS 系统权限（TCC）检测 ═══════════════════════
// 「完全控制这台主机」所需的四项。检测零依赖：直接查 TCC SQLite 库（macOS 26 实测无列名/版本
// 反查）。授权目标 = 当前可执行文件路径（裸二进制按 TCC「责任进程」记）。

/// 一项系统权限的检测状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermState {
    /// 已授权（auth_value=2）
    Granted,
    /// 被拒绝过（auth_value=3）——TCC 不会再自动弹窗，只能去设置手动开
    /// （仅 macOS/Linux 权限探测构造；Windows 提权检测只产 Granted/NotDetermined）
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    Denied,
    /// 从未请求 / 未授权（auth_value=0 或无行）
    NotDetermined,
}

// ═══════════════════════ Windows 管理员提权 ═══════════════════════
// Windows 无 TCC 细粒度隐私权限；「完全控制主机」= 以管理员身份运行。

/// 检测当前进程是否以管理员/提升权限运行。
#[cfg(target_os = "windows")]
pub fn is_elevated() -> bool {
    // IsUserAnAdmin: shell32.dll，返回 BOOL（非 0 = 管理员组成员）
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    unsafe { IsUserAnAdmin() != 0 }
}

/// 以管理员身份重启自己（ShellExecuteExW "runas"）。成功则当前进程应立即退出。
#[cfg(target_os = "windows")]
pub fn relaunch_as_admin() -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    let exe = std::env::current_exe().map_err(|e| format!("拿不到可执行路径: {e}"))?;
    let exe_w: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb_w: Vec<u16> = "runas\0".encode_utf16().collect();
    let args_w: Vec<u16> = "\0".encode_utf16().collect(); // 不带参数

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteExW(pExecInfo: *mut c_void) -> i32;
    }

    #[repr(C)]
    struct ShellExecuteInfoW {
        cb_size: u32,
        f_mask: u32,
        hwnd: *mut c_void,
        lp_verb: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show: i32,
        h_inst_app: *mut c_void,
        lp_id_list: *mut c_void,
        lp_class: *const u16,
        h_key_class: *mut c_void,
        dw_hot_key: u32,
        h_icon_or_monitor: *mut c_void,
        h_process: *mut c_void,
    }

    let mut sei = ShellExecuteInfoW {
        cb_size: std::mem::size_of::<ShellExecuteInfoW>() as u32,
        f_mask: 0,
        hwnd: std::ptr::null_mut(),
        lp_verb: verb_w.as_ptr(),
        lp_file: exe_w.as_ptr(),
        lp_parameters: args_w.as_ptr(),
        lp_directory: std::ptr::null(),
        n_show: 1, // SW_SHOWNORMAL
        h_inst_app: std::ptr::null_mut(),
        lp_id_list: std::ptr::null_mut(),
        lp_class: std::ptr::null(),
        h_key_class: std::ptr::null_mut(),
        dw_hot_key: 0,
        h_icon_or_monitor: std::ptr::null_mut(),
        h_process: std::ptr::null_mut(),
    };

    let r = unsafe { ShellExecuteExW(&mut sei as *mut ShellExecuteInfoW as *mut c_void) };
    if r == 0 {
        Err("提权失败（用户可能取消了 UAC 弹框）".to_string())
    } else {
        Ok(())
    }
}

/// Windows 权限检测：只查一项「管理员身份」。
#[cfg(target_os = "windows")]
pub fn detect_permissions() -> Vec<PermStatus> {
    vec![PermStatus {
        id: "admin",
        label: "管理员身份运行",
        state: if is_elevated() {
            PermState::Granted
        } else {
            PermState::NotDetermined
        },
        settings_url: "", // Windows 无设置面板 URL，UI 显示「提权重启」按钮
    }]
}

/// Linux 权限检测：无 TCC/UAC 概念，返回空。
#[cfg(all(unix, not(target_os = "macos")))]
pub fn detect_permissions() -> Vec<PermStatus> {
    Vec::new()
}
pub struct PermStatus {
    /// 机器键：full-disk | accessibility | screen | automation | camera | microphone
    pub id: &'static str,
    /// 展示名 + 面板 URL：当前 UI 只按 id 映射，字段保留作排障/将来展示。
    #[allow(dead_code)]
    pub label: &'static str,
    pub state: PermState,
    /// 「去授权」跳转的系统设置面板 URL。
    #[allow(dead_code)]
    pub settings_url: &'static str,
}

/// 查一个 TCC 库里某 service 对本二进制的 auth_value。库不可读/无行 → None。
#[cfg(target_os = "macos")]
fn tcc_auth(db: &std::path::Path, service: &str, client: &str) -> Option<i64> {
    use rusqlite::{Connection, OpenFlags};
    let con = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    // 该 service 下对本 client 可能有多行（不同 csreq/indirect），取最大 auth_value（授权优先）。
    let mut stmt = con
        .prepare("SELECT MAX(auth_value) FROM access WHERE service=?1 AND client=?2")
        .ok()?;
    stmt.query_row(rusqlite::params![service, client], |r| {
        r.get::<_, Option<i64>>(0)
    })
    .ok()
    .flatten()
}

#[cfg(target_os = "macos")]
fn state_of(auth: Option<i64>) -> PermState {
    match auth {
        Some(2) => PermState::Granted, // allowed（含 user-intent 落库）
        Some(3) => PermState::Denied,  // denied
        _ => PermState::NotDetermined, // 0 / 无行 / 其它
    }
}

// ── 权威 API 检测（与系统设置同源）──
// 摄像头/麦克风/屏幕录制这类现代权限，对 ad-hoc 签名、非 bundle 的裸二进制，**授权不写进
// TCC.db 的 client 路径**（存 tccd 内存/按 code identity），读库永远读不到 → 误显「未授权」。
// 系统设置 UI 用的是真实 API，故检测也必须用 API（手写 objc_msgSend FFI，零依赖，同 platform.rs）。

/// AVAuthorizationStatus → PermState。0 NotDetermined / 1 Restricted / 2 Denied / 3 Authorized
#[cfg(target_os = "macos")]
fn av_state(sym: &std::ffi::CStr) -> PermState {
    use std::ffi::c_void;
    type Id = *mut c_void;
    type Sel = *mut c_void;
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> Id;
        fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
        fn objc_msgSend();
        fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
    }
    unsafe {
        let media_p = dlsym(-2isize as *mut c_void, sym.as_ptr());
        if media_p.is_null() {
            return PermState::NotDetermined;
        }
        let media: Id = *(media_p as *const Id);
        if media.is_null() {
            return PermState::NotDetermined;
        }
        let cls = objc_getClass(c"AVCaptureDevice".as_ptr());
        if cls.is_null() {
            return PermState::NotDetermined;
        }
        let f: unsafe extern "C" fn(Id, Sel, Id) -> i64 =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        match f(
            cls,
            sel_registerName(c"authorizationStatusForMediaType:".as_ptr()),
            media,
        ) {
            3 => PermState::Granted,
            2 => PermState::Denied,
            _ => PermState::NotDetermined, // 0 / 1
        }
    }
}

/// 屏幕录制：CGPreflightScreenCaptureAccess（权威布尔，不弹框）。
#[cfg(target_os = "macos")]
fn screen_state() -> PermState {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }
    if unsafe { CGPreflightScreenCaptureAccess() } {
        PermState::Granted
    } else {
        PermState::NotDetermined
    }
}

/// 辅助功能：AXIsProcessTrusted（权威布尔，判定当前进程身份）。
/// 不用 TCC 库读——库里可能残留旧身份（换签名后 csreq 失配）的行，MAX(auth_value) 会假阳性。
#[cfg(target_os = "macos")]
fn accessibility_state() -> PermState {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    if unsafe { AXIsProcessTrusted() } {
        PermState::Granted
    } else {
        PermState::NotDetermined
    }
}

/// 完全磁盘访问：功能探测——能列出受保护目录 `~/Library/Safari` 即视为已授权。
/// 比读 TCC 库可靠（路径身份对 ad-hoc 二进制落库不稳，且 WAL 可能读到旧值）。
#[cfg(target_os = "macos")]
fn full_disk_state() -> PermState {
    let probe = dirs::home_dir().unwrap_or_default().join("Library/Safari");
    match std::fs::read_dir(&probe) {
        Ok(_) => PermState::Granted,
        // 目录存在但读不到 = 明确无权限；目录本身不存在（全新机）无法判定，按未决定
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => PermState::Denied,
        Err(_) => PermState::NotDetermined,
    }
}
/// 自动化（AppleEvents）：功能探测——osascript 向 System Events 发一个只读事件。
/// 判定按当前进程身份，能区分「未授权」（osascript 报 not allowed/not authorized）与其它失败。
/// osascript 不可用时回落 TCC 库（旧身份残留行会有假阳性，但此路径理论不会走到）。
#[cfg(target_os = "macos")]
fn automation_state(client: &str) -> PermState {
    let out = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "tell application \"System Events\" to get name of first process",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => PermState::Granted,
        Ok(o) => {
            let e = String::from_utf8_lossy(&o.stderr).to_lowercase();
            if e.contains("not allowed") || e.contains("not authorized") || e.contains("denied") {
                PermState::NotDetermined
            } else {
                automation_db_fallback(client)
            }
        }
        Err(_) => automation_db_fallback(client),
    }
}

/// automation 的 TCC 库兜底（仅 osascript 不可用/异常时走；旧身份残留行会假阳性，已知）。
#[cfg(target_os = "macos")]
fn automation_db_fallback(client: &str) -> PermState {
    let user = dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Application Support/com.apple.TCC/TCC.db");
    state_of(tcc_auth(&user, "kTCCServiceAppleEvents", client))
}

/// 检测六项系统权限（仅 macOS 有意义）。client = 当前可执行文件路径。
/// 全部走权威 API / 功能探测（TCC 库按 code identity 匹配，裸二进制换签名后库里的
/// 旧身份残留行会假阳性）：
///   full-disk   功能探测 ~/Library/Safari 可读
///   accessibility AXIsProcessTrusted
///   screen      CGPreflightScreenCaptureAccess
///   automation  osascript 实测（向 System Events 发只读事件）
///   camera/mic  AVCaptureDevice authorizationStatusForMediaType:
#[cfg(target_os = "macos")]
pub fn detect_permissions() -> Vec<PermStatus> {
    let client = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    vec![
        PermStatus {
            id: "full-disk",
            label: "完全磁盘访问",
            state: full_disk_state(),
            settings_url:
                "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
        },
        PermStatus {
            id: "accessibility",
            label: "辅助功能（控制鼠标键盘）",
            state: accessibility_state(),
            settings_url:
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        },
        PermStatus {
            id: "screen",
            label: "屏幕录制",
            state: screen_state(),
            settings_url:
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        },
        PermStatus {
            id: "automation",
            label: "自动化（AppleEvents）",
            state: automation_state(&client),
            settings_url:
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation",
        },
        PermStatus {
            id: "camera",
            label: "摄像头",
            state: av_state(c"AVMediaTypeVideo"),
            settings_url: "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera",
        },
        PermStatus {
            id: "microphone",
            label: "麦克风",
            state: av_state(c"AVMediaTypeAudio"),
            settings_url:
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
        },
    ]
}

/// 非 macOS/Windows：Linux 无 TCC/UAC 概念，返回空（UI 显示「仅 macOS/Windows 需要」）。
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub fn detect_permissions() -> Vec<PermStatus> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_all_covers_seven() {
        let all = detect_all();
        assert_eq!(all.len(), 7);
        let ids: Vec<&str> = all.iter().map(|d| d.id).collect();
        for want in [
            "claude",
            "codex",
            "pi",
            "node",
            "python3",
            "lark-cli",
            "dingtalk-cli",
        ] {
            assert!(ids.contains(&want), "缺 {want}");
        }
    }

    #[test]
    fn install_plan_known_unknown() {
        assert!(install_plan("claude").is_ok());
        assert!(install_plan("codex").is_ok());
        assert!(install_plan("pi").is_ok());
        assert!(install_plan("node").is_ok());
        assert!(install_plan("python3").is_ok());
        assert!(install_plan("lark-cli").is_ok());
        assert!(install_plan("dingtalk-cli").is_ok());
        assert!(install_plan("nope").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn perm_state_mapping() {
        assert_eq!(state_of(Some(2)), PermState::Granted);
        assert_eq!(state_of(Some(3)), PermState::Denied);
        assert_eq!(state_of(Some(0)), PermState::NotDetermined);
        assert_eq!(state_of(None), PermState::NotDetermined);
        assert_eq!(state_of(Some(5)), PermState::NotDetermined); // 未知码按未授权
    }

    #[test]
    fn detect_permissions_shape() {
        let perms = detect_permissions();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(perms.len(), 6);
            let ids: Vec<&str> = perms.iter().map(|p| p.id).collect();
            for want in [
                "full-disk",
                "accessibility",
                "screen",
                "automation",
                "camera",
                "microphone",
            ] {
                assert!(ids.contains(&want), "缺 {want}");
            }
            // 每项都有可跳的设置面板 URL
            assert!(perms
                .iter()
                .all(|p| p.settings_url.starts_with("x-apple.systempreferences:")));
        }
        #[cfg(target_os = "windows")]
        {
            assert_eq!(perms.len(), 1);
            assert_eq!(perms[0].id, "admin");
            assert_eq!(perms[0].label, "管理员身份运行");
        }
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        {
            assert!(perms.is_empty(), "Linux 不应有权限项");
        }
    }

    #[cfg(unix)]
    #[test]
    fn composed_path_unix_layout() {
        let p = composed_path();
        assert!(p.contains(".local/bin"));
        assert!(p.contains(".npm-global/bin"));
        assert!(p.contains('/'), "unix 用 / 分隔目录");
    }
}
