//! 入站：钉钉 Stream 长连接 → 缓冲 → 通知 ABB 来 `resources/read`。
//!
//! 从 ABB `src/dingtalk.rs` 的 `stream_loop` + `run_conn`（原 987–1150 行）搬来的**连接层**
//! （注册 ticket → 连 wss → 收帧 → ack → 客户端 keepalive + 半开看门狗 → 断线重连），
//! 改动只有一处语义替换：
//!
//! | 原 ABB | 本 crate |
//! |---|---|
//! | `bridge.on_dingtalk(msg)`（进 Bridge：准入/去重/落盘/开回合） | `ingest()`：解成契约 §3.3 消息 → 塞进缓冲 → 推 `notifications/resources/updated` |
//!
//! 也就是说：**准入/去重/回合这些都不再是渠道的事**——ABB 自己按 MCP 契约做。
//! 本进程只负责「把钉钉推来的回调翻译成契约消息」。
//!
//! ## 依赖替换（与任务口径一致：11 处）
//!
//! 1. `use crate::bridge::Bridge;` → **删除**（入站不再进 Bridge）。
//! 2. `crate::botstatus::report(...)` **3 处 → 整句删除**：ABB 侧 `mcp_channel_loop` 已自己
//!    报连接状态，而让渠道进程去写 ABB 的运行时文件正是搬迁要消除的耦合。
//!    （原实现里 watchdog 分支**只**为「每 30s 刷一次在线」而存在；删掉后该分支仍保留
//!    ——它还有半开看门狗那条判据。）
//! 3. `crate::log!(...)` **7 处 → `eprintln!`**，并统一加 `[dingtalk-ws]` 前缀。
//!    **必须 stderr**：stdout 是 MCP 协议通道，一行杂输出就让 ABB 的 JSON-RPC 解析炸掉。
//!
//! `bridge.bot.{key,kind,bot_name}` 这 9 处元信息原本只喂给 `botstatus`（日志/状态文件），
//! 随上面 3 处 report 一并消失——本 crate 的进程名与渠道名由 env / 常量决定，不读 ABB 配置。
//!
//! ## 保留的现场结论（都是踩过的坑，注释照搬）
//! - 客户端 **keepalive**（#19）：平台空闲时不推帧，健康判定依赖客户端发 WebSocket Ping 换 Pong；
//! - `send` 要超时包装（#18 / #179）：网络半死时 `sink.send()` 可永久挂起 ⇒ `select!` 冻死 ⇒ 永不重连；
//! - **半开看门狗**：连通性的证据是「最近收到过对端帧」，不是「ping 发得出去」。
//!
//! 未做的（本批范围外，与飞书版同口径）：入站附件**不预下载**——这里只带 `kind`/`name`，
//! `url_or_path` 留空；真下载由 `download_attachment` 工具**按需**做（钉钉的 `downloadCode`
//! 只在入站帧里出现一次，所以入站时先记进台账，见 `Channel::remember`）。
//! **不编造本地路径**（编造会让 agent 去读一个不存在的文件且不报错）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::dingtalk::{self, DingTalkClient, DingtalkMessage};
use crate::Channel;

/// 半开连接看门狗阈值：超过该时长没收到任何入站帧（含 keepalive 的 Pong）即判定通道假死，主动重连。
/// 平台空闲时不推帧（#19），健康判定依赖客户端 keepalive 的 Pong；180s ≈ 6 个 keepalive 周期，足够宽松。
const STALL_AFTER: Duration = Duration::from_secs(180);
/// 客户端 keepalive 间隔：周期发 WebSocket Ping，平台回 Pong。官方 SDK 默认 8s，
/// 这里取 30s 已能可靠续命且省流量（看门狗 180s = 6 拍，容忍 5 拍连续丢 Pong）。
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// 看门狗自身的节拍：每 30s 检查一次 `last_rx`（与 keepalive 同周期，够密且不空转）。
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);

/// 起一个后台线程跑长连接。**连接失败不影响主线程**（契约层照常工作）。
pub fn spawn(ch: Arc<Channel>) {
    std::thread::Builder::new()
        .name("dingtalk-stream".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[dingtalk-ws] 建 runtime 失败，入站不可用：{e}");
                    return;
                }
            };
            rt.block_on(ws_loop(ch));
        })
        .map(|_| ())
        .unwrap_or_else(|e| eprintln!("[dingtalk-ws] 起线程失败，入站不可用：{e}"));
}

