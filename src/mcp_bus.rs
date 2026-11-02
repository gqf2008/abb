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
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// 出站工具名（契约 §3.1）。缺失即该渠道不可用。
pub const TOOL_SEND_TEXT: &str = "send_text";
/// 出站工具名：会话内话题回复（可选，缺失回落 [`TOOL_SEND_TEXT`]）。
pub const TOOL_SEND_THREAD_REPLY: &str = "send_thread_reply";
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
