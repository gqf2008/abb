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

/// 频道 uuid：fnv128 确定性派生（命名空间与 #194 vb_uuid 区分）。
/// chat_id ↔ uuid 双向映射由本函数 + 登记表共同维护。
pub fn channel_uuid(bot_key: &str, chat_id: &str) -> String {
    fn fnv64(seed: u64, s: &str) -> u64 {
        let mut h = seed;
        for b in s.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }
    let ns = format!("abb-relay:{bot_key}:{chat_id}");
    let hi = fnv64(0xcbf2_9ce4_8422_2325, &ns);
    let lo = fnv64(0x9e37_79b9_7f4a_7c15, &ns);
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

/// NIP-01 客户端消息（mini-relay 消费的子集）。
#[derive(Debug, Clone)]
pub enum ClientFrame {
    Event(Box<nostr::Event>),
    Req {
        sub_id: String,
        filters: Vec<nostr::Filter>,
    },
    Close(String),
    /// NIP-42 AUTH 响应（Phase 1 不验签不读内容）。
    Auth,
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
        "AUTH" => Some(ClientFrame::Auth),
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

/// 从事件 tags 提取 `#h` 首值（频道 uuid）。
fn h_tag_of(e: &nostr::Event) -> Option<String> {
    e.tags.iter().find_map(|tags| {
        tags.as_slice()
            .first()
            .is_some_and(|t| t.as_str() == "h")
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
    /// 未登记 → false 且不入库；已登记 → true、kind 9、单 #h tag、内容原样；
    /// 同秒同文不同 mid → 两条独立入库（事件 id 含 mid tag，不撞 hash 被吞）。
    #[tokio::test]
    async fn publish_user_message_maps_channel_with_single_h_tag() {
        let db_path = test_db("abb-buzzrelay-pub");
        let store = EventStore::open(&db_path).await.unwrap();
        let (state, _reply_rx) = RelayState::new(store, Keys::generate(), String::new());
        let uuid_a = channel_uuid("bot_a", "oc_a");
        state.set_channels([Channel {
            uuid: uuid_a.clone(),
            chat_id: "oc_a".into(),
            name: "角色A".into(),
            about: String::new(),
        }]);

        // 未登记 chat（含「另一 bot 的同名 chat」——uuid 含 bot_key，不串线）：false 不入库
        assert!(
            !state
                .publish_user_message("bot_x", "oc_a", "m0", "hi")
                .await
        );
        assert!(
            !state
                .publish_user_message("bot_a", "oc_unknown", "m0", "hi")
                .await
        );
        let all: Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        assert!(state.db.query(std::slice::from_ref(&all)).await.is_empty());

        // 已登记频道：true、kind 9、单 #h tag、内容原样
        assert!(
            state
                .publish_user_message("bot_a", "oc_a", "m1", "你好，buzz")
                .await
        );
        let evs = state.db.query(&[all]).await;
        assert_eq!(evs.len(), 1);
        let e = &evs[0];
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
        state
            .publish_user_message("bot_a", "oc_a", "m2", "好")
            .await;
        state
            .publish_user_message("bot_a", "oc_a", "m3", "好")
            .await;
        let nine: Filter = serde_json::from_str(r#"{"kinds":[9]}"#).unwrap();
        let evs = state.db.query(&[nine]).await;
        assert_eq!(evs.len(), 3, "mid tag 应保证同文不同 mid 的两条各自入库");
        drop(state);
        drop(_reply_rx);
        remove_test_db(&db_path);
    }

    /// 准入权威回归（审查 #205r3 高危）：**验签只证明自洽，不证明可信**。任意本地
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

        // ② agent 签的：投递一次，路由键与 chat 都对
        let real = kind9(&agent, "真回复", "chan-1");
        state.ingest(&real).await;
        let got = rx.try_recv().expect("agent 回复应投递");
        assert_eq!(got.chat_id, "oc_1");
        assert_eq!(got.channel_uuid, "chan-1");
        assert_eq!(got.content, "真回复");

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
}

// ══════════ 事件存储（turso）与 relay 服务 ══════════

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 一个频道（= ABB 登记的虚拟 Bot 群）。
#[derive(Debug, Clone)]
pub struct Channel {
    /// 频道 uuid（REQ #h 与回复事件 #h 都用它）
    pub uuid: String,
    /// 对应的 ABB chat_id（回流路由用）
    pub chat_id: String,
    /// 频道名（= 角色名）
    pub name: String,
    /// 频道描述（= 角色提示词）
    pub about: String,
}

/// 回流事件：虚拟 Bot agent 的回复（kind 9），bridge 据此发回聊天平台。
#[derive(Debug, Clone)]
pub struct AgentReply {
    /// 频道 uuid（回流路由键——含 bot 归属，两 bot 同群不串线）
    pub channel_uuid: String,
    pub chat_id: String,
    pub content: String,
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

    /// 入库（id 去重；NIP 语义同 id 重复提交为 no-op）。返回是否新写入。
    pub async fn store(&self, e: &nostr::Event) -> bool {
        let values = Self::row_values(e);
        self.conn
            .execute(
                "INSERT OR IGNORE INTO events
                     (id, pubkey, kind, created_at, h_tag, content, payload_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                turso::params_from_iter(values),
            )
            .await
            .map(|n| n > 0)
            .unwrap_or(false)
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
    /// 频道 uuid 由 (bot_key, chat_id) 确定性派生——不扫表按 chat_id 匹配（两 bot
    /// 同群时会有歧义）。返回 false = 未送达（无频道/签名失败/未入库），调用方负责
    /// 给用户可见反馈（不做静默黑洞）。mid 进 tag：Nostr 事件 id 是内容哈希，
    /// 「同秒同文」两条消息不带 mid 会撞 id——第二条被 INSERT OR IGNORE 静默吞。
    pub async fn publish_user_message(
        &self,
        bot_key: &str,
        chat_id: &str,
        mid: &str,
        content: &str,
    ) -> bool {
        let uuid = channel_uuid(bot_key, chat_id);
        if self.channel_by_uuid(&uuid).is_none() {
            // 非虚拟 Bot 群（或登记晚于 relay 启动的频道集快照），不经 relay；
            // 留日志防静默丢消息。
            crate::log!(
                "[mini-relay] ⚠️ chat 无对应频道（未登记/登记晚于启动），消息不入 relay chat={}",
                crate::agent::truncate(chat_id, 16)
            );
            return false;
        }
        // 单 #h tag：buzz-acp extract_h_tag_uuid 取首个命中，冗余 tag 无益（对齐其
        // build_typing_event 的单 tag 形态）；abb-mid 自定义 tag 保事件 id 唯一。
        let ev = EventBuilder::new(Kind::Custom(9), content)
            .tag(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::H)),
                [uuid.as_str()],
            ))
            .tag(Tag::custom(TagKind::custom("abb-mid"), [mid]))
            .sign_with_keys(&self.bridge_keys)
            .ok();
        let Some(e) = ev else {
            crate::log!("[mini-relay] ⚠️ 用户消息事件签名失败（桥身份密钥异常），丢弃");
            return false;
        };
        if !self.db.store(&e).await {
            crate::log!(
                "[mini-relay] ⚠️ 消息事件未入库（重复 id 或写失败）chat={} mid={}",
                crate::agent::truncate(chat_id, 16),
                crate::agent::truncate(mid, 16)
            );
            return false;
        }
        self.fan_out(&e);
        // 入库 ≠ 有人消费：零订阅时事件仍在库里，acp 重连按 since 水位回放（不丢），
        // 但持续零订阅 = buzz-acp 未装/未起，60s 一条告警让运维看到（审查 #205r2）。
        if self.subscriber_count() == 0 {
            let now = nostr::prelude::Timestamp::now().as_secs();
            if self.should_warn_no_subscriber(now) {
                crate::log!(
                    "[mini-relay] ⚠️ 已入库但当前无 WS 订阅者（buzz-acp 未连？回复会等它重连背充）"
                );
            }
        }
        true
    }

    /// 当前连上来的 WS 连接数（≈buzz-acp 消费者数）。dispatch 用它区分
    /// 「已入库待背充」与「无人消费」——事件存储后 acp 重连会按 since 水位回放
    /// （Phase 1 REQ 语义），所以零订阅**不等于**丢失，只是延迟；但持续零订阅
    /// = buzz-acp 没装/没起，运维必须看得见（审查 #205r2）。
    pub fn subscriber_count(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    /// 测试夹具：登记一个假 WS 连接，用于过 dispatch 预检的「有订阅者」条件
    /// （预检是真的，不能为了测试放宽）。返回的接收端须由调用方持有。
    #[cfg(test)]
    pub fn test_attach_subscriber(&self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let id = self.conn_seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.conns.lock().unwrap().insert(id, tx);
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
        let agent_pk = if self.agent_pubkey.is_empty() {
            None
        } else {
            nostr::PublicKey::from_hex(&self.agent_pubkey).ok()
        };
        for ch in &channels {
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
                    let _ = self.db.store(&ev).await;
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
                let _ = self.db.store(&ev).await;
            }
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
    async fn ingest(&self, e: &Event) -> String {
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
        let stored = self.db.store(e).await;
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
            let h = h_tag_of(e);
            let chan = h.as_deref().and_then(|u| self.channel_by_uuid(u));
            match (h, chan) {
                (Some(uuid), Some(ch)) => {
                    let _ = self.reply_tx.send(AgentReply {
                        channel_uuid: uuid,
                        chat_id: ch.chat_id,
                        content: e.content.clone(),
                    });
                }
                // 频道集是启动快照：agent 回了但 #h 不在集内 = 唯一的静默黑洞，
                // 与 dispatch 侧的日志对称（审查 #205r2）。
                _ => crate::log!(
                    "[mini-relay] ⚠️ agent 回复的 #h 无对应频道（登记晚于启动？）或无 #h tag，丢弃 id={}",
                    e.id.to_hex()
                ),
            }
        }
        if stored {
            format!("[\"OK\",\"{}\",true,\"\"]", e.id.to_hex())
        } else {
            format!("[\"OK\",\"{}\",false,\"duplicate\"]", e.id.to_hex())
        }
    }

    /// fan-out：发给所有命中的订阅。
    fn fan_out(&self, e: &Event) {
        let conns = self.conns.lock().unwrap();
        for (conn_id, tx) in conns.iter() {
            let subs = self.subs.lock().unwrap();
            if let Some(filters) = subs.get(conn_id) {
                for (sub_id, filters) in filters {
                    if filters.iter().any(|f| filter_matches(f, e)) {
                        let frame = format!("[\"EVENT\",\"{sub_id}\",{}]", e.as_json());
                        let _ = tx.send(frame);
                    }
                }
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

async fn ws_upgrade(
    axum::extract::State(state): axum::extract::State<Arc<RelayState>>,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    upgrade.on_upgrade(move |socket| async move { ws_loop(state, socket).await })
}

/// WS 会话：AUTH challenge → 帧循环（EVENT/REQ/CLOSE）。
async fn ws_loop(state: Arc<RelayState>, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message as WsMessage;
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
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
                    ClientFrame::Auth => {
                        // NIP-42 AUTH 响应：本地回环不强制门禁，接受即可
                    }
                    ClientFrame::Event(e) => {
                        let ack = state.ingest(&e).await;
                        if sink.send(WsMessage::Text(ack.into())).await.is_err() {
                            break;
                        }
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
