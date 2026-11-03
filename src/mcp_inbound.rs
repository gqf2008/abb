//! 渠道 MCP 的**入站**（总线 S2）。
//!
//! 契约见 `docs/mcp-bus-architecture.md` §3.2：
//!
//! | 方案 | 形式 | 适用 |
//! |---|---|---|
//! | **A. 资源订阅**（首选） | 订阅 `channel://…/inbox`，收到 `notifications/resources/updated` 后 `resources/read` | server 支持 resources |
//! | **B. 工具轮询**（兜底） | 工具 `poll` → `{ messages, next_cursor }` | 不支持 resources |
//!
//! 两者都不可用 ⇒ 建连时就判该渠道不可用（`ChannelCapabilities::validate` 已经在做）。
//!
//! ## 为什么「订阅」不够，必须再 read 一次
//!
//! MCP 的 `notifications/resources/updated` **只带 URI、不带内容**——这是规范设计
//! （避免推送大载荷）。所以收到通知只是「有变化」的信号，真正的消息要 `resources/read`。
//! 把通知当消息解析会得到一条空消息，而 ABB 会以为渠道在推空消息。
//!
//! ## 为什么两种方案都要实现
//!
//! 若只做 A，则会拒绝所有不支持 resources 的 server（很多轻量实现只暴露 tools）。
//! 若只做 B，订阅路径的实时性就白丢了。**按能力选路**，是 `ChannelCapabilities` 存在的意义。
//!
//! ## 游标语义（B 方案）
//!
//! `next_cursor` 由**渠道服务**决定（它才知道自己的消息 id 怎么排）。ABB 只负责
//! 原样回传上次拿到的值——不猜、不重排、不从消息 id 反推序号。

// 这里曾有 `#![allow(dead_code)]`，注释写「入站还没接进 service 的轮询循环（S2b）」。
//
// **那个 allow 已经移除**（2026-11-03）：`mcp_channel_loop.rs` 现在真的在调用本模块
// （`negotiate` / `messages_from_notifications` / `poll_once`），摘掉后 clippy 零报错。
use serde_json::{json, Value};

use crate::mcp_bus::{updated_uri, ChannelMessage, Inbound, TOOL_POLL};
use crate::mcp_client::StdioChannelClient;

/// 入站实现。
///
///  是为了能移进 （同步 client 的调用要挪到阻塞线程）。
#[derive(Debug, Clone)]
pub enum ChannelInbound {
    /// A：订阅若干 URI，等通知后 read。
    Subscribe {
        /// 已成功订阅的 URI（`resources/subscribe` 全部成功的那些）。
        uris: Vec<String>,
        /// 通知里出现的、但**没订阅成功**的 URI ⇒ 只记着，不 read（read 它会重复拿）。
        ignored: Vec<String>,
    },
    /// B：工具轮询。
    Poll,
}

/// 从能力清单选出入站方式。
///
/// A 优先：`resources/subscribe` 至少要能订上一个 URI；一个都订不上就退 B；
/// 两者都不可用返回 `None`（调用方据此判渠道不可用，**不静默降级**）。
///
/// 注意：这里真的发订阅请求，不只是看清单——`resources/list` 里列出 URI
/// 不代表 `resources/subscribe` 会成功（实现常只做前者）。
pub fn negotiate(
    client: &mut StdioChannelClient,
    caps: &crate::mcp_bus::ChannelCapabilities,
) -> Option<ChannelInbound> {
    if let Some(Inbound::Subscribe(uris)) = caps.inbound() {
        let mut ok = Vec::new();
        let mut failed = Vec::new();
        for uri in uris {
            match client.call("resources/subscribe", json!({ "uri": uri })) {
                Ok(_) => ok.push(uri.clone()),
                Err(_) => failed.push(uri.clone()),
            }
        }
        if !ok.is_empty() {
            return Some(ChannelInbound::Subscribe {
                uris: ok,
                ignored: failed,
            });
        }
        // A 全部订不上 ⇒ 落到 B（若 B 也不可用，下面 `caps.has_poll` 会返回 None）。
    }
    if caps.has_poll {
        return Some(ChannelInbound::Poll);
    }
    None
}

