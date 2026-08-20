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
    /// API 基址（mock 单测注入本地服务器用；生产恒为 API_BASE）。
    base: String,
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
            base: API_BASE.to_string(),
        }
    }

    /// 测试用：基址指向本地 mock 服务器（请求形状断言）。与 new 的唯一差异是 base。
    #[cfg(test)]
    pub(crate) fn with_base(app_id: &str, app_secret: &str, base: &str) -> FeishuClient {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        FeishuClient {
            http,
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            token: Mutex::new(None),
            base: base.to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
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
            .post(self.url("/auth/v3/tenant_access_token/internal"))
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
        let url = self.url(&format!("/contact/v3/users/{open_id}"));
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
                .post(self.url("/im/v1/messages?receive_id_type=chat_id"))
                .bearer_auth(&token)
                .json(&json!({
                    "receive_id": chat_id,
                    "msg_type": "interactive",
                    // 卡片 markdown 元素：飞书渲染 markdown 成富文本（对齐微信原生渲染）
                    "content": serde_json::to_string(&json!({
                        "elements": [{"tag":"markdown","content": body_text}]
                    }))?,
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
                .post(self.url(&format!("/im/v1/messages/{message_id}/reply")))
                .bearer_auth(&token)
                .json(&reply_markdown_body(&body_text))
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
            .get(self.url(&format!("/im/v1/messages/{message_id}")))
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
        let url = self.url(&format!(
            "/im/v1/messages/{message_id}/resources/{file_key}?type={rtype}"
        ));
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
            .post(self.url(&format!("/im/v1/messages/{message_id}/reactions")))
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
                .delete(self.url(&format!(
                    "/im/v1/messages/{message_id}/reactions/{reaction_id}"
                )))
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
            .get(self.url("/bot/v3/info"))
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

    /// 创建群聊（虚拟 Bot #75）：POST /im/v1/chats。
    /// 群主策略（8-20 用户决策，两次调整后的最终形态）：**群主 = 机器人**（不传
    /// owner_id，创建者默认群主，管理权自持），**owner 用户为管理员 + 成员**：
    /// - `user_id_list: [owner_user_id]`：把 owner 拉进群（否则用户飞书客户端看不到群，
    ///   8-20 实测踩坑）——注意字段名是 `user_id_list` 不是旧文档的 user_ids（现网 schema，
    ///   lark-cli dry-run + SDK 核实）；
    /// - 创建后调 `add_managers` 把 owner 设为管理员（bot 是群主，有权限；管理员才能
    ///   在飞书里改群名/群介绍 = 改角色）。best-effort：失败只 log 返回 Ok（owner 仍是
    ///   成员可用，可手动在飞书设管理员）；
    /// - 曾试过 owner_id=用户（PR #78）：转让后 bot 降级普通成员且"只有群主能加管理员"
    ///   → bot 失去全部管理权、无法自助恢复——此路不通，弃用；
    /// - `uuid` 幂等：同 uuid 重复调用返回同一个群（网络重试/超时重发安全）；
    /// - `chat_mode: "group"`：显式普通群（与话题群 p2p 群区分）。
    ///
    /// 返回 chat_id。与 send_text 同款 token+bearer+code==0 模板。
    pub async fn create_chat(
        &self,
        name: &str,
        description: &str,
        owner_user_id: &str,
    ) -> Result<String> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .post(self.url("/im/v1/chats"))
            .bearer_auth(&token)
            .json(&json!({
                "name": name,
                "description": description,
                "user_id_list": [owner_user_id],
                "uuid": uuid::Uuid::new_v4().to_string(),
                "chat_mode": "group",
            }))
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "创建群失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        let chat_id = resp["data"]["chat_id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("创建群响应缺 chat_id: {resp}"))?;
        // 创建后把 owner 设为管理员（群主=bot 策略；只有群主能加管理员，创建瞬间
        // bot 即群主，此处调用有权限）。best-effort：失败只 log 返回 Ok——owner 仍是
        // 成员可用，可手动在飞书群设置里补设管理员。
        if !owner_user_id.is_empty() {
            if let Err(e) = self.add_managers(&chat_id, &[owner_user_id]).await {
                crate::log!(
                    "[feishu] 群 {chat_id} 已创建但设置 owner 管理员失败（可手动在飞书设置）: {e:#}"
                );
            }
        }
        Ok(chat_id)
    }

    /// 指定群管理员（虚拟 Bot：owner 当管理员，群主仍是 bot）。POST
    /// /im/v1/chats/:chat_id/managers/add_managers（lark-cli dry-run 确认）。
    /// 仅群主可调——bot 是创建者/群主，天然有权限。member_id_type 默认 open_id。
    pub async fn add_managers(&self, chat_id: &str, manager_ids: &[&str]) -> Result<()> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .post(self.url(&format!("/im/v1/chats/{chat_id}/managers/add_managers")))
            .bearer_auth(&token)
            .json(&json!({ "manager_ids": manager_ids }))
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "设置管理员失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        Ok(())
    }

    /// 群资料（虚拟 Bot 注入用）：(群名, 群介绍)。失败返回 Err——调用方 best-effort：
    /// 拿不到只 log 不阻塞消息处理（对齐 get_quoted_message 的语义）。
    pub async fn get_chat_info(&self, chat_id: &str) -> Result<(String, String)> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .get(self.url(&format!("/im/v1/chats/{chat_id}")))
            .bearer_auth(&token)
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "查询群资料失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        let name = resp["data"]["name"].as_str().unwrap_or("").to_string();
        let desc = resp["data"]["description"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok((name, desc))
    }

    /// 改群资料（虚拟 Bot 编辑：群名=角色名、群介绍=system prompt，即时生效）。
    /// ⚠️ 方法是 **PUT** 不是 PATCH（8-20 真机实测：PATCH /im/v1/chats/:id 返回
    /// 404 page not found——mock 只验形状不验路由，假绿；lark-cli im.chats.update
    /// dry-run 显示 PUT）。owner_id 字段可顺带转让群主（新群主必须在群里）。
    pub async fn update_chat(&self, chat_id: &str, name: &str, description: &str) -> Result<()> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .put(self.url(&format!("/im/v1/chats/{chat_id}")))
            .bearer_auth(&token)
            .json(&json!({
                "name": name,
                "description": description,
            }))
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "更新群资料失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        Ok(())
    }

    /// 解散群（虚拟 Bot 解散：DELETE /im/v1/chats/:chat_id，不可恢复——GUI 侧强确认）。
    /// 需要应用开通 im:chat 或 im:chat:delete 权限（99991672 权限拒绝见 scope_hint）。
    pub async fn delete_chat(&self, chat_id: &str) -> Result<()> {
        let token = self.tenant_token().await?;
        let resp: serde_json::Value = self
            .http
            .delete(self.url(&format!("/im/v1/chats/{chat_id}")))
            .bearer_auth(&token)
            .send()
            .await?
            .json()
            .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "解散群失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        Ok(())
    }

    /// 按 open_id 给用户私聊发消息（receive_id_type=open_id）。
    /// 用途：权限不足时给 owner 发授权指引（见 scope_hint）。与 send_text 同款
    /// 卡片 markdown 分段，只是收件人从 chat_id 换成 open_id（不依赖 primary_chat
    /// 是否可用）。
    pub async fn send_text_to_user(&self, open_id: &str, text: &str) -> Result<()> {
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
                .post(self.url("/im/v1/messages?receive_id_type=open_id"))
                .bearer_auth(&token)
                .json(&json!({
                    "receive_id": open_id,
                    "msg_type": "interactive",
                    "content": serde_json::to_string(&json!({
                        "elements": [{"tag":"markdown","content": body_text}]
                    }))?,
                }))
                .send()
                .await?
                .json()
                .await?;
            if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
                anyhow::bail!(
                    "发给用户失败 code={:?} msg={:?}",
                    resp.get("code"),
                    resp.get("msg")
                );
            }
        }
        Ok(())
    }
}

