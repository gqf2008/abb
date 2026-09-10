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

/// 飞书附件发送计划（纯函数——分发决策有单测钉死，审查 #254：dispatch 零覆盖）。
/// `kind==image` 但扩展名不在 images 端点可靠集（svg/ico/heic…）→ 走文件卡片，
/// 避免上传被服务端格式校验拒时整个附件失败。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FeishuSendPlan {
    Image,
    File(&'static str),
}

pub(crate) fn feishu_send_plan(meta: &crate::attachments::AttachmentMeta) -> FeishuSendPlan {
    // 判定与实际上传用**同一个名字**（`attachment_upload_name`）：只用
    // `meta.file_name` 会在它为空时把 `.../a.png` 这样的图错判成文件卡片
    // （审查 #254 复核 N1）。
    let name = attachment_upload_name(meta);
    if meta.kind == "image" && crate::feishu::feishu_image_uploadable(&name) {
        FeishuSendPlan::Image
    } else {
        FeishuSendPlan::File(crate::feishu::feishu_file_type(&name))
    }
}

/// 钉钉附件能力闸（纯函数）：当前仅「群聊 + 可上传后缀的图片」。判前于读文件
/// （省大 IO）。后缀白名单不可省——`kind_from_name` 把 svg/ico/heic 也归成
/// image，只判 kind 会让它们过闸后被服务端格式校验拒，用户只能看到原始报错
/// （审查 #254 P2-3）。
pub(crate) fn dingtalk_can_send(meta: &crate::attachments::AttachmentMeta, chat_id: &str) -> bool {
    meta.kind == "image"
        && crate::dingtalk::is_group_chat(chat_id)
        && crate::dingtalk::dingtalk_image_uploadable(&attachment_upload_name(meta))
}

/// 微信外发媒体的种类判定（纯函数，单测钉死）：
/// - `image` 且后缀在内联图白名单 → 图片；`svg/ico/heic` 这类（`kind_from_name` 也归
///   image，但微信图片通道必被格式校验拒）退成**文件**，与飞书同款处理；
/// - `video` → 视频；
/// - 其余（含 `audio`）→ 文件。**音频不用 VOICE**：iLink 的 VOICE 类型实测被官方丢弃
///   （message_id 成功但微信端不显示），按文件发才能真收到。
pub(crate) fn wechat_media_kind(
    meta: &crate::attachments::AttachmentMeta,
) -> crate::wechat::OutboundMediaKind {
    let name = attachment_upload_name(meta);
    match meta.kind.as_str() {
        "image" if crate::attachments::image_ext_uploadable(&name) => {
            crate::wechat::OutboundMediaKind::Image
        }
        "video" if wechat_video_uploadable(&name) => crate::wechat::OutboundMediaKind::Video,
        _ => crate::wechat::OutboundMediaKind::File,
    }
}

/// 微信视频通道能可靠渲染的容器。`kind_from_name` 把 mkv/avi/webm/flv 也归成 video，
/// 但这些容器微信端常「发送成功但播不了」——退成文件更实在（与 svg→文件同思路）。
/// 保守起见只放 mp4/m4v/mov（ISO-BMFF 系）。真机若确认别的容器也能播，加回这里即可。
pub(crate) fn wechat_video_uploadable(file_name: &str) -> bool {
    matches!(
        file_name
            .rsplit_once('.')
            .map(|(_, e)| e.trim().to_ascii_lowercase())
            .as_deref(),
        Some("mp4") | Some("m4v") | Some("mov")
    )
}

