//! #200 Phase 1：mini-relay —— buzz-acp 消费的 Nostr NIP-01/42/98 协议子集。
//!
//! 定位：ABB 扮演 relay 角色，让**未改动的 buzz-acp**（配置 BUZZ_RELAY_URL 指向 ABB）
//! 直接连上来订阅虚拟 Bot 频道、触发 buzz-agent、把回复经 buzz-cli 回流到 ABB。
//! 只实现 buzz-acp 消费的封闭子集（见 #200 协议清单）；buzz-relay 的生产平台机制
//! （PG/Redis/多租户/mesh/媒体/git/音频/工作流）一律不做。
//!
//! 协议面：
//! - WS（NIP-01）：`["EVENT", event]`（验签+存储+fan-out+ACK）、`["REQ", sub, filters]`
//!   （订阅+历史回放+EOSE）、`["CLOSE", sub]`；NIP-42：连接即发 `["AUTH", challenge]`，
//!   收到 kind:22242 auth event 验签后标记已认证（不强制门禁——本地回环信任模型）。
//! - HTTP（NIP-98 头可选验签）：`POST /events`（同 EVENT 摄取）、`POST /query`
//!   （filter 数组 → 匹配事件 JSON 数组）。
//! - 频道发现（buzz-acp discover_channels 两步）：`POST /query`
//!   ① filter kinds=[39002] `#p=[agent_pubkey]` → 成员事件（`#d`=频道 uuid）
//!   ② filter kinds=[39000] `#d=[uuids]` → 频道元数据（about=角色描述）。
//!
//! 频道模型：ABB 登记的虚拟 Bot 群 = 频道。channel uuid = fnv128 确定性派生
//! （bot_key+chat_id，与 #194 vb_uuid 同思路不同命名空间），chat_id ↔ uuid 双向
//! 映射由本模块维护（登记表驱动）。agent 回复事件（kind 9，`#h`=频道 uuid）经
//! 回流通道交还 bridge 发往聊天平台。
//!
//! 事件存储用 turso（0.7.2）而非 rusqlite：API 异步原生（tokio），与 relay 的
//! async 上下文同栈；`h_tag` 单列直查（不依赖 json_each 扩展）。

use nostr::prelude::*;

/// fnv128 确定性 uuid 派生（命名空间字符串 → uuid 形态文本）。
/// channel_uuid / topic_channel_uuid 共用（两者只差命名空间，算法绝不漂移）。
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
///（群根 uuid 算法不动：存量频道映射/账本/库内事件全部不失效）。
/// 「话题 = 独立 buzz 频道」而非「同频道 + 话题 tag」：buzz-acp 的会话/队列/轮次
/// 全按 channel_id 键控（上游 pool.rs:117 @ c3132c3），频道方案根治同群话题串线，
/// 且回复路由（#h → 话题频道 → chat+thread）自动正确。不同话题/不同 bot 互异。
pub fn topic_channel_uuid(bot_key: &str, chat_id: &str, thread_id: &str) -> String {
    fnv128_uuid(&format!("abb-relay:{bot_key}:{chat_id}:thread:{thread_id}"))
}

/// NIP-01 客户端消息（mini-relay 消费的子集）。
#[derive(Debug, Clone)]
pub enum ClientFrame {
    Event(Box<nostr::Event>),
    Req {
        sub_id: String,
        filters: Vec<nostr::Filter>,
    },
    Close(String),
    /// NIP-42 AUTH 响应（kind 22242 事件本体——必须拿它的 id 回 OK，见 ws_loop 的
    /// Auth 臂：buzz-acp 的 do_connect 在发出 auth 后**硬等** accepted=true 的 OK，
    /// 拿不到即判连接失败并重试到退出）。
    Auth(Box<nostr::Event>),
}

/// 解析 NIP-01 客户端帧（["EVENT",e] / ["REQ",sub,filters] / ["CLOSE",sub]）。
/// 未知/畸形返回 None（调用方忽略或 NOTICE）。
pub fn parse_client_frame(text: &str) -> Option<ClientFrame> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let arr = v.as_array()?;
    let tag = arr.first()?.as_str()?;
    match tag {
        "EVENT" => {
            let e: nostr::Event = serde_json::from_value(arr.get(1)?.clone()).ok()?;
            Some(ClientFrame::Event(Box::new(e)))
        }
        "REQ" => {
            let sub_id = arr.get(1)?.as_str()?.to_string();
            let filters: Vec<nostr::Filter> = arr
                .iter()
                .skip(2)
                .filter_map(|f| serde_json::from_value(f.clone()).ok())
                .collect();
            Some(ClientFrame::Req { sub_id, filters })
        }
        "AUTH" => {
            let e: nostr::Event = serde_json::from_value(arr.get(1)?.clone()).ok()?;
            Some(ClientFrame::Auth(Box::new(e)))
        }
        "CLOSE" => Some(ClientFrame::Close(arr.get(1)?.as_str()?.to_string())),
        _ => None,
    }
}

/// event 是否命中 filter（mini-relay 子集：kinds、#h 单字母频道 tag、#p、since/until）。
/// 其余 filter 字段忽略——buzz-acp 的 REQ 只用这些（以 relay.rs 的消费为准）。
pub fn filter_matches(f: &nostr::Filter, e: &nostr::Event) -> bool {
    if let Some(kinds) = &f.kinds {
        if !kinds.is_empty() && !kinds.contains(&e.kind) {
            return false;
        }
    }
    if let Some(since) = f.since {
        if e.created_at < since {
            return false;
        }
    }
    if let Some(until) = f.until {
        if e.created_at > until {
            return false;
        }
    }
    // generic_tags: 单字母 tag（#h/#p…）→ 值集合（BTreeMap<SingleLetterTag, BTreeSet>）
    for (tag_letter, values) in &f.generic_tags {
        let tag_name = tag_letter.to_string();
        let hit = e.tags.iter().any(|tags| {
            tags.as_slice()
                .first()
                .is_some_and(|t| t.as_str() == tag_name)
                && tags.as_slice().len() > 1
                && tags.as_slice()[1..].iter().any(|v| values.contains(v))
        });
        if !hit {
            return false;
        }
    }
    true
}

/// 摄取准入结果（见 RelayState::admit）。
enum Admit {
    /// 入库 + fan-out（作者与 kind 都在权威表内）
    Store,
    /// 只 fan-out 不入库（NIP-01 瞬时事件）
    Ephemeral,
    /// 拒收（附返回给客户端的原因）
    Reject(String),
}

/// 从事件 tags 提取 `#h` 首值（频道 uuid）。pub(crate)：bridge 测试断言复用
///（单一实现，防测试侧复刻漂移）。
pub(crate) fn h_tag_of(e: &nostr::Event) -> Option<String> {
    e.tags.iter().find_map(|tags| {
        tags.as_slice()
            .first()
            .is_some_and(|t| t.as_str() == "h")
            .then(|| tags.as_slice().get(1).map(|v| v.as_str().to_string()))
            .flatten()
    })
}

/// #206：从事件 tags 提取首个 `e` tag 的值（NIP-10 回复指向的被回复事件 id）。
/// buzz-acp 回复用户消息时经 `buzz messages send --reply-to <event_id>` 产出该 tag
/// （上游 queue.rs / builders.rs 的 thread_tags，pin SHA c3132c3 已核实）。agent 不带
/// --reply-to 时无 e-tag —— 软关联失败，回流侧按 chat 兜底（风险①，不阻塞主链路）。
fn first_e_tag(e: &nostr::Event) -> Option<String> {
    e.tags.iter().find_map(|tags| {
        tags.as_slice()
            .first()
            .is_some_and(|t| t.as_str() == "e")
            .then(|| tags.as_slice().get(1).map(|v| v.as_str().to_string()))
            .flatten()
    })
}

/// #200 测试夹具：临时 sqlite 库路径（uuid 唯一，防并行测试互删——见仓库 LESSON）。
/// pub(crate)：bridge 侧 dispatch 测试复用，避免第二份拷贝。
#[cfg(test)]
pub(crate) fn test_db(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}.db", uuid::Uuid::new_v4()))
}

