//! CC Switch provider —— 只读查 ~/.cc-switch/cc-switch.db 当前激活的 claude provider，
//! 抽出 settings_config JSON 里 env 的 ANTHROPIC_* 键，注入 claude 子进程 env。
//! （launchd 环境精简，未必带 CC Switch 的代理/auth，必须显式注入。）

use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::path::PathBuf;

fn db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".cc-switch")
        .join("cc-switch.db")
}

/// 读当前激活 claude provider 的 ANTHROPIC_* env。db 不在/无激活行/解析失败 → None。
pub fn active_env() -> Option<HashMap<String, String>> {
    let p = db_path();
    if !p.exists() {
        return None;
    }
    let con = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let settings: String = con
        .query_row(
            "SELECT settings_config FROM providers WHERE app_type='claude' AND is_current=1",
            [],
            |r| r.get(0),
        )
        .ok()?;
    let cfg: serde_json::Value = serde_json::from_str(&settings).ok()?;
    let env = cfg.get("env")?.as_object()?;
    let mut out = HashMap::new();
    for (k, v) in env {
        if k.starts_with("ANTHROPIC_") {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    Some(out)
}
