//! Messenger 抽象 —— 统一飞书 / 微信 / 钉钉的发送接口。
//! `send_text` 是唯一必须实现；话题回复 `send_thread_reply` 有默认回落（飞书覆盖为 reply 接口）；
//! 表情（Typing/DONE）飞书有、微信/钉钉没有（默认空实现）。
//! Bridge 持 `Arc<dyn Messenger>`，按 bot.kind 注入具体实现。

use crate::config::BotConfig;
use crate::dingtalk::DingTalkClient;
use crate::feishu::FeishuClient;
use crate::wechat::WeixinClient;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

/// 引用消息的原始内容（附件尚未下载；`attachments` 是各通道的附件描述，供
/// `download_attachment` 下载成元数据）。
#[derive(Debug, Clone, Default)]
pub struct QuotedMessage {
    /// 被引用文本（含链接 URL；飞书 post 的 href 已拼进文本）。
    pub text: String,
    pub attachments: Vec<crate::attachments::AttachmentDesc>,
}

/// 引用消息内容（附件已下载为元数据）。随 `Ev` 进 handle / 随 pending.json 持久化。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotedContent {
    pub text: String,
    pub attachments: Vec<crate::attachments::AttachmentMeta>,
}

#[async_trait::async_trait]
pub trait Messenger: Send + Sync {
    /// 发文本到会话。chat_id：飞书=chat_id；微信=ilink_user_id。
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<()>;

    /// 发送到会话内话题：飞书以 message_id 走回复接口（reply_in_thread: true），
    /// 保证回复落在原话题内（#14）；其它通道没有话题，默认回落普通发送。
    async fn send_thread_reply(&self, chat_id: &str, _message_id: &str, text: &str) -> Result<()> {
        self.send_text(chat_id, text).await
    }

    /// 拉取一条历史消息的引用内容（文本 + 附件描述；被引用内容进 agent prompt）。
    /// 飞书覆盖（走消息 API）；微信/钉钉的引用内容随入站事件直接携带，默认无操作。
    async fn get_quoted_message(&self, _message_id: &str) -> Option<QuotedMessage> {
        None
    }

    /// 处理中表情（可选）。返回 reaction_id 供 done 时删除。默认 None。
    async fn typing(&self, _message_id: &str) -> Option<String> {
        None
    }
    /// 撤销处理中表情（可选）。默认无操作。
    async fn del_typing(&self, _message_id: &str, _reaction_id: Option<String>) {}
    /// 完成表情（可选）。默认无操作。
    async fn done(&self, _message_id: &str) {}

    /// 反查用户展示名（授权码消费后记录「谁被授权了」）。飞书走联系人 API；其它通道无
    /// 对应概念，默认返回 None（调用方用用户 id 兜底显示）。best-effort，失败不阻塞流程。
    async fn user_display_name(&self, _user_id: &str) -> Option<String> {
        None
    }

    /// 记录某会话的回复上下文（微信 context_token）。飞书不需要，默认无操作。
    fn note_context(&self, _chat_id: &str, _context_token: &str) {}

    /// 记录某会话最近一个发送者（钉钉群聊回复时 @ 对方用）。其它通道不需要，默认无操作。
    fn note_sender(&self, _chat_id: &str, _sender_id: &str) {}

    /// 下载入站附件并保存到工作区，返回元数据（#12；桥注入 agent prompt）。
    /// 默认不支持（返回 None）；各平台实现覆盖。失败返回带 note 的占位元数据，不静默丢消息。
    async fn download_attachment(
        &self,
        _bot_key: &str,
        _mid: &str,
        _seq: usize,
        _desc: &crate::attachments::AttachmentDesc,
    ) -> Option<crate::attachments::AttachmentMeta> {
        None
    }

    /// 发送一个已保存附件（#21 附件跨投递）。默认实现只发文本元数据——接收端按能力处理：
    /// 跨 bot 同机运行时本地路径可读，接收端 agent/用户可按路径取用；各平台按能力覆盖为真图/真文件。
    async fn send_attachment(
        &self,
        chat_id: &str,
        meta: &crate::attachments::AttachmentMeta,
    ) -> Result<()> {
        let mut s = format!(
            "📎 [{}] 文件名={} mime={} 大小={}",
            meta.kind, meta.file_name, meta.mime, meta.size
        );
        if !meta.path.is_empty() {
            s.push_str(&format!(" 本地路径={}", meta.path));
        }
        if !meta.sha256.is_empty() {
            s.push_str(&format!(" sha256={}", meta.sha256));
        }
        if !meta.note.is_empty() {
            s.push_str(&format!(" 备注={}", meta.note));
        }
        self.send_text(chat_id, &s).await
    }
}

/// 飞书实现：委托 FeishuClient，表情走 reactions。
pub struct FeishuMessenger {
    pub fs: FeishuClient,
}