/// #200 测试夹具：删库及 -wal/-shm 旁文件（只删主文件必漏，审查 #205r2）。
/// ⚠️ 必须等 EventStore/RelayState 全部 drop 之后再调。
#[cfg(test)]
pub(crate) fn remove_test_db(p: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use nostr::prelude::*;

    /// channel_uuid：确定性、uuid 形态、不同群不同、与 bot 绑定。
    #[test]
    fn channel_uuid_is_deterministic() {
        let a = channel_uuid("bot_a", "oc_1");
        assert_eq!(a, channel_uuid("bot_a", "oc_1"));
        assert_ne!(a, channel_uuid("bot_a", "oc_2"));
        assert_ne!(a, channel_uuid("bot_b", "oc_1"));
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            [
                parts[0].len(),
                parts[1].len(),
                parts[2].len(),
                parts[3].len(),
                parts[4].len()
            ],
            [8, 4, 4, 4, 12]
        );
    }

    /// NIP-01 帧解析：EVENT / REQ / CLOSE。
    #[test]
    fn parse_client_frames() {
        let keys = Keys::generate();
        let e = EventBuilder::new(Kind::Custom(9), "hi")
            .sign_with_keys(&keys)
            .unwrap();
        let frame = format!("[\"EVENT\",{}]", e.as_json());
        match parse_client_frame(&frame) {
            Some(ClientFrame::Event(ev)) => assert_eq!(ev.id, e.id),
            other => panic!("EVENT 帧解析失败: {other:?}"),
        }
        let req = r##"["REQ","sub1",{"kinds":[9],"#h":["5c834a8f-4bbd-4d13-8206-0f262c0e15ce"]}]"##;
        match parse_client_frame(req) {
            Some(ClientFrame::Req { sub_id, filters }) => {
                assert_eq!(sub_id, "sub1");
                assert_eq!(filters.len(), 1);
            }
            other => panic!("REQ 帧解析失败: {other:?}"),
        }
        match parse_client_frame(r#"["CLOSE","sub1"]"#) {
            Some(ClientFrame::Close(s)) => assert_eq!(s, "sub1"),
            other => panic!("CLOSE 帧解析失败: {other:?}"),
        }
        assert!(parse_client_frame(r#"["UNKNOWN"]"#).is_none());
        assert!(parse_client_frame("not json").is_none());
    }

    /// filter 匹配子集：kinds/#h/since。
    #[test]
    fn filter_matches_subset() {
        let keys = Keys::generate();
        let e: nostr::Event = EventBuilder::new(Kind::Custom(9), "msg")
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                ["chan-uuid"],
            ))
            .sign_with_keys(&keys)
            .unwrap();
        let f: Filter = serde_json::from_str(r##"{"kinds":[9],"#h":["chan-uuid"]}"##).unwrap();
        assert!(filter_matches(&f, &e));
        let f_other: Filter = serde_json::from_str(r##"{"kinds":[9],"#h":["other"]}"##).unwrap();
        assert!(!filter_matches(&f_other, &e));
    }

    /// h_tag_of：提取频道 uuid tag。
    #[test]
    fn h_tag_extraction() {
        let keys = Keys::generate();
        let e: nostr::Event = EventBuilder::new(Kind::Custom(9), "msg")
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                ["chan-1"],
            ))
            .sign_with_keys(&keys)
            .unwrap();
        assert_eq!(h_tag_of(&e).as_deref(), Some("chan-1"));
        let e2: nostr::Event = EventBuilder::new(Kind::Custom(9), "no h")
            .sign_with_keys(&keys)
            .unwrap();
        assert_eq!(h_tag_of(&e2), None);
    }

    /// publish_user_message（#200 dispatch）：uuid 按 (bot_key, chat_id) 派生定位频道；
    /// 未登记 → None 且不入库；已登记 → Some(事件 id)、kind 9、单 #h tag、内容原样；
    /// 同秒同文不同 mid → 两条独立入库（事件 id 含 mid tag，不撞 hash 被吞）。
    /// #206：返回的事件 id 必须与入库事件 id 一致（账本 awaiting 按它关联回复 e-tag）。
    #[tokio::test]
    async fn publish_user_message_maps_channel_with_single_h_tag() {
        let db_path = test_db("abb-buzzrelay-pub");
        let store = EventStore::open(&db_path).await.unwrap();
        let (state, _reply_rx) = RelayState::new(
            store,
            Keys::generate(),
            Keys::generate().public_key().to_hex(),
        );
        let uuid_a = channel_uuid("bot_a", "oc_a");
        state.set_channels([Channel {
            uuid: uuid_a.clone(),
            chat_id: "oc_a".into(),
            name: "角色A".into(),
            about: String::new(),
            bot_key: "bot_a".into(),
            thread_id: String::new(),
            anchor_mid: String::new(),
        }]);

        // 未登记 chat（含「另一 bot 的同名 chat」——uuid 含 bot_key，不串线）：None 不入库
        assert!(state
            .publish_user_message("bot_x", "oc_a", "m0", "hi", "")
            .await
            .is_none());
        assert!(state
            .publish_user_message("bot_a", "oc_unknown", "m0", "hi", "")
            .await
            .is_none());
        let all: Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        assert!(state.db.query(std::slice::from_ref(&all)).await.is_empty());

        // 已登记频道：Some(事件 id)、kind 9、单 #h tag、内容原样
        let id1 = state
            .publish_user_message("bot_a", "oc_a", "m1", "你好，buzz", "")
            .await
            .expect("已登记频道应返回事件 id");
        let evs = state.db.query(&[all]).await;
        assert_eq!(evs.len(), 1);
        let e = &evs[0];
        assert_eq!(
            e.id.to_hex(),
            id1,
            "返回的事件 id 必须与入库事件 id 一致（回复账本按它关联）"
        );
        assert_eq!(e.kind.as_u16(), 9);
        assert_eq!(e.content, "你好，buzz");
        let h_count = e
            .tags
            .iter()
            .filter(|t| t.as_slice().first().is_some_and(|k| k.as_str() == "h"))
            .count();
        assert_eq!(h_count, 1);
        assert_eq!(h_tag_of(e).as_deref(), Some(uuid_a.as_str()));

        // 同秒同文（「好」「好」）：mid 不同 → 事件 id 不同 → 两条都在库（回归：
        // 无 mid tag 时第二条撞 id 被 INSERT OR IGNORE 静默吞）。
        let id2 = state
            .publish_user_message("bot_a", "oc_a", "m2", "好", "")
            .await
            .expect("m2 应返回事件 id");
        let id3 = state
            .publish_user_message("bot_a", "oc_a", "m3", "好", "")
            .await
            .expect("m3 应返回事件 id");
        assert_ne!(id2, id3, "同秒同文不同 mid 不得撞事件 id");
        let nine: Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        let evs = state.db.query(&[nine]).await;
        assert_eq!(evs.len(), 3, "mid tag 应保证同文不同 mid 的两条各自入库");
        drop(state);
        drop(_reply_rx);
        remove_test_db(&db_path);
    }

    /// #206：publish_control_command 事件形态四要素（对照上游 is_owner_control_command
    /// 判据，buzz-acp lib.rs:3552-3562 @ c3132c3）：kind==9、content 精确 "!cancel"
    /// （不得带任何前后缀）、#h==channel_uuid(bot,chat)、#p==agent 公钥、桥身份签名；
    /// 连发两条不撞事件 id（abb-mid nonce 生效——同秒同 content 撞内容哈希会被
    /// INSERT OR IGNORE 吞）；无频道 → false 且不入库。
    #[tokio::test]
    async fn publish_control_command_event_shape() {
        let db_path = test_db("abb-buzzrelay-ctrl");
        let store = EventStore::open(&db_path).await.unwrap();
        let agent = Keys::generate();
        let bridge = Keys::generate();
        let (state, _rx) = RelayState::new(store, bridge.clone(), agent.public_key().to_hex());
        let uuid_a = channel_uuid("bot_a", "oc_a");
        state.set_channels([Channel {
            uuid: uuid_a.clone(),
            chat_id: "oc_a".into(),
            name: "角色A".into(),
            about: String::new(),
            bot_key: "bot_a".into(),
            thread_id: String::new(),
            anchor_mid: String::new(),
        }]);
        let all: Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();

        // 未登记 chat → false 且不入库
        assert!(
            !state
                .publish_control_command("bot_a", "oc_unknown", "", ControlCommand::Cancel)
                .await
        );
        assert!(state.db.query(std::slice::from_ref(&all)).await.is_empty());

        // 已登记频道 → true，事件四要素
        assert!(
            state
                .publish_control_command("bot_a", "oc_a", "", ControlCommand::Cancel)
                .await
        );
        let evs = state.db.query(std::slice::from_ref(&all)).await;
        assert_eq!(evs.len(), 1);
        let e = &evs[0];
        assert_eq!(e.kind.as_u16(), 9);
        // 协议钉扎：上游 content.trim()=="!cancel" 精确比对（lib.rs:2791/3558）——
        // content 多一个字符（上下文前缀/后缀）都会被 acp 当普通消息喂给 agent。
        assert_eq!(e.content, "!cancel");
        assert_eq!(h_tag_of(e).as_deref(), Some(uuid_a.as_str()));
        let agent_hex = agent.public_key().to_hex();
        let p_hit = e.tags.iter().any(|t| {
            t.as_slice().first().is_some_and(|k| k.as_str() == "p")
                && t.as_slice().get(1).is_some_and(|v| v.as_str() == agent_hex)
        });
        assert!(
            p_hit,
            "#p 必须 mention agent 公钥（event_mentions_agent lib.rs:3545）"
        );
        assert_eq!(
            e.pubkey,
            bridge.public_key(),
            "桥身份签名（owner 门 author==桥公钥，lib.rs:2796）"
        );

        // 连发第二条（同秒同 content）：abb-mid nonce 保事件 id 唯一，不被吞
        assert!(
            state
                .publish_control_command("bot_a", "oc_a", "", ControlCommand::Cancel)
                .await
        );
        let evs = state.db.query(&[all]).await;
        assert_eq!(
            evs.len(),
            2,
            "两条 !cancel 必须各自入库（同 content 撞 id 会被 INSERT OR IGNORE 吞）"
        );
        assert_ne!(evs[0].id, evs[1].id);
        // 同事件重发 = Duplicate 算送达：与 publish_user_message 共用 store3 三态口径
        //（Duplicate 臂无法经本 API 触发——nonce 每次新生成；该臂由 store3 单测语义
        // 与 ingest 重放测试覆盖）。
        drop(state);
        drop(_rx);
        remove_test_db(&db_path);
    }

    /// #206：agent 身份非法（空 pubkey）→ 拒发且不入库（同 publish_user_message 的
    /// I1 口径：命令无法定址时如实失败，不发注定无人订阅的事件）。
    #[tokio::test]
    async fn publish_control_command_fails_without_agent_identity() {
        let db_path = test_db("abb-buzzrelay-ctrl-nopk");
        let store = EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = RelayState::new(store, Keys::generate(), String::new());
        state.set_channels([Channel {
            uuid: channel_uuid("bot_a", "oc_a"),
            chat_id: "oc_a".into(),
            name: "角色".into(),
            about: String::new(),
            bot_key: "bot_a".into(),
            thread_id: String::new(),
            anchor_mid: String::new(),
        }]);
        assert!(
            !state
                .publish_control_command("bot_a", "oc_a", "", ControlCommand::Cancel)
                .await
        );
        let all: Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        assert!(
            state.db.query(std::slice::from_ref(&all)).await.is_empty(),
            "无 agent 身份不得发出控制事件"
        );
        drop(state);
        drop(_rx);
        remove_test_db(&db_path);
    }

    /// P1-1 安全闸（审查 #212）：publish_user_message 以桥身份（=acp owner 门）
    /// 签名——原文 trim 后精确命中 owner 控制命令字面量必须**拒发且不入库**
    ///（否则群成员发纯文本 "!shutdown" 会被 acp 当 owner 命令执行，终态杀 acp
    /// 不复活，buzz-acp lib.rs:2756-2779）。非精确命中的相似文本不受影响
    ///（上游判据是 content.trim()==literal 精确比对，大小写敏感）。
    #[tokio::test]
    async fn publish_user_message_rejects_control_command_literals() {
        let db_path = test_db("abb-buzzrelay-gate");
        let store = EventStore::open(&db_path).await.unwrap();
        let (state, _rx) = RelayState::new(
            store,
            Keys::generate(),
            Keys::generate().public_key().to_hex(),
        );
        state.set_channels([Channel {
            uuid: channel_uuid("bot_a", "oc_a"),
            chat_id: "oc_a".into(),
            name: "角色A".into(),
            about: String::new(),
            bot_key: "bot_a".into(),
            thread_id: String::new(),
            anchor_mid: String::new(),
        }]);
        let all: Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();

        // 三个字面量 + 带首尾空白（上游 trim 后比对）全部拒发
        //（#206 回复侧记账把返回值改为 Option<String>：拒发 = None）
        for (i, lit) in ["!cancel", "!shutdown", "!rotate", "  !shutdown  "]
            .iter()
            .enumerate()
        {
            assert!(
                state
                    .publish_user_message("bot_a", "oc_a", &format!("bad{i}"), lit, "")
                    .await
                    .is_none(),
                "{lit:?} 必须被拒发"
            );
        }
        assert!(
            state.db.query(std::slice::from_ref(&all)).await.is_empty(),
            "被拒的控制字面量不得入库（入库即会被 REQ 回放喂给 acp）"
        );

        // 非精确命中的相似文本不受影响（正常透传）
        assert!(state
            .publish_user_message("bot_a", "oc_a", "ok1", "!shutdown 现在", "")
            .await
            .is_some());
        assert!(state
            .publish_user_message("bot_a", "oc_a", "ok2", "请执行 !rotate 流程", "")
            .await
            .is_some());
        assert!(
            state
                .publish_user_message("bot_a", "oc_a", "ok3", "!CANCEL", "")
                .await
                .is_some(),
            "上游比对大小写敏感，!CANCEL 不是控制命令"
        );
        assert_eq!(state.db.query(&[all]).await.len(), 3);
        drop(state);
        drop(_rx);
        remove_test_db(&db_path);
    }

    /// Origin 判据回归（审查 #205r5）：前缀匹配被 `127.0.0.1.evil.com` 绕过、
    /// "null" 白名单放进 file:// 页面——两处都得拒；原生客户端不发 Origin（由
    /// 调用方放行）不在本函数职责内。
    #[test]
    fn origin_allowed_requires_exact_loopback_host() {
        assert!(origin_allowed("http://127.0.0.1:3000"));
        assert!(origin_allowed("https://localhost"));
        assert!(origin_allowed("http://[::1]:8080"));
        // 攻击面
        assert!(
            !origin_allowed("http://127.0.0.1.evil.com"),
            "前缀绕过必须被拒"
        );
        assert!(!origin_allowed("http://localhostevil.com"));
        assert!(!origin_allowed("null"), "file:// 页面不得放行");
        assert!(!origin_allowed("https://example.com"));
        assert!(!origin_allowed("garbage"), "非 URL 一律拒");
    }

    /// NIP-42 auth 裁决回归：kind/签名/challenge tag 三道都要过；challenge 错误
    /// （别处 auth 事件重放）必须拒——这是挡「跨连接重放认证」的那道闸。
    #[test]
    fn auth_decision_verifies_kind_sig_and_challenge() {
        let keys = Keys::generate();
        // EventBuilder::auth() 即 NIP-42 正形（challenge+relay tag），buzz 也用它
        let mk = |challenge: &str| {
            EventBuilder::auth(
                challenge,
                nostr::RelayUrl::parse("ws://127.0.0.1:3000").unwrap(),
            )
            .sign_with_keys(&keys)
            .unwrap()
        };
        assert!(auth_decision(&mk("chal-1"), "chal-1").is_ok());
        let wrong = mk("chal-other");
        assert!(
            auth_decision(&wrong, "chal-1")
                .unwrap_err()
                .contains("challenge"),
            "challenge 不匹配必须拒（跨连接重放闸）"
        );
        // kind 不对：把同一签名结构做成普通事件
        let not_auth = EventBuilder::new(Kind::Custom(9), "")
            .sign_with_keys(&keys)
            .unwrap();
        assert!(auth_decision(&not_auth, "chal-1").is_err());
        // 无 challenge tag = 缺失 → 拒
        let no_tag = EventBuilder::new(Kind::Authentication, "")
            .sign_with_keys(&keys)
            .unwrap();
        assert!(auth_decision(&no_tag, "chal-1").is_err());
    }

    /// 消费者判据回归（审查 #205r4/r5）：预检判「有连接 REQ 订阅了本频道」，
    /// kinds 不含 9、#h 不匹配、无订阅，都不得算有消费者。
    #[tokio::test]
    async fn has_subscription_for_matches_channel_filters() {
        let db = test_db("abb-buzzrelay-sub");
        let store = EventStore::open(&db).await.unwrap();
        let (state, _rx) = RelayState::new(
            store,
            Keys::generate(),
            Keys::generate().public_key().to_hex(),
        );
        let conn_id = state.conn_seq.fetch_add(1, Ordering::Relaxed);
        let (tx, _rx2) = tokio::sync::mpsc::unbounded_channel();
        state.conns.lock().unwrap().insert(conn_id, tx);
        let mk = |json: &str| -> Filter { serde_json::from_str(json).unwrap() };

        // 无订阅 → false
        assert!(!state.has_subscription_for("chan-1"));

        // kinds 含 9 且 #h 命中 → true
        state
            .subs
            .lock()
            .unwrap()
            .entry(conn_id)
            .or_default()
            .insert(
                "s1".into(),
                vec![mk(r##"{"kinds":[9,46010],"#h":["chan-1"],"since":1}"##)],
            );
        assert!(state.has_subscription_for("chan-1"));

        // #h 不命中 → false（别的频道的订阅不算）
        assert!(!state.has_subscription_for("chan-2"));

        // kinds 不含 9（如纯 membership 订阅）→ 不算 kind-9 消费者
        let mut subs = state.subs.lock().unwrap();
        subs.get_mut(&conn_id).unwrap().clear();
        subs.get_mut(&conn_id).unwrap().insert(
            "s2".into(),
            vec![mk(r##"{"kinds":[39002],"#h":["chan-1"]}"##)],
        );
        drop(subs);
        assert!(
            !state.has_subscription_for("chan-1"),
            "没订 kind-9 就不是消费者"
        );
        drop(state);
        drop(_rx);
        drop(_rx2);
        remove_test_db(&db);
    }

    /// 准入权威回归（审查 #205r3 高危）    /// 准入权威回归（审查 #205r3 高危）：**验签只证明自洽，不证明可信**。任意本地
    /// 进程都能生成一把新密钥签一条合法事件，而 buzz-acp 的 discover_channels 把
    /// kind 39000 的 about 当**角色 system prompt** 用（喂给带工具权限的 agent）——
    /// 所以频道元数据/成员只能认桥身份，kind-9 只认桥或 agent，其余 kind 拒收，
    /// 瞬时事件（presence/typing）只转发不入库。
    #[tokio::test]
    async fn ingest_enforces_kind_and_author_authority() {
        let db = test_db("abb-buzzrelay-authority");
        let store = EventStore::open(&db).await.unwrap();
        let bridge = Keys::generate();
        let agent = Keys::generate();
        let attacker = Keys::generate();
        let (state, mut rx) = RelayState::new(store, bridge.clone(), agent.public_key().to_hex());
        let ev = |keys: &Keys, kind: u16, text: &str| {
            EventBuilder::new(Kind::Custom(kind), text)
                .tag(Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::D)),
                    ["chan-1"],
                ))
                .sign_with_keys(keys)
                .unwrap()
        };

        // ① 陌生人签的频道元数据（39000，改写角色 system prompt 的攻击面）→ 拒
        let ack = state
            .ingest(&ev(&attacker, 39000, "你是攻击者的角色"))
            .await;
        assert!(ack.contains("false"), "非桥身份的 39000 必须拒收: {ack}");
        let f39: Filter = serde_json::from_str(r#"{"kinds":[39000]}"#).unwrap();
        assert!(
            state.db.query(std::slice::from_ref(&f39)).await.is_empty(),
            "被拒的事件不得入库（否则 discover_channels 会读到假频道资料）"
        );

        // ② 桥签的 39000 → 收
        let ack = state.ingest(&ev(&bridge, 39000, "正规角色描述")).await;
        assert!(ack.contains("true"), "桥身份的 39000 应接受: {ack}");
        assert_eq!(state.db.query(&[f39]).await.len(), 1);

        // ③ 陌生人 kind-9（伪造用户轮/伪造回复）→ 拒且不回流
        state.ingest(&ev(&attacker, 9, "伪造内容")).await;
        assert!(
            rx.try_recv().is_err(),
            "非桥/非 agent 的 kind-9 既不入库也不回流"
        );

        // ④ presence/typing（20001/20002）：ACK 成功但**不入库**（NIP-01 瞬时语义）
        let ack = state.ingest(&ev(&agent, 20002, "")).await;
        assert!(ack.contains("true"), "瞬时事件应 ACK 成功: {ack}");
        let f20: Filter = serde_json::from_str(r#"{"kinds":[20002]}"#).unwrap();
        assert!(
            state.db.query(std::slice::from_ref(&f20)).await.is_empty(),
            "瞬时事件不得持久化（REQ 回放会把陈旧在线态当历史喂回）"
        );

        // ⑤ 未列入白名单的 kind（含 auth 22242：只属于 WS 握手路径）→ 拒
        let ack = state.ingest(&ev(&attacker, 22242, "auth")).await;
        assert!(
            ack.contains("false"),
            "22242 不得经 HTTP /events 摄取: {ack}"
        );
        drop(state);
        drop(rx);
        remove_test_db(&db);
    }

    /// 回流安全门（审查 #205r2）：只有 **agent 身份签名**的 kind-9 才投递回聊天，
    /// 且只在**首次入库**投递。原判定「非 bridge 即回复」叠加本 PR 填上的 senders
    /// 路由 = 任意本地进程（或 SSRF 到 127.0.0.1:port 的 POST /events）自签一条
    /// `#h=任意频道 uuid` 的事件即可**以 bot 身份向真实群发任意文本**；WS 断连后
    /// buzz-acp 重发的同一条回复也会被投递两遍。
    /// #206：投递的 AgentReply 必须带 event_id（账本去重键/历史 mid）、in_reply_to
    /// （首个 e-tag）、created_at。
    #[tokio::test]
    async fn ingest_only_forwards_first_seen_agent_replies() {
        let db = test_db("abb-buzzrelay-gate");
        let store = EventStore::open(&db).await.unwrap();
        let agent = Keys::generate();
        let stranger = Keys::generate();
        let (state, mut rx) = RelayState::new(store, Keys::generate(), agent.public_key().to_hex());
        state.set_channels([Channel {
            uuid: "chan-1".into(),
            chat_id: "oc_1".into(),
            name: "角色".into(),
            about: String::new(),
            bot_key: "bot_a".into(),
            thread_id: String::new(),
            anchor_mid: String::new(),
        }]);
        let kind9 = |keys: &Keys, text: &str, h: &str| {
            EventBuilder::new(Kind::Custom(9), text)
                .tag(Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                    [h],
                ))
                .sign_with_keys(keys)
                .unwrap()
        };

        // ① 陌生人签的 kind-9：**准入层就拒**（连库都不进，见 admit）；即便绕过也
        //    绝不投递——两层防御各管一层，都不许漏（审查 #205r3 更正本注释）
        state
            .ingest(&kind9(&stranger, "冒充 bot 的伪造回复", "chan-1"))
            .await;
        assert!(
            rx.try_recv().is_err(),
            "安全回归：非 agent 身份的 kind-9 不得回流真实聊天"
        );

        // ② agent 签的（带 e-tag 指向用户事件）：投递一次，路由键与 chat 都对，
        //    #206 新字段齐全（event_id=事件 id hex，in_reply_to=首个 e-tag）
        let real = EventBuilder::new(Kind::Custom(9), "真回复")
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                ["chan-1"],
            ))
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E)),
                ["user-event-id-hex"],
            ))
            .sign_with_keys(&agent)
            .unwrap();
        state.ingest(&real).await;
        let got = rx.try_recv().expect("agent 回复应投递");
        assert_eq!(got.chat_id, "oc_1");
        assert_eq!(got.channel_uuid, "chan-1");
        assert_eq!(got.content, "真回复");
        assert_eq!(got.event_id, real.id.to_hex());
        assert_eq!(got.in_reply_to.as_deref(), Some("user-event-id-hex"));
        assert_eq!(got.created_at, real.created_at.as_secs());

        // ③ 同一事件重发（ACK 未收到 → 重连重投）：不得二次投递
        state.ingest(&real).await;
        assert!(
            rx.try_recv().is_err(),
            "重复事件不得把同一条回复再发一遍到群里"
        );

        // ④ agent 签但 #h 不在频道集：丢弃不投递（配日志）
        state
            .ingest(&kind9(&agent, "孤儿回复", "chan-unregistered"))
            .await;
        assert!(rx.try_recv().is_err(), "#h 不在频道集时不得投递");
        drop(state);
        drop(rx);
        remove_test_db(&db);
    }

    /// #206：first_e_tag 提取——无 e-tag → None；单个 → 其值；多个 → 首值
    ///（NIP-10 首 e-tag 是被回复事件，其余是 thread 祖先，只认首个）。
    #[test]
    fn first_e_tag_extracts_first_only() {
        let keys = Keys::generate();
        let no_e: nostr::Event = EventBuilder::new(Kind::Custom(9), "plain")
            .sign_with_keys(&keys)
            .unwrap();
        assert_eq!(first_e_tag(&no_e), None);

        let one: nostr::Event = EventBuilder::new(Kind::Custom(9), "reply")
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E)),
                ["ev-1"],
            ))
            .sign_with_keys(&keys)
            .unwrap();
        assert_eq!(first_e_tag(&one).as_deref(), Some("ev-1"));

        let multi: nostr::Event = EventBuilder::new(Kind::Custom(9), "reply")
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E)),
                ["ev-root"],
            ))
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E)),
                ["ev-parent"],
            ))
            .sign_with_keys(&keys)
            .unwrap();
        assert_eq!(
            first_e_tag(&multi).as_deref(),
            Some("ev-root"),
            "只取首个 e-tag（NIP-10 被回复事件）"
        );
    }

    /// #206 对账查询：按用户事件 id 反查 agent 回复——e-tag 命中才返回；非 agent
    /// 作者（桥签的用户轮也带同 e-tag 时）排除；#h 不在频道集的排除；无 e-tag /
    /// e-tag 指别处的不命中。返回的 AgentReply 带 event_id（账本去重键）。
    #[tokio::test]
    async fn find_agent_replies_to_matches_e_tag_and_agent_author() {
        let db = test_db("abb-buzzrelay-find");
        let store = EventStore::open(&db).await.unwrap();
        let bridge_keys = Keys::generate();
        let agent = Keys::generate();
        let (state, _rx) = RelayState::new(store, bridge_keys.clone(), agent.public_key().to_hex());
        state.set_channels([Channel {
            uuid: "chan-1".into(),
            chat_id: "oc_1".into(),
            name: "角色".into(),
            about: String::new(),
            bot_key: "bot_a".into(),
            thread_id: String::new(),
            anchor_mid: String::new(),
        }]);
        let h = || {
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                ["chan-1"],
            )
        };
        let et = |v: &str| {
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E)),
                [v],
            )
        };
        // 目标用户事件（桥签的 kind-9；其 id 是回复 e-tag 的指向）
        let user_ev = EventBuilder::new(Kind::Custom(9), "用户问题")
            .tag(h())
            .sign_with_keys(&bridge_keys)
            .unwrap();
        state.ingest(&user_ev).await;
        let uid = user_ev.id.to_hex();

        // ① agent 回复带 e-tag → 命中；同 e-tag 第二条（一轮多回复）也命中
        let r1 = EventBuilder::new(Kind::Custom(9), "回复一")
            .tag(h())
            .tag(et(&uid))
            .sign_with_keys(&agent)
            .unwrap();
        let r2 = EventBuilder::new(Kind::Custom(9), "回复二")
            .tag(h())
            .tag(et(&uid))
            .sign_with_keys(&agent)
            .unwrap();
        state.ingest(&r1).await;
        state.ingest(&r2).await;
        // ② 桥签的「假回复」（同 e-tag 但作者是桥——重放的用户事件/注入块）→ 排除
        let forged = EventBuilder::new(Kind::Custom(9), "桥签同 e-tag")
            .tag(h())
            .tag(et(&uid))
            .sign_with_keys(&bridge_keys)
            .unwrap();
        state.ingest(&forged).await;
        // ③ agent 签但 e-tag 指别处 / 无 e-tag / #h 未登记 → 不命中
        let other = EventBuilder::new(Kind::Custom(9), "别的轮次")
            .tag(h())
            .tag(et("someone-else"))
            .sign_with_keys(&agent)
            .unwrap();
        let no_tag = EventBuilder::new(Kind::Custom(9), "无 e-tag")
            .tag(h())
            .sign_with_keys(&agent)
            .unwrap();
        let unreg = EventBuilder::new(Kind::Custom(9), "未登记频道")
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                ["chan-x"],
            ))
            .tag(et(&uid))
            .sign_with_keys(&agent)
            .unwrap();
        state.ingest(&other).await;
        state.ingest(&no_tag).await;
        state.ingest(&unreg).await;

        let found = state.find_agent_replies_to(&uid, 0).await;
        let mut ids: Vec<String> = found.iter().map(|r| r.event_id.clone()).collect();
        ids.sort();
        let mut want = vec![r1.id.to_hex(), r2.id.to_hex()];
        want.sort();
        assert_eq!(ids, want, "只有 agent 签、e-tag 命中、频道已登记的回复返回");
        assert_eq!(found[0].chat_id, "oc_1");
        assert_eq!(found[0].in_reply_to.as_deref(), Some(uid.as_str()));
        assert!(
            state
                .find_agent_replies_to("never-seen", 0)
                .await
                .is_empty(),
            "无关联事件 → 空"
        );
        // since 下推生效（审查 P2）：未来起点 → 全空（本测试事件都是「现在」创建）
        assert!(
            state
                .find_agent_replies_to(&uid, crate::chrono_lite::unix_secs() + 3600)
                .await
                .is_empty(),
            "since 晚于回复 created_at 必须过滤掉全部"
        );
        drop(state);
        drop(_rx);
        remove_test_db(&db);
    }

    // ---- #206 话题隔离（话题 = 独立 buzz 频道）----

    /// topic_channel_uuid：确定性；与群根互异；同群互异话题互异；同群同话题不同
    /// bot 互异（uuid 含 bot_key——两 bot 同群同话题不串线）；uuid 形态同群根。
    #[test]
    fn topic_channel_uuid_is_deterministic_and_isolated() {
        let root = channel_uuid("bot_a", "oc_1");
        let t1 = topic_channel_uuid("bot_a", "oc_1", "omt_1");
        assert_eq!(t1, topic_channel_uuid("bot_a", "oc_1", "omt_1"), "确定性");
        assert_ne!(t1, root, "话题频道必须与群根频道互异");
        assert_ne!(
            t1,
            topic_channel_uuid("bot_a", "oc_1", "omt_2"),
            "同群两个话题必须互异"
        );
        assert_ne!(
            t1,
            topic_channel_uuid("bot_b", "oc_1", "omt_1"),
            "两 bot 同群同话题必须互异（不串线）"
        );
        let parts: Vec<&str> = t1.split('-').collect();
        assert_eq!(
            [
                parts[0].len(),
                parts[1].len(),
                parts[2].len(),
                parts[3].len(),
                parts[4].len()
            ],
            [8, 4, 4, 4, 12],
            "uuid 形态与群根一致"
        );
    }

    /// #206 测试夹具：登记了群根频道的 RelayState（话题测试共用）。
    async fn topic_fixture_state(
        db_name: &str,
    ) -> (
        std::path::PathBuf,
        Arc<RelayState>,
        tokio::sync::mpsc::UnboundedReceiver<AgentReply>,
    ) {
        let db_path = test_db(db_name);
        let store = EventStore::open(&db_path).await.unwrap();
        let (state, rx) = RelayState::new(
            store,
            Keys::generate(),
            Keys::generate().public_key().to_hex(),
        );
        state.set_channels([Channel {
            uuid: channel_uuid("bot_a", "oc_a"),
            chat_id: "oc_a".into(),
            name: "角色A".into(),
            about: String::new(),
            bot_key: "bot_a".into(),
            thread_id: String::new(),
            anchor_mid: String::new(),
        }]);
        (db_path, state, rx)
    }

    /// ensure_topic_channel：登记 + 种子 39002(#p=agent,#d=uuid)/39000(name=
    /// 「角色·话题」) + 44100 成员通知（桥签名，#h=uuid,#p=agent）**入库**；
    /// 44100 可被 REQ（kinds=[44100], #p=agent, since）回放（acp 重连恢复订阅的
    /// 关键路径）且实时 fan-out 到达订阅连接。**幂等**：重复调用不重复种子/通知。
    #[tokio::test]
    async fn ensure_topic_channel_registers_seeds_and_notifies_once() {
        let (db_path, state, _rx) = topic_fixture_state("abb-buzzrelay-topic").await;
        let agent_pk = state.agent_pubkey.clone();
        let mut sub = state.test_attach_subscriber_with("{}"); // 全量订阅收 fan-out

        let tuuid = state
            .ensure_topic_channel("bot_a", "oc_a", "omt_1", "角色A", "m1")
            .await;
        assert_eq!(tuuid, topic_channel_uuid("bot_a", "oc_a", "omt_1"));
        // 登记表：thread_id/anchor_mid/bot_key 落位
        let ch = state.channel_by_uuid(&tuuid).expect("话题频道必须登记");
        assert_eq!(ch.chat_id, "oc_a");
        assert_eq!(ch.thread_id, "omt_1");
        assert_eq!(ch.anchor_mid, "m1");
        assert_eq!(ch.bot_key, "bot_a");
        assert_eq!(ch.name, "角色A·话题");

        // 种子 39002（#p=agent, #d=uuid）+ 39000（name=「角色·话题」）入库
        let f39002: Filter = serde_json::from_str(r#"{"kinds":[39002]}"#).unwrap();
        let members = state.query(&[f39002]).await;
        assert_eq!(members.len(), 1, "话题频道成员事件恰好一条");
        assert_eq!(members[0].kind.as_u16(), 39002);
        assert!(
            members[0]
                .tags
                .iter()
                .any(|t| t.as_slice().first().is_some_and(|k| k.as_str() == "d")
                    && t.as_slice().get(1).is_some_and(|v| v.as_str() == tuuid)),
            "39002 必须带 #d=话题 uuid"
        );
        let f39000: Filter = serde_json::from_str(r#"{"kinds":[39000]}"#).unwrap();
        let metas = state.query(&[f39000]).await;
        assert_eq!(metas.len(), 1, "话题频道元数据恰好一条");
        assert!(
            metas[0].tags.iter().any(|t| t
                .as_slice()
                .first()
                .is_some_and(|k| k.as_str() == "name")
                && t.as_slice()
                    .get(1)
                    .is_some_and(|v| v.as_str() == "角色A·话题")),
            "39000 必须带 name=「角色·话题」"
        );

        // 44100：桥签名 + #h=话题 uuid + #p=agent + 入库 + fan-out 到达
        let f44100: Filter =
            serde_json::from_str(&format!(r##"{{"kinds":[44100],"#p":["{agent_pk}"]}}"##)).unwrap();
        let notifs = state.query(std::slice::from_ref(&f44100)).await;
        assert_eq!(notifs.len(), 1, "44100 成员通知必须入库（REQ 回放路径）");
        assert_eq!(h_tag_of(&notifs[0]).as_deref(), Some(tuuid.as_str()));
        assert_eq!(
            notifs[0].pubkey.to_hex(),
            state.bridge_pubkey(),
            "44100 必须桥身份签名（relay-signed）"
        );
        // 实时 fan-out：全量订阅连接按序收到 44100（fan-out 帧含 kind:44100）
        let frame = sub.try_recv().expect("44100 必须实时 fan-out 到订阅连接");
        assert!(
            frame.contains("\"kind\":44100"),
            "fan-out 帧必须是 44100: {frame}"
        );
        // REQ since 回放语义：since=0 的查询（acp 重连回放形态）能捞到 44100
        let replay: Filter = serde_json::from_str(&format!(
            r##"{{"kinds":[44100],"#p":["{agent_pk}"],"since":0}}"##
        ))
        .unwrap();
        assert_eq!(state.query(&[replay]).await.len(), 1, "since 回放必须命中");

        // 幂等：重复 ensure 不重复种子/通知，登记表不变
        let tuuid2 = state
            .ensure_topic_channel("bot_a", "oc_a", "omt_1", "角色A", "m2")
            .await;
        assert_eq!(tuuid2, tuuid);
        let f39000b: Filter = serde_json::from_str(r#"{"kinds":[39000]}"#).unwrap();
        assert_eq!(
            state.query(&[f39000b]).await.len(),
            1,
            "重复 ensure 不得重复种子"
        );
        assert_eq!(
            state.query(std::slice::from_ref(&f44100)).await.len(),
            1,
            "重复 ensure 不得重发 44100"
        );
        assert!(sub.try_recv().is_err(), "重复 ensure 不得再 fan-out 44100");
        // 幂等路径不改锚点（锚点只随 publish 更新）
        assert_eq!(state.channel_by_uuid(&tuuid).unwrap().anchor_mid, "m1");
        drop(state);
        drop(_rx);
        drop(sub);
        remove_test_db(&db_path);
    }

    /// 安全钉扎（relay-signed only，与上游 ingest.rs:2187 对齐）：44100/44101
    /// **不在 admit 白名单**——任何外部身份（含桥身份）经 ingest（WS EVENT /
    /// HTTP /events 同路径）提交一律拒收且不入库。成员通知只经
    /// publish_membership_notification 内部发布。
    #[tokio::test]
    async fn admit_rejects_membership_kinds_from_any_author() {
        let db = test_db("abb-buzzrelay-44100");
        let store = EventStore::open(&db).await.unwrap();
        let bridge = Keys::generate();
        let agent = Keys::generate();
        let attacker = Keys::generate();
        let (state, _rx) = RelayState::new(store, bridge.clone(), agent.public_key().to_hex());
        let tuuid = topic_channel_uuid("bot_a", "oc_a", "omt_1");
        let mk = |keys: &Keys, kind: u16| {
            EventBuilder::new(Kind::Custom(kind), "")
                .tag(Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                    [tuuid.as_str()],
                ))
                .tag(Tag::public_key(agent.public_key()))
                .sign_with_keys(keys)
                .unwrap()
        };

        // 桥身份经 ingest 提交 44100 → 仍拒（内部发布不入 admit）
        let ack = state.ingest(&mk(&bridge, 44100)).await;
        assert!(
            ack.contains("false"),
            "桥身份经 ingest 的 44100 必须拒收: {ack}"
        );
        // 陌生人 44100 → 拒
        let ack = state.ingest(&mk(&attacker, 44100)).await;
        assert!(ack.contains("false"), "陌生人 44100 必须拒收: {ack}");
        // 44101（移除，预留）同门关死
        let ack = state.ingest(&mk(&bridge, 44101)).await;
        assert!(ack.contains("false"), "44101 同样不得经 ingest 摄取: {ack}");
        let f: Filter = serde_json::from_str(r#"{"kinds":[44100,44101]}"#).unwrap();
        assert!(
            state.query(&[f]).await.is_empty(),
            "被拒的成员通知不得入库（否则 REQ 回放会把伪造成员关系喂给 acp）"
        );
        drop(state);
        drop(_rx);
        remove_test_db(&db);
    }

    /// 话题发布：#h = 话题频道 uuid（不是群根）；锚点随发布更新为当前 mid；
    /// fan-out 顺序 = 44100（ensure）先于 kind-9 用户消息（acp 先收成员通知
    /// 即时订阅，再收消息）。话题频道未 ensure → 拒发（不静默落群根）。
    #[tokio::test]
    async fn publish_to_topic_channel_uses_topic_uuid_and_updates_anchor() {
        let (db_path, state, _rx) = topic_fixture_state("abb-buzzrelay-topicpub").await;
        let mut sub = state.test_attach_subscriber_with("{}");
        let tuuid = topic_channel_uuid("bot_a", "oc_a", "omt_1");

        // 未 ensure → None 且不入库（话题频道不存在时不许落到群根频道）
        assert!(state
            .publish_user_message("bot_a", "oc_a", "m0", "话题消息", "omt_1")
            .await
            .is_none());
        let nine: Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        assert!(state.query(std::slice::from_ref(&nine)).await.is_empty());

        // ensure（44100 fan-out 先到）→ publish（kind-9 随后）
        state
            .ensure_topic_channel("bot_a", "oc_a", "omt_1", "角色A", "m1")
            .await;
        let f44100: Filter = serde_json::from_str(r#"{"kinds":[44100]}"#).unwrap();
        let n44100 = state.query(std::slice::from_ref(&f44100)).await.len();
        let id1 = state
            .publish_user_message("bot_a", "oc_a", "m1", "话题消息一", "omt_1")
            .await
            .expect("话题频道已 ensure 必须可发布");
        let evs = state.query(std::slice::from_ref(&nine)).await;
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].id.to_hex(), id1);
        assert_eq!(
            h_tag_of(&evs[0]).as_deref(),
            Some(tuuid.as_str()),
            "话题消息必须发到话题频道（#h=话题 uuid，不是群根）"
        );
        assert_eq!(state.channel_by_uuid(&tuuid).unwrap().anchor_mid, "m1");
        // 锚点随发布更新
        state
            .publish_user_message("bot_a", "oc_a", "m2", "话题消息二", "omt_1")
            .await
            .unwrap();
        assert_eq!(
            state.channel_by_uuid(&tuuid).unwrap().anchor_mid,
            "m2",
            "锚点必须随发布更新为最新话题用户 mid"
        );
        // 重复发布不重复 44100
        assert_eq!(
            state.query(std::slice::from_ref(&f44100)).await.len(),
            n44100
        );

        // fan-out 顺序：44100 在两条 kind-9 之前（同一订阅连接按序到达）
        let f1 = sub.try_recv().expect("44100 帧");
        let f2 = sub.try_recv().expect("用户消息帧 1");
        let f3 = sub.try_recv().expect("用户消息帧 2");
        assert!(f1.contains("\"kind\":44100"), "首帧必须是成员通知: {f1}");
        assert!(f2.contains("\"kind\":9") && f2.contains("话题消息一"));
        assert!(f3.contains("\"kind\":9") && f3.contains("话题消息二"));
        drop(state);
        drop(_rx);
        drop(sub);
        remove_test_db(&db_path);
    }

    /// 话题回复回流抽取：agent 签名的 kind-9（#h=话题 uuid）→ AgentReply 带出
    /// chat_id + thread_id + anchor_mid（bridge 据此 send_thread_reply）；
    /// 陌生人伪造同 #h 回复仍拒（作者门不松）。
    #[tokio::test]
    async fn ingest_topic_reply_carries_thread_fields() {
        let db = test_db("abb-buzzrelay-topicingest");
        let store = EventStore::open(&db).await.unwrap();
        let agent = Keys::generate();
        let stranger = Keys::generate();
        let (state, mut rx) = RelayState::new(store, Keys::generate(), agent.public_key().to_hex());
        state.set_channels([Channel {
            uuid: channel_uuid("bot_a", "oc_a"),
            chat_id: "oc_a".into(),
            name: "角色A".into(),
            about: String::new(),
            bot_key: "bot_a".into(),
            thread_id: String::new(),
            anchor_mid: String::new(),
        }]);
        let tuuid = state
            .ensure_topic_channel("bot_a", "oc_a", "omt_1", "角色A", "m1")
            .await;
        let kind9 = |keys: &Keys, text: &str| {
            EventBuilder::new(Kind::Custom(9), text)
                .tag(Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                    [tuuid.as_str()],
                ))
                .sign_with_keys(keys)
                .unwrap()
        };

        // 陌生人伪造话题回复 → 拒（准入层作者门），不回流
        state.ingest(&kind9(&stranger, "伪造话题回复")).await;
        assert!(rx.try_recv().is_err(), "陌生人伪造话题回复不得回流");

        // agent 话题回复 → AgentReply 带 thread_id/anchor_mid
        state.ingest(&kind9(&agent, "话题里的回复")).await;
        let got = rx.try_recv().expect("agent 话题回复应回流");
        assert_eq!(got.chat_id, "oc_a");
        assert_eq!(got.channel_uuid, tuuid);
        assert_eq!(got.thread_id, "omt_1", "话题回复必须带 thread_id");
        assert_eq!(got.anchor_mid, "m1", "话题回复必须带锚点 mid");
        assert_eq!(got.content, "话题里的回复");
        drop(state);
        drop(rx);
        remove_test_db(&db);
    }
}

// ══════════ 事件存储（turso）与 relay 服务 ══════════

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 一个频道（= ABB 登记的虚拟 Bot 群；#206 起话题 = 群下的独立频道）。
#[derive(Debug, Clone)]
pub struct Channel {
    /// 频道 uuid（REQ #h 与回复事件 #h 都用它）
    pub uuid: String,
    /// 对应的 ABB chat_id（回流路由用）
    pub chat_id: String,
    /// 频道名（= 角色名；话题频道 = 「角色·话题」）
    pub name: String,
    /// 频道描述（= 角色提示词）
    pub about: String,
    /// 归属 bot（话题频道是运行期动态登记的——service 回流路由的启动快照
    /// uuid→bot_key 表覆盖不到它们，按本字段回落解析归属 Bridge）。
    pub bot_key: String,
    /// #206：话题 id（飞书 omt_ 开头）；空 = 群根频道。话题频道由
    /// [`RelayState::ensure_topic_channel`] 在首条话题消息时登记。
    pub thread_id: String,
    /// #206：话题锚点 mid（最近一条话题用户消息的 mid）——回流回复经
    /// send_thread_reply 落在该消息所在话题（飞书 reply_in_thread）。内存态：
    /// 重启后首条话题消息以当前 mid 重锚（见 ensure_topic_channel 注释）。
    pub anchor_mid: String,
}

/// 回流事件：虚拟 Bot agent 的回复（kind 9），bridge 据此发回聊天平台。
#[derive(Debug, Clone)]
pub struct AgentReply {
    /// 频道 uuid（回流路由键——含 bot 归属，两 bot 同群不串线）
    pub channel_uuid: String,
    pub chat_id: String,
    pub content: String,
    /// #206：回复事件 id（hex）。回复侧记账的去重键（重连重发同 id 不重复投递），
    /// 也作历史助手轮的 mid——一轮多条回复各自落一条（(mid,user) 去重不吞第二条）。
    pub event_id: String,
    /// #206：首个 e-tag 值 = 被回复的用户事件 id（NIP-10；agent 未带 --reply-to 时
    /// 为 None → 账本按 chat 兜底关联，见 bridge::buzzreply）。
    pub in_reply_to: Option<String>,
    /// 事件 created_at（unix 秒；对账排序/诊断用）。
    pub created_at: u64,
    /// #206 话题隔离：来源频道的话题 id；空 = 群根频道回复（发送走 send_text 原路径）。
    /// 非空 → bridge 走 send_thread_reply 落回原话题。
    pub thread_id: String,
    /// #206：话题锚点 mid（抽取自频道登记表，见 Channel::anchor_mid）——
    /// send_thread_reply 的 reply 目标。空（异常态：话题频道无锚点）时 bridge
    /// 如实回落 send_text + 日志。
    pub anchor_mid: String,
}

/// #206：上游 buzz-acp 的**全部** owner 控制命令字面量（lib.rs:2756-2848
/// @ c3132c3：!shutdown / !cancel / !rotate）。聊天面只开放 `!cancel`
///（见 [`ControlCommand`]），但入口闸必须挡**全集**：ABB 发布的用户消息由
/// 桥身份（= acp owner 门，BUZZ_ACP_AGENT_OWNER=桥公钥）签名——原文 trim 后
/// 命中任一即被 acp 当 owner 命令执行（!shutdown 终态杀 acp 不复活、!rotate
/// 静默作废频道会话），见 [`RelayState::publish_user_message`] 的入口闸。
pub(crate) const CONTROL_COMMAND_LITERALS: [&str; 3] = ["!cancel", "!shutdown", "!rotate"];

/// 文本 trim 后是否精确命中 owner 控制命令字面量（与上游判据同形：
/// `event.content.trim() == command`，大小写敏感，lib.rs:3558）。
pub(crate) fn is_control_command_text(text: &str) -> bool {
    let t = text.trim();
    CONTROL_COMMAND_LITERALS.contains(&t)
}

/// #206 话题隔离：频道成员通知 kind（上游 buzz-relay 同值；44101=移除留给后续
/// channel-refresh 项）。**relay-signed only 纪律**：本仓库只经
/// [`RelayState::publish_membership_notification`] 内部发布（桥签名 + 入库 +
/// fan-out，绕开 ingest/admit）；admit 白名单对 44100/44101 保持关闭——任何外部
/// 身份（含桥身份）经 WS/HTTP 提交一律拒收，与上游 ingest.rs:2187 对齐。
/// 内部发布已满足全部需求（acp 实时靠 fan-out、重连靠 REQ since 回放——故必须
/// 入库），开口 admit 只会把「任意本地进程注入频道成员」的攻击面放到协议层。
pub(crate) const KIND_MEMBERSHIP_ADD: u16 = 44100;

/// #206：可发布到频道的 owner 控制命令——**白名单是结构性的**（枚举闭集，不接受
/// 任意 content 字符串）。上游同机制的 `!shutdown`（终态杀 acp，I5 不复活）与
/// `!rotate`（会话轮换）绝不加入本枚举暴露给聊天面（审查风险：所有 IM 用户的
/// /cancel 都以桥身份=owner 发出，命令面必须锁死最小集）；同时
/// [`publish_user_message`](RelayState::publish_user_message) 入口闸拒发任何
/// 原文命中 [`CONTROL_COMMAND_LITERALS`] 的用户消息（旁路封堵，审查 #212 P1-1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    /// 叫停本频道当前在跑的轮次（buzz-acp 在 queue.push 前拦截：kind 9 +
    /// content.trim()=="!cancel" + #p mentions agent + author==owner，
    /// lib.rs:2788-2812 / is_owner_control_command lib.rs:3552-3562 @ c3132c3）。
    Cancel,
}

impl ControlCommand {
    /// 线上协议字面量：上游 `event.content.trim() == command` 精确比对
    ///（lib.rs:3558）——不得带任何前后缀/上下文，否则静默失效（外部进程协议，
    /// ABB 测试钉不住对端行为，只能钉住自己发出的字面量）。与
    /// [`CONTROL_COMMAND_LITERALS`] 单源，防两处漂移。
    pub fn literal(self) -> &'static str {
        match self {
            ControlCommand::Cancel => CONTROL_COMMAND_LITERALS[0],
        }
    }
}

