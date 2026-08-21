//! Bridge 子模块：访问控制/授权/会话模式（#80 按功能面拆分，impl Bridge 分散到子模块——
//! 子模块是父模块后代，可访问 mod.rs 私有字段，无需改可见性）。

use super::*;

impl Bridge {
    /// config 读不到本 bot（单测注入）时，用构造时的访问控制快照判定放行。
    /// 快照 = self.bot（build 时从 config 复制，含 kind 与全部访问字段）。
    pub(super) fn access_snapshot_allows(&self, sender_id: &str) -> bool {
        self.bot.access_allows(sender_id)
    }

    /// #51 免 @ 开关的「config 优先、快照回落」读取（见 mention_snapshot 字段注释）。
    /// 判定键 = config.json 里有没有该 bot：有 → 生产路径（热读，重启保持）；
    /// 没有（单测随机 key / bot 被改名）或 config 读失败 → 内存快照（最后已知状态）。
    /// 快照与 config 写入路径写穿同步（见 set_mention_mode），config 临时读失败时
    /// 回落的是「最后一次生效的开关状态」而非启动时状态（fail-closed 语义成立）。
    pub(super) fn mention_mode(&self, chat_id: &str) -> Option<String> {
        match crate::config::Config::load() {
            Ok(c) => match c.bots.into_iter().find(|b| b.key() == self.bot.key()) {
                Some(b) => b.mention_modes.get(chat_id).cloned(),
                None => self.mention_snapshot.lock().unwrap().get(chat_id).cloned(),
            },
            Err(_) => self.mention_snapshot.lock().unwrap().get(chat_id).cloned(),
        }
    }

    /// 写穿快照：无论走 config 还是快照分支，内存快照都同步到本次目标状态。
    pub(super) fn write_snapshot(&self, chat_id: &str, mode: Option<&str>) {
        let mut m = self.mention_snapshot.lock().unwrap();
        match mode {
            Some(v) => {
                m.insert(chat_id.to_string(), v.to_string());
            }
            None => {
                m.remove(chat_id);
            }
        }
    }

    /// #51 写入开关。返回是否成功（false = config 加载/保存失败，未持久化——
    /// 调用方必须如实回显失败；此时快照仍同步写入，本次运行内行为一致，重启后不保留）。
    /// 成功路径写穿快照：config 与「最后已知状态」同源，config 临时读失败时
    /// 门槛回落的不是启动时快照而是最后一次生效的开关状态（审查 I2 fail-closed 承诺）。
    pub(super) fn set_mention_mode(&self, chat_id: &str, mode: Option<&str>) -> bool {
        // 写穿先行：任何路径下快照都代表「最后一次尝试的开关状态」
        self.write_snapshot(chat_id, mode);
        match crate::config::Config::set_mention_mode(&self.bot.key(), chat_id, mode) {
            crate::config::MentionModeSave::Saved | crate::config::MentionModeSave::BotNotFound => {
                true
            }
            crate::config::MentionModeSave::Failed => false,
        }
    }

    /// 发送者展示名解析（#74 历史/提醒展示用，8-20 用户反馈：显示名字不是 id）：
    /// 本地授权者名单优先（授权时已反查过名字），未授权用户 API 反查（best-effort）。
    /// 空 = 未查到（GUI 回落 id）。
    pub(super) async fn resolve_sender_name(&self, sender_id: &str) -> String {
        // 本地名单名字先克隆（释放 &self 借用——跨 await 持有引用会让 future 非 Send）
        let local = {
            let infos = if self.bot.is_dingtalk() {
                &self.bot.ding_granted_infos
            } else {
                &self.bot.granted_infos
            };
            infos
                .iter()
                .find(|i| i.open_id == sender_id)
                .map(|i| i.name.clone())
                .filter(|n| !n.is_empty())
        };
        if let Some(n) = local {
            return n;
        }
        self.msgr
            .user_display_name(sender_id)
            .await
            .unwrap_or_default()
    }

