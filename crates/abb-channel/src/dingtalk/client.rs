//! 钉钉协议层 —— 从 ABB `src/dingtalk.rs` 第 1–985 行搬来的**渠道实现**（`parse_message`
//! 与两个连接期辅助函数为止，即 Stream 连接循环之前的一切）。
//!
//! 与原文件的差别只有四处（都为「脱离 ABB 进程、只做 MCP server」服务）：
//! 1. `use crate::bridge::Bridge;` → **删除**：本 crate 不认识 Bridge，入站走 MCP 缓冲
//!    （见 `src/wsin.rs`）。连带 `Arc` 也不再需要。
//! 2. `futures_util` / `tokio_tungstenite` 的 import → **删除**：它们只服务被搬走的
//!    `stream_loop`/`run_conn`（现在在 `src/wsin.rs`），连同两个连接期常量
//!    `STALL_AFTER` / `KEEPALIVE_INTERVAL` 一起搬过去。
//! 3. `crate::log!` → `eprintln!`、`crate::botstatus::report(...)` → **整句删除**（共 11 处，
//!    见 `src/wsin.rs` 文件头：那 11 处都在连接循环里，随循环一起搬到 wsin.rs 改写）。
//! 4. 文件头这份说明 + 模块级 `allow(dead_code)`（见下）。
//!
//! **协议逻辑本身一行未改**——搬迁不是重写。原文件末尾的 `#[cfg(test)] mod tests`
//! 与本 crate 的测试无关（那些测试验的是 ABB 侧的 mock server 与 bridge 接线），
//! 本 crate 的契约测试在 `tests/contract.rs`，单测在 `src/main.rs` 与 `src/wsin.rs` 底部。
//!
//! ## 关于 `allow(dead_code)`
//!
//! 搬过来的这 985 行里，**群管理/附件下载已经全部接进了 MCP 契约**（可选能力那一批）：
//! `download_msg_file`（`download_attachment`）、`create_chat`、`get_chat_info`、
//! `user_name` 都有了调用方。**只剩 `update_chat`（改群名）没有契约入口**——可选能力
//! 清单里没有「改群名」，所以它现在只挂着一条 `#[allow(dead_code)]`（在函数自己身上，
//! 不是整个模块），将来若要接进契约，解除条件就是把那条 allow 去掉。
//! 与飞书版同一处置口径（`crates/abb-channel-feishu/src/feishu.rs` 文件头）。

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

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const API_BASE: &str = "https://api.dingtalk.com";
/// 旧网关（topapi 系列接口）：鉴权走 query 参数 `?access_token=`，不是 v1.0 的头鉴权。
/// 虚拟 Bot 群管理（#75）走这里——「创建群会话 topapi/im/chat/create」等接口只在旧网关注册。
const OAPI_BASE: &str = "https://oapi.dingtalk.com";
/// 单条文本安全长度（按字符数）。钉钉文本消息上限约 2 万字节，这里保守取 8000 字符，
/// 与飞书分段逻辑同一套（split_text 按字符逐行贪心）。
pub const DINGTALK_MSG_LIMIT: usize = 8000;
// 半开看门狗阈值 `STALL_AFTER` 与 `KEEPALIVE_INTERVAL`（原在本文件 41–45 行）随连接循环
// 一并搬到 `src/wsin.rs`——它们是**连接**的参数，不是协议参数。

