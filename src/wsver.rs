//! #209 工作区版本管理：内置 libgit2（`git2` crate，进程内操作，不依赖系统 git、
//! 不依赖 PATH 解析）。为每个 bot 工作区提供默认开启的版本管理，作为删除保护
//! （#88）的底座。
//!
//! - [`ensure_repo`]：幂等确保工作区是可用 git 仓库（init + .gitignore 合并 + 基线
//!   commit + 存量运行时文件去 track）
//! - [`snapshot`]：add -A + 有变更才 commit——删除发生前的内容留在快照里，删除即有恢复点
//! - [`snapshot_lazy`]：ensure + snapshot 组合（工作区可能从未 init，trash 留痕入口用）
//! - [`log`] / [`restore_path`]：快照历史与按快照恢复（批次 4，CLI 起步）
//! - [`repo_status`]：保护状态一览（批次 5）
//!
//! 失败降级原则：版本管理是增强层，任何失败由调用方决定降级（启动 init 失败仅告警、
//! 删除留痕失败不阻断删除保护），本模块不 log 不 panic。作者身份固定内联
//! `ABB <abb@agent-bridge.local>`，不读不写全局 git config。
//!
//! 并发（审查确认，best-effort 语义内可接受）：bot 启动 init 与 guard hook 子进程
//! 的 trash 快照可能并发操作同一工作区——index 写入走 index.lock，撞锁一方干净
//! 报 Err 走降级，无索引损坏；ref 更新无 CAS，双 commit 竞态最坏孤儿化一个 commit
//! （对象仍可达，git fsck 可恢复）。

use git2::{IndexAddOption, Repository, RepositoryInitOptions, Signature, Sort};
use std::path::Path;

/// 内置提交身份（与 #88 时代 tidy 内联 `-c user.name/user.email` 同值）。
const ABB_NAME: &str = "ABB";
const ABB_EMAIL: &str = "abb@agent-bridge.local";

/// 运行时文件排除清单（缺失时写入；用户已有 .gitignore 不覆盖）。单一事实源：
/// tidy.rs（每日整理留痕）与本模块（启动 init / 删除快照）共用，防两处清单漂移。
///
/// 隐私相关条目（history/、sessions*、summaries/、GRANTED.md、attachments/）必须
/// 保留：会话历史/归纳摘要含对话全文（summaries/ 为 #130 session_gc 产物）、
/// GRANTED.md 含授权者名单，git add -A 绝不能把它们提交进仓库（#88 审查结论：
/// 历史外泄）。churn 条目（context_tokens.json / agent-pids.json / .abb-*-last /
/// *.tmp 等）防高频变更击穿「无变更不空 commit」快路径。
///
/// 注意：ignore 只对未跟踪文件生效——存量 tidy 仓库若曾把上述文件提交过，需
/// `git rm --cached` 去 track（#209 批次 3 tidy 迁移时一并处理）。
pub(crate) const GITIGNORE: &str = "\
.pi-sessions/
history/
sessions/
sessions.json
summaries/
context_tokens.json
agent-pids.json
jobs.json
pending.json
pending_outbox.json
*.tmp
*.swp
*.bak
.DS_Store
GRANTED.md
attachments/
.trash/
.abb-tidy-last
.abb-session-gc-last
.abb-trash-gc-last
";

/// libgit2 错误统一转 String（message 含根因，class 略去——调用方只做日志/降级）。
fn g2r<T>(r: Result<T, git2::Error>) -> Result<T, String> {
    r.map_err(|e| format!("libgit2: {}", e.message()))
}

