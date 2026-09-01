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

/// Windows 上 codex 沙箱 runner 解析 pwsh 用的候选目录（真实 PowerShell 7 安装路径）。
/// 背景（#90 spike 2026-08-27 实测）：codex sandbox（workspace-write / read-only）在
/// Windows 上以受限用户 CreateProcessAsUserW 拉起 pwsh 作为命令 runner；而 PATH 里
/// `WindowsApps\pwsh.exe` 是重解析点别名（App Execution Alias），受限沙箱用户无权
/// 访问 → `CreateProcessAsUserW failed: 1920`，沙箱内任何命令都跑不了（这也是 main
/// 一直走 --dangerously-bypass-approvals-and-sandbox 的真实原因）。把真实 pwsh 目录
/// 前置到 composed_path 最前，让 sandbox runner 解析到可执行的 pwsh.exe。
#[cfg(windows)]
fn windows_pwsh_dirs() -> Vec<String> {
    let mut out = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(var) {
            let dir = PathBuf::from(pf).join("PowerShell").join("7");
            if dir.join("pwsh.exe").is_file() {
                out.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    out
}

#[cfg(windows)]
pub fn composed_path() -> String {
    let mut parts: Vec<String> = Vec::new();
    // #90：真实 pwsh 目录必须最优先（见 windows_pwsh_dirs 注释——sandbox runner 用
    // PATH 解析 pwsh，WindowsApps 别名会 1920 失败）。放最前保证优先于 WindowsApps。
    parts.extend(windows_pwsh_dirs());
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
/// #93 codex 最低版本锁定：与 agent.rs codex_command 的 OS 沙箱能力对齐（issues/93：
/// 「最低版本锁定，如 >= 0.140」）。低于此版本视为「需升级」（进一键安装清单 / UI 显示
/// 升级态），缺失视为「需安装」。
pub const MIN_CODEX_VERSION: &str = "0.140";

/// #105 git 最低版本锁定：< 2.30 视为「需升级」（2.30 起覆盖后续安全修复；
/// 删除保护 git 留痕（#88）与 tidy 每日整理（#104 已核实）都依赖 git）。
pub const MIN_GIT_VERSION: &str = "2.30";

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
    /// 已装版本字符串（仅 codex 做版本探测；其它依赖为空串）。
    pub version: String,
    /// 版本是否满足最低要求（仅 codex 有最低版本锁定，见 MIN_CODEX_VERSION；
    /// 未找到恒 false，其它依赖恒 true）。
    pub version_ok: bool,
}

/// 检测全部依赖。设置窗打开 + 「重新检测」时调。
/// node 探 `node`；python 先试 `python3` 再 `python`；lark-cli 用于技能引导门控。
/// codex 额外做版本探测（#93 最低版本锁定，见 MIN_CODEX_VERSION）；
/// git 额外做版本探测（#105 最低版本锁定，见 MIN_GIT_VERSION）。
pub fn detect_all() -> Vec<DepStatus> {
    let probe = |id: &'static str, label: &'static str, names: &[&str]| -> DepStatus {
        for n in names {
            if let Some(p) = find_in_path(n) {
                return DepStatus {
                    id,
                    label,
                    found: true,
                    path: p.to_string_lossy().into_owned(),
                    version: String::new(),
                    version_ok: true,
                };
            }
        }
        DepStatus {
            id,
            label,
            found: false,
            path: String::new(),
            version: String::new(),
            version_ok: false,
        }
    };
    let codex = probe("codex", "Codex CLI", &["codex"]);
    let codex = if codex.found {
        // 版本探测失败（`codex --version` 跑不通）保守按「可用」放行：能跑 codex 就
        // 大概率能跑 --version，探测失败的场景极少；若按「不满足」会假阳性地一直提示
        // 升级，误伤已装用户（每次启动都被引导）。权衡后取放行。
        match codex_version(&codex.path) {
            Some(v) => DepStatus {
                version: v.clone(),
                version_ok: version_at_least(&v, MIN_CODEX_VERSION),
                ..codex
            },
            None => DepStatus {
                version: String::new(),
                version_ok: true,
                ..codex
            },
        }
    } else {
        codex
    };
    let git = probe("git", "Git", &["git"]);
    let git = if git.found {
        // git 版本探测失败保守按「可用」放行（与 codex 同权衡：能跑 git 就大概率能
        // 跑 --version；按不满足会假阳性一直提示升级）。
        match git_version(&git.path) {
            Some(v) => DepStatus {
                version: v.clone(),
                version_ok: version_at_least(&v, MIN_GIT_VERSION),
                ..git
            },
            None => DepStatus {
                version: String::new(),
                version_ok: true,
                ..git
            },
        }
    } else {
        git
    };
    vec![
        probe("claude", "Claude Code", &["claude"]),
        codex,
        // pi：npm 全局 bin（~/.npm-global/bin/pi，软链到 pi-coding-agent 的 cli.js）
        probe("pi", "Pi (pi-coding-agent)", &["pi"]),
        probe("node", "Node.js", &["node"]),
        probe("python3", "Python 3", &["python3", "python"]),
        probe("lark-cli", "lark-cli", &["lark-cli"]),
        // 钉钉 CLI（dingtalk-workspace-cli，命令名 dws）：接入钉钉 bot 后让 agent 调钉钉能力
        probe("dingtalk-cli", "dingtalk-cli (dws)", &["dws"]),
        git,
    ]
}

/// 跑 `codex --version` 解析版本号。跑不通/非零退出 → None。
pub fn codex_version(exe: &str) -> Option<String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version");
    // Windows：依赖检测跑 codex --version 也抑制控制台窗口（#104），
    // 否则每次「环境检测/一键安装」都会闪一个黑框。
    #[cfg(windows)]
    {
        apply_no_window(&mut cmd);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    codex_version_from_text(&String::from_utf8_lossy(&out.stdout))
}

/// 从 `codex --version` 输出文本里提取版本号。输出形如 `codex-cli 0.146.0`
/// （也可能带 build 后缀，如 `codex-cli 0.146.0 (abc1234)`）→ 返回 `0.146.0`。
/// 找不到形如 `d+.d+` 的 token → None。
pub fn codex_version_from_text(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|tok| {
            let head: String = tok
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            !head.is_empty()
                && head.split('.').count() >= 2
                && head
                    .split('.')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(|tok| {
            tok.chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect()
        })
}

/// codex 版本探测缓存（进程内不变）：`codex --version` 是 spawn 子进程的同步阻塞调用，
/// 每条消息都探测纯属浪费（#172 后默认 Auto 档探测结果恒不消费——审查发现；#180 版本
/// 门控又要每消息用一次）。首次调用探测一次，后续走缓存；探测失败缓存 None（进程生命
/// 周期内不重试——安装/升级 codex 的场景由重启 ABB 覆盖）。注意：以首次调用方传入的
/// exe 为准（进程内各调用点解析的是同一 codex 路径）。
pub fn codex_version_cached(exe: &str) -> Option<&'static str> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| codex_version(exe)).as_deref()
}

/// 版本门控便捷入口：缓存的 codex 版本是否 >= min（探测失败保守 false）。
pub fn codex_version_at_least_cached(exe: &str, min: &str) -> bool {
    codex_version_cached(exe)
        .map(|v| version_at_least(v, min))
        .unwrap_or(false)
}

/// 跑 `git --version` 解析版本号。跑不通/非零退出 → None。
pub fn git_version(exe: &str) -> Option<String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version");
    // Windows：检测也抑制控制台窗口（#104 同款——GUI 下跑 git --version 不闪黑框）。
    #[cfg(windows)]
    {
        apply_no_window(&mut cmd);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    git_version_from_text(&String::from_utf8_lossy(&out.stdout))
}

/// 从 `git --version` 输出文本里提取版本号。输出形如 `git version 2.39.2.windows.1`
/// → 返回 `2.39.2`（去平台后缀）；找不到形如 `d+.d+` 的 token → None。
pub fn git_version_from_text(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|tok| {
            tok.chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect::<String>()
        })
        .map(|head| head.trim_end_matches('.').to_string())
        .find(|head| {
            !head.is_empty()
                && head.split('.').count() >= 2
                && head
                    .split('.')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
}

/// 解析 `X.Y.Z` 版本串为数值元组（前导数字段；多余段忽略，缺段补 0）。
/// 如 `0.146.0` → (0,146,0)；`1.2` → (1,2,0)；非版本串 → None。
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let parts: Vec<&str> = digits.split('.').collect();
    if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let mut nums = [0u64; 3];
    for (i, p) in parts.iter().take(3).enumerate() {
        nums[i] = p.parse().ok()?;
    }
    Some((nums[0], nums[1], nums[2]))
}

/// `v >= min` 的语义化版本比较（三段数值；解析失败保守返回 false）。
pub fn version_at_least(v: &str, min: &str) -> bool {
    match (parse_version(v), parse_version(min)) {
        (Some(a), Some(b)) => a >= b,
        _ => false,
    }
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
            // git：brew 首选（可自动化）；无 brew 回落 xcode-select（弹系统安装对话框，需人工确认）。
            "git" => vec![
                InstallStep::exec("brew", &["install", "git"]),
                InstallStep::shell("xcode-select --install"),
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
            // git：winget 官方包（Git for Windows）静默安装——与 node/python 同款入口；
            // 装完 WinGet\Links 里生成 git.exe 链接（composed_path 已含该目录），
            // 无需重启 ABB 即可重新检测到（PATH 即时生效）。
            "git" => vec![InstallStep::exec(
                "winget",
                &[
                    "install",
                    "Git.Git",
                    "--silent",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ],
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
            "git" => vec![InstallStep::shell(
                "if command -v apt-get >/dev/null; then sudo apt-get install -y git; \
                     elif command -v dnf >/dev/null; then sudo dnf install -y git; \
                     else echo 'no-supported-pkg-mgr' >&2; exit 1; fi",
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
                // #93/#105：codex/git 装完还要过最低版本锁（旧版缓存/降级场景少见，但过一下更稳）。
                let ok_after = detect_one(dep_id)
                    .map(|d| d.found && !((dep_id == "codex" || dep_id == "git") && !d.version_ok))
                    .unwrap_or(false);
                if ok_after {
                    crate::log!("[deps] {dep_id} 安装成功（步骤{}）", i + 1);
                    // #93 登录引导：codex 装完给下一步指引（装完即能用的最后一公里）。
                    // 不自动跑 codex login（交互式浏览器授权，GUI 里无人值守会挂死）；
                    // 给出两条路径：命令行 codex login / GUI 供应商页配 key（ABB 已有供应商注入）。
                    // 状态行要精简：不返回 npm/brew 的冗长输出，只回引导文案。
                    if dep_id == "codex" {
                        return Ok(
                            "✅ codex 安装完成。首次使用：① 命令行运行 `codex login`（浏览器授权）；② 或到「设置 → 模型供应商」配 OpenAI / DeepSeek / OpenRouter 的 API key。"
                                .to_string(),
                        );
                    }
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
        .stdin(Stdio::null())
        // #60 审查 Critical：一键装的 20 分钟超时靠 drop future 生效——必须设
        // kill_on_drop（与 agent.rs/larkskills.rs 同款），否则超时只停止等待、
        // 子进程成孤儿继续安装（与下一项并发 brew 锁互斥、跳过决策失真、
        // Windows 孤儿 winget 随时弹 UAC）。正常完成路径不受影响。
        .kill_on_drop(true);

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

// ─── #60 一键安装全部缺失组件 ─────────────────────────────────────

/// 一键安装进度事件（每开始一项发一次；label 用 DepStatus.label，如 "Claude Code"）。
#[derive(Debug, Clone)]
pub struct InstallEvt {
    pub label: String,
    /// 当前项序号（1-based）。
    pub idx: usize,
    pub total: usize,
}

/// 一键安装汇总（如实呈现：成功/失败/跳过三类，失败附尾因）。
#[derive(Debug, Clone, Default)]
pub struct AllInstallOutcome {
    /// 安装成功的 dep_id（含「本来就在、重装幂等成功」的 node）。
    pub ok: Vec<String>,
    /// (dep_id, 失败尾因，已截断)。
    pub failed: Vec<(String, String)>,
    /// (dep_id, 跳过原因——node 未装好时「npm 首步」依赖不尝试)。
    pub skipped: Vec<(String, String)>,
}

/// 安装失败的分类（普通用户可操作的引导依据）。启发式从错误串识别，非精确判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// 权限不足（EACCES/denied/not permitted）——macOS 授权拦截 / Windows 未提权。
    Permission,
    /// 网络失败（curl 错误/连接超时/无法解析）。
    Network,
    /// 前置命令缺失（找不到 brew/npm 等）。
    CommandMissing,
    /// 安装超时（20 分钟兜底）。
    Timeout,
    /// 命令退出 0 但 PATH 检测不到。
    Path,
    /// 未识别。
    Other,
}

impl FailKind {
    /// 每类的「怎么办」引导（普通用户照做即可，GUI 语境不提终端概念）。
    pub fn advice(self) -> &'static str {
        match self {
            FailKind::Permission => {
                "权限不足。macOS：检查「系统设置 → 隐私与安全性」是否有安装拦截；\
                 Windows：关闭 ABB 后右键「以管理员身份运行」再重试"
            }
            FailKind::Network => "网络连接失败。请检查网络或代理设置后重试",
            FailKind::CommandMissing => "缺少前置依赖。重试一键安装即可（会自动先装 Node.js）",
            FailKind::Timeout => {
                "安装超时（可能卡在网络或系统弹窗）。请检查是否有弹窗等待确认后重试"
            }
            FailKind::Path => "已安装但未找到可执行文件。请重启 ABB 后重新检测",
            FailKind::Other => "安装失败。可复制下方原始错误反馈",
        }
    }
}

/// 一条带分类与引导的安装失败（UI 渲染用；原始错误保留供复制/日志）。
#[derive(Debug, Clone)]
pub struct FailedItem {
    pub id: String,
    /// 分类（当前仅测试断言消费——advice 才是 UI 可操作内容；保留字段供未来
    /// 按类定制 UI（如权限类给提权按钮））。
    #[allow(dead_code)]
    pub kind: FailKind,
    pub advice: String,
    pub raw: String,
}

/// 把失败尾因分类为可操作条目。启发式按序子串匹配（大小写不敏感）；
/// 匹配不到归 Other（仍给通用引导 + 原始错误）。
pub fn classify_fail(dep_id: &str, err: &str) -> FailedItem {
    let lower = err.to_ascii_lowercase();
    let kind = if lower.contains("denied")
        || lower.contains("eacces")
        || lower.contains("permission")
        || lower.contains("not permitted")
    {
        FailKind::Permission
    } else if lower.contains("curl:")
        || lower.contains("network")
        || lower.contains("etimedout")
        || lower.contains("econnrefused")
        || lower.contains("failed to connect")
        || lower.contains("couldn't resolve")
        || lower.contains("enotfound")
        || lower.contains("eai_again")
        || lower.contains("getaddrinfo")
    {
        FailKind::Network
    } else if lower.contains("找不到") || lower.contains("command not found") {
        FailKind::CommandMissing
    } else if lower.contains("安装超时") {
        FailKind::Timeout
    } else if lower.contains("未在 path 找到") {
        FailKind::Path
    } else {
        FailKind::Other
    };
    FailedItem {
        id: dep_id.to_string(),
        kind,
        advice: kind.advice().to_string(),
        raw: err.to_string(),
    }
}

/// 单项目安装超时（#60）：run_step 无超时（winget 弹 UAC 等用户 / 网络挂起），
/// 一键装 8 项串行会把「卡死一项」放大成全流程卡死——每项 20 分钟兜底。
const ALL_INSTALL_DEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// 缺失清单：node 恒在最前（其它 npm 计划的前置），其余按 detect_all 顺序。纯函数。
pub fn missing_dep_ids(deps: &[DepStatus]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for d in deps {
        // #93/#105：codex/git 已装但版本低于最低锁定（MIN_CODEX_VERSION /
        // MIN_GIT_VERSION）也进清单——一键安装/单项安装会重跑安装器升级到最新。
        // 其余依赖只看 found。
        if !d.found || ((d.id == "codex" || d.id == "git") && !d.version_ok) {
            ids.push(d.id.to_string());
        }
    }
    // node 移到最前（保持其余相对顺序稳定）
    if let Some(pos) = ids.iter().position(|id| id == "node") {
        if pos > 0 {
            let n = ids.remove(pos);
            ids.insert(0, n);
        }
    }
    ids
}

/// 该依赖安装是否需要 node/npm 已就绪：安装计划首步用 npm（win 的 claude/pi 首步是
/// npm；mac 的 claude/pi 是 curl shell 不依赖 node）。纯函数（内部调 install_plan），
/// 供「node 失败 → 跳过」判定。
fn install_needs_node(dep_id: &str) -> bool {
    install_plan(dep_id)
        .ok()
        .and_then(|steps| steps.into_iter().next())
        .map(|s| s.program == "npm" && s.shell.is_none())
        .unwrap_or(false)
}

/// 汇总文案（状态行用）：成功 N 项 / 失败 M 项（id: 尾因截断 100 字）/ 跳过 K 项。
/// 空 outcome（全齐无缺失）= 「全部依赖均已安装」。
pub fn format_all_summary(o: &AllInstallOutcome) -> String {
    if o.ok.is_empty() && o.failed.is_empty() && o.skipped.is_empty() {
        return "✅ 全部依赖均已安装".to_string();
    }
    let mut s = format!("一键安装完成：成功 {} 项", o.ok.len());
    if !o.failed.is_empty() {
        s.push_str(&format!("，失败 {} 项（", o.failed.len()));
        let parts: Vec<String> = o
            .failed
            .iter()
            .map(|(id, e)| format!("{id}: {}", crate::agent::truncate(e, 100)))
            .collect();
        s.push_str(&parts.join("；"));
        s.push('）');
    }
    if !o.skipped.is_empty() {
        let names: Vec<&str> = o.skipped.iter().map(|(id, _)| id.as_str()).collect();
        s.push_str(&format!(
            "，跳过 {} 项（{}）",
            o.skipped.len(),
            names.join("、")
        ));
    }
    s
}

/// 一键安装全部缺失组件（#60）。on_evt 在每项开始前同步调用（非 async 闭包，await
/// 间隙之间触发）。策略：继续不中断 + 如实汇总；node 失败后跳过需 node/npm 的依赖
/// （npm 首步计划；mac 的 claude/pi 走 curl 原生路径不受影响）；
/// 每项 20 分钟超时。
pub async fn install_all_missing(mut on_evt: impl FnMut(InstallEvt) + Send) -> AllInstallOutcome {
    let mut outcome = AllInstallOutcome::default();
    let deps = detect_all();
    let npm_ok = find_in_path("npm").is_some();
    let mut ids = missing_dep_ids(&deps);
    // node 在但 npm 不在 PATH（nvm 装法/裸二进制）→ 插入 node 步自愈（brew/winget
    // 重装幂等）；node 缺时 missing_dep_ids 已把它放最前。
    if !npm_ok && !ids.iter().any(|id| id == "node") {
        ids.insert(0, "node".to_string());
    }
    if ids.is_empty() {
        return outcome;
    }
    let total = ids.len();
    let mut node_failed = false;
    for (i, id) in ids.iter().enumerate() {
        let label = deps
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.label.to_string())
            .unwrap_or_else(|| id.clone());
        on_evt(InstallEvt {
            label,
            idx: i + 1,
            total,
        });
        // node 未装好 → 需 node/npm 的依赖必然报「找不到 npm」，跳过而非制造失败噪音。
        // node 恒在最前（i==0 不可能是跳过对象）。
        if node_failed && !npm_ok && install_needs_node(id) {
            crate::log!("[deps] 一键装跳过 {id}（node/npm 未装好）");
            outcome
                .skipped
                .push((id.clone(), "node/npm 未装好，跳过".to_string()));
            continue;
        }
        crate::log!("[deps] 一键装 [{}/{}] {}", i + 1, total, id);
        match tokio::time::timeout(ALL_INSTALL_DEP_TIMEOUT, run_install(id)).await {
            Ok(Ok(_)) => outcome.ok.push(id.clone()),
            Ok(Err(e)) => {
                if id == "node" {
                    node_failed = true;
                }
                outcome.failed.push((id.clone(), e));
            }
            Err(_) => {
                if id == "node" {
                    node_failed = true;
                }
                outcome.failed.push((
                    id.clone(),
                    "安装超时（20 分钟），可能卡在网络或系统弹窗".to_string(),
                ));
            }
        }
    }
    outcome
}

/// 读流，只保留尾部约 800 字符（安装输出可能很长，状态行/日志只要结尾）。
/// #69 审计：短命、有 owner（run_step 内 await 收尾），不登记。
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

/// #129：辅助功能（kTCCServicePostEvent）权威布尔检测——CGPreflightPostEventAccess。
/// macOS 10.15+。与 screen_state 同模式（同步、不弹框）。
#[cfg(target_os = "macos")]
fn post_event_state() -> PermState {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightPostEventAccess() -> bool;
    }
    if unsafe { CGPreflightPostEventAccess() } {
        PermState::Granted
    } else {
        PermState::NotDetermined
    }
}

/// #129：输入监控（kTCCServiceListenEvent）权威布尔检测——CGPreflightListenEventAccess。
/// macOS 10.15+。
#[cfg(target_os = "macos")]
fn listen_event_state() -> PermState {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
    }
    if unsafe { CGPreflightListenEventAccess() } {
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
        // #129 锁屏控制：辅助功能（kTCCServicePostEvent）与输入监控（kTCCServiceListenEvent）
        // 分开展示。权威检测用 CGPreflight*EventAccess（与系统设置同源，同 screen_state 模式）。
        PermStatus {
            id: "post-event",
            label: "辅助功能·锁屏按键注入",
            state: post_event_state(),
            settings_url:
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        },
        PermStatus {
            id: "listen-event",
            label: "输入监控·锁屏按键注入",
            state: listen_event_state(),
            settings_url:
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
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
    fn parse_version_normalizes() {
        assert_eq!(parse_version("0.146.0"), Some((0, 146, 0)));
        assert_eq!(parse_version("0.140"), Some((0, 140, 0)));
        assert_eq!(parse_version("1.2.3.4"), Some((1, 2, 3))); // 多余段忽略
        assert_eq!(parse_version("1.2-beta"), Some((1, 2, 0))); // 后缀截断
        assert_eq!(parse_version("0.146.0 (abc1234)"), Some((0, 146, 0)));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("0"), None); // 不足两段
        assert_eq!(parse_version("1..2"), None); // 空段
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_at_least_gates() {
        // 满足
        assert!(version_at_least("0.146.0", MIN_CODEX_VERSION));
        assert!(version_at_least("0.140.0", MIN_CODEX_VERSION));
        assert!(version_at_least("0.140.1", MIN_CODEX_VERSION));
        assert!(version_at_least("1.0.0", MIN_CODEX_VERSION));
        assert!(version_at_least("0.200", MIN_CODEX_VERSION)); // 缺段补 0
                                                               // 不满足
        assert!(!version_at_least("0.139.9", MIN_CODEX_VERSION));
        assert!(!version_at_least("0.14", MIN_CODEX_VERSION)); // 0.14 < 0.140
        assert!(!version_at_least("0.1.99", MIN_CODEX_VERSION));
        // 解析失败保守 false
        assert!(!version_at_least("abc", MIN_CODEX_VERSION));
        assert!(!version_at_least("0.146.0", "not-a-version"));
    }

    #[test]
    fn codex_version_parses_cli_output() {
        // codex --version 实测输出形态：`codex-cli 0.146.0`（新版可能带 build 后缀）。
        assert_eq!(
            codex_version_from_text("codex-cli 0.146.0"),
            Some("0.146.0".into())
        );
        assert_eq!(
            codex_version_from_text("codex-cli 0.146.0 (abc1234)\n"),
            Some("0.146.0".into())
        );
        assert_eq!(
            codex_version_from_text("@openai/codex 0.140.0"),
            Some("0.140.0".into())
        );
        assert_eq!(codex_version_from_text("未知版本"), None);
        assert_eq!(codex_version_from_text(""), None);
    }

    #[test]
    fn git_version_parses_cli_output() {
        // git --version 实测输出形态（win 带平台后缀）：`git version 2.39.2.windows.1`。
        assert_eq!(
            git_version_from_text("git version 2.39.2.windows.1\n"),
            Some("2.39.2".into())
        );
        assert_eq!(
            git_version_from_text("git version 2.30.0"),
            Some("2.30.0".into())
        );
        assert_eq!(git_version_from_text("git version"), None);
        assert_eq!(git_version_from_text(""), None);
        // 版本门：2.30 边界
        assert!(version_at_least("2.30.0", MIN_GIT_VERSION));
        assert!(version_at_least("2.39.2", MIN_GIT_VERSION));
        assert!(!version_at_least("2.29.3", MIN_GIT_VERSION));
        assert!(!version_at_least("1.9.5", MIN_GIT_VERSION));
    }

    #[test]
    fn missing_deps_includes_version_low_codex() {
        // 已装但版本过低 → 进缺失清单（升级路径）
        let low = DepStatus {
            id: "codex",
            label: "Codex CLI",
            found: true,
            path: String::new(),
            version: "0.139.0".into(),
            version_ok: false,
        };
        let ok = DepStatus {
            id: "codex",
            label: "Codex CLI",
            found: true,
            path: String::new(),
            version: "0.146.0".into(),
            version_ok: true,
        };
        let missing = missing_dep_ids(&[low]);
        assert_eq!(missing, vec!["codex".to_string()], "版本过低应进安装清单");
        assert!(missing_dep_ids(&[ok]).is_empty(), "版本满足不应进清单");
    }

    #[test]
    fn classify_fail_matrix() {
        // 分类矩阵：每类构造代表性错误串 → 断言 kind + advice 非空
        let cases = [
            (
                "codex",
                "退出码 1：EACCES: permission denied",
                FailKind::Permission,
            ),
            (
                "codex",
                "退出码 1：curl: (7) Failed to connect",
                FailKind::Network,
            ),
            (
                "lark-cli",
                "退出码 127：npm: command not found",
                FailKind::CommandMissing,
            ),
            (
                "pi",
                "找不到 npm（请先装它的前置依赖）",
                FailKind::CommandMissing,
            ),
            (
                "claude",
                "安装超时（20 分钟），可能卡在网络或系统弹窗",
                FailKind::Timeout,
            ),
            (
                "codex",
                "步骤1跑完但未在 PATH 找到 codex（可能需重开终端/刷新 PATH）",
                FailKind::Path,
            ),
            ("pi", "退出码 2：some unknown error text", FailKind::Other),
        ];
        for (id, err, expect) in cases {
            let f = classify_fail(id, err);
            assert_eq!(f.kind, expect, "{id}: {err}");
            assert!(!f.advice.is_empty(), "{id} advice 非空");
            assert_eq!(f.raw, err, "原始错误保留");
        }
        // 大小写不敏感
        assert_eq!(
            classify_fail("x", "EACCES").kind,
            FailKind::Permission,
            "大写匹配"
        );
        assert_eq!(
            classify_fail("x", "Failed to CONNECT").kind,
            FailKind::Network,
            "大小写不敏感"
        );
    }

    #[test]
    fn detect_all_covers_eight() {
        let all = detect_all();
        assert_eq!(all.len(), 8);
        let ids: Vec<&str> = all.iter().map(|d| d.id).collect();
        for want in [
            "claude",
            "codex",
            "pi",
            "node",
            "python3",
            "lark-cli",
            "dingtalk-cli",
            "git",
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

    // ─── #60 一键安装纯函数 ───

    fn dep(id: &'static str, found: bool) -> DepStatus {
        DepStatus {
            id,
            label: id,
            found,
            path: String::new(),
            version: String::new(),
            version_ok: found,
        }
    }

    #[test]
    fn missing_dep_ids_order_and_empties() {
        let all_ok = detect_all()
            .into_iter()
            .map(|d| dep(d.id, true))
            .collect::<Vec<_>>();
        assert!(missing_dep_ids(&all_ok).is_empty(), "全装 → 空清单");
        let all_missing = detect_all()
            .into_iter()
            .map(|d| dep(d.id, false))
            .collect::<Vec<_>>();
        let ids = missing_dep_ids(&all_missing);
        assert_eq!(ids.len(), 8, "全缺 → 8 项");
        assert_eq!(ids[0], "node", "node 恒在最前");
        // 部分缺保 detect 序（node 不在缺失集时不插队）
        let partial = vec![
            dep("claude", true),
            dep("codex", false),
            dep("pi", false),
            dep("node", true),
            dep("python3", false),
            dep("lark-cli", true),
            dep("dingtalk-cli", false),
        ];
        assert_eq!(
            missing_dep_ids(&partial),
            vec!["codex", "pi", "python3", "dingtalk-cli"]
        );
    }

    #[test]
    fn missing_dep_ids_node_first_even_mid_list() {
        // 乱序输入里 node 缺失 → 恒提到首位，其余相对顺序稳定
        let deps = vec![
            dep("claude", false),
            dep("codex", true),
            dep("node", false),
            dep("lark-cli", false),
        ];
        assert_eq!(missing_dep_ids(&deps), vec!["node", "claude", "lark-cli"]);
    }

    #[test]
    fn install_needs_node_per_platform() {
        #[cfg(target_os = "macos")]
        {
            // mac：claude 首选 curl | bash（shell 步骤）→ false；codex 首选 npm → true
            assert!(
                !install_needs_node("claude"),
                "mac claude 走 curl 不依赖 node"
            );
            assert!(install_needs_node("codex"), "mac codex 首选 npm");
            assert!(!install_needs_node("python3"), "mac python3 走 brew");
        }
        #[cfg(target_os = "windows")]
        {
            // win：claude/codex/pi 全 npm；python3 走 winget
            assert!(install_needs_node("claude"));
            assert!(install_needs_node("pi"));
            assert!(!install_needs_node("python3"), "win python3 走 winget");
        }
        // 未知 id：install_plan Err → false（不误跳过）
        assert!(!install_needs_node("no-such-dep"));
    }

    #[test]
    fn format_all_summary_shapes() {
        let empty = AllInstallOutcome::default();
        assert_eq!(format_all_summary(&empty), "✅ 全部依赖均已安装");
        let all_ok = AllInstallOutcome {
            ok: vec!["node".into(), "claude".into()],
            ..Default::default()
        };
        assert_eq!(format_all_summary(&all_ok), "一键安装完成：成功 2 项");
        let mixed = AllInstallOutcome {
            ok: vec!["node".into()],
            failed: vec![("codex".into(), "找不到 npm（请先装它的前置依赖）".into())],
            skipped: vec![("pi".into(), "node/npm 未装好，跳过".into())],
        };
        let s = format_all_summary(&mixed);
        assert!(s.contains("成功 1 项"), "成功数: {s}");
        assert!(s.contains("失败 1 项"), "失败数: {s}");
        assert!(s.contains("codex:"), "失败项 id: {s}");
        assert!(s.contains("跳过 1 项（pi）"), "跳过项: {s}");
        // 尾因超 100 字截断
        let long = AllInstallOutcome {
            ok: vec![],
            failed: vec![("x".into(), "长".repeat(200))],
            skipped: vec![],
        };
        let s2 = format_all_summary(&long);
        assert!(s2.contains(&"长".repeat(100)), "截断至 100 字");
        assert!(!s2.contains(&"长".repeat(101)), "不超 100 字: {s2}");
    }
}
