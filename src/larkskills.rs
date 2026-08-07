//! lark-cli + lark-* 技能引导 —— 接入飞书 bot 时自动装，让飞书 bot 背后的 claude 能用 lark 技能。
//!
//! 机制：lark-cli 本体走 npm（`@larksuite/cli`）；技能走 vercel-labs 的 `skills` installer
//! （`npx skills add larksuite/cli`，把 `~/.agents/skills/lark-*` 软链进 `~/.claude/skills/`）。
//! 幂等、best-effort、绝不阻塞 bot 启动——装不上只 log 警告给手动命令。
//!
//! 触发点：service 启动（非 GUI 路径）+ GUI 保存（GUI 路径），仅当存在「启用的飞书 bot」。

use std::path::PathBuf;
use std::process::Stdio;

/// 技能就绪判据：~/.claude/skills/lark-im 能解析（fs::metadata 随软链到 ~/.agents/skills）。
/// 取 lark-im 当代表（27 个 lark-* 同进同出，任一个在即在）。
fn skills_ready() -> bool {
    let p = dirs::home_dir()
        .unwrap_or_default()
        .join(".claude/skills/lark-im");
    std::fs::metadata(&p).is_ok()
}

/// 失败节流 marker：~/.agent-bridge/logs/.lark-setup-attempt（内容 = unix 秒）。
fn marker_path() -> PathBuf {
    crate::bridge_dir().join("logs/.lark-setup-attempt")
}

const THROTTLE_SECS: u64 = 24 * 3600;

/// 距上次尝试不足 24h → true（跳过，别每次重启都用 npx 锤网络）。
fn throttled() -> bool {
    if let Ok(text) = std::fs::read_to_string(marker_path()) {
        if let Ok(ts) = text.trim().parse::<u64>() {
            let now = crate::chrono_lite::unix_secs();
            return now.saturating_sub(ts) < THROTTLE_SECS;
        }
    }
    false
}

fn touch_marker() {
    let p = marker_path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, crate::chrono_lite::unix_secs().to_string());
}

fn clear_marker() {
    let _ = std::fs::remove_file(marker_path());
}

/// 入口：有任何启用的飞书 bot 时调用。自检 → 装 lark-cli → 装技能。永不 panic / 不传错给调用方。
pub async fn ensure_lark_setup() {
    // 快路径：lark-cli 在 PATH 且技能已就位 → 啥都不做
    let cli_there = crate::deps::find_in_path("lark-cli").is_some();
    if cli_there && skills_ready() {
        return;
    }
    if throttled() {
        crate::log!("[lark] 距上次安装尝试不足 24h，跳过（节流）");
        return;
    }
    touch_marker();

    // 第一步：lark-cli 本体
    if !cli_there {
        crate::log!("[lark] 未找到 lark-cli，尝试 npm 安装 @larksuite/cli");
        if crate::deps::find_in_path("npm").is_none() {
            crate::log!("[lark] ⚠️ 无 npm，无法装 lark-cli（请先在「环境配置」装 Node.js）");
            return;
        }
        match run("npm", &["install", "-g", "@larksuite/cli"], None).await {
            Ok(_) => crate::log!("[lark] lark-cli 安装成功"),
            Err(e) => {
                crate::log!("[lark] ⚠️ lark-cli 安装失败：{e}");
                // 不 return：技能可能已就位（只是 cli 缺），继续尝试技能步骤也无妨
            }
        }
    }

    // 第二步：lark-* 技能
    if skills_ready() {
        clear_marker();
        return;
    }
    crate::log!("[lark] 未找到 lark 技能，尝试 npx skills add larksuite/cli");
    if crate::deps::find_in_path("npx").is_none() {
        crate::log!("[lark] ⚠️ 无 npx，走 git 兜底");
        install_via_git().await;
        return;
    }
    // 非交互安装全部 lark-* 技能到 claude：-y 跳过确认、-g 全局、-a 指定 agent、-s '*' 全技能。
    // CI=1 双保险防任何交互提示。首跑 npx 要下载包，给足超时。
    let args = [
        "-y",
        "skills",
        "add",
        "larksuite/cli",
        "-g",
        "-a",
        "claude",
        "-s",
        "*",
        "-y",
    ];
    match run("npx", &args, Some(std::time::Duration::from_secs(600))).await {
        Ok(_) if skills_ready() => {
            crate::log!("[lark] lark 技能安装成功（~/.claude/skills/lark-*）");
            clear_marker();
        }
        Ok(_) => {
            crate::log!("[lark] npx 退出0但技能未就位，走 git 兜底");
            install_via_git().await;
        }
        Err(e) => {
            crate::log!("[lark] ⚠️ npx skills 安装失败：{e}，走 git 兜底");
            install_via_git().await;
        }
    }
}

