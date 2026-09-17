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

/// 该字符串是否「形如 buzz 频道 UUID」——即 [`channel_uuid`] 的产物形态（canonical uuid）。
///
/// 用途（投递侧硬闸）：真实平台的会话 id（飞书 `oc_…`/`ou_…`、微信 `wxid…`/`o9cq…`、
/// 钉钉 `cid…`）**都不是**裸 UUID；而 `AGENT_BRIDGE_CHAT_ID` 这类跨进程注入值在 ACP
/// 架构下曾实际被填成频道 UUID。目标一旦命中本判据，就绝不可能是任何平台可用的
/// receive_id，直发必被平台拒（飞书 `230001 invalid receive_id`）——应当 fail loud
/// 或回落主会话，而不是静默发出注定失败的请求。**只做形态判定**，不承诺能反解回
/// chat_id（fnv128 不可逆，登记表是内存态）。
pub fn looks_like_channel_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 频道 UUID 形态判定：`channel_uuid` 的产物命中；真实平台 id 一律不命中。
    /// 这是「绝不把频道 UUID 当 receive_id 直发」的形态锁（deliver / task 两条链共用）。
    #[test]
    fn looks_like_channel_uuid_only_matches_uuid_shape() {
        let uuid = channel_uuid(
            "cli_a8a27ff268b8900e",
            "oc_1f097b843c4d12b3bc8b91205cfe4dd8",
        );
        // 与线上坏值逐一吻合（三个 bot 的频道 UUID 皆是此形态）
        assert_eq!(uuid, "f72338af-1402-7f76-4520-189911e0d106");
        assert!(looks_like_channel_uuid(&uuid));
        // 话题频道 uuid 同形态
        assert!(looks_like_channel_uuid(&topic_channel_uuid(
            "b", "oc_x", "t1"
        )));
        // 三个平台的真实 chat_id：都不是裸 UUID
        assert!(!looks_like_channel_uuid(
            "oc_1f097b843c4d12b3bc8b91205cfe4dd8"
        ));
        assert!(!looks_like_channel_uuid("ou_abc123"));
        assert!(!looks_like_channel_uuid(
            "o9cq806Evm1T9LW4PNihIL11j2cEimwechat"
        ));
        assert!(!looks_like_channel_uuid("cid_something"));
        assert!(!looks_like_channel_uuid(""));
    }
}
