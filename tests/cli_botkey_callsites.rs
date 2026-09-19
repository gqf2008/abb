//! 黑盒回归：其余 CLI bot-key 入口必须使用真实二进制，在临时 HOME 下证明
//! “非法 key 不落盘 / 显式来源优先 / alias 收敛到规范 key”。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

struct TempHome {
    root: PathBuf,
}

impl TempHome {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("abb-cli-botkey-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn bridge_dir(&self) -> PathBuf {
        self.root.join(".agent-bridge")
    }

    fn write_config(&self, bots: Value) {
        let bridge = self.bridge_dir();
        std::fs::create_dir_all(&bridge).unwrap();
        let cfg = json!({
            "cross_delivery_enabled": true,
            "bots": bots,
        });
        std::fs::write(
            bridge.join("config.json"),
            serde_json::to_vec_pretty(&cfg).unwrap(),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str], envs: &[(&str, &str)], stdin: &str) -> Output {
        self.run_at(&self.root, &self.bridge_dir(), args, envs, stdin)
    }

    fn run_at(
        &self,
        home: &Path,
        bridge_home: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
        stdin: &str,
    ) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        for key in [
            "AGENT_BRIDGE_BOT_KEY",
            "AGENT_BRIDGE_CHAT_ID",
            "AGENT_BRIDGE_SENDER_ROLE",
            "AGENT_BRIDGE_HOME",
            "ABB_AGENT_CONTEXT",
        ] {
            cmd.env_remove(key);
        }
        cmd.env("HOME", home);
        cmd.env("AGENT_BRIDGE_HOME", bridge_home);
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            cmd.env("TMPDIR", tmpdir);
        }
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn run_owned(&self, args: &[String], envs: &[(&str, &str)], stdin: &str) -> Output {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&refs, envs, stdin)
    }

    /// 模拟 buzz-agent `dev__shell` 的干净子进程环境：只有 HOME/PATH/TMPDIR，
    /// 再加调用方显式注入的 ACP 标记；不带 AGENT_BRIDGE_HOME（Unix 下 HOME 足够）。
    #[cfg(unix)]
    fn run_minimal(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        cmd.env_clear();
        cmd.env("HOME", &self.root);
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            cmd.env("TMPDIR", tmpdir);
        }
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(b"").unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Windows 的 `dirs::home_dir()` 不读 `HOME`；运行数据目录必须由
/// `AGENT_BRIDGE_HOME` 独立覆盖，否则测试会落到真实用户目录并读到空配置。
#[test]
fn agent_bridge_home_overrides_platform_home() {
    let data_home = TempHome::new();
    data_home.write_config(json!([standard_bot()]));
    let unrelated_home = TempHome::new();

    let out = data_home.run_at(
        &unrelated_home.root,
        &data_home.bridge_dir(),
        &[
            "deliver", "--bot", "app_safe", "--chat", "oc_main", "--text", "hi",
        ],
        &[],
        "",
    );

    assert_ok(&out);
    let queued = data_home.bridge_dir().join("deliveries.json");
    assert!(queued.is_file(), "投递应写入 AGENT_BRIDGE_HOME: {queued:?}");
    assert!(
        !unrelated_home.bridge_dir().join("deliveries.json").exists(),
        "不得回落到 HOME 下的运行数据目录"
    );
}

fn standard_bot() -> Value {
    json!({
        "name": "显示名",
        "kind": "feishu",
        "enabled": true,
        "app_id": "app_safe",
        "app_secret": "secret",
        "primary_chat_id": "oc_main",
    })
}

fn unsafe_bot() -> Value {
    json!({
        "name": "CON",
        "kind": "feishu",
        "enabled": true,
        "app_id": "CON",
        "app_secret": "secret",
        "primary_chat_id": "oc_main",
    })
}

fn other_bot() -> Value {
    json!({
        "name": "其他bot",
        "kind": "feishu",
        "enabled": true,
        "app_id": "other_bot",
        "app_secret": "secret",
        "primary_chat_id": "oc_other",
    })
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "command failed: status={:?}\\nstdout={}\\nstderr={}",
        out.status,
        stdout(out),
        stderr(out)
    );
}