/// 幂等合并 `.gitignore`：[`GITIGNORE`] 中工作区尚缺的行才追加，已有内容（含用户
/// 自定义排除）绝不覆盖。返回追加行数（0 = 无需变更）。写失败 → Err（隐私优先：
/// tidy 据此跳过整轮留痕，运行时文件宁可不提交也不外泄）。
pub(crate) fn merge_gitignore(workspace: &Path) -> std::io::Result<usize> {
    let gi = workspace.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    let missing: Vec<&str> = GITIGNORE
        .lines()
        .filter(|l| !l.is_empty() && !existing.lines().any(|e| e.trim() == *l))
        .collect();
    if missing.is_empty() {
        return Ok(0);
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!(
        "\n# ABB 运行时文件（自动追加，勿删）\n{}\n",
        missing.join("\n")
    ));
    // 原子写（项目约定，同 trash manifest）：崩溃不残留半截 .gitignore
    crate::atomic_write_text(&gi, &content)?;
    Ok(missing.len())
}

/// 幂等确保工作区是可用 git 仓库：
///
/// - 无 `.git` → init（默认分支 `main`，父目录不存在则创建）+ .gitignore + 基线
///   commit（空仓也打，作为版本起点；工作区为空时是空树 commit）
/// - 已有 `.git` → 补 .gitignore 缺失行；ABB 自建历史（HEAD 作者为内置身份）的
///   存量仓库顺带去 track 命中 ignore 清单的运行时文件（批次 3 清理，文件不动只出
///   索引）；用户自有仓库其余部分一概不动
///
/// bot 启动时（service.rs）与快照前（[`snapshot_lazy`]）都会调用，重复调用零副作用。
pub fn ensure_repo(workspace: &Path) -> Result<(), String> {
    if g2r(Repository::open(workspace)).is_err() {
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("main").mkpath(true);
        g2r(Repository::init_opts(workspace, &opts))?;
        merge_gitignore(workspace).map_err(|e| format!(".gitignore 合并失败：{e}"))?;
        return snapshot_impl(workspace, "工作区基线", true).map(|_| ());
    }
    merge_gitignore(workspace).map_err(|e| format!(".gitignore 合并失败：{e}"))?;
    let repo = g2r(Repository::open(workspace))?;
    if is_abb_managed(&repo) {
        let n = untrack_ignored(&repo)?;
        if n > 0 {
            snapshot(workspace, "存量运行时文件去 track")?;
        }
    }
    Ok(())
}

/// 仓库是否由 ABB 管理（HEAD 作者邮箱为内置身份）——存量清理只动这类仓库；
/// 用户自建/自有身份的仓库绝不改写索引（即使是用户自己 track 了运行时文件，
/// 那是用户对自己仓库的选择）。
fn is_abb_managed(repo: &Repository) -> bool {
    repo.head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| c.author().email().ok() == Some(ABB_EMAIL))
        .unwrap_or(false)
}

/// 存量清理（批次 3）：把「已被 track、现在命中 .gitignore」的文件从索引移除
/// （`git rm --cached` 语义：工作区文件不动，快照记录去 track）。调用方保证只在
/// [`is_abb_managed`] 仓库上调用。返回去 track 条数（0 = 干净）。
fn untrack_ignored(repo: &Repository) -> Result<usize, String> {
    let mut index = g2r(repo.index())?;
    let mut victims: Vec<std::path::PathBuf> = Vec::new();
    for i in 0..index.len() {
        let Some(e) = index.get(i) else { continue };
        // IndexEntry.path 字段（Vec<u8>，git 内部恒 UTF-8）→ lossy 转路径
        let p = std::path::PathBuf::from(String::from_utf8_lossy(&e.path).into_owned());
        // 单条判定失败保守保留（宁多 track 不误删用户索引）
        if repo.is_path_ignored(&p).unwrap_or(false) {
            victims.push(p);
        }
    }
    if victims.is_empty() {
        return Ok(0);
    }
    // git_index_remove_all = `git rm --cached`：仅移出索引，工作区文件不动
    g2r(index.remove_all(&victims, None))?;
    g2r(index.write())?;
    Ok(victims.len())
}

