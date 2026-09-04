//! 授权者私聊消息记录库（#74）—— 非 owner 私聊提醒 + 设置窗历史页的数据源。
//!
//! - 存储：`~/.agent-bridge/messages.sqlite`（WAL 模式：service 写、GUI 只读并发互不阻塞）。
//! - 写入方只有 bridge（handle 内 per-chat 串行锁下，单进程内天然串行）；GUI 历史页用
//!   只读连接查询（照抄 ccswitch.rs 的 `SQLITE_OPEN_READ_ONLY` 打开模式）；「手动清除」
//!   由 service 的 history-gc 任务消费命令文件执行（GUI 只读连接不能写消息库）。
//! - mid 幂等：`UNIQUE(mid, direction)` + INSERT OR IGNORE。用户轮与助手轮**共用同一
//!   mid**（history.rs 既有语义：一消息一回复），单列 `UNIQUE(mid)` 会把助手条目当重复
//!   拒绝——故用复合唯一（偏离原始设计稿的单列 UNIQUE，理由如上）。
//! - 保留期：超过 `history_retention_days` 的记录由 service 的 history-gc 任务周期
//!   `gc` 清理（启动一次 + 每 24h）。
//! - IO 失败一律只 log 警告：历史是增强能力，绝不阻塞聊天主链路（与 history.rs 同纪律）。

use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

/// 单条历史消息（GUI 历史页会话消息流用）。
#[derive(Debug, Clone)]
pub struct MsgRow {
    /// 库主键（查询投影保留；GUI 消息流按 ts 排序不读它，bridge 测试断言用）
    #[allow(dead_code)]
    pub id: i64,
    pub bot_key: String,
    /// 会话 id（查询投影保留：per-chat 过滤/后续按会话分组用；当前 GUI 不展示）
    #[allow(dead_code)]
    pub chat_id: String,
    /// 消息 id（查询投影保留：去重/定位用；当前 GUI 不展示）
    #[allow(dead_code)]
    pub mid: String,
    /// "user"=发送者消息 / "assistant"=bot 回复。
    pub direction: String,
    pub sender_id: String,
    /// 发送者展示名（落库时反查：未授权 API / 授权者本地名单；空 = 未查到，GUI 回落）。
    pub sender_name: String,
    pub text: String,
    /// 事件时间（unix 秒）。
    pub ts: i64,
}

fn db_path() -> PathBuf {
    crate::bridge_dir().join("messages.sqlite")
}

/// 消息库句柄（无内存态：每次操作现开连接）。生产用 `production()`；
/// 测试用 `at(临时路径)` 隔离——handle 内的落库绝不能碰真实用户消息库。
pub struct MsgStore {
    path: PathBuf,
}

impl MsgStore {
    pub fn production() -> MsgStore {
        MsgStore { path: db_path() }
    }

    /// 按指定路径构造（测试注入临时路径，先例：DeliveryStore::new_at / PendingStore::at）。
    /// cfg(test)：只有测试构建需要（bridge 测试注入隔离路径），非测试构建不编译，
    /// 避免 dead_code。
    #[cfg(test)]
    pub fn at(path: PathBuf) -> MsgStore {
        MsgStore { path }
    }

