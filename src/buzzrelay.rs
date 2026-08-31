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
    /// NIP-42 AUTH 响应（kind 22242 已签名事件）。
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
    #[allow(dead_code)]
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

    /// 种子频道元数据/成员事件（kind 39002 成员 + 39000 元数据，bridge 身份签名）——
    /// buzz-acp discover_channels 两步 /query 的数据源。幂等（同 id 去重）。
    /// agent_pubkey 为空时跳过成员事件（无法构造有效的 #p tag）。
    pub async fn seed_channel_events(&self) {
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

    /// 事件摄取：验签 → 入库 → fan-out → 回流抽取（kind 9 = agent 回复）。
    async fn ingest(&self, e: &Event) -> String {
        let sig_ok = e.verify_id() && e.verify_signature();
        if !sig_ok {
            return format!(
                "[\"OK\",\"{}\",false,\"invalid: signature or id\"]",
                e.id.to_hex()
            );
        }
        let stored = self.db.store(e).await;
        if stored {
            self.fan_out(e);
        }
        // kind 9（Buzz 频道消息）→ 回复回流。不按 pubkey 过滤（Phase 1 简化：
        // 事件已验签、频道已映射，任何 kind 9 都是有效回复）。
        if e.kind.as_u16() == 9 {
            if let Some(h) = h_tag_of(e) {
                if let Some(ch) = self.channel_by_uuid(&h) {
                    let _ = self.reply_tx.send(AgentReply {
                        channel_uuid: h,
                        chat_id: ch.chat_id,
                        content: e.content.clone(),
                    });
                }
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
                    ClientFrame::Auth(_) => {
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
pub async fn run_server(state: Arc<RelayState>, port: u16) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    axum::serve(listener, router(state)).await
}