/// `EventStore::store3` 的三态结果（见其文档：Duplicate ≠ 失败）。
#[derive(Debug, PartialEq, Eq)]
pub enum Store3 {
    Stored,
    Duplicate,
    Failed(String),
}

/// 事件存储（turso：buzz-relay.db）。`h_tag` 单列直查，不依赖 json_each 扩展。
pub struct EventStore {
    conn: turso::Connection,
}

impl EventStore {
    /// 打开/建表。
    pub async fn open(path: &Path) -> turso::Result<Self> {
        let db = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 id TEXT PRIMARY KEY,
                 pubkey TEXT NOT NULL,
                 kind INTEGER NOT NULL,
                 created_at INTEGER NOT NULL,
                 h_tag TEXT,
                 content TEXT NOT NULL,
                 payload_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_events_kind_h ON events(kind, h_tag, created_at);",
        )
        .await?;
        // 对话内容工件对齐仓库口径（history/msgstore 0600）：库里是全量 prompt 与
        // agent 回复，不得世界可读。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Self { conn })
    }

    /// 从事件提取入库行参数。
    fn row_values(e: &nostr::Event) -> Vec<turso::Value> {
        let h = h_tag_of(e);
        vec![
            turso::Value::Text(e.id.to_hex()),
            turso::Value::Text(e.pubkey.to_hex()),
            turso::Value::Integer(e.kind.as_u16() as i64),
            turso::Value::Integer(e.created_at.as_secs() as i64),
            h.map(turso::Value::Text).unwrap_or(turso::Value::Null),
            turso::Value::Text(e.content.clone()),
            turso::Value::Text(e.as_json()),
        ]
    }

    /// 入库（id 去重）。**三种结果必须分开**（审查 #205r4）：Duplicate 是 NIP-01
    /// 幂等语义下的正常结果（崩溃重放同一 mid 会命中），把它当失败会让用户收到
    /// 「写入失败请重发」而消息其实就在库里等回复；真失败（磁盘/权限）才要报错。
    pub async fn store3(&self, e: &nostr::Event) -> Store3 {
        match self.store_raw(e).await {
            Ok(n) if n > 0 => Store3::Stored,
            Ok(_) => Store3::Duplicate,
            Err(e) => Store3::Failed(e.to_string()),
        }
    }

    /// 入库（id 去重；NIP 语义同 id 重复提交为 no-op）。返回是否新写入。
    async fn store_raw(&self, e: &nostr::Event) -> Result<u64, turso::Error> {
        let values = Self::row_values(e);
        self.conn
            .execute(
                "INSERT OR IGNORE INTO events
                     (id, pubkey, kind, created_at, h_tag, content, payload_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                turso::params_from_iter(values),
            )
            .await
    }

    /// 按 (kind 集合, pubkey) 删除事件。用于种子事件的「先清后写」：种子 id 由内容
    /// 哈希决定（created_at 固定），群改名/取消登记后旧 39000/39002 行会永存，而
    /// buzz-acp 的 merge_discovered_channels 对同 #d 多行无 created_at 决胜
    /// （都=1）→ 旧名可能胜出。清后重发使库状态恒等于当前登记表（审查 #205r2）。
    pub async fn delete_where(&self, kinds: &[u16], pubkey: &str) -> Result<u64, String> {
        if kinds.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; kinds.len()].join(",");
        let sql = format!("DELETE FROM events WHERE kind IN ({placeholders}) AND pubkey = ?");
        let mut params: Vec<turso::Value> = kinds
            .iter()
            .map(|k| turso::Value::Integer(*k as i64))
            .collect();
        params.push(turso::Value::Text(pubkey.to_string()));
        self.conn
            .execute(&sql, turso::params_from_iter(params))
            .await
            .map_err(|e| format!("DELETE kinds={kinds:?} pubkey={pubkey}: {e}"))
    }

    /// 按 filter 集合查事件（NIP-01 多 filter OR 语义；h_tag/kind 走索引，
    /// 其余条件内存二次过滤）。
    pub async fn query(&self, filters: &[Filter]) -> Vec<nostr::Event> {
        let mut out: Vec<nostr::Event> = Vec::new();
        for f in filters {
            let kinds: Vec<u16> = f
                .kinds
                .as_ref()
                .map(|ks| ks.iter().map(|k| k.as_u16()).collect())
                .unwrap_or_default();
            let since = f.since.as_ref().map(|t| t.as_secs());
            let until = f.until.as_ref().map(|t| t.as_secs());
            // 单 filter 的 kind 集合折叠为 IN 列表（多 kind 时用区间界定 + 内存精确过滤）
            let (kmin, kmax) = if kinds.is_empty() {
                (0i64, i64::MAX)
            } else {
                (
                    *kinds.iter().min().unwrap() as i64,
                    *kinds.iter().max().unwrap() as i64,
                )
            };
            let h_filter = f.generic_tags.get(&SingleLetterTag::lowercase(Alphabet::H));
            let mut sql =
                String::from("SELECT payload_json FROM events WHERE kind >= ? AND kind <= ?");
            if let Some(hv) = h_filter {
                sql.push_str(&format!(
                    " AND h_tag IN ({})",
                    hv.iter().map(|_| "?").collect::<Vec<_>>().join(",")
                ));
            }
            if let Some(s) = since {
                sql.push_str(&format!(" AND created_at >= {s}"));
            }
            if let Some(u) = until {
                sql.push_str(&format!(" AND created_at <= {u}"));
            }
            let mut params: Vec<turso::Value> =
                vec![turso::Value::Integer(kmin), turso::Value::Integer(kmax)];
            if let Some(hv) = h_filter {
                for v in hv {
                    params.push(turso::Value::Text(v.clone()));
                }
            }
            let mut rows = match self.conn.query(&sql, turso::params_from_iter(params)).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(payload) = row.get_value(0).and_then(|v| match v {
                    turso::Value::Text(s) => Ok(s),
                    _ => Err(turso::Error::Misuse("payload col not text".into())),
                }) {
                    if let Ok(ev) = serde_json::from_str::<nostr::Event>(&payload) {
                        if filter_matches(f, &ev) && !out.iter().any(|o| o.id == ev.id) {
                            out.push(ev);
                        }
                    }
                }
            }
        }
        out
    }
}

