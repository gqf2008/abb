//! 微信通道 —— 腾讯官方 ilink 协议（参考 ~/.openclaw/extensions/openclaw-weixin）。
//! QR 扫码登录 → bot_token；之后 HTTP JSON 长轮询 getupdates 收消息、sendmessage 发消息。
//! 端点：`{baseurl}/ilink/bot/{get_bot_qrcode,get_qrcode_status,getupdates,sendmessage}`。
//! 收：文本 + 图片/语音/文件/视频（入站媒体经 CDN 下载 + AES-128-ECB 解密）。
//! 发：文本 + 媒体（`send_media`：getuploadurl → AES-128-ECB 加密 → CDN → sendmessage）。
//! **音频按文件发**——VOICE 类型实测被官方丢弃（message_id 成功但端上不显示）。
//! 跨平台（reqwest rustls）。

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// 登录固定网关（扫码阶段连这里；登录后多数情况仍用它，除非响应带 redirect_host/baseurl）。
pub const FIXED_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
/// 媒体 CDN 根地址（openclaw-weixin 默认；可通过 wx_cdn_base_url 配置覆盖）。
pub const DEFAULT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const BOT_TYPE: &str = "3";
const ILINK_APP_ID: &str = "bot";
const ILINK_APP_CLIENT_VERSION: &str = "20301"; // 对应 2.3.1

// ── base64（标准字母表，含 padding；项目原则零依赖，手写不引 crate）──
mod b64 {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                A[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                A[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    pub fn decode(s: &str) -> Option<Vec<u8>> {
        let bytes: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
        let mut out = Vec::new();
        for chunk in bytes.chunks(4) {
            if chunk.len() < 2 {
                break;
            }
            let v0 = val(chunk[0])?;
            let v1 = val(chunk[1])?;
            let v2 = if chunk.len() > 2 && chunk[2] != b'=' {
                val(chunk[2])?
            } else {
                0
            };
            let v3 = if chunk.len() > 3 && chunk[3] != b'=' {
                val(chunk[3])?
            } else {
                0
            };
            let n = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
            out.push((n >> 16) as u8);
            if chunk.len() > 2 && chunk[2] != b'=' {
                out.push((n >> 8) as u8);
            }
            if chunk.len() > 3 && chunk[3] != b'=' {
                out.push(n as u8);
            }
        }
        Some(out)
    }
}

/// X-WECHAT-UIN：随机 uint32 → 十进制字符串 → base64。
fn random_uin() -> String {
    let n = fastrand::u32(..);
    b64::encode(n.to_string().as_bytes())
}

/// base_info：每个请求都带。
fn base_info() -> serde_json::Value {
    serde_json::json!({
        "channel_version": "2.3.1",
        "bot_agent": "agent-bridge/2.0.0",
    })
}

/// 微信通道错误（类型化，调用方按种类处理，不靠字符串匹配）。
#[derive(Debug)]
pub enum WxError {
    /// ilink 会话超时（errcode -14），需重新扫码登录。
    SessionExpired,
    /// 客户端长轮询超时。**不等于「成功空轮询」**——连续多次超时是通道假死（半开 TCP）的信号，
    /// 调用方须计数并降级状态，不能当作正常在线续命。
    PollTimeout,
    /// 其它网络/协议错误。
    Other(anyhow::Error),
}

impl std::fmt::Display for WxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WxError::SessionExpired => write!(f, "ilink 会话超时（errcode -14），需重新扫码登录"),
            WxError::PollTimeout => write!(f, "长轮询客户端超时"),
            WxError::Other(e) => write!(f, "{e:#}"),
        }
    }
}
impl std::error::Error for WxError {}

#[derive(Debug, Clone)]
pub struct WeixinClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    /// 媒体 CDN 根地址（默认 https://novac2c.cdn.weixin.qq.com/c2c）。
    /// 入站图片/语音/文件通过它拼下载 URL + AES-128-ECB 解密（对齐 openclaw-weixin）。
    cdn_base_url: String,
}

/// message_id 服务器有时返回整数、有时返回字符串（实测真实消息里是整数 7490...）。
/// 这里两种都收，统一存成 String（下游当去重键用，需要 .is_empty() 等）。
fn string_or_int<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    use serde::de;
    struct V;
    impl<'de> de::Visitor<'de> for V {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "string or integer")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<String, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
    }
    d.deserialize_any(V)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WeixinMessage {
    #[serde(default)]
    pub from_user_id: String,
    #[serde(default)]
    pub to_user_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default, deserialize_with = "string_or_int")]
    pub message_id: String,
    #[serde(default)]
    pub create_time_ms: i64,
    #[serde(default)]
    pub context_token: String,
    #[serde(default)]
    pub message_type: i64,
    #[serde(default)]
    pub item_list: Vec<MessageItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageItem {
    #[serde(default, rename = "type")]
    pub item_type: i64,
    /// 引用/回复的原消息（ilink 协议：用户引用某条消息时，本条 item 带 ref_msg，
    /// 内含原消息摘要 title + 原消息内容 message_item）。
    #[serde(default)]
    pub ref_msg: Option<RefMessage>,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub image_item: Option<ImageItem>,
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,
    #[serde(default)]
    pub file_item: Option<FileItem>,
    #[serde(default)]
    pub video_item: Option<VideoItem>,
}

/// 微信引用消息（quote/reply）：title 是摘要，message_item 是被引用消息的完整内容
/// （可为文本/图片/文件等，与入站 item 同构）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RefMessage {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub message_item: Option<Box<MessageItem>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: String,
}