#[test]
fn deliver_explicit_source_overrides_bad_env_and_bad_env_without_source_still_fails() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let out = home.run(
        &[
            "deliver",
            "--source-bot",
            "app_safe",
            "--source-chat",
            "oc_src",
            "--bot",
            "app_safe",
            "--chat",
            "oc_main",
            "--text",
            "hi",
        ],
        &[("AGENT_BRIDGE_BOT_KEY", "../../evil")],
        "",
    );
    assert_ok(&out);
    let queued: Value = serde_json::from_str(
        &std::fs::read_to_string(home.bridge_dir().join("deliveries.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(queued[0]["source_bot"], "app_safe");

    let bad_home = TempHome::new();
    bad_home.write_config(json!([standard_bot()]));
    let out = bad_home.run(
        &[
            "deliver", "--bot", "app_safe", "--chat", "oc_main", "--text", "hi",
        ],
        &[("AGENT_BRIDGE_BOT_KEY", "../../evil")],
        "",
    );
    assert!(!out.status.success(), "坏 env 被实际采用时必须失败");
    assert!(
        !bad_home.bridge_dir().join("deliveries.json").exists(),
        "失败不得入队"
    );
}

#[test]
fn granted_deliver_rejects_cross_workspace_source_and_allows_own_workspace() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot(), other_bot()]));
    let own_file = home.bridge_dir().join("workspaces/app_safe/own.txt");
    std::fs::create_dir_all(own_file.parent().unwrap()).unwrap();
    std::fs::write(&own_file, "own").unwrap();
    let other_file = home.bridge_dir().join("workspaces/other_bot/secret.txt");
    std::fs::create_dir_all(other_file.parent().unwrap()).unwrap();
    std::fs::write(&other_file, "secret").unwrap();

    let malicious = vec![
        "deliver".to_string(),
        "--source-bot".to_string(),
        "other_bot".to_string(),
        "--source-chat".to_string(),
        "oc_src".to_string(),
        "--bot".to_string(),
        "other_bot".to_string(),
        "--chat".to_string(),
        "oc_other".to_string(),
        "--file".to_string(),
        other_file.to_string_lossy().into_owned(),
        "--text".to_string(),
        "leak".to_string(),
    ];
    let out = home.run_owned(
        &malicious,
        &[
            ("AGENT_BRIDGE_BOT_KEY", "app_safe"),
            ("AGENT_BRIDGE_SENDER_ROLE", "granted"),
        ],
        "",
    );
    assert!(!out.status.success(), "granted 不得切换附件边界");
    assert!(
        !home.bridge_dir().join("deliveries.json").exists(),
        "恶意投递不得入队"
    );
    assert!(
        stderr(&out).contains("不能修改来源身份"),
        "{}",
        stderr(&out)
    );

    let positive = vec![
        "deliver".to_string(),
        "--source-bot".to_string(),
        "app_safe".to_string(),
        "--source-chat".to_string(),
        "oc_src".to_string(),
        "--bot".to_string(),
        "other_bot".to_string(),
        "--chat".to_string(),
        "oc_other".to_string(),
        "--file".to_string(),
        own_file.to_string_lossy().into_owned(),
        "--text".to_string(),
        "ok".to_string(),
    ];
    let out = home.run_owned(
        &positive,
        &[
            ("AGENT_BRIDGE_BOT_KEY", "app_safe"),
            ("AGENT_BRIDGE_SENDER_ROLE", "granted"),
        ],
        "",
    );
    assert_ok(&out);
    let queued: Value = serde_json::from_str(
        &std::fs::read_to_string(home.bridge_dir().join("deliveries.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(queued[0]["source_bot"], "app_safe");
}

#[test]
fn trash_purge_rejects_unknown_alias_without_touching_ghost_workspace() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let ghost = home.bridge_dir().join("workspaces").join("隔壁老王");
    let item = ghost.join(".trash/items/sentinel");
    std::fs::create_dir_all(&item).unwrap();
    std::fs::write(item.join("keep.txt"), "must-survive").unwrap();
    let manifest = ghost.join(".trash/manifest.json");
    let before = r#"[{"id":"sentinel","orig":"/tmp/x","trashed_at":0,"size":1,"dangerous":false,"reason":"test"}]"#;
    std::fs::write(&manifest, before).unwrap();

    let out = home.run(&["trash", "purge", "--all", "--bot", "隔壁老王"], &[], "");
    assert!(!out.status.success(), "未知 alias 必须拒绝");
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), before);
    assert!(item.join("keep.txt").exists(), "幽灵回收站不得被 purge");
}

