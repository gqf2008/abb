//! 每日工作目录自动整理（tidy）——bot 配置页 per-bot 开关控制（默认关）。
//!
//! 每天整理 `workspaces/<bot_key>/` 五项内容（全部纯文件操作，绝不出工作区）：
//! 1. **孤儿后端会话文件**：.pi-sessions / .prime-sessions 中不属于任何 live 会话
//!    槽位（sessions.json）且 mtime 超 [`ORPHAN_FRESH_SECS`]（24h——每日节奏下 fresh
//!    文件在检查时最多 ~24h 新，24h 护栏 + live 集双保险，误删风险≈0；/new 的 10 分钟
//!    护栏是即时清理语义，不适用于跨日扫描）。
//! 2. **临时/垃圾文件**：根目录 *.tmp / *.tmp.* / *.swp / *.bak / .DS_Store /
//!    core / core.<纯数字pid>（防 core.md 误删）；只扫根目录（结构目录天然豁免）。
//! 3. **超期历史 jsonl 截断**：history/*.jsonl 按 `history_retention_days`（与
//!    messages.sqlite 的 history-gc 同口径）截断超期行，全过期删文件。
//! 4. **文档归档**：根目录白名单扩展名（md/txt/docx/pdf/…）且 mtime 超 30 天的文件
//!    → archive/YYYY-MM/（按 mtime 所在月分目录）；AGENTS.md/CLAUDE.md/GRANTED.md
//!    与隐藏文件永不归档。
//! 5. **空目录清理**：结构目录（history/ sessions/ summaries/ .pi-sessions/ 等）永不
//!    清理，即使为空。
//!
//! 整理完成后 git 留痕（[`git_commit`]）：存在 .git 复用、无则 init；.gitignore 排除
//! 运行时文件（history/、会话文件、sessions.json 等逐日 churn 的）——删除/截断/归档
//! 都被 `git add -A` 记录，整理痕迹有历史可回退。git 不可用 → 降级纯整理 + 日志警告。
//!
//! 安全：只操作 workspace 内路径，`~/.claude` 等后端私有目录物理不可达；guard 名单
//! 不覆盖 archive/——归档文件从不进入任何 prompt 注入面，无提权路径（受限会话写
//! archive 无害：文件只躺在目录里，不被加载）。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// 孤儿后端会话文件 mtime 护栏（秒）：每日整理用 24h（/new 的 10 分钟是即时清理语义）。
pub const ORPHAN_FRESH_SECS: u64 = 24 * 3600;
/// 文档归档 mtime 阈值（天）：超过即归档。
pub const ARCHIVE_AGE_DAYS: u32 = 30;
/// 结构目录：临时文件扫描 / 空目录清理 / 文档扫描一律跳过（绝不触碰）。
/// attachments 是消息附件落地目录（下载中目录可能瞬时为空——walk 见空与删除之间文件
/// 落地会删掉下载目标；且内含用户聊天媒体，与结构目录同待遇；审查修复）。
const STRUCTURE_DIRS: [&str; 8] = [
    ".git",
    "history",
    "sessions",
    "summaries",
    ".pi-sessions",
    ".prime-sessions",
    "archive",
    "attachments",
];
/// 根目录临时/垃圾文件后缀（小写比较；.tmp.* 形态单独匹配）。
const TEMP_SUFFIXES: [&str; 3] = [".tmp", ".swp", ".bak"];
/// 文档归档扩展名白名单（小写）。
const DOC_EXTS: [&str; 8] = ["md", "txt", "docx", "pdf", "csv", "xlsx", "pptx", "rtf"];
/// 根目录指令文件：永不归档（bot 级指引 / 受限记忆文件，桥会注入它们）。
const PROTECTED_DOCS: [&str; 3] = ["agents.md", "claude.md", "granted.md"];

/// 一轮整理的结果计数（日志汇总用）。
#[derive(Debug, Default, Clone)]
pub struct TidyReport {
    pub temp_removed: usize,
    pub orphan_removed: usize,
    pub history_truncated: usize,
    pub archived: usize,
    pub emptied_dirs: usize,
}

/// git 留痕结果。
#[derive(Debug, Clone, PartialEq)]
pub enum GitOutcome {
    /// 已提交（携带短 hash）。
    Committed(String),
    /// 无变更，不产生空 commit。
    NothingToCommit,
    /// 跳过（git 不可用等），reason 供日志。
    Skipped(String),
}

