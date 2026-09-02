//! #206 回复侧记账 —— buzz 后端回复的持久账本。
//!
//! 背景：buzz 后端的回复经 mini-relay 异步回流（dispatch 与回复之间没有同步轮次），
//! 回流路径一旦断在「relay 已入库 → 桥未发送」窗口（崩溃/token 过期），回复就静默
//! 丢失；且回复从不写 ABB 历史，buzz 长期运行后 history 只剩用户轮（buzz→CLI 切换
//! 时的迁移注入是单边转录）。本模块是修复这两件事的记账层：
//!
//! - `awaiting`：dispatch 时登记「用户事件 id → {mid, key, chat_id, epoch, session_id}」。
//!   回复事件的 e-tag（NIP-10）按它反查归属会话与**代际快照**（epoch），回复写历史时
//!   与 /new 互斥（epoch 失配 = 回复属于已作废会话，只发不写）。
//! - `replies`：`回复事件 id → 状态（in_flight/sent）+ 时间`。实时回流与启动对账共用
//!   `claim`（锁内「查未发→标 in-flight」原子转移）防双发；发送成功 `mark_sent`，
//!   失败 `unclaim`（留待下次启动对账补发）。
//!
//! 崩溃语义（at-least-once，与 pending.rs 同口径）：
//! - claim 后 mark_sent 前崩溃 → 盘上残留 in_flight。**加载时一律归一化为「未发」**
//!   （进程已死，in-flight 只可能是上次的残骸；本进程内的 in-flight 恒在内存锁保护下），
//!   启动对账按未发补发——代价是「发送成功但 mark 前崩溃」补发一条重复回复，严格优于
//!   静默丢失。
//! - 账本文件：`workspaces/<bot>/buzz-reply-ledger.json`，atomic_write_sensitive +
//!   0600（含 chat_id/事件 id 等对话元数据，对齐 history/pending 的敏感工件口径）。
//!
//! 剪枝：awaiting 按条数上限 + 保留期丢最旧（被剪的条目若再来迟到回复 → 走 chat 兜底
//! 关联，见 bridge::buzzreply）；replies 按条数上限 + 保留期丢最旧（溢出后同事件 id
//! 再来会当 Fresh 重发一次——at-least-once 语义内可接受，窗口需先撑爆上限）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// awaiting 条数上限：超了丢最旧（正常会话每用户消息一条，500 条 ≈ 数百轮对话，
/// 只防长期运行无界增长）。被剪后迟到回复走 chat 兜底（软关联降级，不丢发送）。
const AWAITING_MAX: usize = 500;
/// awaiting 保留期（秒）：7 天。一轮对话的回复不会跨周到达；到期条目被剪，
/// 迟到回复同上进兜底。
const AWAITING_RETAIN_SECS: u64 = 7 * 86400;
/// replies 已发送记录上限/保留期：去重窗只覆盖「重连重放」的现实窗口（acp 重连
/// 水位回放在分钟~小时级），14 天/2000 条足覆盖；超窗重发属可接受的尾部风险。
const REPLIES_MAX: usize = 2000;
const REPLIES_RETAIN_SECS: u64 = 14 * 86400;

/// dispatch 登记：回复到达时按它定位会话与代际。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwaitingEntry {
    /// 用户消息 mid（诊断/配对参考；历史助手轮的 mid 用回复事件 id，不复用它）。
    pub mid: String,
    /// 会话隔离 key（chat 或 chat:thread）——历史/串行锁必须与 dispatch 同 key，
    /// 否则用户轮与助手轮落进不同历史文件（风险③）。
    pub key: String,
    pub chat_id: String,
    /// dispatch 时历史代际快照（history_lock 内取）。回复到达时代际已变 = /new
    /// 清过历史 → 只发不写（孤儿闸，风险④；与 CLI same_session=false 语义一致）。
    pub epoch: u64,
    pub session_id: String,
    /// 登记时间（unix 秒；剪枝排序用）。
    pub ts: u64,
}

