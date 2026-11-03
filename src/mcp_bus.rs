//! ABB 作为 MCP 总线：渠道侧契约（**ABB 自己**是 MCP client，不依赖执行层）。
//!
//! ## 为什么这一层必须归 ABB
//!
//! 旧模型把 MCP 连接放在执行层（`abb-agent`），再让它用**私有 ACP 通知**
//! `_abb/unstable/resource/updated` 把事件转给 ABB。两个致命点：
//!
//! 1. **标准之外**：ACP 规范里没有这个通知 ⇒ 任何其它 ACP agent（ki / Claude / Codex）
//!    都不可能实现它。2026-10-23 执行层换成 ki 后能力直接归零（实测 ki 二进制里
//!    `_abb/unstable` 零命中）——这正是旧模型脆弱性的实证，不是 ki 的缺陷。
//! 2. **层级放错**：MCP 连接是 ABB 的关切（它要拿渠道事件），不是 agent 的关切
//!    （agent 只需要能调工具）。
//!
//! 目标拓扑与分期见 `docs/mcp-bus-architecture.md`。
//!
//! ## 本模块的边界
//!
//! 这里只放**契约类型与校验**——纯数据、可单测、不碰网络。真正的连接（`rmcp`）在后续
//! 步骤接进来；把契约先钉住，是为了让「渠道服务该满足什么」有唯一可查的权威，
//! 而不是散落在各处的字符串。
//!
//! **不认识的字段不报错**（MCP server 由外部提供，加字段是它们的事）；
//! **缺必需字段要报错**——静默降级会让渠道「显示在线但收不到消息」。
//!
//! 契约常量与校验函数在本步骤（S1a）只被测试使用；接线到实际连接是 S1b/S1c，
//! 所以整模块暂时 `allow(dead_code)`——移除它的时候必须是「真的接上了」而不是「先关警告」。
use serde::{Deserialize, Serialize};

/// 出站工具名（契约 §3.1）。缺失即该渠道不可用。
pub const TOOL_SEND_TEXT: &str = "send_text";
/// 出站工具名：会话内话题回复（可选，缺失回落 [`TOOL_SEND_TEXT`]）。
pub const TOOL_SEND_THREAD_REPLY: &str = "send_thread_reply";
/// 出站**可选**工具名：发附件（契约 §3.5）。
///
/// 缺它就**只能发文本**——ABB 的 `Messenger::send_attachment` 会显式失败并说清原因，
/// 不会假装送达（历史教训：旧默认实现把路径当文本发出去还返回 `Ok`）。
pub const TOOL_SEND_ATTACHMENT: &str = "send_attachment";

/// 入站兜底工具名（契约 §3.2 方案 B）。
pub const TOOL_POLL: &str = "poll";
/// 入站首选资源 URI 前缀（契约 §3.2 方案 A）。
pub const INBOX_URI_PREFIX: &str = "channel://";
/// 首选入站资源 URI（全局收件箱）。
pub const INBOX_URI: &str = "channel://inbox";

/// 一条入站消息（契约 §3.3）。
///
/// `chat_id` 的语义照 [`crate::messenger::Messenger`] 既有约定：渠道内会话键
/// （飞书 = `chat_id`，微信 = `ilink_user_id`，邮件 = 线程键……）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMessage {
    pub chat_id: String,
    pub message_id: String,
    #[serde(default)]
    pub sender_id: String,
    #[serde(default)]
    pub sender_name: String,
    #[serde(default)]
    pub text: String,
    /// 渠道时间戳（秒）。0 = 未提供。
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub attachments: Vec<ChannelAttachment>,
}

/// 入站附件（只留定位信息，下载仍由渠道侧负责——ABB 不碰各渠道的文件 API）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAttachment {
    /// `image` / `file` / 其它渠道自定义值。
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url_or_path: String,
}

