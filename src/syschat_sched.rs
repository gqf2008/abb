//! 系统对话「定时发送」——固定档位解析 + 一处持久化 + 到点判定。
//!
//! ## 为什么不直接复用 IM 定时任务（`schedule::JobStore` / `service::run_job`）
//!
//! 那条链的语义是「按某个 **bot** 的 `chat_id`，把 prompt 投给 IM 渠道」：
//! `JobStore` 落在 `workspaces/<bot_key>/jobs.json`，执行端 `run_job` 需要
//! `JobTarget`/`Router::deliver`，结果经飞书/微信等渠道发回。系统对话是
//! **GUI 进程内的 owner 私有会话**（见 `syschat.rs` 头注：`--service` 无头运行时不实例化窗口），
//! 没有 bot、没有 IM `chat_id`，硬塞进去只会凭空造一个假 bot 并把消息发到错误的地方。
//!
//! 因此这里**复用的是 `schedule` 的时间底座**（[`DateTime`] / `schedule::parse_once` /
//! `from_unix`，同一套 UTC+8 与 `"YYYY-MM-DD HH:MM"` 格式），自己只加一层最小的
//! 「syschat 定时消息」持久化 + 到点判定。到点由托盘进程**既有的**主定时器
//! （`ui.rs` 里 2s 的 `slint::Timer`）驱动发出，不新增线程、不新增依赖。
//!
//! ## 持久化位置
//!
//! `~/.agent-bridge/syschat/scheduled.json`（与 `sessions.json` 同目录，一处落盘）。
//!
//! ## 边界（如实记录）
//!
//! 窗口进程不在时不会触发：本功能依赖托盘进程存活（GUI 专属）。无头 `--service`
//! 不跑 syschat 调度——这是「系统对话只在 GUI 存在」的既有产品边界，不是本卡的遗漏。

use crate::schedule::DateTime;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 固定档位（菜单里的三项）。刻意只给固定档：选时间既不需要日历控件，
/// 也避免解析自由文本带来的歧义；需要「随便挑个时刻」时另立卡。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// 5 分钟后（`now + 5min`）。
    In5Min,
    /// 30 分钟后（`now + 30min`）。
    In30Min,
    /// 下一个 18:00（今天 18:00 未过就是今天，已过就是明天）。
    Today18,
}

/// 菜单档位顺序（下标即 UI 传给 [`Preset::from_index`] 的值）。
pub const PRESETS: [Preset; 3] = [Preset::In5Min, Preset::In30Min, Preset::Today18];

impl Preset {
    /// UI 下标 → 档位（越界 = None，UI 只发 0..3）。
    pub fn from_index(i: i32) -> Option<Preset> {
        match i {
            0 => Some(Preset::In5Min),
            1 => Some(Preset::In30Min),
            2 => Some(Preset::Today18),
            _ => None,
        }
    }

    /// 档位在 `now` 的**下一次**触发时刻，格式与 once 任务同款 `"YYYY-MM-DD HH:MM"`。
    /// 结果一定 **> now**（[`Preset::Today18`] 已过则滚到明天），因此不会「一排就立刻发」。
    pub fn resolve(self, now: DateTime) -> String {
        match self {
            Preset::In5Min => format_at(&DateTime::from_unix(now.to_unix() + 5 * 60)),
            Preset::In30Min => format_at(&DateTime::from_unix(now.to_unix() + 30 * 60)),
            Preset::Today18 => {
                let today = DateTime {
                    hour: 18,
                    minute: 0,
                    ..now
                };
                if today.to_unix() > now.to_unix() {
                    format_at(&today)
                } else {
                    format_at(&DateTime::from_unix(today.to_unix() + 86400))
                }
            }
        }
    }

    /// 菜单文案。[`Preset::Today18`] 按当时钟点显示「今天/明天」——文案与 [`Preset::resolve`]
    /// 用的是**同一个判据**（`now < 18:00`），不会出现「写着今天、排到明天」。
    pub fn label(self, now: DateTime) -> String {
        match self {
            Preset::In5Min => "5 分钟后".to_string(),
            Preset::In30Min => "30 分钟后".to_string(),
            Preset::Today18 => {
                let today = DateTime {
                    hour: 18,
                    minute: 0,
                    ..now
                };
                if today.to_unix() > now.to_unix() {
                    "今天 18:00".to_string()
                } else {
                    "明天 18:00".to_string()
                }
            }
        }
    }
}

