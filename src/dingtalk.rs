//! 钉钉通道 —— 企业内部应用机器人（Stream 模式）。
//!
//! 收：Stream 长连接（无需公网回调地址，对齐飞书 WS）。
//!   ① POST /v1.0/gateway/connections/open（clientId=AppKey / clientSecret=AppSecret）
//!      → {endpoint, ticket}（ticket 一次性、90s 有效，每次连接都要重新注册）；
//!   ② WebSocket 连 `endpoint?ticket=…`，服务端推 JSON 文本帧：
//!      - SYSTEM ping → 回 ack（data 带 opaque）；
//!      - SYSTEM disconnect → 不响应，等服务端 10s 后断开，重连；
//!      - CALLBACK /v1.0/im/bot/messages/get → 回 ack + 交给 bridge.on_dingtalk；
//!      - EVENT → 回 ack（data 带 status）。
//!
//! 保活（#19）：钉钉 Stream 平台**空闲时不主动推帧**，健康检查靠**客户端主动发 WebSocket
//! Ping**（官方 Go/Python SDK 内部 8s keepalive，Rust SDK keep_alive 默认 8000ms）。
//! 旧实现只被动等帧，空闲 180s 无帧被半开看门狗误杀健康连接——须客户端周期发 Ping，
//! 平台回 Pong 即证明连接活着。
//! 发：v1.0 新 OpenAPI，鉴权头 x-acs-dingtalk-access-token。
//!    - 单聊：POST /v1.0/robot/oToMessages/batchSend（userIds=[对方 staffId]）
//!    - 群聊：POST /v1.0/robot/groupMessages/send（openConversationId=cid…）
//!
//! 会话标识约定（Bridge/Messenger 共用）：单聊 chat_id=对方 staffId；群聊 chat_id=openConversationId（cid 开头）。
//! 据此按前缀区分单聊/群聊，无需额外路由表——钉钉群会话 ID 恒以 "cid" 开头。

use crate::bridge::Bridge;
use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const API_BASE: &str = "https://api.dingtalk.com";
/// 单条文本安全长度（按字符数）。钉钉文本消息上限约 2 万字节，这里保守取 8000 字符，
/// 与飞书分段逻辑同一套（split_text 按字符逐行贪心）。
pub const DINGTALK_MSG_LIMIT: usize = 8000;
/// 半开连接看门狗阈值：超过该时长没收到任何入站帧（含 keepalive 的 Pong）即判定通道假死，主动重连。
/// 平台空闲时不推帧（#19），健康判定依赖客户端 keepalive 的 Pong；180s ≈ 6 个 keepalive 周期，足够宽松。
const STALL_AFTER: Duration = Duration::from_secs(180);
/// 客户端 keepalive 间隔：周期发 WebSocket Ping，平台回 Pong。官方 SDK 默认 8s，
/// 这里取 30s 已能可靠续命且省流量（看门狗 180s = 6 拍，容忍 5 拍连续丢 Pong）。
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// 钉钉客户端：access_token 缓存 + 收发消息 + 注册 Stream 连接。
pub struct DingTalkClient {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl DingTalkClient {
    pub fn new(app_id: &str, app_secret: &str) -> DingTalkClient {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        DingTalkClient {
            http,
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            token: Mutex::new(None),
        }
    }

    /// 获取/缓存 app access_token（提前 60s 过期）。
    pub async fn access_token(&self) -> Result<String> {
        {
            let guard = self.token.lock().unwrap();
            if let Some((tok, exp)) = guard.as_ref() {
                if Instant::now() + Duration::from_secs(60) < *exp {
                    return Ok(tok.clone());
                }
            }
        }
        let resp: Value = self
            .http
            .post(format!("{API_BASE}/v1.0/oauth2/accessToken"))
            .json(&json!({"appKey": self.app_id, "appSecret": self.app_secret}))
            .send()
            .await?
            .json()
            .await?;
        let tok = resp["accessToken"]
            .as_str()
            .context("accessToken 缺失（检查 AppKey/AppSecret）")?
            .to_string();
        let expire_secs = resp["expireIn"].as_i64().unwrap_or(7200).max(60) as u64;
        *self.token.lock().unwrap() = Some((
            tok.clone(),
            Instant::now() + Duration::from_secs(expire_secs),
        ));
        Ok(tok)
    }

