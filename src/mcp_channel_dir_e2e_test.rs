//! 真渠道参考实现的**端到端验证**（目标 2 的可用性证据）。
//!
//! `tests/fake_mcp_channel.py` 是测试桩（预置消息、只回固定字符串）；
//! `tools/channel-mcp-dir/channel_mcp_dir.py` 是**真能用**的渠道——它把目录当渠道，
//! 从 `<dir>/inbox/*.json` 收消息、把回复写进 `<dir>/outbox/*.txt`。
//!
//! 这条测试证明的是**总线可用**，不是"协议实现自洽"：
//!
//! 1. 往 inbox 写一个 json ⇒ ABB 能取到并翻译成 `Ev`
//! 2. `send_text` ⇒ outbox 里出现一个文件，内容与发出的逐字相同
//! 3. 坏消息（非法 json）不阻塞后面的消息
//!
//! 第 3 条尤其重要：渠道实现经常因为一条坏消息整个卡住，表现是"在线但没反应"。

use crate::messenger::Messenger; // 调 trait 方法要引入作用域
use std::process::Command;

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

fn script() -> String {
    format!(
        "{}/tools/channel-mcp-dir/channel_mcp_dir.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// 建一个临时渠道目录（测试结束自动清理）。
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "abb-chan-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).expect("建临时目录");
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 起一个真渠道（`--no-resources` 走 poll，便于测试确定性；订阅路径由另一条用例覆盖）。
fn spawn_dir_channel(
    dir: &std::path::Path,
    no_resources: bool,
) -> Option<crate::mcp_client::StdioChannelClient> {
    let py = match python3() {
        Some(p) => p,
        None => {
            eprintln!("[skip] 需要 python3，本机没有 ⇒ 跳过（不是代码问题）");
            return None;
        }
    };
    let s = script();
    if !std::path::Path::new(&s).exists() {
        eprintln!("[skip] 找不到 {s} ⇒ 跳过");
        return None;
    }
    let mut args = vec![s, "--dir".into(), dir.display().to_string()];
    if no_resources {
        args.push("--no-resources".into());
    }
    match crate::mcp_client::StdioChannelClient::spawn("dir", &py, &args, &[]) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[skip] 起不来渠道：{e} ⇒ 跳过");
            None
        }
    }
}

fn write_msg(dir: &std::path::Path, name: &str, body: &str) {
    let inbox = dir.join("inbox");
    std::fs::create_dir_all(&inbox).expect("建 inbox");
    std::fs::write(inbox.join(name), body).expect("写消息");
}

/// 真渠道能接收：写进 inbox 的消息被取出来，且能翻译成 `Ev`。
#[test]
fn dir_channel_receives_and_translates() {
    let dir = TempDir::new("recv");
    let Some(mut c) = spawn_dir_channel(dir.path(), true) else {
        return;
    };
    let _ = c.call(
        "initialize",
        serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{},
                           "clientInfo":{"name":"abb","version":"test"}}),
    );
    let tools = c
        .call("tools/list", serde_json::json!({}))
        .expect("tools/list");
    let resources = c
        .call("resources/list", serde_json::json!({}))
        .expect("resources/list");
    let caps = crate::mcp_bus::capabilities_from(&tools, &resources).expect("能力归纳");
    // `--no-resources` ⇒ 必须能靠 poll 兜底
    assert!(caps.has_poll, "应当有 poll");
    let inbound = crate::mcp_inbound::negotiate(&mut c, &caps).expect("协商出 poll");
    assert!(matches!(inbound, crate::mcp_inbound::ChannelInbound::Poll));

    write_msg(
        dir.path(),
        "0001.json",
        r#"{"chat_id":"c1","message_id":"m1","sender_id":"u1","text":"来自真渠道","ts":1793600001}"#,
    );

    let batch = crate::mcp_inbound::poll_once(&mut c, None, 100).expect("poll");
    assert_eq!(batch.messages.len(), 1, "应取到 1 条");
    assert_eq!(batch.messages[0].text, "来自真渠道");

    let ev = crate::mcp_adapter::to_ev(&batch.messages[0]).expect("翻译");
    assert_eq!(ev.chat_id, "c1");
    assert_eq!(ev.text, "来自真渠道");
    assert_eq!(
        ev.role,
        crate::config::SenderRole::Granted,
        "真渠道也不许给全权限"
    );
}

/// 真渠道能发送：`send_text` 之后 outbox 里有文件，内容逐字相同。
#[test]
fn dir_channel_sends_to_outbox() {
    let dir = TempDir::new("send");
    let Some(mut c) = spawn_dir_channel(dir.path(), true) else {
        return;
    };
    let _ = c.call(
        "initialize",
        serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{},
                           "clientInfo":{"name":"abb","version":"test"}}),
    );
    let tools = c
        .call("tools/list", serde_json::json!({}))
        .expect("tools/list");
    let resources = c
        .call("resources/list", serde_json::json!({}))
        .expect("resources/list");
    let caps = crate::mcp_bus::capabilities_from(&tools, &resources).expect("能力归纳");
    let m = crate::mcp_channel::McpChannelMessenger::from_client_for_test(c, "dir", caps);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        m.send_text("c1", "回你一句\n带换行").await.expect("发送");
    });
    let _ = m.into_client_for_test(); // 归还并丢弃（Drop 会 kill 子进程）

    let outbox = dir.path().join("outbox");
    let files: Vec<_> = std::fs::read_dir(&outbox)
        .expect("outbox 存在")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1, "应当写出 1 个文件，实际 {}", files.len());
    let body = std::fs::read_to_string(files[0].path()).expect("读 outbox");
    // 第一行是元信息，之后是正文——正文必须**逐字**送达（含换行）
    let (meta, text) = body.split_once('\n').expect("有元信息行");
    let meta: serde_json::Value = serde_json::from_str(meta).expect("元信息是 JSON");
    assert_eq!(meta["chat_id"], "c1");
    assert_eq!(text, "回你一句\n带换行", "正文必须逐字送达");
}

/// **坏消息不阻塞后面的**：渠道实现常见故障是「一条坏消息让整个渠道卡住」，
/// 表现是「在线但没反应」，很难查。这条钉住它。
#[test]
fn dir_channel_bad_message_does_not_block_later_ones() {
    let dir = TempDir::new("bad");
    let Some(mut c) = spawn_dir_channel(dir.path(), true) else {
        return;
    };
    let _ = c.call(
        "initialize",
        serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{},
                           "clientInfo":{"name":"abb","version":"test"}}),
    );

    // 文件名排序在前的是坏消息
    write_msg(dir.path(), "0001.json", "这不是 JSON");
    write_msg(
        dir.path(),
        "0002.json",
        r#"{"chat_id":"c1","message_id":"m2","text":"坏消息之后的好消息"}"#,
    );

    let batch = crate::mcp_inbound::poll_once(&mut c, None, 100).expect("poll 不该整体失败");
    assert_eq!(
        batch.messages.len(),
        1,
        "坏消息应被跳过，好消息应取到；实际 {} 条",
        batch.messages.len()
    );
    assert_eq!(batch.messages[0].text, "坏消息之后的好消息");
}
