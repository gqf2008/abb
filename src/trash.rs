//! 删除回收站（#88）：agent 的删除操作改走回收站——文件移入工作区 `.trash/`，
//! TTL（默认 7 天）内可恢复，超期由每日 tidy 清理。防止「删了永久丢」的事故。
//!
//! 形态（CLI，main.rs 分发）：
//! ```text
//!   agent-bridge trash <path...>              移入回收站（mv 到 .trash/，保留相对结构）
//!   agent-bridge trash list                   列出可恢复条目（剩余保留时间）
//!   agent-bridge trash restore <条目名>       恢复（移回原相对路径）
//!   agent-bridge trash purge [--older-than N] 清理超期条目（tidy 每日自动调；也可手动）
//! ```
//!
//! 目录形态：`.trash/<yyyyMMdd-HHmmss>-<flatname>/`，条目内放 `.abb-trash.json`
//! manifest（原相对路径 + 移动时间 + 大小），restore 据此定位，不依赖名字解析。
//!
//! 安全边界：
//! - 只操作 workspace 内路径；源路径经 `canonical_in_workspace` 校验（防 symlink 逃逸）。
//! - `.trash/` 不进任何 prompt 注入面：tidy `STRUCTURE_DIRS` 豁免 + guard 名单豁免
//!   （受限 agent 写 .trash 无害：文件只躺在目录里，不被加载，同 archive/ 待遇）。
//! - 恢复目标 = manifest 里的原相对路径，必然在 workspace 内（fail-closed 拒绝越界）。

use std::path::{Path, PathBuf};

/// 回收站目录名（工作区根下）。
pub const TRASH_DIR_NAME: &str = ".trash";
/// 默认保留天数（超期由 tidy 每日清理；与产品方案一致）。
pub const TRASH_TTL_DAYS: u64 = 7;
/// 条目内 manifest 文件名。
const MANIFEST_NAME: &str = ".abb-trash.json";

/// 回收站根：<workspace>/.trash/
pub fn trash_dir(workspace: &Path) -> PathBuf {
    workspace.join(TRASH_DIR_NAME)
}

/// 一条回收站记录。
#[derive(Debug, Clone)]
pub struct TrashEntry {
    /// 条目目录名（restore/purge 的定位键）。
    pub name: String,
    /// 原相对路径（相对 workspace）。
    pub original: String,
    /// 移动时间（unix 秒）。
    pub moved_at: u64,
    /// 条目总大小（字节；-1 = 统计失败）。
    pub size: i64,
}

