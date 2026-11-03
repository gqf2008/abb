//! 微信渠道的 MCP server 主程序。
//!
//! ## 与 ABB 的关系
//!
//! ABB 是总线，本进程是它拉起的**渠道服务**之一。契约见
//! `docs/mcp-bus-architecture.md` §3：
//!
//! | 方向 | 契约 | 本进程的实现 |
//! |---|---|---|
//! | 出站 | 工具 `send_text`（必需）/ `send_thread_reply`（可选） | 微信无话题概念 ⇒ 只实现 `send_text` |
//! | 入站 | `poll`（兜底）或 `channel://` 资源订阅 | 本进程用 `poll`：微信 `get_updates` 天然是游标长轮询 |
//! | 登录 | —— | 工具 `login`（二维码写到 `--dir`，路径打 stderr） |
//!
//! ## 三个必须做对的地方
//!
//! 1. **stdout 只能出 JSON-RPC**。任何一行杂输出都会让 ABB 侧的解析炸掉，
//!    所以日志一律 `eprintln!`（`weixin.rs` 里的 `aeprintln!` 就是为此）。
//! 2. **`context_token` 自己记**。微信 `send_text` 需要它，而 MCP 契约只有
//!    `{chat_id, text}`。每条入站消息都带 `context_token` ⇒ 入站时记进 map。
//!    这正是原来 ABB 里 `Messenger::note_context` 那个渠道特例的归宿——
//!    **搬出去之后 ABB 就不需要它了**。
//! 3. **游标只在成功后推进**。失败却推进 = 那批消息永久丢失。

mod weixin;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use weixin::{WeixinClient, WeixinMessage};

/// 渠道进程的共享状态。
struct Channel {
    client: WeixinClient,
    /// `chat_id → context_token`。微信发消息要它，契约里没有 ⇒ 本进程记。
    contexts: Mutex<HashMap<String, String>>,
    /// 长轮询游标（`get_updates_buf`）。**只在成功后推进**。
    cursor: Mutex<String>,
    /// 扫码得到的 `wx_user_id`（= ABB 侧的 owner 标识；`chat_id` 就是它）。
    user_id: Mutex<String>,
    /// 二维码与登录态的输出目录（`--dir`）。
    dir: PathBuf,
}

fn main() {
    let dir = parse_dir();
    eprintln!("[wechat-mcp] 启动 dir={}", dir.display());

    // 凭证从环境变量拿（ABB 的 `mcp_channel.env` 下发）——渠道配置由提供方决定，
    // 本进程不认识 ABB 的 config.json。
    let token = std::env::var("WX_TOKEN").unwrap_or_default();
    let base = std::env::var("WX_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| weixin::FIXED_BASE_URL.to_string());
    let cdn = std::env::var("WX_CDN_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| weixin::DEFAULT_CDN_BASE_URL.to_string());
    let user_id = std::env::var("WX_USER_ID").unwrap_or_default();

    let client = WeixinClient::new(&base, &token, &cdn);
    let ch = Arc::new(Channel {
        client,
        contexts: Mutex::new(HashMap::new()),
        cursor: Mutex::new(String::new()),
        user_id: Mutex::new(user_id),
        dir,
    });

    if token.is_empty() {
        eprintln!(
            "[wechat-mcp] 未提供 WX_TOKEN —— 先调 login 工具扫码，\
             再把拿到的 token 写进 ABB 的 mcp_channel.env"
        );
    }

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            eprintln!("[wechat-mcp] 非法 JSON，忽略");
            continue;
        };
        let Some(resp) = handle(&ch, &req) else {
            continue; // 通知不回
        };
        // 一行一条：MCP 是 NDJSON。
        if serde_json::to_writer(&mut out, &resp).is_err() {
            break;
        }
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
    eprintln!("[wechat-mcp] stdin 关闭，退出");
}

/// `--dir <path>`：二维码与登录态输出目录。默认当前目录。
fn parse_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--dir" {
            if let Some(v) = args.next() {
                return PathBuf::from(v);
            }
        } else if let Some(rest) = a.strip_prefix("--dir=") {
            return PathBuf::from(rest);
        }
    }
    PathBuf::from(".")
}