/// 钉钉客户端：access_token 缓存 + 收发消息 + 注册 Stream 连接。
pub struct DingTalkClient {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    token: Mutex<Option<(String, Instant)>>,
    /// v1.0 OpenAPI 基址（mock 单测注入本地服务器用；生产恒为 API_BASE）。
    base: String,
    /// 旧网关（topapi）基址（mock 单测同指向本地；生产恒为 OAPI_BASE）。
    oapi_base: String,
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
            base: API_BASE.to_string(),
            oapi_base: OAPI_BASE.to_string(),
        }
    }

    /// 测试用：两个基址都指向本地 mock 服务器（请求形状断言）。与 new 的唯一差异。
    #[cfg(test)]
    pub(crate) fn with_base(app_id: &str, app_secret: &str, base: &str) -> DingTalkClient {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        DingTalkClient {
            http,
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            token: Mutex::new(None),
            base: base.to_string(),
            oapi_base: base.to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn oapi_url(&self, path: &str, token: &str) -> String {
        format!("{}{}?access_token={}", self.oapi_base, path, token)
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
            .post(self.url("/v1.0/oauth2/accessToken"))
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

    /// 按 staffId 反查用户姓名（授权码消费后展示用）。best-effort：token/API 失败或用户不可见
    /// 返回 None，调用方用 staffId 兜底显示。
    pub async fn user_name(&self, staff_id: &str) -> Option<String> {
        let token = self.access_token().await.ok()?;
        let url = self.url(&format!("/v1.0/contact/users/{staff_id}"));
        let resp: Value = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        // 钉钉用户对象：name / nick 字段（不同版本字段有差异，取第一个非空）
        let obj = resp.get("result").or_else(|| resp.get("data"))?;
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let nick = obj
            .get("nick")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        name.or(nick).map(|s| s.to_string())
    }

    /// v1.0 机器人消息统一发送：群聊走 groupMessages，单聊走 oToMessages。
    /// 文本与所有媒体模板共用请求形状；单聊额外检查 `invalidStaffIdList`，
    /// 防止服务端 200 但 staffId 无效时被误报为成功。
    async fn send_robot_message(
        &self,
        chat_id: &str,
        robot_code: &str,
        msg_key: &str,
        msg_param: Value,
        group_at: Option<Value>,
    ) -> Result<()> {
        let token = self.access_token().await?;
        let is_group = is_group_chat(chat_id);
        let (path, mut body) = if is_group {
            (
                "/v1.0/robot/groupMessages/send",
                json!({
                    "robotCode": robot_code,
                    "openConversationId": chat_id,
                    "msgKey": msg_key,
                    "msgParam": serde_json::to_string(&msg_param)?,
                }),
            )
        } else {
            (
                "/v1.0/robot/oToMessages/batchSend",
                json!({
                    "robotCode": robot_code,
                    "userIds": [chat_id],
                    "msgKey": msg_key,
                    "msgParam": serde_json::to_string(&msg_param)?,
                }),
            )
        };
        if is_group {
            if let Some(at) = group_at.filter(|v| !v.is_null()) {
                body["at"] = at;
            }
        }
        let resp = self
            .http
            .post(self.url(path))
            .header("x-acs-dingtalk-access-token", &token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text_body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text_body));
        }
        if !is_group {
            let v: Value = serde_json::from_str(&text_body).unwrap_or(Value::Null);
            let invalid = v["invalidStaffIdList"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            if invalid > 0 {
                anyhow::bail!("钉钉单聊发送失败：{invalid} 个用户 ID 无效（staffId={chat_id}）");
            }
            let flow_controlled = v["flowControlledStaffIdList"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            if flow_controlled > 0 {
                anyhow::bail!(
                    "钉钉单聊发送失败：{flow_controlled} 个用户被平台限流（staffId={chat_id}）"
                );
            }
        }
        Ok(())
    }

    /// 发单聊文本（userIds 批量接口，单元素）。
    pub async fn send_single(&self, user_id: &str, robot_code: &str, text: &str) -> Result<()> {
        self.send_robot_message(
            user_id,
            robot_code,
            "sampleText",
            json!({"content": text}),
            None,
        )
        .await
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
        let (content, at) = match at_user_id.filter(|u| !u.is_empty()) {
            Some(uid) => (
                format!("@{uid} {text}"),
                json!({"atUserIds": [uid], "isAtAll": false}),
            ),
            None => (text.to_string(), Value::Null),
        };
        self.send_robot_message(
            conversation_id,
            robot_code,
            "sampleText",
            json!({"content": content}),
            Some(at),
        )
        .await
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
            .post(self.url("/v1.0/robot/messageFiles/download"))
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

    /// 上传媒体文件 → media_id。
    /// 走旧网关 app 级媒体上传 POST /media/upload?type=<image|voice|file|video>&access_token=…，
    /// multipart 字段名固定 `media`（服务端缺 multipart 时报 43008「参数需要multipart类型」——
    /// 已用无鉴权 POST 探测确认该路由真实存在）。返回 media_id（形如 "@…"）。
    /// `file_name` 必须带真实扩展名：钉钉按文件名后缀校验媒体类型，固定名 "image"
    /// 会被判 40004「不合法的媒体文件类型」（审查 #254）。上传走 120s 超时
    /// （全局 30s 会掐断大图；与下载路径同预算）。
    pub async fn upload_media(
        &self,
        media_type: &str,
        bytes: Vec<u8>,
        file_name: &str,
    ) -> Result<String> {
        debug_assert!(matches!(media_type, "image" | "voice" | "file" | "video"));
        let token = self.access_token().await?;
        let url = format!(
            "{}{}?type={}&access_token={}",
            self.oapi_base, "/media/upload", media_type, token
        );
        let form = reqwest::multipart::Form::new().part(
            "media",
            reqwest::multipart::Part::bytes(bytes).file_name(file_name.to_string()),
        );
        let resp: Value = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(120))
            .multipart(form)
            .send()
            .await
            .context("钉钉媒体上传网络错误")?
            .json()
            .await
            .context("钉钉媒体上传响应非 JSON")?;
        if resp.get("errcode").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "钉钉媒体上传失败 errcode={:?} errmsg={:?}",
                resp.get("errcode"),
                resp.get("errmsg")
            );
        }
        resp.get("media_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .context("钉钉媒体上传响应缺 media_id")
    }

    /// 上传一张图片（兼容既有调用）→ media_id。
    pub async fn upload_image(&self, bytes: Vec<u8>, file_name: &str) -> Result<String> {
        self.upload_media("image", bytes, file_name).await
    }

    /// 发送图片消息（机器人 msgKey=sampleImageMsg，群聊/单聊同模板）。官方文档该 msgKey 的
    /// msgParam 字段是 **photoURL**（可填完整 URL 或 media/upload 返回的 media_id，
    /// 见 open.dingtalk.com 消息类型文档）；`picMediaId` 是 sampleVideoMsg 的封面
    /// 字段，用错会 400 miss.param.photo（审查 #254）。
    pub async fn send_image_message(
        &self,
        chat_id: &str,
        robot_code: &str,
        media_id: &str,
    ) -> Result<()> {
        self.send_robot_message(
            chat_id,
            robot_code,
            "sampleImageMsg",
            json!({ "photoURL": media_id }),
            None,
        )
        .await
    }

    /// 发送普通文件消息（机器人群聊/单聊同模板 sampleFile）。
    pub async fn send_file_message(
        &self,
        chat_id: &str,
        robot_code: &str,
        media_id: &str,
        file_name: &str,
        file_type: &str,
    ) -> Result<()> {
        self.send_robot_message(
            chat_id,
            robot_code,
            "sampleFile",
            json!({
                "mediaId": media_id,
                "fileName": file_name,
                "fileType": file_type,
            }),
            None,
        )
        .await
    }

    /// 创建群会话（虚拟 Bot #75；topapi/im/chat/create 的文档形状）。
    ///
    /// ⚠️ 能力边界（重要）：该接口要求 `owner`（群主 userId）+ `useridlist`（至少一名
    /// 成员），两项都不得为空。内置版当时没有 owner 参数、只能传空值，所以真实建群
    /// 大概率失败，ABB 靠「登记制降级」兜底（用户平台手动建群→用 GUI 手动登记 chat_id）。
    /// MCP 契约把 `owner_user_id` 一并给了渠道（§3.4 `create_chat`），所以这里把它填进
    /// `owner`，并**同时放进 `useridlist`**（群主本身就是成员）：不给成员列表的请求必然
    /// 被平台拒，而契约手上只有这一个 userid。
    ///
    /// 失败仍由 ABB 走登记制降级：错误原样上报，不假装建成功。
    /// `description` 参数保留仅为对齐契约签名：钉钉群没有群介绍字段，忽略。
    pub async fn create_chat(
        &self,
        name: &str,
        _description: &str,
        owner_user_id: &str,
    ) -> Result<String> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .post(self.oapi_url("/topapi/im/chat/create", &token))
            .json(&create_chat_body(name, owner_user_id))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text));
        }
        let v: Value =
            serde_json::from_str(&text).context("topapi/im/chat/create 响应不是 JSON")?;
        if v.get("errcode").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "钉钉创建群失败 errcode={:?} errmsg={:?}",
                v.get("errcode"),
                v.get("errmsg")
            );
        }
        let chatid = v
            .get("chatid")
            .or_else(|| v.get("data").and_then(|d| d.get("chatid")))
            .and_then(|c| c.as_str())
            .context("topapi/im/chat/create 响应缺 chatid")?
            .to_string();
        Ok(chatid)
    }

    /// 群信息（topapi/im/chat/get 的文档形状）：返回 (群名, 群介绍)。
    /// 钉钉群**没有**「群介绍」字段——desc 恒空（平台限制：虚拟 Bot 的 system prompt
    /// 在钉钉上只能靠群名注入；如需完整提示词需平台支持，见模块注释与 PR 说明）。
    /// best-effort 语义与飞书一致：失败返回 Err，调用方只 log 不阻塞。
    pub async fn get_chat_info(&self, chat_id: &str) -> Result<(String, String)> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .post(self.oapi_url("/topapi/im/chat/get", &token))
            .json(&json!({"chatid": chat_id}))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text));
        }
        let v: Value = serde_json::from_str(&text).context("topapi/im/chat/get 响应不是 JSON")?;
        if v.get("errcode").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "钉钉查询群信息失败 errcode={:?} errmsg={:?}",
                v.get("errcode"),
                v.get("errmsg")
            );
        }
        let name = v["chat_info"]["name"].as_str().unwrap_or("").to_string();
        Ok((name, String::new()))
    }

    /// 改群名（topapi/im/chat/update）：钉钉可改的只有 name（无群介绍字段，desc 忽略）。
    /// 与 create_chat 同需用户实测：机器人改群名权限取决于机器人权限配置。
    ///
    /// **目前没有契约入口**（可选能力清单里没有「改群名」），所以是这个模块唯一
    /// 带 `allow(dead_code)` 的函数——不是整个模块放宽。
    #[allow(dead_code)]
    pub async fn update_chat(&self, chat_id: &str, name: &str, _description: &str) -> Result<()> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .post(self.oapi_url("/topapi/im/chat/update", &token))
            .json(&json!({"chatid": chat_id, "name": name}))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text));
        }
        let v: Value =
            serde_json::from_str(&text).context("topapi/im/chat/update 响应不是 JSON")?;
        if v.get("errcode").and_then(|c| c.as_i64()) != Some(0) {
            anyhow::bail!(
                "钉钉更新群失败 errcode={:?} errmsg={:?}",
                v.get("errcode"),
                v.get("errmsg")
            );
        }
        Ok(())
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