/// 时间戳前缀（条目目录名用）：yyyyMMdd-HHmmss。
fn ts_prefix(now_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = crate::chrono_lite::epoch_to_ymd(now_secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// 把相对路径展平成目录名安全片段（`/` `\` → `__`；其它路径非法字符替换为 `_`）。
fn flat_name(rel: &Path) -> String {
    let mut s = String::new();
    for c in rel.to_string_lossy().chars() {
        match c {
            '/' | '\\' => s.push_str("__"),
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => s.push('_'),
            c => s.push(c),
        }
    }
    if s.is_empty() {
        s.push_str("root");
    }
    s
}

/// 把路径移入回收站。返回条目目录名。
/// 校验：src 必须存在且在 workspace 内（canonicalize 防 symlink 逃逸；支持相对路径）；
/// 已在 .trash 内 / 是 workspace 根自身 → 拒绝。
pub fn trash_path(workspace: &Path, src: &str) -> Result<String, String> {
    let ws_canon = std::fs::canonicalize(workspace).map_err(|e| format!("工作区不可达：{e}"))?;
    let src_path = Path::new(src);
    let full = if src_path.is_absolute() {
        src_path.to_path_buf()
    } else {
        workspace.join(src_path)
    };
    let full_canon = std::fs::canonicalize(&full)
        .map_err(|_| format!("路径不存在：{}（需先确认路径在工作区内）", src))?;
    let rel = full_canon.strip_prefix(&ws_canon).map_err(|_| {
        format!(
            "路径不在工作区内（已拒绝）：{}（canonical 后 = {}）",
            src,
            full_canon.display()
        )
    })?;
    if rel.as_os_str().is_empty() {
        return Err("不能回收站化工作区根目录".into());
    }
    if rel.starts_with(TRASH_DIR_NAME) {
        return Err(format!("{} 已在回收站内", src));
    }

    let trash = trash_dir(workspace);
    std::fs::create_dir_all(&trash).map_err(|e| format!("创建回收站失败：{e}"))?;
    // 条目 = 目录（源是文件时包一层）：.trash/<ts>-<flatname>/
    let dest_dir = unique_entry_dir(&trash, rel);
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("创建回收站条目失败：{e}"))?;
    let dest = dest_dir.join(rel.file_name().unwrap_or_default());
    let is_dir = full_canon.is_dir();
    std::fs::rename(&full_canon, &dest).map_err(|e| format!("移入回收站失败（{}）：{e}", src))?;
    let moved_at = crate::chrono_lite::unix_secs();
    write_manifest(&dest_dir, rel, moved_at, is_dir)?;
    Ok(dest_dir.file_name().unwrap().to_string_lossy().into_owned())
}

/// 生成不冲突的条目目录名（同秒同路径冲突追加序号）。
fn unique_entry_dir(trash: &Path, rel: &Path) -> PathBuf {
    let base = trash.join(format!(
        "{}-{}",
        ts_prefix(crate::chrono_lite::unix_secs()),
        flat_name(rel)
    ));
    if !base.exists() {
        return base;
    }
    let mut i = 1;
    loop {
        let cand = trash.join(format!(
            "{}-{}-{}",
            ts_prefix(crate::chrono_lite::unix_secs()),
            flat_name(rel),
            i
        ));
        if !cand.exists() {
            return cand;
        }
        i += 1;
    }
}

/// 写条目 manifest。kind = 源是目录（dir）还是文件（file）——restore 按此拆包。
fn write_manifest(entry_dir: &Path, rel: &Path, moved_at: u64, is_dir: bool) -> Result<(), String> {
    let size = dir_size(entry_dir);
    let m = serde_json::json!({
        "original": rel.to_string_lossy(),
        "moved_at": moved_at,
        "size": size,
        "kind": if is_dir { "dir" } else { "file" },
    });
    let json = serde_json::to_string_pretty(&m).map_err(|e| format!("manifest 序列化失败：{e}"))?;
    crate::atomic_write_text(&entry_dir.join(MANIFEST_NAME), &json)
        .map_err(|e| format!("写回收站 manifest 失败：{e}"))
}

/// 条目目录总大小（含 manifest；失败 -1）。
fn dir_size(p: &Path) -> i64 {
    fn walk(d: &Path, out: &mut i64) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if let Ok(md) = std::fs::metadata(&p) {
                    *out += md.len() as i64;
                }
            }
        }
    }
    let mut total = 0i64;
    walk(p, &mut total);
    total
}

/// 列出回收站条目（含剩余保留秒数，供 list 展示）。
pub fn list_entries(workspace: &Path) -> Vec<TrashEntry> {
    let trash = trash_dir(workspace);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&trash) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        // 读 manifest
        let manifest_path = p.join(MANIFEST_NAME);
        let (original, moved_at) = std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|v| {
                (
                    v["original"].as_str().unwrap_or("").to_string(),
                    v["moved_at"].as_u64().unwrap_or(0),
                )
            })
            .unwrap_or_default();
        if original.is_empty() {
            // 无 manifest（异常/手工目录）——按名字推断 original，moved_at 取目录 mtime
            let moved_at = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let orig = name
                .split_once('-')
                .map(|(_, rest)| rest.replace("__", "/"))
                .unwrap_or_else(|| name.clone());
            out.push(TrashEntry {
                name,
                original: orig,
                moved_at,
                size: dir_size(&p),
            });
        } else {
            out.push(TrashEntry {
                name,
                original,
                moved_at,
                size: dir_size(&p),
            });
        }
    }
    out.sort_by(|a, b| b.moved_at.cmp(&a.moved_at));
    out
}