/// 一次入站拉取的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboundBatch {
    pub messages: Vec<ChannelMessage>,
    /// B 方案下次要回传的游标（A 方案为 `None`）。
    pub next_cursor: Option<String>,
}

/// A 方案：把一批通知里涉及的 URI 换成消息。
///
/// 入参是 `drain_notifications()` 拿到的原始通知列表——**只认 `channel://` 且已订阅的**：
/// - 未定义的 payload 变体 ⇒ 丢弃（`updated_uri` 已做）；
/// - 没订阅成功的 URI ⇒ 丢弃（read 它会把同一批消息读两遍）；
/// - 不是我们订阅的那个 URI ⇒ 丢弃。
///
/// 为什么要这么严：`resources/read` 的结果会直接变成给用户的消息，
/// 读错资源 = 把别的会话内容发到这个会话里。
pub fn messages_from_notifications(
    client: &mut StdioChannelClient,
    inbound: &ChannelInbound,
    notifications: &[Value],
    per_read_limit: usize,
) -> Result<Vec<ChannelMessage>, String> {
    let ChannelInbound::Subscribe { uris, ignored } = inbound else {
        return Err("messages_from_notifications 只适用于订阅方案".into());
    };
    let mut out: Vec<ChannelMessage> = Vec::new();
    let mut seen_uris: Vec<&str> = Vec::new();
    for n in notifications {
        let Some(uri) = n.get("params").and_then(updated_uri) else {
            continue;
        };
        if !uris.iter().any(|u| u == uri) || ignored.iter().any(|u| u == uri) {
            continue;
        }
        // 同一条通知可能重复到达（服务端合并/重试）；重复 read 会拿到同样的消息，
        // 由下面的 message_id 去重兜住，但少发一次请求更省事。
        if seen_uris.contains(&uri) {
            continue;
        }
        seen_uris.push(uri);
        let resp = client
            .call("resources/read", json!({ "uri": uri }))
            .map_err(|e| format!("读 `{uri}` 失败：{e}"))?;
        out.extend(messages_from_resource(&resp, Some(uri), per_read_limit)?);
    }
    Ok(out)
}

/// 把 `resources/read` 的返回体解成消息。
///
/// 形状：`{ contents: [ { uri, mimeType?, text: "<json>" } ] }`——
/// `text` 里是 §3.3 定义的单条或多条消息（数组或单个对象都接受，
/// 因为「一个 inbox 里攒了几条」是常见实现）。
pub fn messages_from_resource(
    resp: &Value,
    uri: Option<&str>,
    limit: usize,
) -> Result<Vec<ChannelMessage>, String> {
    let contents = resp
        .get("contents")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "resources/read 缺 contents 数组".to_string())?;
    let mut out = Vec::new();
    for c in contents {
        // 有 uri 时只认我们自己订阅的那个（`contents` 可能是多资源混装）。
        if let (Some(want), Some(got)) = (uri, c.get("uri").and_then(|v| v.as_str())) {
            if want != got {
                continue;
            }
        }
        let Some(text) = c.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        out.extend(parse_messages(text, limit)?);
    }
    out.truncate(limit);
    Ok(out)
}

