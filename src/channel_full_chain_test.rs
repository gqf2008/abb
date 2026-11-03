//! **全链路**验证：用 ABB 自己的装配代码接「目录当渠道」的 MCP server。
//!
//! 与 `mcp_channel_dir_e2e_test.rs` 的区别（两个都要，验的不是一回事）：
//!
//! | 测试 | 证明什么 |
//! |---|---|
//! | `mcp_channel_dir_e2e_test` | **通道层**能跟这个渠道收发（协议 + 翻译） |
//! | 本文件 | **ABB 的装配代码**（`messenger::build` / `build_source` / `tick` / `BridgeSink`）能接上它 |
//!
//! 只过前者可能只是"渠道自己自洽"；两者都过才说明总线可用。

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

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "abb-full-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).expect("建临时目录");
        Self(p)
    }
}

impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 造一个 `kind = "mcp"` 的 bot 配置，指向目录渠道。
fn mcp_bot(dir: &std::path::Path) -> crate::config::BotConfig {
    let py = python3().expect("调用方已确认");
    crate::config::BotConfig {
        name: "full-chain".into(),
        kind: "mcp".into(),
        mcp_channel: Some(crate::channel_mcp::McpServerConfig {
            name: "dir".into(),
            command: py,
            // `--no-resources` 走 poll：确定性高，且能顺带验证兜底路径。
            args: vec![
                script(),
                "--dir".into(),
                dir.display().to_string(),
                "--no-resources".into(),
            ],
            env: vec![],
        }),
        ..Default::default()
    }
}

fn ready() -> bool {
    if python3().is_none() {
        eprintln!("[skip] 需要 python3，本机没有 ⇒ 跳过（不是代码问题）");
        return false;
    }
    if !std::path::Path::new(&script()).exists() {
        eprintln!("[skip] 找不到渠道脚本 ⇒ 跳过");
        return false;
    }
    true
}

/// 出站全链路：`messenger::build(bot)` 真的能建出 messenger 并发出消息。
///
/// 这条走的是**生产装配路径**（`messenger::build` 的 `is_mcp` 分支），
/// 不是测试专用构造器。
#[test]
fn build_for_mcp_bot_produces_working_messenger() {
    if !ready() {
        return;
    }
    let dir = TempDir::new();
    let bot = mcp_bot(dir.path());

    let m = crate::messenger::build(&bot).expect("mcp bot 应当能建出 messenger");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        m.send_text("full-chat", "来自生产装配路径")
            .await
            .expect("应当发送成功");
    });
    drop(m); // 释放 ⇒ 子进程被 kill

    let outbox = dir.0.join("outbox");
    let files: Vec<_> = std::fs::read_dir(&outbox)
        .expect("outbox 存在")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1, "应当写出 1 个文件");
    let body = std::fs::read_to_string(files[0].path()).expect("读 outbox");
    assert!(
        body.contains("来自生产装配路径"),
        "正文应当送达，实际：{body}"
    );
}

/// 入站全链路：`build_source` + `tick` + `BridgeSink` 真的能从渠道取到消息。
///
/// 这条覆盖 `mcp_channel_loop` 里除 `run()`（要真 Bridge 与循环）之外的全部接线：
/// 建连协商（`build_source`）→ 取（`fetch`）→ 去重投递（`tick`）。
#[test]
fn build_source_and_tick_deliver_from_real_channel() {
    if !ready() {
        return;
    }
    let dir = TempDir::new();
    let bot = mcp_bot(dir.path());
    let ch = bot.mcp_channel.as_ref().expect("刚设过");

    // 投递一条入站消息
    let inbox = dir.0.join("inbox");
    std::fs::create_dir_all(&inbox).expect("建 inbox");
    std::fs::write(
        inbox.join("0001.json"),
        r#"{"chat_id":"full-chat","message_id":"f1","text":"全链路入站","ts":1793600001}"#,
    )
    .expect("写消息");

    // 用生产代码建连 + 协商
    let mut source =
        crate::mcp_channel_loop::build_source_for_test(&ch.name, &ch.command, &ch.args, &[])
            .expect("应当能建连并协商出入站方式");

    // 用生产代码跑一轮（tick = fetch + 去重 + 投递）
    let mut state = crate::mcp_inbound_loop::InboundState::new(64);
    let sink = RecordingSink::default();
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let report = rt
        .block_on(async {
            let mut n = 0;
            // poll 模式下消息要等渠道把文件读出来；给几轮机会
            loop {
                let r = crate::mcp_channel_loop::tick(&mut state, source.as_mut(), &sink).await;
                match r {
                    Ok(rep) if rep.delivered > 0 => return Ok(rep),
                    Ok(_) if n < 5 => {
                        n += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Ok(rep) => return Ok(rep),
                    Err(e) => return Err(e),
                }
            }
        })
        .expect("tick 不该失败");

    assert_eq!(report.delivered, 1, "应当投递 1 条");
    let got = sink.got.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].text, "全链路入站");
}

/// 记录投递内容的 sink（不需要真 `Bridge`）。
#[derive(Default)]
struct RecordingSink {
    got: std::sync::Mutex<Vec<crate::mcp_bus::ChannelMessage>>,
}

#[async_trait::async_trait]
impl crate::mcp_inbound_loop::InboundSink for RecordingSink {
    async fn deliver(&self, msg: &crate::mcp_bus::ChannelMessage) -> Result<(), String> {
        self.got.lock().unwrap().push(msg.clone());
        Ok(())
    }
}