/// 飞书权限不足错误识别（8-20 实测：code=99991672 Access denied，错误文本带 scope
/// 列表与授权链接）。返回 (需要的 scope 摘要, 授权链接)。ABB 用它给 owner 发授权指引——
/// 平台权限问题不该只躺在日志/状态行里，owner 需要可执行的下一步。
/// 参数是任意 API 失败后的完整错误字符串（含 code=… msg=…，见各方法 bail 格式）；
/// 检测「99991672 或 Access denied」出现才解析，避免误判其它错误码。
pub fn scope_hint(err: &str) -> Option<(String, String)> {
    if !err.contains("99991672") && !err.contains("Access denied") {
        return None;
    }
    // msg 里的 scope 列表：[im:chat, im:chat:delete]（取 [] 内原文）
    let scopes = err
        .split('[')
        .nth(1)
        .and_then(|s| s.split(']').next())
        .unwrap_or("（详见授权链接）")
        .to_string();
    // msg 里的授权链接：https://open.feishu.cn/app/...——链接常紧跟在「即可：」等
    // 中文标点后（同一 token 内），不能用 starts_with 匹配整个 token，改为 contains
    // 定位后截断（实测错误串就是「…即可：https://…」形态，8-20 真机样例）。
    // 无链接给不出则 None——调用方发不带链接的提示。
    err.split_whitespace()
        .find(|w| w.contains("https://open.feishu.cn/app/"))
        .and_then(|w| {
            w.find("https://open.feishu.cn/app/")
                .map(|i| w[i..].to_string())
        })
        .map(|s| {
            s.trim_end_matches(['，', '。', ',', '.', '"', '\''])
                .to_string()
        })
        .map(|l| (scopes, l))
}

