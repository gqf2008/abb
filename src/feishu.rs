//! 飞书直连 REST —— tenant_access_token 缓存、发消息（分段）、表情、bot 信息。
//! base: https://open.feishu.cn/open-apis。token 即 bot 身份（不需要 user token）。

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const API_BASE: &str = "https://open.feishu.cn/open-apis";
pub const FEISHU_MSG_LIMIT: usize = 3500; // 单条安全长度（按字符数，对齐 Python len()）

/// 消息里的一条资源引用（图片/文件/音视频/富文本图片）。
#[derive(Debug, Clone)]
pub struct FeishuResource {
    /// image | file | audio | video
    pub kind: String,
    /// 下载用的 file_key / image_key
    pub file_key: String,
    pub file_name: String,
}

/// 解析后的飞书消息内容：文本 + 资源列表（含富文本里全部图片/资源）。
#[derive(Debug, Clone, Default)]
pub struct FeishuParsed {
    pub text: String,
    pub resources: Vec<FeishuResource>,
}

/// 从消息 content（JSON 字符串）解析文本与资源引用。
/// - image：`{"image_key":"img_xxx"}`
/// - file/audio/media：`{"file_key":"file_xxx","file_name":"..."[, "type":"audio|media"]}`
/// - text：`{"text":"..."}`
/// - post 富文本：`{"title":"...","content":[[{"tag":"text","text":"..."},{"tag":"a",...},{"tag":"img","image_key":"..."}]]}`
///   抽 text/a 成纯文本，富文本里的首张图也作为资源（可下载）。
pub fn parse_content(raw: &str) -> FeishuParsed {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return FeishuParsed::default();
    };
    if let Some(r) = resource_from_content(&v) {
        return FeishuParsed {
            text: String::new(),
            resources: vec![r],
        };
    }
    if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
        return FeishuParsed {
            text: t.to_string(),
            resources: Vec::new(),
        };
    }
    // post 富文本
    let mut text = String::new();
    let mut imgs: Vec<FeishuResource> = Vec::new();
    if let Some(title) = v.get("title").and_then(|x| x.as_str()) {
        if !title.is_empty() {
            text.push_str(title);
            text.push('\n');
        }
    }
    if let Some(rows) = v.get("content").and_then(|x| x.as_array()) {
        for row in rows {
            let cells = row.as_array().cloned().unwrap_or_default();
            for c in cells {
                match c.get("tag").and_then(|x| x.as_str()) {
                    Some("text") => {
                        if let Some(t) = c.get("text").and_then(|x| x.as_str()) {
                            text.push_str(t);
                        }
                    }
                    Some("a") => {
                        if let Some(t) = c.get("text").and_then(|x| x.as_str()) {
                            text.push_str(t);
                        }
                        if let Some(h) = c.get("href").and_then(|x| x.as_str()) {
                            if !h.is_empty() {
                                text.push(' ');
                                text.push_str(h);
                            }
                        }
                    }
                    Some("img") => {
                        if let Some(k) = c.get("image_key").and_then(|x| x.as_str()) {
                            if !k.is_empty() {
                                imgs.push(FeishuResource {
                                    kind: "image".into(),
                                    file_key: k.to_string(),
                                    file_name: String::new(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            text.push('\n');
        }
    }
    FeishuParsed {
        text: text.trim().to_string(),
        resources: imgs,
    }
}

fn resource_from_content(v: &serde_json::Value) -> Option<FeishuResource> {
    if let Some(k) = v.get("image_key").and_then(|x| x.as_str()) {
        if !k.is_empty() {
            return Some(FeishuResource {
                kind: "image".into(),
                file_key: k.into(),
                file_name: String::new(),
            });
        }
    }
    if let Some(k) = v.get("file_key").and_then(|x| x.as_str()) {
        if !k.is_empty() {
            let kind = match v.get("type").and_then(|x| x.as_str()) {
                Some("audio") => "audio",
                Some("media") | Some("video") => "video",
                _ => "file",
            };
            return Some(FeishuResource {
                kind: kind.into(),
                file_key: k.into(),
                file_name: v
                    .get("file_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    None
}

pub struct FeishuClient {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl FeishuClient {
    pub fn new(app_id: &str, app_secret: &str) -> FeishuClient {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        FeishuClient {
            http,
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            token: Mutex::new(None),
        }
    }

    /// 缓存复用 tenant_access_token，提前 60s 过期。
    pub async fn tenant_token(&self) -> Result<String> {
        {
            let guard = self.token.lock().unwrap();
            if let Some((tok, exp)) = guard.as_ref() {
                if Instant::now() + Duration::from_secs(60) < *exp {
                    return Ok(tok.clone());
                }
            }
        }
        let resp: serde_json::Value = self
            .http
            .post(format!("{API_BASE}/auth/v3/tenant_access_token/internal"))
            .json(&json!({"app_id": self.app_id, "app_secret": self.app_secret}))
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            return Err(anyhow!(
                "tenant_access_token 失败: code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            ));
        }
        let tok = resp["tenant_access_token"]
            .as_str()
            .context("tenant_access_token 缺失")?
            .to_string();
        let expire_secs = resp["expire"].as_i64().unwrap_or(7200).max(60) as u64;
        *self.token.lock().unwrap() = Some((
            tok.clone(),
            Instant::now() + Duration::from_secs(expire_secs),
        ));
        Ok(tok)
    }

    /// 按 open_id 反查用户姓名（授权码消费后展示用）。best-effort：token/API 失败或
    /// 用户不可见（如机器人/已离职）返回 None，调用方用 open_id 兜底显示。
    pub async fn user_name(&self, open_id: &str) -> Option<String> {
        let token = self.tenant_token().await.ok()?;
        let url = format!("{API_BASE}/contact/v3/users/{open_id}");
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .query(&[("department_id_type", "open_department_id")])
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let name = resp["data"]["user"]["name"].as_str()?.to_string();
        Some(name)
    }

    /// 发文本到 chat，超长自动分段加（i/n）前缀。token 即 bot 身份。
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {        let token = self.tenant_token().await?;
        let chunks = split_text(text, FEISHU_MSG_LIMIT);
        let n = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let body_text = if n > 1 {
                format!("（{}/{n}）\n{}", i + 1, chunk)
            } else {
                chunk
            };
            let resp: serde_json::Value = self
                .http
                .post(format!("{API_BASE}/im/v1/messages?receive_id_type=chat_id"))
                .bearer_auth(&token)
                .json(&json!({
                    "receive_id": chat_id,
                    "msg_type": "text",
                    "content": serde_json::to_string(&json!({"text": body_text}))?,
                }))
                .send()
                .await?
                .json()
                .await?;
            if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
                // API 级失败必须上报（不能只 log 返回 Ok）：否则调用方以为发成功了，
                // 定时任务回落主会话等重试路径全成死代码，用户收不到回复也无任何痕迹。
                anyhow::bail!(
                    "发送失败 code={:?} msg={:?}",
                    resp.get("code"),
                    resp.get("msg")
                );
            }
        }
        Ok(())
    }

    /// 回复指定消息（飞书话题消息回复走这里）：以 message_id 为回复目标，
    /// `reply_in_thread: true` 保证回复落在原话题内（#14）。
    /// 注意：create 发送接口不支持 thread 参数，话题回复必须走 reply 接口。
    pub async fn reply_text(&self, message_id: &str, text: &str) -> Result<()> {
        let token = self.tenant_token().await?;
        let chunks = split_text(text, FEISHU_MSG_LIMIT);
        let n = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let body_text = if n > 1 {
                format!("（{}/{n}）\n{}", i + 1, chunk)
            } else {
                chunk
            };
            let resp: serde_json::Value = self
                .http
                .post(format!("{API_BASE}/im/v1/messages/{message_id}/reply"))
                .bearer_auth(&token)
                .json(&reply_body(&body_text))
                .send()
                .await?
                .json()
                .await?;
            if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
                // API 级失败必须上报（同 send_text）：否则调用方以为回复成功，
                // 用户在话题里收不到任何回复也无痕迹。
                anyhow::bail!(
                    "回复失败 code={:?} msg={:?}",
                    resp.get("code"),
                    resp.get("msg")
                );
            }
        }
        Ok(())
    }

    /// 拉取一条历史消息的全文与资源（引用/回复场景：用户回复时 @ bot，bot 需要读到
    /// 被引用消息的文本 + 图片/文件/音视频）。GET /im/v1/messages/:message_id 返回
    /// `data.items[0]`，`body.content` 是 JSON 字符串（text/post/…），复用 parse_content
    /// 抽取纯文本与全部资源引用。失败返回 Err（调用方 best-effort：拿不到不阻塞回复）。
    pub async fn get_quoted_message(&self, message_id: &str) -> Result<FeishuParsed> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .get(format!("{API_BASE}/im/v1/messages/{message_id}"))
            .bearer_auth(&token)
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "拉取消息失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        // serde_json 对越界/缺失索引返回 Null（不 panic）：items 为空 / data 缺失 /
        // items 非数组时 item=Null → raw="" → parse_content 返回空 → 桥按「无引用内容」跳过。
        let item = &resp["data"]["items"][0];
        let raw = item["body"]["content"].as_str().unwrap_or("");
        Ok(parse_content(raw))
    }

    /// 下载消息内资源（图片/文件/音视频，≤100MB）。返回 (字节, Content-Type)。
    /// `kind` 决定 query type：image → `type=image`；file/audio/video → `type=file`。
    /// 错误码 234003 等会带进错误文案，便于排查（key 与 message 不匹配/权限缺失）。
    pub async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        kind: &str,
    ) -> Result<(Vec<u8>, String)> {
        let token = self.tenant_token().await?;
        let rtype = if kind == "image" { "image" } else { "file" };
        let url =
            format!("{API_BASE}/im/v1/messages/{message_id}/resources/{file_key}?type={rtype}");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .context("飞书资源下载网络错误")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .map(|v| format!("code={:?} msg={:?}", v.get("code"), v.get("msg")))
                .unwrap_or_else(|| body.chars().take(200).collect());
            return Err(anyhow!("飞书资源下载失败 (HTTP {status}): {detail}"));
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp.bytes().await.context("读飞书资源响应失败")?.to_vec();
        Ok((bytes, mime))
    }

    /// 加表情，返回 reaction_id（删除用）。失败 None。
    pub async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Option<String> {
        let token = self.tenant_token().await.ok()?;
        let resp: serde_json::Value = self
            .http
            .post(format!("{API_BASE}/im/v1/messages/{message_id}/reactions"))
            .bearer_auth(&token)
            .json(&json!({"reaction_type": {"emoji_type": emoji_type}}))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        if resp.get("code").and_then(|c| c.as_i64()) == Some(0) {
            resp["data"]["reaction_id"].as_str().map(|s| s.to_string())
        } else {
            crate::log!(
                "[feishu] 加表情失败 {emoji_type}: code={:?}",
                resp.get("code")
            );
            None
        }
    }

    pub async fn del_reaction(&self, message_id: &str, reaction_id: &str) {
        if reaction_id.is_empty() {
            return;
        }
        if let Ok(token) = self.tenant_token().await {
            let _ = self
                .http
                .delete(format!(
                    "{API_BASE}/im/v1/messages/{message_id}/reactions/{reaction_id}"
                ))
                .bearer_auth(&token)
                .send()
                .await;
        }
    }

    /// bot 信息（设置窗自动填充 bot_name/bot_open_id）。GET /open-apis/bot/v3/info。
    pub async fn bot_info(&self) -> Result<(String, String)> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .get(format!("{API_BASE}/bot/v3/info"))
            .bearer_auth(&token)
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            return Err(anyhow!(
                "bot/v3/info 失败: code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            ));
        }
        let bot = &resp["bot"];
        let name = bot["app_name"].as_str().unwrap_or("").to_string();
        let open_id = bot["open_id"].as_str().unwrap_or("").to_string();
        Ok((name, open_id))
    }

    // ── 流式打字机（#42）：CardKit 流式卡片四步 ─────────────────────────────
    // 创建实体（streaming_mode 开）→ im/v1/messages 发送 → PUT elements 累积更新 →
    // PUT settings 关流式。需要机器人开通 cardkit:card:write 权限，未开通时 code!=0，
    // 调用方（桥）回落逐条发送。注意：CardKit 写接口都要求 sequence 严格递增（乐观并发）。
    //
    // 平台侧限制：单卡片 10 次/s（桥按 500ms 节流）；流式闲置 10 分钟自动关闭
    // （任务结束必须显式关，见 messenger::FeishuMessenger::stream_finalize）；
    // 卡片实体仅可发送一次；流式卡片不可转发。

    /// ① 创建流式卡片实体，返回 card_id。
    pub async fn card_create_streaming(&self, initial_text: &str) -> Result<String> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .post(format!("{API_BASE}/cardkit/v1/cards"))
            .bearer_auth(&token)
            .json(&json!({
                "type": "card_json",
                "data": serde_json::to_string(&streaming_card_json(initial_text))?,
            }))
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "cardkit 创建卡片失败 code={:?} msg={:?}（可能未开通 cardkit:card:write 权限）",
                resp.get("code"),
                resp.get("msg")
            );
        }
        resp["data"]["card_id"]
            .as_str()
            .map(|s| s.to_string())
            .context("cardkit 响应缺 card_id")
    }

    /// ② 把卡片实体发到会话（msg_type=interactive 引用 card_id；实体仅可发送一次）。
    pub async fn card_send(&self, chat_id: &str, card_id: &str) -> Result<()> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .post(format!("{API_BASE}/im/v1/messages?receive_id_type=chat_id"))
            .bearer_auth(&token)
            .json(&json!({
                "receive_id": chat_id,
                "msg_type": "interactive",
                "content": serde_json::to_string(&json!({
                    "type": "card",
                    "data": {"card_id": card_id},
                }))?,
            }))
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "卡片消息发送失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        Ok(())
    }

    /// ③ 累积**全文**更新卡片 markdown 元素（平台自动算增量渲染打字机效果）。
    pub async fn card_update_content(
        &self,
        card_id: &str,
        full_text: &str,
        sequence: u64,
    ) -> Result<()> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .put(format!(
                "{API_BASE}/cardkit/v1/cards/{card_id}/elements/{STREAM_ELEMENT_ID}/content"
            ))
            .bearer_auth(&token)
            .json(&json!({
                "content": full_text,
                "sequence": sequence,
            }))
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "卡片内容更新失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        Ok(())
    }

    /// ④ 关流式（streaming_mode=false）。任务结束必调——否则平台侧 10 分钟才自动关。
    pub async fn card_close_streaming(&self, card_id: &str, sequence: u64) -> Result<()> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .put(format!("{API_BASE}/cardkit/v1/cards/{card_id}/settings"))
            .bearer_auth(&token)
            .json(&json!({
                "settings": serde_json::to_string(&json!({
                    "config": {"streaming_mode": false},
                }))?,
                "sequence": sequence,
            }))
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "卡片关流式失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        Ok(())
    }
}

/// 构造回复消息请求体（纯函数，便于单测断言 reply_in_thread / content）。
fn reply_body(body_text: &str) -> serde_json::Value {
    json!({
        "msg_type": "text",
        "content": serde_json::to_string(&json!({"text": body_text})).unwrap_or_default(),
        "reply_in_thread": true,
    })
}

/// 流式卡片内 markdown 元素的 element_id（#42 更新内容时按它寻址）。
pub const STREAM_ELEMENT_ID: &str = "stream_md";

/// 构造流式卡片 JSON（card_json 2.0，纯函数便于单测）：
/// streaming_mode 开 + 打字机渲染参数；内容全部落在唯一 markdown 元素上，
/// 后续 PUT elements/{element_id}/content 以累积全文驱动原地滚动。
fn streaming_card_json(text: &str) -> serde_json::Value {
    json!({
        "schema": "2.0",
        "config": {
            "streaming_mode": true,
            "streaming_config": {
                "print_frequency_ms": 70,
                "print_step": 2,
                "print_strategy": "fast",
            },
            "update_multi": true,
        },
        "body": {
            "elements": [
                {"tag": "markdown", "element_id": STREAM_ELEMENT_ID, "content": text},
            ],
        },
    })
}

/// 按字符数（不是字节）逐行贪心分段，对齐 Python 的 len() 语义。
pub fn split_text(text: &str, limit: usize) -> Vec<String> {
    let char_count = text.chars().count();
    if char_count <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for line in text.split_inclusive('\n') {
        let l = line.chars().count();
        if cur_len + l > limit && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        cur.push_str(line);
        cur_len += l;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    if chunks.is_empty() {
        // 兜底：硬切前 limit 字符
        return vec![text.chars().take(limit).collect()];
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_short() {
        assert_eq!(split_text("hello", 3500), vec!["hello".to_string()]);
    }

    #[test]
    fn split_by_chars_not_bytes() {
        // 中文每字 3 字节但 1 字符；按字符数分段
        let line = "汉".repeat(100);
        let text = format!("{line}\n{line}\n{line}");
        let chunks = split_text(&text, 150);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.chars().count() <= 150 + 1); // +1 给换行
        }
    }

    #[test]
    fn split_long_single_line_fallback() {
        let text = "a".repeat(8000); // 无换行的超长行
        let chunks = split_text(&text, 3500);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn reply_body_uses_reply_in_thread() {
        // 话题回复必须带 reply_in_thread: true，否则回复会落到群根会话（#14）
        let body = reply_body("你好");
        assert_eq!(body["msg_type"], "text");
        assert_eq!(body["reply_in_thread"], true);
        let content: serde_json::Value =
            serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["text"], "你好");
    }

    #[test]
    fn parse_content_text() {
        let p = parse_content(r#"{"text":"你好"}"#);
        assert_eq!(p.text, "你好");
        assert!(p.resources.is_empty());
    }

    #[test]
    fn parse_content_image() {
        let p = parse_content(r#"{"image_key":"img_abc"}"#);
        assert_eq!(p.text, "");
        let r = p.resources.first().expect("应解析出图片资源");
        assert_eq!(r.kind, "image");
        assert_eq!(r.file_key, "img_abc");
    }

    #[test]
    fn parse_content_file() {
        let p = parse_content(r#"{"file_key":"file_1","file_name":"报告.pdf"}"#);
        let r = p.resources.first().unwrap();
        assert_eq!(r.kind, "file");
        assert_eq!(r.file_key, "file_1");
        assert_eq!(r.file_name, "报告.pdf");
    }

    #[test]
    fn parse_content_audio_video_kind() {
        let a = parse_content(r#"{"file_key":"f1","file_name":"a.amr","type":"audio"}"#);
        assert_eq!(a.resources.first().unwrap().kind, "audio");
        let v = parse_content(r#"{"file_key":"f2","file_name":"b.mp4","type":"media"}"#);
        assert_eq!(v.resources.first().unwrap().kind, "video");
    }

    #[test]
    fn parse_content_post_multiple_images() {
        // 富文本多图：resources 收集全部（引用/回复场景要全部下载），resource 兼容第一张
        let p = parse_content(
            r#"{"title":"多图","content":[[{"tag":"img","image_key":"img_1"}],[{"tag":"img","image_key":"img_2"}]]}"#,
        );
        assert_eq!(p.resources.len(), 2, "富文本多图应全部收集");
        assert_eq!(p.resources[0].file_key, "img_1");
        assert_eq!(p.resources[1].file_key, "img_2");
        assert_eq!(
            p.resources.first().unwrap().file_key,
            "img_1",
            "第一张图仍在最前"
        );
    }

    #[test]
    fn parse_content_post_rich_text() {
        let p = parse_content(
            r#"{"title":"标题","content":[[{"tag":"text","text":"你好 "},{"tag":"a","text":"链接","href":"https://example.com"}],[{"tag":"img","image_key":"img_pic"}]]}"#,
        );
        assert!(p.text.contains("标题"));
        assert!(p.text.contains("你好"));
        assert!(p.text.contains("https://example.com"));
        let r = p.resources.first().expect("富文本图片应解析为资源");
        assert_eq!(r.kind, "image");
        assert_eq!(r.file_key, "img_pic");
    }

    #[test]
    fn parse_content_empty_on_garbage() {
        let p = parse_content("not json");
        assert_eq!(p.text, "");
        assert!(p.resources.is_empty());
    }

    #[test]
    fn streaming_card_json_shape() {
        // #42：流式卡片结构 —— streaming_mode 开、唯一 markdown 元素带 element_id 供更新寻址
        let c = streaming_card_json("⏳ 思考中…");
        assert_eq!(c["schema"], "2.0");
        assert_eq!(c["config"]["streaming_mode"], true);
        let el = &c["body"]["elements"][0];
        assert_eq!(el["tag"], "markdown");
        assert_eq!(el["element_id"], STREAM_ELEMENT_ID);
        assert_eq!(el["content"], "⏳ 思考中…");
        // 序列化再反序列化不丢结构（create 时 data 是字符串内嵌 JSON）
        let s = serde_json::to_string(&c).unwrap();
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["config"]["streaming_mode"], true);
    }
}
