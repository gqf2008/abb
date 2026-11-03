//! 用**真的子进程**验契约：起 `abb-channel-feishu`，冒充 ABB 走一遍协议。
//!
//! 为什么不做成单测（在 `src/main.rs` 里 mock）：单测只能验「函数返回什么」，
//! 验不了「进程按 NDJSON 收发、stdout 不被日志污染」。而后者恰恰是渠道进程
//! 最容易坏的地方——一行 `println!` 就能让 ABB 侧解析炸掉。
//!
//! 判据对齐 `docs/mcp-bus-architecture.md` §3：
//! - `send_text` 必须存在；`send_thread_reply` 必须声明（飞书**有话题**）
//! - 入站走 **资源订阅**（与微信版相反：那边 `resources/list` 是空数组、靠 `poll`）
//! - `channel://inbox` 必须在 `resources/list` 里出现

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn bin() -> String {
    // 集成测试里用同一次构建产出的二进制。
    env!("CARGO_BIN_EXE_abb-channel-feishu").to_string()
}

/// 起进程、发请求、收响应。
fn roundtrip(reqs: &[&str]) -> Vec<serde_json::Value> {
    let mut child = Command::new(bin())
        // 空凭证：不启长连接（`main` 里有判断），契约层必须照样工作。
        .env("FS_APP_ID", "")
        .env("FS_APP_SECRET", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // stderr 是日志，不该看
        .spawn()
        .expect("起不来 abb-channel-feishu");

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
    // 空凭证（或凭证错）都**不许**影响握手：入站失败只能打 stderr。
    assert_eq!(got[0]["result"]["serverInfo"]["name"], "abb-channel-feishu");
}

/// 出站：三个工具都得在。飞书有话题 ⇒ `send_thread_reply` 必须声明
/// （不声明的话 ABB 会把所有回复拍平到主聊天）。
#[test]
fn declares_all_three_outbound_tools() {
    let got = roundtrip(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#]);
    let tools = got[0]["result"]["tools"].as_array().expect("tools 数组");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for want in ["send_text", "send_thread_reply", "send_attachment"] {
        assert!(
            names.contains(&want),
            "缺 `{want}`（ABB 据此判能力）。实际：{names:?}"
        );
    }
    // 每个工具的必需参数都要在 schema 里 —— 只写名字不写 schema 等于让调用方猜。
    let send_text = tools.iter().find(|t| t["name"] == "send_text").unwrap();
    assert_eq!(
        send_text["inputSchema"]["required"],
        serde_json::json!(["chat_id", "text"])
    );
}

/// **入站：本渠道用资源订阅**（与微信版相反——那边 `resources/list` 是空数组）。
/// `channel://inbox` 不出现 ⇒ ABB 判「无入站能力」，渠道直接不可用。
#[test]
fn inbox_resource_is_declared_and_readable() {
    let got = roundtrip(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/list","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{"uri":"channel://inbox"}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"channel://inbox"}}"#,
    ]);
    let resources = got[0]["result"]["resources"]
        .as_array()
        .expect("resources 数组（不是 null）");
    let uris: Vec<&str> = resources.iter().filter_map(|r| r["uri"].as_str()).collect();
    assert_eq!(
        uris,
        vec!["channel://inbox"],
        "入站资源必须是 channel://inbox"
    );
    assert_eq!(got[1]["result"], serde_json::json!({}), "订阅要回空对象");
    // read 的正文必须是**合法 JSON 数组**：ABB 侧解析失败会整批报错。
    let text = got[2]["result"]["contents"][0]["text"]
        .as_str()
        .expect("contents[0].text 必须是字符串");
    let parsed: serde_json::Value = serde_json::from_str(text).expect("text 必须是合法 JSON");
    assert!(parsed.is_array(), "正文必须是消息数组：{parsed}");
}

/// **可选能力（契约 §3.4）一个都不能漏声明**：ABB 侧靠 `tools/list` 判能力，
/// 没声明的工具它永远不会调 —— 代码写了也是死代码。
///
/// 这里列的是「本渠道真实现了」的九个：ABB 缺了它们会回落 trait 默认，
/// 但飞书有能力做到，所以声明面必须与实现同步。
/// （未声明的只有 `poll` 与 `note_context`/`note_sender`，原因见 README。）
#[test]
fn declares_the_optional_contract_tools() {
    let got = roundtrip(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#]);
    let tools = got[0]["result"]["tools"].as_array().expect("tools 数组");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for want in [
        "get_quoted_message",
        "download_attachment",
        "create_chat",
        "user_display_name",
        "typing",
        "del_typing",
        "done",
        "chat_brief",
        "get_chat_info",
    ] {
        assert!(
            names.contains(&want),
            "缺 `{want}`：ABB 永远不会调没声明的工具。实际：{names:?}"
        );
    }
    // 每个新增工具的必需参数也要在 schema 里（ABB 按这些 key 传参）。
    for (tool, required) in [
        ("get_quoted_message", vec!["message_id"]),
        ("download_attachment", vec!["message_id", "seq"]),
        ("create_chat", vec!["name", "owner_user_id"]),
        ("user_display_name", vec!["user_id"]),
        ("typing", vec!["message_id"]),
        ("del_typing", vec!["message_id"]),
        ("done", vec!["message_id"]),
        ("chat_brief", vec!["chat_id"]),
        ("get_chat_info", vec!["chat_id"]),
    ] {
        let t = tools
            .iter()
            .find(|t| t["name"] == tool)
            .unwrap_or_else(|| panic!("{tool} 未声明"));
        assert_eq!(
            t["inputSchema"]["required"],
            serde_json::json!(required),
            "{tool} 的 required 与契约不一致"
        );
    }
}

/// 未知方法要回 error，不是静默成功。
#[test]
fn unknown_method_errors() {
    let got = roundtrip(&[r#"{"jsonrpc":"2.0","id":7,"method":"nope","params":{}}"#]);
    assert!(got[0].get("error").is_some(), "未知方法必须回 error");
}

/// 读未知资源必须报错（fail-closed）：读错资源 = 把别的内容当消息交出去。
#[test]
fn reading_unknown_resource_errors() {
    let got = roundtrip(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"channel://nope"}}"#,
    ]);
    assert!(
        got[0].get("error").is_some(),
        "未知资源必须回 error：{}",
        got[0]
    );
}
