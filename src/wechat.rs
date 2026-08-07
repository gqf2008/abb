//! 微信通道 —— 腾讯官方 ilink 协议（参考 ~/.openclaw/extensions/openclaw-weixin）。
//! QR 扫码登录 → bot_token；之后 HTTP JSON 长轮询 getupdates 收消息、sendmessage 发消息。
//! 端点：`{baseurl}/ilink/bot/{get_bot_qrcode,get_qrcode_status,getupdates,sendmessage}`。
//! 只实现文本收发（图片/语音/文件走 CDN AES，后续再加）。跨平台（reqwest rustls）。

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// 登录固定网关（扫码阶段连这里；登录后多数情况仍用它，除非响应带 redirect_host/baseurl）。
pub const FIXED_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
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

    #[allow(dead_code)] // 解码：二维码内容若是 base64 图片时用（当前服务端返回 URL，保留备用）
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
    #[serde(default)]
    pub text_item: Option<TextItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: String,
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

impl WeixinClient {
    pub fn new(base_url: &str, token: &str) -> WeixinClient {
        // 单 client 复用连接池/TLS；长轮询用每请求超时覆盖默认总超时。
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        WeixinClient {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
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
        let v: serde_json::Value = resp.json().await.context("响应非 JSON")?;
        Ok(v)
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
        // 协议（github epiral/weixin-bot PROTOCOL.md）要求的外发 msg 字段，缺一会被服务器
        // 「先 ack（给 message_id）再丢弃」——消息不投递。message_state:2=FINISH 尤其关键，
        // 缺了被当「生成中」不渲染；from_user_id bot 外发留空；client_id 每条唯一。
        let client_id = format!("fb_{}_{}", crate::chrono_lite::unix_secs(), fastrand::u64(..));
        let body = serde_json::json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to_user_id,
                "client_id": client_id,
                "message_type": 2,            // BOT
                "message_state": 2,           // FINISH（缺这个 = 不投递）
                "context_token": context_token,
                "item_list": [ { "type": 1, "text_item": { "text": text } } ],
            },
            "base_info": base_info(),
        });
        let v = self.post("ilink/bot/sendmessage", &body).await?;
        if v.get("ret").and_then(|r| r.as_i64()).unwrap_or(0) != 0 {
            return Err(anyhow!("sendmessage 失败: {v}"));
        }
        Ok(())
    }
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
            }],
            ..Default::default()
        };
        assert_eq!(m2.text(), "");
    }
}
