//! 飞书协议层 —— 从 ABB `src/feishu.rs` 第 1–1165 行搬来的**渠道实现**（含 `#[cfg(test)]` 之前的全部内容）。
//!
//! 与原文件的差别只有三处（都为「脱离 ABB 进程、只做 MCP server」服务）：
//! 1. `crate::attachments::image_ext_uploadable` → 本 crate 的同名函数
//!    （[`image_ext_uploadable`]，逻辑照抄 `src/attachments.rs`：按扩展名白名单判，
//!    大小写不敏感，无后缀即 false）
//! 2. `crate::log!` → `eprintln!`（**必须走 stderr**：stdout 是 MCP 协议通道，
//!    任何一行杂输出都会让 ABB 侧的 JSON-RPC 解析炸掉）
//! 3. 文件头这份说明 + 模块级 `allow(dead_code)`（见下）
//!
//! **协议逻辑本身一行未改**——搬迁不是重写。原文件末尾的 `#[cfg(test)] mod tests`
//! 与本 crate 的测试无关（那些测试验的是 ABB 侧的 mock server 与 bridge 接线），
//! 本 crate 的契约测试在 `tests/contract.rs`，单测在 `src/main.rs` 底部。
//!
//! ## 关于 `dead_code`
//!
//! 这份文件从 ABB 整份搬来时挂着**模块级** `allow(dead_code)`，因为当时契约只用到
//! `send_text` / `reply_text` / `upload_image` / `upload_file` / `send_media_message`
//! 这五条，其余能力“能用但暂时没调用方”。注释里写了拆除条件：等入站附件下载
//! （`download_resource`）、引用消息（`get_quoted_message`）、表情
//! （`add_reaction`/`del_reaction`）与群管理依次接进契约之后移除。
//!
//! **那些能力已经接上了**（`get_quoted_message` / `download_attachment` / `create_chat` /
//! `user_display_name` / `typing` / `del_typing` / `done` / `chat_brief` / `get_chat_info`，
//! 见 `src/main.rs` 的 `tools()`），所以模块级 `allow` 已拆除，只剩逐条标记：
//! `bot_info` / `update_chat` / `delete_chat` / `send_text_to_user` / `scope_hint`
//! ——它们属于**群管理与虚拟 Bot 后台**的 GUI 流程，不在 MCP 契约里，且已超出
//! “可选能力”那一条的范围。只标记这五个而不是整个模块，警告才能继续当信号用。

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

/// 一条消息的定位结果：内容 + 「它回复的是哪条」（见 `get_message`）。
#[derive(Debug, Clone, Default)]
pub struct FeishuMessage {
    pub parsed: FeishuParsed,
    /// 回复消息的原消息 id；非回复消息为空串。
    pub parent_id: String,
}

/// 解析后的飞书消息内容：文本 + 资源列表（含富文本里全部图片/资源）。
#[derive(Debug, Clone, Default)]
pub struct FeishuParsed {
    pub text: String,
    pub resources: Vec<FeishuResource>,
    /// 消息**类型**（`msg_type`/`message_type`）。`parse_content` 不知道类型，恒留空；
    /// 由拿到整条消息对象的调用方填：引用拉取填 `items[0].msg_type`，入站路径在 `on_payload`
    /// 里用 `message.message_type` 补。
    pub msg_type: String,
    /// `post` 富文本里**未识别**的元素 tag（去重、保序、最多记 8 个）。
    ///
    /// 为什么要有：旧实现对未知 tag 是 `_ => {}` **静默丢弃** —— 若被引用内容/入站消息把
    /// 「引用块」之类元素以内嵌 tag 下发，日志上看不出任何痕迹，表现就是「读不到原消息」
    /// （2026-09-27 owner 实测提问）。记下来才能在日志里区分「没带 parent_id」与
    /// 「带了但内容元素我们不认」。
    pub unparsed_tags: Vec<String>,
}