fn handle(ch: &Arc<Channel>, req: &Value) -> Option<Value> {
    let method = req.get("method")?.as_str()?;
    let rid = req.get("id")?;
    if rid.is_null() {
        return None; // 通知
    }
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}, "resources": {}},
            "serverInfo": {"name": "abb-channel-wechat", "version": env!("CARGO_PKG_VERSION")},
        })),
        // 只暴露 `send_text` + `poll` + `login`：
        // - `send_thread_reply`：微信没有话题概念，不声明（ABB 会自动回落 send_text）
        // - `resources`：本进程入站走 poll，不声明资源（ABB 会据能力选路）
        "tools/list" => Ok(json!({"tools": tools()})),
        "resources/list" => Ok(json!({"resources": []})),
        "tools/call" => call_tool(ch, &params),
        other => Err(format!("未实现的方法 `{other}`")),
    };

    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": rid, "result": r}),
        Err(msg) => json!({
            "jsonrpc": "2.0", "id": rid,
            "error": {"code": -32603, "message": msg}
        }),
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "send_text",
            "description": "发文本给微信会话（内部自动带 context_token）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": {"type": "string", "description": "会话键（微信 = user id）"},
                    "text": {"type": "string"}
                },
                "required": ["chat_id", "text"]
            }
        },
        {
            "name": "poll",
            "description": "取入站消息。首次不传 cursor；之后回传上次的 next_cursor",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cursor": {"type": "string"},
                    "limit": {"type": "integer"}
                }
            }
        },
        {
            "name": "login",
            "description": "扫码登录：渲染二维码 PNG 到 --dir，返回图片路径（给人扫）",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}

fn call_tool(ch: &Arc<Channel>, params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let a = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "send_text" => {
            let chat_id = a.get("chat_id").and_then(|v| v.as_str()).unwrap_or("");
            let text = a.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if chat_id.is_empty() {
                return Err("send_text 缺 chat_id".into());
            }
            let ctx = ch
                .contexts
                .lock()
                .map_err(|_| "contexts 锁中毒".to_string())?
                .get(chat_id)
                .cloned()
                .unwrap_or_default();
            // 空 context_token 也能发（微信侧会按新会话处理），但要记日志，
            // 因为这意味着「ABB 在对方还没说过话时主动发」——那种消息可能被丢弃。
            if ctx.is_empty() {
                eprintln!("[wechat-mcp] 警告：chat_id={chat_id} 无 context_token，按新会话发送");
            }
            let rt = rt();
            rt.block_on(ch.client.send_text(chat_id, &ctx, text))
                .map_err(|e| format!("send_text 失败：{e}"))?;
            Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
        }
        "poll" => {
            let limit = a.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let cursor = a
                .get("cursor")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| ch.cursor.lock().map(|c| c.clone()).unwrap_or_default());
            let rt = rt();
            let got = rt
                .block_on(ch.client.get_updates(&cursor, 20_000))
                .map_err(|e| format!("poll 失败：{e}"))?;
            let (msgs, next, _) = got;
            let mut out = Vec::new();
            for m in msgs.iter().take(limit) {
                if let Some(cm) = to_channel_message(ch, m) {
                    out.push(cm);
                }
            }
            // **游标只在成功后推进**（这里是成功路径）。
            if let Ok(mut c) = ch.cursor.lock() {
                *c = next.clone();
            }
            Ok(json!({"content": [{"type": "text", "text": json!({
                "messages": out,
                "next_cursor": next,
            }).to_string()}]}))
        }
        "login" => {
            let rt = rt();
            let (qc, _uuid) = rt
                .block_on(weixin::fetch_qrcode())
                .map_err(|e| format!("取二维码失败：{e}"))?;
            let p = weixin::save_qrcode_image(&ch.dir, &qc)
                .map_err(|e| format!("写二维码失败：{e}"))?;
            // 路径打到 stderr：人看得到，ABB 不会误读成协议数据。
            eprintln!("[wechat-mcp] 请用微信扫码：{}", p.display());
            Ok(json!({"content": [{"type": "text", "text": json!({
                "qrcode_path": p.display().to_string(),
                "next": "扫码后调 login_status 拿 token（未实现，见卡）",
            }).to_string()}]}))
        }
        other => Err(format!("未知工具 `{other}`")),
    }
}

