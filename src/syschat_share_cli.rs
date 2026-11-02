//! `agent-bridge syschat-share`：把一段会话渲染成一张长图（PNG）。
//!
//! ## 为什么是子命令而不是库调用
//!
//! 渲染要装 Slint 的软件后端（`MinimalSoftwareWindow`），而 **Slint 的平台是每线程一次**：
//! GUI 进程那条线程已被 winit 占住。所以分享按钮**只能** spawn 一个子进程来做这件事
//! （见 `ui::share_render` 的模块头）。这条子命令就是那个子进程的入口。
//!
//! ## 用法
//!
//! ```text
//! agent-bridge syschat-share [--session <id>] [--out <path>] [--json]
//! ```
//!
//! - 不给 `--session` ⇒ 用当前会话；
//! - 不给 `--out` ⇒ 写到 `<bridge>/syschat/share/<session>-<ts>.png`；
//! - `--json` ⇒ stdout 打一行 JSON（GUI 侧解析用）：`{ok, path, width, height, truncated, error}`。
//!
//! ## 退出码
//!
//! 0 成功 · 1 会话不存在/无内容/渲染失败 · 2 参数错。
//!
//! **失败一律带可读报因**（GUI 要能原样讲给用户听），不静默退非零。

/// 解析并执行 `syschat-share`。
pub fn run(args: &[String]) -> i32 {
    let mut session: Option<String> = None;
    let mut out: Option<std::path::PathBuf> = None;
    let mut json = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{}", HELP);
                return 0;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--session" => {
                let Some(v) = args.get(i + 1) else {
                    return fail(json, 2, "--session 需要一个会话 id");
                };
                session = Some(v.clone());
                i += 2;
            }
            "--out" => {
                let Some(v) = args.get(i + 1) else {
                    return fail(json, 2, "--out 需要一个路径");
                };
                out = Some(std::path::PathBuf::from(v));
                i += 2;
            }
            other => {
                return fail(json, 2, &format!("未知参数「{other}」\n\n{HELP}"));
            }
        }
    }

    let id = match session.or_else(crate::syschat::current_session_id) {
        Some(v) => v,
        None => return fail(json, 1, "没有当前会话；用 --session <id> 指定"),
    };
    let Some(turns) = crate::syschat::read_all_turns(&id) else {
        return fail(json, 1, &format!("找不到会话「{id}」"));
    };

    let share = crate::syschat_share::share_blocks(&turns);
    if crate::syschat_share::is_empty(&share) {
        return fail(json, 1, "这个会话没有可分享的内容");
    }

    // 输出路径缺省走 `<bridge>/syschat/share/`：与 syschat 的其它运行数据同根，
    // 便于「按 bot / 按会话」清理，也不往用户桌面乱丢文件。
    let out = out.unwrap_or_else(|| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        crate::syschat::dir()
            .join("share")
            .join(format!("{}-{ts}.png", crate::config::sanitize(&id)))
    });

    // 渲染在 `ui::share_render`（生产模块，非测试）。它自己会装平台——
    // 本进程是全新子进程，平台还没被占，所以能装。
    match crate::ui::share_render::render_share(&share, &out) {
        Ok(r) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "path": r.path.display().to_string(),
                        "width": r.width,
                        "height": r.height,
                        "truncated": r.truncated,
                    })
                );
            } else {
                println!(
                    "已生成分享图：{}（{}×{}{}）",
                    r.path.display(),
                    r.width,
                    r.height,
                    if r.truncated {
                        "，**内容超上限已截断**"
                    } else {
                        ""
                    }
                );
            }
            0
        }
        Err(e) => fail(json, 1, &e),
    }
}

fn fail(json: bool, code: i32, msg: &str) -> i32 {
    if json {
        println!("{}", serde_json::json!({ "ok": false, "error": msg }));
    } else {
        eprintln!("⚠️ {msg}");
    }
    code
}

const HELP: &str = "\
用法: agent-bridge syschat-share [--session <id>] [--out <path>] [--json]

  把整个会话的上下文渲染成一张长图（PNG）。含 markdown、表格、代码块、
  块级公式与本地图片；**不含**思考过程、标题栏、输入区。

  --session <id>  要分享的会话；缺省用当前会话
  --out <path>    输出路径；缺省写到 <bridge>/syschat/share/
  --json          stdout 打一行 JSON（GUI 用）
  -h, --help      显示本帮助

退出码: 0 成功 · 1 会话不存在/无内容/渲染失败 · 2 参数错";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_is_available_and_exits_zero() {
        assert_eq!(run(&["--help".to_string()]), 0);
        assert_eq!(run(&["-h".to_string()]), 0);
    }

    #[test]
    fn unknown_flag_is_a_parameter_error_not_a_silent_success() {
        // 参数错必须退 2（与 chart 子命令同口径），不能当成功。
        assert_eq!(run(&["--nope".to_string()]), 2);
    }

    #[test]
    fn missing_value_for_session_is_a_parameter_error() {
        assert_eq!(run(&["--session".to_string()]), 2);
        assert_eq!(run(&["--out".to_string()]), 2);
    }

    #[test]
    fn unknown_session_is_reported_with_its_id_and_exit_one() {
        // 报因要指名是哪个会话找不到——GUI 会原样讲给用户。
        let code = run(&[
            "--session".to_string(),
            "no-such-session-zzz".to_string(),
            "--json".to_string(),
        ]);
        assert_eq!(code, 1, "找不到会话应退 1");
    }
}
