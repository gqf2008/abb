//! 以**用户本人身份**发送飞书消息（`deliver --as user`）——委派本机 `lark-cli` 执行。
//!
//! ## 为什么是"委派"而不是 ABB 自己取 user_access_token
//!
//! ABB 全链路只持有 **tenant_access_token**（app 身份，见 `src/feishu.rs`）；要让消息以**用户本人**
//! 身份发出，必须持有该用户的 `user_access_token`，而它只能由用户本人走一次 OAuth 授权得到。
//! 本机 `lark-cli` 已经完成过这次授权，但**凭据是加密落盘的**
//! （`~/Library/Application Support/lark-cli/<appId>_<openId>.enc`，密钥在系统钥匙串里）——
//! ABB 既解不开，也不该去解别人的凭据库。因此 `--as user` 的形态是：
//! **`lark-cli im +messages-send --as user` 承担「授权 + 刷新 + 构造请求」三件事，ABB 只负责调用并把失败如实回报。**
//!
//! ## 身份一致性前提（本机实测 2026-10-12）
//!
//! `~/.lark-cli/config.json` 里配置的 app **就是 ABB 这个 bot 的 app**
//! （`cli_a75884b6c733900b`，授权用户 `ou_96d2c546fe6d493195b67ebb8a281286` / 高庆丰，
//! 与 `~/.agent-bridge/config.json` 的 `bots[].app_id` 一致）⇒ 以用户身份发出的消息仍属于**同一个 app**，
//! 不会出现"另一个 app 顶着本人名义发消息"。**若哪天二者 app_id 不一致，`--as user` 的语义就不再等价**
//! （那等于用另一个 app 的用户身份发），此时应视为配置错误并报错——见 [`check_app_consistency`] 的说明。
//!
//! ## 失败绝不静默回落
//!
//! `--as user` 的任何一步失败（没装 lark-cli / 没授权 / token 失效 / 超时 / 平台拒绝）都必须
//! **显式报错并回源提示**，**绝不悄悄改用 bot 身份发出**——那正是本卡要消灭的最坏失败模式
//! （用户以为是以本人身份发的，实际是 bot）。见 `Router::deliver` 的 `DeliveryIdentity::User` 分支。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 指定 `lark-cli` 可执行文件路径（覆盖 PATH 查找；测试注入假 CLI 用它）。
pub const LARK_CLI_ENV: &str = "ABB_LARK_CLI";

/// 单次发送的超时。用户身份发送要现场刷新 token（一次网络往返）再发消息，留足 60s；
/// 超时按失败处理（kill 子进程），绝不无限等待——投递消费循环是串行的，卡住会堵住整条队列。
pub const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// 可执行文件名（Windows 上按 PATHEXT 逐个试探）。
const CLI_NAME: &str = "lark-cli";