/// add -A + 有变更才 commit。返回 `Some(短 hash)` = 产生了新 commit（恢复点），
/// `None` = 无变更不空 commit。
pub fn snapshot(workspace: &Path, reason: &str) -> Result<Option<String>, String> {
    snapshot_impl(workspace, reason, false)
}

/// 删除保护入口：确保**当前工作区状态**有对应 commit，返回承载它的恢复点短 hash。
/// 恢复点可能来自本次快照，也可能来自刚打的基线（快照随即 no-op）——两者都完整
/// 含有删除前的内容；仅 HEAD 不可读（异常）时返回 None。
pub fn snapshot_lazy(workspace: &Path, reason: &str) -> Result<Option<String>, String> {
    ensure_repo(workspace)?;
    match snapshot(workspace, reason)? {
        Some(h) => Ok(Some(h)),
        None => {
            let Ok(repo) = Repository::open(workspace) else {
                return Ok(None);
            };
            Ok(repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_commit().ok())
                .map(|c| short(&c.id())))
        }
    }
}

/// 快照核心：stage 全部变更（含删除），与 HEAD 有差异才 commit。
///
/// - unborn HEAD（新仓库/基线）→ 无父 commit，直接 commit（`allow_empty` 决定空树
///   是否落 commit；基线=true，日常快照=false）
/// - 有 HEAD 且工作树无差异 → 不产生空 commit（返回 None）
fn snapshot_impl(
    workspace: &Path,
    reason: &str,
    allow_empty: bool,
) -> Result<Option<String>, String> {
    let repo = g2r(Repository::open(workspace))?;
    let mut index = g2r(repo.index())?;
    // add_all = git add -A：新增/修改/删除全部 stage，遵循 .gitignore（.trash/、
    // history/ 等运行时文件不入库）
    g2r(index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None))?;
    g2r(index.write())?;
    let tree_id = g2r(index.write_tree())?;
    let tree = g2r(repo.find_tree(tree_id))?;
    let sig = g2r(Signature::now(ABB_NAME, ABB_EMAIL))?;
    let msg = format!("[abb] {reason} {}", crate::chrono_lite::now());
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let oid = match &parent {
        Some(p) => {
            let parent_tree = g2r(p.tree())?;
            let diff = g2r(repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None))?;
            if diff.deltas().count() == 0 && !allow_empty {
                return Ok(None);
            }
            g2r(repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &[p]))?
        }
        None => g2r(repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &[]))?,
    };
    Ok(Some(short(&oid)))
}

/// Oid 短 hash（7 位，与 `git log --oneline` 同宽度）。
fn short(oid: &git2::Oid) -> String {
    oid.to_string()[..7].to_string()
}

/// 快照历史（批次 4）：最近 `limit` 条，新→旧，`(短 hash, UTC+8 时间, 首行消息)`。
pub fn log(workspace: &Path, limit: usize) -> Result<Vec<(String, String, String)>, String> {
    let repo = g2r(Repository::open(workspace))?;
    let mut revwalk = g2r(repo.revwalk())?;
    g2r(revwalk.push_head())?;
    // TOPOLOGICAL：子先于父——线性历史严格新→旧（纯 TIME 在同秒 commit 上顺序不稳定）
    g2r(revwalk.set_sorting(Sort::TOPOLOGICAL))?;
    let mut out = Vec::new();
    for oid in revwalk.take(limit) {
        let c = g2r(repo.find_commit(g2r(oid)?))?;
        // 提交时间 UTC → UTC+8 展示（与 chrono_lite::now() 同口径）
        let (y, mo, d, h, mi, s) =
            crate::chrono_lite::epoch_to_ymd(c.time().seconds().max(0) as u64 + 8 * 3600);
        out.push((
            short(&c.id()),
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
            c.summary().ok().flatten().unwrap_or("(无消息)").to_string(),
        ));
    }
    Ok(out)
}