/// 兜底：git sparse-clone larksuite/cli → copy skills/lark-* 到 ~/.agents/skills/ →
/// 软链进 ~/.claude/skills/（跳过已存在）→ 清 tmp。git 也没有/失败 → 仅 log 手动命令。
async fn install_via_git() {
    if crate::deps::find_in_path("git").is_none() {
        crate::log!("[lark] ⚠️ 无 git。请手动执行：npx -y skills add larksuite/cli -g -a claude -s '*' -y");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("agent-bridge-lark-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let url = "https://github.com/larksuite/cli.git";
    let clone = run(
        "git",
        &[
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            url,
            &tmp.to_string_lossy(),
        ],
        Some(std::time::Duration::from_secs(300)),
    )
    .await;
    if let Err(e) = clone {
        crate::log!("[lark] ⚠️ git clone 失败：{e}。请手动执行：npx -y skills add larksuite/cli -g -a claude -s '*' -y");
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    // 只取 skills/ 子树
    let tmp_s = tmp.to_string_lossy().into_owned();
    let _ = run(
        "git",
        &["-C", &tmp_s, "sparse-checkout", "set", "skills"],
        Some(std::time::Duration::from_secs(60)),
    )
    .await;

    let home = dirs::home_dir().unwrap_or_default();
    let agents_skills = home.join(".agents/skills");
    let claude_skills = home.join(".claude/skills");
    let _ = std::fs::create_dir_all(&agents_skills);
    let _ = std::fs::create_dir_all(&claude_skills);

    let mut copied = 0;
    let skills_src = tmp.join("skills");
    if let Ok(rd) = std::fs::read_dir(&skills_src) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if !name.starts_with("lark-") || !ent.path().is_dir() {
                continue;
            }
            let dst = agents_skills.join(&name);
            if !dst.exists() && copy_dir(&ent.path(), &dst).is_ok() {
                copied += 1;
            }
            // 软链进 ~/.claude/skills（跳过已存在）
            let link = claude_skills.join(&name);
            if std::fs::symlink_metadata(&link).is_err() {
                let rel = PathBuf::from("../../.agents/skills").join(&name);
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink(&rel, &link);
                #[cfg(windows)]
                let _ = std::os::windows::fs::symlink_dir(&dst, &link);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    if skills_ready() {
        crate::log!("[lark] git 兜底成功：copy {copied} 个 lark-* 技能并软链进 ~/.claude/skills");
        clear_marker();
    } else {
        crate::log!("[lark] ⚠️ git 兜底后技能仍未就位。请手动执行：npx -y skills add larksuite/cli -g -a claude -s '*' -y");
    }
}

/// 递归 copy 目录（无 fs_extra 依赖，手写）。
fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let (s, d) = (ent.path(), dst.join(ent.file_name()));
        if s.is_dir() {
            copy_dir(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

/// 跑一个命令，捕获输出尾；可选超时（超时杀掉算失败）。Ok=0 退出（返回输出尾），Err=非零/超时/启动失败。
async fn run(
    prog: &str,
    args: &[&str],
    timeout: Option<std::time::Duration>,
) -> Result<String, String> {
    let path = crate::deps::find_in_path(prog).ok_or_else(|| format!("找不到 {prog}"))?;
    let mut cmd = tokio::process::Command::new(path);
    cmd.args(args)
        .env("PATH", crate::deps::composed_path())
        .env("CI", "1") // 双保险防交互
        .env("LARKSUITE_CLI_NO_UPDATE_NOTIFIER", "1")
        .env("LARKSUITE_CLI_NO_SKILLS_NOTIFIER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("启动 {prog} 失败：{e}"))?;
    let work = async {
        let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
        let mut tail = String::from_utf8_lossy(&out.stdout).into_owned();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        if !err.is_empty() {
            if !tail.is_empty() {
                tail.push('\n');
            }
            tail.push_str(&err);
        }
        if out.status.success() {
            Ok(tail)
        } else {
            Err(format!("退出码 {:?}：{}", out.status.code(), crate::agent::truncate(&tail, 400)))
        }
    };
    match timeout {
        Some(d) => match tokio::time::timeout(d, work).await {
            Ok(r) => r,
            Err(_) => Err(format!("超时（{}s）", d.as_secs())),
        },
        None => work.await,
    }
}