    /// 打开写连接：建表（幂等）+ WAL + 收紧权限。
    /// WAL 是持久属性，每次打开重设一次幂等；0600 对齐 history.jsonl 的敏感工件权限
    /// （对话内容，与 config.json 同档）。
    fn open_writer(&self) -> Option<Connection> {
        let con = Connection::open(&self.path).ok()?;
        con.pragma_update(None, "journal_mode", "WAL").ok()?;
        con.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bot_key TEXT NOT NULL,
                chat_id TEXT NOT NULL,
                mid TEXT NOT NULL,
                direction TEXT NOT NULL,
                sender_id TEXT NOT NULL,
                sender_name TEXT NOT NULL DEFAULT '',
                text TEXT NOT NULL,
                ts INTEGER NOT NULL,
                UNIQUE(mid, direction)
            );
            CREATE INDEX IF NOT EXISTS idx_messages_bot_ts ON messages(bot_key, ts);
            CREATE INDEX IF NOT EXISTS idx_messages_bot_chat_ts ON messages(bot_key, chat_id, ts);",
        )
        .ok()?;
        // 老库迁移（8-20 加 sender_name 列）：PRAGMA 检查列存在，缺失则 ALTER 补列
        // （CREATE TABLE IF NOT EXISTS 不补列；缺列会让 list_recent 的 SELECT 失败）
        let has_col: bool = con
            .prepare("PRAGMA table_info(messages)")
            .ok()?
            .query_map([], |r| r.get::<_, String>(1))
            .ok()?
            .filter_map(|r| r.ok())
            .any(|name| name == "sender_name");
        if !has_col {
            let _ = con.execute(
                "ALTER TABLE messages ADD COLUMN sender_name TEXT NOT NULL DEFAULT ''",
                [],
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Some(con)
    }

    /// 落一条历史消息。返回是否真正插入（false = mid+direction 已存在，幂等忽略，或失败）。
    /// 调用方：bridge.handle（per-chat 串行锁内，见模块注释）。
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &self,
        bot_key: &str,
        chat_id: &str,
        mid: &str,
        direction: &str,
        sender_id: &str,
        sender_name: &str,
        text: &str,
        ts: i64,
    ) -> bool {
        let Some(con) = self.open_writer() else {
            crate::log!("[msgstore] 打开消息库失败，跳过落库");
            return false;
        };
        match con.execute(
            "INSERT OR IGNORE INTO messages (bot_key, chat_id, mid, direction, sender_id, sender_name, text, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![bot_key, chat_id, mid, direction, sender_id, sender_name, text, ts],
        ) {
            Ok(n) => n > 0,
            Err(e) => {
                crate::log!("[msgstore] 落库失败: {e:#}");
                false
            }
        }
    }

    /// GUI 历史页数据：全 bot、最新在上（ts 反序，同秒按 id 反序保持插入顺序）。
    /// 只读连接——GUI 进程绝不能写消息库；库不存在/损坏只返回空列表。
    /// （历史页 #会话化 后改走 chat_stats+list_chat；本函数保留给 bridge 测试。）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn list_recent(&self, limit: usize) -> Vec<MsgRow> {
        if !self.path.exists() {
            return Vec::new();
        }
        let con = match Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(e) => {
                crate::log!("[msgstore] 只读打开失败: {e:#}");
                return Vec::new();
            }
        };
        let mut stmt = match con.prepare(
            "SELECT id, bot_key, chat_id, mid, direction, sender_id, sender_name, text, ts
             FROM messages ORDER BY ts DESC, id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                crate::log!("[msgstore] 查询失败: {e:#}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(rusqlite::params![limit as i64], |r| {
            Ok(MsgRow {
                id: r.get(0)?,
                bot_key: r.get(1)?,
                chat_id: r.get(2)?,
                mid: r.get(3)?,
                direction: r.get(4)?,
                sender_id: r.get(5)?,
                sender_name: r.get(6)?,
                text: r.get(7)?,
                ts: r.get(8)?,
            })
        });
        rows.and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
            .unwrap_or_default()
    }

    /// GUI 历史页会话消息流：某 bot 某 chat 的全部消息，**ts 升序**（聊天流正序）。
    /// limit 防御超大历史（取最近的 limit 条再正序返回）；走 (bot_key, chat_id, ts) 索引。
    pub fn list_chat(&self, bot_key: &str, chat_id: &str, limit: usize) -> Vec<MsgRow> {
        if !self.path.exists() {
            return Vec::new();
        }
        let con = match Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(e) => {
                crate::log!("[msgstore] 只读打开失败: {e:#}");
                return Vec::new();
            }
        };
        // 内层 DESC 取最近 limit 条，外层 ASC 翻正序
        let mut stmt = match con.prepare(
            "SELECT id, bot_key, chat_id, mid, direction, sender_id, sender_name, text, ts FROM (
                 SELECT id, bot_key, chat_id, mid, direction, sender_id, sender_name, text, ts
                 FROM messages WHERE bot_key = ?1 AND chat_id = ?2
                 ORDER BY ts DESC, id DESC LIMIT ?3
             ) ORDER BY ts ASC, id ASC",
        ) {
            Ok(s) => s,
            Err(e) => {
                crate::log!("[msgstore] 会话消息查询失败: {e:#}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(rusqlite::params![bot_key, chat_id, limit as i64], |r| {
            Ok(MsgRow {
                id: r.get(0)?,
                bot_key: r.get(1)?,
                chat_id: r.get(2)?,
                mid: r.get(3)?,
                direction: r.get(4)?,
                sender_id: r.get(5)?,
                sender_name: r.get(6)?,
                text: r.get(7)?,
                ts: r.get(8)?,
            })
        });
        rows.and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
            .unwrap_or_default()
    }

    /// 保留期清理：删 `ts < now - retention_days` 的记录，返回删除条数。
    /// 由 service 的 history-gc 任务周期调用（启动一次 + 每 24h）。
    pub fn gc(&self, retention_days: u32) -> u64 {
        let days = retention_days.max(1) as i64;
        let cutoff = crate::chrono_lite::unix_secs() as i64 - days * 86400;
        let Some(con) = self.open_writer() else {
            return 0;
        };
        match con.execute(
            "DELETE FROM messages WHERE ts < ?1",
            rusqlite::params![cutoff],
        ) {
            Ok(n) => n as u64,
            Err(e) => {
                crate::log!("[msgstore] 保留期清理失败: {e:#}");
                0
            }
        }
    }

    /// 清空全部消息记录（设置窗「手动清除」执行端）。service 侧调用（GUI 只读连接不能写）。
    pub fn clear_all(&self) -> u64 {
        let Some(con) = self.open_writer() else {
            return 0;
        };
        match con.execute("DELETE FROM messages", []) {
            Ok(n) => n as u64,
            Err(e) => {
                crate::log!("[msgstore] 清空消息库失败: {e:#}");
                0
            }
        }
    }

    /// 按 (bot_key, chat_id) 聚合的消息统计（#87 session list 数据源）。
    /// 消息量按 chat_id 粒度（msgstore 只记原始 chat_id，话题消息同属其群）；
    /// 库不存在/损坏返回空。
    pub fn chat_stats(&self) -> Vec<ChatStats> {
        if !self.path.exists() {
            return Vec::new();
        }
        let con = match Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(e) => {
                crate::log!("[msgstore] 只读打开失败: {e:#}");
                return Vec::new();
            }
        };
        let cutoff = crate::chrono_lite::unix_secs() as i64 - 7 * 86400;
        let mut stmt = match con.prepare(
            "SELECT m.bot_key, m.chat_id,
                    SUM(CASE WHEN m.ts >= ?1 THEN 1 ELSE 0 END),
                    COUNT(*),
                    MAX(m.ts),
                    (SELECT m2.sender_name FROM messages m2
                      WHERE m2.bot_key = m.bot_key AND m2.chat_id = m.chat_id
                      ORDER BY m2.ts DESC, m2.id DESC LIMIT 1),
                    (SELECT m2.text FROM messages m2
                      WHERE m2.bot_key = m.bot_key AND m2.chat_id = m.chat_id
                      ORDER BY m2.ts DESC, m2.id DESC LIMIT 1),
                    (SELECT m2.sender_id FROM messages m2
                      WHERE m2.bot_key = m.bot_key AND m2.chat_id = m.chat_id
                      ORDER BY m2.ts DESC, m2.id DESC LIMIT 1)
             FROM messages m GROUP BY m.bot_key, m.chat_id",
        ) {
            Ok(s) => s,
            Err(e) => {
                crate::log!("[msgstore] chat_stats 查询失败: {e:#}");
                return Vec::new();
            }
        };
        stmt.query_map(rusqlite::params![cutoff], |r| {
            Ok(ChatStats {
                bot_key: r.get(0)?,
                chat_id: r.get(1)?,
                count_7d: r.get(2)?,
                count_total: r.get(3)?,
                last_ts: r.get(4)?,
                last_sender: r.get(5).unwrap_or_default(),
                last_text: r.get(6).unwrap_or_default(),
                last_sender_id: r.get(7).unwrap_or_default(),
            })
        })
        .and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
        .unwrap_or_default()
    }

    /// 删除某 bot 某 chat 的全部消息记录（#87 session delete --purge），返回删除条数。
    pub fn delete_chat(&self, bot_key: &str, chat_id: &str) -> u64 {
        let Some(con) = self.open_writer() else {
            return 0;
        };
        match con.execute(
            "DELETE FROM messages WHERE bot_key = ?1 AND chat_id = ?2",
            rusqlite::params![bot_key, chat_id],
        ) {
            Ok(n) => n as u64,
            Err(e) => {
                crate::log!("[msgstore] 删除会话消息失败: {e:#}");
                0
            }
        }
    }

    /// 某 chat 的消息量 + 时间范围（#87 session delete 二次确认展示）。
    /// 返回 (条数, 最早 ts, 最晚 ts)；无记录返回 (0, None, None)。
    pub fn chat_count_and_range(
        &self,
        bot_key: &str,
        chat_id: &str,
    ) -> (u64, Option<i64>, Option<i64>) {
        if !self.path.exists() {
            return (0, None, None);
        }
        let con = match Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(e) => {
                crate::log!("[msgstore] 只读打开失败: {e:#}");
                return (0, None, None);
            }
        };
        let mut stmt = match con.prepare(
            "SELECT COUNT(*), MIN(ts), MAX(ts) FROM messages WHERE bot_key = ?1 AND chat_id = ?2",
        ) {
            Ok(s) => s,
            Err(e) => {
                crate::log!("[msgstore] chat_count 查询失败: {e:#}");
                return (0, None, None);
            }
        };
        stmt.query_row(rusqlite::params![bot_key, chat_id], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?))
        })
        .unwrap_or((0, None, None))
    }
}