/// `topapi/im/chat/create` 的请求体。
///
/// 抽成纯函数是为了**单测能验真实请求形状**（钉钉侧对 `owner` / `useridlist` 都有
/// 非空要求，写错只需要一次真实调用才发现，代价太大）。
pub fn create_chat_body(name: &str, owner_user_id: &str) -> Value {
    json!({
        "name": name,
        "owner": owner_user_id,
        // 群主本身就是成员：只给 owner 不给 useridlist 的请求会被平台拒。
        "useridlist": [owner_user_id],
    })
}

/// 钉钉图片上传支持的扩展名（官方 /media/upload 口径：jpg、gif、png、bmp；
/// jpeg 是 jpg 的常见别名，一并接受）。`kind_from_name` 会把 svg/ico/heic/webp
/// 也归成 image，但钉钉服务端会拒；在能力闸就拦下，别把原始错误丢给用户。
pub const DINGTALK_IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "gif", "png", "bmp"];

pub fn dingtalk_image_uploadable(file_name: &str) -> bool {
    extension_lower(file_name).is_some_and(|e| DINGTALK_IMAGE_EXTS.contains(&e.as_str()))
}

/// 钉钉 `sampleFile` 消息模板端到端支持的扩展名。注意 `/media/upload?type=file`
/// 本身还接受 xls/ppt/pptx，但消息模板官方文档未保证，能力闸按**发送链最窄集合**
/// 收口，避免上传成功后才在消息发送阶段失败。
pub const DINGTALK_FILE_EXTS: &[&str] = &["doc", "docx", "xlsx", "pdf", "zip", "rar"];