    /// 发单聊文本（userIds 批量接口，单元素）。
    pub async fn send_single(&self, user_id: &str, robot_code: &str, text: &str) -> Result<()> {
        let token = self.access_token().await?;
        let body = json!({
            "robotCode": robot_code,
            "userIds": [user_id],
            "msgKey": "sampleText",
            "msgParam": serde_json::to_string(&json!({"content": text}))?,
        });
        let resp = self
            .http
            .post(format!("{API_BASE}/v1.0/robot/oToMessages/batchSend"))
            .header("x-acs-dingtalk-access-token", &token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text_body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text_body));
        }
        // 200 但用户 ID 全部无效（如 staffId 配错/已被删除）→ 必须上报，不能谎报成功
        let v: Value = serde_json::from_str(&text_body).unwrap_or(Value::Null);
        let invalid = v["invalidStaffIdList"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        if invalid > 0 {
            anyhow::bail!("钉钉单聊发送失败：{invalid} 个用户 ID 无效（staffId={user_id}）");
        }
        Ok(())
    }

    /// 发群聊文本；at_user_id 非空时 @ 该用户（回复群消息时 @ 提问者，避免被淹没）。
    /// 钉钉的 @ 生效要求：content 里带 "@<userId>" 标记 + at.atUserIds 两样都齐才行
    /// （单独 atUserIds 不渲染成 @ 提及），故这里给内容前缀 @ 标记。
    pub async fn send_group(
        &self,
        conversation_id: &str,
        robot_code: &str,
        text: &str,
        at_user_id: Option<&str>,
    ) -> Result<()> {
        let token = self.access_token().await?;
        let (content, at) = match at_user_id.filter(|u| !u.is_empty()) {
            Some(uid) => (
                format!("@{uid} {text}"),
                json!({"atUserIds": [uid], "isAtAll": false}),
            ),
            None => (text.to_string(), Value::Null),
        };
        let mut body = json!({
            "robotCode": robot_code,
            "openConversationId": conversation_id,
            "msgKey": "sampleText",
            "msgParam": serde_json::to_string(&json!({"content": content}))?,
        });
        if !at.is_null() {
            body["at"] = at;
        }
        let resp = self
            .http
            .post(format!("{API_BASE}/v1.0/robot/groupMessages/send"))
            .header("x-acs-dingtalk-access-token", &token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text_body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text_body));
        }
        Ok(())
    }

    /// 按会话标识分发：cid 开头 = 群聊，否则 = 单聊（staffId）。
    pub async fn send_text(
        &self,
        chat_id: &str,
        robot_code: &str,
        text: &str,
        at_user_id: Option<&str>,
    ) -> Result<()> {
        if is_group_chat(chat_id) {
            self.send_group(chat_id, robot_code, text, at_user_id).await
        } else {
            self.send_single(chat_id, robot_code, text).await
        }
    }

    /// 下载机器人接收到的文件/图片/语音（downloadCode → downloadUrl → 二进制）。
    /// 两步：POST /v1.0/robot/messageFiles/download 换 downloadUrl，再 GET 下载。
    pub async fn download_msg_file(
        &self,
        download_code: &str,
        robot_code: &str,
    ) -> Result<Vec<u8>> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .post(format!("{API_BASE}/v1.0/robot/messageFiles/download"))
            .header("x-acs-dingtalk-access-token", &token)
            .json(&json!({
                "downloadCode": download_code,
                "robotCode": robot_code,
            }))
            .send()
            .await
            .context("messageFiles/download 网络错误")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text));
        }
        let v: Value =
            serde_json::from_str(&text).context("messageFiles/download 响应不是 JSON")?;
        let url = v
            .get("downloadUrl")
            .and_then(|x| x.as_str())
            .or_else(|| {
                v.get("data")
                    .and_then(|d| d.get("downloadUrl"))
                    .and_then(|x| x.as_str())
            })
            .context("messageFiles/download 响应缺 downloadUrl")?
            .to_string();
        let resp2 = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("钉钉文件下载网络错误")?;
        if !resp2.status().is_success() {
            return Err(anyhow!("钉钉文件下载失败 HTTP {}", resp2.status().as_u16()));
        }
        Ok(resp2.bytes().await.context("读钉钉文件响应失败")?.to_vec())
    }

    /// 注册 Stream 长连接凭证（clientId/clientSecret 直接鉴权，无需先取 access_token）。
    /// 返回 (endpoint, ticket)；ticket 一次性、90s 有效。
    pub async fn open_connection(app_id: &str, app_secret: &str) -> Result<(String, String)> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let resp = http
            .post(format!("{API_BASE}/v1.0/gateway/connections/open"))
            .json(&json!({
                "clientId": app_id,
                "clientSecret": app_secret,
                "subscriptions": [
                    {"type": "CALLBACK", "topic": "/v1.0/im/bot/messages/get"}
                ],
                "ua": "agent-bridge/2.0.0",
            }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text));
        }
        let v: Value = serde_json::from_str(&text).context("connections/open 响应不是 JSON")?;
        let endpoint = v["endpoint"]
            .as_str()
            .context("connections/open 缺 endpoint")?
            .to_string();
        let ticket = v["ticket"]
            .as_str()
            .context("connections/open 缺 ticket")?
            .to_string();
        Ok((endpoint, ticket))
    }
}

