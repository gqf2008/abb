//! agent 子进程的构造入口（**只属于 bin**）。
//!
//! 与 `spawn.rs` 的分工：`spawn.rs` 是 lib/bin 共用的「抑制控制台窗口」叶子模块（纯构造）；
//! 本模块多了 Windows 的**降权**职责，而它要用 `crate::log!` 与 lib 的 `elev`（bin 侧通过
//! `agent_bridge::elev` 访问），所以只声明在 `main.rs`，不进 lib。
//!
//! 背景（批 `abb-svc-persist-password-gate` 的 B2）：Windows 上 bridge 由计划任务以
//! `RunLevel=HighestAvailable` 拉起（高完整性 ⇒ 普通权限杀不掉，owner 要的「不能被随便杀死」），
//! 但子进程会继承该完整性：claude/codex 这类 agent 若被诱导，后果从用户级升到管理员级。
//! 故本模块在「本进程已提权」时把命令包一层 `abb-spawner.exe`：由它用桌面 shell（explorer）
//! 的令牌重建进程，把权限降回普通用户。
//!
//! fail-closed：已提权却找不到 `abb-spawner.exe` 时**不退回直接 spawn**（那等于静默提权），
//! 而是让 spawn 明确失败并留下日志 —— 宁可 agent 起不来，也不要它带着管理员权限跑。
//!
//! **边界（评审 R23 B2-e，如实登记）**：本入口只覆盖 **ACP agent**（claude/codex/pi 等，
//! 经 `buzz/acp.rs` 启动）。bridge 里其它子进程仍是高完整性 —— 例如 `larkskills` 的
//! `npx`、`mcp_events`/`tidy` 的 `git`、`platform` 的 `cmd`/`clip`。它们的输入不来自模型
//! 输出（命令与参数都是代码里写死的常量），风险面与「agent 可能被诱导」不同，故本轮不扩到
//! 它们；发布说明里也要按这句口径写，别让「agent 已降权」被读成「所有子进程都已降权」。

/// `abb-spawner.exe`（与本二进制同目录，装机时随包）；找不到返回 `None`。
///
/// 非 Windows 平台恒 `None`（那里的 bridge 本来就是普通用户身份，无需降权）。
fn spawner_exe() -> Option<std::path::PathBuf> {
    #[cfg(not(windows))]
    {
        None
    }
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().ok()?;
        let p = exe.parent()?.join("abb-spawner.exe");
        p.is_file().then_some(p)
    }
}

/// 本进程是否处于高完整性（需要给 agent 降权）。
#[cfg(windows)]
fn need_de_elevate() -> bool {
    agent_bridge::elev::win::is_elevated()
}

#[cfg(not(windows))]
fn need_de_elevate() -> bool {
    false
}

/// 「缺 shim」时用的占位路径：绝对路径 + **必然不存在**（评审 R23 B2-b）。
///
/// 用它而不是裸名，是为了避免 CreateProcess 的搜索顺序命中同名的其它程序；
/// 也让单测能断言「绝对 && 不存在」（比只断言「不等于 program」有判别力）。
fn missing_shim_path() -> String {
    std::env::temp_dir()
        .join(format!("abb-spawner-missing-{}.exe", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// 取「启动 agent 要用的程序 + 前置参数」：需要降权时是 `abb-spawner.exe -- <program>`，
/// 否则就是 `<program>` 本身。
///
/// 单测点：`resolve(need, shim, program)` 是纯函数（见文件末测试）。
fn resolve(need: bool, shim: Option<std::path::PathBuf>, program: &str) -> (String, Vec<String>) {
    match (need, shim) {
        (true, Some(shim)) => (
            shim.to_string_lossy().into_owned(),
            vec!["--".to_string(), program.to_string()],
        ),
        // 已提权但没有 shim：指向一个**绝对且必然不存在**的路径 —— spawn 会明确失败，
        // 而不是「成功但带着管理员权限」（fail-closed）。
        //
        // 评审 R23 B2-b：原来用裸相对名 `abb-spawner.exe` 会被 CreateProcess 的搜索顺序
        // （应用目录 → 当前目录 → System32 → PATH）先命中**别人**的同名程序 —— 那就等于
        // 用未知程序去启动 agent。绝对路径 + 带 pid 的唯一名把这个面收掉。
        (true, None) => (
            missing_shim_path(),
            vec!["--".to_string(), program.to_string()],
        ),
        (false, _) => (program.to_string(), Vec::new()),
    }
}

/// 构造 agent 子进程命令（tokio 版，ACP 走这条）。
pub fn tokio_command(program: &str) -> tokio::process::Command {
    let need = need_de_elevate();
    let (exe, pre) = resolve(need, spawner_exe(), program);
    if need {
        crate::log!(
            "[spawn] 本进程已提权：经 abb-spawner 以桌面 shell 身份启动 {}",
            program
        );
    }
    let mut cmd = crate::spawn::tokio_command(&exe);
    cmd.args(pre);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未提权：原样启动（不多一层进程）。
    #[test]
    fn direct_spawn_when_not_elevated() {
        let (exe, pre) = resolve(false, Some("C:\\APP\\abb-spawner.exe".into()), "claude");
        assert_eq!(exe, "claude");
        assert!(pre.is_empty());
    }

    /// 已提权 + 有 shim：包一层，且 `<program>` 放在 `--` 之后（shim 侧 split_args 的契约）。
    #[test]
    fn wrapped_spawn_when_elevated() {
        let (exe, pre) = resolve(true, Some("C:\\APP\\abb-spawner.exe".into()), "claude");
        assert_eq!(exe, "C:\\APP\\abb-spawner.exe");
        assert_eq!(pre, vec!["--".to_string(), "claude".to_string()]);
    }

    /// 已提权但**没有 shim**：fail-closed —— 指向绝对且必然不存在的占位路径（spawn 必失败），
    /// 绝不退化成「直接启动、继承管理员权限」，也不会命中 PATH 里的同名程序。
    #[test]
    fn elevated_without_shim_is_fail_closed() {
        let (exe, pre) = resolve(true, None, "claude");
        assert_eq!(pre, vec!["--".to_string(), "claude".to_string()]);
        let path = std::path::Path::new(&exe);
        assert!(
            path.is_absolute(),
            "必须是绝对路径（避免命中 PATH 同名程序）：{exe}"
        );
        assert!(!path.exists(), "占位路径必须不存在（fail-closed）：{exe}");
        assert!(
            !exe.contains("claude"),
            "不能退化成直接启动被降权对象：{exe}"
        );
    }
}
