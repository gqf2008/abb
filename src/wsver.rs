//! #209 工作区版本管理：内置 libgit2（`git2` crate，进程内操作，不依赖系统 git、
//! 不依赖 PATH 解析）。为每个 bot 工作区提供默认开启的版本管理，作为删除保护
//! （#88）的底座。
//!
//! - [`ensure_repo`]：幂等确保工作区是可用 git 仓库（init + .gitignore 合并 + 基线 commit）
//! - [`snapshot`]：add -A + 有变更才 commit——删除发生前的内容留在上一 commit，删除即有恢复点
//! - [`snapshot_lazy`]：ensure + snapshot 组合（工作区可能从未 init，trash 留痕入口用）
//!
//! 失败降级原则：版本管理是增强层，任何失败由调用方决定降级（启动 init 失败仅告警、
//! 删除留痕失败不阻断删除保护），本模块不 log 不 panic。作者身份固定内联
//! `ABB <abb@agent-bridge.local>`，不读不写全局 git config。
//!
//! 并发（审查确认，best-effort 语义内可接受）：bot 启动 init 与 guard hook 子进程
//! 的 trash 快照可能并发操作同一工作区——index 写入走 index.lock，撞锁一方干净
//! 报 Err 走降级，无索引损坏；ref 更新无 CAS，双 commit 竞态最坏孤儿化一个 commit
//! （对象仍可达，git fsck 可恢复）。

use git2::{IndexAddOption, Repository, RepositoryInitOptions, Signature};
use std::path::Path;

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
/// - 已有 `.git` → 只补 .gitignore 缺失行，用户仓库其余部分一概不动
/// - 无 `.git` → init（默认分支 `main`，父目录不存在则创建）+ .gitignore + 基线
///   commit（空仓也打，作为版本起点；工作区为空时是空树 commit）
///
/// bot 启动时（service.rs）与快照前（[`snapshot_lazy`]）都会调用，重复调用零副作用。
pub fn ensure_repo(workspace: &Path) -> Result<(), String> {
    if workspace.join(".git").exists() {
        merge_gitignore(workspace).map_err(|e| format!(".gitignore 合并失败：{e}"))?;
        return Ok(());
    }
    let mut opts = RepositoryInitOptions::new();
    opts.initial_head("main").mkpath(true);
    g2r(Repository::init_opts(workspace, &opts))?;
    merge_gitignore(workspace).map_err(|e| format!(".gitignore 合并失败：{e}"))?;
    snapshot_impl(workspace, "工作区基线", true)?;
    Ok(())
}

/// add -A + 有变更才 commit。返回是否产生了新 commit（无变更 = false）。
pub fn snapshot(workspace: &Path, reason: &str) -> Result<bool, String> {
    snapshot_impl(workspace, reason, false)
}

/// 删除保护入口：工作区可能从未 init（bot 启动 init 被关/失败过的存量工作区），
/// 先 ensure 再快照。
pub fn snapshot_lazy(workspace: &Path, reason: &str) -> Result<bool, String> {
    ensure_repo(workspace)?;
    snapshot(workspace, reason)
}

/// 快照核心：stage 全部变更（含删除），与 HEAD 有差异才 commit。
///
/// - unborn HEAD（新仓库/基线）→ 无父 commit，直接 commit（`allow_empty` 决定空树
///   是否落 commit；基线=true，日常快照=false）
/// - 有 HEAD 且工作树无差异 → 不产生空 commit（返回 false）
fn snapshot_impl(workspace: &Path, reason: &str, allow_empty: bool) -> Result<bool, String> {
    let repo = g2r(Repository::open(workspace))?;
    let mut index = g2r(repo.index())?;
    // add_all = git add -A：新增/修改/删除全部 stage，遵循 .gitignore（.trash/、
    // history/ 等运行时文件不入库）
    g2r(index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None))?;
    g2r(index.write())?;
    let tree_id = g2r(index.write_tree())?;
    let tree = g2r(repo.find_tree(tree_id))?;
    let sig = g2r(Signature::now("ABB", "abb@agent-bridge.local"))?;
    let msg = format!("[abb] {reason} {}", crate::chrono_lite::now());
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    match &parent {
        Some(p) => {
            let parent_tree = g2r(p.tree())?;
            let diff = g2r(repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None))?;
            if diff.deltas().count() == 0 && !allow_empty {
                return Ok(false);
            }
            g2r(repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &[p]))?;
        }
        None => {
            g2r(repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &[]))?;
        }
    }
    Ok(true)
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
        // 基线后新增文件 → 快照产生新 commit
        std::fs::write(ws.0.join("a.rs"), "fn main() {}").unwrap();
        assert!(snapshot(&ws.0, "测试快照").unwrap(), "有变更 → commit");
        assert_eq!(commit_count(&ws.0), 2);
        // 立即再快照 → 无变更不空 commit
        assert!(!snapshot(&ws.0, "测试快照").unwrap(), "无变更 → false");
        assert_eq!(commit_count(&ws.0), 2);
        // 删除也能被快照记录
        std::fs::remove_file(ws.0.join("a.rs")).unwrap();
        assert!(snapshot(&ws.0, "删除测试").unwrap(), "删除 → commit");
        assert_eq!(commit_count(&ws.0), 3);
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
        snapshot(&ws.0, "删除保护快照").unwrap();
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
}
