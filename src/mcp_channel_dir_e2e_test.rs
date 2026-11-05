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

    let ev = crate::mcp_adapter::to_ev(&batch.messages[0], &[]).expect("翻译");
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

// ── 真渠道：微信（crates/abb-channel 的 --transport wechat）──────────

/// 渠道二进制路径（三渠道共用一个 `abb-channel`）。
///
/// ## 为什么不能硬编码（评审 a0e0ff0 的 non-blocking，属实）
///
/// 第一版写死了作者本机的绝对路径。
/// 那在作者本机能跑，但：
/// - 别人机器上没有这个路径 ⇒ 永远走 `[skip]`
/// - CI 上更是必然 skip（`ci.yml` 不构建这个 crate）
///
/// 于是这条 e2e **看起来在门禁里、实际只在一个人机器上真跑过**——
/// 又一种「看着绿、其实没检查」。
///
/// ## 现在的寻址顺序
///
/// 1. `ABB_WECHAT_BIN` 环境变量（`tools/gates.sh` 在跑微信 crate 那组时**会传**）
/// 2. `target/debug/abb-channel`（同一次 `cargo build` 的常规产物路径，
///    用 `CARGO_TARGET_DIR` 或默认 `target`）
///
/// 都找不到就 `[skip]` **并打印找过哪些路径**——"跳过"必须能看出是环境问题，
/// 而不是静默当成通过。
/// 渠道自己的参数 + `--transport <name>`。
///
/// 合并成一个二进制之后，三条 e2e 起的是**同一个** `abb-channel`，靠这个参数分派。
/// 顺序不重要（`src/main.rs` 会全参数扫描），但统一放在最前面更容易在日志里读出来。
fn transport_args(transport: &str, dir: String) -> Vec<String> {
    vec![
        "--transport".to_string(),
        transport.to_string(),
        "--dir".to_string(),
        dir,
    ]
}

fn channel_bin(transport: &str) -> Option<String> {
    let mut tried: Vec<String> = Vec::new();

    // ① 显式指定（CI / gates.sh 用这条）。三个渠道共用**同一个二进制**，
    //    靠 `--transport` 分派，所以只有一个环境变量。
    if let Ok(p) = std::env::var("ABB_CHANNEL_BIN") {
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
        tried.push(format!("ABB_CHANNEL_BIN={p}（不存在）"));
    }

    // ② 常规构建产物（用 `CARGO_TARGET_DIR` 或默认 `target`）
    if let Ok(td) = std::env::var("CARGO_TARGET_DIR") {
        let p = format!("{td}/debug/abb-channel");
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
        tried.push(p);
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let p = format!("{manifest}/target/debug/abb-channel");
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
        tried.push(p);
    }

    eprintln!(
        "[skip] 没找到 abb-channel 构建产物 ⇒ {transport} 这条 e2e 没跑。找过：\n  {}\n\
         构建方式：cargo build --manifest-path crates/abb-channel/Cargo.toml\n\
         或显式指定：ABB_CHANNEL_BIN=<path> cargo test …",
        tried.join("\n  ")
    );
    None
}

/// **用 ABB 的生产协商代码**接真微信 server：能力归纳 + 入站选路。
///
/// 与 `crates/abb-channel/tests/wechat_contract.rs` 的分工：
/// - 那边验「微信 server 自己符合契约」（它自己发的请求）
/// - 这边验「**ABB 的协商器**能认出它」（走 `build_source` 生产路径）
///
/// 只过前者可能只是"server 自己自洽"；两者都过才说明这条渠道真能接。
#[test]
fn abb_negotiates_real_wechat_channel() {
    let Some(bin) = channel_bin("wechat") else {
        return;
    };
    let dir = TempDir::new("wx");
    // 不提供 WX_TOKEN：握手与能力协商不需要网络（Tools/list 是纯本地应答）。
    let args = transport_args("wechat", dir.path().display().to_string());
    let mut source = crate::mcp_channel_loop::build_source_for_test("wechat", &bin, &args, &[])
        .unwrap_or_else(|e| panic!("ABB 的协商器接不上微信 server：{e}"));

    // 协商出的入站方式必须是 Poll（微信不暴露 channel:// 资源）。
    // `fetch` 在没有 WX_TOKEN 时会失败——但那已经证明**协商成功了**，
    // 我们要的就是"走到 fetch 这一步"而不是"fetch 成功"。
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let err = rt.block_on(async { source.fetch().await }).unwrap_err();
    assert!(
        !err.contains("能力") && !err.contains("既没有"),
        "不该是协商失败（那说明契约不满足）；实际：{err}"
    );
    // 网络/凭证错误是预期的（没 token）。
    assert!(
        err.contains("poll") || err.contains("失败") || err.contains("token"),
        "应当是取消息阶段的错误（说明协商通过了）：{err}"
    );

    // **显式标记本用例真的跑了**（评审 a4040fdf 的 non-blocking）。
    //
    // 为什么需要：判据原来写的是「门禁输出里 `grep -c abb_negotiates_real_wechat ≥ 1`」——
    // 但测试名本身就会出现在输出里（编译、运行、结果三行），**skip 时也成立**。
    // 那是假绿判据：它数的是「这个测试被登记了」，不是「它真的执行到断言」。
    //
    // 这行的作用是：只有在**拿到二进制、真起了子进程、走完协商**之后才会打印。
    // 门禁里 `grep -q 'ABB_WECHAT_E2E_RAN'` 才是有意义的判据。
    eprintln!("ABB_WECHAT_E2E_RAN（已起真子进程并完成协商）");
}

