//! 入站：飞书长连接（WS）→ 缓冲 → 通知 ABB 来 `resources/read`。
//!
//! 从 ABB `src/ws.rs` 搬来的**连接层**（握手 → 连 wss → 收 Frame → DATA 回 ack →
//! 定时 ping → 断线重连），改动只有一处语义替换：
//!
//! | 原 ABB | 本 crate |
//! |---|---|
//! | `bridge.on_payload(&payload)`（进 Bridge：去重/准入/落盘/开回合） | `ingest()`：解成契约 §3.3 消息 → 塞进缓冲 → 推 `notifications/resources/updated` |
//!
//! 也就是说：**准入/去重/回合这些都不再是渠道的事**——ABB 自己按 MCP 契约做。
//! 本进程只负责「把飞书推来的事件翻译成契约消息」。
//!
//! 保留的现场结论（都是踩过的坑，注释照搬）：
//! - 重连用服务端下发的 `ReconnectInterval` + `ReconnectNonce` 抖动；
//! - `send` 要超时包装（#179：网络半死时 `sink.send()` 可永久挂起）；
//! - **半开看门狗**：连通性的证据是「最近收到过对端帧」，不是「ping 发得出去」。
//!
//! 未做的（本批范围外，见 `src/feishu.rs` 文件头）：入站附件只带 `kind`/`name`，
//! `url_or_path` 留空——真下载要 `download_resource`，属下一批。

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::proto::Frame;
use crate::Channel;

const WS_ENDPOINT: &str = "https://open.feishu.cn/callback/ws/endpoint";

#[derive(Debug, Clone)]
struct WsConf {
    url: String,
    service_id: u32,
    ping_interval: u64,
    reconnect_interval: u64,
    reconnect_nonce: u64,
    reconnect_count: i64, // -1 = 无限
}

impl Default for WsConf {
    fn default() -> Self {
        WsConf {
            url: String::new(),
            service_id: 0,
            ping_interval: 90,
            reconnect_interval: 90,
            reconnect_nonce: 25,
            reconnect_count: -1,
        }
    }
}

/// 起一个后台线程跑长连接。**连接失败不影响主线程**（契约层照常工作）。
pub fn spawn(ch: Arc<Channel>) {
    std::thread::Builder::new()
        .name("feishu-ws".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[feishu-ws] 建 runtime 失败，入站不可用：{e}");
                    return;
                }
            };
            rt.block_on(ws_loop(ch));
        })
        .map(|_| ())
        .unwrap_or_else(|e| eprintln!("[feishu-ws] 起线程失败，入站不可用：{e}"));
}

async fn handshake(app_id: &str, app_secret: &str) -> Result<WsConf> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp: Value = http
        .post(WS_ENDPOINT)
        .header("locale", "zh")
        .json(&json!({"AppID": app_id, "AppSecret": app_secret}))
        .send()
        .await?
        .json()
        .await?;
    let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(anyhow!(
            "ws endpoint 失败: code={} msg={:?}",
            code,
            resp.get("msg")
        ));
    }
    let data = resp.get("data").context("ws endpoint 无 data")?;
    let url = data["URL"]
        .as_str()
        .context("ws endpoint 无 URL")?
        .to_string();
    let cc = &data["ClientConfig"];
    let mut conf = WsConf {
        url: url.clone(),
        ping_interval: cc["PingInterval"].as_u64().unwrap_or(90),
        reconnect_interval: cc["ReconnectInterval"].as_u64().unwrap_or(90),
        reconnect_nonce: cc["ReconnectNonce"].as_u64().unwrap_or(25),
        reconnect_count: cc["ReconnectCount"].as_i64().unwrap_or(-1),
        ..Default::default()
    };
    conf.service_id = parse_query_u32(&url, "service_id").unwrap_or(0);
    Ok(conf)
}

/// 从 URL query 取整型参数（手工解析，不引 url crate）。
fn parse_query_u32(url: &str, key: &str) -> Option<u32> {
    let q = url.split('?').nth(1)?;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return it.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

/// 发送超时包装（#179）：网络半死时 `sink.send()` 可能永久挂起（现场：进程活着、
/// 全线程 parked）。超时返回 Err ⇒ 外层重连；半死本来也该重连。
async fn send_with_timeout(
    sink: &mut (impl futures_util::Sink<Message> + Unpin),
    msg: Message,
) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), sink.send(msg))
        .await
        .context("发送超时（网络卡死）")?
        .map_err(|_| anyhow!("发送失败"))?;
    Ok(())
}

fn ping_frame(service_id: u32) -> Frame {
    Frame {
        seq_id: 0,
        log_id: 0,
        service: service_id,
        method: 0, // CONTROL
        headers: vec![("type".into(), "ping".into())],
        payload: vec![],
    }
}