/// mini-relay 共享状态：事件库 + 频道表 + 订阅/连接注册 + 回流通道。
pub struct RelayState {
    db: EventStore,
    /// 频道 uuid → 频道信息
    channels: std::sync::RwLock<HashMap<String, Channel>>,
    /// WS 连接出站帧（按连接 id）
    conns: Mutex<HashMap<u64, tokio::sync::mpsc::UnboundedSender<String>>>,
    /// 连接 → 订阅（sub_id → filter 集）
    subs: Mutex<HashMap<u64, HashMap<String, Vec<Filter>>>>,
    conn_seq: AtomicU64,
    /// 上次「无订阅者」告警的 unix 秒（节流用，60s 一条）
    last_no_sub_warn: AtomicU64,
    /// ABB 桥身份密钥（签名种子事件与正向喂入的消息事件）
    bridge_keys: Keys,
    /// agent 身份公钥（hex）——回流事件按它识别
    agent_pubkey: String,
    /// 同上的解析结果（构造时一次）；None = 身份非法/空（I1：消息无法「发给
    /// agent」，publish 必须如实失败而非发出一条没人收的事件）
    agent_pk: Option<nostr::PublicKey>,
    /// kind 9 回流（频道 uuid → 文本）
    pub reply_tx: tokio::sync::mpsc::UnboundedSender<AgentReply>,
}