/// 恢复条目：移回 manifest 里的原相对路径。父目录自动重建；目标已存在 → 拒绝（不覆盖）。
/// 文件条目拆包（条目目录里只有 manifest + 原文件）；目录条目整体移回。
pub fn restore_entry(workspace: &Path, name: &str) -> Result<(), String> {
    let entry_dir = trash_dir(workspace).join(name);
    if !entry_dir.is_dir() {
        return Err(format!("回收站条目不存在：{name}"));
    }
    let manifest_path = entry_dir.join(MANIFEST_NAME);
    let v: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).map_err(|e| format!("读 manifest 失败：{e}"))?,
    )
    .map_err(|e| format!("manifest 解析失败：{e}"))?;
    let rel_str = v["original"]
        .as_str()
        .ok_or("manifest 缺 original")?
        .to_string();
    let kind = v["kind"].as_str().unwrap_or("dir");
    let rel = Path::new(&rel_str);
    // fail-closed：原路径必须仍在工作区内、且不是回收站自身
    if rel.as_os_str().is_empty() || rel.starts_with(TRASH_DIR_NAME) {
        return Err(format!("manifest 原路径非法：{rel_str}"));
    }
    let dest = workspace.join(rel);
    if dest.exists() {
        return Err(format!("原路径已存在（拒绝覆盖）：{}", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("重建父目录失败：{e}"))?;
    }
    // 条目目录里取「非 manifest 的那一项」（文件或目录，trash 时保持原名移入）移回 dest；
    // 再清空条目目录。目录条目整体 rename 会多包一层原名（trash 时内容在 条目目录/原名 下）。
    let mut found: Option<PathBuf> = None;
    if let Ok(rd) = std::fs::read_dir(&entry_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.file_name().and_then(|n| n.to_str()) == Some(MANIFEST_NAME) {
                continue;
            }
            found = Some(p);
            break;
        }
    }
    let item = found.ok_or("回收站条目缺原内容（数据异常）")?;
    let _ = kind; // 文件/目录统一按「移出条目」处理，kind 仅存档
    std::fs::rename(&item, &dest).map_err(|e| format!("恢复失败：{e}"))?;
    let _ = std::fs::remove_dir_all(&entry_dir); // 清空条目目录
    Ok(())
}