/// 长连接主循环：断了就重连，永不放弃（要退出只能杀进程）。
async fn ws_loop(ch: Arc<Channel>) {
    let app_id = std::env::var("FS_APP_ID").unwrap_or_default();
    let app_secret = std::env::var("FS_APP_SECRET").unwrap_or_default();
    let mut conf = WsConf::default();
    let mut fails: i64 = 0;
    loop {
        match run_conn(&app_id, &app_secret, &ch, &mut conf).await {
            Ok(()) => return,
            Err(e) => {
                fails += 1;
                eprintln!("[feishu-ws] 断开: {e:#}（第 {fails} 次）");
            }
        }
        let over = conf.reconnect_count >= 0 && fails > conf.reconnect_count;
        let wait = if over {
            std::time::Duration::from_secs(60)
        } else {
            let jitter = if conf.reconnect_nonce > 0 {
                fastrand::u64(0..=conf.reconnect_nonce * 1000)
            } else {
                0
            };
            std::time::Duration::from_secs(conf.reconnect_interval.max(2))
                + std::time::Duration::from_millis(jitter)
        };
        eprintln!("[feishu-ws] {}s 后重连…", wait.as_secs());
        tokio::time::sleep(wait).await;
    }
}

async fn run_conn(
    app_id: &str,
    app_secret: &str,
    ch: &Arc<Channel>,
    conf: &mut WsConf,
) -> Result<()> {
    let new_conf = handshake(app_id, app_secret).await?;
    *conf = new_conf.clone();
    eprintln!("[feishu-ws] 握手成功 ping={}s，连接中…", conf.ping_interval);

    let (ws, _) = connect_async(&new_conf.url)
        .await
        .context("ws connect 失败")?;
    let (mut sink, mut stream) = ws.split();
    eprintln!(
        "[feishu-ws] 已连接，监听 im.message.receive_v1 (service_id={})",
        conf.service_id
    );

    let mut ping_t =
        tokio::time::interval(std::time::Duration::from_secs(conf.ping_interval.max(5)));
    ping_t.tick().await; // 跳过立即触发的第一拍

    // 半开连接看门狗：只有「最近收到过对端的帧」才算连通。半开 TCP（Mac 睡眠/NAT
    // 断流）下 send() 照样成功（数据只进内核重传缓冲），不看收帧就会永久「在线」
    // 却收不到消息。任何入站帧都刷新 last_rx；ping 拍上超 2×ping_interval 没收帧 ⇒
    // 主动断开重连。
    let mut last_rx = std::time::Instant::now();
    let stall_after = std::time::Duration::from_secs(conf.ping_interval.max(5).saturating_mul(2));

    loop {
        tokio::select! {
            _ = ping_t.tick() => {
                if last_rx.elapsed() > stall_after {
                    return Err(anyhow!(
                        "半开看门狗：{}s 未收到任何入站帧（>2×ping {}s），判定连接已死，主动重连",
                        last_rx.elapsed().as_secs(),
                        conf.ping_interval
                    ));
                }
                let pf = ping_frame(conf.service_id);
                send_with_timeout(&mut sink, Message::Binary(pf.encode().into()))
                    .await
                    .context("发 ping 失败")?;
            }
            msg = stream.next() => {
                // 任何入站帧（含 pong/CONTROL）都是「对端还活着」的证据。
                if msg.is_some() {
                    last_rx = std::time::Instant::now();
                }
                match msg {
                    None => return Err(anyhow!("连接被服务端关闭")),
                    Some(Ok(Message::Close(_))) => return Err(anyhow!("收到 Close 帧")),
                    Some(Ok(Message::Binary(b))) => {
                        if let Some(f) = Frame::decode(&b) {
                            if f.method == 1 {
                                // DATA：**先回 ack 再处理**。飞书按 ack 判投递成功，
                                // 不回就重投；先 ack 保证「慢处理」不会变成「重复事件」。
                                let ack = Frame {
                                    seq_id: f.seq_id,
                                    log_id: f.log_id,
                                    service: conf.service_id,
                                    method: 1,
                                    headers: f.headers.clone(),
                                    payload: br#"{"code":200}"#.to_vec(),
                                };
                                send_with_timeout(&mut sink, Message::Binary(ack.encode().into()))
                                    .await
                                    .context("回 ack 失败")?;
                                let ch = ch.clone();
                                let payload = f.payload.clone();
                                // 单独 spawn：解事件要查通讯录（一次 HTTP），不能挡住收帧循环。
                                tokio::spawn(async move {
                                    ingest(&ch, &payload).await;
                                });
                            }
                        }
                    }
                    Some(Err(e)) => return Err(e.into()),
                    _ => {} // Text/Ping/Pong 帧忽略（last_rx 已刷新）
                }
            }
        }
    }
}