/// 未识别元素 tag 的登记上限（防一条畸形消息刷爆日志）。
const UNPARSED_TAG_CAP: usize = 8;

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
            msg_type: String::new(),
            unparsed_tags: Vec::new(),
        };
    }
    if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
        return FeishuParsed {
            text: t.to_string(),
            resources: Vec::new(),
            msg_type: String::new(),
            unparsed_tags: Vec::new(),
        };
    }
    // post 富文本
    let mut text = String::new();
    let mut imgs: Vec<FeishuResource> = Vec::new();
    let mut unparsed: Vec<String> = Vec::new();
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
                    other => {
                        // 未识别元素：记下来（去重、限量），别静默丢掉——「引用块」这类元素
                        // 若以内嵌 tag 下发，正是「读不到原消息」的一种成因。
                        let tag = other.unwrap_or("").to_string();
                        if !tag.is_empty()
                            && !unparsed.contains(&tag)
                            && unparsed.len() < UNPARSED_TAG_CAP
                        {
                            unparsed.push(tag);
                        }
                    }
                }
            }
            text.push('\n');
        }
    }
    FeishuParsed {
        text: text.trim().to_string(),
        resources: imgs,
        msg_type: String::new(),
        unparsed_tags: unparsed,
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

/// /im/v1/files 允许的 file_type（飞书文档口径：opus/mp4/pdf/doc/xls/ppt/stream）。
/// 发送侧（#253）按文件名后缀映射；未知后缀回落通用 "stream"——消息仍以 file 卡片
/// 真实送达（验收只要求「收到真实文件」），更细的类型（如把 mp3 当 opus 语音卡）后续按需扩展。
/// 注意：mp4 是飞书为**视频**保留的容器类型，mov/mkv/webm 等异容器声明成 mp4
/// 会被服务端按内容校验拒掉（审查 #254）——一律回落 stream。
pub fn feishu_file_type(file_name: &str) -> &'static str {
    // 无点文件名（如名为 "pdf" 的文件）不能把整个名字当扩展名。
    let Some((_, ext)) = file_name.rsplit_once('.') else {
        return "stream";
    };
    match ext.trim().to_ascii_lowercase().as_str() {
        "pdf" => "pdf",
        "doc" | "docx" => "doc",
        "xls" | "xlsx" => "xls",
        "ppt" | "pptx" => "ppt",
        "mp4" => "mp4",
        _ => "stream",
    }
}

/// 能过飞书 images 端点格式校验的图片扩展名。
///
/// **与 ABB `src/attachments.rs::IMAGE_UPLOAD_EXTS` 同清单、同判定**（大小写不敏感、
/// 无后缀即 false）。搬进本 crate 后 ABI 依赖消失，但两份清单必须继续一致——
/// 判定漂移的表现是「ABB 侧认为能内联、渠道侧被判成文件卡片」，两边都还能跑，
/// 只是行为悄悄变了。改一处就同步另一处。
pub const IMAGE_UPLOAD_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// 文件名是否为可上传的内联图片扩展名（大小写不敏感）。
pub fn image_ext_uploadable(file_name: &str) -> bool {
    file_name
        .rsplit_once('.')
        .map(|(_, e)| e.trim().to_ascii_lowercase())
        .is_some_and(|e| IMAGE_UPLOAD_EXTS.contains(&e.as_str()))
}