/// 契约校验失败的原因。**每条都要能直接讲给用户看**（ABB 有「告知怎么配」的义务）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// 出站工具缺失——没有它连回复都发不出去。
    MissingSendText,
    /// 入站两条路都不可用。
    NoInbound,
    /// 资源 URI 形状不对（有 `channel://` 前缀但解析不出会话键）。
    BadInboxUri(String),
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSendText => write!(
                f,
                "渠道服务缺工具 `{TOOL_SEND_TEXT}`——ABB 无法回消息。\
                 该 MCP server 需要暴露 `{TOOL_SEND_TEXT}`（参数 chat_id / text）。"
            ),
            Self::NoInbound => write!(
                f,
                "渠道服务没有任何入站能力：既没有 `{INBOX_URI_PREFIX}` 资源可订阅，\
                 也没有 `{TOOL_POLL}` 工具。二者至少需要一个，否则收不到消息。"
            ),
            Self::BadInboxUri(uri) => write!(
                f,
                "资源 URI `{uri}` 形状不对：需要 `{INBOX_URI_PREFIX}<chat_id>/inbox` 或 `{INBOX_URI}`。"
            ),
        }
    }
}

/// 一个渠道服务**自报**的能力清单（从 `tools/list` + `resources/list` 归纳）。
///
/// 这是 ABB 侧的**唯一判据**：不信配置里写的声明，只信服务实际暴露的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelCapabilities {
    pub has_send_text: bool,
    pub has_send_thread_reply: bool,
    /// 是否声明了附件工具（可选；缺了就只能发文本）。
    pub has_send_attachment: bool,
    pub has_poll: bool,
    /// 可订阅的收件箱 URI（`resources/list` 里以 `channel://` 开头的那些）。
    pub inbox_uris: Vec<String>,
}

impl ChannelCapabilities {
    /// 入站方式：资源订阅优先，轮询兜底；都没有 ⇒ 不可用。
    pub fn inbound(&self) -> Option<Inbound> {
        if !self.inbox_uris.is_empty() {
            return Some(Inbound::Subscribe(self.inbox_uris.clone()));
        }
        if self.has_poll {
            return Some(Inbound::Poll);
        }
        None
    }

    /// 整体可用性校验。
    pub fn validate(&self) -> Result<(), ContractError> {
        if !self.has_send_text {
            return Err(ContractError::MissingSendText);
        }
        if self.inbound().is_none() {
            return Err(ContractError::NoInbound);
        }
        Ok(())
    }
}

/// 入站实现方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// 方案 A：订阅这些资源，等 `notifications/resources/updated`。
    Subscribe(Vec<String>),
    /// 方案 B：定期调 `poll`。
    Poll,
}