/// 把一段 JSON 文本解成消息列表。
///
/// **单条对象也接受**：契约写的是「消息形状」，一个 inbox 只放一条时服务端
/// 很可能直接给对象。拒绝它会让「只推一条」的实现完全不能用。
pub fn parse_messages(text: &str, limit: usize) -> Result<Vec<ChannelMessage>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("消息内容不是合法 JSON：{e}"))?;
    let arr: Vec<Value> = match v {
        Value::Array(a) => a,
        Value::Object(_) => vec![v],
        _ => return Err("消息内容既不是数组也不是对象".into()),
    };
    let mut out = Vec::new();
    for item in arr {
        // 形状不对**整批报错**而不是跳过：静默跳过会让「渠道收到了但 ABB 没回」
        // 变成无迹可查的丢消息。
        let m: ChannelMessage =
            serde_json::from_value(item).map_err(|e| format!("消息形状不符合契约 §3.3：{e}"))?;
        if m.chat_id.trim().is_empty() || m.message_id.trim().is_empty() {
            return Err("消息缺 chat_id 或 message_id（契约 §3.3 要求两者必填）".into());
        }
        out.push(m);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// B 方案：调 `poll` 取消息。
///
/// `cursor` 是上次拿到的 `next_cursor`，原样回传（见模块注释）。
pub fn poll_once(
    client: &mut StdioChannelClient,
    cursor: Option<&str>,
    limit: usize,
) -> Result<InboundBatch, String> {
    let params = match cursor {
        Some(c) => json!({ "cursor": c, "limit": limit }),
        None => json!({ "limit": limit }),
    };
    let resp = client
        .call(
            "tools/call",
            json!({ "name": TOOL_POLL, "arguments": params }),
        )
        .map_err(|e| format!("`{TOOL_POLL}` 调用失败：{e}"))?;
    parse_poll_result(&resp, limit)
}

/// 解 `poll` 的返回。形状：`{ content:[{type:"text",text:"<json>"}] }`，
/// 里面是 `{ messages: [...], next_cursor: "…" }`。
///
/// **两种形状都接受**：MCP 要求工具结果包在 `content` 里，但有的实现直接把
/// `{messages,…}` 放进 `structuredContent` 或顶层。只认一种会让能用的 server 用不了。
pub fn parse_poll_result(resp: &Value, limit: usize) -> Result<InboundBatch, String> {
    let payload: Value = if let Some(arr) = resp.get("content").and_then(|v| v.as_array()) {
        let text = arr
            .iter()
            .filter(|s| s.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|s| s.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("");
        serde_json::from_str(&text)
            .map_err(|e| format!("{TOOL_POLL} 返回的文本不是合法 JSON：{e}"))?
    } else if resp.get("structuredContent").is_some() {
        resp["structuredContent"].clone()
    } else {
        resp.clone()
    };

    let msgs = payload
        .get("messages")
        .cloned()
        .unwrap_or(Value::Array(vec![]));
    let messages = parse_messages(&msgs.to_string(), limit)?;
    let next_cursor = payload
        .get("next_cursor")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(InboundBatch {
        messages,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(inbox: &[&str], poll: bool) -> crate::mcp_bus::ChannelCapabilities {
        crate::mcp_bus::ChannelCapabilities {
            has_send_text: true,
            has_send_thread_reply: false,
            has_send_attachment: false,
            has_poll: poll,
            inbox_uris: inbox.iter().map(|s| s.to_string()).collect(),
            tool_names: vec![],
        }
    }

    fn fake(body: &str) -> StdioChannelClient {
        StdioChannelClient::spawn("fake", "/bin/sh", &["-c".into(), body.into()], &[])
            .unwrap_or_else(|e| panic!("起不来脚本：{e}"))
    }

    // ── 消息解析 ────────────────────────────────────────────────

    #[test]
    fn single_object_is_accepted_as_one_message() {
        // 只推一条的实现不该被判不能接。
        let msgs = parse_messages(r#"{"chat_id":"c","message_id":"m","text":"hi"}"#, 10)
            .expect("单条对象应当接受");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "hi");
    }

    #[test]
    fn array_of_messages_is_parsed_in_order() {
        let msgs = parse_messages(
            r#"[{"chat_id":"c","message_id":"1"},{"chat_id":"c","message_id":"2"}]"#,
            10,
        )
        .unwrap();
        assert_eq!(
            msgs.iter()
                .map(|m| m.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2"]
        );
    }

    /// **缺 chat_id / message_id 必须整批报错**，不许跳过。
    /// 跳过的话，渠道「收到但 ABB 没回」会变成无迹可查的丢消息。
    #[test]
    fn missing_required_fields_fails_the_batch() {
        let err = parse_messages(r#"[{"message_id":"1"}]"#, 10).unwrap_err();
        assert!(err.contains("chat_id"), "报因要点名缺哪个字段：{err}");
        let err = parse_messages(r#"[{"chat_id":"c"}]"#, 10).unwrap_err();
        assert!(err.contains("message_id"), "报因要点名缺哪个字段：{err}");
        let err = parse_messages(r#"[{"chat_id":"  ","message_id":"1"}]"#, 10).unwrap_err();
        assert!(err.contains("缺"), "空白也算缺：{err}");
    }

    #[test]
    fn limit_truncates() {
        let msgs = parse_messages(
            r#"[{"chat_id":"c","message_id":"1"},{"chat_id":"c","message_id":"2"},
                {"chat_id":"c","message_id":"3"}]"#,
            2,
        )
        .unwrap();
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn attachment_shape_is_carried_through() {
        let msgs = parse_messages(
            r#"[{"chat_id":"c","message_id":"m","attachments":[
                 {"kind":"image","name":"a.png","url_or_path":"/tmp/a.png"}]}]"#,
            10,
        )
        .unwrap();
        assert_eq!(msgs[0].attachments.len(), 1);
        assert_eq!(msgs[0].attachments[0].kind, "image");
    }

    // ── 方案选择 ────────────────────────────────────────────────

    #[test]
    fn subscribe_is_preferred_and_only_successful_uris_count() {
        // 第一个 URI 订阅成功、第二个失败 ⇒ 用成功的那个，失败的进 ignored。
        let mut c = fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *"channel://good/inbox"*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                   *"channel://bad/inbox"*)  printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"nope"}}\n' "$id" ;;
                   *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
                 esac
               done"#,
        );
        let got = negotiate(
            &mut c,
            &caps(&["channel://good/inbox", "channel://bad/inbox"], false),
        )
        .expect("应当走订阅");
        match got {
            ChannelInbound::Subscribe { uris, ignored } => {
                assert_eq!(uris, vec!["channel://good/inbox"]);
                assert_eq!(ignored, vec!["channel://bad/inbox"]);
            }
            ChannelInbound::Poll => panic!("没订上任何一个就该退 B，这里订上了一个"),
        }
    }

    #[test]
    fn falls_back_to_poll_when_all_subscribes_fail() {
        let mut c = fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no"}}\n' "$id"
               done"#,
        );
        let got = negotiate(&mut c, &caps(&["channel://a/inbox"], true)).expect("应退 B");
        assert!(matches!(got, ChannelInbound::Poll));
    }

    /// 订阅全失败、也没有 poll ⇒ **返回 None**（渠道不可用），不静默降级。
    #[test]
    fn none_when_neither_works() {
        let mut c = fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no"}}\n' "$id"
               done"#,
        );
        assert!(negotiate(&mut c, &caps(&["channel://a/inbox"], false)).is_none());
    }

    // ── 通知 → 读资源 ───────────────────────────────────────────

    #[test]
    fn notification_for_unsubscribed_uri_is_ignored() {
        // read 一个没订阅的 URI 会把同一批消息读两遍（重复发给用户）。
        let mut c = fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"contents":[{"uri":"channel://other/inbox","text":"{\"chat_id\":\"c\",\"message_id\":\"leak\"}"}]}}\n' "$id"
               done"#,
        );
        let inbound = ChannelInbound::Subscribe {
            uris: vec!["channel://mine/inbox".into()],
            ignored: vec!["channel://other/inbox".into()],
        };
        let ns = vec![json!({"method":"notifications/resources/updated",
                             "params":{"uri":"channel://other/inbox"}})];
        let got = messages_from_notifications(&mut c, &inbound, &ns, 10).unwrap();
        assert!(got.is_empty(), "不该读到没订阅的 URI：{got:?}");
    }

    #[test]
    fn subscribed_notification_reads_and_parses() {
        let mut c = fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"contents":[{"uri":"channel://mine/inbox","text":"[{\\"chat_id\\":\\"c\\",\\"message_id\\":\\"m1\\",\\"text\\":\\"hi\\"}]"}]}}\n' "$id"
               done"#,
        );
        let inbound = ChannelInbound::Subscribe {
            uris: vec!["channel://mine/inbox".into()],
            ignored: vec![],
        };
        let ns = vec![json!({"method":"notifications/resources/updated",
                             "params":{"uri":"channel://mine/inbox"}})];
        let got = messages_from_notifications(&mut c, &inbound, &ns, 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "hi");
    }

    /// 同一条通知来两次只 read 一次（服务端合并/重试很常见）。
    #[test]
    fn duplicate_notification_reads_once() {
        let mut c = fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"contents":[{"uri":"channel://mine/inbox","text":"[{\\"chat_id\\":\\"c\\",\\"message_id\\":\\"m1\\"}]"}]}}\n' "$id"
               done"#,
        );
        let inbound = ChannelInbound::Subscribe {
            uris: vec!["channel://mine/inbox".into()],
            ignored: vec![],
        };
        let n = json!({"method":"notifications/resources/updated",
                       "params":{"uri":"channel://mine/inbox"}});
        let got = messages_from_notifications(&mut c, &inbound, &[n.clone(), n], 10).unwrap();
        assert_eq!(got.len(), 1, "重复通知不该读两次：{got:?}");
    }

    // ── B 方案 ─────────────────────────────────────────────────

    #[test]
    fn poll_reads_content_text_json() {
        let mut c = fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"{\\"messages\\":[{\\"chat_id\\":\\"c\\",\\"message_id\\":\\"m\\"}],\\"next_cursor\\":\\"cur-2\\"}"}]}}\n' "$id"
               done"#,
        );
        let b = poll_once(&mut c, None, 10).unwrap();
        assert_eq!(b.messages.len(), 1);
        assert_eq!(b.next_cursor.as_deref(), Some("cur-2"));
    }

    /// 游标必须**原样回传**（服务端才知道自己的排序）。
    #[test]
    fn poll_passes_cursor_back_verbatim() {
        let mut c = fake(
            r#"while IFS= read -r line; do
                 id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                 case "$line" in
                   *WANT-CURSOR-XYZ*)
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"{\\"messages\\":[],\\"next_cursor\\":\\"cur-3\\"}"}]}}\n' "$id" ;;
                   *)
                     printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"cursor not passed"}}\n' "$id" ;;
                 esac
               done"#,
        );
        let b = poll_once(&mut c, Some("WANT-CURSOR-XYZ"), 10).expect("游标应当原样传过去");
        assert_eq!(b.next_cursor.as_deref(), Some("cur-3"));
    }

    /// 没 `content` 但顶层直接给 `{messages,…}` 也要接受（轻量实现常这样）。
    #[test]
    fn poll_accepts_top_level_shape() {
        let resp = json!({"messages":[{"chat_id":"c","message_id":"m"}],"next_cursor":"n"});
        let b = parse_poll_result(&resp, 10).unwrap();
        assert_eq!(b.messages.len(), 1);
        assert_eq!(b.next_cursor.as_deref(), Some("n"));
    }

    #[test]
    fn poll_with_no_messages_is_not_an_error() {
        let resp = json!({"content":[{"type":"text","text":"{\"messages\":[]}"}]});
        let b = parse_poll_result(&resp, 10).unwrap();
        assert!(b.messages.is_empty());
        assert!(b.next_cursor.is_none());
    }
}
