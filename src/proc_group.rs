//! 进程组：proc 任务的「整组停止」抽象（跨平台，2026-10-06）。
//!
//! 为什么需要：proc 载荷跑的是**外部命令**，超时/取消时必须把**整棵进程树**停掉；只杀根进程
//! 会留下孤儿（Windows 上尤其如此）。两个平台各有一个「组」的等价物：
//!
//! - Unix：进程组 —— proc 启动时 `setsid` 独立成组，停的时候 `kill -9 -<pgid>` 一次全杀；
//! - Windows：没有进程组等价物，用 `taskkill /T /F /PID <pid>` 递归杀子树（仓库里停服务、
//!   升级前收尾已经在用同一手段 ⇒ 不引入新机制、不引新依赖）。
//!
//! 对外只暴露语义（停掉这棵树），平台差异封在这里。更严格的 Windows 方案（Job Object，
//! 能让孙进程也逃不掉）留作以后替换 —— 换掉时**对外接口不变**。
#![allow(dead_code)] // 第一步先落地抽象，第二步才接进 task_proc（未接线前不该报 dead_code）。

/// Windows：递归杀子树 + 强制（纯函数，单测点）。
pub fn taskkill_args(pid: u32) -> Vec<String> {
    vec![
        "/T".to_string(),
        "/F".to_string(),
        "/PID".to_string(),
        pid.to_string(),
    ]
}

/// Unix：向进程组发 SIGKILL（负 pid = 组；纯函数，单测点）。
pub fn killpg_args(pid: u32) -> Vec<String> {
    vec!["-9".to_string(), format!("-{pid}")]
}

/// 停掉一棵进程树。
///
/// `pid` 必须是**组的根**（Unix 下即 setsid 后的组长 pid；Windows 下即子树根 pid）。
#[cfg(target_os = "windows")]
pub fn kill_tree(pid: u32) -> std::io::Result<()> {
    let out = crate::spawn::command("taskkill")
        .args(taskkill_args(pid))
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    // 进程可能已经自己退了：taskkill 会说「找不到」⇒ 视为已停止（与本仓库处理服务停止同一条经验）。
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("not found") || err.contains("找不到") || err.contains("没有找到") {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "taskkill 失败：{}",
        err.trim()
    )))
}

/// 停掉一棵进程树（Unix 后端）。
#[cfg(not(target_os = "windows"))]
pub fn kill_tree(pid: u32) -> std::io::Result<()> {
    let out = std::process::Command::new("kill")
        .args(killpg_args(pid))
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // 组已不存在（ESRCH）⇒ 已停止。
    if err.contains("No such process") || err.contains("No such") {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "kill -9 -{pid} 失败：{}",
        err.trim()
    )))
}

#[cfg(test)]
mod proc_group_guard_tests {
    /// Windows 必须用 `/T`（递归整棵子树）+ `/F`；少了 `/T` 就会留下孤儿进程。
    ///
    /// 判别力：把 `/T` 去掉 ⇒ 第一条断言红。
    #[test]
    fn windows_kills_the_whole_tree() {
        let a = crate::proc_group::taskkill_args(4321);
        assert!(a.contains(&"/T".to_string()), "必须递归杀子树：{a:?}");
        assert!(a.contains(&"/F".to_string()), "必须强制：{a:?}");
        assert!(a.contains(&"4321".to_string()), "必须点名 pid：{a:?}");
    }

    /// Unix 必须杀**进程组**（负 pid）而不是单个进程。
    ///
    /// 判别力：把 `-{pid}` 改成 `{pid}` ⇒ 第一条断言红（只杀根，留下孤儿）。
    #[test]
    fn unix_kills_the_process_group() {
        let a = crate::proc_group::killpg_args(4321);
        assert_eq!(a, vec!["-9".to_string(), "-4321".to_string()]);
        assert!(
            a[1].starts_with('-'),
            "必须是负 pid（组）而不是单进程：{a:?}"
        );
    }
}
