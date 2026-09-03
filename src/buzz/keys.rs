//! 频道 uuid 派生（从原 `buzzrelay.rs` 抽取，随 relay 删除迁移至此）。
//!
//! 命名空间与算法**不得改动**：频道 uuid 是确定性映射（bot_key, chat_id[, thread]）
//! ↔ uuid 的单一事实来源，历史话题路由、消息锚点、会话键控全部依赖它。新增
//! 映射（如新话题隔离层）只允许加新命名空间，不允许改存量算法/命名空间字符串。

fn fnv128_uuid(ns: &str) -> String {
    fn fnv64(seed: u64, s: &str) -> u64 {
        let mut h = seed;
        for b in s.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }
    let hi = fnv64(0xcbf2_9ce4_8422_2325, ns);
    let lo = fnv64(0x9e37_79b9_7f4a_7c15, ns);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..].copy_from_slice(&lo.to_be_bytes());
    let hex = |r: std::ops::Range<usize>| {
        bytes[r]
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<String>()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..10),
        hex(10..16)
    )
}

/// 频道 uuid：fnv128 确定性派生（命名空间与 #194 vb_uuid 区分）。
/// chat_id ↔ uuid 双向映射由本函数 + 登记表共同维护。
pub fn channel_uuid(bot_key: &str, chat_id: &str) -> String {
    fnv128_uuid(&format!("abb-relay:{bot_key}:{chat_id}"))
}

/// #206 话题隔离：话题频道 uuid——命名空间在群根基础上加 thread 段
///（群根 uuid 算法不动：存量频道映射/话题路由全部不失效）。
/// 「话题 = 独立 buzz 频道」而非「同频道 + 话题 tag」：会话/队列/轮次
/// 全按 channel_id 键控，频道方案根治同群话题串线，回复路由
///（#h → 话题频道 → chat+thread）自动正确。不同话题/不同 bot 互异。
pub fn topic_channel_uuid(bot_key: &str, chat_id: &str, thread_id: &str) -> String {
    fnv128_uuid(&format!("abb-relay:{bot_key}:{chat_id}:thread:{thread_id}"))
}