/// CDN 媒体引用（对齐 openclaw-weixin src/api/types.ts 的 CDNMedia）。
/// aes_key 是 base64 字符串；图片优先用 ImageItem.aeskey（hex），文件/语音/视频用 media.aes_key。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CDNMedia {
    #[serde(default)]
    pub encrypt_query_param: String,
    #[serde(default)]
    pub aes_key: String,
    #[serde(default)]
    pub encrypt_type: Option<i64>,
    /// 完整下载 URL（服务端直接返回时优先用它，无需拼接 CDN）。
    #[serde(default)]
    pub full_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageItem {
    #[serde(default)]
    pub media: Option<CDNMedia>,
    /// Raw AES-128 key，hex 字符串（16 字节）；入站解密优先于 media.aes_key。
    #[serde(default)]
    pub aeskey: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceItem {
    #[serde(default)]
    pub media: Option<CDNMedia>,
    /// 语音转文字内容（有则 agent 直接用，不必解音频）。
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileItem {
    #[serde(default)]
    pub media: Option<CDNMedia>,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub md5: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoItem {
    #[serde(default)]
    pub media: Option<CDNMedia>,
    #[serde(default)]
    pub thumb_media: Option<CDNMedia>,
}

/// 从 MessageItem 提取出的 CDN 媒体引用（跨模块给 messenger 下载用，避免携带整个 item）。
#[derive(Debug, Clone, Default)]
pub struct WechatMedia {
    /// image | audio | video | file
    pub kind: String,
    pub file_name: String,
    pub encrypt_query_param: String,
    /// CDNMedia.aes_key（base64）
    pub aes_key: String,
    /// ImageItem.aeskey（hex 字符串，图片解密优先）
    pub aeskey_hex: String,
    pub full_url: String,
    /// 语音转写文本（voice_item.text），有则记进附件备注
    pub voice_text: String,
}

impl MessageItem {
    /// 提取媒体引用（图片/语音/文件/视频；无则 None）。
    /// 图片无 aes 时允许明文下载；语音/文件/视频必须带 media.aes_key 才能解密。
    pub fn media(&self) -> Option<WechatMedia> {
        if self.item_type == 2 {
            let img = self.image_item.as_ref()?;
            let empty = CDNMedia::default();
            let m = img.media.as_ref().unwrap_or(&empty);
            Some(WechatMedia {
                kind: "image".into(),
                file_name: String::new(),
                encrypt_query_param: m.encrypt_query_param.clone(),
                aes_key: m.aes_key.clone(),
                aeskey_hex: img.aeskey.clone(),
                full_url: m.full_url.clone(),
                voice_text: String::new(),
            })
        } else if self.item_type == 3 {
            let v = self.voice_item.as_ref()?;
            let m = v.media.as_ref()?;
            Some(WechatMedia {
                kind: "audio".into(),
                file_name: String::new(),
                encrypt_query_param: m.encrypt_query_param.clone(),
                aes_key: m.aes_key.clone(),
                aeskey_hex: String::new(),
                full_url: m.full_url.clone(),
                voice_text: v.text.clone(),
            })
        } else if self.item_type == 4 {
            let f = self.file_item.as_ref()?;
            let m = f.media.as_ref()?;
            Some(WechatMedia {
                kind: "file".into(),
                file_name: f.file_name.clone(),
                encrypt_query_param: m.encrypt_query_param.clone(),
                aes_key: m.aes_key.clone(),
                aeskey_hex: String::new(),
                full_url: m.full_url.clone(),
                voice_text: String::new(),
            })
        } else if self.item_type == 5 {
            let v = self.video_item.as_ref()?;
            let m = v.media.as_ref()?;
            Some(WechatMedia {
                kind: "video".into(),
                file_name: String::new(),
                encrypt_query_param: m.encrypt_query_param.clone(),
                aes_key: m.aes_key.clone(),
                aeskey_hex: String::new(),
                full_url: m.full_url.clone(),
                voice_text: String::new(),
            })
        } else {
            None
        }
    }
}

impl WeixinMessage {
    /// 提取首条文本内容（无则空串）。
    pub fn text(&self) -> String {
        self.item_list
            .iter()
            .find(|it| it.item_type == 1)
            .and_then(|it| it.text_item.as_ref())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    /// 提取全部媒体引用（含文本消息旁的图片等）。
    pub fn media_items(&self) -> Vec<WechatMedia> {
        self.item_list.iter().filter_map(|it| it.media()).collect()
    }

    /// 提取被引用消息的媒体附件（图片/文件/音视频；引用/回复场景）。
    /// ref_msg.message_item 与入站 item 同构，复用 media() 提取 CDN 引用，
    /// 由桥下载成附件元数据进 prompt。
    pub fn quoted_media(&self) -> Vec<WechatMedia> {
        self.item_list
            .iter()
            .filter_map(|it| it.ref_msg.as_ref())
            .filter_map(|r| r.message_item.as_ref())
            .filter_map(|mi| mi.media())
            .collect()
    }

    /// 提取被引用消息的文本（引用/回复场景）。协议里 ref_msg 带 title 摘要 +
    /// message_item 原内容（原内容可为文本/媒体/再嵌套引用）；这里拼成
    /// `摘要 | 原文本`，供 agent 读到「上面被引用的消息内容」。
    pub fn quoted_text(&self) -> String {
        for it in &self.item_list {
            if let Some(ref_msg) = &it.ref_msg {
                let mut parts: Vec<String> = Vec::new();
                if !ref_msg.title.trim().is_empty() {
                    parts.push(ref_msg.title.trim().to_string());
                }
                if let Some(mi) = &ref_msg.message_item {
                    if let Some(t) = &mi.text_item {
                        if !t.text.trim().is_empty() {
                            parts.push(t.text.trim().to_string());
                        }
                    }
                    // 原消息本身也是引用 → 递归带出最底层内容
                    if let Some(nested) = quoted_text_of_item(mi) {
                        parts.push(nested);
                    }
                }
                if !parts.is_empty() {
                    return parts.join(" | ");
                }
            }
        }
        String::new()
    }
}

/// 递归提取某条 MessageItem 自身携带的引用文本（ref_msg.message_item 可能再嵌套）。
fn quoted_text_of_item(item: &MessageItem) -> Option<String> {
    let ref_msg = item.ref_msg.as_ref()?;
    let mut parts: Vec<String> = Vec::new();
    if !ref_msg.title.trim().is_empty() {
        parts.push(ref_msg.title.trim().to_string());
    }
    if let Some(mi) = &ref_msg.message_item {
        if let Some(t) = &mi.text_item {
            if !t.text.trim().is_empty() {
                parts.push(t.text.trim().to_string());
            }
        }
        if let Some(nested) = quoted_text_of_item(mi) {
            parts.push(nested);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" | "))
}

#[derive(Debug, Deserialize)]
struct GetUpdatesResp {
    #[serde(default)]
    errcode: Option<i64>,
    #[serde(default)]
    msgs: Vec<WeixinMessage>,
    #[serde(default)]
    get_updates_buf: String,
    #[serde(default)]
    longpolling_timeout_ms: Option<u64>,
}

/// 外发媒体的种类：决定 `media_type`（上传）与 item `type`（消息体）。
///
/// 注意 item type 与 media_type **不是同一张表**：图片 item=2/media=1、视频 item=5/media=2、
/// 文件 item=4/media=3（协议 Message Types 节）。语音（item=3/media=4）刻意不提供——
/// 实测被官方丢弃，音频按文件发（见 `send_media` 注释）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundMediaKind {
    Image,
    Video,
    File,
}

impl OutboundMediaKind {
    /// `getuploadurl` 的 `media_type`。
    pub fn media_type(self) -> i64 {
        match self {
            OutboundMediaKind::Image => 1,
            OutboundMediaKind::Video => 2,
            OutboundMediaKind::File => 3,
        }
    }

    /// `item_list[].type`。
    pub fn item_type(self) -> i64 {
        match self {
            OutboundMediaKind::Image => 2,
            OutboundMediaKind::Video => 5,
            OutboundMediaKind::File => 4,
        }
    }
}

/// 一次媒体上传的结果（拼 CDNMedia 与 item 尺寸字段所需的全部事实）。
struct UploadedMedia {
    /// CDN 下载参数（`CDNMedia.encrypt_query_param`）。
    download_param: String,
    /// `CDNMedia.aes_key`：图片=base64(原始 16 字节)，其余=base64(32 hex)。
    aes_key_b64: String,
    /// 原始 16 字节密钥的 hex（图片的 `image_item.aeskey` 用）。
    aeskey_hex: String,
    /// 加密后字节数（图片 mid_size / 视频 video_size）。
    cipher_size: u64,
}

impl WeixinClient {
    pub fn new(base_url: &str, token: &str, cdn_base_url: &str) -> WeixinClient {
        // 单 client 复用连接池/TLS；长轮询用每请求超时覆盖默认总超时。
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        WeixinClient {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            cdn_base_url: cdn_base_url.trim_end_matches('/').to_string(),
        }
    }

    /// 带 ilink 认证头的 POST 构建器（6 个头一处维护）。
    fn authed(&self, url: &str) -> reqwest::RequestBuilder {
        self.http
            .post(url)
            .header("Content-Type", "application/json")
            .header("AuthorizationType", "ilink_bot_token")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("X-WECHAT-UIN", random_uin())
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("iLink-App-ClientVersion", ILINK_APP_CLIENT_VERSION)
    }

    async fn post(&self, endpoint: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let resp = self
            .authed(&url)
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {endpoint} 网络错误"))?;
        // 非 2xx 先把状态码与响应体带出来：网关 401/502 回 HTML 时，直接 `.json()`
        // 会把它们塌成「响应非 JSON」，状态码与原因全丢（#254 在飞书侧修过同款）。
        let status = resp.status();
        let text = resp.text().await.context("读响应体失败")?;
        if !status.is_success() {
            anyhow::bail!(
                "POST {endpoint} HTTP {status}: {}",
                crate::agent::truncate(&text, 200)
            );
        }
        serde_json::from_str(&text).with_context(|| format!("POST {endpoint} 响应非 JSON"))
    }

    /// 长轮询取新消息。返回 (消息列表, 新游标, 建议下次超时ms)。
    /// 会话超时（errcode -14）返回 WxError::SessionExpired；客户端超时返回 PollTimeout（由调用方计数判假死）。
    pub async fn get_updates(
        &self,
        cursor: &str,
        timeout_ms: u64,
    ) -> std::result::Result<(Vec<WeixinMessage>, String, u64), WxError> {
        let body = serde_json::json!({
            "get_updates_buf": cursor,
            "base_info": base_info(),
        });
        let url = format!("{}/ilink/bot/getupdates", self.base_url);
        let resp = self
            .authed(&url)
            .timeout(std::time::Duration::from_millis(timeout_ms + 10_000)) // 比服务端长轮询略长
            .json(&body)
            .send()
            .await;
        let r: GetUpdatesResp = match resp {
            Ok(r) => {
                // 先取文本再反序列化：reqwest 的 .json() 在失败时只报「error decoding response body」，
                // 把 serde 的具体原因和响应内容吞了，没法排查。这里带上 status + 响应预览。
                let status = r.status();
                let txt = r.text().await.unwrap_or_default();
                if txt.is_empty() {
                    // 200 + 空体（部分错误路径）：当空轮询处理，不刷错误日志。
                    return Ok((vec![], cursor.to_string(), timeout_ms));
                }
                let parsed: GetUpdatesResp = serde_json::from_str(&txt).map_err(|e| {
                    WxError::Other(anyhow!(
                        "getupdates 解析失败(status={status}, {e}) 响应预览: {}",
                        crate::agent::truncate(&txt, 400)
                    ))
                })?;
                parsed
            }
            Err(e) => {
                // 客户端超时单独类型化：偶发是长轮询常态，但**连续**超时 = 通道假死信号。
                // 不能混进 Ok（成功空轮询）——那样看门逻辑会拿它续命「在线」（半开假绿）。
                if e.is_timeout() {
                    return Err(WxError::PollTimeout);
                }
                return Err(WxError::Other(e.into()));
            }
        };
        if r.errcode == Some(-14) {
            return Err(WxError::SessionExpired);
        }
        // 其它非零 errcode 也是失败，别当成功空轮询（否则错误被吞、循环可能空转）
        if let Some(code) = r.errcode {
            if code != 0 {
                return Err(WxError::Other(anyhow!("getupdates errcode={code}")));
            }
        }
        let next_cursor = if r.get_updates_buf.is_empty() {
            cursor.to_string()
        } else {
            r.get_updates_buf
        };
        let next_timeout = r.longpolling_timeout_ms.unwrap_or(timeout_ms);
        Ok((r.msgs, next_cursor, next_timeout))
    }

    /// 发文本消息给指定用户（context_token 从入站消息带上）。
    pub async fn send_text(&self, to_user_id: &str, context_token: &str, text: &str) -> Result<()> {
        self.send_items(
            to_user_id,
            context_token,
            vec![serde_json::json!({ "type": 1, "text_item": { "text": text } })],
        )
        .await
    }

    /// 发一条 item_list 消息（文本/媒体共用信封）。
    ///
    /// 协议（github epiral/weixin-bot PROTOCOL.md）要求的外发 msg 字段，缺一会被服务器
    /// 「先 ack（给 message_id）再丢弃」——消息不投递。message_state:2=FINISH 尤其关键，
    /// 缺了被当「生成中」不渲染；from_user_id bot 外发留空；client_id 每条唯一；
    /// **context_token 是必填**（须回显入站消息带上来的那个）。
    async fn send_items(
        &self,
        to_user_id: &str,
        context_token: &str,
        item_list: Vec<serde_json::Value>,
    ) -> Result<()> {
        let client_id = format!(
            "fb_{}_{}",
            crate::chrono_lite::unix_secs(),
            fastrand::u64(..)
        );
        let body = serde_json::json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to_user_id,
                "client_id": client_id,
                "message_type": 2,            // BOT
                "message_state": 2,           // FINISH（缺这个 = 不投递）
                "context_token": context_token,
                "item_list": item_list,
            },
            "base_info": base_info(),
        });
        let v = self.post("ilink/bot/sendmessage", &body).await?;
        if v.get("ret").and_then(|r| r.as_i64()).unwrap_or(0) != 0 {
            return Err(anyhow!("sendmessage 失败: {v}"));
        }
        Ok(())
    }

    /// 发一条媒体消息（图片/视频/文件）。
    ///
    /// **音频走 File**：iLink 的 VOICE 类型（media_type=4 / item type=3）实测被官方丢弃
    /// ——`message_id` 返回成功但微信端不显示（2026-08-23 实测，见 skill `wx-send`）。
    /// 所以 `--voice` 那条路不用，音频按文件发（微信以文件消息展示，能点开）。
    ///
    /// 目标可以是**任意微信会话**：`to_user_id` 由调用方给，`context_token` 用该会话
    /// 入站消息带上来的那个（与外发文本同一条要求）。
    pub async fn send_media(
        &self,
        to_user_id: &str,
        context_token: &str,
        kind: OutboundMediaKind,
        file_name: &str,
        data: &[u8],
    ) -> Result<()> {
        let up = self.upload_media(to_user_id, kind, data).await?;
        // CDNMedia：下载参数 + 密钥（编码按类型，见 upload_media）。
        let cdn_media = serde_json::json!({
            "encrypt_query_param": up.download_param,
            "aes_key": up.aes_key_b64,
            "encrypt_type": 1,
        });
        let item = match kind {
            OutboundMediaKind::Image => serde_json::json!({
                "type": kind.item_type(),
                "image_item": {
                    "media": cdn_media.clone(),
                    // 图片密钥的两种表示都放上：`media.aes_key` 是 base64(原始 16 字节)
                    // （协议对图片的规定），`aeskey` 是 hex。入站图片也同时带这两个字段
                    // （见 parse_aes_key_from_b64 的两种容忍分支），接收端取哪个都能解。
                    "aeskey": up.aeskey_hex,
                    "mid_size": up.cipher_size,
                }
            }),
            OutboundMediaKind::Video => serde_json::json!({
                "type": kind.item_type(),
                "video_item": { "media": cdn_media.clone(), "video_size": up.cipher_size }
            }),
            OutboundMediaKind::File => serde_json::json!({
                "type": kind.item_type(),
                "file_item": {
                    "media": cdn_media.clone(),
                    "file_name": file_name,
                    "len": data.len().to_string(),
                }
            }),
        };
        self.send_items(to_user_id, context_token, vec![item]).await
    }

    /// 上传媒体到 CDN，返回拼消息所需的全部事实。
    ///
    /// 链路（PROTOCOL.md「Get Upload URL」+「CDN Upload」）：
    /// `getuploadurl`（带 rawsize/rawfilemd5/filesize/aeskey/no_need_thumb）
    /// → AES-128-ECB(PKCS7) 加密 → POST 密文到 CDN → 响应头 `x-encrypted-param`
    /// 即下载参数。
    async fn upload_media(
        &self,
        to_user_id: &str,
        kind: OutboundMediaKind,
        data: &[u8],
    ) -> Result<UploadedMedia> {
        if data.is_empty() {
            anyhow::bail!("拒绝上传空媒体");
        }
        let rawsize = data.len();
        // PKCS7 **恒补**：明文是 16 的倍数时补一个整块，故密文 = (rawsize/16 + 1) * 16。
        // 别用 `div_ceil(rawsize,16)*16` —— 整除输入会少报一整个块（审查 P2；wx-send 的
        // `((rawsize+15)//16)*16` 正是这个偏差，它大概没拿 16 倍数大小的文件测过）。
        // 下面加密完还有 debug_assert 锁住「预测 == 实际」。
        let cipher_size = (rawsize / 16 + 1) * 16;
        // 密钥是机密：用 CSPRNG（uuid v4 由 getrandom 提供）而不是通用 PRNG。
        let aeskey: [u8; 16] = *uuid::Uuid::new_v4().as_bytes();
        let aeskey_hex: String = aeskey.iter().map(|b| format!("{b:02x}")).collect();
        // 图片与文件/视频/语音的 aes_key 编码不同（协议「Crypto」节）：
        // 图片=base64(原始 16 字节)；其余=base64(32 个 hex 字符)。
        let aes_key_b64 = match kind {
            OutboundMediaKind::Image => b64::encode(&aeskey),
            _ => b64::encode(aeskey_hex.as_bytes()),
        };
        let filekey: String = (0..32)
            .map(|_| format!("{:x}", fastrand::u8(..16)))
            .collect();
        let body = serde_json::json!({
            "filekey": filekey,
            "media_type": kind.media_type(),
            "to_user_id": to_user_id,
            "rawsize": rawsize,
            "rawfilemd5": format!("{:x}", md5::compute(data)),
            "filesize": cipher_size,
            "no_need_thumb": true,
            "aeskey": aeskey_hex,
            "base_info": base_info(),
        });
        let up = self.post("ilink/bot/getuploadurl", &body).await?;
        // 两种响应形态都要认：实测（wx-send）返回 `upload_full_url` 直给完整地址；
        // PROTOCOL.md 记的是 `upload_param` + 由客户端拼 CDN 地址。优先前者。
        let cdn_url = up
            .get("upload_full_url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                up.get("upload_param")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|p| {
                        format!(
                            "{}/upload?encrypted_query_param={}&filekey={filekey}",
                            self.cdn_base_url,
                            percent_encode_query(p)
                        )
                    })
            })
            .ok_or_else(|| {
                // 只报关键字段：`up` 里可能有短期签名的上传地址，别进日志、更别随
                // deliver 的回源提示发给聊天里的真人（审查 P3-3）。
                anyhow!(
                    "getuploadurl 未返回 upload_full_url/upload_param（ret={:?} errcode={:?} errmsg={:?}）",
                    up.get("ret"),
                    up.get("errcode"),
                    up.get("errmsg")
                )
            })?;
        let cipher = aes_ecb_encrypt(data, &aeskey);
        debug_assert_eq!(
            cipher.len(),
            cipher_size,
            "filesize 预测必须等于实际密文长度"
        );
        let resp = self
            .http
            .post(&cdn_url)
            .header("Content-Type", "application/octet-stream")
            // 大文件上传用更长超时（默认 60s 对慢网偏紧）；上传是整块一次 POST。
            .timeout(std::time::Duration::from_secs(300))
            .body(cipher)
            .send()
            .await
            .context("CDN 上传网络错误")?;
        let status = resp.status();
        let download_param = resp
            .headers()
            .get("x-encrypted-param")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("CDN 上传失败 HTTP {status}：响应缺 x-encrypted-param"))?;
        Ok(UploadedMedia {
            download_param,
            aes_key_b64,
            aeskey_hex,
            cipher_size: cipher_size as u64,
        })
    }

    /// 下载并解密一条 CDN 媒体（图片/语音/文件/视频），返回 (明文字节, 猜测 mime, 文件名, 备注)。
    /// 下载失败返回 Err（由调用方转成附件「下载失败」备注，不丢消息）；非媒体 item 返回 Ok(None)。
    pub async fn download_media(
        &self,
        media: &WechatMedia,
    ) -> Result<Option<(Vec<u8>, String, String, String)>> {
        // 既无 encrypt_query_param 也无 full_url → 无可下载内容
        if media.encrypt_query_param.is_empty() && media.full_url.is_empty() {
            return Ok(None);
        }
        let url = if !media.full_url.is_empty() {
            media.full_url.clone()
        } else if self.cdn_base_url.is_empty() {
            anyhow::bail!("微信 CDN 未配置（wx_cdn_base_url 为空）且无 full_url");
        } else {
            format!(
                "{}/download?encrypted_query_param={}",
                self.cdn_base_url,
                percent_encode_query(&media.encrypt_query_param)
            )
        };
        let resp = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .with_context(|| format!("微信 CDN 下载网络错误 kind={}", media.kind))?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "微信 CDN 下载失败 HTTP {} kind={}",
                resp.status().as_u16(),
                media.kind
            );
        }
        let encrypted = resp.bytes().await.context("微信 CDN 读响应失败")?;

        // 图片：优先 ImageItem.aeskey（hex）→ media.aes_key（base64）；两者都无 = 明文 CDN。
        // 语音/文件/视频：必须有 media.aes_key 才能解密。
        let key = if !media.aeskey_hex.is_empty() {
            Some(parse_aes_key_from_hex(&media.aeskey_hex)?)
        } else if !media.aes_key.is_empty() {
            Some(parse_aes_key_from_b64(&media.aes_key)?)
        } else if media.kind == "image" {
            None
        } else {
            anyhow::bail!("{} 媒体缺少 aes_key，无法解密", media.kind);
        };

        let bytes = match key {
            Some(k) => aes_ecb_decrypt(&encrypted, &k)?,
            None => encrypted.to_vec(),
        };
        let mime = crate::attachments::mime_from_name(&media.file_name, &media.kind);
        let note = if media.voice_text.is_empty() {
            String::new()
        } else {
            format!("语音转写={}", media.voice_text)
        };
        Ok(Some((bytes, mime, media.file_name.clone(), note)))
    }
}