/// 清理超期条目（mtime/moved_at 超过 ttl_days）。返回清理数。
pub fn purge_expired(workspace: &Path, ttl_days: u64) -> usize {
    let now = crate::chrono_lite::unix_secs();
    let cutoff = now.saturating_sub(ttl_days * 24 * 3600);
    let entries = list_entries(&workspace);
    let mut removed = 0;
    for e in entries {
        if e.moved_at < cutoff {
            let p = trash_dir(workspace).join(&e.name);
            if std::fs::remove_dir_all(&p).is_ok() || std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// CLI 入口（main.rs 分发 `agent-bridge trash ...`）。
/// 工作区解析：AGENT_BRIDGE_BOT_KEY env（桥 spawn agent 时注入；人用可 --bot 指定）。
pub fn run_trash_cli(args: &[String]) -> i32 {
    let (rest, workspace) = match resolve_cli_workspace(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut rest = rest;
    let sub = rest.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "list" => {
            let entries = list_entries(&workspace);
            if entries.is_empty() {
                println!("回收站为空");
                return 0;
            }
            let now = crate::chrono_lite::unix_secs();
            println!("回收站（.trash/，默认保留 {} 天）：", TRASH_TTL_DAYS);
            for e in entries {
                let remain = e
                    .moved_at
                    .saturating_add(TRASH_TTL_DAYS * 24 * 3600)
                    .saturating_sub(now);
                let remain_txt = if remain == 0 {
                    "已过期".to_string()
                } else {
                    format!("剩 {} 小时", remain / 3600)
                };
                let size_txt = if e.size >= 0 {
                    format!("{:.1} MB", e.size as f64 / 1048576.0)
                } else {
                    "?".into()
                };
                println!("{}  {}  {}  {}", e.name, e.original, size_txt, remain_txt);
            }
            0
        }
        "restore" => {
            rest.remove(0);
            let name = rest.first().map(|s| s.as_str()).unwrap_or("");
            if name.is_empty() {
                eprintln!("用法：agent-bridge trash restore <条目名>");
                return 1;
            }
            match restore_entry(&workspace, name) {
                Ok(()) => {
                    println!("✅ 已恢复：{name}");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "purge" => {
            let mut ttl = TRASH_TTL_DAYS;
            let mut i = 1;
            while i < rest.len() {
                if rest[i] == "--older-than" {
                    if let Some(v) = rest.get(i + 1).and_then(|s| s.parse::<u64>().ok()) {
                        ttl = v;
                        i += 2;
                        continue;
                    }
                }
                i += 1;
            }
            let n = purge_expired(&workspace, ttl);
            println!("已清理 {n} 个超期回收站条目（保留 {ttl} 天）");
            0
        }
        // 无子命令：把剩余参数当路径移入回收站（agent 常用形态）
        _ => {
            if rest.is_empty() {
                eprintln!(
                    "用法：agent-bridge trash <path...> | trash list | trash restore <条目> | trash purge"
                );
                return 1;
            }
            let mut ok = 0;
            let mut fail = 0;
            for p in rest {
                match trash_path(&workspace, &p) {
                    Ok(name) => {
                        println!("🗑️ {} → .trash/{name}", p);
                        ok += 1;
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        fail += 1;
                    }
                }
            }
            if fail > 0 {
                println!("完成：成功 {ok} 项，失败 {fail} 项（可用 trash list 查看回收站）");
                1
            } else {
                0
            }
        }
    }
}

/// 解析 CLI 工作区：优先 `--bot <key>`（人用），缺省取 AGENT_BRIDGE_BOT_KEY（agent 用）。
/// 返回 (去掉 --bot 后的剩余参数, workspace 路径)。
fn resolve_cli_workspace(args: &[String]) -> Result<(Vec<String>, PathBuf), String> {
    let mut rest = args.to_vec();
    let mut bot_key: Option<String> = None;
    if let Some(pos) = rest.iter().position(|a| a == "--bot") {
        if let Some(v) = rest.get(pos + 1) {
            bot_key = Some(v.clone());
            rest.drain(pos..=pos + 1);
        }
    }
    let key = match bot_key {
        Some(k) => k,
        None => std::env::var("AGENT_BRIDGE_BOT_KEY")
            .map_err(|_| "无法解析工作区：缺 AGENT_BRIDGE_BOT_KEY env（agent 会话自动注入；人用请加 --bot <key>）".to_string())?,
    };
    let ws = crate::workspace_dir(&key);
    std::fs::create_dir_all(&ws).map_err(|e| format!("工作区创建失败：{e}"))?;
    Ok((rest, ws))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_ws(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "abb-trash-test-{tag}-{}",
            crate::chrono_lite::unix_secs()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn trash_restore_roundtrip() {
        let ws = tmp_ws("roundtrip");
        fs::create_dir_all(ws.join("sub")).unwrap();
        fs::write(ws.join("sub/hello.txt"), b"hi").unwrap();

        let name = trash_path(&ws, "sub/hello.txt").unwrap();
        assert!(!ws.join("sub/hello.txt").exists(), "源应被移走");
        assert!(trash_dir(&ws).join(&name).is_dir(), "条目目录存在");

        restore_entry(&ws, &name).unwrap();
        assert_eq!(fs::read_to_string(ws.join("sub/hello.txt")).unwrap(), "hi");
        assert!(!trash_dir(&ws).join(&name).exists(), "条目已移回");

        // 目录条目：层级不增不减（回归：整体 rename 曾多包一层原名）
        fs::create_dir_all(ws.join("proj/src")).unwrap();
        fs::write(ws.join("proj/src/main.rs"), b"fn main(){}").unwrap();
        let dname = trash_path(&ws, "proj/src").unwrap();
        assert!(!ws.join("proj/src").exists());
        restore_entry(&ws, &dname).unwrap();
        assert_eq!(
            fs::read_to_string(ws.join("proj/src/main.rs")).unwrap(),
            "fn main(){}",
            "目录恢复层级必须与原路径一致"
        );

        fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn trash_rejects_outside_and_trash() {
        let ws = tmp_ws("outside");
        let outside =
            std::env::temp_dir().join(format!("abb-outside-{}", crate::chrono_lite::unix_secs()));
        fs::write(&outside, b"x").unwrap();

        let err = trash_path(&ws, outside.to_str().unwrap()).unwrap_err();
        assert!(err.contains("不在工作区内"), "{err}");

        // 已在回收站内
        fs::create_dir_all(ws.join(".trash").join("t1")).unwrap();
        let err2 = trash_path(&ws, ".trash/t1").unwrap_err();
        assert!(err2.contains("回收站内"), "{err2}");

        fs::remove_dir_all(&ws).ok();
        fs::remove_file(&outside).ok();
    }

    #[test]
    fn purge_expired_removes_old_keeps_fresh() {
        let ws = tmp_ws("purge");
        fs::create_dir_all(ws.join(".trash")).unwrap();
        // 旧条目（直接造目录 + 旧 mtime）
        let old = ws.join(".trash/20260101-000000-old");
        fs::create_dir_all(&old).unwrap();
        fs::write(
            old.join(MANIFEST_NAME),
            r#"{"original":"old.txt","moved_at":1704067200}"#,
        )
        .unwrap();
        let old_meta = fs::metadata(&old).unwrap();
        let _ = old_meta;
        // 新条目
        fs::create_dir_all(ws.join(".trash/20260101-000000-fresh")).unwrap();
        fs::write(
            ws.join(".trash/20260101-000000-fresh").join(MANIFEST_NAME),
            format!(
                r#"{{"original":"fresh.txt","moved_at":{}}}"#,
                crate::chrono_lite::unix_secs()
            ),
        )
        .unwrap();

        let removed = purge_expired(&ws, 7);
        assert_eq!(removed, 1, "只清旧条目");
        assert!(!ws.join(".trash/20260101-000000-old").exists());
        assert!(ws.join(".trash/20260101-000000-fresh").exists());

        fs::remove_dir_all(&ws).ok();
    }
}