/// 回复事件的发送状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyStatus {
    /// 已认领、发送中（本进程内存锁保护下）；盘上见到它 = 上次进程发送途中崩溃。
    InFlight,
    Sent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ReplyRec {
    status: ReplyStatus,
    ts: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerData {
    /// 用户事件 id → dispatch 登记
    #[serde(default)]
    awaiting: HashMap<String, AwaitingEntry>,
    /// 回复事件 id → 发送状态
    #[serde(default)]
    replies: HashMap<String, ReplyRec>,
}

/// claim 结果（「查未发→标 in-flight」锁内原子转移的产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// 已发送或本进程正在发送——调用方不得再发（重连重放/对账与实时并发去重）。
    Duplicate,
    /// 新回复（已标 in-flight）。附带 awaiting 登记（e-tag 命中时）；
    /// None = 无 e-tag 或登记已被剪枝 → 调用方按 chat 兜底（风险①）。
    Fresh(Option<AwaitingEntry>),
}

/// per-bot 回复账本。全部状态转移在内部 Mutex 内完成（实时回流任务与启动对账
/// 任务并发安全）；每次变更原子整文件重写（文件极小，条数有上限）。
pub struct ReplyLedger {
    path: PathBuf,
    data: Mutex<LedgerData>,
}

impl ReplyLedger {
    pub fn new(bot_key: &str) -> ReplyLedger {
        let dir = crate::workspace_dir(bot_key);
        let _ = std::fs::create_dir_all(&dir);
        Self::at(dir.join("buzz-reply-ledger.json"))
    }

    /// 按指定路径构造（生产/测试共用）。加载归一化：**丢弃全部 in_flight 残留**
    ///（上次进程发送途中崩溃的残骸）——它们按「未发」进入对账补发，防「发送成功
    /// 但 mark 前崩溃」的回复被 in_flight 永久挡住（静默丢失）。解析失败按空账本
    /// 启动 + 留痕（同 pending.rs 口径：静默空置会让下次写盘覆盖损坏文件无迹可循）。
    pub(crate) fn at(path: PathBuf) -> ReplyLedger {
        let mut data = match std::fs::read_to_string(&path) {
            Ok(t) => match serde_json::from_str::<LedgerData>(&t) {
                Ok(v) => v,
                Err(e) => {
                    crate::log!(
                        "[replyledger] ⚠️ 账本解析失败，按空账本启动（下次写盘将覆盖原文件）path={}: {e:#}",
                        path.display()
                    );
                    LedgerData::default()
                }
            },
            Err(_) => LedgerData::default(),
        };
        let stale = data.replies.len();
        data.replies
            .retain(|_, r| r.status != ReplyStatus::InFlight);
        let stale = stale - data.replies.len();
        if stale > 0 {
            crate::log!(
                "[replyledger] 加载归一化：{} 条 in-flight 残留按未发处理（启动对账将补发）path={}",
                stale,
                path.display()
            );
        }
        ReplyLedger {
            path,
            data: Mutex::new(data),
        }
    }

    /// dispatch 登记（发布 mini-relay 成功后调用）。同一用户事件 id 重登记 =
    /// 崩溃重放同一 mid 的幂等覆盖（事件 id 是内容哈希，重放同 mid 同文必同 id）。
    pub fn register_dispatch(&self, user_event_id: &str, entry: AwaitingEntry) {
        let mut data = self.data.lock().unwrap();
        data.awaiting.insert(user_event_id.to_string(), entry);
        Self::prune(&mut data, crate::chrono_lite::unix_secs());
        self.save(&data);
    }

    /// 回复认领（实时回流与启动对账共用）：锁内「查重 → 标 in-flight」原子转移，
    /// 并发下同一回复事件至多一个 Fresh（风险⑤）。`in_reply_to` = 回复事件首个
    /// e-tag（被回复的用户事件 id）。
    pub fn claim(&self, reply_event_id: &str, in_reply_to: Option<&str>) -> Claim {
        let mut data = self.data.lock().unwrap();
        if data.replies.contains_key(reply_event_id) {
            return Claim::Duplicate;
        }
        data.replies.insert(
            reply_event_id.to_string(),
            ReplyRec {
                status: ReplyStatus::InFlight,
                ts: crate::chrono_lite::unix_secs(),
            },
        );
        let entry = in_reply_to.and_then(|u| data.awaiting.get(u).cloned());
        Self::prune(&mut data, crate::chrono_lite::unix_secs());
        self.save(&data);
        Claim::Fresh(entry)
    }