/// 一条入站事件的**纯解析结果**（不含 `sender_name`——那个要查通讯录，是 IO）。
#[derive(Debug, Clone, PartialEq)]
pub struct Inbound {
    pub chat_id: String,
    pub message_id: String,
    pub sender_id: String,
    pub text: String,
    pub ts: i64,
    pub attachments: Vec<Value>,
}

/// 事件推来的原始字节 → [`Inbound`]。**不认识的事件一律 `None`**（不猜）。
///
/// 飞书 schema 2.0 的事件形状：
/// ```json
/// { "schema":"2.0",
///   "header": {"event_type":"im.message.receive_v1", ...},
///   "event": { "sender": {"sender_id":{"open_id":"ou_…"}, "sender_type":"user"},
///              "message": {"message_id":"om_…","chat_id":"oc_…","create_time":"169…",
///                          "message_type":"text","content":"{\"text\":\"hi\"}",
///                          "mentions":[{"key":"@_user_1","name":"张三"}]} } }
/// ```
pub fn parse_event(payload: &[u8]) -> Option<Inbound> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    let event_type = v["header"]["event_type"].as_str().unwrap_or("");
    if event_type != "im.message.receive_v1" {
        eprintln!("[feishu-ws] 忽略事件类型 `{event_type}`");
        return None;
    }
    let msg = &v["event"]["message"];
    let chat_id = msg["chat_id"].as_str().unwrap_or("").trim().to_string();
    let message_id = msg["message_id"].as_str().unwrap_or("").trim().to_string();
    if chat_id.is_empty() || message_id.is_empty() {
        eprintln!("[feishu-ws] 事件缺 chat_id/message_id，丢弃");
        return None;
    }
    // 自己/别的应用发的不收（飞书可能把自己的消息也推回来 ⇒ 会自循环）。
    let sender = &v["event"]["sender"];
    let sender_type = sender["sender_type"].as_str().unwrap_or("");
    if sender_type != "user" {
        return None;
    }
    let sender_id = sender["sender_id"]["open_id"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    if sender_id.is_empty() {
        return None;
    }
    let content = msg["content"].as_str().unwrap_or("");
    let parsed = crate::feishu::parse_content(content);
    let mut text = parsed.text;
    // 富文本/文本消息里的 @ 是占位符（`@_user_1`），用 mentions 里的真名替换。
    if let Some(mentions) = msg["mentions"].as_array() {
        for m in mentions {
            let key = m["key"].as_str().unwrap_or("");
            let name = m["name"].as_str().unwrap_or("");
            if !key.is_empty() && !name.is_empty() {
                text = text.replace(key, &format!("@{name}"));
            }
        }
    }
    let attachments: Vec<Value> = parsed
        .resources
        .iter()
        .map(|r| {
            // 真下载（`download_resource`）不在本批范围 ⇒ `url_or_path` 留空，
            // ABB 会如实记成「渠道未提供附件位置」，**不编造本地路径**
            //（编造会让 agent 去读一个不存在的文件且不报错）。
            eprintln!(
                "[feishu-ws] 入站附件未下载 kind={} file_key={} name={}",
                r.kind, r.file_key, r.file_name
            );
            json!({"kind": r.kind, "name": r.file_name, "url_or_path": ""})
        })
        .collect();
    if text.trim().is_empty() && attachments.is_empty() {
        eprintln!("[feishu-ws] 事件既无文本也无附件，丢弃 mid={message_id}");
        return None;
    }
    // create_time 是**毫秒**字符串（schema 2.0），契约要秒。
    let ts = msg["create_time"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| msg["create_time"].as_i64())
        .map(|ms| ms / 1000)
        .unwrap_or(0);
    Some(Inbound {
        chat_id,
        message_id,
        sender_id,
        text,
        ts,
        attachments,
    })
}

/// 处理一个 DATA 帧的 payload：解析 → （best-effort 查名字）→ 入缓冲 + 推通知。
async fn ingest(ch: &Arc<Channel>, payload: &[u8]) {
    let Some(in_) = parse_event(payload) else {
        return;
    };
    // 名字只用于显示：查不到（无通讯录 scope / 已离职 / 网络抖动）就留空，
    // **绝不因此丢消息**。
    let sender_name = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ch.client.user_name(&in_.sender_id),
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    eprintln!(
        "[feishu-ws] 收到 chat={} mid={} from={} text_len={}",
        in_.chat_id,
        in_.message_id,
        in_.sender_id,
        in_.text.chars().count()
    );
    ch.push_inbound(to_channel_message(&in_, &sender_name));
}