/// 一轮整理：孤儿会话文件 → 临时文件 → 历史截断 → 文档归档 → 空目录。
/// `now_secs` / `live` 由调用方注入（live = sessions.json 的存活会话 id 集合，
/// 在 service loop 里现取）——保持纯函数可测。
pub fn run_once(
    workspace: &Path,
    now_secs: u64,
    retention_days: u32,
    live: &std::collections::HashSet<String>,
) -> TidyReport {
    // 1. 孤儿后端会话文件（live 集 + mtime 双保险，宁留不删）。
    // 公共判定见 agent::remove_*_transcripts（/new 与 session_gc 同源，护栏时间各异）
    // live 集失效兜底：sessions.json 存在但 live 集为空 = 文件损坏/解析失败（正常态
    // 至少有一个槽位）→ 空 live 集会让 NotInSet 把全部超龄转录当孤儿删掉（含仍存活
    // 但两天未活跃的会话转录——pi 靠它续聊，删后静默丢上下文）。宁留不删：跳过本轮
    // 孤儿清理（审查修复）。「sessions.json 不存在」= 全新工作区，live 空是常态，放行。
    let orphan_removed = if live.is_empty() && workspace.join("sessions.json").exists() {
        crate::log!("[tidy] ⚠️ sessions.json 存在但存活槽位集为空（文件损坏或解析失败？）——本轮跳过孤儿清理，宁留不删");
        0
    } else {
        crate::agent::remove_pi_transcripts(
            workspace,
            live,
            crate::agent::SidMatch::NotInSet,
            Some(ORPHAN_FRESH_SECS),
        ) + crate::agent::remove_prime_transcripts(
            workspace,
            live,
            crate::agent::SidMatch::NotInSet,
            Some(ORPHAN_FRESH_SECS),
        )
    };
    // 2. 根目录临时/垃圾文件
    let mut temp_removed = 0usize;
    for p in collect_temp_files(workspace, now_secs) {
        if std::fs::remove_file(&p).is_ok() {
            temp_removed += 1;
        }
    }
    // 3. 超期历史 jsonl 截断（保留期与 messages.sqlite 的 history-gc 同口径）
    let cutoff = now_secs.saturating_sub(u64::from(retention_days.max(1)) * 86400);
    let history_truncated = crate::history::truncate_stale(&workspace.join("history"), cutoff);
    // 4. 根目录文档归档（先归档再清空目录，归档后源目录可能变空）
    let cutoff_mtime = UNIX_EPOCH
        + std::time::Duration::from_secs(
            now_secs.saturating_sub(u64::from(ARCHIVE_AGE_DAYS) * 86400),
        );
    let items = collect_doc_archives(workspace, cutoff_mtime);
    let archived = archive_files(&items);
    // 5. 空目录清理（自底向上；结构目录永不清理）
    let mut emptied_dirs = 0usize;
    for d in collect_empty_dirs(workspace) {
        if std::fs::remove_dir(&d).is_ok() {
            emptied_dirs += 1;
        }
    }
    TidyReport {
        orphan_removed,
        temp_removed,
        history_truncated,
        archived,
        emptied_dirs,
    }
}

/// 收集根目录临时/垃圾文件（含 .DS_Store、core/core.<纯数字pid>）。
/// 只扫根目录不递归（结构目录内容天然豁免）；符号链接跳过（不删链接目标）。
/// mtime 护栏 [`ORPHAN_FRESH_SECS`]（与孤儿会话同口径，宁留不删）：活着的编辑器
/// 临时文件（.swp/.bak 等）被删会破坏未保存会话；mtime 读不到 → 按保留处理。
fn collect_temp_files(workspace: &Path, now_secs: u64) -> Vec<std::path::PathBuf> {
    let Ok(rd) = std::fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue; // 符号链接跳过
        }
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let core_dump = name == "core"
            || (name.starts_with("core.")
                && name
                    .get(5..)
                    // 后缀必须**非空**且纯数字：空串 all() 恒真——"core."（尾部带点，
                    // 从 NTFS/exFAT 拷贝可得，APFS 建不出）会被判成 core dump 误删（审查修复）
                    .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())));
        let is_temp = core_dump
            || name == ".ds_store"
            || TEMP_SUFFIXES.iter().any(|s| {
                // 结尾形态 *.tmp/*.swp/*.bak 直接命中；`.` 中缀形态仅当后缀之后是纯数字
                //（编辑器崩溃残留 file.tmp.123）——"notes.tmp.md"/"backup.bak.docx" 是
                // 用户文档名，子串匹配会永久误删（审查修复；与 core.<pid> 同规则）。
                name.ends_with(s)
                    || name.find(&format!("{s}.")).is_some_and(|i| {
                        // 后缀必须非空纯数字（同 core.<pid> 规则；"x.tmp." 尾部带点
                        // 空串 all() 恒真会误删——审查修复）
                        let suffix = &name[i + s.len() + 1..];
                        !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
                    })
            });
        if !is_temp {
            continue;
        }
        let old = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()))
            .map(|mtime| now_secs.saturating_sub(mtime) >= ORPHAN_FRESH_SECS)
            .unwrap_or(false);
        if !old {
            continue;
        }
        out.push(path);
    }
    out
}