    /// 发送成功 → 终态。重复 mark 幂等（重放补发与实时路径撞单时第二次仍落 Sent）。
    pub fn mark_sent(&self, reply_event_id: &str) {
        let mut data = self.data.lock().unwrap();
        data.replies.insert(
            reply_event_id.to_string(),
            ReplyRec {
                status: ReplyStatus::Sent,
                ts: crate::chrono_lite::unix_secs(),
            },
        );
        Self::prune(&mut data, crate::chrono_lite::unix_secs());
        self.save(&data);
    }

    /// 发送失败 → 回滚认领（记录移除 = 未发），下次启动对账按未发补发
    /// （对齐 pending.rs W2「失败留盘，下次启动再试」；本进程内不自动重试）。
    pub fn unclaim(&self, reply_event_id: &str) {
        let mut data = self.data.lock().unwrap();
        if data
            .replies
            .get(reply_event_id)
            .is_some_and(|r| r.status == ReplyStatus::InFlight)
        {
            data.replies.remove(reply_event_id);
            self.save(&data);
        }
    }

    /// 启动对账用快照：全部 awaiting 登记（按登记时间升序，恢复顺序与对话顺序一致）。
    pub fn awaiting_snapshot(&self) -> Vec<(String, AwaitingEntry)> {
        let data = self.data.lock().unwrap();
        let mut v: Vec<(String, AwaitingEntry)> = data
            .awaiting
            .iter()
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        v.sort_by_key(|(_, e)| e.ts);
        v
    }

    /// 该回复事件是否已确认发送（sent 终态；对账/测试断言用）。
    #[cfg(test)]
    pub(crate) fn is_sent(&self, reply_event_id: &str) -> bool {
        self.data
            .lock()
            .unwrap()
            .replies
            .get(reply_event_id)
            .is_some_and(|r| r.status == ReplyStatus::Sent)
    }