/// 飞书 images 端点（msg_type=image 内联图）可靠渲染的图片扩展名。
/// 不在表内的「图片」（svg/ico/heic/tiff…，kind_from_name 会把它们归 image）
/// 走文件卡片更稳——images 上传被服务端格式校验拒时整个附件会失败（审查 #254）。
pub fn feishu_image_uploadable(file_name: &str) -> bool {
    image_ext_uploadable(file_name)
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
    ///
    /// 本 crate **目前没有**用它的测试（搬过来的那份 `#[cfg(test)] mod tests` 留在 ABB
    /// 侧，验的是 mock server 与 bridge 接线）。留着它是因为它是协议层唯一的「可注入基址」
    /// 入口：将来要在渠道侧做请求形状断言（假 green 清单第 4 条：没测试的能力等于没有），
    /// 得从它开始。标记 allow 而不是删掉——删了就只剩“以后再写一遍”。
    #[cfg(test)]
    #[allow(dead_code)]
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
            // 卡片 markdown + 纯文本降级（见 post_text_card_with_fallback：
            // API 级失败必须上报，不能让调用方以为发成功了；卡片被拒时还要保证内容能落地）。
            self.post_text_card_with_fallback(&token, "chat_id", chat_id, &body_text)
                .await?;
        }
        Ok(())
    }

    /// 发一段文本：**先卡片 markdown，失败则降级纯文本重发一次**。
    ///
    /// 为什么要降级（2026-09-17 实测）：ABB 的文本都走 `interactive` 卡片的 markdown
    /// 元素，而卡片对内容有额外约束——例如 markdown 里出现图片（`![alt](url)`）但没有
    /// `image_key` 时，服务端直接拒整条消息：
    /// `code=230099 … ext=ErrCode: 11310 the card contains images but no imagekey is passed in`。
    /// 调用方是任务结果/回复投递，被拒 = 用户什么都收不到，所以必须有兜底。
    /// 纯文本没有这些约束；降级成功会记一行日志（排障时能看到「这条是降级发出去的」）。
    async fn post_text_card_with_fallback(
        &self,
        token: &str,
        receive_id_type: &str,
        receive_id: &str,
        body_text: &str,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/im/v1/messages?receive_id_type={receive_id_type}"
        ));
        let card: serde_json::Value = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&json!({
                "receive_id": receive_id,
                "msg_type": "interactive",
                // 卡片 markdown 元素：飞书渲染 markdown 成富文本（对齐微信原生渲染）
                "content": serde_json::to_string(&json!({
                    "elements": [{"tag":"markdown","content": sanitize_card_markdown(body_text)}]
                }))?,
            }))
            .send()
            .await?
            .json()
            .await?;
        if card.get("code").and_then(|c| c.as_i64()) == Some(0) {
            return Ok(());
        }
        let card_err = format!("code={:?} msg={:?}", card.get("code"), card.get("msg"));
        let plain: serde_json::Value = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&json!({
                "receive_id": receive_id,
                "msg_type": "text",
                "content": serde_json::to_string(&json!({ "text": body_text }))?,
            }))
            .send()
            .await?
            .json()
            .await?;
        if plain.get("code").and_then(|c| c.as_i64()) == Some(0) {
            // 日志只能走 stderr：stdout 是 MCP 协议通道。
            eprintln!("[feishu] 卡片发送失败（{card_err}）→ 已降级为纯文本发出");
            return Ok(());
        }
        // 两条路都失败：如实上报（含卡片侧原因），调用方据此走回落/告警
        anyhow::bail!(
            "发送失败（卡片：{card_err}；纯文本降级亦失败：code={:?} msg={:?}）",
            plain.get("code"),
            plain.get("msg")
        );
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
                // 话题回复同样要兜底：卡片被拒（例如漏网的图片语法触发 230099/11310）
                // 时，用户在话题里会**一个字都收不到**。降级纯文本再试一次，
                // 两条都失败才如实上报（调用方据此走回落/告警）。
                let card_err = format!("code={:?} msg={:?}", resp.get("code"), resp.get("msg"));
                let plain: serde_json::Value = self
                    .http
                    .post(self.url(&format!("/im/v1/messages/{message_id}/reply")))
                    .bearer_auth(&token)
                    .json(&json!({
                        "msg_type": "text",
                        "content": serde_json::to_string(&json!({ "text": body_text }))?,
                        "reply_in_thread": true,
                    }))
                    .send()
                    .await?
                    .json()
                    .await?;
                if plain.get("code").and_then(|c| c.as_i64()) == Some(0) {
                    eprintln!("[feishu] 话题回复卡片失败（{card_err}）→ 已降级为纯文本发出");
                    continue;
                }
                anyhow::bail!(
                    "回复失败（卡片：{card_err}；纯文本降级亦失败：code={:?} msg={:?}）",
                    plain.get("code"),
                    plain.get("msg")
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
        Ok(self.get_message(message_id).await?.parsed)
    }

    /// 拉取一条消息的**完整定位信息**：内容解析结果 + `parent_id`。
    ///
    /// `parent_id` 是「这条消息回复的是哪条」——MCP 侧的 `download_attachment` 只能拿到
    /// `{message_id, seq}`（ABB 的契约就这么宽），而引用消息的附件用 `seq >= 100` 表示
    /// （与内置渠道同约定：`bridge/mod.rs` 里 `100 + i`）。渠道要自己从 `parent_id`
    /// 反查原消息，才能把那个 `file_key` 找出来。
    pub async fn get_message(&self, message_id: &str) -> Result<FeishuMessage> {
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
        let mut parsed = parse_content(raw);
        // 消息的类型：内容解析不出东西时，这行是唯一能说明「为什么读不到」的线索
        // （例如 merge_forward / sticker / 卡片类消息）。
        parsed.msg_type = item["msg_type"].as_str().unwrap_or("").to_string();
        let parent_id = item["parent_id"].as_str().unwrap_or("").to_string();
        Ok(FeishuMessage { parsed, parent_id })
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

    /// 非 2xx / 非 JSON 的 API 响应统一转可诊断错误（审查 #254：直接 `.json()`
    /// 会把网关的 413/502 HTML 塌成「响应非 JSON」，丢光状态码）。
    async fn api_json(resp: reqwest::Response, label: &str) -> Result<serde_json::Value> {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .context(format!("{label}：读响应体失败"))?;
        if !status.is_success() {
            let detail = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .map(|v| format!("code={:?} msg={:?}", v.get("code"), v.get("msg")))
                .unwrap_or_else(|| body.chars().take(200).collect());
            anyhow::bail!("{label}失败 (HTTP {status}): {detail}");
        }
        serde_json::from_str(&body).context(format!("{label}：响应非 JSON（HTTP {status}）"))
    }

    /// 按扩展名给 multipart part 的 Content-Type（飞书 images 端点校验格式；
    /// 表内没有的回落 octet-stream，服务端仍以文件名嗅探为准）。
    fn mime_from_name(name: &str) -> String {
        let ext = name.rsplit('.').next().filter(|e| *e != name).unwrap_or("");
        match ext.to_ascii_lowercase().as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "tiff" | "tif" => "image/tiff",
            "svg" => "image/svg+xml",
            _ => "application/octet-stream",
        }
        .to_string()
    }

    /// 上传一张图片（发消息用）→ image_key。表单字段对齐飞书开放平台文档与 open-lark
    /// 参考实现：POST /im/v1/images，multipart `image_type=message` + `image=<二进制>`。
    /// 图片必须走 images 端点拿 image_key，不能走 /im/v1/files（那是 file_key）。
    /// `file_name` 带真实扩展名：服务端按文件名嗅探格式，固定 "image" 会被判
    /// 234001/234011（审查 #254）。上传 120s 超时（全局 30s 会掐断大图）。
    pub async fn upload_image(&self, bytes: Vec<u8>, file_name: &str) -> Result<String> {
        let token = self.tenant_token().await?;
        let form = reqwest::multipart::Form::new()
            .text("image_type", "message")
            .part(
                "image",
                reqwest::multipart::Part::bytes(bytes)
                    .file_name(file_name.to_string())
                    .mime_str(&Self::mime_from_name(file_name))
                    .context("设置图片 part Content-Type 失败")?,
            );
        let resp = self
            .http
            .post(self.url("/im/v1/images"))
            .bearer_auth(&token)
            .timeout(std::time::Duration::from_secs(120))
            .multipart(form)
            .send()
            .await
            .context("飞书图片上传网络错误")?;
        let value: serde_json::Value = Self::api_json(resp, "飞书图片上传").await?;
        let key = value["data"]["image_key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if value.get("code").and_then(|c| c.as_i64()) != Some(0) || key.is_none() {
            anyhow::bail!(
                "飞书图片上传失败 code={:?} msg={:?}",
                value.get("code"),
                value.get("msg")
            );
        }
        Ok(key.unwrap())
    }

    /// 上传一个普通文件 → file_key（非图片：文档/压缩包/音频/视频等）。
    /// POST /im/v1/files，multipart `file_type` + `file_name` + `file=<二进制>`。
    pub async fn upload_file(
        &self,
        file_type: &str,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<String> {
        let token = self.tenant_token().await?;
        let form = reqwest::multipart::Form::new()
            .text("file_type", file_type.to_string())
            .text("file_name", file_name.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(bytes).file_name(file_name.to_string()),
            );
        let resp = self
            .http
            .post(self.url("/im/v1/files"))
            .bearer_auth(&token)
            .timeout(std::time::Duration::from_secs(120))
            .multipart(form)
            .send()
            .await
            .context("飞书文件上传网络错误")?;
        let value: serde_json::Value = Self::api_json(resp, "飞书文件上传").await?;
        let key = value["data"]["file_key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if value.get("code").and_then(|c| c.as_i64()) != Some(0) || key.is_none() {
            anyhow::bail!(
                "飞书文件上传失败 code={:?} msg={:?}",
                value.get("code"),
                value.get("msg")
            );
        }
        Ok(key.unwrap())
    }

    /// 发一条携带媒体 key 的消息（#253）。image → msg_type=image（内联图）；
    /// 其它一律 msg_type=file（**下载卡片**——飞书真正的语音条要 msg_type=audio
    /// +opus 上传、视频要 msg_type=media +封面 image_key，一期不做，注释与实现
    /// 对齐；content 按官方 schema 只带 key，自造 type/file_name 字段有被服务端
    /// 校验拒的风险——审查 #254）。
    pub async fn send_media_message(&self, chat_id: &str, msg_type: &str, key: &str) -> Result<()> {
        let content = if msg_type == "image" {
            json!({ "image_key": key })
        } else {
            json!({ "file_key": key })
        };
        let token = self.tenant_token().await?;
        // 与上传路径同款 `api_json`：网关 5xx/限流返回 HTML 时也要保住 HTTP 状态码，
        // 不能塌成「响应非 JSON」（审查 #254 P2-5：同一条意见原先只修了上传两处）。
        let resp = Self::api_json(
            self.http
                .post(self.url("/im/v1/messages?receive_id_type=chat_id"))
                .bearer_auth(&token)
                .json(&json!({
                    "receive_id": chat_id,
                    "msg_type": msg_type,
                    "content": serde_json::to_string(&content)?,
                }))
                .send()
                .await
                .context("飞书媒体消息发送网络错误")?,
            "飞书媒体消息发送",
        )
        .await?;
        if resp.get("code").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "飞书媒体消息发送失败 code={:?} msg={:?}",
                resp.get("code"),
                resp.get("msg")
            );
        }
        Ok(())
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
            eprintln!(
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
    #[allow(dead_code)] // 托盘设置窗的自动填充流程不在 MCP 契约里（见文件头 dead_code 说明）
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
                eprintln!(
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
    /// 会话简报：chat_type（"p2p"/"group"）+ 会话名。历史回填任务用——
    /// 与 get_chat_info 同一端点，但取 chat_type 字段（群/私聊判定源）。
    pub async fn chat_brief(&self, chat_id: &str) -> Result<(String, String)> {
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
            anyhow::bail!("查询会话失败 code={:?}", resp.get("code"));
        }
        let ctype = resp["data"]["chat_type"].as_str().unwrap_or("").to_string();
        let name = resp["data"]["name"].as_str().unwrap_or("").to_string();
        if ctype.is_empty() {
            anyhow::bail!("响应缺 chat_type");
        }
        Ok((ctype, name))
    }

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
        // 已解散的群 API 不报错：返回 200 + chat_status="dissolved"（8-20 真机实测，
        // 用户解散全部虚拟 Bot 群后逐个 GET 确认）。识别为错误——虚拟 Bot 手动刷新
        // 据此移除登记+归档；注入路径 best-effort 失败也无害（dissolved 群无消息）。
        if resp["data"]["chat_status"].as_str() == Some("dissolved") {
            anyhow::bail!("群已解散（chat_status=dissolved）");
        }
        Ok((name, desc))
    }

    /// 改群资料（虚拟 Bot 编辑：群名=角色名、群介绍=system prompt，即时生效）。
    /// ⚠️ 方法是 **PUT** 不是 PATCH（8-20 真机实测：PATCH /im/v1/chats/:id 返回
    /// 404 page not found——mock 只验形状不验路由，假绿；lark-cli im.chats.update
    /// dry-run 显示 PUT）。owner_id 字段可顺带转让群主（新群主必须在群里）。
    #[allow(dead_code)] // 虚拟 Bot 编辑：群资料修改（GUI 流程，不是契约能力）
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
    #[allow(dead_code)] // 虚拟 Bot 解散：不可恢复动作，只应由 GUI 强确认触发
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
    #[allow(dead_code)] // 主动给用户发消息（虚拟 Bot 流程；MCP 契约只按 chat_id 发）
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
            // 同 send_text：卡片优先 + 纯文本降级（授权指引这类关键通知更不能因为
            // 「卡片里带了图片语法」整条被拒）。
            self.post_text_card_with_fallback(&token, "open_id", open_id, &body_text)
                .await?;
        }
        Ok(())
    }
}