/// 收集可清理的空目录（自底向上：深度降序）。结构目录子树（history/ sessions/ 等）
/// 永不收集——即使为空也不删（它们是桥的固定布局）。
fn collect_empty_dirs(workspace: &Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(usize, std::path::PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            // 符号链接不跟随：is_dir() 会解引用——外部目录可达，且链接环
            //（a→b→a）会让递归无限下钻直接爆栈
            if std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                continue;
            }
            if !path.is_dir() {
                continue;
            }
            let is_structure = path
                .file_name()
                .map(|n| STRUCTURE_DIRS.contains(&n.to_string_lossy().as_ref()))
                .unwrap_or(false);
            if is_structure {
                continue; // 结构目录子树整体豁免（含 archive/——归档存储）
            }
            out.push((depth, path.clone()));
            walk(&path, depth + 1, out);
        }
    }
    let mut all = Vec::new();
    walk(workspace, 0, &mut all);
    all.sort_by_key(|(d, _)| std::cmp::Reverse(*d)); // 深度降序：先删最深
    all.into_iter().map(|(_, p)| p).collect()
}

/// 收集应归档的根目录文档：(源路径, 目标路径) 对。
/// 判定：扩展名白名单 + mtime 超 cutoff + 非隐藏 + 非 [`PROTECTED_DOCS`]。
fn collect_doc_archives(
    workspace: &Path,
    cutoff_mtime: SystemTime,
) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
    let Ok(rd) = std::fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        // 符号链接跳过（不归档链接目标的内容；与临时文件扫描同口径）
        if std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        if name.starts_with('.') {
            continue; // 隐藏文件不归档（.DS_Store 已由临时文件处理）
        }
        if PROTECTED_DOCS.contains(&lower.as_str()) {
            continue; // 指令/记忆文件永不归档
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !DOC_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t < cutoff_mtime)
            .unwrap_or(false); // mtime 读不到不归档（宁留不删）
        if !old {
            continue;
        }
        // 目标：archive/YYYY-MM/（按 mtime 所在月）。epoch_to_ymd 按固定 UTC+8
        // 口径解释秒数（与 main.rs 全站一致）：UTC 原生 mtime 先平移 +8h 再求年月，
        // 否则 UTC 晚上 20:00-24:00 归档会落进上一个月目录。
        let mtime_unix = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()))
            .unwrap_or(0);
        let (y, mo, _, _, _, _) = crate::chrono_lite::epoch_to_ymd(mtime_unix + 8 * 3600);
        out.push((
            path.clone(),
            workspace
                .join("archive")
                .join(format!("{y:04}-{mo:02}"))
                .join(name),
        ));
    }
    out
}

/// 执行归档（rename；目标重名 → 文件名加 `.`+unix_secs 后缀），返回归档数。
/// rename 失败只 log 不中断（跨卷 rename 失败时文件留在原地，下轮重试）。
fn archive_files(items: &[(std::path::PathBuf, std::path::PathBuf)]) -> usize {
    let mut archived = 0usize;
    for (src, dst) in items {
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // 重名冲突：加 unix 秒后缀（源名可能已在 archive 里）
        let dst = if dst.exists() {
            let now = crate::chrono_lite::unix_secs();
            dst.with_file_name(format!(
                "{}.{now}",
                dst.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default()
            ))
        } else {
            dst.clone()
        };
        match std::fs::rename(src, &dst) {
            Ok(()) => archived += 1,
            Err(e) => crate::log!(
                "[tidy] ⚠️ 归档失败 src={} : {e}",
                trunc(&src.display().to_string())
            ),
        }
    }
    archived
}