// ── 真渠道：飞书（crates/abb-channel 的 --transport feishu）──────────

/// 飞书 crate 的二进制路径（同一次 `cargo build` 的产物）。
/// **ABB 的生产协商器必须对飞书选 `Subscribe`，不是 `Poll`**。
///
/// 与微信那条正好相反：微信不暴露 `channel://` 资源（只能 poll），
/// 飞书暴露 `channel://inbox`（走订阅）——ABB 的订阅路径**第一次在真渠道上被验证**。
///
/// 判据不只是「能连上」，而是**选路正确**：`build_source` 内部调 `negotiate`，
/// 它对 `resources` 有内容的服务会优先订阅。若错选 poll，fetch 会去打 HTTP
/// 并因凭证无效报错——所以「fetch 返回 Ok 的空批次」本身就证明走了订阅。
#[test]
fn abb_negotiates_feishu_via_subscribe_not_poll() {
    let Some(bin) = channel_bin("feishu") else {
        return;
    };
    let dir = TempDir::new("fs");
    let args = transport_args("feishu", dir.path().display().to_string());
    let envs = vec![
        ("FS_APP_ID".to_string(), "test".to_string()),
        ("FS_APP_SECRET".to_string(), "test".to_string()),
    ];
    let mut source = crate::mcp_channel_loop::build_source_for_test("feishu", &bin, &args, &envs)
        .unwrap_or_else(|e| panic!("ABB 的协商器接不上飞书 server：{e}"));

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let got = rt.block_on(async { source.fetch().await });
    assert!(
        got.is_ok(),
        "订阅路径下无通知应返回空批次（不是错误）；实际：{:?}",
        got.err()
    );
    assert!(got.unwrap().is_empty(), "没有通知时不该有消息");

    eprintln!("ABB_FEISHU_E2E_RAN（订阅路径协商成功，fetch 走的是通知排空）");
}

// ── 真渠道：钉钉（crates/abb-channel 的 --transport dingtalk）────────

/// **ABB 的生产协商器必须对钉钉选 `Subscribe`**（Stream 是推模型，没有 poll 工具）。
///
/// 与飞书同构、与微信相反。三个渠道各有一条这样的 e2e，合起来证明
/// `negotiate` 的选路在**三种真实能力组合**下都对。
#[test]
fn abb_negotiates_dingtalk_via_subscribe() {
    let Some(bin) = channel_bin("dingtalk") else {
        return;
    };
    let dir = TempDir::new("dt");
    let args = transport_args("dingtalk", dir.path().display().to_string());
    let envs = vec![
        ("DT_APP_ID".to_string(), "test".to_string()),
        ("DT_APP_SECRET".to_string(), "test".to_string()),
    ];
    let mut source = crate::mcp_channel_loop::build_source_for_test("dingtalk", &bin, &args, &envs)
        .unwrap_or_else(|e| panic!("ABB 的协商器接不上钉钉 server：{e}"));

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let got = rt.block_on(async { source.fetch().await });
    assert!(
        got.is_ok(),
        "订阅路径下无通知应返回空批次（不是错误）；实际：{:?}",
        got.err()
    );
    assert!(got.unwrap().is_empty(), "没有通知时不该有消息");

    eprintln!("ABB_DINGTALK_E2E_RAN（订阅路径协商成功）");
}

// ── 契约 §3.4 可选能力：真 server 上的调用链 ────────────────────────
//
// 为什么这条必须存在：`mcp_channel.rs` 里 11 个可选能力的单测用的是**手写假
// server**（我用 `printf` 拼 JSON）。假 server 只证明「ABB 解析器能吃我喂的形状」，
// **不证明真 crate 返回那个形状**。
//
// 这条用真 `abb-channel --transport feishu` 走一遍：能力协商 → `has_tool` →
// `call_optional` → 解析。它咬住的是两侧**名字与形状**的对接，而那正是
// 最容易错、且错了不会报错的地方（回落 trait 默认 = 静默降级）。