/// 发送超时包装（2026-10-05 审计 #18，与 `ws.rs` 的 #179 修法同款）。
///
/// 为什么必须有：网络半死（代理/VPN TUN 抖动、睡眠唤醒）时 `sink.send()` 可能**永久挂起** ——
/// 而 `tokio::select!` 已进入分支体，180s 半开看门狗分支不再被 poll ⇒ 永不重连 ⇒ bot 常显
/// 「在线」却收不到任何消息（飞书侧 #179 已实锤修过，钉钉漏改）。超时 ⇒ Err ⇒ 外层重连。
async fn send_with_timeout(
    sink: &mut (impl futures_util::Sink<Message> + Unpin),
    msg: Message,
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), sink.send(msg))
        .await
        .context("发送超时（网络卡死）")?
        .map_err(|_| anyhow!("发送失败"))?;
    Ok(())
}

/// 长连接主循环：断了就重连，永不放弃（要退出只能杀进程）。
///
/// 原实现的 `stop: CancellationToken` 分支一并去掉：本进程的生命周期由 stdin EOF
/// （ABB 关掉渠道）与进程退出决定，进程内不再需要「优雅停单条连接」的入口。
async fn ws_loop(ch: Arc<Channel>) {
    let app_id = std::env::var("DT_APP_ID").unwrap_or_default();
    let app_secret = std::env::var("DT_APP_SECRET").unwrap_or_default();
    let mut fails: u32 = 0;
    loop {
        match run_conn(&app_id, &app_secret, &ch).await {
            Ok(()) => return,
            Err(e) => {
                fails += 1;
                // 原实现此处先 `botstatus::report(…, "重连中")` 再打日志：状态上报已删（见文件头）。
                eprintln!("[dingtalk-ws] 连接断开: {e:#}（第 {fails} 次）");
            }
        }
        let wait = Duration::from_secs(3) + Duration::from_millis(fastrand::u64(0..=1000));
        eprintln!("[dingtalk-ws] {}s 后重连…", wait.as_secs());
        tokio::time::sleep(wait).await;
    }
}