impl RelayState {
    /// 组装。reply_rx 由 bridge 消费（发回聊天平台）。
    pub fn new(
        db: EventStore,
        bridge_keys: Keys,
        agent_pubkey: String,
    ) -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<AgentReply>) {
        let (reply_tx, reply_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Self {
                db,
                channels: std::sync::RwLock::new(HashMap::new()),
                conns: Mutex::new(HashMap::new()),
                subs: Mutex::new(HashMap::new()),
                agent_pk: nostr::PublicKey::from_hex(&agent_pubkey).ok(),
                conn_seq: AtomicU64::new(1),
                last_no_sub_warn: AtomicU64::new(0),
                bridge_keys: bridge_keys.clone(),
                agent_pubkey,
                reply_tx,
            }),
            reply_rx,
        )
    }

    /// 登记表驱动：同步频道集合（新增/更新元数据；不删除——下线频道由登记侧清理）。
    pub fn set_channels(&self, channels: impl IntoIterator<Item = Channel>) {
        let mut map = self.channels.write().unwrap();
        for ch in channels {
            map.insert(ch.uuid.clone(), ch);
        }
    }

    /// 按 uuid 取频道（回流路由用）。
    pub fn channel_by_uuid(&self, uuid: &str) -> Option<Channel> {
        self.channels.read().unwrap().get(uuid).cloned()
    }

    /// 查询已入库事件（仅测试断言用；与 EventStore::query 同义）。
    #[cfg(test)]
    pub async fn query(&self, filters: &[Filter]) -> Vec<nostr::Event> {
        self.db.query(filters).await
    }

    /// 桥身份公钥（hex）——用户消息事件的签名者，buzz-acp owner 门的合法作者。
    pub fn bridge_pubkey(&self) -> String {
        self.bridge_keys.public_key().to_hex()
    }

    /// #200 胶水：bridge 调用——把用户消息签成 kind-9 事件注入指定频道。
    /// `thread_id` 空 = 群根频道（uuid 由 (bot_key, chat_id) 确定性派生——不扫表
    /// 按 chat_id 匹配，两 bot 同群时会有歧义）；非空 = 话题频道（#206 话题隔离，
    /// uuid 见 [`topic_channel_uuid`]；**调用方须先 [`RelayState::ensure_topic_channel`]**
    /// ——话题频道缺失如实失败，不静默落到群根）。返回 None = 未送达（无频道/签名
    /// 失败/未入库），调用方负责给用户可见反馈（不做静默黑洞）。成功（含 Duplicate
    /// 幂等重放）返回 Some(事件 id hex)——#206 回复侧记账以它登记 awaiting，回复事件
    /// 的 e-tag 按它反查。mid 进 tag：Nostr 事件 id 是内容哈希，「同秒同文」两条消息
    /// 不带 mid 会撞 id——第二条被 INSERT OR IGNORE 静默吞。
    /// 话题发布成功后锚点随发布更新（Channel.anchor_mid = 当前 mid）——回流回复
    /// 以最新话题用户消息为 reply 目标（同话题内的近似：迟到回复锚到更新消息的
    /// 话题同侧，飞书 reply_in_thread 仍落原话题）。
    pub async fn publish_user_message(
        &self,
        bot_key: &str,
        chat_id: &str,
        mid: &str,
        content: &str,
        thread_id: &str,
    ) -> Option<String> {
        // P1-1 安全闸（审查 #212，#206 回复侧记账 rebase 移植：签名 bool→Option，
        // 拒发由 return false 改为 return None）：本函数以**桥身份**（= acp 的
        // owner 门，BUZZ_ACP_AGENT_OWNER=桥公钥）签名用户消息——原文 trim 后精确
        // 命中 owner 控制命令字面量（!cancel/!shutdown/!rotate，buzz-acp
        // lib.rs:2756-2848 @ c3132c3）会被 acp 当 owner 命令执行：!shutdown 终态
        // 杀 acp 不复活（I5）、!rotate 静默作废频道会话、!cancel 旁路 /cancel
        // 专用路径。此前只靠 prompt 组装的副作用（角色块/历史注入改变最终
        // content）侥幸挡住，不是设计边界。本闸是权威边界（任何调用方都绕不过）；
        // bridge dispatch 在更上层拦同一判据以给用户看得懂的文案。
        if is_control_command_text(content) {
            crate::log!(
                "[mini-relay] ⚠️ 用户消息命中 owner 控制命令字面量，拒发（防桥身份提权） chat={} mid={}",
                crate::agent::truncate(chat_id, 16),
                crate::agent::truncate(mid, 16)
            );
            return None;
        }
        let uuid = if thread_id.is_empty() {
            channel_uuid(bot_key, chat_id)
        } else {
            topic_channel_uuid(bot_key, chat_id, thread_id)
        };
        if self.channel_by_uuid(&uuid).is_none() {
            // 非虚拟 Bot 群（或登记晚于 relay 启动的频道集快照；话题频道 = 调用方
            // 未先 ensure_topic_channel），不经 relay；留日志防静默丢消息。
            crate::log!(
                "[mini-relay] ⚠️ chat 无对应频道（未登记/登记晚于启动/话题未 ensure），消息不入 relay chat={} thread={}",
                crate::agent::truncate(chat_id, 16),
                crate::agent::truncate(thread_id, 16)
            );
            return None;
        }
        // I1 口径：没有合法 agent 身份 = 消息「发不出去给谁」——如实失败，
        // 不发一条注定无人订阅的事件（审查 #205r5：测试夹具的空串暴露的正是
        // 这个分支，生产里 init 必然生成真钥）。
        let Some(agent_pk) = self.agent_pk else {
            crate::log!("[mini-relay] ⚠️ agent 身份未配置（pubkey 为空/非法），消息无法定址，拒发");
            return None;
        };
        // tag 三件套（缺一不可，审查 #205r5）：
        // - #h：频道（buzz-acp extract_h_tag_uuid / REQ #h）
        // - #p：**agent 公钥**——acp 默认 subscribe=mentions，订阅 filter 为
        //   {"kinds":[9,…],"#h":[uuid],"#p":[agent]}；消息不带 #p 就永远匹配不上
        //   订阅（fan-out 与 REQ 回放都进不去，静默无回复）。语义也正确：消息是
        //   「发给这个 agent」的。
        // - abb-mid：保事件 id 唯一（同秒同文不撞 hash）。
        let ev = EventBuilder::new(Kind::Custom(9), content)
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                [uuid.as_str()],
            ))
            .tag(Tag::public_key(agent_pk))
            .tag(Tag::custom(TagKind::custom("abb-mid"), [mid]))
            .sign_with_keys(&self.bridge_keys)
            .ok();
        let Some(e) = ev else {
            crate::log!("[mini-relay] ⚠️ 用户消息事件签名失败（桥身份密钥异常），丢弃");
            return None;
        };
        let event_id = e.id.to_hex();
        match self.db.store3(&e).await {
            // Duplicate：同一 mid 的幂等重放（崩溃恢复路径），事件已在库里等被应答
            // ——算送达。把它当失败会让用户收到「写入失败请重发」的假错（#205r4）。
            // 事件 id 是内容哈希，Duplicate 的 id 与库内一致，照常返回供记账。
            Store3::Stored | Store3::Duplicate => {}
            Store3::Failed(err) => {
                crate::log!(
                    "[mini-relay] ⚠️ 消息事件入库失败 chat={} mid={}: {err}",
                    crate::agent::truncate(chat_id, 16),
                    crate::agent::truncate(mid, 16)
                );
                return None;
            }
        }
        // 话题频道：锚点随发布更新为当前用户 mid（回流回复的 reply 目标；
        // 群根频道无锚点概念）。
        if !thread_id.is_empty() {
            if let Some(ch) = self.channels.write().unwrap().get_mut(&uuid) {
                ch.anchor_mid = mid.to_string();
            }
        }
        self.fan_out(&e);
        // 入库 ≠ 有人消费。**能否补发取决于对端的订阅水位**（buzz-acp 的
        // subscribe_since/startup_watermark 在其进程内存里，ABB 不掌握也不该声称
        // 「不丢」——#205r4 更正上一版注释），所以这里只保证运维看得见（60s 一条）。
        if self.subscriber_count() == 0 {
            let now = nostr::prelude::Timestamp::now().as_secs();
            if self.should_warn_no_subscriber(now) {
                crate::log!(
                    "[mini-relay] ⚠️ 已入库但当前无 WS 订阅者（buzz-acp 未连？回复会等它重连背充）"
                );
            }
        }
        Some(event_id)
    }

    /// #206 对账查询：按用户事件 id 反查该轮 **agent 身份**的 kind-9 回复事件
    /// （e-tag 命中）。e-tag 无索引、不加表结构迁移（风险⑥）：kind 走
    /// idx_events_kind_h 索引，作者/e-tag 用 payload 内存过滤。`since_secs` 下推
    /// SQL（回复不可能早于其用户消息诞生——审查 P2：无下推时启动对账 = awaiting
    /// 条数 × 全量 kind-9 扫描，事件表随 relay 寿命无界增长）。返回的 AgentReply
    /// 与实时回流同构（含 event_id —— 去重键）；#h 不在频道集的事件跳过（与
    /// ingest 的回流门同口径）。
    pub async fn find_agent_replies_to(
        &self,
        user_event_id: &str,
        since_secs: u64,
    ) -> Vec<AgentReply> {
        let f = Filter::new()
            .kind(Kind::Custom(9))
            .since(nostr::Timestamp::from(since_secs));
        self.db
            .query(&[f])
            .await
            .into_iter()
            .filter(|e| e.pubkey.to_hex() == self.agent_pubkey)
            .filter(|e| first_e_tag(e).as_deref() == Some(user_event_id))
            .filter_map(|e| self.agent_reply_from_event(&e))
            .collect()
    }

    /// kind-9 事件 → AgentReply（共享抽取：实时回流 ingest 与对账
    /// find_agent_replies_to 同一条装配线，字段口径绝不漂移）。
    /// None = #h 缺失或不在频道集（登记晚于启动快照；话题频道在 ABB 重启后、
    /// 下一条话题消息重登记前的窗口内同样不在——该窗口内到达的话题回复按此
    /// 丢弃并留日志，崩溃窗口条目由启动对账的重登记兜住，见 bridge::buzzreply）。
    /// 话题频道 → 带出 thread_id/anchor_mid（bridge 据此走 send_thread_reply）。
    fn agent_reply_from_event(&self, e: &Event) -> Option<AgentReply> {
        let uuid = h_tag_of(e)?;
        let ch = self.channel_by_uuid(&uuid)?;
        Some(AgentReply {
            channel_uuid: uuid,
            chat_id: ch.chat_id,
            content: e.content.clone(),
            event_id: e.id.to_hex(),
            in_reply_to: first_e_tag(e),
            created_at: e.created_at.as_secs(),
            thread_id: ch.thread_id,
            anchor_mid: ch.anchor_mid,
        })
    }

    /// #206 胶水：bridge /cancel 调用——把 owner 控制命令签成 kind-9 事件注入指定
    /// 频道。buzz-acp 主循环在 queue.push 前拦截（**消费即丢弃，不进 prompt 队列**，
    /// lib.rs:2794-2812），对在跑轮次 signal_in_flight_task → ControlSignal::Cancel →
    /// session/cancel notification（acp.rs:849）→ 5s 排水（pool.rs:1047
    /// CONTROL_CANCEL_GRACE）→ 丢弃触发批次 + 作废频道 session（pool.rs:2654-2678；
    /// requeue_cancelled_batch 对 Cancel return None，pool.rs:4184）。无在跑轮次时
    /// acp 仅打一条 warn no-op，**不给频道任何反馈**——送达≠叫停成功，回执话术由
    /// 桥侧负责诚实表述（只说「已发送」）。
    ///
    /// 事件形态四要素（对照上游判据，缺一不可）：
    /// - kind 9 + content 精确 "!cancel"（is_owner_control_command lib.rs:3552-3562）；
    /// - #p = agent 公钥（event_mentions_agent lib.rs:3545——订阅 filter 也含 #p，
    ///   缺了永远匹配不上 mentions 订阅）；
    /// - #h = 频道 uuid：relay 侧 REQ filter 恒带 #h（relay.rs:3266-3267）且本
    ///   relay 的 fan_out/回放都按它匹配——缺了事件根本到不了 acp。注意 acp 定位
    ///   频道在跑任务走的是 **sub_id→channel 映射**（channel_id_from_sub_id
    ///   relay.rs:2228/3573）；extract_h_tag_uuid（relay.rs:2168）是 membership
    ///   通知路径，不是本事件的定址机制（审查 #212 更正归属）。
    /// - 桥身份签名（owner 门：author==BUZZ_ACP_AGENT_OWNER=桥公钥，lib.rs:2796）。
    ///
    /// abb-mid 必须保留：!cancel 无业务 mid 可用，用随机 nonce——Nostr 事件 id 是
    /// 内容哈希，同秒两条 "!cancel" 会撞 id，第二条被 INSERT OR IGNORE 静默吞。
    /// 返回 false = 未送达（无频道/agent 身份非法/签名失败/入库失败）；Duplicate
    /// 算送达（幂等语义同 publish_user_message）。
    ///
    /// `thread_id`（#206 话题隔离）：空 = 发布到群根频道；非空 = 发布到话题频道
    /// （叫停粒度与 CLI 的 chat:thread key 对齐——话题消息自本项起 dispatch 进
    /// 独立话题频道，留在群根频道的 !cancel 停不到话题频道的在跑轮次）。
    /// 话题频道缺失（从未 dispatch 过 = 无轮次可停）→ false，调用方如实表述。
    ///
    /// 背充重放**不是无害 no-op**（审查 #212 更正），两种真实形态：
    /// ① 半开连接吞帧——fan-out 进 mpsc 成功但 acp 未收到/未 record_event；acp
    ///   重连回放窗口为 min(last_seen, dropped_since) - SINCE_SKEW_SECS(5s)
    ///   （relay.rs:59/3277），若本频道后续流量把 last_seen 推过
    ///   cancel.created_at+5s，回放永久跳过这条 !cancel——叫停丢失，但用户已收
    ///   「已发送」回执（桥无从确知，回执措辞已按此设计）。
    /// ② 快速重连——未见的 !cancel 在窗口内被回放，若此时已有**新轮次**在跑，
    ///   会被误停（批次丢弃 + 频道会话作废），不是 no-op。
    pub async fn publish_control_command(
        &self,
        bot_key: &str,
        chat_id: &str,
        thread_id: &str,
        cmd: ControlCommand,
    ) -> bool {
        let uuid = if thread_id.is_empty() {
            channel_uuid(bot_key, chat_id)
        } else {
            topic_channel_uuid(bot_key, chat_id, thread_id)
        };
        if self.channel_by_uuid(&uuid).is_none() {
            crate::log!(
                "[mini-relay] ⚠️ 控制命令 {:?} 无对应频道（未登记/登记晚于启动/话题未 ensure），不发布 chat={} thread={}",
                cmd,
                crate::agent::truncate(chat_id, 16),
                crate::agent::truncate(thread_id, 16)
            );
            return false;
        }
        // 同 publish_user_message 的 I1 口径：没有合法 agent 身份 = 命令无法定址，
        // 如实失败而非发出一条没人收的控制事件。
        let Some(agent_pk) = self.agent_pk else {
            crate::log!(
                "[mini-relay] ⚠️ agent 身份未配置（pubkey 为空/非法），控制命令无法定址，拒发"
            );
            return false;
        };
        let nonce = uuid::Uuid::new_v4().to_string();
        let ev = EventBuilder::new(Kind::Custom(9), cmd.literal())
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                [uuid.as_str()],
            ))
            .tag(Tag::public_key(agent_pk))
            .tag(Tag::custom(TagKind::custom("abb-mid"), [nonce.as_str()]))
            .sign_with_keys(&self.bridge_keys)
            .ok();
        let Some(e) = ev else {
            crate::log!("[mini-relay] ⚠️ 控制命令事件签名失败（桥身份密钥异常），丢弃");
            return false;
        };
        match self.db.store3(&e).await {
            Store3::Stored | Store3::Duplicate => {}
            Store3::Failed(err) => {
                crate::log!(
                    "[mini-relay] ⚠️ 控制命令 {:?} 入库失败 chat={}: {err}",
                    cmd,
                    crate::agent::truncate(chat_id, 16)
                );
                return false;
            }
        }
        self.fan_out(&e);
        true
    }

    /// 当前连上来的 WS 连接数（≈buzz-acp 消费者数）。dispatch 用它区分
    /// 「已入库待背充」与「无人消费」——事件存储后 acp 重连会按 since 水位回放
    /// （Phase 1 REQ 语义），所以零订阅**不等于**丢失，只是延迟；但持续零订阅
    /// = buzz-acp 没装/没起，运维必须看得见（审查 #205r2）。
    pub fn subscriber_count(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    /// 是否有连接**已 REQ 订阅了该频道**（filter 的 #h 含此 uuid，或 filter 未限定
    /// #h＝订阅全频道）。dispatch 预检用它而非 `subscriber_count`：只有连接数会把
    /// 「刚握手还没订阅」「半开黑洞」也算成消费者，于是当场放行、事后无人应答。
    pub fn has_subscription_for(&self, uuid: &str) -> bool {
        let conns = self.conns.lock().unwrap();
        let subs = self.subs.lock().unwrap();
        conns.keys().any(|cid| {
            subs.get(cid).map(|by_sub| {
                by_sub.values().any(|filters| {
                    filters.iter().any(|f| {
                        // 无 kinds 或 kinds 含 9 且 #h 命中（未给 #h＝全频道订阅）
                        let kind_ok = f
                            .kinds
                            .as_ref()
                            .map(|ks| ks.is_empty() || ks.iter().any(|k| k.as_u16() == 9))
                            .unwrap_or(true);
                        let h_ok = match f.generic_tags.iter().find(|(t, _)| t.to_string() == "h") {
                            Some((_, vals)) => vals.iter().any(|v| v == uuid),
                            None => true,
                        };
                        kind_ok && h_ok
                    })
                })
            }) == Some(true)
        })
    }

    /// 测试夹具：模拟一个**已完成 REQ 订阅的 buzz-acp 连接**（conn + 一条 kinds=[9]
    /// 全频道订阅），用于过 dispatch 预检——预检判据是真的（要真订阅），不能为了
    /// 测试放宽。返回的接收端须由调用方持有。
    #[cfg(test)]
    pub fn test_attach_subscriber(&self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        self.test_attach_subscriber_with(r#"{"kinds":[9]}"#)
    }

    /// 测试夹具：同上但订阅 filter 自定（如 `{}` 全量订阅——断言 fan-out 帧序列
    /// 用：44100 成员通知与 kind-9 用户消息的同连接到达顺序）。
    #[cfg(test)]
    pub fn test_attach_subscriber_with(
        &self,
        filter_json: &str,
    ) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let id = self.conn_seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.conns.lock().unwrap().insert(id, tx);
        let f: Filter = serde_json::from_str(filter_json).unwrap();
        self.subs
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .insert("sub-test".to_string(), vec![f]);
        rx
    }

    /// 距上次「无订阅者」告警是否已过节流窗（60s；避免每消息一行刷屏）。
    pub fn should_warn_no_subscriber(&self, now_secs: u64) -> bool {
        let prev = self.last_no_sub_warn.load(Ordering::Relaxed);
        if now_secs.saturating_sub(prev) >= 60 {
            self.last_no_sub_warn.store(now_secs, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// 种子**单个**频道的元数据/成员事件（kind 39002 成员 + 39000 元数据，桥身份
    /// 签名，created_at=1 幂等）——seed_channel_events（启动全量）与
    /// ensure_topic_channel（运行期话题登记）共用的单入口，防两份种子逻辑漂移。
    /// agent 身份非法时跳过成员事件（无法构造有效的 #p tag；39000 照发）。
    async fn seed_one_channel(&self, ch: &Channel) {
        let agent_pk = if self.agent_pubkey.is_empty() {
            None
        } else {
            nostr::PublicKey::from_hex(&self.agent_pubkey).ok()
        };
        // kind 39002：成员（#p = agent pubkey, #d = 频道 uuid）
        // P0-1 修复：agent_pubkey 无效时跳过成员事件，不再 panic
        if let Some(pk) = &agent_pk {
            let member = EventBuilder::new(Kind::Custom(39002), "")
                .tag(Tag::public_key(*pk))
                .tag(Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::D)),
                    [ch.uuid.as_str()],
                ))
                .custom_created_at(Timestamp::from_secs(1))
                .sign_with_keys(&self.bridge_keys)
                .ok();
            if let Some(ev) = member {
                let _ = self.db.store3(&ev).await;
            }
        }
        // kind 39000：频道元数据（name/about 标签供 discover_channels 读取）
        let meta = EventBuilder::new(Kind::Custom(39000), ch.about.clone())
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::D)),
                [ch.uuid.as_str()],
            ))
            .tag(Tag::custom(TagKind::custom("name"), [ch.name.as_str()]))
            .tag(Tag::custom(TagKind::custom("about"), [ch.about.as_str()]))
            .custom_created_at(Timestamp::from_secs(1))
            .sign_with_keys(&self.bridge_keys)
            .ok();
        if let Some(ev) = meta {
            let _ = self.db.store3(&ev).await;
        }
    }

    /// #206 话题隔离：确保话题频道已登记（**幂等**——已在登记表 → 直接返回
    /// uuid，不重复种子/通知）。新话题：登记 Channel（bot_key/thread_id/锚点）
    /// → 种子 39002(#p=agent,#d=uuid)/39000(name=「角色·话题」) → 内部发布
    /// 44100 成员通知（#h=uuid,#p=agent）入库并 fan-out。acp 收 44100 后
    /// Mentions 模式默认放行并即时订阅（resolve_dynamic_channel_filter
    /// config.rs:1376 @ c3132c3）；重连则靠 REQ since 回放恢复订阅（故 44100
    /// 必须入库，见 publish_membership_notification）。
    ///
    /// 重启自愈：频道登记表是内存态——重启后话题频道不在表内，下一条话题消息
    /// 经 bridge 预检再次走本函数重登记 + 重发 44100（库内旧 39000/39002 种子
    /// 按事件 id 幂等不重复，44100 每次新增一条属预期，无界增长见 PR 风险节）；
    /// 锚点 mid 同为内存态，以触发消息的 mid 重锚。
    /// `role_name`：话题频道名 = 「{role_name}·话题」（调用方取群根频道名，
    /// 与群频道同源）。`anchor_mid`：触发登记的消息 mid。
    pub async fn ensure_topic_channel(
        &self,
        bot_key: &str,
        chat_id: &str,
        thread_id: &str,
        role_name: &str,
        anchor_mid: &str,
    ) -> String {
        let uuid = topic_channel_uuid(bot_key, chat_id, thread_id);
        if self.channel_by_uuid(&uuid).is_some() {
            return uuid; // 幂等：已登记不重复种子/通知
        }
        let ch = Channel {
            uuid: uuid.clone(),
            chat_id: chat_id.to_string(),
            name: format!("{role_name}·话题"),
            // 与群根频道口径一致（service 启动登记同样 about=空——角色提示词
            // 经 buzz-acp 侧 channel→session 的群根频道元数据承载，话题频道
            // 不另造一份）
            about: String::new(),
            bot_key: bot_key.to_string(),
            thread_id: thread_id.to_string(),
            anchor_mid: anchor_mid.to_string(),
        };
        self.set_channels([ch.clone()]);
        self.seed_one_channel(&ch).await;
        if !self
            .publish_membership_notification(&uuid, KIND_MEMBERSHIP_ADD)
            .await
        {
            // agent 身份非法（同 publish_user_message 的 I1 口径）：频道已登记、
            // 消息照常入库——acp 收不到 44100 不会即时订阅，回复等下次启动种子
            // /REQ 回放，日志如实。
            crate::log!(
                "[mini-relay] ⚠️ 话题频道已登记但 44100 成员通知发布失败（agent 身份非法？）uuid={}",
                crate::agent::truncate(&uuid, 16)
            );
        }
        uuid
    }

    /// 内部发布频道成员通知（44100=加入；44101=移除预留，后续 channel-refresh
    /// 项用）：**桥身份签名 → 入库 → fan-out，绕开 ingest/admit**——admit 白名单
    /// 对 44100/44101 保持关闭（任何外部身份含桥身份经 WS/HTTP 提交一律拒收），
    /// 与上游 buzz-relay「relay-signed only」纪律对齐（上游 ingest.rs:2187 拒
    /// 外部提交）。内部发布已满足全部需求：acp 实时靠 fan-out，重连靠 REQ
    /// （kinds=[44100], #p=agent, since）回放——**故必须入库**（Store 语义）。
    /// Duplicate（同秒同参数重发撞内容哈希）算送达：事件已在库里等被回放。
    /// false = 未送达（agent 身份非法无法构造 #p / 签名失败 / 入库失败）。
    pub(crate) async fn publish_membership_notification(&self, uuid: &str, kind: u16) -> bool {
        let Some(agent_pk) = self.agent_pk else {
            crate::log!(
                "[mini-relay] ⚠️ agent 身份未配置（pubkey 为空/非法），成员通知 kind={kind} 无法定址，拒发"
            );
            return false;
        };
        let ev = EventBuilder::new(Kind::Custom(kind), "")
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                [uuid],
            ))
            .tag(Tag::public_key(agent_pk))
            .sign_with_keys(&self.bridge_keys)
            .ok();
        let Some(e) = ev else {
            crate::log!("[mini-relay] ⚠️ 成员通知事件签名失败（桥身份密钥异常），丢弃");
            return false;
        };
        match self.db.store3(&e).await {
            Store3::Stored | Store3::Duplicate => {}
            Store3::Failed(err) => {
                crate::log!(
                    "[mini-relay] ⚠️ 成员通知 kind={kind} 入库失败 uuid={}: {err}",
                    crate::agent::truncate(uuid, 16)
                );
                return false;
            }
        }
        self.fan_out(&e);
        true
    }

    /// 种子频道元数据/成员事件（kind 39002 成员 + 39000 元数据，bridge 身份签名）——
    /// buzz-acp discover_channels 两步 /query 的数据源。幂等（同 id 去重）。
    /// agent_pubkey 为空时跳过成员事件（无法构造有效的 #p tag）。
    pub async fn seed_channel_events(&self) {
        // 先清后写（见 EventStore::delete_where）：本函数只写桥身份签的 39000/39002，
        // 清同 (kinds, 桥 pubkey) 的旧行不会影响 agent 的 kind-9 回复，且让改名与
        // 取消登记的频道不再留残行。
        let bridge_pk = self.bridge_keys.public_key().to_hex();
        // 清理失败必须可见：吞掉错误的话，旧名的 39000 会继续在 discover_channels
        // 里胜出——那正是本函数要修的场景（审查 #205r3）。
        if let Err(e) = self.db.delete_where(&[39000, 39002], &bridge_pk).await {
            crate::log!("[mini-relay] ⚠️ 旧种子事件清理失败: {e}（频道资料可能残留旧名）");
        }
        // 先克隆（不持 RwLock 跨 await——RwLockReadGuard 非 Send）
        let channels: Vec<Channel> = self.channels.read().unwrap().values().cloned().collect();
        for ch in &channels {
            self.seed_one_channel(ch).await;
        }
    }

    /// 摄取准入判定（**kind 白名单 + 作者权威**，审查 #205r3 安全项）。
    ///
    /// 验签只证明「事件与其 pubkey 自洽」，任何本地进程（或 SSRF 到
    /// 127.0.0.1:port 的 POST /events）都能自签一把新密钥造出合法签名的事件——
    /// 而 buzz-acp 的 discover_channels 把 kind 39000 的 name/about 当频道资料用、
    /// **about 即角色 system prompt**，喂给带工具权限的 agent 会话。因此「签名有效」
    /// 绝不能当「可信」用。规则：
    /// - 39000/39002（频道元数据/成员）：**只认桥身份**——ABB 登记表是唯一权威，
    ///   否则任何进程都能改写角色的系统提示词。
    /// - 9（频道消息）：只认 **桥**（dispatch 的用户轮）或 **agent**（回复）；
    ///   陌生人 kind-9 = 伪造用户轮注入上下文，同样拒。
    /// - 20001/20002（presence/typing）：瞬时事件——只 fan-out 不入库（NIP-01
    ///   ephemeral 语义；入库会在 REQ 回放时把陈旧在线态当历史喂回去）。
    /// - 44100/44101（频道成员通知，#206）：**保持关闭**——成员通知是
    ///   relay-signed only（上游 ingest.rs:2187 同纪律），本仓库只经
    ///   [`RelayState::publish_membership_notification`] 内部发布（绕开
    ///   ingest/admit）；任何外部身份（含桥身份）经 WS/HTTP 提交一律拒收，
    ///   否则任意本地进程可注入频道成员把 agent 拉进攻击者频道。
    /// - 其余（含 22242 auth：只属于 WS 握手路径）：拒。
    ///
    /// 白名单的已知缺口：**kind 24200（buzz-acp 的 observer frame，它按 durable 经
    /// WS EVENT 发）** 现会被拒——ABB 未配 `relay_observer`（buzz-acp 默认 off）故
    /// 零影响，且 OK-false 不引发重投活锁。日后接 observer 视图（#206）必须把它加
    /// 进白名单，别当成「消费矩阵已核全」。
    fn admit(&self, e: &Event) -> Admit {
        let author = e.pubkey.to_hex();
        let bridge = self.bridge_keys.public_key().to_hex();
        match e.kind.as_u16() {
            39000 | 39002 if author == bridge => Admit::Store,
            9 if author == bridge || author == self.agent_pubkey => Admit::Store,
            20001 | 20002 => Admit::Ephemeral,
            k => Admit::Reject(format!("blocked: unsupported kind or author {k}")),
        }
    }

    /// 事件摄取：验签 → **准入（kind 白名单 + 作者权威）** → 入库 → fan-out →
    /// 回流抽取（kind 9 = agent 回复）。
    /// pub(crate)：bridge 的 #206 对账测试需要把 agent 回复事件经真实准入/入库/回流
    /// 管线喂进来（绕过它直插 db 会漏掉回流通道一侧的覆盖）。
    pub(crate) async fn ingest(&self, e: &Event) -> String {
        let sig_ok = e.verify_id() && e.verify_signature();
        if !sig_ok {
            return format!(
                "[\"OK\",\"{}\",false,\"invalid: signature or id\"]",
                e.id.to_hex()
            );
        }
        match self.admit(e) {
            Admit::Reject(r) => {
                // 留日志：这是「有东西在试着改频道资料/塞假用户轮」的指纹（本地进程
                // 或 SSRF），运维必须能看到，不能静默 200 OK。
                crate::log!(
                    "[mini-relay] ⚠️ 拒收事件 kind={} pubkey={:.12}…: {r}",
                    e.kind.as_u16(),
                    e.pubkey.to_hex()
                );
                return format!("[\"OK\",\"{}\",false,\"{r}\"]", e.id.to_hex());
            }
            // 瞬时事件：只转发不落库（ACK true——对端要的是送达，不是持久化）
            Admit::Ephemeral => {
                self.fan_out(e);
                return format!("[\"OK\",\"{}\",true,\"\"]", e.id.to_hex());
            }
            Admit::Store => {}
        }
        let st = self.db.store3(e).await;
        let stored = st == Store3::Stored;
        if stored {
            self.fan_out(e);
        }
        // kind 9（Buzz 频道消息）→ 回复回流。**两道闸缺一不可**（审查 #205r2）：
        // ① 作者必须是 **agent 身份公钥**——原「非 bridge 即回复」让任意本地进程
        //    （或 SSRF 到 127.0.0.1:port 的 POST /events）自签一条 kind-9 + 任意
        //    #h（uuid 从 virtual-bots.json 可推）就能**以 bot 身份向真实聊天群发
        //    任意文本**；本 PR 填上 senders 路由后才从死代码变成活通路，绝不能留。
        // ② 必须**首次入库**——WS 断连后 buzz-acp 重发同一事件（ACK 未收到）会走
        //    这里第二次，重复投递同一条回复到群里。
        if stored && e.kind.as_u16() == 9 && e.pubkey.to_hex() == self.agent_pubkey {
            match self.agent_reply_from_event(e) {
                Some(reply) => {
                    let _ = self.reply_tx.send(reply);
                }
                // 频道集是启动快照：agent 回了但 #h 不在集内 = 唯一的静默黑洞，
                // 与 dispatch 侧的日志对称（审查 #205r2）。
                None => crate::log!(
                    "[mini-relay] ⚠️ agent 回复的 #h 无对应频道（登记晚于启动？）或无 #h tag，丢弃 id={}",
                    e.id.to_hex()
                ),
            }
        }
        match st {
            Store3::Stored => format!("[\"OK\",\"{}\",true,\"\"]", e.id.to_hex()),
            Store3::Duplicate => format!("[\"OK\",\"{}\",false,\"duplicate\"]", e.id.to_hex()),
            Store3::Failed(err) => {
                crate::log!("[mini-relay] ⚠️ 入库失败 id={}: {err}", e.id.to_hex());
                format!("[\"OK\",\"{}\",false,\"internal error\"]", e.id.to_hex())
            }
        }
    }

    /// fan-out：发给所有命中的订阅。
    fn fan_out(&self, e: &Event) {
        // 先收集再发（不持锁跨 send）。send 失败 = 接收端已被 drop = ws_loop 已
        // 退出（ws_loop 自己也会清，这里是竞态窗口内的兜底）。注意**不能**指望它
        // 清「半开 TCP」：无界 mpsc 对半开连接的 send 永远 Ok——那类僵尸要靠 acp
        // 自己的 30s Ping 断线重连兜底（buzz relay.rs:47、2026），注释不得夸大
        // （审查 #205r5）。
        let mut targets: Vec<(u64, String)> = Vec::new();
        {
            let conns = self.conns.lock().unwrap();
            let subs = self.subs.lock().unwrap();
            for conn_id in conns.keys() {
                if let Some(by_sub) = subs.get(conn_id) {
                    for (sub_id, filters) in by_sub {
                        if filters.iter().any(|f| filter_matches(f, e)) {
                            targets.push((
                                *conn_id,
                                format!("[\"EVENT\",\"{sub_id}\",{}]", e.as_json()),
                            ));
                        }
                    }
                }
            }
        }
        let mut dead: Vec<u64> = Vec::new();
        for (conn_id, frame) in targets {
            let sent = match self.conns.lock().unwrap().get(&conn_id).cloned() {
                Some(tx) => tx.send(frame).is_ok(),
                None => false,
            };
            if !sent && !dead.contains(&conn_id) {
                dead.push(conn_id);
            }
        }
        if !dead.is_empty() {
            let mut conns = self.conns.lock().unwrap();
            let mut subs = self.subs.lock().unwrap();
            for id in dead {
                conns.remove(&id);
                subs.remove(&id);
            }
        }
    }
}