/// 日志路径截短（显示尾部，对齐 history.rs trunc_path 口径）。
fn trunc(s: &str) -> String {
    s.chars()
        .rev()
        .take(60)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// git 留痕（独立薄函数，与 tidy 核心解耦）：init/reuse → add -A → 有变更才 commit。
/// 60s 超时保护（大仓库 add 可能慢）；git 不可用返回 [`GitOutcome::Skipped`]，
/// 调用方降级为纯整理 + 日志警告。身份用 `-c` 内联（不触碰全局 git config）。
pub async fn git_commit(workspace: &Path) -> Result<GitOutcome, String> {
    // 每步 git 命令统一 60s 超时 + kill_on_drop：git 挂住（大仓库 add、网络盘）不能
    // 卡死整理循环（spawn_forever 任务，关停要等它退）；超时则杀掉子进程返回跳过。
    async fn git(workspace: &Path, args: &[&str]) -> Result<std::process::Output, String> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args)
            .current_dir(workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output()).await {
            Ok(Ok(o)) => Ok(o),
            Ok(Err(e)) => Err(format!("git {} 启动失败：{e}", args.join(" "))),
            Err(_) => Err(format!("git {} 超时（60s）", args.join(" "))),
        }
    }
    // 1. git 可用性
    match git(workspace, &["--version"]).await {
        Err(r) => return Ok(GitOutcome::Skipped(r)),
        Ok(out) if !out.status.success() => {
            return Ok(GitOutcome::Skipped(format!(
                "git --version 失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
        Ok(_) => {}
    }
    // 2. 无 .git → init（用户已有仓库复用，不重复 init）
    if !workspace.join(".git").exists() {
        match git(workspace, &["init"]).await {
            Err(r) => return Ok(GitOutcome::Skipped(r)),
            Ok(out) if !out.status.success() => {
                return Ok(GitOutcome::Skipped(format!(
                    "git init 失败：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )))
            }
            Ok(_) => {}
        }
    }
    // 2b. 确保运行时文件排除写进 .gitignore——**无论仓库/ignore 是否已存在**：用户
    // 已有 .gitignore 时漏了这些排除，git add -A 会把会话历史/槽位/GRANTED.md 等
    // 运行时文件提交进 git（历史含对话全文、GRANTED.md 含授权者名单——隐私外泄，
    // 审查修复）。幂等：逐行比对，只追加缺失条目，不覆盖用户内容。
    {
        let gi = workspace.join(".gitignore");
        let existing = std::fs::read_to_string(&gi).unwrap_or_default();
        let missing: Vec<&str> = GITIGNORE
            .lines()
            .filter(|l| !l.is_empty() && !existing.lines().any(|e| e.trim() == *l))
            .collect();
        if !missing.is_empty() {
            let mut content = existing;
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!(
                "\n# ABB 运行时文件（每日整理自动追加，勿删）\n{}\n",
                missing.join("\n")
            ));
            if let Err(e) = std::fs::write(&gi, content) {
                // 写失败（只读目录/权限）→ 运行时文件可能被 add -A 提交，降级为不提交
                // 任何内容（隐私优先，宁不留痕不可外泄——审查修复）
                crate::log!("[tidy] ⚠️ 追加 .gitignore 失败（{}）：跳过本轮 git 留痕", e);
                return Ok(GitOutcome::Skipped(
                    "运行时文件排除写入失败，跳过 git 留痕".into(),
                ));
            }
        }
    }
    // 3. add -A（删除/截断/归档全被 stage）
    match git(workspace, &["add", "-A"]).await {
        Err(r) => return Ok(GitOutcome::Skipped(r)),
        Ok(out) if !out.status.success() => {
            return Ok(GitOutcome::Skipped(format!(
                "git add 失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
        Ok(_) => {}
    }
    // 4. 无变更 → 不产生空 commit（读不到/超时 → 保守继续 commit）
    match git(workspace, &["diff", "--cached", "--quiet"]).await {
        Ok(out) if out.status.success() => return Ok(GitOutcome::NothingToCommit),
        _ => {}
    }
    // 5. commit（内联身份，不依赖用户 git config）
    let msg = format!("[abb] 每日整理 {}", crate::chrono_lite::now());
    match git(
        workspace,
        &[
            "-c",
            "user.name=ABB",
            "-c",
            "user.email=abb@agent-bridge.local",
            "commit",
            "-m",
            &msg,
        ],
    )
    .await
    {
        Err(r) => return Ok(GitOutcome::Skipped(r)),
        Ok(out) if !out.status.success() => {
            return Ok(GitOutcome::Skipped(format!(
                "git commit 失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
        Ok(_) => {}
    }
    // 6. 短 hash 回执
    let hash = match git(workspace, &["rev-parse", "--short", "HEAD"]).await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    };
    Ok(GitOutcome::Committed(hash))
}

/// 运行时文件排除（缺失时写入；用户已有 .gitignore 不覆盖）。
const GITIGNORE: &str = "\
.pi-sessions/
.prime-sessions/
history/
sessions/
sessions.json
jobs.json
pending.json
pending_outbox.json
*.tmp
*.swp
*.bak
.DS_Store
GRANTED.md
attachments/
.abb-tidy-last
.abb-session-gc-last
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 唯一 temp workspace（每个测试独立，避免并发互踩）。
    fn temp_ws(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("abb-tidy-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 把文件 mtime 拨旧（模拟不活跃文件；与 bridge /new 测试同款手法）。
    fn set_mtime_old(path: &std::path::Path, secs_ago: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago),
            ),
        )
        .unwrap();
    }

    #[test]
    fn collect_temp_files_finds_patterns_and_skips_others() {
        let ws = temp_ws("temp");
        std::fs::create_dir_all(&ws).unwrap();
        for f in [
            "a.tmp",
            "b.tmp.123",
            "c.swp",
            "d.bak",
            ".DS_Store",
            "core",
            "core.1234",
        ] {
            std::fs::write(ws.join(f), "x").unwrap();
            // 命中的必须是超 24h mtime 护栏的（活着的编辑器临时文件不删）
            set_mtime_old(&ws.join(f), ORPHAN_FRESH_SECS + 60);
        }
        // 不命中的：文档/数据/会话文件/目录；含「后缀 + 扩展名」形态的用户文档——
        // "notes.tmp.md"/"backup.bak.docx" 是合法文档名，子串匹配曾永久误删（审查回归）；
        // 含尾部带点的空后缀形态（"core."/"a.tmp."——NTFS/exFAT 拷贝可得，空串 all()
        // 恒真曾误删，审查回归）
        for f in [
            "core.md",
            "notes.md",
            "data.jsonl",
            "readme.txt",
            "AGENTS.md",
            "notes.tmp.md",
            "backup.bak.docx",
            "report.tmp.txt",
            "a.tmp.notes.md",
            "core.",
            "a.tmp.",
        ] {
            std::fs::write(ws.join(f), "x").unwrap();
            set_mtime_old(&ws.join(f), ORPHAN_FRESH_SECS + 60); // 拨旧：误删只发生在超龄分支
        }
        // 新鲜临时文件不收集（刚生成的垃圾 / 活着的编辑器会话，宁留不删）
        for f in ["fresh.tmp", "fresh.swp", "fresh.bak"] {
            std::fs::write(ws.join(f), "x").unwrap();
        }
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub").join("x.tmp"), "子目录里的临时文件").unwrap();
        // 符号链接跳过
        #[cfg(unix)]
        std::os::unix::fs::symlink(ws.join("data.jsonl"), ws.join("link.tmp")).unwrap();

        let hits = collect_temp_files(&ws, crate::chrono_lite::unix_secs());
        let names: HashSet<String> = hits
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        let want: HashSet<String> = [
            "a.tmp",
            "b.tmp.123",
            "c.swp",
            "d.bak",
            ".DS_Store",
            "core",
            "core.1234",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            names, want,
            "只命中超龄临时/垃圾文件（新鲜的与符号链接跳过）"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn empty_dir_collection_excludes_structure_bottom_up() {
        let ws = temp_ws("empty");
        // 嵌套空目录（应收集并按深度降序）
        std::fs::create_dir_all(ws.join("a/b/c")).unwrap();
        // 结构目录（空也不收）
        for d in STRUCTURE_DIRS {
            std::fs::create_dir_all(ws.join(d)).unwrap();
        }
        // 非空目录不收（删除阶段 remove_dir 失败即跳过——收集阶段只看结构与深度）
        std::fs::create_dir_all(ws.join("full")).unwrap();
        std::fs::write(ws.join("full/f.txt"), "x").unwrap();

        let dirs = collect_empty_dirs(&ws);
        let rel: Vec<String> = dirs
            .iter()
            .map(|p| p.strip_prefix(&ws).unwrap().to_string_lossy().to_string())
            .collect();
        // 深度降序（最深在前；同深度顺序不定——read_dir 顺序不保证）
        let pos = |r: &str| rel.iter().position(|x| x == r).unwrap();
        assert!(
            pos("a/b/c") < pos("a/b") && pos("a/b") < pos("a"),
            "深度降序: {rel:?}"
        );
        assert!(
            rel.iter().any(|r| r == "full"),
            "非空目录也在候选（删除时失败跳过）: {rel:?}"
        );
        for s in STRUCTURE_DIRS {
            assert!(!rel.iter().any(|r| r == s), "{s} 结构目录永不收集: {rel:?}");
        }
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn orphan_transcripts_live_and_fresh_survive() {
        let ws = temp_ws("orphan");
        // pi：<ts>_<sid>.jsonl；live 集含 sid_live
        std::fs::create_dir_all(ws.join(".pi-sessions")).unwrap();
        let p_live = ws.join(".pi-sessions/1000_live_sid.jsonl");
        let p_old = ws.join(".pi-sessions/1000_dead_sid.jsonl");
        let p_fresh = ws.join(".pi-sessions/1000_dead_fresh.jsonl");
        std::fs::write(&p_live, "x").unwrap();
        std::fs::write(&p_old, "x").unwrap();
        std::fs::write(&p_fresh, "x").unwrap();
        // prime：首行 {"id":...}；live 集含 prime_live
        std::fs::create_dir_all(ws.join(".prime-sessions")).unwrap();
        let q_live = ws.join(".prime-sessions/prime_live.jsonl");
        let q_bad = ws.join(".prime-sessions/prime_bad.jsonl");
        std::fs::write(&q_live, "{\"id\":\"prime_live\"}\n").unwrap();
        std::fs::write(&q_bad, "{坏json\n").unwrap();
        // 拨旧：dead_sid 与 prime_bad 拨旧到超 24h 护栏；live 保持新；fresh 拨旧 2h（未超护栏）
        set_mtime_old(&p_old, 30 * 3600);
        set_mtime_old(&q_bad, 30 * 3600);
        set_mtime_old(&p_fresh, 2 * 3600);

        let live: HashSet<String> = ["live_sid".into(), "prime_live".into()]
            .into_iter()
            .collect();
        // 孤儿清理 = 公共判定的 NotInSet 模式（与 /new、session_gc 同源，护栏 24h）
        let removed = crate::agent::remove_pi_transcripts(
            &ws,
            &live,
            crate::agent::SidMatch::NotInSet,
            Some(ORPHAN_FRESH_SECS),
        ) + crate::agent::remove_prime_transcripts(
            &ws,
            &live,
            crate::agent::SidMatch::NotInSet,
            Some(ORPHAN_FRESH_SECS),
        );
        assert_eq!(removed, 2, "只删 dead_sid 与 prime_bad（坏首行）");
        assert!(p_live.exists(), "live 集内不删");
        assert!(!p_old.exists(), "死 sid + 旧 mtime 删");
        assert!(p_fresh.exists(), "死 sid 但 mtime 新鲜不删（护栏）");
        assert!(q_live.exists(), "prime live 不删");
        assert!(!q_bad.exists(), "prime 首行损坏 + 旧 mtime 删");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn archive_docs_moves_old_protects_instructions() {
        let ws = temp_ws("arch");
        let now = crate::chrono_lite::unix_secs();
        // 超 30 天的文档：应归档
        let old_md = ws.join("报告.md");
        let old_pdf = ws.join("手册.pdf");
        std::fs::write(&old_md, "x").unwrap();
        std::fs::write(&old_pdf, "x").unwrap();
        set_mtime_old(&old_md, 40 * 86400);
        set_mtime_old(&old_pdf, 40 * 86400);
        // 豁免：指令/记忆文件（再旧也不归档）
        for f in ["AGENTS.md", "CLAUDE.md", "GRANTED.md"] {
            let p = ws.join(f);
            std::fs::write(&p, "x").unwrap();
            set_mtime_old(&p, 40 * 86400);
        }
        // 其它不归档：新鲜文档 / 非白名单 / 隐藏文件 / 目录
        let fresh_md = ws.join("新文档.md");
        std::fs::write(&fresh_md, "x").unwrap();
        let exe = ws.join("工具.sh");
        std::fs::write(&exe, "x").unwrap();
        set_mtime_old(&exe, 40 * 86400);
        let hidden = ws.join(".secret.md");
        std::fs::write(&hidden, "x").unwrap();
        set_mtime_old(&hidden, 40 * 86400);

        let cutoff = UNIX_EPOCH + std::time::Duration::from_secs(now - 30 * 86400);
        let items = collect_doc_archives(&ws, cutoff);
        assert_eq!(items.len(), 2, "只收两个超期白名单文档");
        let archived = archive_files(&items);
        assert_eq!(archived, 2);
        // 归档目标：archive/YYYY-MM/
        assert!(!old_md.exists(), "源文件已移走");
        assert!(!old_pdf.exists());
        let (y, mo, _, _, _, _) = crate::chrono_lite::epoch_to_ymd((now - 40 * 86400) + 8 * 3600);
        let dest = ws.join("archive").join(format!("{y:04}-{mo:02}"));
        assert!(dest.join("报告.md").exists(), "按 mtime 所在月归档");
        assert!(dest.join("手册.pdf").exists());
        // 豁免项全部还在原处
        for f in [
            "AGENTS.md",
            "CLAUDE.md",
            "GRANTED.md",
            "新文档.md",
            "工具.sh",
            ".secret.md",
        ] {
            assert!(ws.join(f).exists(), "{f} 不应归档");
        }
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn archive_name_collision_appends_timestamp() {
        let ws = temp_ws("collide");
        let now = crate::chrono_lite::unix_secs();
        let src = ws.join("文档.md");
        std::fs::write(&src, "x").unwrap();
        set_mtime_old(&src, 40 * 86400);
        let (y, mo, _, _, _, _) = crate::chrono_lite::epoch_to_ymd((now - 40 * 86400) + 8 * 3600);
        let dest_dir = ws.join("archive").join(format!("{y:04}-{mo:02}"));
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(dest_dir.join("文档.md"), "已存在").unwrap();

        let cutoff = UNIX_EPOCH + std::time::Duration::from_secs(now - 30 * 86400);
        let items = collect_doc_archives(&ws, cutoff);
        assert_eq!(archive_files(&items), 1, "重名也归档");
        assert!(!src.exists(), "源移走");
        let dests = std::fs::read_dir(&dest_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(dests.len(), 2, "原文件 + 带时间戳后缀的归档副本");
        assert!(
            dests.iter().any(|n| n.starts_with("文档.md.")),
            "重名加后缀"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn run_once_full_tidy_roundtrip() {
        let ws = temp_ws("roundtrip");
        let now = crate::chrono_lite::unix_secs();
        std::fs::create_dir_all(ws.join("history")).unwrap();
        std::fs::create_dir_all(ws.join(".pi-sessions")).unwrap();
        // 临时文件（拨旧超 24h mtime 护栏才会被删）
        let tmp = ws.join("a.tmp");
        std::fs::write(&tmp, "x").unwrap();
        set_mtime_old(&tmp, ORPHAN_FRESH_SECS + 60);
        // 孤儿会话（死 sid + 拨旧超 24h 护栏）
        let orphan = ws.join(".pi-sessions/1000_dead.jsonl");
        std::fs::write(&orphan, "x").unwrap();
        set_mtime_old(&orphan, 30 * 3600);
        // 历史：1 条超期 + 1 条新鲜
        let mk = |mid: &str, ts: u64| {
            format!("{{\"mid\":\"{mid}\",\"user\":true,\"backend\":\"claude\",\"text\":\"{mid}\",\"ts\":{ts}}}\n")
        };
        std::fs::write(
            ws.join("history/oc_x.jsonl"),
            format!("{}{}", mk("old", now - 40 * 86400), mk("new", now)),
        )
        .unwrap();
        // 超期文档
        let doc = ws.join("文档.md");
        std::fs::write(&doc, "x").unwrap();
        set_mtime_old(&doc, 40 * 86400);
        // 空目录 + 结构目录
        std::fs::create_dir_all(ws.join("empty/sub")).unwrap();
        std::fs::create_dir_all(ws.join("history")).unwrap();

        let live: HashSet<String> = HashSet::new();
        let report = run_once(&ws, now, 30, &live);
        assert_eq!(report.temp_removed, 1);
        assert_eq!(report.orphan_removed, 1);
        assert_eq!(report.history_truncated, 1, "超期历史行删除");
        assert_eq!(report.archived, 1);
        assert_eq!(report.emptied_dirs, 2, "empty/sub + empty");
        // 终态
        assert!(!ws.join("a.tmp").exists());
        assert!(!orphan.exists());
        let hist = std::fs::read_to_string(ws.join("history/oc_x.jsonl")).unwrap();
        assert!(hist.contains("new") && !hist.contains("old"));
        assert!(!doc.exists());
        assert!(ws.join("history").exists(), "结构目录永不清理");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn git_commit_inits_commits_and_idles() {
        // git 不可用 → 跳过（环境无 git 时提前 return）
        if tokio::process::Command::new("git")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            eprintln!("环境无 git，跳过 git 测试");
            return;
        }
        let ws = temp_ws("git");
        std::fs::create_dir_all(&ws).unwrap();
        // 首轮：init + 归档内容 → commit
        std::fs::write(ws.join("notes.md"), "v1").unwrap();
        match git_commit(&ws).await {
            Ok(GitOutcome::Committed(h)) => {
                assert!(!h.is_empty(), "有短 hash");
            }
            other => panic!("首轮应 commit，实际 {other:?}"),
        }
        assert!(ws.join(".git").exists(), "git init 完成");
        assert!(ws.join(".gitignore").exists(), ".gitignore 写入");
        // 二轮：无变更 → NothingToCommit，commit 数不增
        assert_eq!(
            git_commit(&ws).await.unwrap(),
            GitOutcome::NothingToCommit,
            "无变更不产生空 commit"
        );
        // 三轮：再改内容 → 新 commit（消息前缀 [abb]）
        std::fs::write(ws.join("notes.md"), "v2").unwrap();
        assert!(matches!(
            git_commit(&ws).await.unwrap(),
            GitOutcome::Committed(_)
        ));
        let log = tokio::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&ws)
            .output()
            .await
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout).to_string();
        assert_eq!(log.lines().count(), 2, "两个 commit");
        assert!(log.contains("[abb] 每日整理"), "commit 消息前缀");
        // 运行时文件被 .gitignore 排除：sessions.json 不在仓库
        std::fs::write(ws.join("sessions.json"), "{}").unwrap();
        let tracked = tokio::process::Command::new("git")
            .args(["ls-files"])
            .current_dir(&ws)
            .output()
            .await
            .unwrap();
        let tracked = String::from_utf8_lossy(&tracked.stdout).to_string();
        assert!(!tracked.contains("sessions.json"), "运行时文件被排除");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn run_once_skips_orphans_when_sessions_corrupt() {
        // 审查修复：sessions.json 存在但解析失败（损坏）→ live 集为空 → NotInSet 会把
        // 全部超龄转录当孤儿删掉（含仍存活但两天未活跃的会话转录）。宁留不删：跳过
        // 本轮孤儿清理。sessions.json 不存在（全新工作区）→ live 空是常态，照常清理。
        let ws = temp_ws("corrupt");
        std::fs::create_dir_all(ws.join(".pi-sessions")).unwrap();
        let p = ws.join(".pi-sessions/1000_live_sid.jsonl");
        std::fs::write(&p, "x").unwrap();
        set_mtime_old(&p, 30 * 3600);
        // 损坏的 sessions.json：解析失败 → SessionStore 空数据 → live=∅
        std::fs::write(ws.join("sessions.json"), "{这不是合法json").unwrap();
        let live: HashSet<String> = HashSet::new();
        let report = run_once(&ws, crate::chrono_lite::unix_secs(), 30, &live);
        assert_eq!(
            report.orphan_removed, 0,
            "损坏 sessions.json 时跳过孤儿清理"
        );
        assert!(p.exists(), "存活转录不得被删");
        // 对照：sessions.json 不存在 + live 空 → 孤儿照常清理
        std::fs::remove_file(ws.join("sessions.json")).ok();
        let report = run_once(&ws, crate::chrono_lite::unix_secs(), 30, &live);
        assert_eq!(report.orphan_removed, 1, "无 sessions.json 时孤儿照常清理");
        assert!(!p.exists());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn mtime_unreadable_counts_as_fresh() {
        // 孤儿判定：mtime 读不到 → 宁留不删（unreachable 路径由 unwrap_or(u64::MAX) 覆盖，
        // 此处验证纯逻辑：新鲜文件即使非 live 也保留——回归护栏语义）
        let ws = temp_ws("fresh");
        std::fs::create_dir_all(ws.join(".pi-sessions")).unwrap();
        let fresh = ws.join(".pi-sessions/1000_dead_fresh.jsonl");
        std::fs::write(&fresh, "x").unwrap();
        let live: HashSet<String> = HashSet::new();
        let removed = || {
            crate::agent::remove_pi_transcripts(
                &ws,
                &live,
                crate::agent::SidMatch::NotInSet,
                Some(ORPHAN_FRESH_SECS),
            )
        };
        assert_eq!(removed(), 0, "mtime 未超 24h 护栏不删");
        assert!(fresh.exists());
        // 拨旧超护栏后删（对照）
        set_mtime_old(&fresh, 30 * 3600);
        assert_eq!(removed(), 1);
        std::fs::remove_dir_all(&ws).ok();
    }
}