/// 从资源 URI 解析会话键。
///
/// `channel://inbox` ⇒ `None`（全局收件箱，会话键随消息自带）；
/// `channel://<chat_id>/inbox` ⇒ `Some(chat_id)`。
///
/// **当前没被调用，但保留**（逐项 allow 而非删掉）：
/// 入站走的是「订阅整个 inbox → read → 消息自带 chat_id」这条路，用不到从 URI 反解。
/// 但 `channel://<chat_id>/inbox` 是契约允许的形态（`docs/mcp-bus-architecture.md` §3.2），
/// 有些 server 会按会话分资源——接入那种 server 时就要靠它把 URI 映射回会话键。
/// 删掉它等于把契约的一半实现丢掉，而契约本身没变。
#[allow(dead_code)]
pub fn chat_id_of_inbox_uri(uri: &str) -> Result<Option<String>, ContractError> {
    let rest = uri
        .strip_prefix(INBOX_URI_PREFIX)
        .ok_or_else(|| ContractError::BadInboxUri(uri.to_string()))?;
    // **只去掉尾部斜杠**，不能 trim 开头：`channel:///inbox` 的区别正在于开头那个空段
    // （权威部分为空 ⇒ 不是合法形态）。用 `trim_matches('/')` 会把这种畸形输入洗成合法，
    // 于是解析结果与实际归属不符——这是单测 `malformed_inbox_uri_is_rejected` 抓到的。
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return Err(ContractError::BadInboxUri(uri.to_string()));
    }
    if rest == "inbox" {
        return Ok(None);
    }
    match rest.strip_suffix("/inbox") {
        // chat_id 里再出现 '/' 说明是更深的路径，不属于本契约认识的形状。
        Some(chat) if !chat.is_empty() && !chat.contains('/') => Ok(Some(chat.to_string())),
        _ => Err(ContractError::BadInboxUri(uri.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(send: bool, poll: bool, uris: &[&str]) -> ChannelCapabilities {
        ChannelCapabilities {
            has_send_text: send,
            has_send_thread_reply: false,
            has_send_attachment: false,
            has_poll: poll,
            inbox_uris: uris.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn subscribe_wins_over_poll_when_both_available() {
        // 两条路都在时优先订阅：轮询有延迟且有配额代价。
        let c = caps(true, true, &[INBOX_URI]);
        assert_eq!(
            c.inbound(),
            Some(Inbound::Subscribe(vec![INBOX_URI.to_string()]))
        );
    }

    #[test]
    fn poll_is_fallback_when_no_resources() {
        assert_eq!(caps(true, true, &[]).inbound(), Some(Inbound::Poll));
    }

    #[test]
    fn no_inbound_is_an_error_not_a_silent_degradation() {
        // 这条是契约的核心：宁可判不可用，也不要「显示在线但收不到消息」。
        let c = caps(true, false, &[]);
        assert_eq!(c.validate(), Err(ContractError::NoInbound));
        assert!(c.validate().unwrap_err().to_string().contains(TOOL_POLL));
    }

    #[test]
    fn missing_send_text_is_reported_first() {
        // 两端都缺时先报出站：连回复都发不出去，比收不到更根本。
        let c = caps(false, false, &[]);
        assert_eq!(c.validate(), Err(ContractError::MissingSendText));
    }

    #[test]
    fn inbox_uri_parses_global_and_per_chat() {
        assert_eq!(chat_id_of_inbox_uri(INBOX_URI), Ok(None));
        assert_eq!(
            chat_id_of_inbox_uri("channel://oc_abc123/inbox"),
            Ok(Some("oc_abc123".to_string()))
        );
        assert_eq!(
            chat_id_of_inbox_uri("channel://oc_abc123/inbox/"),
            Ok(Some("oc_abc123".to_string()))
        );
    }

    #[test]
    fn malformed_inbox_uri_is_rejected() {
        for bad in [
            "https://x/inbox",           // 前缀不对
            "channel://",                // 空
            "channel:////",              // 只有斜杠
            "channel://oc_abc/messages", // 后缀不是 /inbox
            "channel:///inbox",          // 缺 chat_id
        ] {
            assert!(
                chat_id_of_inbox_uri(bad).is_err(),
                "应拒绝 {bad:?}，实际通过了"
            );
        }
    }

    #[test]
    fn channel_message_tolerates_unknown_and_missing_optionals() {
        // 外部 server 加字段不该让我们解析失败；可选字段缺省要能落地。
        let json = r#"{
            "chat_id":"c1","message_id":"m1",
            "text":"hi","ts":1793612338,
            "extra_from_future_spec":{"a":1}
        }"#;
        let m: ChannelMessage = serde_json::from_str(json).expect("应能解析");
        assert_eq!(m.chat_id, "c1");
        assert_eq!(m.text, "hi");
        assert_eq!(m.ts, 1793612338);
        assert_eq!(m.sender_id, ""); // 缺省为空，不是解析失败
        assert!(m.attachments.is_empty());
    }

    #[test]
    fn channel_message_requires_identity_fields() {
        // chat_id / message_id 是去重与投递的键，缺了必须报错而不是补空串。
        let json = r#"{"text":"hi"}"#;
        assert!(serde_json::from_str::<ChannelMessage>(json).is_err());
    }

    #[test]
    fn attachment_kinds_roundtrip() {
        let a = ChannelAttachment {
            kind: "image".into(),
            name: "a.png".into(),
            url_or_path: "/tmp/a.png".into(),
        };
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<ChannelAttachment>(&s).unwrap(), a);
    }
}

// ── JSON-RPC 2.0 帧（MCP 传输层用）───────────────────────────────────────────
//
// 为什么手写而不引 rmcp：root 包已有手写的 ACP JSON-RPC（`src/runtime/acp.rs`），
// 风格一致；MCP 客户端侧只需要 initialize / tools/list / tools/call /
// resources/list / resources/subscribe / resources/read 六个方法与一种通知，
// 为这点东西在 11 万行的二进制里摊一棵新依赖树不划算。
// （`crates/abb-agent` 那份用 rmcp 是因为它是独立包 + 已有依赖。）

/// JSON-RPC 错误。每条都要能讲给用户听——ABB 有「告知怎么配」的义务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// 响应里带 `error` 对象。
    Remote { code: i64, message: String },
    /// 响应既没有 `result` 也没有 `error`。
    Malformed(&'static str),
    /// 服务端没按契约回形状（例如 `tools` 不是数组）。
    UnexpectedShape(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote { code, message } => write!(f, "渠道服务报错（{code}）：{message}"),
            Self::Malformed(what) => write!(f, "渠道服务回了不合规的响应：缺 {what}"),
            Self::UnexpectedShape(detail) => write!(f, "渠道服务响应形状不符契约：{detail}"),
        }
    }
}

/// 构造一条请求（带 id，期待响应）。
pub fn build_request(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// 构造一条通知（无 id，不期待响应）。
pub fn build_notification(method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// 从一条响应报文里取出 `result`；`error` 或畸形都判错。
///
/// **不看 id**：id 匹配由传输层做（它才知道自己发的哪个 id）。
/// 这里只判断「这条报文作为响应是否合规」。
pub fn parse_response(msg: &serde_json::Value) -> Result<serde_json::Value, RpcError> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("（服务端未给出错误信息）")
            .to_string();
        return Err(RpcError::Remote { code, message });
    }
    match msg.get("result") {
        Some(r) => Ok(r.clone()),
        None => Err(RpcError::Malformed("result / error 二者之一")),
    }
}

/// 从 `tools/list` + `resources/list` 的**响应体**归纳渠道能力。
///
/// 这是「只信服务实际暴露的，不信配置里写的」这条原则的落点：
/// 两个入参都是服务端原始响应，不是我们的推断。
pub fn capabilities_from(
    tools_list: &serde_json::Value,
    resources_list: &serde_json::Value,
) -> Result<ChannelCapabilities, RpcError> {
    let tools = tools_list
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| RpcError::UnexpectedShape("tools/list 缺 tools 数组".into()))?;
    let has = |name: &str| {
        tools
            .iter()
            .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
    };

    // resources/list 缺 resources 不算错：很多 server 根本不实现资源面。
    // 但**有**却不是数组就是形状错。
    let resources: &[serde_json::Value] = match resources_list.get("resources") {
        Some(v) => v.as_array().map(|a| a.as_slice()).ok_or_else(|| {
            RpcError::UnexpectedShape("resources/list 的 resources 不是数组".into())
        })?,
        None => &[],
    };
    let inbox_uris: Vec<String> = resources
        .iter()
        .filter_map(|r| r.get("uri").and_then(|v| v.as_str()))
        .filter(|u| u.starts_with(INBOX_URI_PREFIX))
        .map(|u| u.to_string())
        .collect();

    Ok(ChannelCapabilities {
        has_send_text: has(TOOL_SEND_TEXT),
        has_send_thread_reply: has(TOOL_SEND_THREAD_REPLY),
        has_send_attachment: has(TOOL_SEND_ATTACHMENT),
        has_poll: has(TOOL_POLL),
        inbox_uris,
    })
}