// ── CDN AES-128-ECB 解密（对齐 openclaw-weixin src/cdn）──

/// 从 base64 的 aes_key 还原 16 字节密钥。两种形态：
///   - base64(原始 16 字节)          → 图片（media.aes_key）
///   - base64(16 字节的 hex 字符串)  → 文件/语音/视频（media.aes_key）
fn parse_aes_key_from_b64(aes_key_b64: &str) -> Result<[u8; 16]> {
    let decoded = b64::decode(aes_key_b64).context("aes_key base64 解码失败")?;
    if decoded.len() == 16 {
        let mut key = [0u8; 16];
        key.copy_from_slice(&decoded);
        return Ok(key);
    }
    if decoded.len() == 32 {
        if let Ok(hex) = std::str::from_utf8(&decoded) {
            if hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return parse_aes_key_from_hex(hex);
            }
        }
    }
    anyhow::bail!(
        "aes_key 必须是 16 原始字节或 32 hex 字符的 base64，实际 {} 字节",
        decoded.len()
    )
}

/// 从 hex 字符串还原 16 字节密钥（ImageItem.aeskey 形态）。
fn parse_aes_key_from_hex(hex: &str) -> Result<[u8; 16]> {
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!(
            "aeskey hex 必须是 32 个 hex 字符，实际 {:?}",
            &hex[..hex.len().min(40)]
        );
    }
    let mut key = [0u8; 16];
    for (i, item) in key.iter_mut().enumerate() {
        let hi = (hex.as_bytes()[i * 2] as char).to_digit(16).unwrap() as u8;
        let lo = (hex.as_bytes()[i * 2 + 1] as char).to_digit(16).unwrap() as u8;
        *item = (hi << 4) | lo;
    }
    Ok(key)
}