/// mini-relay axum 应用：`GET /health`、`GET /`（WS upgrade）、
/// `POST /events`、`POST /query`。
pub fn router(state: Arc<RelayState>) -> axum::Router {
    axum::Router::new()
        .route("/health", axum::routing::get(health))
        .route("/", axum::routing::get(ws_upgrade))
        .route("/events", axum::routing::post(post_events))
        .route("/query", axum::routing::post(post_query))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// NIP-42 auth 事件裁决（纯函数，可测）：kind=22242 + 自洽签名 + challenge tag
/// 必须等于**本连接**发出的那条（acp 的 auth 两个分支都带 challenge tag，buzz
/// relay.rs:3530-3548）。Err = 拒绝原因（进 OK 的 message 与日志）。
fn auth_decision(ev: &nostr::Event, expected_challenge: &str) -> Result<(), String> {
    if ev.kind.as_u16() != 22242 {
        return Err(format!("invalid: kind {} is not 22242", ev.kind.as_u16()));
    }
    if !ev.verify_id() || !ev.verify_signature() {
        return Err("invalid: signature or id".into());
    }
    let got = ev
        .tags
        .iter()
        .find(|t| {
            t.as_slice()
                .first()
                .is_some_and(|k| k.as_str() == "challenge")
        })
        .and_then(|t| t.as_slice().get(1))
        .map(|v| v.as_str())
        .unwrap_or("");
    if got != expected_challenge {
        return Err("invalid: challenge tag mismatch".into());
    }
    Ok(())
}

async fn ws_upgrade(
    axum::extract::State(state): axum::extract::State<Arc<RelayState>>,
    headers: axum::http::HeaderMap,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    // **WS 握手不受 CORS 约束**：浏览器会为跨源 WS 带上 Origin（=发起页的源），
    // 而 relay 只听 127.0.0.1 并不构成防护——用户访问的任意网页都能开
    // ws://127.0.0.1:port 拉走全量对话存档。因此：**带 Origin 且 host 非回环**即拒；
    // 原生客户端不发 Origin，放行。必须**解析后精确比对 host**——前缀匹配
    // （starts_with）会被 `http://127.0.0.1.evil.com` 绕过，"null" 白名单会放进
    // file:// 页面与沙箱 iframe（审查 #205r5）。
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if !origin_allowed(origin) {
            crate::log!("[mini-relay] ⚠️ 拒绝非回环 Origin 的 WS 握手: {origin}");
            return axum::http::Response::builder()
                .status(axum::http::StatusCode::FORBIDDEN)
                .body(axum::body::Body::from("forbidden origin"))
                .unwrap();
        }
    }
    upgrade.on_upgrade(move |socket| async move { ws_loop(state, socket).await })
}

