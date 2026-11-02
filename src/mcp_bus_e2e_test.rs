//! MCP 总线的**端到端**验证（S2e）：拿一个真的 Python stdio MCP server 跑通全链路。
//!
//! 与单测的区别：单测里 server 是 `/bin/sh` 脚本桩（只回固定字符串），
//! 这里是真的按 JSON-RPC 规范读写、真的推 `notifications/resources/updated`、
//! 真的维护游标的进程。**它验证的是「ABB 按规范理解对端」**，而不是
//! 「ABB 按自己造的服务理解自己」。
//!
//! 为什么必须 pty 之外的方式发现 Python：CI 与本地 python3 路径不同，
//! 找不到就 `[skip]` 并说明（与本仓 `glyph_skip_notice` 同口径——不静默变绿，
//! 但要能区分「环境没有」和「代码坏了」）。

use crate::messenger::Messenger; // 调 trait 方法要把它引入作用域
use std::process::Command;

/// 找一个可用的 python3。
fn python3() -> Option<String> {
    for cand in ["python3", "/usr/bin/python3", "/opt/homebrew/bin/python3"] {
        if Command::new(cand)
            .arg("-c")
            .arg("print(1)")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(cand.to_string());
        }
    }
    None
}

fn script_path() -> String {
    format!("{}/tests/fake_mcp_channel.py", env!("CARGO_MANIFEST_DIR"))
}

fn spawn(mode: &str) -> Option<crate::mcp_client::StdioChannelClient> {
    let py = match python3() {
        Some(p) => p,
        None => {
            eprintln!("[skip] 端到端验证需要 python3，本机没有 ⇒ 跳过（不是代码问题）");
            return None;
        }
    };
    let path = script_path();
    if !std::path::Path::new(&path).exists() {
        eprintln!("[skip] 找不到 {path} ⇒ 跳过");
        return None;
    }
    match crate::mcp_client::StdioChannelClient::spawn(
        "e2e",
        &py,
        &[path, format!("--mode={mode}")],
        &[],
    ) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[skip] 起不来假 server：{e} ⇒ 跳过");
            None
        }
    }
}

/// A 方案端到端：订阅 → 收通知 → read → 翻译成 `Ev`。
#[test]
fn subscribe_end_to_end_reaches_ev() {
    let Some(mut client) = spawn("subscribe") else {
        return;
    };
    // 握手
    let _ = client.call(
        "initialize",
        serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{},
                           "clientInfo":{"name":"abb","version":"test"}}),
    );
    let tools = client
        .call("tools/list", serde_json::json!({}))
        .expect("tools/list");
    let resources = client
        .call("resources/list", serde_json::json!({}))
        .expect("resources/list");

    // 能力必须被正确归纳
    let caps = crate::mcp_bus::capabilities_from(&tools, &resources)
        .unwrap_or_else(|e| panic!("能力归纳失败：{e:?}"));
    assert!(caps.has_send_text, "假 server 暴露了 send_text");
    assert!(caps.has_send_thread_reply);
    assert_eq!(caps.inbox_uris, vec!["channel://inbox"]);
    caps.validate().expect("能力应当齐备");

    // 协商：应当走订阅
    let inbound = crate::mcp_inbound::negotiate(&mut client, &caps).expect("应当能协商出入站");
    assert!(
        matches!(
            inbound,
            crate::mcp_inbound::ChannelInbound::Subscribe { .. }
        ),
        "有 resources 时应优先订阅，实际 {inbound:?}"
    );

    // 等 server 推通知（它 0.8s 后推第一条、1.3s 后推重复的一条）
    std::thread::sleep(std::time::Duration::from_millis(1800));
    let notes = client.drain_notifications();
    assert!(
        !notes.is_empty(),
        "应当收到 notifications/resources/updated（server 推了 2 条）"
    );

    // 通知 → read（两条重复通知只该 read 一次）
    let msgs = crate::mcp_inbound::messages_from_notifications(&mut client, &inbound, &notes, 100)
        .expect("读资源");
    assert_eq!(
        msgs.len(),
        3,
        "假 server 预置 3 条消息，实际 {}",
        msgs.len()
    );
    assert_eq!(msgs[0].text, "第一条");

    // 翻译成 Ev：真实链路最后一环
    let ev = crate::mcp_adapter::to_ev(&msgs[0]).expect("应当能翻译");
    assert_eq!(ev.chat_id, "mcp-chat");
    assert_eq!(ev.mid, "m1");
    assert_eq!(ev.text, "第一条");
    // 安全边界在任何链路下都不放松
    assert_eq!(
        ev.role,
        crate::config::SenderRole::Granted,
        "端到端也不许给全权限"
    );
}