/// 构造话题回复请求体（纯函数，便于单测断言 msg_type/reply_in_thread/content）。
/// 用卡片 markdown 元素承载：飞书把 markdown 渲染成富文本（对齐微信原生渲染，
/// 而非纯文本显示原始 `##`/`**`/` ``` ` 符号）。content 是字符串化 JSON（飞书 API 要求）。
fn reply_markdown_body(body_text: &str) -> serde_json::Value {
    json!({
        "msg_type": "interactive",
        "content": serde_json::to_string(&json!({
            "elements": [{"tag":"markdown","content": body_text}]
        })).unwrap_or_default(),
        "reply_in_thread": true,
    })
}

/// 按字符数（不是字节）逐行贪心分段，对齐 Python 的 len() 语义。
/// 代码块 fence（```）保护：超限时只在非代码块状态切，避免代码块跨段断裂
/// （飞书/微信分段发多条时，代码块被切到两条会破坏渲染）。三端共用此分段
/// （飞书/微信 3500、钉钉 8000），故保护对三端均生效。
pub fn split_text(text: &str, limit: usize) -> Vec<String> {
    let char_count = text.chars().count();
    if char_count <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let mut in_fence = false; // 是否在 ``` 代码块内（跨行跟踪）
    for line in text.split_inclusive('\n') {
        let l = line.chars().count();
        if cur_len + l > limit && !cur.is_empty() && !in_fence {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        cur.push_str(line);
        cur_len += l;
        // 本行 ``` 出现奇数次 → 切换 fence 状态（fence 行归当前块后再翻转）
        if line.matches("```").count() % 2 == 1 {
            in_fence = !in_fence;
        }
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
    fn split_preserves_code_fence() {
        // 含闭合代码块的长文本，分段后每段 fence 计数平衡（不在代码块中间切）
        let unit = format!("前言{}：\n```\ncode\n```\n", "汉".repeat(200));
        let text = unit.repeat(20); // 多段代码块，总长超 limit
        let chunks = split_text(&text, 3500);
        assert!(chunks.len() >= 2, "应分段");
        for (i, c) in chunks.iter().enumerate() {
            let n = c.matches("```").count();
            assert_eq!(n % 2, 0, "第 {} 段 fence 未平衡（``` 出现 {} 次）", i, n);
        }
    }

    #[test]
    fn reply_markdown_body_uses_markdown_and_thread() {
        // 话题回复用卡片 markdown 元素 + reply_in_thread: true（#14 + markdown 渲染）
        let body = reply_markdown_body("你好");
        assert_eq!(body["msg_type"], "interactive");
        assert_eq!(body["reply_in_thread"], true);
        let content: serde_json::Value =
            serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["elements"][0]["tag"], "markdown");
        assert_eq!(content["elements"][0]["content"], "你好");
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

    // ─── 虚拟 Bot 群管理 API（#75）：mock HTTP 断言请求形状 ───
    // 走本地 TCP mock（test_mock 模块）：token 端点到群端点全部落地断言，
    // 不碰真实飞书 API（token/凭证全 mock）。

    /// 起一个带 token 端点的 mock：返回 (server, 默认 token 路由)。
    /// token 路由统一（/auth/v3/tenant_access_token/internal），各测试再补业务路由。
    async fn mock_server(
        routes: std::collections::HashMap<(String, String), serde_json::Value>,
    ) -> super::test_mock::MockServer {
        let mut all = std::collections::HashMap::new();
        all.insert(
            (
                "POST".to_string(),
                "/auth/v3/tenant_access_token/internal".to_string(),
            ),
            json!({"code": 0, "tenant_access_token": "mock-token", "expire": 7200}),
        );
        all.extend(routes);
        super::test_mock::MockServer::start(all).await
    }

    #[tokio::test]
    async fn create_chat_sends_expected_shape_and_returns_chat_id() {
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            ("POST".to_string(), "/im/v1/chats".to_string()),
            json!({"code": 0, "msg": "success", "data": {"chat_id": "oc_vb_new"}}),
        );
        routes.insert(
            (
                "POST".to_string(),
                "/im/v1/chats/oc_vb_new/managers/add_managers".to_string(),
            ),
            json!({"code": 0, "msg": "success"}),
        );
        let server = mock_server(routes).await;
        let fs = FeishuClient::with_base("cli_a", "secret", &server.base);
        let chat_id = fs
            .create_chat("后端开发", "你是后端工程师。", "ou_boss")
            .await
            .unwrap();
        assert_eq!(chat_id, "oc_vb_new");

        let recs = server.requests.lock().unwrap().clone();
        // 三次请求：token + 建群 + 设 owner 为管理员（顺序固定：token 先）
        assert_eq!(recs.len(), 3, "建群后应把 owner 设为管理员");
        assert_eq!(recs[0].method, "POST");
        assert!(recs[0].path.starts_with("/auth/v3/"));
        let create = &recs[1];
        assert_eq!(create.method, "POST");
        assert_eq!(create.path, "/im/v1/chats");
        assert_eq!(create.auth, "Bearer mock-token", "应带 tenant token");
        let body: serde_json::Value = serde_json::from_str(&create.body).unwrap();
        assert_eq!(body["name"], "后端开发");
        assert_eq!(body["description"], "你是后端工程师。");
        // 群主策略（8-20 用户决策最终版）：不传 owner_id/set_bot_manager（群主=bot，
        // 创建者默认；传 owner_id 会导致转让后 bot 失去管理权无法恢复）
        assert!(
            body.get("owner_id").is_none(),
            "群主必须是 bot（不传 owner_id）"
        );
        assert!(
            body.get("set_bot_manager").is_none(),
            "bot 默认群主，无需 set_bot_manager"
        );
        assert_eq!(
            body["user_id_list"],
            json!(["ou_boss"]),
            "必须把用户拉进群（8-20 实测：群里只有 bot 时飞书客户端不可见）"
        );
        assert_eq!(body["chat_mode"], "group");
        assert!(
            body["uuid"]
                .as_str()
                .map(|u| !u.is_empty())
                .unwrap_or(false),
            "uuid 幂等键必须有"
        );
        // 第二次调用：owner 设为管理员（bot 群主身份有权限）
        let mgr = &recs[2];
        assert_eq!(mgr.method, "POST");
        assert_eq!(mgr.path, "/im/v1/chats/oc_vb_new/managers/add_managers");
        let mb: serde_json::Value = serde_json::from_str(&mgr.body).unwrap();
        assert_eq!(mb["manager_ids"], json!(["ou_boss"]), "owner 设为管理员");
    }

    #[tokio::test]
    async fn create_chat_errors_on_api_failure() {
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            ("POST".to_string(), "/im/v1/chats".to_string()),
            json!({"code": 99991, "msg": "permission denied"}),
        );
        let server = mock_server(routes).await;
        let fs = FeishuClient::with_base("cli_a", "secret", &server.base);
        let e = fs.create_chat("x", "y", "ou_boss").await.unwrap_err();
        assert!(e.to_string().contains("99991"), "错误码应进文案: {e:#}");
    }

    #[tokio::test]
    async fn get_chat_info_returns_name_and_description() {
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            ("GET".to_string(), "/im/v1/chats/oc_vb_1".to_string()),
            json!({"code": 0, "data": {"chat_id": "oc_vb_1", "name": "后端开发", "description": "你是后端工程师。"}}),
        );
        let server = mock_server(routes).await;
        let fs = FeishuClient::with_base("cli_a", "secret", &server.base);
        let (name, desc) = fs.get_chat_info("oc_vb_1").await.unwrap();
        assert_eq!(name, "后端开发");
        assert_eq!(desc, "你是后端工程师。");
        let recs = server.requests.lock().unwrap().clone();
        assert_eq!(recs[1].method, "GET");
        assert_eq!(recs[1].path, "/im/v1/chats/oc_vb_1");
        assert_eq!(recs[1].auth, "Bearer mock-token");
    }

    #[tokio::test]
    async fn update_chat_puts_name_and_description() {
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            ("PUT".to_string(), "/im/v1/chats/oc_vb_1".to_string()),
            json!({"code": 0}),
        );
        let server = mock_server(routes).await;
        let fs = FeishuClient::with_base("cli_a", "secret", &server.base);
        fs.update_chat("oc_vb_1", "前端开发", "你是前端工程师。")
            .await
            .unwrap();
        let recs = server.requests.lock().unwrap().clone();
        assert_eq!(recs[1].method, "PUT");
        assert_eq!(recs[1].path, "/im/v1/chats/oc_vb_1");
        let body: serde_json::Value = serde_json::from_str(&recs[1].body).unwrap();
        assert_eq!(body["name"], "前端开发");
        assert_eq!(body["description"], "你是前端工程师。");
    }

    #[tokio::test]
    async fn delete_chat_sends_delete_request() {
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            ("DELETE".to_string(), "/im/v1/chats/oc_vb_1".to_string()),
            json!({"code": 0}),
        );
        let server = mock_server(routes).await;
        let fs = FeishuClient::with_base("cli_a", "secret", &server.base);
        fs.delete_chat("oc_vb_1").await.unwrap();
        let recs = server.requests.lock().unwrap().clone();
        assert_eq!(recs[1].method, "DELETE");
        assert_eq!(recs[1].path, "/im/v1/chats/oc_vb_1");
        assert_eq!(recs[1].auth, "Bearer mock-token");
    }

    #[test]
    fn scope_hint_parses_real_access_denied_error() {
        // 8-20 真机实测的错误串（解散群被拒）：应解析出 scope 列表与授权链接
        let err = "解散群失败 code=99991672 msg=\"Access denied. One of the following scopes is required: [im:chat, im:chat:delete].应用尚未开通所需的应用身份权限：[im:chat, im:chat:delete]，点击链接申请并开通任一权限即可：https://open.feishu.cn/app/cli_a75884b6c733900b/auth?q=im:chat,im:chat:delete&op_from=openapi&token_type=tenant\"";
        let (scopes, link) = scope_hint(err).expect("应识别出权限不足");
        assert_eq!(
            scopes, "im:chat, im:chat:delete",
            "scope 列表取自错误文本 []"
        );
        assert!(
            link.starts_with("https://open.feishu.cn/app/"),
            "授权链接应被提取: {link}"
        );
    }

    #[test]
    fn scope_hint_ignores_non_scope_errors() {
        assert!(scope_hint("创建群失败 code=99992000 msg=unknown").is_none());
        assert!(scope_hint("网络超时").is_none());
        assert!(scope_hint("").is_none());
    }

    #[tokio::test]
    async fn send_text_to_user_uses_open_id_receive_type() {
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            ("POST".to_string(), "/im/v1/messages".to_string()),
            json!({"code": 0, "msg": "success", "data": {"message_id": "om_1"}}),
        );
        let server = mock_server(routes).await;
        let fs = FeishuClient::with_base("cli_a", "secret", &server.base);
        fs.send_text_to_user("ou_boss", "请开通权限：https://x")
            .await
            .unwrap();
        let recs = server.requests.lock().unwrap().clone();
        assert_eq!(recs[1].method, "POST");
        assert_eq!(recs[1].path, "/im/v1/messages");
        assert_eq!(
            recs[1].query, "receive_id_type=open_id",
            "发给用户必须走 open_id 接收类型"
        );
        let body: serde_json::Value = serde_json::from_str(&recs[1].body).unwrap();
        assert_eq!(body["receive_id"], "ou_boss");
        assert_eq!(body["msg_type"], "interactive");
    }
}

