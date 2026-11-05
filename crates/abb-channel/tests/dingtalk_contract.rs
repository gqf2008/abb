//! 用**真的子进程**验契约：起 `abb-channel-dingtalk`，冒充 ABB 走一遍协议。
//!
//! 为什么不做成单测（在 `src/main.rs` 里 mock）：单测只能验「函数返回什么」，
//! 验不了「进程按 NDJSON 收发、stdout 不被日志污染」。而后者恰恰是渠道进程
//! 最容易坏的地方——一行 `println!` 就能让 ABB 侧解析炸掉。
//!
//! 判据对齐 `docs/mcp-bus-architecture.md` §3：
//! - `tools/list` **精确等于**本渠道真的实现的那 8 个工具（2 个必需 + 6 个可选能力）；
//!   **`send_thread_reply` 必须不存在**（钉钉无话题概念——这一条与飞书版正好相反），
//!   `typing` / `del_typing` / `done` / `note_context` / `get_quoted_message` / `poll`
//!   同样一个都不许声明（做不到或无此概念，理由见 crate README §2）
//! - 入站走 **资源订阅**（`resources/list` 精确等于 `["channel://inbox"]`）
//! - 未知方法回 error；可选能力查不到数据时回 error（不回空串）

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn bin() -> String {
    // 集成测试里用同一次构建产出的二进制。
    env!("CARGO_BIN_EXE_abb-channel").to_string()
}

/// 起进程、发请求、收响应。
fn roundtrip(reqs: &[&str]) -> Vec<serde_json::Value> {
    let mut child = Command::new(bin())
        // 合并成一个二进制之后，渠道由 `--transport` 指定（原来是三个独立二进制）。
        .arg("--transport")
        .arg("dingtalk")
        // 空凭证：不启 Stream 长连接（`main` 里有判断），契约层必须照样工作。
        .env("DT_APP_ID", "")
        .env("DT_APP_SECRET", "")
        .env("DT_ROBOT_CODE", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // stderr 是日志，不该看
        .spawn()
        .expect("起不来 abb-channel-dingtalk");

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
    assert_eq!(
        got[0]["result"]["serverInfo"]["name"],
        "abb-channel-dingtalk"
    );
    // 声明 resources 是必须的：ABB 据 `resources/subscribe` 能力选入站路线（A 方案）。
    assert_eq!(
        got[0]["result"]["capabilities"]["resources"]["subscribe"],
        serde_json::json!(true)
    );
}

/// `tools/list` **精确等于**真的实现了的那 8 个（多一条 = 虚报能力，少一条 = 静默退化）。
///
/// 必须逐名核对而不是「包含某几个」：
/// - 缺 `send_text` ⇒ ABB 判渠道不可用；
/// - 多声明 `send_thread_reply` / `typing` / `done` / `note_context` / `get_quoted_message`
///   ⇒ ABB 会走到必然失败（或注定落空）的路径——钉钉没有话题、没有表情 API。
#[test]
fn declares_exactly_the_implemented_tools() {
    let got = roundtrip(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#]);
    let tools = got[0]["result"]["tools"].as_array().expect("tools 数组");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(
        names,
        vec![
            "send_text",
            "send_attachment",
            "create_chat",
            "update_chat",
            "download_attachment",
            "get_chat_info",
            "chat_brief",
            "user_display_name",
            "note_sender",
        ],
        "工具集必须精确等于实际实现的那几个"
    );
    for absent in [
        "send_thread_reply",
        "typing",
        "del_typing",
        "done",
        "note_context",
        "get_quoted_message",
        "poll",
    ] {
        assert!(
            !names.contains(&absent),
            "钉钉做不到（或无此概念），不该声明 `{absent}`。实际：{names:?}"
        );
    }
    // 每个工具的必需参数都要在 schema 里 —— 只写名字不写 schema 等于让调用方猜。
    let required = |name: &str| -> serde_json::Value {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("没有工具 {name}"))["inputSchema"]["required"]
            .clone()
    };
    assert_eq!(
        required("send_text"),
        serde_json::json!(["chat_id", "text"])
    );
    assert_eq!(
        required("send_attachment"),
        serde_json::json!(["chat_id", "name", "data_b64"])
    );
    assert_eq!(
        required("create_chat"),
        serde_json::json!(["name", "owner_user_id"])
    );
    assert_eq!(
        required("download_attachment"),
        serde_json::json!(["message_id", "seq"])
    );
    assert_eq!(required("get_chat_info"), serde_json::json!(["chat_id"]));
    assert_eq!(required("chat_brief"), serde_json::json!(["chat_id"]));
    assert_eq!(
        required("user_display_name"),
        serde_json::json!(["user_id"])
    );
    assert_eq!(
        required("note_sender"),
        serde_json::json!(["chat_id", "sender_id"])
    );
}

/// 可选能力在**没有凭证**时也必须回 error，而不是回一个空路径/空名字（静默空串
/// 会让 ABB 把「没下载」当成「下载了」）。`download_attachment` 的未知 message_id
/// 在联网前就该被台账拒掉，所以这条不需要真凭证。
#[test]
fn download_attachment_unknown_message_errors_before_network() {
    let got = roundtrip(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"download_attachment","arguments":{"message_id":"mid-never-seen","seq":0}}}"#,
    ]);
    let err = got[0]
        .get("error")
        .unwrap_or_else(|| panic!("必须回 error：{}", got[0]));
    assert!(
        err["message"]
            .as_str()
            .unwrap_or("")
            .contains("没有 message_id"),
        "报因要说是没记到这条消息：{err}"
    );
}

/// **入站：本渠道用资源订阅**，且 `resources/list` **精确等于** `["channel://inbox"]`。
/// 多一条或一条都没有都会让 ABB 的路线判定出错。
#[test]
fn inbox_resource_is_declared_and_empty_read_is_json_array() {
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
        "入站资源必须精确等于 channel://inbox"
    );
    assert_eq!(got[1]["result"], serde_json::json!({}), "订阅要回空对象");
    // read 的正文必须是**合法 JSON 数组**：ABB 侧解析失败会整批报错。
    // 空缓冲也要是 `[]`，不是 `null`、不是空串。
    let text = got[2]["result"]["contents"][0]["text"]
        .as_str()
        .expect("contents[0].text 必须是字符串");
    let parsed: serde_json::Value = serde_json::from_str(text).expect("text 必须是合法 JSON");
    assert_eq!(parsed, serde_json::json!([]), "空缓冲要序列化成 []");
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

/// 调一个不存在的工具要回 error（不是回 ok）——尤其 `send_thread_reply`：
/// ABB 若真来调它（例如配置写错渠道），必须**响亮失败**而不是假装送达。
#[test]
fn calling_thread_reply_errors_instead_of_silently_succeeding() {
    let got = roundtrip(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"send_thread_reply","arguments":{"chat_id":"c","message_id":"m","text":"hi"}}}"#,
    ]);
    let err = got[0]
        .get("error")
        .unwrap_or_else(|| panic!("必须回 error：{}", got[0]));
    assert!(
        err["message"].as_str().unwrap_or("").contains("未知工具"),
        "报因要说是未知工具：{err}"
    );
}