/// 真飞书 crate 声明了 9 个可选能力 ⇒ ABB 的 `has_tool` 要认得其中每一个。
///
/// 这条**不调工具**（那要真凭证、真网络），只验「能力协商」这一半：
/// `tools/list` 的结果经 `capabilities_from` 后，`has_tool` 对每个名字为真。
///
/// 变异它会红的情况：crate 拼错工具名、ABB 没把 `tool_names` 填进能力、
/// `has_tool` 的比较写错（比如做了 `to_lowercase`）。
#[tokio::test]
async fn real_feishu_server_exposes_optional_capabilities() {
    let Some(bin) = channel_bin("feishu") else {
        return;
    };
    // 空凭证也能握手（`connect` 只做 tools/list + resources/list，不发网络请求）。
    let m = crate::mcp_channel::McpChannelMessenger::connect(
        "feishu-e2e",
        &bin,
        // 合并成一个二进制后必须显式指定渠道（原来是靠二进制文件名区分）。
        &["--transport".to_string(), "feishu".to_string()],
        &[],
    )
    .unwrap_or_else(|e| panic!("连不上真飞书 server（{bin}）：{e:#}"));

    for tool in [
        crate::mcp_bus::TOOL_GET_QUOTED_MESSAGE,
        crate::mcp_bus::TOOL_DOWNLOAD_ATTACHMENT,
        crate::mcp_bus::TOOL_CREATE_CHAT,
        crate::mcp_bus::TOOL_USER_DISPLAY_NAME,
        crate::mcp_bus::TOOL_TYPING,
        crate::mcp_bus::TOOL_DEL_TYPING,
        crate::mcp_bus::TOOL_DONE,
        crate::mcp_bus::TOOL_CHAT_BRIEF,
        crate::mcp_bus::TOOL_GET_CHAT_INFO,
    ] {
        assert!(
            m.capabilities().has_tool(tool),
            "真飞书 server 没暴露 `{tool}`（或 ABB 没把它记进能力）\n\
             实际暴露：{:?}",
            m.capabilities().tool_names
        );
    }

    // 反向：飞书**不该**声明两个跨渠道概念的能力。
    // 声明了却没有实现，比不声明更糟——ABB 会调它然后拿到 error。
    for tool in [
        crate::mcp_bus::TOOL_NOTE_CONTEXT,
        crate::mcp_bus::TOOL_NOTE_SENDER,
    ] {
        assert!(
            !m.capabilities().has_tool(tool),
            "飞书不该声明 `{tool}`（那是微信/钉钉的概念）"
        );
    }
    eprintln!("✅ 真飞书 server 的可选能力协商验过了（{bin}）");
}

/// 调真 server 的 `user_display_name` / `chat_brief` / `get_chat_info`：
/// **空凭证下必然失败**，但必须**失败得干净**（返回 `None`/`Err`，不 panic、不挂住）。
///
/// 这条守的是「可选能力的失败路径」在真进程上成立：
/// 单测里假 server 可以精确控制错误形状，但真 server 的错误是飞书 API 回的
/// （`99991663` 之类），形状完全不同。**只要它不把 ABB 拖死**，回落就是对的。
///
/// 变异它会红的情况：`call_optional` 在真错误上 panic、或 `McpChannelMessenger`
/// 的锁在中途中毒后所有后续调用都失败。
#[tokio::test]
async fn real_feishu_server_optional_calls_fail_softly() {
    let Some(bin) = channel_bin("feishu") else {
        return;
    };
    let m = crate::mcp_channel::McpChannelMessenger::connect(
        "feishu-e2e",
        &bin,
        // 合并成一个二进制后必须显式指定渠道（原来是靠二进制文件名区分）。
        &["--transport".to_string(), "feishu".to_string()],
        &[],
    )
    .unwrap_or_else(|e| panic!("连不上真飞书 server（{bin}）：{e:#}"));

    // 三个都要**在合理时间内**返回（真发 HTTP 会超时，所以这里给宽一点）。
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        (
            m.user_display_name("ou_nonexistent").await,
            m.chat_brief("oc_nonexistent").await,
            m.get_chat_info("oc_nonexistent").await,
        )
    })
    .await
    .expect("真 server 的可选能力调用不能挂住（30s 超时）");

    // 空凭证下拿不到东西是**预期**的；关键是没 panic、没挂住。
    assert!(
        out.0.is_none() && out.1.is_none() && out.2.is_none(),
        "空凭证下应当全部回落 None，实际：{:?}",
        out
    );

    // **关键**：失败没有污染 client —— 后续调用仍然正常。
    // （`call_optional` 若在错误路径上忘了归还锁，这里会永久挂住。）
    let again = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        m.user_display_name("ou_another"),
    )
    .await
    .expect("失败之后 client 必须仍然可用（锁没被吃掉）");
    assert!(again.is_none());
    eprintln!("✅ 真飞书 server 的可选能力失败路径验过了（干净回落，client 未污染）");
}