/// 微信消息 → 契约 §3.3 的 `ChannelMessage`。
///
/// **顺手把 `context_token` 记下来**——这是原 ABB `note_context` 的归宿。
fn to_channel_message(ch: &Arc<Channel>, m: &WeixinMessage) -> Option<Value> {
    let from = m.from_user_id.trim();
    if from.is_empty() {
        return None;
    }
    // 只处理别人发来的（自己发的会 echo 回来，忽略以防自循环）。
    let me = ch.user_id.lock().map(|u| u.clone()).unwrap_or_default();
    if !me.is_empty() && from == me {
        return None;
    }
    if !m.context_token.is_empty() {
        if let Ok(mut c) = ch.contexts.lock() {
            c.insert(from.to_string(), m.context_token.clone());
        }
    }
    let text = m.text();
    Some(json!({
        "chat_id": from,
        "message_id": m.message_id,
        "sender_id": from,
        "sender_name": "",
        "text": text,
        "ts": if m.create_time_ms > 0 { m.create_time_ms / 1000 } else { 0 },
        "attachments": [],
    }))
}

/// 只在需要时建 runtime（工具调用是同步入口，内部用 `block_on`）。
///
/// 单线程足够：每次调用都是「一次 HTTP 往返」，没有并发需求。
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建 tokio runtime 失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_channel() -> Arc<Channel> {
        Arc::new(Channel {
            client: WeixinClient::new("http://127.0.0.1:1", "t", "http://127.0.0.1:1"),
            contexts: Mutex::new(HashMap::new()),
            cursor: Mutex::new(String::new()),
            user_id: Mutex::new("me".into()),
            dir: PathBuf::from("."),
        })
    }

    /// 能力自报必须**只**声明 send_text + poll + login：
    /// 声明 `send_thread_reply` 会让 ABB 走话题路径（微信没有话题）；
    /// 声明 `channel://` 资源会让 ABB 选订阅（本进程不支持）。
    #[test]
    fn tools_list_declares_exactly_what_we_support() {
        let names: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["send_text", "poll", "login"]);
        assert!(
            !names.contains(&"send_thread_reply".to_string()),
            "微信无话题概念，不该声明它（否则 ABB 不回落 send_text）"
        );
    }

    /// resources/list 必须是空数组（不是 null）：ABB 的 `capabilities_from`
    /// 把「缺 resources 字段」当没有资源，但显式空数组更明确。
    #[test]
    fn resources_list_is_empty_array_not_null() {
        let r = handle(
            &fake_channel(),
            &json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
        )
        .expect("要回包");
        assert_eq!(r["result"]["resources"], json!([]));
    }

    /// 自己发的消息不该回灌（防自循环）。
    #[test]
    fn own_message_is_ignored() {
        let ch = fake_channel();
        let m = WeixinMessage {
            from_user_id: "me".into(),
            message_id: "1".into(),
            ..Default::default()
        };
        assert!(
            to_channel_message(&ch, &m).is_none(),
            "自己发的不该进 poll 结果"
        );
    }

    /// **`context_token` 必须被记下来**——这是 send_text 能工作的前提，
    /// 也是原 ABB `note_context` 的归宿。
    #[test]
    fn incoming_message_records_context_token() {
        let ch = fake_channel();
        let m = WeixinMessage {
            from_user_id: "user-a".into(),
            message_id: "1".into(),
            context_token: "ctx-123".into(),
            ..Default::default()
        };
        let cm = to_channel_message(&ch, &m).expect("应当转出一条");
        assert_eq!(cm["chat_id"], "user-a");
        assert_eq!(
            ch.contexts.lock().unwrap().get("user-a"),
            Some(&"ctx-123".to_string()),
            "context_token 没记下来 ⇒ 后续 send_text 会当新会话发"
        );
    }

    /// 空 from 的消息丢弃（微信会推系统事件，那些没有 sending user）。
    #[test]
    fn empty_sender_is_dropped() {
        let ch = fake_channel();
        let m = WeixinMessage {
            from_user_id: "".into(),
            message_id: "x".into(),
            ..Default::default()
        };
        assert!(to_channel_message(&ch, &m).is_none());
    }

    /// 未实现的方法要报错（不是静默成功）。
    #[test]
    fn unknown_method_returns_error() {
        let r = handle(
            &fake_channel(),
            &json!({"jsonrpc":"2.0","id":9,"method":"nope"}),
        )
        .expect("要回包");
        assert!(r.get("error").is_some(), "未知方法必须回 error");
    }
}
