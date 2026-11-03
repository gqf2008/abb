//! 用**真的子进程**验契约：起 `abb-channel-wechat`，冒充 ABB 走一遍协议。
//!
//! 为什么不做成单测（在 `src/main.rs` 里 mock）：单测只能验「函数返回什么」，
//! 验不了「进程按 NDJSON 收发、stdout 不被日志污染」。而后者恰恰是渠道进程
//! 最容易坏的地方——一行 `println!` 就能让 ABB 侧解析炸掉。
//!
//! 判据对齐 `docs/mcp-bus-architecture.md` §3：
//! - `send_text` 必须存在
//! - 入站必须二选一（本 crate 走 `poll`）
//! - `resources/list` 显式空数组（不是 null）

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn bin() -> String {
    // 集成测试里用同一次构建产出的二进制。
    env!("CARGO_BIN_EXE_abb-channel-wechat").to_string()
}

/// 起进程、发请求、收响应。
fn roundtrip(reqs: &[&str]) -> Vec<serde_json::Value> {
    let mut child = Command::new(bin())
        .arg("--dir")
        .arg(std::env::temp_dir())
        .env("WX_TOKEN", "") // 不登录也能握手与列工具
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // stderr 是日志，不该看
        .spawn()
        .expect("起不来 abb-channel-wechat");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        for r in reqs {
            writeln!(stdin, "{r}").expect("写请求");
        }
        // drop stdin ⇒ 子进程读到 EOF 后退出
    }

    let stdout = child.stdout.take().expect("stdout");
    let got: Vec<serde_json::Value> = BufReader::new(stdout)
        .lines()
        .map_while(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(&l).expect("stdout 每一行都必须是 JSON"))
        .collect();
    let _ = child.wait();
    got
}

/// **stdout 只能是 JSON-RPC**：任何一行杂输出都会让 ABB 解析炸掉。
/// 这条用「每一行都能解成 JSON」隐式验了（解析失败会 panic）。
#[test]
fn stdout_is_pure_json_rpc() {
    let got = roundtrip(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    ]);
    assert!(got.len() >= 2, "至少要两条响应，实际 {}", got.len());
    for v in &got {
        assert_eq!(v["jsonrpc"], "2.0");
        assert!(v.get("id").is_some(), "每条响应都要带 id");
    }
}

/// 契约核心：`send_text` 必须在，`resources/list` 必须显式空数组。
#[test]
fn satisfies_channel_contract() {
    let got = roundtrip(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"resources/list","params":{}}"#,
    ]);
    let tools = got[0]["result"]["tools"].as_array().expect("tools 数组");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"send_text"),
        "缺 send_text ⇒ ABB 判该渠道不可用。实际：{names:?}"
    );
    assert!(
        names.contains(&"poll"),
        "缺 poll ⇒ 入站不可用（本 crate 不提供 resources）。实际：{names:?}"
    );
    assert!(
        !names.contains(&"send_thread_reply"),
        "微信无话题概念，不该声明它（否则 ABB 不回落 send_text）"
    );
    assert_eq!(
        got[1]["result"]["resources"],
        serde_json::json!([]),
        "resources/list 必须是空数组，不是 null"
    );
}

/// **工具列表精确等于实测清单**。
///
/// 为什么不能只写 `contains`：契约 §3.4 的可选能力是开放集合，每多声明一个
/// ABB 就会真去调它——微信做不了的能力一旦混进来，用户看到的是「调用无提示地失败」。
/// 这条断言把「声明了什么」钉在代码里，改工具面必须同时改这里（= 一次有意识的决定）。
#[test]
fn tool_list_is_exactly_the_implemented_set() {
    let got = roundtrip(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#]);
    let names: Vec<&str> = got[0]["result"]["tools"]
        .as_array()
        .expect("tools 数组")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            // 契约必需 / 入站 / 出站附件
            "send_text",
            "poll",
            "send_attachment",
            // 契约 §3.4 里微信真能做的两个
            "download_attachment",
            "note_context",
            // 扫码登录（本渠道专有）
            "login",
            "login_status",
        ],
        "工具面变了：新增/删除必须是刻意决定，并同步 README、main.rs 的单测"
    );
}

/// 可选能力里微信做不到的那些**一个都不许出现**（声明了 ABB 就会去调）。
#[test]
fn unsupported_optional_tools_never_appear() {
    let got = roundtrip(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#]);
    let names: Vec<&str> = got[0]["result"]["tools"]
        .as_array()
        .expect("tools 数组")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for tool in [
        "get_quoted_message",
        "create_chat",
        "user_display_name",
        "typing",
        "del_typing",
        "done",
        "note_sender",
        "chat_brief",
        "get_chat_info",
        "send_thread_reply",
    ] {
        assert!(
            !names.contains(&tool),
            "微信做不到 `{tool}`，不得声明（原因见 README 与 main.rs 头部）。实际：{names:?}"
        );
    }
}

/// 未实现的工具被调用 ⇒ JSON-RPC `error`（不是静默空串），
/// 且**进程不崩**（一条坏请求不能把整个渠道带走）。
#[test]
fn calling_unknown_tool_errors_and_stays_alive() {
    let got = roundtrip(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"typing","arguments":{"message_id":"m"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    ]);
    assert!(
        got[0].get("error").is_some(),
        "调用未声明的工具必须回 error：{:?}",
        got[0]
    );
    assert!(
        got[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("typing"),
        "报因要指名是哪个工具：{:?}",
        got[0]
    );
    assert_eq!(got.len(), 2, "一条坏请求不得终止进程（后面那条仍要回）");
}

/// `download_attachment` 对没见过的消息必须回 error，而不是给一个假路径。
#[test]
fn download_attachment_on_unknown_message_errors() {
    let got = roundtrip(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"download_attachment","arguments":{"message_id":"never-seen","seq":0}}}"#,
    ]);
    assert!(got[0].get("error").is_some(), "必须回 error：{:?}", got[0]);
    let msg = got[0]["error"]["message"].as_str().unwrap();
    assert!(msg.contains("缓存"), "报因要说清缓存里没有：{msg}");
    assert!(!msg.is_empty(), "不许回空串当报错");
}

/// 未知方法要回 error，不是静默成功。
#[test]
fn unknown_method_errors() {
    let got = roundtrip(&[r#"{"jsonrpc":"2.0","id":7,"method":"nope","params":{}}"#]);
    assert!(got[0].get("error").is_some(), "未知方法必须回 error");
}
