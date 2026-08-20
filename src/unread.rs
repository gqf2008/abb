//! 未读私聊提醒队列（#74）—— service（bridge）写、GUI 托盘红点/弹窗读。
//!
//! 单文件 `logs/unread.json`：`{count, items: [{bot_key, sender, preview, ts}]}`，
//! items 最近 20 条、最新在前；有未读时写，清空时写 `{count:0, items:[]}`。
//! 与 botstatus.rs 同款模式：service 写、GUI 读；进程内 Mutex 串行化写 + 原子写文件。
//! GUI「弹出即已读」不直接写本文件（会与 service 的写竞争），而是落 `msg-read.command`
//! 令牌，由 service 的 history-gc 任务消费后清空——service 是 unread.json 的唯一写方。

use serde_json::{json, Value};
use std::sync::Mutex;

/// 未读列表条数上限（红点/弹窗只关心最近几条）。
pub const MAX_ITEMS: usize = 20;

fn path() -> std::path::PathBuf {
    crate::bridge_dir().join("logs").join("unread.json")
}

/// 未读队列句柄（进程内 Mutex 串行写）。生产用 `production()`；
/// 测试用 `at(临时路径)` 隔离——handle 内的提醒绝不能碰真实用户 unread.json。
pub struct UnreadStore {
    path: std::path::PathBuf,
    mu: Mutex<()>,
}

impl UnreadStore {
    pub fn production() -> UnreadStore {
        UnreadStore {
            path: path(),
            mu: Mutex::new(()),
        }
    }

    /// 按指定路径构造（测试注入临时路径，先例：DeliveryStore::new_at / PendingStore::at）。
    /// cfg(test)：只有测试构建需要（bridge 测试注入隔离路径），非测试构建不编译，
    /// 避免 dead_code。
    #[cfg(test)]
    pub fn at(path: std::path::PathBuf) -> UnreadStore {
        UnreadStore {
            path,
            mu: Mutex::new(()),
        }
    }

    /// 有新的授权者私聊消息：插到队首，超上限丢最旧，整文件原子写回。
    /// sender 传发送者 id（open_id/staffId）；展示名由 GUI 侧经 config 授权者名单反查
    /// （授权时已反查过名字，桥内再做一次异步反查不值——参考 botstatus 只传原始态）。
    pub fn report(&self, bot_key: &str, sender: &str, name: &str, preview: &str, ts: i64) {
        let _g = self.mu.lock().unwrap();
        let mut items: Vec<Value> = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default();
        items.insert(
            0,
            json!({"bot_key": bot_key, "sender": sender, "name": name, "preview": preview, "ts": ts}),
        );
        items.truncate(MAX_ITEMS);
        self.write(&items);
    }

    /// 清空未读（「弹出即已读」/ 手动清除的执行端）。只允许 service 侧调用（唯一写方）。
    pub fn clear(&self) {
        let _g = self.mu.lock().unwrap();
        self.write(&[]);
    }

    fn write(&self, items: &[Value]) {
        let state = json!({"count": items.len(), "items": items});
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = crate::atomic_write_text(
            &self.path,
            &serde_json::to_string_pretty(&state).unwrap_or_default(),
        );
    }

    /// GUI 读：文件缺失/损坏 → None；条数为 0 表示无未读（count 以 items 实际条数为准）。
    pub fn snapshot(&self) -> Option<Vec<UnreadItem>> {
        let s = std::fs::read_to_string(&self.path).ok()?;
        let v: Value = serde_json::from_str(&s).ok()?;
        let items = v["items"].as_array()?;
        Some(
            items
                .iter()
                .map(|e| UnreadItem {
                    bot_key: e["bot_key"].as_str().unwrap_or("").to_string(),
                    sender: e["sender"].as_str().unwrap_or("").to_string(),
                    name: e["name"].as_str().unwrap_or("").to_string(),
                    preview: e["preview"].as_str().unwrap_or("").to_string(),
                    ts: e["ts"].as_i64().unwrap_or(0),
                })
                .collect(),
        )
    }
}

/// 一条未读项（GUI 弹窗展示用）。
#[derive(Debug, Clone)]
pub struct UnreadItem {
    pub bot_key: String,
    /// 发送者 id（open_id / staffId）。
    pub sender: String,
    /// 发送者展示名（bridge 侧反查：授权者用本地名单名、未授权者 API 反查；
    /// 空 = 未查到，GUI 回落 id/名单）。
    pub name: String,
    pub preview: String,
    pub ts: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("abb-unread-test-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn report_inserts_newest_first_and_roundtrips() {
        let s = UnreadStore::at(temp_path("roundtrip"));
        s.report("b1", "ou_1", "王小明", "你好", 100);
        s.report("b1", "ou_2", "", "在吗", 200);
        let items = s.snapshot().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].sender, "ou_2", "最新一条必须在最前");
        assert_eq!(items[0].preview, "在吗");
        assert_eq!(items[1].sender, "ou_1");
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn report_caps_at_max_items() {
        let s = UnreadStore::at(temp_path("cap"));
        for i in 0..(MAX_ITEMS + 5) {
            s.report("b1", &format!("ou_{i}"), "", "x", i as i64);
        }
        let items = s.snapshot().unwrap();
        assert_eq!(items.len(), MAX_ITEMS);
        assert_eq!(items[0].sender, format!("ou_{}", MAX_ITEMS + 4), "最新在前");
        // 丢的是最旧 5 条（ou_0..ou_4），队尾是最旧保留 ou_5
        assert_eq!(items[MAX_ITEMS - 1].sender, "ou_5");
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn clear_writes_empty_state_and_count_zero() {
        let s = UnreadStore::at(temp_path("clear"));
        s.report("b1", "ou_1", "小明", "x", 1);
        s.clear();
        let items = s.snapshot().unwrap();
        assert!(items.is_empty());
        // 文件内容也是 count=0 的空列表（GUI 读到的唯一事实源）
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&s.path).unwrap()).unwrap();
        assert_eq!(v["count"], 0);
        assert!(v["items"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn snapshot_on_missing_file_returns_none() {
        let s = UnreadStore::at(temp_path("missing"));
        assert!(s.snapshot().is_none());
    }
}