/// 从 v1.0 OpenAPI 错误响应里抠 code/message（新接口错误体是 {"code","message"}，HTTP 非 2xx）。
fn api_error(status: u16, body: &str) -> anyhow::Error {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            let code = v.get("code").map(|c| c.to_string()).unwrap_or_default();
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if code.is_empty() && msg.is_empty() {
                None
            } else {
                Some(format!("code={code} msg={msg}"))
            }
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    anyhow!("钉钉 API 失败 (HTTP {status}): {detail}")
}

/// 群聊会话判定：钉钉群会话 ID 恒以 "cid" 开头；单聊我们用对方 staffId 当 chat_id。
pub fn is_group_chat(chat_id: &str) -> bool {
    chat_id.starts_with("cid")
}

/// 一条待下载的钉钉附件引用（picture/file/audio/video/富文本图片）。
#[derive(Debug, Clone)]
pub struct DingtalkAttachment {
    /// image | file | audio | video
    pub kind: String,
    pub download_code: String,
    pub file_name: String,
    /// 语音转写文本（audio 消息的 recognition），有则记进附件备注
    pub voice_text: String,
}

/// 一条钉钉入站机器人消息（已从 CALLBACK 帧解析）。
#[derive(Debug)]
pub struct DingtalkMessage {
    /// 消息 ID（去重键）。
    pub mid: String,
    /// 发送者 staffId（单聊时兼作 chat_id）。
    pub sender_staff_id: String,
    /// 会话 ID：群聊 = openConversationId（cid…）；单聊可能为空（用 sender 当 chat_id）。
    pub conversation_id: String,
    /// "1"=单聊 "2"=群聊。
    pub conversation_type: String,
    /// 文本内容（已 trim；群聊含 @机器人名 前缀，由 bridge 剥）。纯附件消息为空。
    pub text: String,
    /// 是否在 @ 名单里（群聊判据）。
    pub mentioned: bool,
    /// 回调里的 robotCode（下载附件必需；缺省回落到 app_id）。
    pub robot_code: String,
    /// 被引用消息的文本（引用/回复场景：`text.isReplyMsg` + `text.repliedMsg.content.text`；
    /// 非文本引用或未引用为空）。
    pub quoted_text: String,
    /// 被引用消息的附件引用（图片/文件/音视频；来自 repliedMsg.content，下载由桥负责）。
    pub quoted_attachments: Vec<DingtalkAttachment>,
    /// 附件引用列表（富文本可能多张图；文本消息为空）。
    pub attachments: Vec<DingtalkAttachment>,
}

impl DingtalkMessage {
    pub fn is_group(&self) -> bool {
        // conversationType 是权威判据（"1"=单聊 "2"=群聊）；缺失时才回落 cid 前缀启发式
        match self.conversation_type.as_str() {
            "2" => true,
            "1" => false,
            _ => is_group_chat(&self.conversation_id),
        }
    }

    /// 会话标识：群聊用 openConversationId；单聊用发送者 staffId。
    pub fn chat_id(&self) -> String {
        if self.is_group() {
            self.conversation_id.clone()
        } else {
            self.sender_staff_id.clone()
        }
    }
}