#[test]
fn wsver_rejects_unknown_alias_before_status() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let ghost = home.bridge_dir().join("workspaces").join("隔壁老王");
    std::fs::create_dir_all(&ghost).unwrap();
    std::fs::write(ghost.join("sentinel.txt"), "keep").unwrap();

    let out = home.run(&["wsver", "status", "--bot", "隔壁老王"], &[], "");
    assert!(!out.status.success(), "未知 alias 不得进入 wsver 状态路径");
    assert!(ghost.join("sentinel.txt").exists());
}

#[test]
fn session_import_rejects_unknown_alias_before_import() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let ghost = home.bridge_dir().join("workspaces").join("隔壁老王");
    std::fs::create_dir_all(&ghost).unwrap();
    std::fs::write(ghost.join("sentinel.txt"), "keep").unwrap();

    let out = home.run(&["session-import", "--bot", "隔壁老王"], &[], "");
    assert!(!out.status.success(), "未知 alias 不得当 import 目标");
    assert!(ghost.join("sentinel.txt").exists());
}

#[test]
fn session_import_full_scan_rejects_unsafe_configured_key() {
    let home = TempHome::new();
    home.write_config(json!([unsafe_bot()]));
    let out = home.run(&["session-import", "--dry-run"], &[], "");
    assert!(!out.status.success(), "全量导入也必须拒绝 CON");
    assert!(
        stderr(&out).contains("安全的单一路径组件"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn session_list_full_scan_rejects_unsafe_configured_key() {
    let home = TempHome::new();
    home.write_config(json!([unsafe_bot()]));
    let out = home.run(&["session", "list"], &[], "");
    assert!(!out.status.success(), "全量 list 也必须拒绝 CON");
    assert!(stderr(&out).contains("CON"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("安全的单一路径组件"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn session_reset_rejects_unsafe_canonical_key_without_creating_workspace() {
    let home = TempHome::new();
    home.write_config(json!([unsafe_bot()]));
    let out = home.run(
        &["session", "reset"],
        &[
            ("AGENT_BRIDGE_BOT_KEY", "CON"),
            ("AGENT_BRIDGE_CHAT_ID", "oc_x"),
        ],
        "",
    );
    assert!(!out.status.success(), "CON 不得作为 session 工作区键");
    assert!(
        !home
            .bridge_dir()
            .join("workspaces/CON/sessions.json")
            .exists(),
        "解析失败不得创建/写会话状态"
    );
}

#[test]
fn session_pause_resume_and_list_converge_alias_and_reject_bad_env() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let out = home.run(
        &["session", "pause", "oc_x"],
        &[("AGENT_BRIDGE_BOT_KEY", "显示名")],
        "",
    );
    assert_ok(&out);
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.bridge_dir().join("session_state.json")).unwrap(),
    )
    .unwrap();
    assert!(
        state["paused"]["app_safe"]["oc_x"].is_object(),
        "state={state}"
    );
    assert!(state["paused"].get("显示名").is_none(), "state={state}");

    let hist = home
        .bridge_dir()
        .join("workspaces/app_safe/history/oc_x.jsonl");
    std::fs::create_dir_all(hist.parent().unwrap()).unwrap();
    std::fs::write(&hist, "{}\n").unwrap();
    let out = home.run(&["session", "list", "--bot", "显示名"], &[], "");
    assert_ok(&out);
    assert!(
        stdout(&out).contains("[app_safe]"),
        "stdout={}",
        stdout(&out)
    );

    let out = home.run(
        &["session", "resume", "oc_x"],
        &[("AGENT_BRIDGE_BOT_KEY", "显示名")],
        "",
    );
    assert_ok(&out);

    let bad_home = TempHome::new();
    bad_home.write_config(json!([standard_bot()]));
    let out = bad_home.run(
        &["session", "pause", "oc_x"],
        &[("AGENT_BRIDGE_BOT_KEY", "../../evil")],
        "",
    );
    assert!(!out.status.success(), "坏 env 不得暂停任意会话");
    assert!(
        !bad_home.bridge_dir().join("session_state.json").exists(),
        "解析失败不得写暂停状态"
    );
}

#[test]
fn guard_check_rejects_unsafe_canonical_key() {
    let home = TempHome::new();
    home.write_config(json!([unsafe_bot()]));
    std::fs::create_dir_all(home.bridge_dir().join("workspaces/CON")).unwrap();
    let out = home.run(
        &["guard-check"],
        &[
            ("AGENT_BRIDGE_BOT_KEY", "CON"),
            ("AGENT_BRIDGE_SENDER_ROLE", "owner"),
        ],
        r#"{"tool_name":"Bash","tool_input":{"command":"rm -rf x"}}"#,
    );
    assert_eq!(out.status.code(), Some(2), "stderr={}", stderr(&out));
    assert!(
        stdout(&out).contains(r#""permissionDecision":"deny""#),
        "stdout={}",
        stdout(&out)
    );
}

#[test]
fn guard_check_legal_key_missing_workspace_fails_closed_and_existing_allows() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"echo hi"}}"#;
    let envs = [
        ("AGENT_BRIDGE_BOT_KEY", "app_safe"),
        ("AGENT_BRIDGE_SENDER_ROLE", "owner"),
    ];

    let missing = home.run(&["guard-check"], &envs, stdin);
    assert_eq!(
        missing.status.code(),
        Some(2),
        "stderr={}",
        stderr(&missing)
    );
    let deny = stdout(&missing);
    assert!(
        deny.contains(r#""permissionDecision":"deny""#),
        "stdout={deny}"
    );
    assert!(deny.contains("工作区不可解析"), "stdout={deny}");
    assert!(!deny.contains("安全的单一路径组件"), "stdout={deny}");

    std::fs::create_dir_all(home.bridge_dir().join("workspaces/app_safe")).unwrap();
    let present = home.run(&["guard-check"], &envs, stdin);
    assert_eq!(
        present.status.code(),
        Some(0),
        "stderr={}",
        stderr(&present)
    );
    assert!(
        stdout(&present).contains(r#""permissionDecision":"allow""#),
        "stdout={}",
        stdout(&present)
    );
}

#[test]
fn guard_check_owner_rejects_abb_proc_creation() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    std::fs::create_dir_all(home.bridge_dir().join("workspaces/app_safe")).unwrap();
    let out = home.run(
        &["guard-check"],
        &[
            ("AGENT_BRIDGE_BOT_KEY", "app_safe"),
            ("AGENT_BRIDGE_CHAT_ID", "oc_agent"),
            ("AGENT_BRIDGE_SENDER_ROLE", "owner"),
        ],
        r#"{"tool_name":"Bash","tool_input":{"command":"$ABB_BIN task add --proc --cmd /bin/true"}}"#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr={}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(r#""permissionDecision":"deny""#),
        "owner guard 必须拒绝 proc 创建：{text}"
    );
    assert!(
        text.contains("不允许 agent 创建 proc 任务"),
        "拒绝原因应复用 Q8 文案：{text}"
    );
}

#[cfg(unix)]
#[test]
fn task_add_proc_rejects_acp_agent_context_without_creating_task() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let out = home.run_minimal(
        &["task", "add", "--proc", "--cmd", "/bin/true"],
        &[("ABB_AGENT_CONTEXT", "1")],
    );
    assert!(
        !out.status.success(),
        "真实 ACP dev__shell 的 agent 上下文不得创建 proc 任务"
    );
    assert!(
        stderr(&out).contains("proc 只允许 GUI/人工入口"),
        "stderr={}",
        stderr(&out)
    );
    assert!(
        !home.bridge_dir().join("tasks/app_safe/tasks.json").exists(),
        "被拒绝的 agent proc 创建不得留下 tasks.json"
    );
}

#[cfg(unix)]
#[test]
fn task_add_proc_allows_human_entry_without_agent_env() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let out = home.run_minimal(&["task", "add", "--proc", "--cmd", "/bin/true"], &[]);
    assert_ok(&out);

    let path = home.bridge_dir().join("tasks/app_safe/tasks.json");
    let tasks: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let items = tasks.as_array().expect("tasks.json 应为数组");
    assert_eq!(items.len(), 1, "人类入口应正常登记一条任务：{tasks}");
    assert_eq!(items[0]["payload"]["kind"], "proc");
    assert_eq!(items[0]["created_by"]["role"], "owner");
}

#[cfg(unix)]
#[test]
fn task_add_proc_human_multibot_uses_explicit_bot() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot(), other_bot()]));
    let out = home.run_minimal(
        &[
            "task",
            "add",
            "--bot",
            "other_bot",
            "--proc",
            "--cmd",
            "/bin/true",
        ],
        &[],
    );
    assert_ok(&out);

    let path = home.bridge_dir().join("tasks/other_bot/tasks.json");
    let tasks: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let items = tasks.as_array().expect("tasks.json 应为数组");
    assert_eq!(items.len(), 1, "多 bot 人类入口应登记到 --bot 指定目录");
    assert_eq!(items[0]["bot_key"], "other_bot");
    assert_eq!(items[0]["payload"]["kind"], "proc");
    assert_eq!(items[0]["created_by"]["role"], "owner");
    assert!(
        !home.bridge_dir().join("tasks/app_safe/tasks.json").exists(),
        "不能误写到默认/另一个 bot 的任务目录"
    );
}

