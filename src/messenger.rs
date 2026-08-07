//! Messenger 抽象 —— 统一飞书 / 微信的发送接口。
//! `send_text` 是唯一必须实现；表情（Typing/DONE）飞书有、微信没有（默认空实现）。
//! Bridge 持 `Arc<dyn Messenger>`，按 bot.kind 注入具体实现。

use crate::config::BotConfig;
use crate::feishu::FeishuClient;
use crate::wechat::WeixinClient;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;

#[async_trait::async_trait]
pub trait Messenger: Send + Sync {
    /// 发文本到会话。chat_id：飞书=chat_id；微信=ilink_user_id。
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<()>;

    /// 处理中表情（可选）。返回 reaction_id 供 done 时删除。默认 None。
    async fn typing(&self, _message_id: &str) -> Option<String> {
        None
    }
    /// 撤销处理中表情（可选）。默认无操作。
    async fn del_typing(&self, _message_id: &str, _reaction_id: Option<String>) {}
    /// 完成表情（可选）。默认无操作。
    async fn done(&self, _message_id: &str) {}

    /// 记录某会话的回复上下文（微信 context_token）。飞书不需要，默认无操作。
    fn note_context(&self, _chat_id: &str, _context_token: &str) {}
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

    fn note_context(&self, chat_id: &str, context_token: &str) {
        if !chat_id.is_empty() && !context_token.is_empty() {
            let mut m = self.ctx.lock().unwrap();
            m.insert(chat_id.to_string(), context_token.to_string());
            self.persist(&m);
        }
    }
}

/// 按 bot 配置构造对应 Messenger。
pub fn build(bot: &BotConfig) -> Result<std::sync::Arc<dyn Messenger>> {
    if bot.is_wechat() {
        let base = if bot.wx_base_url.is_empty() {
            crate::wechat::FIXED_BASE_URL
        } else {
            &bot.wx_base_url
        };
        Ok(std::sync::Arc::new(WeixinMessenger::new(
            WeixinClient::new(base, &bot.wx_token),
            &bot.key(),
        )))
    } else {
        Ok(std::sync::Arc::new(FeishuMessenger {
            fs: FeishuClient::new(&bot.app_id, &bot.app_secret),
        }))
    }
}
