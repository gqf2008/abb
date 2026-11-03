//! Bridge 子模块：出站消息/流控（#80 按功能面拆分，impl Bridge 分散到子模块——
//! 子模块是父模块后代，可访问 mod.rs 私有字段，无需改可见性）。

use super::*;

impl Bridge {
    pub(super) async fn send_reply(&self, ev: &Ev, text: &str) -> anyhow::Result<()> {
        if ev.thread_id.is_empty() {
            self.msgr.send_text(&ev.chat_id, text).await
        } else {
            self.msgr
                .send_thread_reply(&ev.chat_id, &ev.mid, text)
                .await
        }
    }

    /// 把一条待发消息落盘积压（仅微信：主动推送被拒时缓存，等下次入站补发）。
    /// 其它通道主动推送不受 token 活跃度约束，继续走既有「失败回落主会话」路径，不入队。
    pub fn queue_outbox(&self, chat_id: &str, text: &str, job_id: &str) {
        if !self.bot.is_wechat() || text.is_empty() {
            return;
        }
        // 归一化入队键：任务的 created_by.chat_id 可能是注入的 bot key / ABB 频道 UUID，
        // 直接入队会与补发侧的平台 receive_id 键错位（缺陷 abb-wx-outbox-flush-mismatch-20260917）。
        // 入队键归一化：与（已随渠道退役的）补发侧共用同一函数，保证历史积压与
        // 现存入队路径写出同一种 chat_key。
        let chat_id = crate::outbox::resolve_delivery_chat(&self.bot.key(), chat_id);
        if chat_id.is_empty() {
            return;
        }
        self.outbox.add(OutboxItem {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.clone(),
            text: text.to_string(),
            created_at: crate::chrono_lite::unix_secs(),
            attempts: 0,
            job_id: job_id.to_string(),
        });
        crate::log!(
            "[bot:{}] [outbox] 任务报告写入待发积压 chat={} 长度={}（当前积压 {} 条）",
            self.bot.key(),
            trunc(&chat_id, 10),
            text.chars().count(),
            self.outbox.len()
        );
    }

    // 群聊 @ 判定：入站事件解析随内置渠道退役（MCP 契约暂无「是否被 @」字段）。
    // 缺口已记入 docs/channel-retirement-plan.md。
    #[allow(dead_code)]
    /// 群消息 mentions 里是否 @了本机器人（name/app_id/open_id 三重冗余）。
    pub(super) fn bot_is_mentioned(&self, mentions: &[serde_json::Value]) -> bool {
        for m in mentions {
            // name 命中
            if m.get("name").and_then(|x| x.as_str()) == Some(self.bot.bot_name.as_str())
                && !self.bot.bot_name.is_empty()
            {
                return true;
            }
            // id.app_id / id.open_id 命中
            if let Some(id) = m.get("id") {
                if id.get("app_id").and_then(|x| x.as_str()) == Some(self.bot.app_id.as_str())
                    && !self.bot.app_id.is_empty()
                {
                    return true;
                }
                if id.get("open_id").and_then(|x| x.as_str()) == Some(self.bot.bot_open_id.as_str())
                    && !self.bot.bot_open_id.is_empty()
                {
                    return true;
                }
            }
            // 顶层 open_id 命中
            if m.get("open_id").and_then(|x| x.as_str()) == Some(self.bot.bot_open_id.as_str())
                && !self.bot.bot_open_id.is_empty()
            {
                return true;
            }
        }
        false
    }
}