/// 菜单三项的文案（顺序同 [`PRESETS`]）。
pub fn preset_labels(now: DateTime) -> Vec<String> {
    PRESETS.iter().map(|p| p.label(now)).collect()
}

/// `"YYYY-MM-DD HH:MM"`（本地，分钟粒度）。公开给测试（造已到点/未到点的时间串）。
pub fn format_at(t: &DateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

/// 一条待发定时消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledMsg {
    /// 唯一 id（取消时按它匹配）。
    pub id: String,
    /// 触发时刻 `"YYYY-MM-DD HH:MM"`（本地）。
    pub at: String,
    /// 到点要发的文本（就是排程那一刻的草稿）。
    pub text: String,
}

/// `at` 是否已到点（复用 [`crate::schedule::parse_once`]；解析失败 = 永不触发，
/// 不会因为坏数据把整条队列卡死或误发）。
pub fn is_due(at: &str, now: DateTime) -> bool {
    match crate::schedule::parse_once(at) {
        Some(t) => t.to_unix() <= now.to_unix(),
        None => false,
    }
}

/// 菜单里「待发」那一行的文案：`"10-14 18:00 · 前 10 个字…"`。
pub fn menu_row(m: &ScheduledMsg) -> String {
    let when = if m.at.len() >= 16 {
        &m.at[5..16]
    } else {
        m.at.as_str()
    };
    let mut preview: String = m.text.chars().take(10).collect();
    if m.text.chars().count() > 10 {
        preview.push('…');
    }
    format!("{when} · {preview}")
}

/// 定时消息的唯一存储（内存 + `scheduled.json`）。所有访问都在 [`Mutex`] 内，
/// 与生产只有 UI 主线程一个读者/写者，锁本身仍是必要的（`refresh`/`take_due` 也可能来自测试）。
pub struct SchedStore {
    path: PathBuf,
    data: Mutex<Vec<ScheduledMsg>>,
}

impl SchedStore {
    /// 生产入口：`~/.agent-bridge/syschat/scheduled.json`。
    pub fn load() -> SchedStore {
        SchedStore::at(crate::syschat::dir().join("scheduled.json"))
    }

    /// 指定路径构造（测试在隔离根下构造）。
    pub fn at(path: PathBuf) -> SchedStore {
        let data = read_list(&path);
        SchedStore {
            path,
            data: Mutex::new(data),
        }
    }

    /// 当前全部待发（按触发时刻升序，便于菜单展示）。
    pub fn list(&self) -> Vec<ScheduledMsg> {
        let mut v = self.data.lock().unwrap().clone();
        v.sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
        v
    }

    /// 记忆中的待发条数（不做排序）。**测试用**（生产只关心 `list()` 与 `take_due()`）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 排一条。`text` 空 / 全空白、或 `at` 解析不出时间 ⇒ `Err`（**拒绝时不留痕**，
    /// 由单测钉住「拒绝后存储不变」）。返回新建的条目（id 供取消用）。
    pub fn add(&self, text: &str, at: &str) -> Result<ScheduledMsg, String> {
        if text.trim().is_empty() {
            return Err("定时发送需要非空内容".to_string());
        }
        if crate::schedule::parse_once(at).is_none() {
            return Err(format!("无法解析触发时间：{at}"));
        }
        let msg = ScheduledMsg {
            id: uuid::Uuid::new_v4().simple().to_string(),
            at: at.to_string(),
            text: text.to_string(),
        };
        let mut d = self.data.lock().unwrap();
        d.push(msg.clone());
        save(&self.path, &d);
        Ok(msg)
    }

    /// 按 id 取消一条；返回是否真的删掉了（菜单按 index 调用，取不到就是 no-op）。
    pub fn remove(&self, id: &str) -> bool {
        let mut d = self.data.lock().unwrap();
        let before = d.len();
        d.retain(|m| m.id != id);
        let changed = d.len() != before;
        if changed {
            save(&self.path, &d);
        }
        changed
    }

    /// 取消全部；返回取消的条数。
    pub fn remove_all(&self) -> usize {
        let mut d = self.data.lock().unwrap();
        let n = d.len();
        if n > 0 {
            d.clear();
            save(&self.path, &d);
        }
        n
    }