/// AES-128-ECB 加密（PKCS7 填充）——`aes_ecb_decrypt` 的镜像，外发媒体用。
/// 明文长度是 16 的倍数时补**整块**（PKCS7 规定），故密文恒为 16 的倍数。
fn aes_ecb_encrypt(plain: &[u8], key: &[u8; 16]) -> Vec<u8> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};
    let cipher = aes::Aes128::new_from_slice(key).expect("AES-128 密钥长度固定 16 字节");
    let pad = 16 - (plain.len() % 16);
    let mut buf = plain.to_vec();
    buf.resize(buf.len() + pad, pad as u8);
    for block in buf.as_chunks_mut::<16>().0 {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    buf
}

/// AES-128-ECB 解密（PKCS7 去填充）。密文长度必须是 16 的倍数。
fn aes_ecb_decrypt(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecrypt, KeyInit};
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        anyhow::bail!("CDN 密文长度 {} 不是 16 的倍数，解密失败", ciphertext.len());
    }
    let cipher = aes::Aes128::new_from_slice(key).context("AES key 初始化失败")?;
    let mut buf = ciphertext.to_vec();
    for block in buf.as_chunks_mut::<16>().0 {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    // PKCS7 去填充
    let pad = *buf.last().unwrap() as usize;
    if pad == 0 || pad > 16 || buf.len() < pad {
        anyhow::bail!("CDN 明文 PKCS7 填充非法 (pad={pad})");
    }
    let end = buf.len() - pad;
    if buf[end..].iter().any(|&b| b as usize != pad) {
        anyhow::bail!("CDN 明文 PKCS7 填充校验失败 (pad={pad})");
    }
    buf.truncate(end);
    Ok(buf)
}