/// 按快照恢复路径（批次 4）：把 `<rev>` 快照里的文件/目录写回工作区（覆盖现文件，
/// 绝不删除恢复点没有的多余文件——恢复是"取回"，不是"回滚"）。
/// rev/路径验证通过后自动打「恢复前快照」防套娃（当前状态先入库，恢复错了还能
/// 退回来）；快照失败（如撞锁）即中止恢复——保护优先。返回写回的文件数。
pub fn restore_path(workspace: &Path, rev: &str, rel: &str) -> Result<usize, String> {
    // 路径安全：须为工作区内相对路径（rev/rel 来自 CLI 参数，不可信）
    let p = Path::new(rel);
    if p.is_absolute() || rel.trim().is_empty() {
        return Err(format!("非法路径（须为工作区内相对路径）：{rel}"));
    }
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err(format!("非法路径（不允许 .. 上跳）：{rel}"));
    }
    let repo = g2r(Repository::open(workspace))?;
    // 先解析验证（rev/路径都有效才动仓库），再打「恢复前快照」，最后写回——
    // 坏请求不产生垃圾快照。
    let commit = g2r(g2r(repo.revparse_single(rev))?.peel_to_commit())?;
    let tree = g2r(commit.tree())?;
    let entry = tree
        .get_path(p)
        .map_err(|_| format!("快照 {rev} 中无此路径：{rel}"))?;
    let obj = g2r(entry.to_object(&repo))?;
    snapshot(workspace, "恢复前快照")?;
    let target = workspace.join(p);
    match entry.kind() {
        Some(git2::ObjectType::Blob) => {
            let blob = g2r(obj.peel_to_blob())?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("重建父目录失败：{e}"))?;
            }
            std::fs::write(&target, blob.content()).map_err(|e| format!("写回失败：{e}"))?;
            apply_filemode(&target, entry.filemode());
            Ok(1)
        }
        Some(git2::ObjectType::Tree) => {
            let t = g2r(obj.peel_to_tree())?;
            write_tree_to(&repo, &t, &target)
        }
        _ => Err(format!("不支持的条目类型（仅文件/目录）：{rel}")),
    }
}

/// 递归写回子树。返回写回文件数（子模块等特殊条目跳过）。
fn write_tree_to(repo: &Repository, tree: &git2::Tree, dir: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
    let mut n = 0;
    for e in tree.iter() {
        let name = g2r(e.name())?; // 非 UTF-8 文件名 → libgit2 错误（暂不支持）
        let obj = g2r(e.to_object(repo))?;
        let dst = dir.join(name);
        match e.kind() {
            Some(git2::ObjectType::Blob) => {
                let blob = g2r(obj.peel_to_blob())?;
                std::fs::write(&dst, blob.content())
                    .map_err(|e| format!("写回 {name} 失败：{e}"))?;
                apply_filemode(&dst, e.filemode());
                n += 1;
            }
            Some(git2::ObjectType::Tree) => {
                n += write_tree_to(repo, &g2r(obj.peel_to_tree())?, &dst)?;
            }
            _ => {}
        }
    }
    Ok(n)
}

/// 恢复可执行位（git filemode 0o111 位）；Windows 无 POSIX 权限，no-op。
#[allow(unused_variables)]
fn apply_filemode(path: &Path, filemode: i32) {
    #[cfg(unix)]
    {
        if filemode & 0o111 != 0 {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        }
    }
}

/// 保护状态一览（批次 5）：仓库存在性 + HEAD 恢复点 + 快照总数。
pub struct RepoStatus {
    pub has_repo: bool,
    /// HEAD 短 hash（无 repo / unborn = None）。
    pub head: Option<String>,
    pub commit_count: usize,
}

