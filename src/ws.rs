//! 飞书长连接 —— 握手 → 连 wss → 收 Frame → DATA 回 ack → 定时 ping → 断线重连。
//! 重连用服务端下发的 ReconnectInterval + ReconnectNonce 抖动（每次重连重新握手）。

use crate::bridge::Bridge;
use crate::proto::Frame;
use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const WS_ENDPOINT: &str = "https://open.feishu.cn/callback/ws/endpoint";

#[derive(Debug, Clone)]
pub struct WsConf {
    pub url: String,
    pub service_id: u32,
    pub ping_interval: u64,
    pub reconnect_interval: u64,
    pub reconnect_nonce: u64,
    pub reconnect_count: i64, // -1 = 无限
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

async fn handshake(app_id: &str, app_secret: &str) -> Result<WsConf> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp: serde_json::Value = http
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

/// #179：发送超时包装——网络半死（代理 TUN 抖动，os error 54/60）时 sink.send() 可能
/// 永久挂起，优雅关闭卡死在该 await（现场：进程活着全线程 parked、flock 不释放）。
/// 超时（5s）返回 Err → 外层重连/退出，不永久卡。超时后 sink 状态不可靠，但网络
/// 半死本就该重连——Err 路径由调用方处理。
async fn send_with_timeout(
    sink: &mut (impl futures_util::Sink<Message> + Unpin),
    msg: Message,
) -> anyhow::Result<()> {
    // 泛型 Sink::Error 不保证 Debug/Display/std Error——统一转 anyhow 报错
    //（错误细节由调用方 context 补充：发 ping 失败/回 ack 失败/关闭连接）
    tokio::time::timeout(Duration::from_secs(5), sink.send(msg))
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

/// 长连接主循环。stop 收到 true 时优雅退出（Ok）；否则出错重连。
pub async fn ws_loop(
    app_id: String,
    app_secret: String,
    bridge: Arc<Bridge>,
    stop: tokio_util::sync::CancellationToken,
) {
    let mut conf = WsConf::default();
    let mut fails: i64 = 0;
    let (key, kind, name) = (
        bridge.bot.key(),
        bridge.bot.kind.clone(),
        bridge.bot.bot_name.clone(),
    );
    loop {
        if stop.is_cancelled() {
            return;
        }
        match run_conn(&app_id, &app_secret, &bridge, &stop, &mut conf).await {
            Ok(()) => return, // 只有 stop 才会 Ok
            Err(e) => {
                fails += 1;
                crate::botstatus::report(&key, &kind, &name, "重连中");
                crate::log!("[ws] 断开: {e:#}（第 {fails} 次）");
            }
        }
        // 重连退避：reconnect_interval + nonce 抖动；超过 reconnect_count 封顶 60s
        let over = conf.reconnect_count >= 0 && fails > conf.reconnect_count;
        let wait = if over {
            Duration::from_secs(60)
        } else {
            let jitter = if conf.reconnect_nonce > 0 {
                fastrand::u64(0..=conf.reconnect_nonce * 1000)
            } else {
                0
            };
            Duration::from_secs(conf.reconnect_interval.max(2)) + Duration::from_millis(jitter)
        };
        crate::log!("[ws] {}s 后重连…", wait.as_secs());
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.cancelled() => return,
        }
    }
}

async fn run_conn(
    app_id: &str,
    app_secret: &str,
    bridge: &Arc<Bridge>,
    stop: &tokio_util::sync::CancellationToken,
    conf: &mut WsConf,
) -> Result<()> {
    let new_conf = handshake(app_id, app_secret).await?;
    *conf = new_conf.clone();
    crate::log!("[ws] 握手成功 ping={}s，连接中…", conf.ping_interval);

    let (ws, _) = connect_async(&new_conf.url)
        .await
        .context("ws connect 失败")?;
    let (mut sink, mut stream) = ws.split();
    crate::log!(
        "[ws] 已连接，监听 im.message.receive_v1 (service_id={})",
        conf.service_id
    );
    crate::botstatus::report(
        &bridge.bot.key(),
        &bridge.bot.kind,
        &bridge.bot.bot_name,
        "在线",
    );

    let mut ping_t = tokio::time::interval(Duration::from_secs(conf.ping_interval.max(5)));
    ping_t.tick().await; // 跳过立即触发的第一拍

    // 半开连接看门狗：连通性的唯一可靠证据是「最近收到过对端的帧」，不是「ping 发得出去」。
    // 半开 TCP（对端无声消失、本端没收到 FIN/RST——Mac 睡眠/NAT 断流典型）下，send() 照样成功
    // （数据只进内核重传缓冲），而 stream.next() 永远阻塞 → 不看收帧就会永久「在线」却收不到消息。
    // 任何入站帧都刷新 last_rx；ping 拍上若超 2×ping_interval 没收帧 → 主动断开重连。
    // （阈值取最长心跳 2 倍，与 botstatus 僵尸阈值同理。）
    let mut last_rx = std::time::Instant::now();
    let stall_after = Duration::from_secs(conf.ping_interval.max(5).saturating_mul(2));

    loop {
        tokio::select! {
            _ = stop.cancelled() => {
                crate::log!("[ws] 收到停止信号，关闭连接");
                let _ = send_with_timeout(&mut sink, Message::Close(None)).await;
                return Ok(());
            }
            _ = ping_t.tick() => {
                // 先查收帧活性，再发 ping：半开时这里返回 Err 触发外层重连，而不是盲目续命。
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
                // 续命「在线」的前提已从「ping 发出去」升级为「刚确认收过帧」（见上面看门狗）：
                // 走到这说明 last_rx 在 2×ping 内，是真实连通，不是半开假绿。
                crate::botstatus::report(
                    &bridge.bot.key(),
                    &bridge.bot.kind,
                    &bridge.bot.bot_name,
                    "在线",
                );
            }
            msg = stream.next() => {
                // 任何入站帧（含 pong/CONTROL）都算「对端还活着」的证据，刷新看门狗。
                if msg.is_some() {
                    last_rx = std::time::Instant::now();
                }
                match msg {
                    None => return Err(anyhow!("连接被服务端关闭")),
                    Some(Ok(Message::Close(_))) => return Err(anyhow!("收到 Close 帧")),
                    Some(Ok(Message::Binary(b))) => {
                        if let Some(f) = Frame::decode(&b) {
                            match f.method {
                                0 => { /* CONTROL: ping/pong 忽略（已在上层刷新 last_rx） */ }
                                1 => {
                                    // DATA：先回 ack {"code":200}，再异步处理
                                    let ack = Frame {
                                        seq_id: f.seq_id,
                                        log_id: f.log_id,
                                        service: conf.service_id,
                                        method: 1,
                                        headers: f.headers.clone(),
                                        payload: br#"{"code":200}"#.to_vec(),
                                    };
                                    // 回 ack 超时包装（#179）：send 网络半死卡住时不得拖死事件循环
                                    send_with_timeout(
                                        &mut sink,
                                        Message::Binary(ack.encode().into()),
                                    )
                                    .await
                                    .context("回 ack 失败")?;
                                    let b = bridge.clone();
                                    let payload = f.payload.clone();
                                    // #69 审计：短/中命、有 owner（bridge chat_lock +
                                    // pending.json 恢复），不登记（见 tasks.rs 登记口径）。
                                    tokio::spawn(async move {
                                        b.on_payload(&payload).await;
                                    });
                                }
                                _ => {}
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
