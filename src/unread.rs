//! 未读私聊提醒队列（#74）—— service（bridge）写、GUI 托盘红点/弹窗读。
//!
//! 单文件 `logs/unread.json`：`{count, items: [{bot_key, sender, preview, ts}]}`，
//! 最新在前；清空时写 `{count:0, items:[]}`。
//! 与 botstatus.rs 同款模式：service 写、GUI 读；进程内 Mutex 串行化写 + 原子写文件。
//! GUI「弹出即已读」不直接写本文件（会与 service 的写竞争），而是落 `msg-read.command`
//! 令牌，由 service 的 history-gc 任务消费后清空——service 是 unread.json 的唯一写方。
//!
//! **写入侧已随内置渠道退役**（2026-11-04）：`report`（入站私聊落未读）与它的
//! `MAX_ITEMS` 上限一起删掉了——唯一入口是已删的三个渠道的入站处理器。现在只剩
//! `clear`（「弹出即已读」执行端）与 `snapshot`（GUI 读）；文件因此只会是空的，
//! 红点/弹窗能不能再长出内容需要渠道侧重新接一个写入点（未接之前恒无未读）。

use serde_json::{json, Value};
use std::sync::Mutex;

fn path() -> std::path::PathBuf {
    crate::bridge_dir().join("logs").join("unread.json")
}

/// 未读队列句柄（进程内 Mutex 串行写）。生产用 `production()`；
/// 测试用 `at(临时路径)` 隔离——handle 内的提醒绝不能碰真实用户 unread.json。
/// 队列上限（条）。超出丢最旧——未读提醒是「最近有人找你」的提示，不是完整收件箱。
pub const MAX_ITEMS: usize = 20;

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

    /// 按指定路径构造（bridge 根注入 / 测试隔离共用）。
    pub(crate) fn at(path: std::path::PathBuf) -> UnreadStore {
        UnreadStore {
            path,
            mu: Mutex::new(()),
        }
    }

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

    /// `report` 写进去的东西能被 `snapshot` 原样读回，且**最新的在最前**。
    ///
    /// 这条守的是「未读提醒真的有写入方」——2026-11-04 内置渠道退役后，
    /// `report` 随三个入站入口一起被删，`UnreadStore` 只剩读方（GUI），
    /// **小红点再也不会出现而没有任何报错**。MCP 入站路径接回它之后，
    /// 这条测试保证「写了能读、顺序对」。
    #[test]
    fn report_then_snapshot_roundtrip_newest_first() {
        let st = UnreadStore::at(temp_path("roundtrip"));
        st.report("b1", "ou_a", "张三", "第一条", 100);
        st.report("b1", "ou_b", "李四", "第二条", 200);
        let got = st.snapshot().expect("应当读得回来");
        assert_eq!(got.len(), 2, "两条都要在：{got:?}");
        assert_eq!(got[0].preview, "第二条", "最新的必须在最前");
        assert_eq!(got[1].preview, "第一条");
        assert_eq!(got[0].name, "李四");
        assert_eq!(got[0].ts, 200);
    }

    /// 超过上限时丢**最旧**的，不是丢最新的。
    #[test]
    fn report_caps_at_max_items_dropping_oldest() {
        let st = UnreadStore::at(temp_path("roundtrip"));
        for i in 0..(MAX_ITEMS + 5) {
            st.report("b1", "ou_x", "", &format!("m{i}"), i as i64);
        }
        let got = st.snapshot().expect("应当读得回来");
        assert_eq!(got.len(), MAX_ITEMS, "必须被截到上限");
        assert_eq!(
            got[0].preview,
            format!("m{}", MAX_ITEMS + 4),
            "最前必须是最新那条"
        );
        assert!(
            !got.iter().any(|x| x.preview == "m0"),
            "最旧的应当被丢掉（{got:?}）"
        );
    }

    #[test]
    fn clear_writes_empty_state_and_count_zero() {
        let s = UnreadStore::at(temp_path("clear"));
        // 写入方（report）已随渠道退役：直接造一个「有未读」的文件现场
        let seeded = json!({"count": 1, "items": [
            {"bot_key": "b1", "sender": "ou_1", "name": "小明", "preview": "x", "ts": 1}
        ]});
        std::fs::write(&s.path, serde_json::to_string_pretty(&seeded).unwrap()).unwrap();
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