pub fn repo_status(workspace: &Path) -> RepoStatus {
    let Ok(repo) = Repository::open(workspace) else {
        return RepoStatus {
            has_repo: false,
            head: None,
            commit_count: 0,
        };
    };
    let head = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| short(&c.id()));
    let commit_count = repo
        .revwalk()
        .ok()
        .and_then(|mut w| w.push_head().ok().map(|_| w.count()))
        .unwrap_or(0);
    RepoStatus {
        has_repo: true,
        head,
        commit_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 测试工作区（uuid 隔离；RAII Drop 清理，失败也回收）。
    struct TempWs(PathBuf);
    impl TempWs {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("abb-wsver-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            TempWs(p)
        }
    }
    impl Drop for TempWs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// HEAD commit 数（走 libgit2，不依赖系统 git）。
    fn commit_count(ws: &Path) -> usize {
        let repo = Repository::open(ws).unwrap();
        let mut revwalk = repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        revwalk.count()
    }

    #[test]
    fn ensure_creates_repo_baseline_and_is_idempotent() {
        let ws = TempWs::new();
        ensure_repo(&ws.0).unwrap();
        assert!(ws.0.join(".git").exists(), ".git 已建");
        assert!(ws.0.join(".gitignore").exists(), ".gitignore 已写");
        assert_eq!(commit_count(&ws.0), 1, "基线 commit 已打");
        let repo = Repository::open(&ws.0).unwrap();
        let head = repo.head().unwrap();
        assert_eq!(
            head.shorthand().ok(),
            Some("main"),
            "默认分支 main（与 GitHub 约定一致）"
        );
        // 幂等：二次 ensure 不新增 commit、不报错
        ensure_repo(&ws.0).unwrap();
        assert_eq!(commit_count(&ws.0), 1, "二次 ensure 不产生新 commit");
    }

    #[test]
    fn snapshot_commits_changes_then_skips_when_clean() {
        let ws = TempWs::new();
        std::fs::write(ws.0.join("note.md"), "hello").unwrap();
        ensure_repo(&ws.0).unwrap();
        // 基线后新增文件 → 快照产生新 commit（带恢复点短 hash）
        std::fs::write(ws.0.join("a.rs"), "fn main() {}").unwrap();
        let h = snapshot(&ws.0, "测试快照")
            .unwrap()
            .expect("有变更 → commit");
        assert_eq!(h.len(), 7, "恢复点为 7 位短 hash");
        assert_eq!(commit_count(&ws.0), 2);
        // 立即再快照 → 无变更不空 commit
        assert!(
            snapshot(&ws.0, "测试快照").unwrap().is_none(),
            "无变更 → None"
        );
        assert_eq!(commit_count(&ws.0), 2);
        // 删除也能被快照记录
        std::fs::remove_file(ws.0.join("a.rs")).unwrap();
        assert!(
            snapshot(&ws.0, "删除测试").unwrap().is_some(),
            "删除 → commit"
        );
        assert_eq!(commit_count(&ws.0), 3);
        // log：新→旧，3 条，短 hash 宽度一致、消息前缀与基线齐备
        let entries = log(&ws.0, 10).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.0.len() == 7));
        assert!(entries[0].2.starts_with("[abb] "), "消息前缀");
        assert!(
            entries.last().unwrap().2.contains("工作区基线"),
            "最旧是基线"
        );
        // 限幅
        assert_eq!(log(&ws.0, 1).unwrap().len(), 1);
    }

    #[test]
    fn gitignore_excludes_runtime_files() {
        let ws = TempWs::new();
        std::fs::write(ws.0.join("real.txt"), "keep").unwrap();
        std::fs::write(ws.0.join("sessions.json"), "{\" secret\":1}").unwrap();
        let hist = ws.0.join("history");
        std::fs::create_dir_all(&hist).unwrap();
        std::fs::write(hist.join("chat.log"), "对话全文").unwrap();
        // 嵌套目录内的非忽略文件也要被收录（pathspec "*" 跨目录语义，审查 P3-7）
        let sub = ws.0.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("real2.txt"), "keep2").unwrap();
        ensure_repo(&ws.0).unwrap();
        let repo = Repository::open(&ws.0).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(tree.get_path(Path::new("real.txt")).is_ok(), "正常文件入库");
        assert!(
            tree.get_path(Path::new("sub/real2.txt")).is_ok(),
            "嵌套目录内非忽略文件入库"
        );
        assert!(
            tree.get_path(Path::new("sessions.json")).is_err(),
            "运行时文件被排除"
        );
        assert!(
            tree.get_path(Path::new("history/chat.log")).is_err(),
            "历史不入库（隐私）"
        );
    }

    #[test]
    fn deleted_file_recoverable_from_previous_commit() {
        let ws = TempWs::new();
        let content = "误删前内容\n";
        std::fs::write(ws.0.join("precious.txt"), content).unwrap();
        ensure_repo(&ws.0).unwrap();
        std::fs::remove_file(ws.0.join("precious.txt")).unwrap();
        assert!(snapshot(&ws.0, "删除保护快照").unwrap().is_some());
        // 删除已入库；上一 commit（HEAD~）仍保有原文——恢复能力的底座
        let repo = Repository::open(&ws.0).unwrap();
        let prev = repo
            .revparse_single("HEAD~")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let blob = prev
            .tree()
            .unwrap()
            .get_path(Path::new("precious.txt"))
            .unwrap()
            .to_object(&repo)
            .unwrap()
            .peel_to_blob()
            .unwrap();
        assert_eq!(std::str::from_utf8(blob.content()).unwrap(), content);
    }

    #[test]
    fn merge_gitignore_appends_only_missing_and_never_overwrites() {
        let ws = TempWs::new();
        std::fs::write(ws.0.join(".gitignore"), "my-own/\n*.tmp\n").unwrap();
        let appended = merge_gitignore(&ws.0).unwrap();
        assert!(appended > 0, "补齐缺失行");
        let content = std::fs::read_to_string(ws.0.join(".gitignore")).unwrap();
        assert!(
            content.starts_with("my-own/\n"),
            "用户已有内容在首位，不被覆盖"
        );
        for line in GITIGNORE.lines().filter(|l| !l.is_empty()) {
            assert!(
                content.lines().any(|e| e.trim() == line),
                "清单行 {line} 已补齐"
            );
        }
        // 二次合并：零追加（幂等）
        assert_eq!(merge_gitignore(&ws.0).unwrap(), 0, "二次合并不追加");
    }

    #[test]
    fn restore_path_recovers_file_and_dir_from_snapshot() {
        // 批次 4：按快照恢复——文件覆盖 + 目录递归 + 恢复前快照防套娃 + 路径安全
        let ws = TempWs::new();
        let dir = ws.0.join("proj/src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(ws.0.join("a.txt"), "v1").unwrap();
        std::fs::write(dir.join("main.rs"), "fn old() {}").unwrap();
        ensure_repo(&ws.0).unwrap();
        let rev = log(&ws.0, 1).unwrap()[0].0.clone(); // 基线短 hash
                                                       // 恢复点之后：改内容 + 删目录内文件
        std::fs::write(ws.0.join("a.txt"), "v2").unwrap();
        std::fs::remove_file(dir.join("main.rs")).unwrap();
        let before = commit_count(&ws.0);
        // 恢复单个文件
        assert_eq!(restore_path(&ws.0, &rev, "a.txt").unwrap(), 1);
        assert_eq!(std::fs::read_to_string(ws.0.join("a.txt")).unwrap(), "v1");
        // 恢复目录（递归写回，含被删文件）
        assert_eq!(restore_path(&ws.0, &rev, "proj").unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("main.rs")).unwrap(),
            "fn old() {}"
        );
        // 防套娃：每次恢复前各打一个「恢复前快照」
        assert_eq!(commit_count(&ws.0), before + 2);
        assert!(
            log(&ws.0, 2)
                .unwrap()
                .iter()
                .all(|e| e.2.contains("恢复前快照")),
            "最近两条都是恢复前快照"
        );
        // 路径安全：绝对路径 / .. 上跳 / 不存在的 rev-路径组合
        assert!(restore_path(&ws.0, &rev, "/etc/passwd").is_err());
        assert!(restore_path(&ws.0, &rev, "../escape.txt").is_err());
        assert!(restore_path(&ws.0, &rev, "no-such.txt").is_err());
        assert!(restore_path(&ws.0, "deadbee", "a.txt").is_err());
        // 恢复点之后新增的文件不被「取回」删除（恢复不是回滚）
        std::fs::write(ws.0.join("new-after.txt"), "keep").unwrap();
        restore_path(&ws.0, &rev, "a.txt").unwrap();
        assert!(ws.0.join("new-after.txt").exists(), "多余文件不动");
    }

    #[test]
    fn ensure_untracks_legacy_tracked_runtime_files() {
        // 批次 3 存量清理：ABB 自建仓库里已被 track 的运行时文件 → 去 track
        // （文件留工作区），下次快照入库；用户自有身份仓库一概不动。
        let ws = TempWs::new();
        // 模拟存量 tidy 仓库：绕过 ensure_repo，手工 init + 以 ABB 身份提交 sessions.json
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("main");
        {
            let repo = Repository::init_opts(&ws.0, &opts).unwrap();
            std::fs::write(ws.0.join("sessions.json"), "{\"legacy\":1}").unwrap();
            std::fs::write(ws.0.join("real.txt"), "keep").unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("sessions.json")).unwrap();
            index.add_path(Path::new("real.txt")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let sig = Signature::now("ABB", "abb@agent-bridge.local").unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "legacy", &tree, &[])
                .unwrap();
        }
        // ensure_repo：合并 gitignore + 去 track（ABB 自建历史）
        ensure_repo(&ws.0).unwrap();
        let repo = Repository::open(&ws.0).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(
            tree.get_path(Path::new("sessions.json")).is_err(),
            "已去 track"
        );
        assert!(tree.get_path(Path::new("real.txt")).is_ok(), "正常文件保留");
        assert!(
            ws.0.join("sessions.json").exists(),
            "工作区文件不动（rm --cached 语义）"
        );
        // 幂等：再次 ensure 不再产生新 commit
        let n = commit_count(&ws.0);
        ensure_repo(&ws.0).unwrap();
        assert_eq!(commit_count(&ws.0), n, "二次 ensure 无新增");
        // 用户自有身份仓库：track 的运行时文件一概不动
        let ws2 = TempWs::new();
        {
            let repo2 = Repository::init_opts(&ws2.0, &opts).unwrap();
            std::fs::write(ws2.0.join("sessions.json"), "{\"user\":1}").unwrap();
            let mut index2 = repo2.index().unwrap();
            index2.add_path(Path::new("sessions.json")).unwrap();
            index2.write().unwrap();
            let tree_id2 = index2.write_tree().unwrap();
            let sig2 = Signature::now("user", "user@example.com").unwrap();
            let tree2 = repo2.find_tree(tree_id2).unwrap();
            repo2
                .commit(Some("HEAD"), &sig2, &sig2, "user repo", &tree2, &[])
                .unwrap();
        }
        ensure_repo(&ws2.0).unwrap();
        let repo2 = Repository::open(&ws2.0).unwrap();
        let tree2 = repo2.head().unwrap().peel_to_tree().unwrap();
        assert!(
            tree2.get_path(Path::new("sessions.json")).is_ok(),
            "用户自有仓库不去 track"
        );
    }

    #[test]
    fn repo_status_reflects_repo_state() {
        // 批次 5：保护状态一览
        let ws = TempWs::new();
        let s = repo_status(&ws.0);
        assert!(!s.has_repo, "未 init 时无仓库");
        ensure_repo(&ws.0).unwrap();
        let s = repo_status(&ws.0);
        assert!(s.has_repo);
        assert_eq!(s.commit_count, 1);
        assert_eq!(s.head.as_deref().map(str::len), Some(7));
    }
}