/// 上传时用的文件名（审查 #254 P3-1）：先取 meta.file_name，空则取本地路径的
/// basename，再空则按 mime/kind 造一个**带后缀**的占位名。绝不能退化成无扩展名的
/// 字面量——飞书 `file_type` 映射与 images 端点、钉钉 upload 都按后缀校验格式，
/// 无后缀必被服务端拒（入站 meta 的 file_name 存在为空的分支）。
pub(crate) fn attachment_upload_name(meta: &crate::attachments::AttachmentMeta) -> String {
    if !meta.file_name.is_empty() {
        return meta.file_name.clone();
    }
    let base = std::path::Path::new(&meta.path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !base.is_empty() {
        return base;
    }
    let ext = match meta
        .mime
        .rsplit_once('/')
        .map(|(_, sub)| sub.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "png",
        Some("jpeg") | Some("jpg") => "jpg",
        Some("gif") => "gif",
        Some("webp") => "webp",
        Some("bmp") => "bmp",
        Some("pdf") => "pdf",
        Some("plain") => "txt",
        _ => {
            if meta.kind == "image" {
                "png"
            } else {
                "bin"
            }
        }
    };
    format!("attachment.{ext}")
}

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

    /// 群资料查询（虚拟 Bot #75 注入用）：返回 (群名, 群介绍)。best-effort——
    /// 失败/平台不支持返回 None，调用方只 log 不阻塞消息处理（完全照抄
    /// get_quoted_message 的失败语义）。微信无群概念，默认 None。
    async fn get_chat_info(&self, _chat_id: &str) -> Option<(String, String)> {
        None
    }

    /// 会话类型 + 会话名（历史回填用）：飞书 chat API 返回体自带 chat_type
    /// （"p2p"/"group"）；其它平台默认 None（钉钉落库时已带 conversationTitle，
    /// 微信无群概念）。None = 查不到/不支持——回填任务跳过该会话。
    async fn chat_brief(&self, _chat_id: &str) -> Option<(String, String)> {
        None
    }

    /// 创建群会话（#124 一键创建团队·聊天入口用）。返回平台 chat_id。
    /// 平台支持：飞书/钉钉实现；微信无建群 API → 默认 Err（聊天侧回落登记制指引：
    /// 手动建群后 GUI 虚拟 Bot 面板登记）。`owner_user_id`：飞书把 owner 设为群成员
    /// + 管理员（建群后用户才看得到群，8-20 实测）；钉钉忽略该参数。
    async fn create_chat(
        &self,
        _name: &str,
        _description: &str,
        _owner_user_id: &str,
    ) -> Result<String, String> {
        Err("当前平台不支持自动建群（微信）。请手动建群后在 GUI 虚拟 Bot 面板登记。".to_string())
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

    /// 发送一个已保存附件（#21 附件跨投递；#253 起要求**真发送**）。
    ///
    /// **必填，无默认实现**——历史默认实现是把「📎 … 本地路径=…」当文本发出去并返回
    /// Ok：投递方以为附件送达、deliver 也报「已送达」，收件人实际只拿到一串自己机器
    /// 上不存在的路径。移除默认实现让「本平台不支持附件」只能显式表达（`bail!`），
    /// 不可能再被静默继承（审查 #254 P2-2）。
    async fn send_attachment(
        &self,
        chat_id: &str,
        meta: &crate::attachments::AttachmentMeta,
    ) -> Result<()>;
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
    async fn chat_brief(&self, chat_id: &str) -> Option<(String, String)> {
        // GET /im/v1/chats/{id} 的 data.chat_type（"p2p"/"group"）+ data.name
        let (ctype, name) = self.fs.chat_brief(chat_id).await.ok()?;
        Some((ctype, name))
    }
    async fn get_chat_info(&self, chat_id: &str) -> Option<(String, String)> {
        // best-effort：失败只 log（缓存层会自然降级为事件名/跳过注入），不阻塞消息
        match self.fs.get_chat_info(chat_id).await {
            Ok(info) => Some(info),
            Err(e) => {
                crate::log!("[feishu] 查询群资料失败 chat={}: {e:#}", chat_id);
                None
            }
        }
    }
    async fn create_chat(
        &self,
        name: &str,
        description: &str,
        owner_user_id: &str,
    ) -> Result<String, String> {
        self.fs
            .create_chat(name, description, owner_user_id)
            .await
            .map_err(|e| format!("建群失败：{e:#}"))
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
    async fn send_attachment(
        &self,
        chat_id: &str,
        meta: &crate::attachments::AttachmentMeta,
    ) -> Result<()> {
        // 真实文件发送（#253）：读本地附件 → 平台上传 → 以媒体消息发出，
        // 不再给用户塞「带本地路径的文本元数据」。失败原样上报（deliver 回源提示）。
        // 分发决策在纯函数 feishu_send_plan（单测钉死）；上传带真实文件名（服务端
        // 按后缀校验格式，审查 #254）。
        let name = attachment_upload_name(meta);
        match feishu_send_plan(meta) {
            FeishuSendPlan::Image => {
                crate::attachments::check_sendable_size(
                    meta,
                    crate::attachments::FEISHU_IMAGE_MAX_BYTES,
                )?;
                let bytes = crate::attachments::read_attachment_bytes(meta)?;
                let key = self.fs.upload_image(bytes, &name).await?;
                self.fs.send_media_message(chat_id, "image", &key).await
            }
            FeishuSendPlan::File(ft) => {
                crate::attachments::check_sendable_size(
                    meta,
                    crate::attachments::FEISHU_FILE_MAX_BYTES,
                )?;
                let bytes = crate::attachments::read_attachment_bytes(meta)?;
                let key = self.fs.upload_file(ft, &name, bytes).await?;
                self.fs.send_media_message(chat_id, "file", &key).await
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
    /// 微信附件外发（#283）：走 iLink 原生链路（`getuploadurl` → AES-128-ECB 加密 →
    /// CDN → `sendmessage` 带 media item），**不再**走 trait 默认的「发一串本地路径
    /// 文本并返回 Ok」的静默降级（审查 #254 P2-2 删的就是它）。
    ///
    /// 目标可以是任意微信会话（`chat_id` = `to_user_id`），`context_token` 用该会话
    /// 入站消息带上来的那个——没有就报错让用户先发一条（与外发文本同一条要求）。
    async fn send_attachment(
        &self,
        chat_id: &str,
        meta: &crate::attachments::AttachmentMeta,
    ) -> Result<()> {
        let token = self.ctx.lock().unwrap().get(chat_id).cloned();
        let ctx = token.ok_or_else(|| {
            anyhow::anyhow!("微信会话 {chat_id} 还没有 context_token（需先收到对方一条消息）")
        })?;
        crate::attachments::check_sendable_size(meta, crate::attachments::WECHAT_UPLOAD_MAX_BYTES)?;
        let name = attachment_upload_name(meta);
        let kind = wechat_media_kind(meta);
        let bytes = crate::attachments::read_attachment_bytes(meta)?;
        self.wx.send_media(chat_id, &ctx, kind, &name, &bytes).await
    }

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
    async fn get_chat_info(&self, chat_id: &str) -> Option<(String, String)> {
        // 钉钉群信息接口无「群介绍」字段：desc 恒空（平台限制，见 dingtalk.rs 注释）
        self.dt.get_chat_info(chat_id).await.ok()
    }
    async fn create_chat(
        &self,
        name: &str,
        description: &str,
        _owner_user_id: &str,
    ) -> Result<String, String> {
        self.dt
            .create_chat(name, description)
            .await
            .map_err(|e| format!("建群失败：{e:#}"))
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

    async fn send_attachment(
        &self,
        chat_id: &str,
        meta: &crate::attachments::AttachmentMeta,
    ) -> Result<()> {
        // 能力闸**先于读文件**（审查 #254：不支持的组合曾把 100MB 整读进内存再丢）。
        if !dingtalk_can_send(meta, chat_id) {
            anyhow::bail!(
                "钉钉机器人发送该附件尚未实现（kind={} 会话={}）：当前仅支持群聊 {exts} 图片；文件/语音/单聊媒体待 #253 后续补",
                meta.kind,
                if crate::dingtalk::is_group_chat(chat_id) { "群聊" } else { "单聊" },
                exts = crate::attachments::IMAGE_UPLOAD_EXTS.join("/")
            )
        }
        crate::attachments::check_sendable_size(
            meta,
            crate::attachments::DINGTALK_IMAGE_MAX_BYTES,
        )?;
        let bytes = crate::attachments::read_attachment_bytes(meta)?;
        let name = attachment_upload_name(meta);
        let media_id = self.dt.upload_image(bytes, &name).await?;
        self.dt
            .send_group_image(chat_id, &self.robot_code, &media_id)
            .await
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(kind: &str, file_name: &str) -> crate::attachments::AttachmentMeta {
        crate::attachments::AttachmentMeta {
            kind: kind.into(),
            source: "feishu".into(),
            file_name: file_name.into(),
            mime: String::new(),
            size: 10,
            path: "/tmp/whatever".into(),
            sha256: String::new(),
            note: String::new(),
        }
    }

    #[test]
    fn feishu_plan_image_only_for_uploadable_extensions() {
        assert_eq!(
            feishu_send_plan(&meta("image", "shot.png")),
            FeishuSendPlan::Image
        );
        assert_eq!(
            feishu_send_plan(&meta("image", "a.JPEG")),
            FeishuSendPlan::Image
        );
        // svg/heic：kind=image 但 images 端点不稳 → 文件卡片（stream）
        assert_eq!(
            feishu_send_plan(&meta("image", "logo.svg")),
            FeishuSendPlan::File("stream")
        );
        assert_eq!(
            feishu_send_plan(&meta("image", "old.heic")),
            FeishuSendPlan::File("stream")
        );
        // 真文件按扩展名定型
        assert_eq!(
            feishu_send_plan(&meta("file", "报告.pdf")),
            FeishuSendPlan::File("pdf")
        );
        assert_eq!(
            feishu_send_plan(&meta("audio", "voice.mp3")),
            FeishuSendPlan::File("stream")
        );
    }

    #[test]
    fn dingtalk_gate_group_image_only() {
        // 群 openConversationId 恒以 cid 开头（模块头约定）
        assert!(dingtalk_can_send(&meta("image", "a.png"), "cidAsXB=="));
        assert!(dingtalk_can_send(&meta("image", "a.JPG"), "cidAsXB=="));
        assert!(!dingtalk_can_send(&meta("image", "a.png"), "staff_123"));
        assert!(!dingtalk_can_send(&meta("file", "a.pdf"), "cidAsXB=="));
        // 同飞书：kind=image 但服务端格式校验必拒的后缀在能力闸就拦下
        // （审查 #254 P2-3——旧闸只判 kind，svg 会过闸后被服务端拒）
        for bad in ["logo.svg", "app.ico", "photo.heic", "noext"] {
            assert!(
                !dingtalk_can_send(&meta("image", bad), "cidAsXB=="),
                "{bad} 不应过钉钉图片闸"
            );
        }
    }

    /// #283：微信外发媒体的种类判定——图片走内联图、视频走视频、其余（含音频）走文件；
    /// svg/ico/heic 这类 kind=image 但微信图片通道必拒的后缀退成文件卡片。
    #[test]
    fn wechat_media_kind_routes_by_kind_and_extension() {
        use crate::wechat::OutboundMediaKind;
        assert_eq!(
            wechat_media_kind(&meta("image", "shot.png")),
            OutboundMediaKind::Image
        );
        assert_eq!(
            wechat_media_kind(&meta("image", "a.JPEG")),
            OutboundMediaKind::Image
        );
        // kind=image 但后缀不在白名单 → 文件（别让服务端格式校验把整条打回）
        for bad in ["logo.svg", "app.ico", "photo.heic", "noext"] {
            assert_eq!(
                wechat_media_kind(&meta("image", bad)),
                OutboundMediaKind::File,
                "{bad} 应退成文件"
            );
        }
        for ok in ["clip.mp4", "a.MOV", "b.m4v"] {
            assert_eq!(
                wechat_media_kind(&meta("video", ok)),
                OutboundMediaKind::Video,
                "{ok} 是 ISO-BMFF 系容器，走视频"
            );
        }
        // 异容器（kind=video 但微信端常"发送成功却播不了"）退成文件
        for bad in ["a.mkv", "b.avi", "c.webm", "d.flv"] {
            assert_eq!(
                wechat_media_kind(&meta("video", bad)),
                OutboundMediaKind::File,
                "{bad} 应退成文件"
            );
        }
        // 音频走文件：VOICE 类型实测被官方丢弃（message_id 成功但端上不显示）
        assert_eq!(
            wechat_media_kind(&meta("audio", "voice.mp3")),
            OutboundMediaKind::File
        );
        assert_eq!(
            wechat_media_kind(&meta("file", "报告.pdf")),
            OutboundMediaKind::File
        );
        // 文件名空但路径带后缀时，判定要跟实际上传名一致（走 attachment_upload_name）
        let mut m = meta("image", "");
        m.path = "/tmp/pic.png".into();
        assert_eq!(wechat_media_kind(&m), OutboundMediaKind::Image);
    }

    /// P3-1：上传文件名不能退化成**无扩展名的字面量** `"attachment"`——优先用
    /// 真名，空则取路径 basename，再空才按 mime/kind 造一个带后缀的占位名。
    /// （有名字时不改写：`report` 这类本来就无后缀的名字原样保留，格式校验交给
    /// 平台，飞书会走 `file_type=stream`。）
    #[test]
    fn attachment_upload_name_prefers_real_name_then_path_then_mime() {
        assert_eq!(attachment_upload_name(&meta("image", "pic.png")), "pic.png");
        assert_eq!(
            attachment_upload_name(&meta("file", "report")),
            "report",
            "真名原样用（无后缀不改写——不做无依据的补缀）"
        );
        let mut m = meta("image", "");
        m.path = "/tmp/收件箱/shot.JPEG".into();
        assert_eq!(
            attachment_upload_name(&m),
            "shot.JPEG",
            "空文件名取路径 basename"
        );
        let mut m = meta("file", "");
        m.path = String::new();
        m.mime = "application/pdf".into();
        assert_eq!(
            attachment_upload_name(&m),
            "attachment.pdf",
            "末路按 mime 补后缀"
        );
        let mut m = meta("image", "");
        m.path = String::new();
        m.mime = String::new();
        assert_eq!(
            attachment_upload_name(&m),
            "attachment.png",
            "image 兜底 png"
        );
        let mut m = meta("file", "");
        m.path = String::new();
        m.mime = String::new();
        assert_eq!(attachment_upload_name(&m), "attachment.bin", "其它兜底 bin");
    }
}