/// 解析 `lark-cli` 可执行文件：`ABB_LARK_CLI` 覆盖优先（**空串视为未设置**），否则扫 PATH。
///
/// `path_var` 与 `exts` 参数化（而不是直接读进程环境）是为了让判定可单测，
/// 不依赖跑测试的机器的 PATH；生产入口 [`resolve_cli`] 传入真实环境。
pub fn resolve_cli_in(
    path_var: &str,
    exts: &[&str],
    override_path: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = override_path.map(str::trim).filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in exts {
            let cand = dir.join(format!("{CLI_NAME}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 生产环境的解析入口：`ABB_LARK_CLI` → PATH（Windows 附带 `.exe`/`.cmd`/`.bat`）。
pub fn resolve_cli() -> Option<PathBuf> {
    let override_path = std::env::var(LARK_CLI_ENV).ok();
    let path_var = std::env::var("PATH").unwrap_or_default();
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    resolve_cli_in(&path_var, exts, override_path.as_deref())
}

/// 未安装 `lark-cli` 时的**唯一文案来源**（可操作：怎么装、怎么授权）。
pub fn missing_cli_hint() -> String {
    format!(
        "未找到 `lark-cli`（以用户身份发送需要它）：请先安装并授权本人账号——\n  \
         npm i -g lark-cli && lark-cli auth login\n  \
         若已装在非 PATH 位置，可用环境变量 {LARK_CLI_ENV}=/path/to/lark-cli 指定。\n  \
         注意：ABB 不会改用 bot 身份代发（那会让你以为消息是本人发的）。"
    )
}

/// 构造 `lark-cli im +messages-send` 的 argv（**纯函数**，可断言身份与目标）。
///
/// `--format json` 让输出可解析、也便于日志留存；`--text` 走 lark-cli 的文本分支。
pub fn send_args(chat_id: &str, text: &str) -> Vec<String> {
    vec![
        "im".to_string(),
        "+messages-send".to_string(),
        "--as".to_string(),
        "user".to_string(),
        "--chat-id".to_string(),
        chat_id.to_string(),
        "--text".to_string(),
        text.to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]
}

/// 以用户身份发送一段文本。成功 → `Ok(())`；任何异常 → `Err`（调用方必须回源报错，不得回落 bot）。
pub async fn send_text_as_user(chat_id: &str, text: &str) -> anyhow::Result<()> {
    send_text_as_user_with_timeout(chat_id, text, SEND_TIMEOUT).await
}

/// 带超时参数的实现（测试用小超时验"卡死会被 kill 并报错"）。
pub async fn send_text_as_user_with_timeout(
    chat_id: &str,
    text: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let cli = resolve_cli().ok_or_else(|| anyhow::anyhow!("{}", missing_cli_hint()))?;
    let mut cmd = tokio::process::Command::new(&cli);
    cmd.args(send_args(chat_id, text))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // 超时/提前返回时必须连带杀掉子进程，否则 lark-cli 会继续跑并真的把消息发出去——
        // "报错说没发成功、实际发出去了"是最难排查的一类不一致。
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("启动 {} 失败：{e:#}", cli.display()))?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => anyhow::bail!("等待 {} 退出失败：{e:#}", cli.display()),
        Err(_) => anyhow::bail!(
            "以用户身份发送超时（{}s）：{} 未退出，已终止。请稍后重试或先跑 `lark-cli auth status` 检查授权。",
            timeout.as_secs(),
            cli.display()
        ),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut detail = crate::agent::truncate(stderr.trim(), 400).to_string();
        if !stdout.trim().is_empty() {
            detail.push_str(&format!(
                " | stdout: {}",
                crate::agent::truncate(stdout.trim(), 200)
            ));
        }
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "被信号终止".to_string());
        anyhow::bail!(
            "以用户身份发送失败（{} 退出码 {}）：{}\n（提示：先跑 `lark-cli auth status` 确认本人授权是否有效；ABB 不会回落 bot 身份代发）",
            cli.display(),
            code,
            detail
        );
    }
    Ok(())
}

/// 读取 `lark-cli` 已授权的 app / 用户（仅用于**一致性自检**与日志，不解析任何 token）。
///
/// 返回 `(app_id, user_open_id)`；文件缺失/格式变了都返回 `None`（**不报错**：它只是可选的
/// 诊断信息，不能因为读不到 config.json 就让发送失败）。
pub fn configured_identity(config_path: &Path) -> Option<(String, String)> {
    let raw = std::fs::read_to_string(config_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let app = v.get("apps")?.as_array()?.first()?;
    let app_id = app.get("appId")?.as_str()?.to_string();
    let user = app.get("users")?.as_array()?.first()?;
    let open_id = user.get("userOpenId")?.as_str()?.to_string();
    Some((app_id, open_id))
}

/// 一致性自检（**只记日志，不拦发送**）：`--as user` 用的 app 是否就是目标 bot 的 app。
///
/// 为什么只在日志里告警而不拒绝：`lark-cli` 的实际身份由它自己的配置/切换状态决定
/// （可能用 keychain 里的多套授权之一），ABB 读到的 `config.json` 未必是最终生效的那套；
/// 拿"读到的可能过期的文件"去做硬拒绝，会把本来能发成功的场景挡死。**但**app 不一致意味着
/// 消息由别的 app 以本人名义发出（语义与卡上写的不同），必须留痕以便排查——故 `warn`。
pub fn check_app_consistency(expected_app_id: &str) {
    if expected_app_id.is_empty() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let cfg = Path::new(&home).join(".lark-cli/config.json");
    if let Some((app_id, open_id)) = configured_identity(&cfg) {
        if app_id != expected_app_id {
            crate::log!(
                "[lark_user] 警告：lark-cli 配置的 app（{}，用户 {}）与目标 bot 的 app（{}）不一致——\
                 以用户身份发出的消息将属于 lark-cli 那个 app，语义与预期不同（跨 app 替本人发消息）。",
                app_id,
                open_id,
                expected_app_id
            );
        }
    }
}

/// 测试期串行化 `ABB_LARK_CLI` 的读写：`send_text_as_user` 内部读进程环境，
/// 而 Rust 单测是**同进程多线程并行**的——两个用例同时设/删这个变量会互相串台。
/// 两个测试模块（本文件与 `deliver.rs`）共用这把锁。
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// 取锁（被 panic 毒化也照用：毒化只说明别的用例失败过，与本用例无关）。
    ///
    /// `#[cfg(unix)]` 是**cfg 对称**（2026-10-12，卡 abb-windows-ps1-exit-code-leak-20261012）：
    /// 只有 `unix` 上那几条"假 CLI 脚本"用例用它；不加门控时 Windows 上它是 dead code，
    /// 而 CI 的 `cargo clippy --all-targets -- -D warnings` 会把 `dead_code` 当 error
    /// （实测：Windows job 在 `src/lark_user.rs:215` 报 `fn env_guard is never used`）。
    /// 不要再"为了统一"把它放开：跨平台的并发用例用 `ENV_LOCK` 本身，不需要这个 helper。
    #[cfg(unix)]
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn send_args_carries_user_identity_and_target() {
        // 判别力：把 `--as user` 改成 `bot`/删掉 ⇒ 第一条断言红；这正是"以本人身份发"的全部意义。
        let args = send_args("oc_abc", "你好");
        assert!(
            args.windows(2).any(|w| w == ["--as", "user"]),
            "argv 必须带 --as user：{args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["--chat-id", "oc_abc"]),
            "argv 必须带目标 chat：{args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["--text", "你好"]),
            "argv 必须带正文：{args:?}"
        );
        assert!(args.iter().any(|a| a == "+messages-send"));
    }

    #[test]
    fn resolve_cli_prefers_env_override_and_ignores_blank() {
        let tmp = std::env::temp_dir().join(format!("abb-lark-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let fake = tmp.join("lark-cli");
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();
        let fake_s = fake.to_str().unwrap();
        // 覆盖生效（且此时 PATH 里什么都没有也能找到）
        assert_eq!(
            resolve_cli_in("", &[""], Some(fake_s)),
            Some(fake.clone()),
            "ABB_LARK_CLI 覆盖应生效"
        );
        // 空串/纯空白 = 未设置 → 回落 PATH（此处显式给空 PATH ⇒ None）
        assert_eq!(resolve_cli_in("", &[""], Some("   ")), None);
        // 覆盖指向不存在的文件 → None（不静默回落 PATH，避免"以为用了指定 CLI"）
        assert_eq!(
            resolve_cli_in("/usr/bin", &[""], Some("/nope/lark-cli")),
            None
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_cli_finds_on_path_with_exts() {
        let tmp = std::env::temp_dir().join(format!("abb-lark-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let exe = tmp.join("lark-cli.exe");
        std::fs::write(&exe, b"x").unwrap();
        let path_var = tmp.to_str().unwrap();
        // windows 扩展名列表命中 .exe；无扩展名列表则不命中
        assert_eq!(
            resolve_cli_in(path_var, &["", ".exe"], None),
            Some(exe.clone())
        );
        assert_eq!(resolve_cli_in(path_var, &[""], None), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_cli_hint_is_actionable_and_forbids_silent_fallback() {
        let h = missing_cli_hint();
        // 可操作：安装 + 授权 + 覆盖路径
        for needle in ["npm i -g lark-cli", "auth login", LARK_CLI_ENV] {
            assert!(h.contains(needle), "提示缺 {needle}: {h}");
        }
        // 明确禁止回落（本卡最坏失败模式）
        assert!(h.contains("不会改用 bot 身份代发"), "提示缺禁止回落: {h}");
    }

    #[test]
    fn configured_identity_reads_app_and_user() {
        let tmp = std::env::temp_dir().join(format!("abb-lark-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg = tmp.join("config.json");
        std::fs::write(
            &cfg,
            br#"{"apps":[{"appId":"cli_x","users":[{"userOpenId":"ou_y"}]}]}"#,
        )
        .unwrap();
        assert_eq!(
            configured_identity(&cfg),
            Some(("cli_x".to_string(), "ou_y".to_string()))
        );
        // 缺文件 / 坏 JSON / 形状不符 → None（诊断信息，不报错）
        assert_eq!(configured_identity(&tmp.join("nope.json")), None);
        std::fs::write(&cfg, b"{not json").unwrap();
        assert_eq!(configured_identity(&cfg), None);
        std::fs::write(&cfg, br#"{"apps":[]}"#).unwrap();
        assert_eq!(configured_identity(&cfg), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    fn write_fake_cli(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("lark-cli");
        std::fs::write(&p, body).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    /// 假 CLI 真的收到 `--as user` + 目标 + 正文，且退出 0 ⇒ Ok（**这是端到端的那条判据**：
    /// 身份、"打到哪个接口"由 lark-cli 承担，ABB 侧可证的就是"我把身份和目标原样交出去了"）。
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 同上：env 是进程全局，锁要横跨整个用例
    async fn fake_cli_receives_identity_and_target() {
        let _g = env_guard(); // ABB_LARK_CLI 是本进程全局，串行化
        let dir = std::env::temp_dir().join(format!("abb-lark-fake-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let rec = dir.join("argv.txt");
        let cli = write_fake_cli(
            &dir,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 0\n",
                rec.display()
            ),
        );
        std::env::set_var(LARK_CLI_ENV, cli.to_str().unwrap());
        let r = send_text_as_user("oc_t", "正文").await;
        std::env::remove_var(LARK_CLI_ENV);
        assert!(r.is_ok(), "假 CLI 退出 0 应 Ok: {r:?}");
        let got = std::fs::read_to_string(&rec).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        assert!(lines.windows(2).any(|w| w == ["--as", "user"]), "{lines:?}");
        assert!(
            lines.windows(2).any(|w| w == ["--chat-id", "oc_t"]),
            "{lines:?}"
        );
        assert!(
            lines.windows(2).any(|w| w == ["--text", "正文"]),
            "{lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非零退出（如"没有 user token"）⇒ Err 且**带上 lark-cli 的 stderr**（用户看得到真原因）。
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 同上：env 是进程全局，锁要横跨整个用例
    async fn failing_cli_surfaces_stderr() {
        let _g = env_guard(); // ABB_LARK_CLI 是本进程全局，串行化
        let dir = std::env::temp_dir().join(format!("abb-lark-fail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cli = write_fake_cli(
            &dir,
            "#!/bin/sh\necho 'no user token, run auth login' >&2\nexit 3\n",
        );
        std::env::set_var(LARK_CLI_ENV, cli.to_str().unwrap());
        let r = send_text_as_user("oc_t", "x").await;
        std::env::remove_var(LARK_CLI_ENV);
        let e = format!("{:#}", r.unwrap_err());
        assert!(e.contains("退出码 3"), "应带退出码: {e}");
        assert!(e.contains("no user token"), "应带 stderr 原文: {e}");
        assert!(e.contains("不会回落 bot"), "应重申不回落: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 超时后**必须真把子进程杀掉**：否则 lark-cli 会继续跑并可能真把消息发出去——
    /// "报错说没发成功、实际发出去了"是本仓点名的最难排查的一类不一致（见本文件生产代码注释）。
    ///
    /// 判据：脚本 `sleep 1; echo yes > <marker>`、超时 100ms、**返回后等 2s（长于脚本自身 1s）**
    /// ⇒ marker 不得出现。判别力：把 `.kill_on_drop(true)` 改 `false` ⇒ 子进程活过 1s 写下 marker ⇒ 本用例红。
    ///
    /// **前提（评审 N1 第 2 轮的实测，必须写明）**：观测窗**必须长于脚本自身时长**。
    /// 本用例第一版（脚本 sleep 5、超时后只等 300ms）踩了这个坑 ⇒ 断言恒真，我当时据此写进注释的
    /// "标记法区分不出 kill 是否生效"是**错误论断**（已更正）——评审判定该面**可测**，并给出阴性/阳性对照。
    ///
    /// **真因未实核（评审 106 的 B2 指正）**：旧的"变异后仍绿"已被"观测窗恒真"完整解释（review
    /// e6ca6bfcc49c），无需第二个原因；我曾把它另行归因为"本卷是 ExFAT、cargo 漏编"，该归因**已被
    /// 证伪**（本机 `/Volumes/DataExt` 是 APFS，mtime 粒度约 25ms，连续 6 次变异跑均出现 `Compiling`）。
    /// 保留的**可操作**医嘱只有一条：**任何"变异后仍绿"的结论，先确认构建输出里真的出现
    /// `Compiling <crate>` 再采信**（或让变异自带编译期标记，如 `include_str!` 断言）。
    /// 复核（含强制 `cargo build --tests` 确认重编）：`kill_on_drop(false)` ⇒ 本用例红，`true` ⇒ 绿。
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 同上：env 是进程全局，锁要横跨整个用例
    async fn killed_cli_does_not_write_its_marker() {
        let _g = env_guard();
        let dir = std::env::temp_dir().join(format!("abb-lark-kill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("sent.txt");
        let cli = write_fake_cli(
            &dir,
            &format!("#!/bin/sh\nsleep 1\necho yes > {}\n", marker.display()),
        );
        std::env::set_var(LARK_CLI_ENV, cli.to_str().unwrap());
        let r = send_text_as_user_with_timeout("oc_t", "x", Duration::from_millis(100)).await;
        std::env::remove_var(LARK_CLI_ENV);
        assert!(format!("{:#}", r.unwrap_err()).contains("超时"));
        // 观测窗 2s > 脚本自身 1s：这是判据成立的前提，别改短（改了就是恒真断言）。
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !marker.exists(),
            "超时后子进程必须被 kill：脚本写完的标记文件不该出现（否则消息可能已真发出）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 卡死的 CLI **不得挂住投递**：超时后必须立刻返回超时错误，而不是等子进程自己跑完
    /// （投递消费循环是串行的，卡住会堵住整条队列）。
    ///
    /// 评审 N1 指出上一版**不可证伪**：脚本 `sleep 5` 而超时后只等 300ms 就断言"标记文件不存在"
    /// ——那一刻它无论如何都不存在（恒真）。
    /// 本用例只钉"**不挂住投递**"这一半：脚本 `sleep 3`、超时 200ms ⇒ 断言①错误为超时
    /// ②整段耗时 < 2s（远小于脚本自身 3s）。判别力：去掉 `tokio::time::timeout(…)` 包装
    /// ⇒ 3s 后才返回 Ok ⇒ **在 `r.unwrap_err()` 处即红**（评审 106 的 N2 指正：`elapsed < 2s`
    /// 那条其实不会被执行到，故这里不再说"两条断言都红"）。
    /// "**真把子进程杀掉**"那一半由 `killed_cli_does_not_write_its_marker` 钉住（需要长于脚本
    /// 自身时长的观测窗，故与这里的短窗脚本分开写）。
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 同上：env 是进程全局，锁要横跨整个用例
    async fn hanging_cli_times_out_instead_of_blocking_delivery() {
        let _g = env_guard(); // ABB_LARK_CLI 是本进程全局，串行化
        let dir = std::env::temp_dir().join(format!("abb-lark-hang-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // 脚本自己跑完需要 3s；断言只看"我们没等它"。（不写"已启动"标记再断言它存在：
        // 那是个竞态——200ms 内 shell 未必已经执行到那里，且错误串含"超时"本身已证明子进程起过。）
        let cli = write_fake_cli(&dir, "#!/bin/sh\nsleep 3\n");
        std::env::set_var(LARK_CLI_ENV, cli.to_str().unwrap());
        let t0 = std::time::Instant::now();
        let r = send_text_as_user_with_timeout("oc_t", "x", Duration::from_millis(200)).await;
        let elapsed = t0.elapsed();
        std::env::remove_var(LARK_CLI_ENV);
        let e = format!("{:#}", r.unwrap_err());
        assert!(e.contains("超时"), "应报超时: {e}");
        assert!(
            elapsed < Duration::from_secs(2),
            "超时必须立刻返回（实测 {elapsed:?}）；等子进程跑完就是挂住投递队列"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