    /// 热读 config 推导（准入, 发送者角色）——on_payload / on_dingtalk 共用同一份
    /// load+find+快照回落，避免两个入口各写一份导致准入与角色推导漂移
    /// （同一发送者在不同通道被推导成不同角色 = 授权者拿到 owner 权限或反之）。
    /// 第三个返回值 = 该 bot 的 mention_modes（config 路径成功时 Some，含空 map——
    /// 空 map 同样是权威判定；config 无该 bot / 读失败时 None，由调用方回落快照）。
    /// 门槛与准入共用这一次 load：未 @ 的顶层群消息不必再整份读一次 config.json。
    pub(super) fn access_and_role(
        &self,
        sender_id: &str,
    ) -> (
        bool,
        crate::config::SenderRole,
        Option<std::collections::HashMap<String, String>>,
    ) {
        match crate::config::Config::load() {
            Ok(c) => match c.bots.into_iter().find(|b| b.key() == self.bot.key()) {
                Some(b) => (
                    b.access_allows(sender_id),
                    b.sender_role(sender_id),
                    Some(b.mention_modes),
                ),
                None => (
                    self.access_snapshot_allows(sender_id),
                    self.bot.sender_role(sender_id),
                    None,
                ),
            },
            Err(_) => (
                self.access_snapshot_allows(sender_id),
                self.bot.sender_role(sender_id),
                None,
            ),
        }
    }

    /// #51 门槛判定：config 路径有该 bot → 以 config 的 map 为准（无条目 = 需要 @）；
    /// 否则（单测随机 key / 读失败）→ 回落内存快照。
    pub(super) fn mention_off(
        &self,
        mention: &Option<std::collections::HashMap<String, String>>,
        chat_id: &str,
    ) -> bool {
        match mention {
            Some(m) => m.get(chat_id).map(String::as_str) == Some("off"),
            None => {
                self.mention_snapshot
                    .lock()
                    .unwrap()
                    .get(chat_id)
                    .map(String::as_str)
                    == Some("off")
            }
        }
    }

    /// 尝试把一条消息当作授权码处理（owner 生成后给到对方）。仅 p2p 接受（飞书 p2p / 钉钉单聊，
    /// 群里发码太公开防抢注）；文本精确匹配 pending 码 → 消费并把发送者加入对应白名单
    /// （管理员码→owner / 普通码→授权者，按 bot kind 落位到 open_id 或 staffId 字段）、回发结果。
    /// 返回 true = 授权码消息已消费/回复，调用方应 return（不再进 agent）。
    pub(super) async fn try_consume_owner_code(
        &self,
        sender_id: &str,
        chat_id: &str,
        is_p2p: bool,
        text: &str,
    ) -> bool {
        let text = text.trim();
        if !is_p2p || text.is_empty() {
            return false;
        }
        use crate::config::OwnerCodeResult as R;
        // 先查发送者展示名（best-effort，查不到用 id 兜底）：随授权一起落盘，GUI 授权列表
        // 能显示「谁」。查名放授权前：失败不阻塞授权。
        let name = self
            .msgr
            .user_display_name(sender_id)
            .await
            .unwrap_or_default();
        let r = crate::config::Config::consume_owner_code(&self.bot.key(), text, sender_id, &name);
        let reply = match r {
            R::Granted => Some("✅ 授权成功，你现在可以在这个 bot 里对话了。"),
            R::Expired => Some("❌ 授权码已过期，请联系管理员重新生成。"),
            R::NotFound => None, // 不是授权码 → 按未授权消息忽略
        };
        let Some(txt) = reply else {
            return false;
        };
        if let Err(e) = self.msgr.send_text(chat_id, txt).await {
            crate::log!(
                "[bridge] 授权码回复发送失败 chat={}: {e:#}",
                trunc(chat_id, 10)
            );
        }
        crate::log!(
            "[bridge] 授权码消息处理完成（bot={} sender={} result={:?}）",
            self.bot.key(),
            sender_id,
            r
        );
        true
    }
}