/// 钉钉文件模板的 `fileType`：支持类型返回规范化扩展名，其余 None。
pub fn dingtalk_file_type(file_name: &str) -> Option<&'static str> {
    let ext = extension_lower(file_name)?;
    DINGTALK_FILE_EXTS
        .iter()
        .copied()
        .find(|candidate| *candidate == ext)
}

fn extension_lower(file_name: &str) -> Option<String> {
    file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.trim().to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
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
    /// 群名（conversationTitle；单聊事件通常没有）。虚拟 Bot #75 注入用：
    /// 群名=角色名，事件携带的群名比 API 查询更新（改群名即时生效）。
    pub conversation_title: String,
    /// 文本内容（已 trim；群聊含 @机器人名 前缀，由 bridge 剥）。纯附件消息为空。
    pub text: String,
    /// 是否在 @ 名单里（群聊判据）。
    ///
    /// 本 crate **不消费**它：准入判断（「群里没 @ 机器人要不要应答」）归 ABB，
    /// 渠道只负责把入站帧翻译成契约消息。留着是因为它是 `parse_message` 的完整产物，
    /// 删掉等于让协议层少解一个字段（将来 ABB 若要按它判准入，契约里要有它）。
    #[allow(dead_code)]
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
            // 语音转写文本（recognition）记进 voice_text，与主消息路径一致
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
                content["recognition"].as_str().unwrap_or("").to_string(),
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
pub(crate) fn parse_message(frame: &Value) -> Option<DingtalkMessage> {
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
        conversation_title: p["conversationTitle"].as_str().unwrap_or("").to_string(),
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
pub(crate) fn ack_json(message_id: &str, data: &str) -> String {
    json!({
        "code": 200,
        "headers": {"contentType": "application/json", "messageId": message_id},
        "message": "OK",
        "data": data,
    })
    .to_string()
}

/// query 参数百分号编码（ticket 可能含 base64 的 +/=）。只放行 URL 安全字符。
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// 一次性 mock HTTP 服务器：按顺序回 `bodies` 里的 JSON，并记录收到的
    /// `<请求行> <请求体>`（断言请求形状用）。
    ///
    /// 只为了让 `with_base` 有真实用途：建群请求的形状（`owner` / `useridlist`
    /// 必须非空）错了要真调一次钉钉才发现，而那是**有副作用**且需要凭证的调用。
    fn mock_http(bodies: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑本地端口");
        let base = format!("http://{}", listener.local_addr().expect("取本地地址"));
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for body in bodies {
                let (mut sock, _) = listener.accept().expect("接受连接");
                let mut reader = BufReader::new(sock.try_clone().expect("复制 socket"));
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                let len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut buf = vec![0u8; len];
                if len > 0 {
                    reader.read_exact(&mut buf).expect("读请求体");
                }
                seen.push(format!(
                    "{} {}",
                    head.lines().next().unwrap_or(""),
                    String::from_utf8_lossy(&buf)
                ));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(resp.as_bytes()).expect("回响应");
                sock.flush().expect("flush");
            }
            seen
        });
        (base, handle)
    }

    /// 建群的真实请求：先换 token，再带 `owner` + `useridlist`（两项都非空才可能建成功），
    /// 旧网关的鉴权走 query 参数。
    #[tokio::test]
    async fn create_chat_request_carries_owner_and_member_list() {
        let (base, seen) = mock_http(vec![
            r#"{"accessToken":"tok","expireIn":7200}"#,
            r#"{"errcode":0,"chatid":"cid_new"}"#,
        ]);
        let client = DingTalkClient::with_base("app", "secret", &base);
        let chat_id = client
            .create_chat("后端开发", "提示词", "staff_1")
            .await
            .expect("建群应成功");
        assert_eq!(chat_id, "cid_new");
        let seen = seen.join().expect("mock 线程");
        assert_eq!(seen.len(), 2, "应当只有换 token + 建群两次请求：{seen:?}");
        assert!(
            seen[0].contains("/v1.0/oauth2/accessToken"),
            "第一次要换 token：{}",
            seen[0]
        );
        assert!(
            seen[1].contains("/topapi/im/chat/create") && seen[1].contains("access_token=tok"),
            "旧网关鉴权走 query 参数：{}",
            seen[1]
        );
        assert!(seen[1].contains(r#""owner":"staff_1""#), "{}", seen[1]);
        assert!(
            seen[1].contains(r#""useridlist":["staff_1"]"#),
            "群主必须同时是成员（只给 owner 的请求会被平台拒）：{}",
            seen[1]
        );
    }

    /// 建群请求必须带非空 `owner` 与 `useridlist`：钉钉侧两项都非空才可能建成功，
    /// 而写错只需要一次真实调用（且有副作用）才发现，所以在纯函数上钉死。
    #[test]
    fn create_chat_body_carries_owner_as_a_member() {
        let body = create_chat_body("后端开发", "staff_1");
        assert_eq!(body["name"], "后端开发");
        assert_eq!(body["owner"], "staff_1", "群主必须是契约给的 owner_user_id");
        assert_eq!(
            body["useridlist"],
            json!(["staff_1"]),
            "只给 owner 不给 useridlist 的请求会被平台拒"
        );
    }

    /// 群/单聊判定是靠前缀的约定，改它会同时影响发送路由与 @ 逻辑。
    #[test]
    fn group_chat_is_decided_by_cid_prefix() {
        assert!(is_group_chat("cidAsXSBLnA=="));
        assert!(!is_group_chat("16650***698"));
    }
}
