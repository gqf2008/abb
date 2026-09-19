use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct KillChild(Option<Child>);

impl Drop for KillChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn send_line(writer: &mut impl Write, value: &Value) {
    serde_json::to_writer(&mut *writer, value).expect("serialize request");
    writer.write_all(b"\n").expect("write newline");
    writer.flush().expect("flush request");
}

fn read_line(reader: &mut impl BufRead) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response line");
    assert!(!line.is_empty(), "MCP server closed stdout unexpectedly");
    println!("E2E RX {}", line.trim_end());
    serde_json::from_str(&line).expect("response must be JSON")
}

#[test]
fn stdio_mcp_events_complete_session_and_error_branches() {
    let root = std::env::temp_dir().join(format!(
        "abb-mcp-events-e2e-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let bridge_home = root.join("bridge");
    let events_repo = root.join("events-repo");
    std::fs::create_dir_all(&bridge_home).expect("create isolated bridge home");
    std::fs::create_dir_all(&events_repo).expect("create events repo");
    let git_status = Command::new("git")
        .arg("-C")
        .arg(&events_repo)
        .args(["init", "-q"])
        .status()
        .expect("git should be available for the e2e fixture");
    assert!(git_status.success(), "git init failed");
    let remote_origin = root.join("origin.git");
    let remote_status = Command::new("git")
        .arg("-C")
        .arg(&events_repo)
        .args(["remote", "add", "origin"])
        .arg(&remote_origin)
        .status()
        .expect("git remote add");
    assert!(remote_status.success(), "git remote add failed");
    let entry_path = root.join("walgit-entry.json");
    std::fs::write(
        &entry_path,
        r#"{"version":1,"kind":"issue","id":"e2e-walgit-thread","actor":"e2e","ts":1,"body":{"title":"E2E walgit event"}}"#,
    )
    .expect("write walgit fixture");
    let oid_output = Command::new("git")
        .arg("-C")
        .arg(&events_repo)
        .args(["hash-object", "-w"])
        .arg(&entry_path)
        .output()
        .expect("git hash-object");
    assert!(oid_output.status.success(), "git hash-object failed");
    let oid = String::from_utf8_lossy(&oid_output.stdout)
        .trim()
        .to_string();
    let ref_status = Command::new("git")
        .arg("-C")
        .arg(&events_repo)
        .args(["update-ref", "refs/collab/inbox/e2e/entry", &oid])
        .status()
        .expect("git update-ref");
    assert!(ref_status.success(), "git update-ref failed");

    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("mcp-events")
        .current_dir(&root)
        .env("AGENT_BRIDGE_HOME", &bridge_home)
        .env("ABB_EVENTS_REPO", &events_repo)
        .env("ABB_EVENTS_WALGIT_REMOTE", "none")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent-bridge mcp-events");
    let mut stdin = BufWriter::new(child.stdin.take().expect("child stdin"));
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let guard = KillChild(Some(child));

    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "clientInfo": { "name": "e2e", "version": "1" },
                "capabilities": {}
            }
        }),
    );
    let initialized = read_line(&mut stdout);
    assert_eq!(initialized["id"], 1);
    assert_eq!(initialized["result"]["protocolVersion"], "2024-11-05");
    assert!(initialized["result"]["capabilities"]["tools"].is_object());

    send_line(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );
    send_line(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
    );
    let ping = read_line(&mut stdout);
    assert_eq!(
        ping["id"], 2,
        "notifications/initialized must not emit a response"
    );
    assert_eq!(ping["result"], json!({}));

    send_line(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    );
    let tools = read_line(&mut stdout);
    assert_eq!(tools["id"], 3);
    assert_eq!(
        tools["result"]["tools"]
            .as_array()
            .expect("tools array")
            .len(),
        4
    );

    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "events_subscribe",
                "arguments": { "kinds": ["task_failed"] }
            }
        }),
    );
    let subscription = read_line(&mut stdout);
    assert_eq!(subscription["id"], 4);
    let subscription_id = subscription["result"]["structuredContent"]["subscription_id"]
        .as_str()
        .expect("subscription id")
        .to_string();
    assert!(!subscription_id.is_empty());
    assert_eq!(
        subscription["result"]["structuredContent"]["warnings"],
        json!([])
    );

    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "no_such_tool", "arguments": {} }
        }),
    );
    let unknown_tool = read_line(&mut stdout);
    assert_eq!(unknown_tool["id"], 5);
    assert_eq!(unknown_tool["error"]["code"], -32602);

    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "events_wait",
                "arguments": { "timeout_secs": 301 }
            }
        }),
    );
    let invalid_timeout = read_line(&mut stdout);
    assert_eq!(invalid_timeout["id"], 6);
    assert_eq!(invalid_timeout["error"]["code"], -32602);

    let started = Instant::now();
    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "events_wait",
                "arguments": {
                    "subscription_id": subscription_id,
                    "timeout_secs": 5
                }
            }
        }),
    );
    let bot_dir = bridge_home.join("tasks").join("bot-e2e");
    std::fs::create_dir_all(&bot_dir).expect("create task dir");
    std::fs::write(
        bot_dir.join("tasks.json"),
        r#"[{"id":"tk_e2e","name":"端到端验证任务"}]"#,
    )
    .expect("write task definitions");
    std::fs::write(
        bot_dir.join("tasks-state.json"),
        r#"{"tk_e2e":{"kind":"failed","finished_at":2000,"last_error":"E2E 故意失败"}}"#,
    )
    .expect("write task state");

    let waited = read_line(&mut stdout);
    assert_eq!(waited["id"], 7);
    let event = &waited["result"]["structuredContent"]["event"];
    assert_eq!(event["kind"], "task_failed");
    assert_eq!(event["task_id"], "tk_e2e");
    assert_eq!(event["payload"]["display_name"], "端到端验证任务");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "events_wait should return promptly after injection; elapsed={:?}",
        started.elapsed()
    );

    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": { "name": "events_list", "arguments": { "limit": 10 } }
        }),
    );
    let listed = read_line(&mut stdout);
    assert_eq!(listed["id"], 8);
    let events = listed["result"]["structuredContent"]["events"]
        .as_array()
        .expect("events array");
    assert_eq!(events.len(), 2, "configured walgit source must be present");
    assert!(events.iter().any(|event| event["task_id"] == "tk_e2e"));
    assert!(events
        .iter()
        .any(|event| event["kind"] == "issue" && event["thread"] == "e2e-walgit-thread"));
    assert_eq!(listed["result"]["structuredContent"]["warnings"], json!([]));

    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "events_unsubscribe",
                "arguments": { "subscription_id": subscription_id }
            }
        }),
    );
    let unsubscribed = read_line(&mut stdout);
    assert_eq!(unsubscribed["id"], 9);
    assert_eq!(unsubscribed["result"]["structuredContent"]["removed"], true);

    send_line(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 10, "method": "no/such/method" }),
    );
    let unknown_method = read_line(&mut stdout);
    assert_eq!(unknown_method["id"], 10);
    assert_eq!(unknown_method["error"]["code"], -32601);

    drop(stdin);
    drop(guard);
    let _ = std::fs::remove_dir_all(root);
}