    /// 取出**全部到点**的条目并**同时从存储摘除**（防同一分钟被下一拍重复触发）。
    /// 返回按触发时刻升序的待发列表；调用方负责真正发出（失败只写状态行，不塞回队列，
    /// 避免坏消息反复重试刷屏）。
    pub fn take_due(&self, now: DateTime) -> Vec<ScheduledMsg> {
        let mut d = self.data.lock().unwrap();
        let mut due = Vec::new();
        let mut keep = Vec::with_capacity(d.len());
        for m in d.drain(..) {
            if is_due(&m.at, now) {
                due.push(m);
            } else {
                keep.push(m);
            }
        }
        let changed = !due.is_empty();
        *d = keep;
        if changed {
            save(&self.path, &d);
        }
        drop(d);
        due.sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
        due
    }
}

fn read_list(path: &Path) -> Vec<ScheduledMsg> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<ScheduledMsg>>(&t).ok())
        .unwrap_or_default()
}

/// 原子落盘（tmp + rename），与 `schedule.rs::save_locked` 同款。
fn save(path: &Path, data: &[ScheduledMsg]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(data) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i64, mo: u32, d: u32, h: u32, mi: u32) -> DateTime {
        crate::schedule::parse_once(&format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")).unwrap()
    }

    fn tmp_store(tag: &str) -> (SchedStore, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("abb-sched-{tag}-{}", uuid::Uuid::new_v4().simple()));
        let path = dir.join("scheduled.json");
        (SchedStore::at(path.clone()), path)
    }

    /// 两个加分钟档：解析结果 = now + 偏移，且格式就是 once 任务的分钟粒度串。
    /// 判别力（负向）：把 `5 * 60` 写成 `5`（当秒用）⇒ 断言红。
    #[test]
    fn minute_presets_add_exact_offsets() {
        let now = dt(2026, 10, 14, 9, 30);
        assert_eq!(Preset::In5Min.resolve(now), "2026-10-14 09:35");
        assert_eq!(Preset::In30Min.resolve(now), "2026-10-14 10:00");
        // 跨小时/跨天
        assert_eq!(
            Preset::In30Min.resolve(dt(2026, 10, 14, 23, 45)),
            "2026-10-15 00:15"
        );
    }

    /// 18:00 档：未过 = 今天，已过 = 明天；`label` 与 `resolve` 同一判据。
    #[test]
    fn today18_rolls_to_tomorrow_after_1800() {
        let before = dt(2026, 10, 14, 17, 59);
        assert_eq!(Preset::Today18.resolve(before), "2026-10-14 18:00");
        assert_eq!(Preset::Today18.label(before), "今天 18:00");

        let after = dt(2026, 10, 14, 18, 0);
        assert_eq!(Preset::Today18.resolve(after), "2026-10-15 18:00");
        assert_eq!(Preset::Today18.label(after), "明天 18:00");

        let midnight = dt(2026, 10, 14, 0, 1);
        assert_eq!(Preset::Today18.resolve(midnight), "2026-10-14 18:00");
    }

    /// 任何档位的解析结果都严格晚于 now（防「刚排就发」）。
    #[test]
    fn all_presets_resolve_to_future() {
        for now in [
            dt(2026, 10, 14, 0, 0),
            dt(2026, 10, 14, 18, 0),
            dt(2026, 10, 14, 23, 59),
        ] {
            for p in PRESETS {
                let at = p.resolve(now);
                let t = crate::schedule::parse_once(&at).unwrap();
                assert!(
                    t.to_unix() > now.to_unix(),
                    "{p:?} @ {now:?} -> {at} 不晚于现在"
                );
            }
        }
    }

    #[test]
    fn preset_index_maps_and_rejects() {
        assert_eq!(Preset::from_index(0), Some(Preset::In5Min));
        assert_eq!(Preset::from_index(2), Some(Preset::Today18));
        assert_eq!(Preset::from_index(3), None);
        assert_eq!(Preset::from_index(-1), None);
        assert_eq!(preset_labels(dt(2026, 10, 14, 9, 0)).len(), 3);
    }

    /// `is_due`：到点/过点算 due，未到不算；坏时间串**永不** due（不能误发）。
    #[test]
    fn is_due_boundaries() {
        let now = dt(2026, 10, 14, 9, 30);
        assert!(!is_due("2026-10-14 09:31", now));
        assert!(is_due("2026-10-14 09:30", now));
        assert!(is_due("2026-10-14 09:29", now)); // 错过的补发
        assert!(!is_due("这不是时间", now));
    }

    /// 承重链路：排两条（一条已到点、一条未来）→ `take_due` 只吐到点那条，
    /// 且**同时**把它从存储摘除（第二拍不再吐）；未来那条原样留着。
    /// 判别力（负向）：把 `take_due` 里 `keep.push(m)` 改成 `drain` 后不保留 ⇒ 未来项被吞，本测红。
    #[test]
    fn take_due_fires_and_removes_only_due() {
        let (store, path) = tmp_store("due");
        let now = dt(2026, 10, 14, 9, 30);
        store.add("已到点", "2026-10-14 09:00").unwrap();
        store.add("还没到", "2026-10-14 10:00").unwrap();
        assert_eq!(store.len(), 2);

        let first = store.take_due(now);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].text, "已到点");
        assert_eq!(store.len(), 1);
        assert!(store.take_due(now).is_empty(), "同一拍之后不应重复触发");
        assert_eq!(store.list()[0].text, "还没到");

        // 到点摘除是**落盘**的：从磁盘重开也不复活。
        let reopened = SchedStore::at(path);
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.list()[0].text, "还没到");
    }

    /// 拒绝路径：空文本 / 坏时间 ⇒ Err，且**存储不变**（拒绝必须先于写入）。
    #[test]
    fn add_rejects_invalid_without_mutating() {
        let (store, _) = tmp_store("reject");
        assert!(store.add("   ", "2026-10-14 10:00").is_err());
        assert!(store.add("你好", "明天吧").is_err());
        assert_eq!(store.len(), 0, "被拒绝的排程不得留下任何条目");
        assert!(store.list().is_empty());
    }

    /// 取消：按 id 删单条 / 全删；删不存在的 id 是 no-op 且不写盘。
    #[test]
    fn remove_and_remove_all() {
        let (store, path) = tmp_store("cancel");
        let a = store.add("a", "2026-10-14 10:00").unwrap();
        store.add("b", "2026-10-14 11:00").unwrap();
        assert!(!store.remove("no-such-id"));
        assert_eq!(store.len(), 2);
        assert!(store.remove(&a.id));
        assert_eq!(store.len(), 1);
        assert_eq!(SchedStore::at(path.clone()).list()[0].text, "b");
        assert_eq!(store.remove_all(), 1);
        assert!(store.is_empty());
        assert_eq!(SchedStore::at(path).len(), 0);
    }

    /// 菜单行文案：`MM-DD HH:MM · 预览`，长文本按**字符**截断（不能按字节切中文）。
    #[test]
    fn menu_row_preview_by_chars() {
        let m = ScheduledMsg {
            id: "x".into(),
            at: "2026-10-14 18:00".into(),
            text: "一二三四五六七八九十一二三".into(),
        };
        assert_eq!(menu_row(&m), "10-14 18:00 · 一二三四五六七八九十…");
        let short = ScheduledMsg {
            id: "y".into(),
            at: "2026-10-14 18:00".into(),
            text: "你好".into(),
        };
        assert_eq!(menu_row(&short), "10-14 18:00 · 你好");
    }

    /// 落盘往返：add 之后换一个 store 实例读同一路径能看到条目（真的持久化了）。
    #[test]
    fn persists_across_instances() {
        let (store, path) = tmp_store("persist");
        store.add("持久化", "2026-10-14 20:00").unwrap();
        let reopened = SchedStore::at(path);
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.list()[0].text, "持久化");
        assert_eq!(reopened.list()[0].at, "2026-10-14 20:00");
    }

    /// 空/坏 JSON 文件按空队列处理（不 panic、不把整条功能卡死）。
    #[test]
    fn corrupt_file_reads_as_empty() {
        let dir = std::env::temp_dir().join(format!("abb-sched-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scheduled.json");
        std::fs::write(&path, "{not json").unwrap();
        let store = SchedStore::at(path);
        assert!(store.is_empty());
        // 坏文件之后仍可正常排程（读容错不会传染成写失败）。
        assert!(store.add("x", "2026-10-14 20:00").is_ok());
        assert_eq!(store.len(), 1);
    }
}