/// query 参数百分号编码（encrypt_query_param 可能含 +/= 等 URL 特殊字符）。
/// 对齐 openclaw-weixin 的 encodeURIComponent 语义：除保留字符外全部转义。
pub(crate) fn percent_encode_query(s: &str) -> String {
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

// ── 扫码登录 ──

#[derive(Debug, Deserialize)]
struct QrcodeResp {
    qrcode: String,
    #[serde(default)]
    qrcode_img_content: String,
}

#[derive(Debug, Deserialize)]
struct QrStatusResp {
    status: String,
    #[serde(default)]
    bot_token: Option<String>,
    #[serde(default)]
    baseurl: Option<String>,
    #[serde(default)]
    ilink_user_id: Option<String>,
}

/// 登录结果（confirmed 时拿到）。
#[derive(Debug, Clone)]
pub struct WeixinLogin {
    pub token: String,
    pub base_url: String,
    pub user_id: String,
}

/// 登录阶段共享的 HTTP client（每次登录要轮询上百次，复用连接池）。
fn login_http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(35))
            .build()
            .expect("reqwest client")
    })
}

/// 登录网关 GET（带两个 iLink 头）。
async fn login_get(url: &str) -> Result<serde_json::Value> {
    let v = login_http()
        .get(url)
        .header("iLink-App-Id", ILINK_APP_ID)
        .header("iLink-App-ClientVersion", ILINK_APP_CLIENT_VERSION)
        .send()
        .await?
        .json()
        .await?;
    Ok(v)
}

/// 第一步：拿二维码。返回 (qrcode 标识, qrcode_img_content 二维码内容)。
pub async fn fetch_qrcode() -> Result<(String, String)> {
    let url = format!(
        "{}/ilink/bot/get_bot_qrcode?bot_type={}",
        FIXED_BASE_URL, BOT_TYPE
    );
    let v = login_get(&url).await?;
    let r: QrcodeResp = serde_json::from_value(v).context("get_bot_qrcode 解析失败")?;
    Ok((r.qrcode, r.qrcode_img_content))
}

/// 第二步：轮询扫码状态一次。confirmed → Ok(Some(登录))；否则 Ok(None) 继续等。
pub async fn poll_qr_status(qrcode: &str) -> Result<Option<WeixinLogin>> {
    let url = format!(
        "{}/ilink/bot/get_qrcode_status?qrcode={}",
        FIXED_BASE_URL, qrcode
    );
    let v = login_get(&url).await?;
    let r: QrStatusResp = serde_json::from_value(v).context("get_qrcode_status 解析失败")?;
    match r.status.as_str() {
        "confirmed" => {
            let token = r.bot_token.context("confirmed 但无 bot_token")?;
            let base_url = r
                .baseurl
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| FIXED_BASE_URL.to_string());
            Ok(Some(WeixinLogin {
                token,
                base_url,
                user_id: r.ilink_user_id.unwrap_or_default(),
            }))
        }
        "expired" => anyhow::bail!("二维码已过期，请重新获取"),
        _ => Ok(None), // wait / scaned / 其它 → 继续等
    }
}

/// 从二维码内容（URL/data-URL/base64 图/文本）渲染出 PNG 字节，供 GUI Image 直接显示。
/// 文本/URL 用 qrcode crate 出位图再编码 PNG；已是图片字节则原样返回。
pub fn render_qr_png(content: &str) -> Result<Vec<u8>> {
    let trimmed = content.trim();
    // 已是图片（data-URL 或纯 base64 PNG/JPEG）→ 直接返回解码字节
    let try_img = |payload: &str| -> Option<Vec<u8>> {
        let b = b64::decode(payload)?;
        if b.starts_with(&[0x89, b'P', b'N', b'G']) || b.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(b)
        } else {
            None
        }
    };
    if let Some(pos) = trimmed.find("base64,") {
        if let Some(b) = try_img(&trimmed[pos + 7..]) {
            return Ok(b);
        }
    }
    if !trimmed.contains(' ') && trimmed.len() > 100 && !trimmed.starts_with("http") {
        if let Some(b) = try_img(trimmed) {
            return Ok(b);
        }
    }
    // 文本/URL → qrcode 渲染位图 → PNG
    let code = qrcode::QrCode::new(trimmed.as_bytes()).context("生成二维码失败")?;
    let img = code.render::<image::Luma<u8>>().quiet_zone(true).build();
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    image::ImageEncoder::write_image(
        encoder,
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::L8,
    )
    .context("PNG 编码失败")?;
    Ok(png)
}