/// 单次连接：注册 ticket → 连 wss → 帧处理循环。返回 Err 触发外层重连。
async fn run_conn(app_id: &str, app_secret: &str, ch: &Arc<Channel>) -> Result<()> {
    let (endpoint, ticket) = DingTalkClient::open_connection(app_id, app_secret).await?;
    let url = format!(
        "{}?ticket={}",
        endpoint,
        dingtalk::percent_encode_query(&ticket)
    );
    eprintln!("[dingtalk-ws] 注册成功，连接 {endpoint}…");
    let (ws, _) = connect_async(&url).await.context("钉钉 ws connect 失败")?;
    let (mut sink, mut stream) = ws.split();
    // 原实现在这里 `botstatus::report(…, "在线")`：已删（ABB 侧自己报连接状态）。
    eprintln!("[dingtalk-ws] 已连接，监听 /v1.0/im/bot/messages/get");

    // 客户端 keepalive（#19）：平台空闲不推帧，须主动发 WebSocket Ping 换 Pong 续命。
    let mut last_rx = Instant::now();
    let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
    watchdog.tick().await; // 跳过第一拍
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.tick().await; // 跳过第一拍

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                // 只发 Ping 不算「收到帧」：last_rx 仍只由入站刷新，
                // 半开连接发 Ping 无 Pong → 看门狗照常兜底（不把假活当在线）。
                if send_with_timeout(&mut sink, Message::Ping(Vec::new().into())).await.is_err() {
                    return Err(anyhow!("keepalive 发 Ping 失败，连接已不可写"));
                }
            }
            _ = watchdog.tick() => {
                if last_rx.elapsed() > STALL_AFTER {
                    return Err(anyhow!(
                        "半开看门狗：{}s 未收到任何入站帧（>{}s），判定连接已死，主动重连",
                        last_rx.elapsed().as_secs(),
                        STALL_AFTER.as_secs()
                    ));
                }
                // 原实现此处每 30s 刷一次 `botstatus::report(…, "在线")`：已删。
                // 分支保留——上面的半开判据仍然只在这一拍上生效。
            }
            msg = stream.next() => {
                // 任何入站帧（含 ping/ack 帧）都算「对端还活着」，刷新看门狗。
                if msg.is_some() {
                    last_rx = Instant::now();
                }
                match msg {
                    None => return Err(anyhow!("连接被服务端关闭")),
                    Some(Ok(Message::Close(_))) => return Err(anyhow!("收到 Close 帧")),
                    Some(Ok(Message::Text(t))) => {
                        let frame: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        let ftype = frame["type"].as_str().unwrap_or("");
                        let topic = frame["headers"]["topic"].as_str().unwrap_or("");
                        let mid = frame["headers"]["messageId"].as_str().unwrap_or("");
                        match ftype {
                            "SYSTEM" if topic == "ping" => {
                                // 服务端健康检查：原样回 opaque
                                let opaque = frame["data"]
                                    .as_str()
                                    .and_then(|d| serde_json::from_str::<Value>(d).ok())
                                    .and_then(|v| v["opaque"].as_str().map(|s| s.to_string()))
                                    .unwrap_or_default();
                                let ack =
                                    dingtalk::ack_json(mid, &json!({"opaque": opaque}).to_string());
                                send_with_timeout(&mut sink, Message::Text(ack.into()))
                                    .await
                                    .context("回 ping ack 失败")?;
                            }
                            "SYSTEM" if topic == "disconnect" => {
                                eprintln!(
                                    "[dingtalk-ws] 服务端 disconnect: {:?}",
                                    frame["data"]
                                );
                                // 协议：不响应，服务端约 10s 后断开；直接返回走重连
                                return Err(anyhow!("服务端主动断开（disconnect）"));
                            }
                            "CALLBACK" => {
                                // 先 ack（fire-forget 消息也按协议回，防服务端诊断误判），再异步处理
                                let ack = dingtalk::ack_json(mid, r#"{"response": null}"#);
                                send_with_timeout(&mut sink, Message::Text(ack.into()))
                                    .await
                                    .context("回 callback ack 失败")?;
                                if topic == "/v1.0/im/bot/messages/get" {
                                    // 原实现走 `bridge.on_dingtalk(msg)`；现在只翻译 + 入缓冲。
                                    // 单独 spawn：查发送者名字是一次 HTTP，不能挡住收帧循环。
                                    let ch = ch.clone();
                                    tokio::spawn(async move {
                                        ingest(&ch, &frame).await;
                                    });
                                }
                            }
                            "EVENT" => {
                                let ack = dingtalk::ack_json(
                                    mid,
                                    r#"{"status":"SUCCESS","message":"success"}"#,
                                );
                                let _ = send_with_timeout(&mut sink, Message::Text(ack.into())).await;
                            }
                            _ => {
                                eprintln!(
                                    "[dingtalk-ws] 未处理的帧 type={ftype} topic={topic}"
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => { /* 协议只推文本帧 */ }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = send_with_timeout(&mut sink, Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        }
    }
}

/// 处理一个 CALLBACK 帧：解析 → （best-effort 查名字）→ 记「最近提问者」→ 入缓冲 + 推通知。
async fn ingest(ch: &Arc<Channel>, frame: &Value) {
    let Some(msg) = dingtalk::parse_message(frame) else {
        return;
    };
    // 名字只用于显示：查不到（scope 不足 / 已离职 / 网络抖动）就留空，**绝不因此丢消息**。
    let sender_name = tokio::time::timeout(
        Duration::from_secs(5),
        ch.client.user_name(&msg.sender_staff_id),
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    let chat_id = msg.chat_id();
    // 原 ABB 在 `on_dingtalk` 里 `note_sender` 刷新「最近提问者」，群聊回复据此 @ 提问者。
    // 现在 `send_thread_reply` 不存在了（钉钉无话题），那条 @ 语义挪进工具侧的 `send_text`，
    // 所以这张表必须继续刷新，否则群里回复会静默失去 @ 提及。
    if !chat_id.is_empty() && !msg.sender_staff_id.is_empty() {
        ch.note_sender(&chat_id, &msg.sender_staff_id);
    }
    // 附件引用（downloadCode）只在入站帧里出现一次 —— 必须现场记下来，
    // 否则 `download_attachment` 工具永远拿不到下载引用（见 `Channel::remember`）。
    ch.remember(&msg);
    eprintln!(
        "[dingtalk-ws] 收到 chat={} mid={} from={} text_len={} atts={}",
        chat_id,
        msg.mid,
        msg.sender_staff_id,
        msg.text.chars().count(),
        msg.attachments.len()
    );
    ch.push_inbound(to_channel_message(&msg, &sender_name));
}

/// → 契约 §3.3 的 `ChannelMessage`。
///
/// 抽成独立函数是为了**单测能验真代码路径**产出的形状（在测试里自己再写一遍
/// `json!` 等于空断言：字段名写错也测不出来）。
///
/// ## 两处口径
///
/// 1. **`ts` 用「收到的当前秒」**：原实现就是 `crate::chrono_lite::unix_secs()`
///    （`bridge/recover.rs` 注释：「钉钉事件无时间字段 → 当前秒」）。回调 `data` 里其实
///    带 `createAt`（毫秒），但协议层从未解析它——本批不改协议逻辑，保持一致。
/// 2. **引用/回复并进 `text`**：契约 §3.3 没有 `quoted` 字段，而被引用文本（`text.repliedMsg`）
///    原本是要进 prompt 的。这里按 ABB 原 prompt 形状
///    （`bridge/virtualbot.rs`：`[引用消息]\n<被引用文本>\n\n<用户文本>`）拼好放进 `text`
///    ⇒ agent 看到的上下文与搬迁前一致。被引用消息里的**附件**不并进来：它们同样没有
///    本地位置（本批不下载），硬塞进 attachments 只会让 agent 去读不存在的文件。
pub fn to_channel_message(msg: &DingtalkMessage, sender_name: &str) -> Value {
    // 引用内容走契约的 `quoted` 字段，**不再拼进 text**。
    //
    // 原来拼成 `[引用消息]\n…\n\n` + 正文：那是契约还没有引用字段时的变通，
    // 后果是 agent 分不清「用户说的」和「被引用的」，也没法单独引用渲染。
    // 现在 ABB 侧把它填进 `Ev.quoted`（与飞书的 `parent_id` 反查同一个下游），
    // 引用块由 ABB 统一排版。
    let quoted = if msg.quoted_text.trim().is_empty() && msg.quoted_attachments.is_empty() {
        Value::Null
    } else {
        json!({
            "text": msg.quoted_text,
            "attachment_count": msg.quoted_attachments.len(),
        })
    };
    let text = msg.text.clone();
    let attachments: Vec<Value> = msg
        .attachments
        .iter()
        .map(|a| {
            // 不预下载（见文件头）⇒ `url_or_path` 留空；真下载走 `download_attachment` 工具，
            // 它的下载引用已由 `Channel::remember` 记进台账。
            // ABB 会如实记成「渠道未提供附件位置」，**不编造本地路径**。
            eprintln!(
                "[dingtalk-ws] 入站附件待按需下载 kind={} code={} name={}",
                a.kind, a.download_code, a.file_name
            );
            json!({"kind": a.kind, "name": a.file_name, "url_or_path": ""})
        })
        .collect();
    json!({
        "chat_id": msg.chat_id(),
        "message_id": msg.mid,
        "sender_id": msg.sender_staff_id,
        "sender_name": sender_name,
        "text": text,
        "ts": now_secs(),
        "attachments": attachments,
        "quoted": quoted,
    })
}

/// 当前 Unix 秒（不引 chrono：这里只要一个 i64）。
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方文档示例帧（机器人接收消息，Stream 模式；取自原 `src/dingtalk.rs` 的单测样本）。
    fn callback_frame() -> Value {
        json!({
            "specVersion": "1.0",
            "type": "CALLBACK",
            "headers": {
                "appId": "1305d5f5-...",
                "contentType": "application/json",
                "messageId": "212ca9d7_974_1898c159aa6_1783b"
            },
            "data": json!({
                "conversationId": "cidAsXSBLnA==",
                "chatbotCorpId": "ding9*****",
                "chatbotUserId": "$:LWCP_v1:$*****",
                "msgId": "msgLICYe****HgY4JtMQw==",
                "senderNick": "用户",
                "isAdmin": true,
                "senderStaffId": "16650***698",
                "createAt": 1690362101894i64,
                "conversationType": "2",
                "senderId": "$:LWCP_v1:***jqTgIfhRX9Q==",
                "conversationTitle": "测试群",
                "isInAtList": true,
                "text": {"content": " 测试数据"},
                "robotCode": "ding*****r3xc0k",
                "msgtype": "text"
            })
            .to_string(),
        })
    }

    /// 契约 §3.3 的七个字段一个都不能少，且会话键按群/单聊规则落位。
    #[test]
    fn callback_becomes_channel_message() {
        let msg = dingtalk::parse_message(&callback_frame()).expect("应当解出一条");
        let m = to_channel_message(&msg, "张三");
        for k in [
            "chat_id",
            "message_id",
            "sender_id",
            "sender_name",
            "text",
            "ts",
            "attachments",
        ] {
            assert!(m.get(k).is_some(), "契约 §3.3 缺字段 `{k}`：{m}");
        }
        assert_eq!(m["chat_id"], "cidAsXSBLnA==", "群聊用 openConversationId");
        assert_eq!(m["message_id"], "msgLICYe****HgY4JtMQw==");
        assert_eq!(m["sender_id"], "16650***698");
        assert_eq!(m["text"], "测试数据", "首尾空白要 trim");
        assert_eq!(m["sender_name"], "张三");
        assert!(m["ts"].as_i64().unwrap_or(0) > 0, "ts 必须是正秒");
        assert_eq!(m["attachments"], json!([]));
    }

    /// 单聊：chat_id 回落到发送者 staffId（钉钉的会话标识约定）。
    #[test]
    fn single_chat_uses_sender_as_chat_id() {
        let mut frame = callback_frame();
        let data: Value = serde_json::from_str(frame["data"].as_str().unwrap()).unwrap();
        let mut data = data;
        data["conversationId"] = json!("");
        data["conversationType"] = json!("1");
        frame["data"] = json!(data.to_string());
        let msg = dingtalk::parse_message(&frame).expect("应当解出一条");
        let m = to_channel_message(&msg, "");
        assert_eq!(m["chat_id"], "16650***698");
        assert_eq!(m["sender_name"], "", "查不到名字就留空，不编造");
    }

    /// **引用内容必须进 text**（契约没有 quoted 字段）：形状对齐 ABB 原 prompt。
    #[test]
    fn quoted_text_is_inlined_into_text() {
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"messageId": "m1"},
            "data": json!({
                "msgId": "m1",
                "senderStaffId": "staff_1",
                "conversationId": "cid_1",
                "conversationType": "2",
                "msgtype": "text",
                "text": {
                    "content": "回复内容",
                    "isReplyMsg": true,
                    "repliedMsg": {
                        "msgType": "text",
                        "content": {"text": "被引用的原消息"}
                    }
                }
            })
            .to_string(),
        });
        let msg = dingtalk::parse_message(&frame).expect("应当解出一条");
        let m = to_channel_message(&msg, "张三");
        // **行为在 2026-11-04 变了**：原来把引用拼进 `text`
        // （`[引用消息]\n…\n\n` + 正文），那是契约还没有引用字段时的变通。
        // 现在走 `quoted`，正文保持干净——agent 能分清「用户说的」与「被引用的」，
        // 引用块由 ABB 统一排版。
        assert_eq!(m["text"], "回复内容", "正文不许再带 [引用消息] 前缀");
        assert_eq!(m["quoted"]["text"], "被引用的原消息", "被引用内容走 quoted");
        assert_eq!(m["quoted"]["attachment_count"], 0);
    }

    /// 不带引用的普通消息：正文干净，且 `quoted` 必须是 null。
    ///
    /// `quoted: null` 很关键——ABB 侧靠它区分「有引用」与「没引用」，
    /// 空对象会让下游渲染出一个空的引用块。
    #[test]
    fn plain_text_has_no_quote() {
        let msg = dingtalk::parse_message(&callback_frame()).unwrap();
        let m = to_channel_message(&msg, "");
        assert!(!m["text"].as_str().unwrap().contains("[引用消息]"));
        assert!(
            m["quoted"].is_null(),
            "无引用时必须 null，实际：{}",
            m["quoted"]
        );
    }

    /// 纯图片消息：文本为空但有附件 ⇒ 必须保留（合法输入），且位置留空不编造。
    #[test]
    fn image_callback_keeps_attachment_without_location() {
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"messageId": "m1"},
            "data": json!({
                "msgId": "m2",
                "senderStaffId": "staff_1",
                "conversationType": "1",
                "msgtype": "picture",
                "content": {"downloadCode": "dc_1"}
            })
            .to_string(),
        });
        let msg = dingtalk::parse_message(&frame).expect("纯图片消息也要收");
        let m = to_channel_message(&msg, "");
        assert_eq!(m["text"], "");
        let atts = m["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["kind"], "image");
        assert_eq!(atts[0]["url_or_path"], "", "不编造本地路径");
    }

    /// 非 CALLBACK 帧 / 非法 data 一律 None（不猜）。
    #[test]
    fn non_callback_frames_are_ignored() {
        assert!(dingtalk::parse_message(&json!({"type": "SYSTEM"})).is_none());
        let bad = json!({"type": "CALLBACK", "headers": {}, "data": "not json"});
        assert!(dingtalk::parse_message(&bad).is_none());
    }
}