/// Origin 是否来自回环宿主。解析 `scheme://host[:port]` 后**精确比对 host**
/// （端口/协议不限——本机页面本就是用户自己跑的；攻击者域名的相似域被拒）。
/// 不引入 url crate 依赖：手写窄解析（scheme 后取到第一个 `/`，IPv6 取 `[..]`）。
/// `None`（原生客户端不发 Origin）由调用方放行。**不含 "null"**：file:// 页面与
/// 沙箱 iframe 发的就是它。
fn origin_allowed(origin: &str) -> bool {
    let Some((_, rest)) = origin.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped
            .split_once(']')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// WS 会话：AUTH challenge → 帧循环（EVENT/REQ/CLOSE）。
async fn ws_loop(state: Arc<RelayState>, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message as WsMessage;
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
    // 该连接是否已完成 NIP-42 认证（读侧门禁：REQ 未认证不服务）
    let mut authenticated = false;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let conn_id = state.conn_seq.fetch_add(1, Ordering::Relaxed);
    state.conns.lock().unwrap().insert(conn_id, tx);

    let challenge = uuid::Uuid::new_v4().to_string();
    // P0-2 修复：challenge 必须 .await 发出（原 let _ = drop 了 future，帧从未发出）
    if sink
        .send(WsMessage::Text(
            format!("[\"AUTH\",\"{challenge}\"]").into(),
        ))
        .await
        .is_err()
    {
        state.conns.lock().unwrap().remove(&conn_id);
        return;
    }

    loop {
        tokio::select! {
            Some(frame) = rx.recv() => {
                if sink.send(WsMessage::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            msg = stream.next() => {
                let Some(Ok(WsMessage::Text(text))) = msg else { break };
                let Some(frame) = parse_client_frame(&text) else { continue };
                match frame {
                    ClientFrame::Auth(ev) => {
                        // NIP-42：**必须回 OK**。buzz-acp 的 do_connect（buzz
                        // relay.rs:3932-3947）在发出 auth 事件后 `wait_for_any_ok`
                        // 硬等 `accepted=true`，拿不到就 return Err → 重试梯耗尽
                        // → 进程非零退出 → 正好喂给 ABB 的崩溃重拉循环（表现为
                        // 「relay 在跑、日志说已拉起、永远没有回复」）。
                        // 校验：kind=22242 + 自洽签名 + **challenge tag 必须等于
                        // 本连接发的那条**（acp 两个分支都带 challenge tag，buzz
                        // relay.rs:3530-3548；NIP-42 语义本身如此）——否则别的
                        // 连接/别的会话的 auth 事件可以重放过来顶替认证。
                        // Origin 检查挡的是浏览器旁路；challenge 校验挡的是
                        // 「拿别处 auth 事件来重放」，两道各管一面。
                        let (ok, why) = match auth_decision(&ev, &challenge) {
                            // **必须回写认证标记**——上一版重构时把它弄丢，读侧门禁
                            // 将永远关死（unused_mut 警告暴露，教训：重构后立即编译）
                            Ok(()) => {
                                authenticated = true;
                                (true, String::new())
                            }
                            Err(why) => {
                                crate::log!("[mini-relay] ⚠️ 拒绝 AUTH: {why}");
                                (false, why)
                            }
                        };
                        let ack = format!("[\"OK\",\"{}\",{ok},\"{why}\"]", ev.id.to_hex());
                        if sink.send(WsMessage::Text(ack.into())).await.is_err() {
                            break;
                        }
                    }
                    ClientFrame::Event(e) => {
                        let ack = state.ingest(&e).await;
                        if sink.send(WsMessage::Text(ack.into())).await.is_err() {
                            break;
                        }
                    }
                    ClientFrame::Req { .. } if !authenticated => {
                        // 未认证连接不给读：REQ 会回放库里全量 kind-9（含注入的
                        // 历史块、附件本地路径、指令块）。回环本机进程不是本门禁的
                        // 目标（那属于「本机已沦陷」），挡的是浏览器 WS 旁路。
                        let _ = sink
                            .send(WsMessage::Text(
                                r#"["NOTICE","auth required before REQ"]"#.into(),
                            ))
                            .await;
                    }
                    ClientFrame::Req { sub_id, filters } => {
                        // 历史回放 + EOSE
                        for ev in state.db.query(&filters).await {
                            let f = format!("[\"EVENT\",\"{sub_id}\",{}]", ev.as_json());
                            if sink.send(WsMessage::Text(f.into())).await.is_err() {
                                break;
                            }
                        }
                        let _ = sink
                            .send(WsMessage::Text(format!("[\"EOSE\",\"{sub_id}\"]").into()))
                            .await;
                        // 订阅注册：sub_id → 全部 filters（后续新事件 fan-out 到本连接）
                        state
                            .subs
                            .lock()
                            .unwrap()
                            .entry(conn_id)
                            .or_default()
                            .insert(sub_id.clone(), filters);
                        let _ = sub_id;
                    }
                    ClientFrame::Close(sub_id) => {
                        // 删除该连接下的指定订阅（不删整个连接）
                        if let Some(conn_subs) = state.subs.lock().unwrap().get_mut(&conn_id) {
                            conn_subs.remove(&sub_id);
                        }
                    }
                }
            }
            else => break,
        }
    }
    state.conns.lock().unwrap().remove(&conn_id);
    state.subs.lock().unwrap().remove(&conn_id);
}

/// POST /events：Nostr event JSON → 验签 → 入库 → fan-out → ACK。
async fn post_events(
    axum::extract::State(state): axum::extract::State<Arc<RelayState>>,
    body: String,
) -> axum::response::Response {
    let ack = match serde_json::from_str::<nostr::Event>(&body) {
        Ok(e) => state.ingest(&e).await,
        Err(err) => format!("[\"OK\",\"\",false,\"invalid: {err}\"]"),
    };
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .body(axum::body::Body::from(ack))
        .unwrap()
}

/// POST /query：NIP-01 filter 数组 → 匹配事件 JSON 数组。
async fn post_query(
    axum::extract::State(state): axum::extract::State<Arc<RelayState>>,
    body: String,
) -> axum::response::Response {
    let filters: Vec<Filter> = serde_json::from_str(&body).unwrap_or_default();
    let events = state.db.query(&filters).await;
    let json = serde_json::to_string(&events).unwrap_or_else(|_| "[]".into());
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .body(axum::body::Body::from(json))
        .unwrap()
}

/// mini-relay 服务入口：端口监听（service spawn 调用）。
/// 关停感知版（#8）：`stop` 触发即优雅排水并返回 Ok——仓库纪律是每个长驻循环
/// 都观察关停令牌（否则 shutdown_wait 恒烧满 20s 总期限走强退，每次正常关停
/// 在日志/看门狗统计里都像崩溃，升级也白等 20s）。
pub async fn run_server_until(
    state: Arc<RelayState>,
    port: u16,
    stop: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    axum::serve(listener, router(state))
        // move 进异步块：with_graceful_shutdown 要 'static future，借用的令牌不满足
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await
}