/// 把二维码内容渲染成 PNG 落盘，返回路径（GUI 弹窗用 Image::load_from_path 显示）。
/// 任何形态（URL/文本/base64 图）都经 render_qr_png 统一成二维码 PNG。
pub fn save_qrcode_image(bot_key: &str, content: &str) -> Result<std::path::PathBuf> {
    let dir = crate::bridge_dir().join("logs");
    std::fs::create_dir_all(&dir)?;
    // 统一走 PNG 渲染（文本/URL 也会变成二维码 PNG）
    let png = render_qr_png(content)?;
    let p = dir.join(format!("wechat-qr-{bot_key}.png"));
    std::fs::write(&p, png)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::test_mock::{MockResp, MockServer};
    use std::collections::HashMap;

    /// #283 外发媒体链路的 mock：getuploadurl → CDN → sendmessage 三个端点。
    /// `full_url=false` 时只回 `upload_param`（PROTOCOL.md 记的形态），逼客户端自己
    /// 拼 CDN 地址；`true` 时回 `upload_full_url`（`wx-send` 实测走的形态）。
    /// 可调参数的媒体链路 mock：`cdn_param=None` 时不回 `x-encrypted-param`
    /// （用来测"CDN 成功但缺下载参数"这条失败路径）。
    async fn media_mock_cfg(
        up: serde_json::Value,
        cdn_param: Option<&str>,
        send_ret: i64,
    ) -> MockServer {
        let mut cdn = MockResp::json(serde_json::json!({}));
        if let Some(p) = cdn_param {
            cdn = cdn.with_header("x-encrypted-param", p);
        }
        let mut routes: HashMap<(String, String), MockResp> = HashMap::new();
        routes.insert(
            ("POST".to_string(), "/ilink/bot/getuploadurl".to_string()),
            MockResp::json(up),
        );
        routes.insert(("POST".to_string(), "/upload".to_string()), cdn);
        routes.insert(
            ("POST".to_string(), "/ilink/bot/sendmessage".to_string()),
            MockResp::json(serde_json::json!({ "ret": send_ret, "message_id": 1 })),
        );
        MockServer::start_rich(routes).await
    }

    async fn media_mock(full_url: bool) -> MockServer {
        let up = if full_url {
            serde_json::json!({ "upload_full_url": "{{BASE}}/upload" })
        } else {
            serde_json::json!({ "upload_param": "UP+param=1" })
        };
        media_mock_cfg(up, Some("DL+param=2"), 0).await
    }

    fn rec<'a>(
        recs: &'a [crate::feishu::test_mock::Recorded],
        path: &str,
    ) -> &'a crate::feishu::test_mock::Recorded {
        recs.iter().find(|r| r.path == path).unwrap_or_else(|| {
            panic!(
                "应有 {path} 请求，实际: {:?}",
                recs.iter().map(|r| &r.path).collect::<Vec<_>>()
            )
        })
    }

    /// #283 端到端形状（`upload_param` 分支）：getuploadurl 必填字段、
    /// **CDN 收到的密文能用请求里那个 aeskey 解回原文**（真 roundtrip，不是只看有没有发）、
    /// sendmessage 的 item 类型与字段。
    #[tokio::test]
    async fn send_media_uploads_ciphertext_and_posts_file_item() {
        let server = media_mock(false).await;
        let wx = WeixinClient::new(&server.base, "tok", &server.base);
        let data = b"hello wechat media".to_vec(); // 19 字节 → 补到 32

        wx.send_media("u1", "ctx-1", OutboundMediaKind::File, "a.txt", &data)
            .await
            .unwrap();

        let recs = server.requests.lock().unwrap().clone();
        let up_body: serde_json::Value =
            serde_json::from_str(&rec(&recs, "/ilink/bot/getuploadurl").body).unwrap();
        assert_eq!(up_body["media_type"], 3, "文件 media_type=3");
        assert_eq!(up_body["to_user_id"], "u1");
        assert_eq!(up_body["rawsize"], data.len());
        assert_eq!(up_body["filesize"], 32, "密文长度按 16 补齐");
        assert_eq!(up_body["no_need_thumb"], true);
        assert_eq!(up_body["rawfilemd5"], format!("{:x}", md5::compute(&data)));
        let aeskey_hex = up_body["aeskey"].as_str().unwrap().to_string();
        assert_eq!(aeskey_hex.len(), 32, "aeskey 是 16 字节的 hex");

        // CDN 请求：地址由 upload_param 拼出，密文可解回原文
        let cdn = rec(&recs, "/upload");
        assert!(
            cdn.query.contains("encrypted_query_param=UP%2Bparam%3D1"),
            "upload_param 需百分号编码后拼进 CDN 地址: {}",
            cdn.query
        );
        assert_eq!(
            cdn.headers.get("content-type").map(String::as_str),
            Some("application/octet-stream"),
            "CDN 上传必须声明 octet-stream"
        );
        let key = parse_aes_key_from_hex(&aeskey_hex).unwrap();
        assert_eq!(aes_ecb_decrypt(&cdn.body_bytes, &key).unwrap(), data);

        // sendmessage：文件 item 形状
        let sm: serde_json::Value =
            serde_json::from_str(&rec(&recs, "/ilink/bot/sendmessage").body).unwrap();
        assert_eq!(sm["msg"]["context_token"], "ctx-1", "context_token 必填");
        assert_eq!(sm["msg"]["message_state"], 2, "FINISH 缺了不投递");
        let item = &sm["msg"]["item_list"][0];
        assert_eq!(item["type"], 4, "文件 item type=4");
        assert_eq!(item["file_item"]["file_name"], "a.txt");
        assert_eq!(item["file_item"]["len"], data.len().to_string());
        assert_eq!(item["file_item"]["media"]["encrypt_type"], 1);
        assert_eq!(
            item["file_item"]["media"]["encrypt_query_param"],
            "DL+param=2"
        );
        assert_eq!(
            item["file_item"]["media"]["aes_key"],
            b64::encode(aeskey_hex.as_bytes()),
            "文件类 aes_key = base64(32 个 hex 字符)"
        );
    }

    /// 图片分支：item=2 / media_type=1，且 aes_key 的编码与文件类**不同**
    /// （协议：图片 base64(原始 16 字节)），并额外带 image_item.aeskey（hex）。
    #[tokio::test]
    async fn send_media_image_uses_image_item_and_raw_key_encoding() {
        let server = media_mock(true).await; // 这次走 upload_full_url 分支
        let wx = WeixinClient::new(&server.base, "tok", &server.base);
        let data = b"fakepng".to_vec();

        wx.send_media("u2", "ctx-2", OutboundMediaKind::Image, "a.png", &data)
            .await
            .unwrap();

        let recs = server.requests.lock().unwrap().clone();
        let up_body: serde_json::Value =
            serde_json::from_str(&rec(&recs, "/ilink/bot/getuploadurl").body).unwrap();
        assert_eq!(up_body["media_type"], 1, "图片 media_type=1");
        let aeskey_hex = up_body["aeskey"].as_str().unwrap();
        let key = parse_aes_key_from_hex(aeskey_hex).unwrap();

        // upload_full_url 直连（不再拼 query）
        let cdn = rec(&recs, "/upload");
        assert!(cdn.query.is_empty(), "upload_full_url 分支不该再拼 query");
        assert_eq!(aes_ecb_decrypt(&cdn.body_bytes, &key).unwrap(), data);

        let sm: serde_json::Value =
            serde_json::from_str(&rec(&recs, "/ilink/bot/sendmessage").body).unwrap();
        let item = &sm["msg"]["item_list"][0];
        assert_eq!(item["type"], 2, "图片 item type=2");
        assert_eq!(item["image_item"]["mid_size"], 16);
        assert_eq!(
            item["image_item"]["aeskey"], aeskey_hex,
            "图片额外带 hex 形式"
        );
        assert_eq!(
            item["image_item"]["media"]["aes_key"],
            b64::encode(&key),
            "图片 aes_key = base64(原始 16 字节)"
        );
    }

    /// 审查 P2 回归：明文长度正好是 16 的倍数时 PKCS7 **补一个整块**，声明的
    /// `filesize` 必须等于实际密文长度（旧实现用 ceil 公式会少报 16 字节）。
    #[tokio::test]
    async fn send_media_declares_real_cipher_size_for_aligned_input() {
        let server = media_mock(false).await;
        let wx = WeixinClient::new(&server.base, "tok", &server.base);
        let data = vec![0xABu8; 32]; // 32 = 16×2：ceil 公式得 32，实际密文 48

        wx.send_media("u3", "ctx-3", OutboundMediaKind::File, "aligned.bin", &data)
            .await
            .unwrap();

        let recs = server.requests.lock().unwrap().clone();
        let up_body: serde_json::Value =
            serde_json::from_str(&rec(&recs, "/ilink/bot/getuploadurl").body).unwrap();
        assert_eq!(up_body["rawsize"], 32);
        assert_eq!(
            up_body["filesize"], 48,
            "16 倍数输入必须申报实际 PKCS7 密文长度（补整块）"
        );
        let cdn = rec(&recs, "/upload");
        assert_eq!(cdn.body_bytes.len(), 48, "实传长度要与申报一致");
        let key = parse_aes_key_from_hex(up_body["aeskey"].as_str().unwrap()).unwrap();
        assert_eq!(aes_ecb_decrypt(&cdn.body_bytes, &key).unwrap(), data);
    }

    /// 视频走 video_item（item=5 / media_type=2），尺寸字段是 video_size。
    #[tokio::test]
    async fn send_media_video_uses_video_item() {
        let server = media_mock(false).await;
        let wx = WeixinClient::new(&server.base, "tok", &server.base);
        wx.send_media("u4", "ctx-4", OutboundMediaKind::Video, "clip.mp4", b"vid")
            .await
            .unwrap();
        let recs = server.requests.lock().unwrap().clone();
        let up_body: serde_json::Value =
            serde_json::from_str(&rec(&recs, "/ilink/bot/getuploadurl").body).unwrap();
        assert_eq!(up_body["media_type"], 2);
        let sm: serde_json::Value =
            serde_json::from_str(&rec(&recs, "/ilink/bot/sendmessage").body).unwrap();
        let item = &sm["msg"]["item_list"][0];
        assert_eq!(item["type"], 5, "视频 item type=5");
        assert_eq!(item["video_item"]["video_size"], 16);
        assert!(item.get("file_item").is_none());
    }

    /// 失败面（审查 P3-5）：四条路径都必须显式报错，不静默、不谎报成功。
    #[tokio::test]
    async fn send_media_errors_are_explicit() {
        // ① 空媒体：在读/传之前就拒
        let server = media_mock(false).await;
        let e = WeixinClient::new(&server.base, "tok", &server.base)
            .send_media("u5", "ctx", OutboundMediaKind::File, "a.bin", b"")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("空媒体"), "{e:#}");
        assert!(
            server.requests.lock().unwrap().is_empty(),
            "空媒体不该发出任何请求"
        );

        // ② getuploadurl 缺上传地址 → 点名缺什么，且不泄露整段响应
        let server = media_mock_cfg(serde_json::json!({ "ret": -1 }), Some("DL"), 0).await;
        let e = WeixinClient::new(&server.base, "tok", &server.base)
            .send_media("u5", "ctx", OutboundMediaKind::File, "a.bin", b"x")
            .await
            .unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("未返回 upload_full_url/upload_param"), "{msg}");
        assert!(msg.contains("ret"), "要带关键字段便于诊断: {msg}");

        // ③ CDN 200 但缺 x-encrypted-param → 显式报错
        let server = media_mock_cfg(serde_json::json!({ "upload_param": "UP" }), None, 0).await;
        let e = WeixinClient::new(&server.base, "tok", &server.base)
            .send_media("u5", "ctx", OutboundMediaKind::File, "a.bin", b"x")
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("x-encrypted-param"),
            "缺下载参数要显式报: {e:#}"
        );

        // ④ sendmessage 返回非 0 ret → 报错，不能当成功
        let server =
            media_mock_cfg(serde_json::json!({ "upload_param": "UP" }), Some("DL"), -1).await;
        let e = WeixinClient::new(&server.base, "tok", &server.base)
            .send_media("u5", "ctx", OutboundMediaKind::File, "a.bin", b"x")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("sendmessage 失败"), "{e:#}");
    }

    /// AES-128-ECB(PKCS7) 加密与既有解密互为逆运算；明文长度是 16 的倍数时补整块。
    #[test]
    fn aes_ecb_encrypt_roundtrips_with_decrypt() {
        let key = [7u8; 16];
        for len in [0usize, 1, 15, 16, 17, 32, 100] {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            let enc = aes_ecb_encrypt(&plain, &key);
            assert_eq!(enc.len() % 16, 0, "密文长度必须是 16 的倍数 (len={len})");
            assert!(enc.len() > plain.len(), "PKCS7 必补填充 (len={len})");
            assert_eq!(aes_ecb_decrypt(&enc, &key).unwrap(), plain, "len={len}");
        }
    }

    #[test]
    fn b64_encode_works() {
        assert_eq!(b64::encode(b""), "");
        assert_eq!(b64::encode(b"f"), "Zg==");
        assert_eq!(b64::encode(b"fo"), "Zm8=");
        assert_eq!(b64::encode(b"foo"), "Zm9v");
        assert_eq!(b64::encode(b"12345"), "MTIzNDU=");
    }

    #[test]
    fn parse_getupdates_resp() {
        let j = serde_json::json!({
            "ret": 0,
            "msgs": [{
                "message_id": 7490935474306831112_i64, // 实测真实消息是整数，不是字符串
                "from_user_id": "ou_abc",
                "message_type": 1,
                "context_token": "ctx1",
                "item_list": [{"type": 1, "text_item": {"text": "你好"}}]
            }],
            "get_updates_buf": "cursor2",
            "longpolling_timeout_ms": 35000
        });
        let r: GetUpdatesResp = serde_json::from_value(j).unwrap();
        assert_eq!(r.msgs.len(), 1);
        assert_eq!(r.msgs[0].message_id, "7490935474306831112"); // int 被收成 String
        assert_eq!(r.msgs[0].text(), "你好");
        assert_eq!(r.msgs[0].context_token, "ctx1");
        assert_eq!(r.get_updates_buf, "cursor2");
    }

    #[test]
    fn message_text_empty_when_no_text() {
        let m = WeixinMessage::default();
        assert_eq!(m.text(), "");
        let m2 = WeixinMessage {
            item_list: vec![MessageItem {
                item_type: 2,
                text_item: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(m2.text(), "");
        assert!(m2.media_items().is_empty());
    }

    #[test]
    fn quoted_text_extracts_ref_msg() {
        // 引用/回复：item.ref_msg 带 title 摘要 + message_item 原文本
        let m = WeixinMessage {
            item_list: vec![MessageItem {
                item_type: 1,
                text_item: Some(TextItem {
                    text: "这是回复".into(),
                }),
                ref_msg: Some(RefMessage {
                    title: "摘要".into(),
                    message_item: Some(Box::new(MessageItem {
                        item_type: 1,
                        text_item: Some(TextItem {
                            text: "被引用的原消息".into(),
                        }),
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(m.text(), "这是回复");
        assert_eq!(m.quoted_text(), "摘要 | 被引用的原消息");

        // 无 ref_msg → 空
        let plain = WeixinMessage {
            item_list: vec![MessageItem {
                item_type: 1,
                text_item: Some(TextItem {
                    text: "普通消息".into(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(plain.quoted_text(), "");
    }

    #[test]
    fn quoted_text_parses_from_json() {
        // 反序列化路径：真实 getupdates 的引用消息
        let j = serde_json::json!({
            "message_id": 123,
            "message_type": 1,
            "item_list": [{
                "type": 1,
                "text_item": {"text": "回复内容"},
                "ref_msg": {
                    "title": "摘要",
                    "message_item": {"type": 1, "text_item": {"text": "原消息内容"}}
                }
            }]
        });
        let m: WeixinMessage = serde_json::from_value(j).unwrap();
        assert_eq!(m.text(), "回复内容");
        assert_eq!(m.quoted_text(), "摘要 | 原消息内容");
    }

    #[test]
    fn quoted_media_extracts_ref_msg_media() {
        // 引用/回复：ref_msg.message_item 里的图片/文件等媒体也要提取（下载进 prompt）
        let m = WeixinMessage {
            item_list: vec![MessageItem {
                item_type: 1,
                text_item: Some(TextItem {
                    text: "回复内容".into(),
                }),
                ref_msg: Some(RefMessage {
                    title: "摘要".into(),
                    message_item: Some(Box::new(MessageItem {
                        item_type: 2,
                        image_item: Some(ImageItem {
                            media: Some(CDNMedia {
                                encrypt_query_param: "enc".into(),
                                aes_key: "a2V5".into(),
                                full_url: String::new(),
                                ..Default::default()
                            }),
                            aeskey: "00112233445566778899aabbccddeeff".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let media = m.quoted_media();
        assert_eq!(media.len(), 1, "引用图片应提取出媒体引用");
        assert_eq!(media[0].kind, "image");
        assert_eq!(media[0].aeskey_hex, "00112233445566778899aabbccddeeff");

        // 无引用媒体 → 空
        let plain = WeixinMessage {
            item_list: vec![MessageItem {
                item_type: 1,
                text_item: Some(TextItem {
                    text: "普通".into(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(plain.quoted_media().is_empty());
    }

    #[test]
    fn message_media_extraction() {
        // 图片：aeskey hex + media
        let m = WeixinMessage {
            item_list: vec![MessageItem {
                item_type: 2,
                image_item: Some(ImageItem {
                    media: Some(CDNMedia {
                        encrypt_query_param: "abc".into(),
                        aes_key: "a2V5".into(),
                        full_url: String::new(),
                        ..Default::default()
                    }),
                    aeskey: "00112233445566778899aabbccddeeff".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let medias = m.media_items();
        assert_eq!(medias.len(), 1);
        assert_eq!(medias[0].kind, "image");
        assert_eq!(medias[0].encrypt_query_param, "abc");
        assert_eq!(medias[0].aeskey_hex, "00112233445566778899aabbccddeeff");

        // 文件：file_name + aes_key
        let m2 = WeixinMessage {
            item_list: vec![MessageItem {
                item_type: 4,
                file_item: Some(FileItem {
                    media: Some(CDNMedia {
                        encrypt_query_param: "f1".into(),
                        aes_key: "a2V5".into(),
                        full_url: String::new(),
                        ..Default::default()
                    }),
                    file_name: "报告.pdf".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let medias2 = m2.media_items();
        assert_eq!(medias2.len(), 1);
        assert_eq!(medias2[0].kind, "file");
        assert_eq!(medias2[0].file_name, "报告.pdf");
    }

    #[test]
    fn parse_aes_key_formats() {
        // base64(16 原始字节) —— "0123456789abcdef" 的 base64
        let b64 = b64::encode(b"0123456789abcdef");
        let k1 = parse_aes_key_from_b64(&b64).unwrap();
        assert_eq!(&k1, b"0123456789abcdef");
        // base64(hex 字符串 32 字符)
        let hex = "00112233445566778899aabbccddeeff";
        let b64_hex = b64::encode(hex.as_bytes());
        let k2 = parse_aes_key_from_b64(&b64_hex).unwrap();
        assert_eq!(k2[0], 0x00);
        assert_eq!(k2[15], 0xff);
        // hex 直解（ImageItem.aeskey）
        let k3 = parse_aes_key_from_hex(hex).unwrap();
        assert_eq!(k2, k3);
        // 非法
        assert!(parse_aes_key_from_hex("xyz").is_err());
        assert!(parse_aes_key_from_b64("AAAA").is_err());
    }

    #[test]
    fn aes_ecb_decrypt_roundtrip() {
        use aes::cipher::generic_array::GenericArray;
        use aes::cipher::{BlockEncrypt, KeyInit};
        let key = *b"0123456789abcdef";
        let plain = b"hello wechat media!";
        // 自己加密（PKCS7 填充）再解密验证
        let cipher = aes::Aes128::new_from_slice(&key).unwrap();
        let pad = 16 - (plain.len() % 16);
        let mut padded = plain.to_vec();
        padded.extend(std::iter::repeat_n(pad as u8, pad));
        let mut enc = padded.clone();
        for block in enc.as_chunks_mut::<16>().0 {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        let dec = aes_ecb_decrypt(&enc, &key).unwrap();
        assert_eq!(dec, plain);
        // 长度非 16 倍数报错
        assert!(aes_ecb_decrypt(&[1, 2, 3], &key).is_err());
    }

    #[test]
    fn percent_encode_query_works() {
        assert_eq!(percent_encode_query("a+b/c=="), "a%2Bb%2Fc%3D%3D");
        assert_eq!(percent_encode_query("simple-._~"), "simple-._~");
    }
}