/// 解析引用/回复内容（`text.isReplyMsg` + `text.repliedMsg`）：
/// 返回 (被引用文本, 被引用附件引用)。非文本引用（图片/文件/音视频/富文本图片）
/// 也提取 downloadCode，由桥下载成附件元数据进 prompt。
fn parse_quoted_reply(p: &Value) -> (String, Vec<DingtalkAttachment>) {
    if !p["text"]["isReplyMsg"].as_bool().unwrap_or(false) {
        return (String::new(), Vec::new());
    }
    let replied = &p["text"]["repliedMsg"];
    let content = &replied["content"];
    let qtype = replied["msgType"].as_str().unwrap_or("");
    let mut text = content["text"].as_str().unwrap_or("").trim().to_string();
    let mut atts: Vec<DingtalkAttachment> = Vec::new();
    let push = |atts: &mut Vec<DingtalkAttachment>,
                kind: &str,
                dc: &str,
                file_name: String,
                voice_text: String| {
        if !dc.is_empty() {
            atts.push(DingtalkAttachment {
                kind: kind.into(),
                download_code: dc.into(),
                file_name,
                voice_text,
            });
        }
    };
    match qtype {
        "text" => {}
        "picture" => {
            push(
                &mut atts,
                "image",
                content["downloadCode"].as_str().unwrap_or(""),
                String::new(),
                String::new(),
            );
        }
        "file" => {
            push(
                &mut atts,
                "file",
                content["downloadCode"].as_str().unwrap_or(""),
                content["fileName"].as_str().unwrap_or("").to_string(),
                String::new(),
            );
        }
        "audio" | "voice" => {
            push(
                &mut atts,
                "audio",
                content["downloadCode"].as_str().unwrap_or(""),
                format!(
                    "voice.{}",
                    content["fileExtension"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("amr")
                ),
                String::new(),
            );
        }
        "video" => {
            push(
                &mut atts,
                "video",
                content["downloadCode"].as_str().unwrap_or(""),
                content["fileName"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("video.mp4")
                    .to_string(),
                String::new(),
            );
        }
        "richText" => {
            // 富文本引用：text 段落拼进 quoted_text，picture downloadCode 进附件列表
            let list: Vec<Value> = match content.get("richText") {
                Some(Value::Array(a)) => a.clone(),
                Some(Value::String(sv)) => serde_json::from_str(sv).unwrap_or_default(),
                _ => Vec::new(),
            };
            for c in list {
                match c.get("type").and_then(|x| x.as_str()) {
                    Some("text") => {
                        if let Some(t) = c["text"].as_str() {
                            let t = t.trim();
                            if !t.is_empty() {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                    }
                    Some("picture") => {
                        push(
                            &mut atts,
                            "image",
                            c["downloadCode"].as_str().unwrap_or(""),
                            String::new(),
                            String::new(),
                        );
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (text, atts)
}

/// 解析一条 CALLBACK 帧 → 钉钉消息。缺关键字段返回 None（外层照常 ack）。
/// 支持文本/图片/文件/语音/视频/富文本；纯附件消息 text 为空但带 attachments。
fn parse_message(frame: &Value) -> Option<DingtalkMessage> {
    if frame["type"].as_str() != Some("CALLBACK") {
        return None;
    }
    let data = frame["data"].as_str()?;
    let p: Value = serde_json::from_str(data).ok()?;
    let msgtype = p["msgtype"].as_str()?;
    let mid = p["msgId"].as_str()?.to_string();
    let sender = p["senderStaffId"]
        .as_str()
        .or_else(|| p["senderId"].as_str())?
        .to_string();
    // 引用/回复场景：钉钉文本消息带 isReplyMsg + repliedMsg（被引用消息内容）。
    let (quoted_text, quoted_attachments) = parse_quoted_reply(&p);

    let base = |text: String,
                quoted_text: String,
                quoted_attachments: Vec<DingtalkAttachment>,
                attachments: Vec<DingtalkAttachment>| DingtalkMessage {
        mid: mid.clone(),
        sender_staff_id: sender.clone(),
        conversation_id: p["conversationId"].as_str().unwrap_or("").to_string(),
        conversation_type: p["conversationType"].as_str().unwrap_or("").to_string(),
        text,
        mentioned: p["isInAtList"].as_bool().unwrap_or(false),
        robot_code: p["robotCode"].as_str().unwrap_or("").to_string(),
        quoted_text,
        quoted_attachments,
        attachments,
    };

    match msgtype {
        "text" => {
            let text = p["text"]["content"].as_str()?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some(base(
                text,
                quoted_text,
                quoted_attachments.clone(),
                Vec::new(),
            ))
        }
        "picture" => {
            let dc = p["content"]["downloadCode"].as_str()?.to_string();
            if dc.is_empty() {
                return None;
            }
            Some(base(
                String::new(),
                quoted_text,
                quoted_attachments.clone(),
                vec![DingtalkAttachment {
                    kind: "image".into(),
                    download_code: dc,
                    file_name: String::new(),
                    voice_text: String::new(),
                }],
            ))
        }
        "file" => {
            let dc = p["content"]["downloadCode"].as_str()?.to_string();
            let file_name = p["content"]["fileName"].as_str().unwrap_or("").to_string();
            if dc.is_empty() {
                return None;
            }
            Some(base(
                String::new(),
                quoted_text,
                quoted_attachments.clone(),
                vec![DingtalkAttachment {
                    kind: "file".into(),
                    download_code: dc,
                    file_name,
                    voice_text: String::new(),
                }],
            ))
        }
        "audio" | "voice" => {
            let dc = p["content"]["downloadCode"].as_str()?.to_string();
            let ext = p["content"]["fileExtension"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("amr");
            let file_name = format!("voice.{ext}");
            if dc.is_empty() {
                return None;
            }
            Some(base(
                String::new(),
                quoted_text,
                quoted_attachments.clone(),
                vec![DingtalkAttachment {
                    kind: "audio".into(),
                    download_code: dc,
                    file_name,
                    voice_text: p["content"]["recognition"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                }],
            ))
        }
        "video" => {
            let dc = p["content"]["downloadCode"].as_str()?.to_string();
            let file_name = p["content"]["fileName"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("video.mp4")
                .to_string();
            if dc.is_empty() {
                return None;
            }
            Some(base(
                String::new(),
                quoted_text,
                quoted_attachments.clone(),
                vec![DingtalkAttachment {
                    kind: "video".into(),
                    download_code: dc,
                    file_name,
                    voice_text: String::new(),
                }],
            ))
        }
        "richText" => {
            // content 可能是数组，也可能是 JSON 字符串（不同版本推送不一致，兼容两者）
            let list: Vec<Value> = match p.get("content") {
                Some(Value::Array(a)) => a.clone(),
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_default(),
                _ => Vec::new(),
            };
            let mut text = String::new();
            let mut attachments = Vec::new();
            for c in list {
                if let Some(t) = c.get("text").and_then(|x| x.as_str()) {
                    text.push_str(t);
                }
                if c.get("type").and_then(|x| x.as_str()) == Some("picture") {
                    if let Some(dc) = c.get("downloadCode").and_then(|x| x.as_str()) {
                        if !dc.is_empty() {
                            attachments.push(DingtalkAttachment {
                                kind: "image".into(),
                                download_code: dc.to_string(),
                                file_name: String::new(),
                                voice_text: String::new(),
                            });
                        }
                    }
                }
            }
            let text = text.trim().to_string();
            if text.is_empty() && attachments.is_empty() {
                return None;
            }
            Some(base(
                text,
                quoted_text,
                quoted_attachments.clone(),
                attachments,
            ))
        }
        _ => None,
    }
}

/// 构造 ack 帧（Text JSON）。data 是**已序列化**的 JSON 字符串（协议要求 data 字段为字符串）。
fn ack_json(message_id: &str, data: &str) -> String {
    json!({
        "code": 200,
        "headers": {"contentType": "application/json", "messageId": message_id},
        "message": "OK",
        "data": data,
    })
    .to_string()
}

/// query 参数百分号编码（ticket 可能含 base64 的 +/=）。只放行 URL 安全字符。
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Stream 长连接主循环：注册 → 连 wss → 收帧 → ack → 断线重连。stop 收到 true 时优雅退出。
pub async fn stream_loop(
    app_id: String,
    app_secret: String,
    bridge: Arc<Bridge>,
    mut stop: watch::Receiver<bool>,
) {
    let (key, kind, name) = (
        bridge.bot.key(),
        bridge.bot.kind.clone(),
        bridge.bot.bot_name.clone(),
    );
    let mut fails: u32 = 0;
    loop {
        if *stop.borrow() {
            return;
        }
        match run_conn(&app_id, &app_secret, &bridge, &mut stop).await {
            Ok(()) => return, // 只有 stop 才会 Ok
            Err(e) => {
                fails += 1;
                crate::botstatus::report(&key, &kind, &name, "重连中");
                crate::log!("[dingtalk] 连接断开: {e:#}（第 {fails} 次）");
            }
        }
        let wait = Duration::from_secs(3) + Duration::from_millis(fastrand::u64(0..=1000));
        crate::log!("[dingtalk] {}s 后重连…", wait.as_secs());
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.changed() => return,
        }
    }
}

/// 单次连接：注册 ticket → 连 wss → 帧处理循环。返回 Err 触发外层重连。
async fn run_conn(
    app_id: &str,
    app_secret: &str,
    bridge: &Arc<Bridge>,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    let (endpoint, ticket) = DingTalkClient::open_connection(app_id, app_secret).await?;
    let url = format!("{}?ticket={}", endpoint, percent_encode_query(&ticket));
    crate::log!("[dingtalk] 注册成功，连接 {endpoint}…");
    let (ws, _) = connect_async(&url).await.context("钉钉 ws connect 失败")?;
    let (mut sink, mut stream) = ws.split();
    crate::botstatus::report(
        &bridge.bot.key(),
        &bridge.bot.kind,
        &bridge.bot.bot_name,
        "在线",
    );
    crate::log!("[dingtalk] 已连接，监听 /v1.0/im/bot/messages/get");

    // 客户端 keepalive（#19）：平台空闲不推帧，须主动发 WebSocket Ping 换 Pong 续命。
    let mut last_rx = Instant::now();
    let mut watchdog = tokio::time::interval(Duration::from_secs(30));
    watchdog.tick().await; // 跳过第一拍
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.tick().await; // 跳过第一拍

    loop {
        tokio::select! {
            _ = stop.changed() => {
                crate::log!("[dingtalk] 收到停止信号，关闭连接");
                let _ = sink.send(Message::Close(None)).await;
                return Ok(());
            }
            _ = keepalive.tick() => {
                // 只发 Ping 不算「收到帧」：last_rx 仍只由入站刷新，
                // 半开连接发 Ping 无 Pong → 看门狗照常兜底（不把假活当在线）。
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
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
                crate::botstatus::report(
                    &bridge.bot.key(),
                    &bridge.bot.kind,
                    &bridge.bot.bot_name,
                    "在线",
                );
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
                                let ack = ack_json(mid, &json!({"opaque": opaque}).to_string());
                                sink.send(Message::Text(ack.into()))
                                    .await
                                    .context("回 ping ack 失败")?;
                            }
                            "SYSTEM" if topic == "disconnect" => {
                                crate::log!(
                                    "[dingtalk] 服务端 disconnect: {:?}",
                                    frame["data"]
                                );
                                // 协议：不响应，服务端约 10s 后断开；直接返回走重连
                                return Err(anyhow!("服务端主动断开（disconnect）"));
                            }
                            "CALLBACK" => {
                                // 先 ack（fire-forget 消息也按协议回，防服务端诊断误判），再异步处理
                                let ack = ack_json(mid, r#"{"response": null}"#);
                                sink.send(Message::Text(ack.into()))
                                    .await
                                    .context("回 callback ack 失败")?;
                                if topic == "/v1.0/im/bot/messages/get" {
                                    if let Some(msg) = parse_message(&frame) {
                                        let b = bridge.clone();
                                        tokio::spawn(async move { b.on_dingtalk(msg).await; });
                                    }
                                }
                            }
                            "EVENT" => {
                                let ack =
                                    ack_json(mid, r#"{"status":"SUCCESS","message":"success"}"#);
                                let _ = sink.send(Message::Text(ack.into())).await;
                            }
                            _ => {
                                crate::log!(
                                    "[dingtalk] 未处理的帧 type={ftype} topic={topic}"
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => { /* 协议只推文本帧 */ }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sink.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_group_message() {
        // 官方文档示例（机器人接收消息，Stream 模式）
        let frame = json!({
            "specVersion": "1.0",
            "type": "CALLBACK",
            "headers": {
                "appId": "1305d5f5-...",
                "contentType": "application/json",
                "messageId": "212ca9d7_974_1898c159aa6_1783b",
                "time": "1690362102194",
                "topic": "/v1.0/im/bot/messages/get"
            },
            "data": r#"{"conversationId":"cidAsXSBLnA==","atUsers":[{"dingtalkId":"$:LWCP_v1:$4*****"}],"chatbotCorpId":"ding9f*****","chatbotUserId":"$:LWCP_v1:$*****","msgId":"msgLICYe****HgY4JtMQw==","senderNick":"用户","isAdmin":true,"senderStaffId":"16650***698","sessionWebhookExpiredTime":1690367502152,"createAt":1690362101894,"senderCorpId":"ding9*****","conversationType":"2","senderId":"$:LWCP_v1:***jqTgIfhRX9Q==","conversationTitle":"测试群","isInAtList":true,"sessionWebhook":"https://oapi.dingtalk.com/robot/sendBySession?session=76da36b4**********8f59e8","text":{"content":" 测试数据"},"robotCode":"ding*****r3xc0b","msgtype":"text"}"#
        });
        let m = parse_message(&frame).expect("应解析出消息");
        assert_eq!(m.mid, "msgLICYe****HgY4JtMQw==");
        assert_eq!(m.sender_staff_id, "16650***698");
        assert_eq!(m.conversation_id, "cidAsXSBLnA==");
        assert_eq!(m.conversation_type, "2");
        assert_eq!(m.text, "测试数据");
        assert!(m.mentioned);
        assert!(m.is_group());
        assert_eq!(m.chat_id(), "cidAsXSBLnA==");
    }

    #[test]
    fn parse_quote_reply_text() {
        // 引用/回复：text.isReplyMsg=true + text.repliedMsg.content.text = 被引用内容
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m1"},
            "data": r#"{"conversationId":"cid1","conversationType":"1","msgId":"msg1","senderStaffId":"u123","isInAtList":false,"text":{"content":"这是回复","isReplyMsg":true,"repliedMsg":{"msgType":"text","content":{"text":"被引用的原消息"},"msgId":"msg0","createdAt":1690362101894}},"msgtype":"text"}"#
        });
        let m = parse_message(&frame).expect("应解析出消息");
        assert_eq!(m.text, "这是回复");
        assert_eq!(m.quoted_text, "被引用的原消息");

        // 未引用 → quoted_text 为空
        let plain = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m2"},
            "data": r#"{"conversationId":"cid1","conversationType":"1","msgId":"msg2","senderStaffId":"u123","isInAtList":false,"text":{"content":"普通消息"},"msgtype":"text"}"#
        });
        let m2 = parse_message(&plain).expect("应解析出消息");
        assert_eq!(m2.quoted_text, "");
    }

    #[test]
    fn parse_quote_reply_attachments() {
        // 引用图片：repliedMsg.content.downloadCode → quoted_attachments
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m1"},
            "data": r#"{"conversationId":"cid1","conversationType":"1","msgId":"msg1","senderStaffId":"u123","isInAtList":false,"text":{"content":"这是回复","isReplyMsg":true,"repliedMsg":{"msgType":"picture","content":{"downloadCode":"dc_pic"},"msgId":"msg0"}},"msgtype":"text"}"#
        });
        let m = parse_message(&frame).expect("应解析出消息");
        assert_eq!(m.text, "这是回复");
        assert_eq!(m.quoted_text, "");
        assert_eq!(m.quoted_attachments.len(), 1);
        assert_eq!(m.quoted_attachments[0].kind, "image");
        assert_eq!(m.quoted_attachments[0].download_code, "dc_pic");

        // 引用富文本（含文本 + 图片）：文本进 quoted_text，图片进 quoted_attachments
        let frame2 = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m2"},
            "data": r#"{"conversationId":"cid1","conversationType":"1","msgId":"msg2","senderStaffId":"u123","isInAtList":false,"text":{"content":"回复2","isReplyMsg":true,"repliedMsg":{"msgType":"richText","content":{"richText":[{"type":"text","text":"引用正文"},{"type":"picture","downloadCode":"dc_rich"}]},"msgId":"msg0"}},"msgtype":"text"}"#
        });
        let m2 = parse_message(&frame2).expect("应解析出消息");
        assert_eq!(m2.quoted_text, "引用正文");
        assert_eq!(m2.quoted_attachments.len(), 1);
        assert_eq!(m2.quoted_attachments[0].kind, "image");
        assert_eq!(m2.quoted_attachments[0].download_code, "dc_rich");
    }

    #[test]
    fn parse_single_chat() {
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m1"},
            "data": r#"{"conversationId":"cid1","conversationType":"1","msgId":"msg1","senderStaffId":"u123","isInAtList":false,"text":{"content":"你好"},"msgtype":"text"}"#
        });
        let m = parse_message(&frame).unwrap();
        assert!(!m.is_group());
        // 单聊 chat_id = senderStaffId
        assert_eq!(m.chat_id(), "u123");
    }

    #[test]
    fn parse_picture_message() {
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m1"},
            "data": r#"{"msgId":"msg1","senderStaffId":"u1","robotCode":"ding123","msgtype":"picture","content":{"downloadCode":"dc1"}}"#
        });
        let m = parse_message(&frame).expect("图片消息应解析");
        assert_eq!(m.text, "");
        assert_eq!(m.robot_code, "ding123");
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].kind, "image");
        assert_eq!(m.attachments[0].download_code, "dc1");
    }

    #[test]
    fn parse_file_and_audio() {
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m1"},
            "data": r#"{"msgId":"msg1","senderStaffId":"u1","msgtype":"file","content":{"downloadCode":"dcf","fileName":"合同.pdf"}}"#
        });
        let m = parse_message(&frame).unwrap();
        assert_eq!(m.attachments[0].kind, "file");
        assert_eq!(m.attachments[0].file_name, "合同.pdf");

        let frame2 = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m2"},
            "data": r#"{"msgId":"msg2","senderStaffId":"u1","msgtype":"audio","content":{"downloadCode":"dca","recognition":"好的"}}"#
        });
        let m2 = parse_message(&frame2).unwrap();
        assert_eq!(m2.attachments[0].kind, "audio");
        assert_eq!(m2.attachments[0].voice_text, "好的");
    }

    #[test]
    fn parse_rich_text_with_picture() {
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m1"},
            "data": r#"{"msgId":"msg1","senderStaffId":"u1","msgtype":"richText","content":[{"text":"看图"},{"type":"picture","downloadCode":"dcp"}]}"#
        });
        let m = parse_message(&frame).unwrap();
        assert_eq!(m.text, "看图");
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].kind, "image");
        assert_eq!(m.attachments[0].download_code, "dcp");
    }

    #[test]
    fn parse_ignores_empty_text() {
        // 空文本仍丢弃
        let frame = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m1"},
            "data": r#"{"msgId":"msg1","senderStaffId":"u1","msgtype":"text","text":{"content":"   "}}"#
        });
        assert!(parse_message(&frame).is_none());
        // 未知 msgtype 丢弃
        let frame2 = json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "m1"},
            "data": r#"{"msgId":"msg1","senderStaffId":"u1","msgtype":"unknown"}"#
        });
        assert!(parse_message(&frame2).is_none());
    }

    #[test]
    fn ack_echoes_message_id() {
        let ack = ack_json("abc_123", r#"{"response": null}"#);
        let v: Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(v["code"], 200);
        assert_eq!(v["headers"]["messageId"], "abc_123");
        assert_eq!(v["data"], r#"{"response": null}"#);
    }

    #[test]
    fn group_chat_detection() {
        assert!(is_group_chat("cidAsXSBLnA=="));
        assert!(!is_group_chat("16650***698"));
        assert!(!is_group_chat(""));
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(
            percent_encode_query("7724109a-ea43-4aa2-b803-87d82c5aaee6"),
            "7724109a-ea43-4aa2-b803-87d82c5aaee6"
        );
        assert_eq!(percent_encode_query("a+b/c=="), "a%2Bb%2Fc%3D%3D");
    }
}