/// 从 `notifications/resources/updated` 取出被更新的 URI。
///
/// 形状：`params: { "uri": "channel://inbox" }`。其它形状返回 `None`
/// （不是我们的通知、或服务端用了未定义的变体）——**丢弃而不猜**。
pub fn updated_uri(params: &serde_json::Value) -> Option<&str> {
    params.get("uri").and_then(|v| v.as_str())
}

#[cfg(test)]
mod rpc_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_and_notification_shapes_differ_only_by_id() {
        // 带 id = 期待响应；无 id = 通知。传输层靠这个区别分流，
        // 所以形状必须逐字段钉住（多了少了 id 都会让分流错位）。
        let req = build_request(7, "tools/list", json!({}));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 7);
        assert_eq!(req["method"], "tools/list");
        assert!(req.get("params").is_some());

        let note = build_notification("notifications/initialized", json!({}));
        assert_eq!(note["jsonrpc"], "2.0");
        assert_eq!(note["method"], "notifications/initialized");
        assert!(note.get("id").is_none(), "通知不得带 id");
    }

    #[test]
    fn response_with_result_is_ok() {
        let got = parse_response(&json!({"jsonrpc":"2.0","id":1,"result":{"tools":[]}})).unwrap();
        assert_eq!(got["tools"], json!([]));
    }

    #[test]
    fn response_with_error_carries_code_and_message() {
        // 报因必须能把话说给用户听，所以 code/message 都得留下。
        let err = parse_response(&json!({
            "jsonrpc":"2.0","id":1,
            "error":{"code":-32601,"message":"Method not found"}
        }))
        .unwrap_err();
        let RpcError::Remote { code, message } = err else {
            panic!("应为 Remote，实得 {err:?}");
        };
        assert_eq!(code, -32601);
        assert_eq!(message, "Method not found");
    }

    #[test]
    fn error_without_message_field_still_explains_itself() {
        // 服务端不回 message 时不得吐出空串——那等于什么也没说。
        let err = parse_response(&json!({"error":{"code":-1}})).unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(err.to_string().contains("未给出"), "实得：{err}");
    }

    #[test]
    fn response_with_neither_result_nor_error_is_rejected() {
        // 既无 result 也无 error 的报文不能当成功——否则传输层会把僵帧当结果。
        assert!(matches!(
            parse_response(&json!({"jsonrpc":"2.0","id":1})),
            Err(RpcError::Malformed(_))
        ));
    }

    #[test]
    fn capabilities_read_the_actual_exposed_surface() {
        // 契约的核心：判据只看服务真暴露了什么。
        let tools = json!({"tools":[
            {"name":"send_text"},
            {"name":"poll"},
            {"name":"something_else"}
        ]});
        let resources = json!({"resources":[
            {"uri":"channel://inbox"},
            {"uri":"file:///other"}
        ]});
        let caps = capabilities_from(&tools, &resources).unwrap();
        assert!(caps.has_send_text);
        assert!(caps.has_poll);
        assert!(!caps.has_send_thread_reply, "未声明的可选能力不得被当成有");
        // 非 channel:// 前缀的资源不进入站候选
        assert_eq!(caps.inbox_uris, vec!["channel://inbox".to_string()]);
        assert_eq!(
            caps.inbound(),
            Some(Inbound::Subscribe(vec!["channel://inbox".to_string()]))
        );
    }

    #[test]
    fn capabilities_without_resources_falls_back_to_poll() {
        // server 不实现资源面是**正常**的，不该报错；此时应落到 poll。
        let caps = capabilities_from(
            &json!({"tools":[{"name":"send_text"},{"name":"poll"}]}),
            &json!({}),
        )
        .unwrap();
        assert_eq!(caps.inbound(), Some(Inbound::Poll));
        caps.validate().expect("send_text + poll 应可用");
    }

    #[test]
    fn capabilities_without_inbound_surface_fails_validation() {
        // 不静默降级：没有入站就判不可用。
        let caps = capabilities_from(
            &json!({"tools":[{"name":"send_text"}]}),
            &json!({"resources":[]}),
        )
        .unwrap();
        assert_eq!(caps.validate(), Err(ContractError::NoInbound));
    }

    #[test]
    fn malformed_tools_list_is_an_error_not_an_empty_capability_set() {
        // 关键：形状错不等于「啥也没有」。若当成空集，会得到
        // 「缺 send_text」这种误导性的报因，掩盖真正的起因（服务端回了怪东西）。
        let err = capabilities_from(&json!({"oops":1}), &json!({})).unwrap_err();
        assert!(matches!(err, RpcError::UnexpectedShape(_)));
        assert!(err.to_string().contains("tools"), "报因要指名缺哪个：{err}");
    }

    #[test]
    fn non_array_resources_is_rejected() {
        let err = capabilities_from(
            &json!({"tools":[{"name":"send_text"}]}),
            &json!({"resources":"nope"}),
        )
        .unwrap_err();
        assert!(matches!(err, RpcError::UnexpectedShape(_)));
    }

    #[test]
    fn updated_uri_only_accepts_the_documented_shape() {
        assert_eq!(
            updated_uri(&json!({"uri":"channel://inbox"})),
            Some("channel://inbox")
        );
        // 未定义变体一律丢弃而不猜（猜错会让 ABB 去读错资源）
        assert_eq!(updated_uri(&json!({"url":"channel://inbox"})), None);
        assert_eq!(updated_uri(&json!({})), None);
        assert_eq!(updated_uri(&json!({"uri":42})), None);
    }
}
