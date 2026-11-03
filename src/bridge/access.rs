//! Bridge 子模块：访问控制/授权/会话模式（#80 按功能面拆分，impl Bridge 分散到子模块——
//! 子模块是父模块后代，可访问 mod.rs 私有字段，无需改可见性）。

use super::*;

impl Bridge {
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
}
