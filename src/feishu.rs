//! 飞书直连 REST —— tenant_access_token 缓存、发消息（分段）、表情、bot 信息。
//! base: https://open.feishu.cn/open-apis。token 即 bot 身份（不需要 user token）。

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const API_BASE: &str = "https://open.feishu.cn/open-apis";
pub const FEISHU_MSG_LIMIT: usize = 3500; // 单条安全长度（按字符数，对齐 Python len()）

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

    /// 发文本到 chat，超长自动分段加（i/n）前缀。token 即 bot 身份。
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
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
}

/// 构造回复消息请求体（纯函数，便于单测断言 reply_in_thread / content）。
fn reply_body(body_text: &str) -> serde_json::Value {
    json!({
        "msg_type": "text",
        "content": serde_json::to_string(&json!({"text": body_text})).unwrap_or_default(),
        "reply_in_thread": true,
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
}