#[async_trait::async_trait]
impl Messenger for FeishuMessenger {
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
        self.fs.send_text(chat_id, text).await
    }
    async fn user_display_name(&self, user_id: &str) -> Option<String> {
        self.fs.user_name(user_id).await
    }
    async fn send_thread_reply(&self, _chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        self.fs.reply_text(message_id, text).await
    }
    async fn get_quoted_message(&self, message_id: &str) -> Option<QuotedMessage> {
        match self.fs.get_quoted_message(message_id).await {
            Ok(parsed) => Some(QuotedMessage {
                text: parsed.text,
                attachments: parsed
                    .resources
                    .into_iter()
                    .map(|r| crate::attachments::AttachmentDesc::Feishu {
                        message_id: message_id.to_string(),
                        file_key: r.file_key,
                        kind: r.kind,
                        file_name: r.file_name,
                    })
                    .collect(),
            }),
            Err(e) => {
                crate::log!("[feishu] 拉取引用消息失败 mid={}: {e:#}", message_id);
                None
            }
        }
    }
    async fn download_attachment(
        &self,
        bot_key: &str,
        mid: &str,
        seq: usize,
        desc: &crate::attachments::AttachmentDesc,
    ) -> Option<crate::attachments::AttachmentMeta> {
        let crate::attachments::AttachmentDesc::Feishu {
            message_id,
            file_key,
            kind,
            file_name,
        } = desc
        else {
            return None;
        };
        match self.fs.download_resource(message_id, file_key, kind).await {
            Ok((bytes, mime)) => crate::attachments::save_attachment(
                bot_key, mid, seq, kind, "feishu", file_name, &mime, &bytes,
            )
            .map_err(|e| {
                crate::log!("[feishu] 附件保存失败: {e:#}");
                e
            })
            .ok(),
            Err(e) => {
                crate::log!("[feishu] 附件下载失败: {e:#}");
                Some(crate::attachments::failed_meta(
                    kind, "feishu", file_name, &e,
                ))
            }
        }
    }
    async fn typing(&self, message_id: &str) -> Option<String> {
        self.fs.add_reaction(message_id, "Typing").await
    }
    async fn del_typing(&self, message_id: &str, reaction_id: Option<String>) {
        if let Some(rid) = reaction_id {
            self.fs.del_reaction(message_id, &rid).await;
        }
    }
    async fn done(&self, message_id: &str) {
        self.fs.add_reaction(message_id, "DONE").await;
    }
}

/// 微信实现：send_text 需要每条会话最新的 context_token（微信协议要求回显）。
/// 用一张 per-chat 表存 from_user_id → context_token，每次收到消息就刷新，发送时取。
/// **持久化**到 `workspaces/<key>/context_tokens.json`（0600）：context_token 是回复寻址凭证，
/// 定时任务/重启后无新入站消息时也要用，只存内存会丢 → job 触发的回复发不出。
pub struct WeixinMessenger {
    pub wx: WeixinClient,
    ctx: Mutex<HashMap<String, String>>,
    path: std::path::PathBuf,
}

impl WeixinMessenger {
    pub fn new(wx: WeixinClient, bot_key: &str) -> WeixinMessenger {
        let path = crate::workspace_dir(bot_key).join("context_tokens.json");
        let ctx: HashMap<String, String> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        WeixinMessenger {
            wx,
            ctx: Mutex::new(ctx),
            path,
        }
    }