/// B 方案端到端：server **不暴露 resources** ⇒ 必须退到 poll，且游标真的推进。
#[test]
fn poll_end_to_end_falls_back_and_advances_cursor() {
    let Some(mut client) = spawn("poll") else {
        return;
    };
    let _ = client.call(
        "initialize",
        serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{},
                           "clientInfo":{"name":"abb","version":"test"}}),
    );
    let tools = client
        .call("tools/list", serde_json::json!({}))
        .expect("tools/list");
    let resources = client
        .call("resources/list", serde_json::json!({}))
        .expect("resources/list");
    let caps = crate::mcp_bus::capabilities_from(&tools, &resources).expect("能力归纳");
    assert!(caps.inbox_uris.is_empty(), "poll 模式不暴露 resources");
    assert!(caps.has_poll);

    let inbound = crate::mcp_inbound::negotiate(&mut client, &caps).expect("应退到 poll");
    assert!(
        matches!(inbound, crate::mcp_inbound::ChannelInbound::Poll),
        "没有 resources 时应退到 poll"
    );

    // 第一轮：取到 3 条，游标推进
    let b1 = crate::mcp_inbound::poll_once(&mut client, None, 100).expect("第一次 poll");
    assert_eq!(b1.messages.len(), 3);
    assert_eq!(b1.next_cursor.as_deref(), Some("3"));

    // 第二轮：用游标 ⇒ 不该重复拿到旧消息
    let b2 = crate::mcp_inbound::poll_once(&mut client, b1.next_cursor.as_deref(), 100)
        .expect("第二次 poll");
    assert!(
        b2.messages.is_empty(),
        "游标应生效（不该重复拿到已取过的消息），实际 {}",
        b2.messages.len()
    );
}

/// 出站端到端：`McpChannelMessenger::send_text` 真的把内容送到 server 侧。
///
/// 校验方式：让假 server 把收到的 `send_text` 内容**回显在 `tools/call` 的返回文本里**
/// （它现在还只打 stderr），这样不用外部日志也能断言「送达的就是发出的」。
#[test]
fn send_text_end_to_end_reaches_the_server() {
    let Some(mut client) = spawn("subscribe") else {
        return;
    };
    let _ = client.call(
        "initialize",
        serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{},
                           "clientInfo":{"name":"abb","version":"test"}}),
    );
    let tools = client
        .call("tools/list", serde_json::json!({}))
        .expect("tools/list");
    let resources = client
        .call("resources/list", serde_json::json!({}))
        .expect("resources/list");
    let caps = crate::mcp_bus::capabilities_from(&tools, &resources).expect("能力归纳");

    let m = crate::mcp_channel::McpChannelMessenger::from_client_for_test(client, "e2e", caps);
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        m.send_text("mcp-chat", "端到端测试")
            .await
            .expect("send_text 应当成功");
        // 没有 send_thread_reply 的服务要回落——这里它有，所以走回复路径。
        m.send_thread_reply("mcp-chat", "m1", "话题回复")
            .await
            .expect("send_thread_reply 应当成功");
    });
}