/// → 契约 §3.3 的 `ChannelMessage`。
///
/// 抽成独立函数是为了**单测能验真代码路径**产出的形状（在测试里自己再写一遍
/// `json!` 等于空断言：字段名写错也测不出来）。
pub fn to_channel_message(in_: &Inbound, sender_name: &str) -> Value {
    json!({
        "chat_id": in_.chat_id,
        "message_id": in_.message_id,
        "sender_id": in_.sender_id,
        "sender_name": sender_name,
        "text": in_.text,
        "ts": in_.ts,
        "attachments": in_.attachments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(content: &str, msg_type: &str) -> Vec<u8> {
        json!({
            "schema": "2.0",
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {
                    "sender_id": {"open_id": "ou_user"},
                    "sender_type": "user"
                },
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_1",
                    "message_type": msg_type,
                    "create_time": "1793612338123",
                    "content": content,
                    "mentions": [{"key": "@_user_1", "name": "张三"}]
                }
            }
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn text_event_becomes_channel_message() {
        let raw = event(r#"{"text":"@_user_1 你好"}"#, "text");
        let in_ = parse_event(&raw).expect("应当解出一条");
        assert_eq!(in_.chat_id, "oc_1");
        assert_eq!(in_.message_id, "om_1");
        assert_eq!(in_.sender_id, "ou_user");
        assert_eq!(in_.text, "@张三 你好", "@ 占位符要换成真名");
        assert_eq!(in_.ts, 1793612338, "毫秒要换成秒");
        let m = to_channel_message(&in_, "张三");
        // 契约 §3.3 的七个字段一个都不能少
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
        assert_eq!(m["sender_name"], "张三");
    }

    /// 自己的/别的应用发的消息不收（飞书会把 bot 自己的消息也推回来）。
    #[test]
    fn app_sender_is_ignored() {
        let mut v: Value = serde_json::from_slice(&event(r#"{"text":"hi"}"#, "text")).unwrap();
        v["event"]["sender"]["sender_type"] = json!("app");
        let raw = v.to_string().into_bytes();
        assert!(parse_event(&raw).is_none(), "sender_type=app 不该入库");
    }

    /// 图片消息：文本为空但有附件 ⇒ 保留（纯附件消息是合法输入）。
    #[test]
    fn image_event_keeps_attachment() {
        let raw = event(r#"{"image_key":"img_1"}"#, "image");
        let in_ = parse_event(&raw).expect("纯图片消息也要收");
        assert_eq!(in_.text, "");
        assert_eq!(in_.attachments.len(), 1);
        assert_eq!(in_.attachments[0]["kind"], "image");
        // 本批不下载 ⇒ 位置留空，不编造路径（见 ingest 注释）。
        assert_eq!(in_.attachments[0]["url_or_path"], "");
    }

    /// 空文本 + 无附件 ⇒ 丢弃（ABB 侧会报「空消息」，早些丢掉不影响语义）。
    #[test]
    fn empty_event_is_dropped() {
        assert!(parse_event(&event(r#"{"text":""}"#, "text")).is_none());
    }

    /// 别的 event_type / 非 JSON 一律丢，不猜。
    #[test]
    fn other_event_types_are_ignored() {
        let mut v: Value = serde_json::from_slice(&event(r#"{"text":"hi"}"#, "text")).unwrap();
        v["header"]["event_type"] = json!("im.chat.updated_v1");
        assert!(parse_event(&v.to_string().into_bytes()).is_none());
        assert!(parse_event(b"not json").is_none());
    }

    /// 缺 chat_id 或 message_id 的事件丢弃（契约 §3.3 要求两者必填）。
    #[test]
    fn missing_ids_are_dropped() {
        for missing in ["chat_id", "message_id"] {
            let mut v: Value = serde_json::from_slice(&event(r#"{"text":"hi"}"#, "text")).unwrap();
            v["event"]["message"][missing] = json!("");
            assert!(
                parse_event(&v.to_string().into_bytes()).is_none(),
                "缺 {missing} 不该通过"
            );
        }
    }

    /// URL query 解析（service_id 从握手 URL 里取）。
    #[test]
    fn parses_service_id_from_url() {
        let url = "wss://example/callback/ws?device_id=1&service_id=33554678&x=2";
        assert_eq!(parse_query_u32(url, "service_id"), Some(33554678));
        assert_eq!(parse_query_u32(url, "nope"), None);
        assert_eq!(parse_query_u32("no-query", "service_id"), None);
    }
}