    /// 剪枝（每次写盘前；条数上限 + 保留期，丢最旧）。纯决策抽离出来便于单测钉死。
    fn prune(data: &mut LedgerData, now: u64) {
        data.awaiting
            .retain(|_, e| now.saturating_sub(e.ts) <= AWAITING_RETAIN_SECS);
        while data.awaiting.len() > AWAITING_MAX {
            // 找最旧（ts 最小）的键删除；条数有限（≤501），线性扫代价可忽略
            if let Some(oldest) = data
                .awaiting
                .iter()
                .min_by_key(|(_, e)| e.ts)
                .map(|(k, _)| k.clone())
            {
                data.awaiting.remove(&oldest);
            } else {
                break;
            }
        }
        data.replies
            .retain(|_, r| now.saturating_sub(r.ts) <= REPLIES_RETAIN_SECS);
        while data.replies.len() > REPLIES_MAX {
            if let Some(oldest) = data
                .replies
                .iter()
                .min_by_key(|(_, r)| r.ts)
                .map(|(k, _)| k.clone())
            {
                data.replies.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// 原子写盘（0600 + tmp + rename）：对话元数据属敏感工件，与 history/pending 同口径。
    /// 失败必须留痕——账本是「崩溃后补发」的持久性依据，静默丢账 = 静默丢回复。
    fn save(&self, data: &LedgerData) {
        match serde_json::to_string(data) {
            Ok(text) => {
                if let Err(e) = crate::atomic_write_sensitive(&self.path, &text) {
                    crate::log!(
                        "[replyledger] ⚠️ 账本写盘失败 path={}: {e:#}",
                        self.path.display()
                    );
                }
            }
            Err(e) => crate::log!("[replyledger] ⚠️ 账本序列化失败: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "abb-replyledger-test-{name}-{}.json",
            uuid::Uuid::new_v4()
        ))
    }

    fn entry(mid: &str, ts: u64) -> AwaitingEntry {
        AwaitingEntry {
            mid: mid.into(),
            key: "oc_1".into(),
            chat_id: "oc_1".into(),
            epoch: 0,
            session_id: "sid-1".into(),
            ts,
        }
    }

    /// 当前时间的登记（非剪枝测试用）：ts 必须是真实时钟附近——剪枝按保留期
    /// （7 天）以真实 now 判定，写死的小 ts 会在 register/claim 时被立刻剪掉。
    fn fresh_entry(mid: &str) -> AwaitingEntry {
        entry(mid, crate::chrono_lite::unix_secs())
    }

    /// 状态机主线：register → claim(Fresh 带登记) → mark_sent → 重启（新实例重读）
    /// 仍在；同回复事件再 claim → Duplicate（重连重放去重）。
    #[test]
    fn register_claim_sent_roundtrip_persists() {
        let p = temp_path("roundtrip");
        let led = ReplyLedger::at(p.clone());
        led.register_dispatch("uev-1", fresh_entry("m1"));
        match led.claim("rev-1", Some("uev-1")) {
            Claim::Fresh(Some(e)) => {
                assert_eq!(e.mid, "m1");
                assert_eq!(e.session_id, "sid-1");
            }
            other => panic!("首次 claim 必须是 Fresh 且带回 awaiting 登记: {other:?}"),
        }
        // 同一事件 id 再 claim（对账与实时并发）→ Duplicate
        assert_eq!(led.claim("rev-1", Some("uev-1")), Claim::Duplicate);
        led.mark_sent("rev-1");
        assert!(led.is_sent("rev-1"));

        // 新实例重读（模拟重启）：登记与 sent 都在
        let led2 = ReplyLedger::at(p.clone());
        assert!(led2.is_sent("rev-1"));
        assert_eq!(led2.claim("rev-1", Some("uev-1")), Claim::Duplicate);
        let snap = led2.awaiting_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "uev-1");
        let _ = std::fs::remove_file(&p);
    }

    /// in-flight 崩溃窗口：claim 后未 mark_sent 就「进程死了」（盘上残留 in_flight）
    /// → 重启加载归一化为未发 → 再 claim 得 Fresh（对账补发的依据，防永久挡住）。
    #[test]
    fn in_flight_residue_is_unsent_after_reload() {
        let p = temp_path("inflight");
        let led = ReplyLedger::at(p.clone());
        led.register_dispatch("uev-1", fresh_entry("m1"));
        assert!(matches!(led.claim("rev-1", Some("uev-1")), Claim::Fresh(_)));
        drop(led); // 不 mark_sent，模拟发送途中崩溃

        let led2 = ReplyLedger::at(p.clone());
        assert!(!led2.is_sent("rev-1"), "in-flight 残留重启后不得当作已发送");
        assert!(
            matches!(led2.claim("rev-1", Some("uev-1")), Claim::Fresh(_)),
            "in-flight 残留必须按未发补发（去掉加载归一化此断言必红）"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 发送失败回滚：unclaim 后记录移除 → 同事件可再 claim（下次对账补发）；
    /// 非 in-flight（已 sent）的 unclaim 是 no-op（不得把终态打回）。
    #[test]
    fn unclaim_rolls_back_only_in_flight() {
        let p = temp_path("unclaim");
        let led = ReplyLedger::at(p.clone());
        led.register_dispatch("uev-1", fresh_entry("m1"));
        assert!(matches!(led.claim("rev-1", Some("uev-1")), Claim::Fresh(_)));
        led.unclaim("rev-1");
        assert!(
            matches!(led.claim("rev-1", Some("uev-1")), Claim::Fresh(_)),
            "unclaim 后必须可再认领（否则发送失败的回复永久丢失）"
        );
        led.mark_sent("rev-1");
        led.unclaim("rev-1");
        assert!(led.is_sent("rev-1"), "sent 终态不得被 unclaim 打回");
        let _ = std::fs::remove_file(&p);
    }

    /// e-tag 未命中（agent 未带 --reply-to / 登记已被剪枝）→ Fresh(None)，
    /// 调用方按 chat 兜底（风险①），绝不阻塞发送。
    #[test]
    fn claim_without_matching_awaiting_is_fresh_none() {
        let p = temp_path("fallback");
        let led = ReplyLedger::at(p.clone());
        assert_eq!(led.claim("rev-x", None), Claim::Fresh(None));
        assert_eq!(led.claim("rev-y", Some("uev-unknown")), Claim::Fresh(None));
        let _ = std::fs::remove_file(&p);
    }

    /// 剪枝纯函数：保留期外的丢；超上限丢最旧；窗内的保留（含 in-flight）。
    #[test]
    fn prune_drops_expired_and_oldest() {
        let now = 10_000_000u64; // 必须大于两张表的保留期（14 天），否则减法溢出
        let mut data = LedgerData::default();
        // 保留期外（> 7 天）的 awaiting 丢；窗内的留
        data.awaiting
            .insert("old".into(), entry("m-old", now - AWAITING_RETAIN_SECS - 1));
        data.awaiting.insert("new".into(), entry("m-new", now));
        // awaiting 超上限：填到上限+2（全在保留期内），最旧两条丢
        for i in 0..(AWAITING_MAX + 2) {
            data.awaiting.insert(
                format!("bulk-{i}"),
                entry(&format!("m-{i}"), now - 100 + i as u64),
            );
        }
        // replies：保留期外丢（含 in-flight 残留也照剪——剪了等于按未发补发，安全侧）
        data.replies.insert(
            "r-old".into(),
            ReplyRec {
                status: ReplyStatus::Sent,
                ts: now - REPLIES_RETAIN_SECS - 1,
            },
        );
        data.replies.insert(
            "r-new".into(),
            ReplyRec {
                status: ReplyStatus::Sent,
                ts: now,
            },
        );
        ReplyLedger::prune(&mut data, now);
        assert!(!data.awaiting.contains_key("old"), "过期 awaiting 必须剪");
        assert!(data.awaiting.contains_key("new"));
        // 年龄剪「old」后剩 1 + (MAX+2) = MAX+3 → 条数剪到 MAX：最旧三条 bulk-0/1/2 丢
        assert_eq!(data.awaiting.len(), AWAITING_MAX, "超上限丢最旧直到封顶");
        for gone in ["bulk-0", "bulk-1", "bulk-2"] {
            assert!(
                !data.awaiting.contains_key(gone),
                "最旧的先剪（{gone} 应在被剪之列）"
            );
        }
        assert!(data.awaiting.contains_key("bulk-3"));
        assert!(data
            .awaiting
            .contains_key(&format!("bulk-{}", AWAITING_MAX + 1)));
        assert!(!data.replies.contains_key("r-old"), "过期 sent 记录必须剪");
        assert!(data.replies.contains_key("r-new"));
    }

    /// 损坏文件：解析失败按空账本启动不 panic，且下次写盘覆盖（同 pending.rs 口径）。
    #[test]
    fn corrupted_file_starts_empty() {
        let p = temp_path("corrupt");
        std::fs::write(&p, "{not json").unwrap();
        let led = ReplyLedger::at(p.clone());
        assert!(led.awaiting_snapshot().is_empty());
        led.register_dispatch("uev-1", fresh_entry("m1"));
        let led2 = ReplyLedger::at(p.clone());
        assert_eq!(led2.awaiting_snapshot().len(), 1);
        let _ = std::fs::remove_file(&p);
    }

    /// 敏感工件权限（unix）：账本含 chat_id/事件 id 对话元数据，必须 0600。
    #[cfg(unix)]
    #[test]
    fn ledger_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let p = temp_path("mode");
        let led = ReplyLedger::at(p.clone());
        led.register_dispatch("uev-1", fresh_entry("m1"));
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "账本必须 0600（对话元数据）");
        let _ = std::fs::remove_file(&p);
    }
}
