//! Messenger 抽象 —— 统一飞书 / 微信 / 钉钉的发送接口。
//! `send_text` 是唯一必须实现；话题回复 `send_thread_reply` 有默认回落（飞书覆盖为 reply 接口）；
//! 表情（Typing/DONE）飞书有、微信/钉钉没有（默认空实现）。
//! Bridge 持 `Arc<dyn Messenger>`，按 bot.kind 注入具体实现。

use crate::config::BotConfig;
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 引用消息的原始内容（附件尚未下载；`attachments` 是各通道的附件描述，供
/// `download_attachment` 下载成元数据）。
#[derive(Debug, Clone, Default)]
pub struct QuotedMessage {
    /// 被引用文本（含链接 URL；飞书 post 的 href 已拼进文本）。
    pub text: String,
    pub attachments: Vec<crate::attachments::AttachmentDesc>,
    /// 被引用消息的**类型**（平台原样给；取不到为空）。内容为空时它是唯一线索
    /// （例如被引用的是 merge_forward / sticker / 卡片类消息）。
    pub msg_type: String,
    /// 平台端内容里**未解析的元素 tag**（飞书 post 富文本；空 = 无）。用于把
    /// 「拉到了但读不出内容」变成可观测，而不是静默降级。
    pub unparsed_tags: Vec<String>,
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
    #[allow(dead_code)] // 契约 §3.4 能力：MCP 渠道实现它，ABB 主仓暂无调用方
    async fn user_display_name(&self, _user_id: &str) -> Option<String> {
        None
    }

    /// 记录某会话的回复上下文（微信 context_token）。飞书不需要，默认无操作。
    #[allow(dead_code)] // 契约 §3.4 能力：MCP 渠道实现它，ABB 主仓暂无调用方
    fn note_context(&self, _chat_id: &str, _context_token: &str) {}

    /// 记录某会话最近一个发送者（钉钉群聊回复时 @ 对方用）。其它通道不需要，默认无操作。
    #[allow(dead_code)] // 契约 §3.4 能力：MCP 渠道实现它，ABB 主仓暂无调用方
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

/// 按 bot 配置构造对应 Messenger。
///
/// **`kind = "mcp"` 与内置渠道在这里合流**（总线 S1c/S1d）：上层（`bridge/`、
/// `deliver`、`schedule`）只拿到一个 `Arc<dyn Messenger>`，不知道消息从哪来。
/// 这就是「ABB 是总线」的实际含义——加渠道不改上层。
pub fn build(bot: &BotConfig) -> Result<std::sync::Arc<dyn Messenger>> {
    if bot.is_mcp() {
        // 缺配置要**明确报错**，不能静默 fallback 到飞书：那会拿飞书凭证去连一个
        // 本该由 MCP 服务的渠道，症状是「渠道在线但收发全乱」。
        let ch = bot.mcp_channel.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "bot「{}」的 kind=mcp 但没配 mcp_channel（需要 command；args/env 可选）",
                bot.name
            )
        })?;
        let envs: Vec<(String, String)> = ch
            .env
            .iter()
            .map(|e| (e.name.clone(), e.value.clone()))
            .collect();
        // 建连时就校验能力（缺 send_text / 无入站 ⇒ 在这里失败），
        // 而不是等到第一条消息要发出去。
        let m = crate::mcp_channel::McpChannelMessenger::connect(
            &ch.name,
            &ch.command,
            &ch.args,
            &envs,
        )
        .map_err(anyhow::Error::msg)?;
        return Ok(std::sync::Arc::new(m));
    }
    // **内置渠道已退役**（2026-11-04）：三套实现搬成独立 MCP server
    // （`crates/abb-channel-{wechat,feishu,dingtalk}`），配置见
    // `docs/mcp-bus-architecture.md` §4.1，迁移步骤见同文档「从内置渠道迁到 MCP 渠道」。
    //
    // 这里**必须响亮失败**，不能静默回落任何一个渠道：
    // 拿一种渠道的凭证去连另一种，症状是「bot 在线但收发全乱」，比启动失败难查得多。
    let hint = match bot.kind.as_str() {
        "feishu" => "abb-channel-feishu（FS_APP_ID / FS_APP_SECRET）",
        "wechat" => "abb-channel-wechat（--dir <目录> + WX_TOKEN / WX_BASE_URL / WX_USER_ID）",
        "dingtalk" => "abb-channel-dingtalk（DT_APP_ID / DT_APP_SECRET / DT_ROBOT_CODE）",
        other => {
            return Err(anyhow::anyhow!(
                "bot「{}」的 kind=`{other}` 不是 `mcp`；内置渠道已退役，只支持 kind=mcp",
                bot.name
            ))
        }
    };
    Err(anyhow::anyhow!(
        "bot「{}」用的是已退役的内置渠道 `{}`。请改成 kind=mcp 并配上对应的 MCP server：{hint}\n         迁移步骤见 docs/mcp-bus-architecture.md「从内置渠道迁到 MCP 渠道」",
        bot.name,
        bot.kind
    ))
}