    /// 原子写 + 0600（context_token 是敏感凭证）。
    fn persist(&self, map: &HashMap<String, String>) {
        if let Ok(text) = serde_json::to_string(map) {
            if crate::atomic_write_text(&self.path, &text).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &self.path,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Messenger for WeixinMessenger {
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
        let token = self.ctx.lock().unwrap().get(chat_id).cloned();
        let ctx = token.ok_or_else(|| {
            anyhow::anyhow!("微信会话 {chat_id} 还没有 context_token（需先收到对方一条消息）")
        })?;
        // 微信单条长度有限，沿用飞书分段逻辑保守切
        for chunk in crate::feishu::split_text(text, crate::feishu::FEISHU_MSG_LIMIT) {
            self.wx.send_text(chat_id, &ctx, &chunk).await?;
        }
        Ok(())
    }
    // 微信无表情：typing/done 用默认空实现

    async fn download_attachment(
        &self,
        bot_key: &str,
        mid: &str,
        seq: usize,
        desc: &crate::attachments::AttachmentDesc,
    ) -> Option<crate::attachments::AttachmentMeta> {
        let crate::attachments::AttachmentDesc::Wechat(media) = desc else {
            return None;
        };
        match self.wx.download_media(media).await {
            Ok(Some((bytes, mime, file_name, note))) => {
                let mut meta = match crate::attachments::save_attachment(
                    bot_key,
                    mid,
                    seq,
                    &media.kind,
                    "wechat",
                    &file_name,
                    &mime,
                    &bytes,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        crate::log!("[wechat] 附件保存失败: {e:#}");
                        return Some(crate::attachments::failed_meta(
                            &media.kind,
                            "wechat",
                            &file_name,
                            &e,
                        ));
                    }
                };
                meta.note = note;
                Some(meta)
            }
            Ok(None) => None,
            Err(e) => {
                crate::log!("[wechat] 附件下载失败: {e:#}");
                Some(crate::attachments::failed_meta(
                    &media.kind,
                    "wechat",
                    &media.file_name,
                    &e,
                ))
            }
        }
    }

    fn note_context(&self, chat_id: &str, context_token: &str) {
        if !chat_id.is_empty() && !context_token.is_empty() {
            let mut m = self.ctx.lock().unwrap();
            m.insert(chat_id.to_string(), context_token.to_string());
            self.persist(&m);
        }
    }
}

/// 钉钉实现：send_text 按会话标识分发（cid 开头=群聊 → groupMessages/send，否则单聊 →
/// oToMessages/batchSend）。群聊回复需要 @ 提问者，故用一张 per-chat 表记最近 sender
/// （入站时 bridge.on_dingtalk 调 note_sender 刷新；job 等非对话路径照常发，只是不 @）。
pub struct DingTalkMessenger {
    pub dt: DingTalkClient,
    robot_code: String,
    last_sender: Mutex<HashMap<String, String>>,
}

impl DingTalkMessenger {
    pub fn new(dt: DingTalkClient, robot_code: String) -> DingTalkMessenger {
        DingTalkMessenger {
            dt,
            robot_code,
            last_sender: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl Messenger for DingTalkMessenger {
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
        self.dt
            .send_text(chat_id, &self.robot_code, text, None)
            .await
    }
    async fn user_display_name(&self, user_id: &str) -> Option<String> {
        self.dt.user_name(user_id).await
    }
    async fn send_thread_reply(&self, chat_id: &str, _message_id: &str, text: &str) -> Result<()> {
        // 群聊回复 @ 最近提问者（单聊 chat_id=对方 staffId，无需 @）
        let at = if crate::dingtalk::is_group_chat(chat_id) {
            self.last_sender.lock().unwrap().get(chat_id).cloned()
        } else {
            None
        };
        // 超长分段（与飞书同一套按字符贪心逻辑）
        for chunk in crate::feishu::split_text(text, crate::dingtalk::DINGTALK_MSG_LIMIT) {
            self.dt
                .send_text(chat_id, &self.robot_code, &chunk, at.as_deref())
                .await?;
        }
        Ok(())
    }
    // 钉钉无表情：typing/done 用默认空实现

    async fn download_attachment(
        &self,
        bot_key: &str,
        mid: &str,
        seq: usize,
        desc: &crate::attachments::AttachmentDesc,
    ) -> Option<crate::attachments::AttachmentMeta> {
        let crate::attachments::AttachmentDesc::Dingtalk {
            download_code,
            robot_code,
            kind,
            file_name,
            voice_text,
        } = desc
        else {
            return None;
        };
        // robot_code 缺省回落到 messenger 配置（配置显式优先，回调值仅当非空才用）
        let rc = if robot_code.is_empty() {
            self.robot_code.as_str()
        } else {
            robot_code.as_str()
        };
        match self.dt.download_msg_file(download_code, rc).await {
            Ok(bytes) => {
                let mut meta = match crate::attachments::save_attachment(
                    bot_key, mid, seq, kind, "dingtalk", file_name, "", &bytes,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        crate::log!("[dingtalk] 附件保存失败: {e:#}");
                        return Some(crate::attachments::failed_meta(
                            kind, "dingtalk", file_name, &e,
                        ));
                    }
                };
                meta.note = voice_text.clone();
                Some(meta)
            }
            Err(e) => {
                crate::log!("[dingtalk] 附件下载失败: {e:#}");
                Some(crate::attachments::failed_meta(
                    kind, "dingtalk", file_name, &e,
                ))
            }
        }
    }

    fn note_sender(&self, chat_id: &str, sender_id: &str) {
        if !chat_id.is_empty() && !sender_id.is_empty() {
            self.last_sender
                .lock()
                .unwrap()
                .insert(chat_id.to_string(), sender_id.to_string());
        }
    }
}

/// 按 bot 配置构造对应 Messenger。
pub fn build(bot: &BotConfig) -> Result<std::sync::Arc<dyn Messenger>> {
    if bot.is_dingtalk() {
        return Ok(std::sync::Arc::new(DingTalkMessenger::new(
            DingTalkClient::new(&bot.app_id, &bot.app_secret),
            bot.ding_robot_code().to_string(),
        )));
    }
    if bot.is_wechat() {
        let base = if bot.wx_base_url.is_empty() {
            crate::wechat::FIXED_BASE_URL
        } else {
            &bot.wx_base_url
        };
        let cdn = if bot.wx_cdn_base_url.is_empty() {
            crate::wechat::DEFAULT_CDN_BASE_URL
        } else {
            bot.wx_cdn_base_url.as_str()
        };
        Ok(std::sync::Arc::new(WeixinMessenger::new(
            WeixinClient::new(base, &bot.wx_token, cdn),
            &bot.key(),
        )))
    } else {
        Ok(std::sync::Arc::new(FeishuMessenger {
            fs: FeishuClient::new(&bot.app_id, &bot.app_secret),
        }))
    }
}
