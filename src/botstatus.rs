//! 各 bot 运行态上报 —— service 写、GUI 托盘读。
//! 单文件 `logs/bot-status.json`：{ bot_key: {kind, name, conn, ts} }。
//! service 侧各事件循环在状态迁移时 report（在线/重连中/会话过期）；GUI 读它渲染托盘。
//! 单进程多 bot 共享此文件，用进程内 Mutex 串行化写。

use serde_json::{json, Value};
use std::sync::Mutex;

static MU: Mutex<()> = Mutex::new(());

fn path() -> std::path::PathBuf {
    crate::bridge_dir().join("logs").join("bot-status.json")
}

/// 更新某 bot 的状态。conn 由事件循环上报真实连接态："在线"/"重连中"/"会话过期"。
pub fn report(bot_key: &str, kind: &str, name: &str, conn: &str) {
    let _g = MU.lock().unwrap();
    let p = path();
    let mut map: serde_json::Map<String, Value> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    map.insert(
        bot_key.to_string(),
        json!({
            "kind": kind,
            "name": name,
            "conn": conn,
            "ts": crate::chrono_lite::unix_secs(),
        }),
    );
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::atomic_write_text(
        &p,
        &serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_default(),
    );
}

/// 抹掉某 bot（停止时）。
pub fn clear(bot_key: &str) {
    let _g = MU.lock().unwrap();
    let p = path();
    if let Ok(s) = std::fs::read_to_string(&p) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&s) {
            if let Some(obj) = v.as_object_mut() {
                obj.remove(bot_key);
                let _ = crate::atomic_write_text(
                    &p,
                    &serde_json::to_string_pretty(&v).unwrap_or_default(),
                );
            }
        }
    }
}

/// 一个 bot 的运行态快照（GUI 托盘显示用）。
pub struct BotStatus {
    pub kind: String,
    pub name: String,
    pub conn: String,
}

/// GUI 读：返回各存活 bot 的状态，过滤掉超阈值无心跳的僵尸条目（service 已停/崩溃）。
/// 阈值 180s = 2× 最长心跳（飞书 ping 90s）；早先写死的 15s 比心跳还短，会把正常 bot
/// 周期性误判成僵尸、从托盘闪烁消失。
pub fn snapshot() -> Vec<BotStatus> {
    let p = path();
    let now = crate::chrono_lite::unix_secs();
    let s = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let v: Value = match serde_json::from_str(&s) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(obj) = v.as_object() {
        for (_key, e) in obj {
            let ts = e["ts"].as_u64().unwrap_or(0);
            if now.saturating_sub(ts) > 180 {
                continue; // 僵尸条目：超过 2× 最长心跳无更新
            }
            out.push(BotStatus {
                kind: e["kind"].as_str().unwrap_or("").to_string(),
                name: e["name"].as_str().unwrap_or("").to_string(),
                conn: e["conn"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}