/// 飞书权限不足错误识别（8-20 实测：code=99991672 Access denied，错误文本带 scope
/// 列表与授权链接）。返回 (需要的 scope 摘要, 授权链接)。ABB 用它给 owner 发授权指引——
/// 平台权限问题不该只躺在日志/状态行里，owner 需要可执行的下一步。
/// 参数是任意 API 失败后的完整错误字符串（含 code=… msg=…，见各方法 bail 格式）；
/// 检测「99991672 或 Access denied」出现才解析，避免误判其它错误码。
#[allow(dead_code)] // 权限报错的「怎么开通」提示，托盘侧用；本 crate 只向 stderr 报原因
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
            "elements": [{"tag":"markdown","content": sanitize_card_markdown(body_text)}]
        })).unwrap_or_default(),
        "reply_in_thread": true,
    })
}

/// 按字符数（不是字节）逐行贪心分段，对齐 Python 的 len() 语义。
/// 代码块 fence（```）保护：超限时只在非代码块状态切，避免代码块跨段断裂
/// （飞书/微信分段发多条时，代码块被切到两条会破坏渲染）。三端共用此分段
/// （飞书/微信 3500、钉钉 8000），故保护对三端均生效。
/// 卡片 markdown 的安全化：把**图片节点**降级成文字。
///
/// 为什么必须做（2026-09-17 实测）：ABB 所有文本都走飞书 `interactive` 卡片的 markdown
/// 元素，而卡片里的图片必须先用 `im/v1/images` 上传换到 `image_key`。`![alt](url)` /
/// `<img src=…>` 这种「URL 型图片」服务端不认——它判定「卡片包含图片但没有 imagekey」，
/// **整条消息被拒**：
/// ```text
/// code=Some(Number(230099)) … ext=ErrCode: 11310
/// the card contains images but no imagekey is passed in
/// ```
/// 任务结果/回复一旦被拒，用户就**什么都收不到**（只剩一条投递失败告警）。
///
/// 实现方式：**用 CommonMark parser 定位图片节点，再按字节区间回填**——不再手写词法扫描。
/// 手写版连续两轮被独立审查证伪（多反引号行内代码、四反引号围栏里嵌三反引号、行中三反引号
/// 误开围栏、代码块里的 `<!--` 污染注释状态、引用式图片误吞普通文本……），根因是「markdown
/// 惰性上下文」本身就是 CommonMark 语法，必须交给 parser：
/// - `Parser::new(text).into_offset_iter()` 给出每个事件的**源字节区间**；
/// - 只有 `Tag::Image` 与内含 `<img` 的 HTML 事件才改写，其余区间**逐字照抄**——
///   代码围栏 / 行内代码 / 转义 / 注释里的 `![` 因此天然不会被误改（parser 不把它们当图片）；
/// - 引用式 `![a][ref]`、shortcut `![a]` 也由 parser 统一识别，不会漏也不会误吞正文。
///
/// 保留与闭合约定：
/// - `![hover](img_v3_xxx)` 是「已上传图片」的合法写法，**原样保留**（只降级 URL/未知引用）；
/// - 回填的 alt/url 都先经 `neutralize_image_syntax`，保证一次 sanitize 之后输出里不再有
///   可解析的 `![` / `<img`（含 `![<img src=x>](url)` 这种 alt 逃逸）。
///
/// 真要发图请走 `deliver --file`（那条通道会真的上传并拿 image_key）。
pub(crate) fn sanitize_card_markdown(text: &str) -> String {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    let mut out = String::with_capacity(text.len());
    let mut copied = 0usize; // 已照抄到的字节位置
    let mut img_start = 0usize;
    let mut img_url = String::new();
    let mut alt = String::new();
    let mut depth = 0usize; // >0 = 正在图片节点内部

    for (event, range) in Parser::new(text).into_offset_iter() {
        if depth > 0 {
            match event {
                Event::Start(Tag::Image { .. }) => depth += 1,
                Event::Text(t) | Event::Code(t) => alt.push_str(&t),
                Event::SoftBreak | Event::HardBreak => alt.push(' '),
                Event::End(TagEnd::Image) => {
                    depth -= 1;
                    if depth == 0 {
                        if is_real_image_key(&img_url) {
                            out.push_str(&text[img_start..range.end]);
                        } else {
                            emit_image_text(&mut out, alt.trim(), &img_url);
                        }
                        alt.clear();
                        copied = range.end;
                    }
                }
                _ => {}
            }
            continue;
        }
        match &event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                out.push_str(&text[copied..range.start]);
                img_start = range.start;
                img_url = dest_url.to_string();
                alt.clear();
                depth = 1;
            }
            // HTML 形式的内联/块级 `<img …>`：pulldown-cmark 不把它当图片节点，
            // 但飞书卡片会（11310 就是这么来的），所以同样要中和。
            // ⚠️ **只中和 `<img`，不能整块替换**：`Event::Html` 的区间可能是一整段
            // 块级 HTML（含注释/多个标签），整块换成「（图片）」会**丢正文**（独立审查指出）。
            Event::InlineHtml(raw) | Event::Html(raw)
                if raw.to_ascii_lowercase().contains("<img") =>
            {
                out.push_str(&text[copied..range.start]);
                // 只中和 HTML 标签形式的 `<img`：这段区间里的 `![` 是**字面量**
                // （HTML 块/注释不是 markdown），一起改写就是内容损坏（独立审查 P1）。
                out.push_str(&neutralize_html_img_tags(raw));
                copied = range.end;
            }
            _ => {}
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// `img_` 开头的、没有空白的引用 = 已经上传好的 image_key（卡片规范允许），原样保留。
fn is_real_image_key(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("img_") && !u.chars().any(char::is_whitespace)
}

/// 把图片改写成文字，并**中和**回填内容里的图片语法（保证一次 sanitize 之后闭合）。
fn emit_image_text(out: &mut String, alt: &str, url: &str) {
    let alt = neutralize_image_syntax(alt.trim());
    out.push_str(if alt.is_empty() {
        "图片"
    } else {
        alt.as_str()
    });
    out.push_str("（图片：");
    out.push_str(&neutralize_image_syntax(url.trim()));
    out.push('）');
}

/// 只中和 HTML 标签形式的 `<img`（大小写不敏感 → `< img`），**不动 `![`**。
/// 用于 HTML 事件区间：那里的 `![a](b)` 是字面量文本，不是 markdown 图片。
fn neutralize_html_img_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < s.len() {
        if bytes[i] == b'<' && s.len() - i >= 4 && bytes[i..i + 4].eq_ignore_ascii_case(b"<img") {
            out.push_str("< img");
            i += 4;
            continue;
        }
        let ch = s[i..].chars().next().expect("i 落在字符边界");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 把可能残留的图片语法中和成纯文本（`![` → `[`，`<img` → `< img`）。
/// alt 与 url 回填都要过这一关，否则 sanitizer 自己会把图片节点放回卡片。
fn neutralize_image_syntax(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < s.len() {
        if bytes[i] == b'!' && bytes.get(i + 1) == Some(&b'[') {
            out.push('[');
            i += 2;
            continue;
        }
        if bytes[i] == b'<' && s.len() - i >= 4 && bytes[i..i + 4].eq_ignore_ascii_case(b"<img") {
            out.push_str("< img");
            i += 4;
            continue;
        }
        let ch = s[i..].chars().next().expect("i 落在字符边界");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

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
