//! 黑盒回归：其余 CLI bot-key 入口必须使用真实二进制，在临时 HOME 下证明
//! “非法 key 不落盘 / 显式来源优先 / alias 收敛到规范 key”。

use std::io::Write;
use std::path::PathBuf;
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        for key in [
            "AGENT_BRIDGE_BOT_KEY",
            "AGENT_BRIDGE_CHAT_ID",
            "AGENT_BRIDGE_SENDER_ROLE",
        ] {
            cmd.env_remove(key);
        }
        cmd.env("HOME", &self.root);
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
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
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