/// mock HTTP 服务器（#75 群 API 单测共用；钉钉 tests 也引它）：
/// 本地 TcpListener + 按 (method, path) 预置 JSON 响应 + 全量记录请求。
/// 请求形状断言（方法/路径/鉴权头/body 字段）靠 Recorded 列表。
#[cfg(test)]
pub(crate) mod test_mock {
    use serde_json::json;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 一次已记录的请求（断言请求形状用）。
    #[derive(Debug, Clone)]
    pub struct Recorded {
        pub method: String,
        /// 不含 query 的路径。
        pub path: String,
        /// query 字符串（原样；无 query 为空）。
        pub query: String,
        /// Authorization 头值（无则为空）。
        pub auth: String,
        /// 请求体（按 Content-Length 精确读）。
        pub body: String,
    }

    /// 本地 mock HTTP 服务器：Drop 时中止服务任务。
    pub struct MockServer {
        pub base: String,
        pub requests: Arc<Mutex<Vec<Recorded>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockServer {
        /// 启动。routes: (HTTP 方法, 路径) → 响应 JSON（未命中的路由回 {"code": 404}，
        /// 让客户端错误路径也能走到 code 判定分支，而不是连接层报错）。
        pub async fn start(routes: HashMap<(String, String), Value>) -> MockServer {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock server");
            let addr = listener.local_addr().expect("mock addr");
            let requests: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
            let reqs = requests.clone();
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let reqs = reqs.clone();
                    let routes = routes.clone();
                    tokio::spawn(async move {
                        // 一条连接可承载多个请求（reqwest keep-alive）：循环读到 EOF
                        loop {
                            let mut buf = Vec::new();
                            let mut tmp = [0u8; 4096];
                            let header_end;
                            loop {
                                let n = stream.read(&mut tmp).await.unwrap_or(0);
                                if n == 0 {
                                    return; // 客户端关闭连接
                                }
                                buf.extend_from_slice(&tmp[..n]);
                                if let Some(pos) = find_headers_end(&buf) {
                                    header_end = pos;
                                    break;
                                }
                                if buf.len() > 64 * 1024 {
                                    return;
                                }
                            }
                            // 关键：headers 与 body 可能同段到达——body 先从 buf 里取
                            // （split_off 留 headers 部分做解析）。原实现把这段丢掉后
                            // 补读永远等不到数据（客户端发完已等响应）→ 连接超时，踩过。
                            let mut body = buf.split_off(header_end);
                            let head = String::from_utf8_lossy(&buf).into_owned();
                            let mut lines = head.split("\r\n");
                            let req_line = lines.next().unwrap_or_default();
                            let mut parts = req_line.split_whitespace();
                            let method = parts.next().unwrap_or("").to_string();
                            let raw_path = parts.next().unwrap_or("").to_string();
                            let (path, query) = match raw_path.split_once('?') {
                                Some((p, q)) => (p.to_string(), q.to_string()),
                                None => (raw_path, String::new()),
                            };
                            let mut content_length = 0usize;
                            let mut auth = String::new();
                            for l in lines {
                                if let Some((k, v)) = l.split_once(": ") {
                                    let k = k.to_ascii_lowercase();
                                    if k == "content-length" {
                                        content_length = v.trim().parse().unwrap_or(0);
                                    } else if k == "authorization" {
                                        auth = v.to_string();
                                    }
                                }
                            }
                            if body.len() < content_length {
                                while body.len() < content_length {
                                    let n = stream.read(&mut tmp).await.unwrap_or(0);
                                    if n == 0 {
                                        return;
                                    }
                                    body.extend_from_slice(&tmp[..n]);
                                }
                            }
                            reqs.lock().unwrap().push(Recorded {
                                method: method.clone(),
                                path: path.clone(),
                                query,
                                auth,
                                body: String::from_utf8_lossy(&body).into_owned(),
                            });
                            // 按 (method, path) 查预置响应；未命中 → 假 code=404，
                            // 让客户端走到 code 判定分支而不是连接层报错。
                            let resp = routes
                                .get(&(method, path))
                                .cloned()
                                .unwrap_or_else(|| json!({"code": 404}));
                            let resp_body = resp.to_string();
                            let resp_head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                                resp_body.len()
                            );
                            if stream.write_all(resp_head.as_bytes()).await.is_err() {
                                return;
                            }
                            if stream.write_all(resp_body.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            MockServer {
                base: format!("http://{addr}"),
                requests,
                handle,
            }
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    fn find_headers_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
    }
}