#[cfg(unix)]
#[test]
fn task_add_keepalive_proc_persists_default_and_opt_out_semantics() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let out = home.run_minimal(
        &["task", "add", "--keepalive", "--proc", "--cmd", "/bin/true"],
        &[],
    );
    assert_ok(&out);
    let out = home.run_minimal(
        &[
            "task",
            "add",
            "--keepalive",
            "--proc",
            "--no-resume-on-boot",
            "--cmd",
            "/bin/true",
        ],
        &[],
    );
    assert_ok(&out);

    let path = home.bridge_dir().join("tasks/app_safe/tasks.json");
    let tasks: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let items = tasks.as_array().expect("tasks.json 应为数组");
    assert_eq!(items.len(), 2, "两条 keepalive 都应登记：{tasks}");
    for item in items {
        assert_eq!(item["payload"]["kind"], "proc");
        assert_eq!(item["trigger"]["kind"], "keepalive");
    }
    assert_eq!(items[0]["resume_on_boot"], true, "缺省必须恢复");
    assert_eq!(items[1]["resume_on_boot"], false, "opt-out 必须落 schema");
}

#[cfg(unix)]
#[test]
fn task_add_keepalive_agent_payload_is_rejected() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot()]));
    let out = home.run_minimal(
        &["task", "add", "--keepalive", "--prompt", "常驻 agent"],
        &[],
    );
    assert!(!out.status.success(), "agent 载荷的 keepalive 必须拒绝");
    assert!(
        stderr(&out).contains("只支持 proc"),
        "stderr={}",
        stderr(&out)
    );
    assert!(
        !home.bridge_dir().join("tasks/app_safe/tasks.json").exists(),
        "拒绝路径不得落任务定义"
    );
}

#[cfg(unix)]
#[test]
fn task_add_proc_acp_context_cannot_bypass_with_explicit_bot() {
    let home = TempHome::new();
    home.write_config(json!([standard_bot(), other_bot()]));
    let out = home.run_minimal(
        &[
            "task",
            "add",
            "--bot",
            "other_bot",
            "--proc",
            "--cmd",
            "/bin/true",
        ],
        &[("ABB_AGENT_CONTEXT", "1")],
    );
    assert!(
        !out.status.success(),
        "显式 --bot 不得绕过真实 ACP agent 上下文拒绝"
    );
    assert!(
        stderr(&out).contains("proc 只允许 GUI/人工入口"),
        "stderr={}",
        stderr(&out)
    );
    assert!(
        !home
            .bridge_dir()
            .join("tasks/other_bot/tasks.json")
            .exists(),
        "被拒绝的 agent proc 不得落盘"
    );
}