/// 单条会话消息统计（#87 session list 用）。
#[derive(Debug, Clone)]
pub struct ChatStats {
    pub bot_key: String,
    pub chat_id: String,
    /// 近 7 天消息数。
    pub count_7d: i64,
    /// 总消息数。
    pub count_total: i64,
    /// 最后一条消息时间（unix 秒）。
    pub last_ts: Option<i64>,
    /// 最后一条消息的发送者名（会话列表展示；assistant 行 = bot 名）。
    pub last_sender: String,
    /// 最后一条 user 消息的发送者 id（会话名授权名单反查用）。
    pub last_sender_id: String,
    /// 最后一条消息文本预览。
    pub last_text: String,
}

/// 消费 GUI 命令文件（跨进程队列，先例：deliveries.json 的令牌语义；这里是「存在即消费」——
/// 原子写 + rename 保证整文件可见，故不读内容）：
/// - `msg-clear.command`：清空全部历史 + 清空未读提醒（设置窗「手动清除」）
/// - `msg-read.command`：清空未读提醒（提醒弹窗「弹出即已读」，弹窗展示后即落盘）
///
/// 由 service 的 history-gc 任务每 2s 轮询调用（service 是 unread.json 的唯一写方，
/// 弹窗已读走命令文件而非 GUI 直写，避免与 service 的写竞争）。
pub fn consume_commands() {
    let dir = crate::bridge_dir().join("logs");
    for (name, unread_only) in [("msg-clear.command", false), ("msg-read.command", true)] {
        let p = dir.join(name);
        if !p.exists() {
            continue;
        }
        if unread_only {
            crate::unread::UnreadStore::production().clear();
        } else {
            let n = MsgStore::production().clear_all();
            crate::unread::UnreadStore::production().clear();
            crate::log!("[msgstore] 手动清除：删除 {n} 条历史记录");
        }
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("abb-msgstore-test-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn insert_then_list_roundtrip_newest_first() {
        let s = MsgStore::at(temp_path("roundtrip"));
        assert!(s.insert("b1", "c1", "m1", "user", "ou_1", "", "你好", 100));
        assert!(s.insert("b1", "c1", "m1", "assistant", "ou_1", "", "回复", 101));
        assert!(s.insert("b1", "c2", "m2", "user", "ou_2", "", "另一条", 200));
        let rows = s.list_recent(10);
        assert_eq!(rows.len(), 3);
        // 最新在上：m2 最前；同 mid 的用户/助手对保留各自方向
        assert_eq!(rows[0].mid, "m2");
        assert_eq!(rows[1].mid, "m1");
        assert_eq!(rows[1].direction, "assistant");
        assert_eq!(rows[2].mid, "m1");
        assert_eq!(rows[2].direction, "user");
        assert_eq!(rows[2].text, "你好");
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn duplicate_mid_same_direction_is_ignored() {
        let s = MsgStore::at(temp_path("dedup"));
        assert!(s.insert("b1", "c1", "m1", "user", "ou_1", "", "第一遍", 100));
        // 同 mid 同方向（重放兜底）→ 幂等忽略
        assert!(!s.insert("b1", "c1", "m1", "user", "ou_1", "", "第一遍", 100));
        // 同 mid 不同方向（bot 回复）→ 允许
        assert!(s.insert("b1", "c1", "m1", "assistant", "ou_1", "", "回复", 101));
        assert_eq!(s.list_recent(10).len(), 2);
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn list_limits_to_requested_count() {
        let s = MsgStore::at(temp_path("limit"));
        for i in 0..5 {
            s.insert(
                "b1",
                "c1",
                &format!("m{i}"),
                "user",
                "ou_1",
                "",
                "x",
                i * 10,
            );
        }
        assert_eq!(s.list_recent(2).len(), 2);
        assert_eq!(s.list_recent(0).len(), 0);
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn list_recent_on_missing_db_returns_empty() {
        let s = MsgStore::at(temp_path("missing"));
        assert!(s.list_recent(10).is_empty());
    }

    #[test]
    fn gc_deletes_only_expired_rows() {
        let s = MsgStore::at(temp_path("gc"));
        let now = crate::chrono_lite::unix_secs() as i64;
        s.insert(
            "b1",
            "c1",
            "m_old",
            "user",
            "ou_1",
            "",
            "旧",
            now - 31 * 86400,
        );
        s.insert(
            "b1",
            "c1",
            "m_new",
            "user",
            "ou_1",
            "",
            "新",
            now - 29 * 86400,
        );
        // 保留 30 天：31 天前的删掉，29 天前的留下
        assert_eq!(s.gc(30), 1);
        let rows = s.list_recent(10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mid, "m_new");
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn gc_with_zero_days_falls_back_to_one() {
        let s = MsgStore::at(temp_path("gc0"));
        let now = crate::chrono_lite::unix_secs() as i64;
        s.insert("b1", "c1", "m1", "user", "ou_1", "", "x", now - 2 * 86400);
        // retention 0（异常配置）→ 按 1 天兜底，2 天前的记录被删
        assert_eq!(s.gc(0), 1);
        assert!(s.list_recent(10).is_empty());
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn clear_all_empties_table() {
        let s = MsgStore::at(temp_path("clear"));
        s.insert("b1", "c1", "m1", "user", "ou_1", "", "x", 100);
        s.insert("b2", "c2", "m2", "user", "ou_2", "", "y", 200);
        assert_eq!(s.clear_all(), 2);
        assert!(s.list_recent(10).is_empty());
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn chat_stats_aggregates_per_bot_and_chat() {
        let s = MsgStore::at(temp_path("stats"));
        let now = crate::chrono_lite::unix_secs() as i64;
        // b1/c1：2 条（1 条近 7 天）；b1/c2：1 条（10 天前）；b2/c1：1 条
        s.insert("b1", "c1", "m1", "user", "ou_1", "", "a", now - 86400);
        s.insert("b1", "c1", "m2", "assistant", "ou_1", "", "b", now);
        s.insert("b1", "c2", "m3", "user", "ou_1", "", "c", now - 10 * 86400);
        s.insert("b2", "c1", "m4", "user", "ou_2", "", "d", now);
        let stats = s.chat_stats();
        assert_eq!(stats.len(), 3);
        let c1 = stats
            .iter()
            .find(|x| x.bot_key == "b1" && x.chat_id == "c1")
            .unwrap();
        assert_eq!(c1.count_total, 2);
        assert_eq!(c1.count_7d, 2, "两条都在 7 天内");
        let c2 = stats
            .iter()
            .find(|x| x.bot_key == "b1" && x.chat_id == "c2")
            .unwrap();
        assert_eq!(c2.count_total, 1);
        assert_eq!(c2.count_7d, 0, "10 天前的消息不计入 7 天");
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn delete_chat_removes_only_target_chat() {
        let s = MsgStore::at(temp_path("delchat"));
        s.insert("b1", "c1", "m1", "user", "ou_1", "", "x", 100);
        s.insert("b1", "c1", "m2", "assistant", "ou_1", "", "y", 101);
        s.insert("b1", "c2", "m3", "user", "ou_1", "", "z", 102);
        assert_eq!(s.delete_chat("b1", "c1"), 2);
        let rows = s.list_recent(10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_id, "c2", "其它会话不受影响");
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn chat_count_and_range_reports_impact() {
        let s = MsgStore::at(temp_path("range"));
        assert_eq!(s.chat_count_and_range("b1", "c1"), (0, None, None));
        s.insert("b1", "c1", "m1", "user", "ou_1", "", "x", 100);
        s.insert("b1", "c1", "m2", "assistant", "ou_1", "", "y", 200);
        let (n, min, max) = s.chat_count_and_range("b1", "c1");
        assert_eq!(n, 2);
        assert_eq!(min, Some(100));
        assert_eq!(max, Some(200));
        let _ = std::fs::remove_file(&s.path);
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn chat_stats_returns_sender_and_text() {
        let dir = std::env::temp_dir().join(format!("msg-stats-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = MsgStore::at(dir.join("messages.sqlite"));
        store.insert("b1", "c1", "m1", "user", "u1", "张三", "你好", 1000);
        store.insert("b1", "c1", "m1", "assistant", "bot", "", "回复你", 1001);
        store.insert("b1", "c2", "m2", "user", "u2", "李四", "第二条", 2000);
        let stats = store.chat_stats();
        assert_eq!(stats.len(), 2, "两个会话: {stats:?}");
        let c1 = stats.iter().find(|s| s.chat_id == "c1").unwrap();
        assert_eq!(c1.count_total, 2);
        assert_eq!(c1.last_ts, Some(1001));
        assert_eq!(c1.last_sender, "");
        assert_eq!(c1.last_text, "回复你");
        let c2 = stats.iter().find(|s| s.chat_id == "c2").unwrap();
        assert_eq!(c2.last_sender, "李四");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_chat_returns_ascending() {
        let dir = std::env::temp_dir().join(format!("msg-lc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = MsgStore::at(dir.join("messages.sqlite"));
        for i in 0..5 {
            store.insert(
                "b1",
                "c1",
                &format!("m{i}"),
                "user",
                "u1",
                "张三",
                &format!("消息{i}"),
                1000 + i,
            );
        }
        let rows = store.list_chat("b1", "c1", 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "消息2", "取最近3条（2/3/4）再正序");
        assert_eq!(rows[2].text, "消息4");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod prod_probe {
    #[test]
    #[ignore = "手动探针：验证 production 库可查（勿在 CI 跑，本机有真实库才 pass）"]
    fn production_chat_stats_probe() {
        let stats = super::MsgStore::production().chat_stats();
        println!("production chat_stats: {} 个会话", stats.len());
        for s in stats.iter().take(3) {
            println!("  {} {} last={:?}", s.chat_id, s.last_sender, s.last_ts);
        }
        assert!(!stats.is_empty());
    }
}
