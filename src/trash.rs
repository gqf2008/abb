//! 删除保护回收站（#88）：agent 的删除不再直接 rm，而是移入工作区 `.trash/`，
//! 保留 TTL（默认 7 天）后可恢复。
//!
//! 架构：拦截点在 guard-check（claude PreToolUse hook）——owner 会话也装 Bash hook，
//! 删除类命令（rm/rmdir/unlink/del/erase，及 find -delete 的显式拒绝）由 guard-check
//! 判定：工作区内安全删除 → 本模块移到 `.trash/`；危险删除（≥阈值或含代码特征）→
//! 拒绝并登记待确认（/trash confirm）；工作区外删除保持 owner 原行为（不拦截）。
//!
//! 目录布局（全部在 `workspace/.trash/` 下，不进任何 prompt 注入面，参照 tidy
//! archive 豁免）：
//! ```text
//! .trash/
//!   manifest.json   # 条目清单（orig 原路径 / trashed_at / size / dangerous / reason）
//!   items/<id>/<原名>   # 实际移动的内容（id = 时间戳-随机短串，防重名互踩）
//! ```
//! manifest 原子写（atomic_write_text），崩溃不残留半截。
//!
//! git 留痕（留痕层，#209 起内置 libgit2）：**删除前** add -A + commit 快照——
//! 被删内容（含从未提交过的新文件）完整落在快照里，删除即有恢复点；工作区无
//! `.git` 自动 init（不依赖系统 git）。best-effort——留痕失败不阻塞删除保护。
//! `.trash/` 本身在 .gitignore 清单里，回收站本体不入库（内容靠清单 restore/purge
//! 恢复）。

use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};

/// 回收站根目录（工作区内隐藏目录）。
pub fn trash_root(workspace: &Path) -> PathBuf {
    workspace.join(".trash")
}

/// 清单文件。
fn manifest_path(workspace: &Path) -> PathBuf {
    trash_root(workspace).join("manifest.json")
}

/// 一条回收站记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashItem {
    /// 条目 id（移动时生成，restore/purge 按它寻址）。
    pub id: String,
    /// 被删时的原路径（绝对路径，restore 时按它放回）。
    pub orig: String,
    /// 移入回收站的 unix 秒。
    pub trashed_at: u64,
    /// 条目总大小（字节；目录 = 递归求和）。
    pub size: u64,
    /// 是否危险删除（≥阈值或含代码特征）。
    pub dangerous: bool,
    /// 拦截说明（hook 决策 reason 摘要，供列表展示）。
    pub reason: String,
    /// git 恢复点（#209 批次 5：删除前快照的短 hash）。开关关闭/留痕失败 = None，
    /// 此时该条目仅回收站可恢复——列表/删除回执须如实展示。
    #[serde(default)]
    pub snapshot: Option<String>,
}

/// 删除保护设置（从 BotConfig 收敛，guard-check / CLI / service 共用一套默认）。
#[derive(Debug, Clone)]
pub struct TrashSettings {
    /// 总开关（默认开，安全默认）。
    pub enabled: bool,
    /// 回收站保留天数（TTL），默认 7。
    pub ttl_days: u32,
    /// 危险删除大小阈值（MB），默认 50。
    pub dangerous_size_mb: u64,
    /// 危险删除代码特征扩展名（小写，含点），默认见 [`default_code_exts`]。
    pub code_exts: Vec<String>,
    /// 危险删除代码特征文件名（如 package.json / Cargo.toml / go.mod）。
    pub code_files: Vec<String>,
    /// #209 全局工作区版本管理开关（config.workspace_git_enabled）：false = 删除前
    /// 快照完全停用（不 init、不写入已有仓库）。from_bot/defaults 填 true——
    /// 全局开关 bot 配置不可见，由持有 Config 的调用方（guard bot_trash_settings_for）
    /// 覆盖；测试直接构造字段即可（不经 Config::load，封闭）。
    pub git_enabled: bool,
}

/// 默认代码扩展名：主流源码/配置即代码特征（与任务拆解 #88 对齐，.py/.rs/.go/.js/
/// package.json/Cargo.toml 为核心，补全常见工程语言）。
pub fn default_code_exts() -> Vec<String> {
    [
        ".py", ".rs", ".go", ".js", ".ts", ".jsx", ".tsx", ".java", ".kt", ".kts", ".c", ".h",
        ".cpp", ".hpp", ".cc", ".cs", ".php", ".rb", ".swift", ".sh", ".bash", ".zsh", ".scala",
        ".lua", ".m", ".mm", ".vue", ".svelte", ".html", ".css", ".scss", ".sql", ".proto",
        ".toml", ".yaml", ".yml", ".json", ".xml", ".gradle",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// 默认代码特征文件名（无扩展名形态的构建/依赖清单）。
pub fn default_code_files() -> Vec<String> {
    [
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "Cargo.toml",
        "Cargo.lock",
        "go.mod",
        "go.sum",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "requirements.txt",
        "pyproject.toml",
        "setup.py",
        "Gemfile",
        "composer.json",
        "Makefile",
        "CMakeLists.txt",
        "Dockerfile",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl TrashSettings {
    /// 从 BotConfig 提取（缺省回落安全默认；enabled 默认 true）。
    pub fn from_bot(bot: &crate::config::BotConfig) -> Self {
        Self {
            enabled: bot.delete_protect_enabled,
            ttl_days: bot.trash_ttl_days,
            dangerous_size_mb: bot.dangerous_size_mb,
            code_exts: if bot.code_exts.is_empty() {
                default_code_exts()
            } else {
                bot.code_exts.clone()
            },
            code_files: default_code_files(),
            git_enabled: true,
        }
    }

    /// 无 bot 匹配（配置损坏/单测注入）时的安全默认。
    pub fn defaults() -> Self {
        Self {
            enabled: true,
            ttl_days: 7,
            dangerous_size_mb: 50,
            code_exts: default_code_exts(),
            code_files: default_code_files(),
            git_enabled: true, // 保护偏置（与 enabled 同口径）
        }
    }
}

/// 危险判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classify {
    pub dangerous: bool,
    /// 字节数（目录 = 递归求和）。
    pub size: u64,
    /// 危险原因（供 hook reason / 列表展示）。
    pub reason: Option<String>,
}

/// 判定路径是否危险删除：目录递归求大小 + 扫描代码特征（单次遍历同时完成）。
/// 不存在的路径 → 不危险（rm -f 语义，无事可做）。
/// 目录很大时遍历耗时与文件数成正比——hook 内同步执行，接受（删除本身也是全量 IO，
/// 且多数删除是小文件；超大目录另有 size 阈值在遍历中就命中提前短路）。
pub fn classify(workspace: &Path, p: &Path, s: &TrashSettings) -> Classify {
    let abs = absolutize(workspace, p);
    let mut total: u64 = 0;
    let mut found: Option<String> = None;
    let mut stack: Vec<PathBuf> = vec![abs.clone()];
    // 先探是否存在（rm -f 不存在目标 = 无事可做）
    if !abs.exists() {
        return Classify {
            dangerous: false,
            size: 0,
            reason: None,
        };
    }
    while let Some(dir) = stack.pop() {
        let meta = match std::fs::symlink_metadata(&dir) {
            Ok(m) => m,
            Err(_) => continue, // 遍历中被删/权限：跳过该支
        };
        if meta.file_type().is_symlink() {
            // 符号链接本身不算内容（不跟随，防环/防逃逸到工作区外大目录）
            total = total.saturating_add(0);
            continue;
        }
        if meta.is_dir() {
            // 目录：按条目大小计入（目录自身元数据小，主要记文件）
            total = total.saturating_add(meta.len());
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    stack.push(e.path());
                }
            }
        } else {
            total = total.saturating_add(meta.len());
            if found.is_none() && is_code_path(&dir, s) {
                found = Some(
                    dir.strip_prefix(&abs)
                        .map(|r| {
                            let rel = r.to_string_lossy();
                            if rel.is_empty() {
                                // 目标本身就是代码文件（如 rm main.rs）→ 展示文件名
                                dir.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            } else {
                                rel.into_owned()
                            }
                        })
                        .unwrap_or_else(|_| dir.to_string_lossy().into_owned()),
                );
            }
        }
    }
    // 判定优先级：代码特征 > 大小阈值
    if let Some(f) = found {
        Classify {
            dangerous: true,
            size: total,
            reason: Some(format!("含代码特征文件（{}）", f)),
        }
    } else if total / (1024 * 1024) >= s.dangerous_size_mb {
        // 阈值 0 = 任何非空内容都危险（total>0 时 0/1MB=0 ≥ 0 恒真）
        Classify {
            dangerous: true,
            size: total,
            reason: Some(format!(
                "大小 {} MB ≥ 阈值 {} MB",
                total / (1024 * 1024),
                s.dangerous_size_mb
            )),
        }
    } else {
        Classify {
            dangerous: false,
            size: total,
            reason: None,
        }
    }
}

/// 文件名（不含目录）是否命中代码特征：扩展名在 code_exts 或文件名在 code_files。
/// 大小写不敏感（Windows/macOS 文件系统行为对齐，`.PY` 与 `.py` 同判）。
fn is_code_path(p: &Path, s: &TrashSettings) -> bool {
    let Some(name) = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
    else {
        return false;
    };
    if s.code_files.iter().any(|f| f.eq_ignore_ascii_case(&name)) {
        return true;
    }
    match p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
    {
        Some(ext) => {
            let dotted = format!(".{ext}");
            s.code_exts.iter().any(|e| e.eq_ignore_ascii_case(&dotted))
        }
        None => false,
    }
}

/// 相对/绝对路径统一解析到绝对路径（相对按工作区根解析）。
pub fn absolutize(workspace: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    }
}

/// 词法 containment：`p` 归一化后是否落在 `root` 之内（防 `..` 上跳 / 越界绝对路径
/// 逃逸出工作区）。只做词法归一（消 `.`/`..`，不碰文件系统），符号链接目标逃逸由
/// 调用方场景决定（restore 的 orig 来自清单，属数据面，必须拦；移动面 guard 已限定
/// 工作区内路径，此处兜底）。
pub fn contained_in(root: &Path, p: &Path) -> bool {
    let mut base = PathBuf::new();
    for c in root.components() {
        base.push(c.as_os_str());
    }
    let mut q = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !q.pop() {
                    // 上跳超过根（如 /.. 或 C:\..）→ 必然逃逸
                    return false;
                }
            }
            other => q.push(other.as_os_str()),
        }
    }
    q.starts_with(&base)
}

/// 用户可读路径：去掉 Windows canonicalize 的 `\\?\` 动词前缀（该前缀是 Win32 长路径
/// 形态，展示/比对都难看；hook reason、清单、CLI 输出统一用它）。
pub fn pretty_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\")
        .map(str::to_string)
        .unwrap_or_else(|| s.into_owned())
}

/// 加载清单（缺失/损坏 → 空清单，损坏不阻断删除保护主链路）。
pub fn load_manifest(workspace: &Path) -> Vec<TrashItem> {
    let Ok(text) = std::fs::read_to_string(manifest_path(workspace)) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 原子写清单。
fn save_manifest(workspace: &Path, items: &[TrashItem]) -> std::io::Result<()> {
    let dir = trash_root(workspace);
    std::fs::create_dir_all(dir.join("items"))?;
    crate::atomic_write_text(
        &manifest_path(workspace),
        &serde_json::to_string_pretty(items).map_err(std::io::Error::other)?,
    )
}

/// 短随机后缀（8 位 hex，防同一批同目录重名互踩）。
fn rand_suffix() -> String {
    let n = fastrand::u64(0..u64::MAX);
    format!("{n:016x}")[..8].to_string()
}

/// 删除前 git 快照（#209 起为内置 libgit2 实现，不再依赖系统 git）：
///
/// - 工作区无 `.git` → 自动 init（[`crate::wsver`]，bot 启动 init 被关/失败过的
///   存量工作区也能在此补上）
/// - init/快照失败 → Err（删除保护不因留痕失败而失效，调用方降级）
/// - `git_enabled`（config.workspace_git_enabled，经 TrashSettings 注入）关闭 →
///   Ok(None) 跳过，完全不 init、不写已有仓库
///
/// 返回恢复点（新 commit 的短 hash；None = 未产生新 commit，供日志/清单）。
pub fn git_snapshot_sync(workspace: &Path, git_enabled: bool) -> Result<Option<String>, String> {
    if !git_enabled {
        return Ok(None);
    }
    crate::wsver::snapshot_lazy(workspace, "删除前快照")
}

/// 把路径移入回收站（核心动作）。`reason` 是拦截原因摘要（写入清单条目）。
/// - 目标不存在 → 跳过（rm -f 语义）
/// - 已在 .trash 内 → 拒绝（回收站内容只能 restore/purge，防二次套娃）
/// - 移动前 best-effort git 快照（内置 libgit2，无 .git 自动 init；#209）
///
/// 返回新条目列表（成功移动的）。
pub fn move_to_trash(
    workspace: &Path,
    paths: &[PathBuf],
    settings: &TrashSettings,
    reason: &str,
) -> Result<Vec<TrashItem>, String> {
    let root = trash_root(workspace);
    std::fs::create_dir_all(root.join("items")).map_err(|e| format!("创建回收站目录失败：{e}"))?;
    // 留痕层（#209 批次 2：删除前自动快照）：**先** add -A + commit 快照再放行删除
    // ——被删内容（含从未提交过的新文件）完整落在快照里，删除即有恢复点。内置
    // libgit2，工作区无 .git 自动 init，不依赖系统 git；best-effort——留痕失败不
    // 阻断删除保护。.trash/ 由 .gitignore 排除，回收站本体不入库（恢复走清单
    // restore/purge）。确有删除对象才快照（全为不存在/越界路径时，不为空工作区
    // 建库打基线）。恢复点短 hash 记入清单条目（批次 5：保护状态可见）。
    let has_target = paths.iter().any(|p| {
        let abs = absolutize(workspace, p);
        abs.exists() && contained_in(workspace, &abs)
    });
    let mut snapshot_hash: Option<String> = None;
    if has_target {
        match git_snapshot_sync(workspace, settings.git_enabled) {
            Ok(Some(h)) => {
                crate::log!("[trash] git 删除前快照完成（恢复点 {h}）");
                snapshot_hash = Some(h); // 批次 5：记入清单条目
            }
            Ok(None) => {}
            Err(e) => {
                // #105 联动：留痕失败静默降级（不阻断删除保护），日志留痕便于
                // 排查「留痕为什么没生效」。
                crate::log!("[trash] git 留痕跳过（best-effort）：{e}");
            }
        }
    }
    let mut items = load_manifest(workspace);
    let now = crate::chrono_lite::unix_secs();
    let mut moved = Vec::new();
    for p in paths {
        let abs = absolutize(workspace, p);
        // 越界防护（#88 审查跟进）：绝对路径越过工作区根（如 /trash rm 别处文件）
        // 拒绝——回收站是工作区级设施，不代收工作区外内容。
        if !contained_in(workspace, &abs) {
            return Err(format!("{} 不在工作区范围内，拒绝移入回收站", p.display()));
        }
        if !abs.exists() {
            continue;
        }
        if abs.starts_with(&root) {
            return Err(format!(
                "{} 已在回收站内，请用 /trash restore 或 /trash purge 处理",
                p.display()
            ));
        }
        let c = classify(workspace, &abs, settings);
        let id = format!(
            "{}-{}",
            crate::chrono_lite::now().replace([':', ' '], ""),
            rand_suffix()
        );
        let dest_dir = root.join("items").join(&id);
        std::fs::create_dir_all(&dest_dir).map_err(|e| format!("创建条目目录失败：{e}"))?;
        let name = abs
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".into());
        let dest = dest_dir.join(name);
        if let Err(e) = std::fs::rename(&abs, &dest) {
            // 跨设备/占用等 rename 失败 → 回退拷贝+删除（内容保命优先）
            if copy_tree(&abs, &dest).is_err() {
                let _ = std::fs::remove_dir_all(&dest_dir); // 清理半成品
                return Err(format!("移动 {} 到回收站失败：{e}", p.display()));
            }
            let _ = remove_tree(&abs);
        }
        items.push(TrashItem {
            id: id.clone(),
            orig: pretty_path(&abs),
            trashed_at: now,
            size: c.size,
            dangerous: c.dangerous,
            reason: reason.to_string(),
            snapshot: snapshot_hash.clone(),
        });
        moved.push(TrashItem {
            id,
            orig: pretty_path(&abs),
            trashed_at: now,
            size: c.size,
            dangerous: c.dangerous,
            reason: reason.to_string(),
            snapshot: snapshot_hash.clone(),
        });
    }
    save_manifest(workspace, &items).map_err(|e| format!("回收站清单写入失败：{e}"))?;
    Ok(moved)
}

/// 恢复条目到原路径（重建父目录；目标已存在 → 报错不覆盖）。
pub fn restore(workspace: &Path, id: &str) -> Result<TrashItem, String> {
    let mut items = load_manifest(workspace);
    let idx = items
        .iter()
        .position(|i| i.id == id || i.id.starts_with(id))
        .ok_or_else(|| format!("回收站无此条目：{id}（/trash list 查看）"))?;
    let item = items.remove(idx);
    let src_dir = trash_root(workspace).join("items").join(&item.id);
    // 条目内容 = src_dir 下唯一子项（原名目录/文件）；空/缺失 → 报错
    let src = std::fs::read_dir(&src_dir)
        .map_err(|e| format!("条目 {} 内容缺失（{}）：{}", item.id, item.orig, e))?
        .flatten()
        .next()
        .map(|e| e.path())
        .ok_or_else(|| format!("条目 {} 内容为空（{}）", item.id, item.orig))?;
    let orig = PathBuf::from(&item.orig);
    // 越界逃逸防护（#88 审查跟进）：清单是普通 JSON，可被手工编辑/污染——
    // orig 若为相对路径或解析后越过工作区根，restore 的 create_dir_all + rename
    // 会把内容写到工作区外任意位置。此处先拒：非绝对路径或不在工作区内 → 拒绝恢复。
    if !orig.is_absolute() || !contained_in(workspace, &orig) {
        return Err(format!(
            "条目 {} 原路径超出工作区范围，拒绝恢复：{}",
            item.id, item.orig
        ));
    }
    if !src.exists() {
        return Err(format!("条目 {} 内容缺失（可能已被手动清理）", item.id));
    }
    if orig.exists() {
        return Err(format!("原路径已存在，拒绝覆盖：{}", item.orig));
    }
    if let Some(parent) = orig.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("重建父目录失败：{e}"))?;
    }
    if let Err(e) = std::fs::rename(&src, &orig) {
        if copy_tree(&src, &orig).is_err() {
            return Err(format!("恢复 {} 失败：{e}", item.orig));
        }
        let _ = remove_tree(&src);
    }
    let _ = std::fs::remove_dir_all(&src_dir); // 清掉空壳条目目录
    save_manifest(workspace, &items).map_err(|e| format!("回收站清单写入失败：{e}"))?;
    Ok(item)
}

/// 清理过期条目（trashed_at 超 TTL），返回清理条数。条目内容目录一并删除。
pub fn purge_expired(workspace: &Path, ttl_days: u32) -> usize {
    let ttl_secs = (ttl_days.max(1) as u64) * 24 * 3600;
    let now = crate::chrono_lite::unix_secs();
    let items = load_manifest(workspace);
    let (keep, drop): (Vec<TrashItem>, Vec<TrashItem>) = items
        .into_iter()
        .partition(|i| now.saturating_sub(i.trashed_at) < ttl_secs);
    let dropped = drop.len();
    for item in &drop {
        let _ = std::fs::remove_dir_all(trash_root(workspace).join("items").join(&item.id));
    }
    if dropped > 0 {
        let _ = save_manifest(workspace, &keep);
    }
    dropped
}

/// 清空回收站（全部条目；--all 语义）。返回清理条数。
pub fn purge_all(workspace: &Path) -> usize {
    let items = load_manifest(workspace);
    let n = items.len();
    for item in &items {
        let _ = std::fs::remove_dir_all(trash_root(workspace).join("items").join(&item.id));
    }
    let _ = save_manifest(workspace, &[]);
    n
}

/// 列表（按删除时间倒序）。
pub fn list(workspace: &Path) -> Vec<TrashItem> {
    let mut items = load_manifest(workspace);
    items.sort_by_key(|a| std::cmp::Reverse(a.trashed_at));
    items
}

/// 目录拷贝（rename 失败回退：内容保命优先，含权限/时间不保证）。
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// 目录/文件删除（copy 回退后清理原位置）。
fn remove_tree(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ws() -> PathBuf {
        let root = std::env::temp_dir().join(format!("abb-trash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn classify_detects_size_and_code() {
        let ws = temp_ws();
        let s = TrashSettings::defaults();
        // 小文本文件：不危险
        std::fs::write(ws.join("a.txt"), "hello").unwrap();
        assert!(!classify(&ws, &PathBuf::from("a.txt"), &s).dangerous);
        // 代码扩展名：危险（即使很小）
        std::fs::write(ws.join("main.rs"), "fn main(){}").unwrap();
        let c = classify(&ws, &PathBuf::from("main.rs"), &s);
        assert!(c.dangerous, "main.rs 应判为代码特征");
        // 代码文件名（无扩展名形态）：危险
        std::fs::write(ws.join("Cargo.toml"), "[package]").unwrap();
        assert!(classify(&ws, &PathBuf::from("Cargo.toml"), &s).dangerous);
        // 目录含代码特征：危险
        std::fs::create_dir_all(ws.join("proj/src")).unwrap();
        std::fs::write(ws.join("proj/src/app.go"), "package main").unwrap();
        assert!(classify(&ws, &PathBuf::from("proj"), &s).dangerous);
        // 不存在路径：不危险
        assert!(!classify(&ws, &PathBuf::from("nope.txt"), &s).dangerous);
        // 大小阈值：自定义小阈值命中
        let s2 = TrashSettings {
            dangerous_size_mb: 0, // 阈值 0 → 任何非空内容都危险
            ..TrashSettings::defaults()
        };
        std::fs::write(ws.join("big.bin"), vec![0u8; 2048]).unwrap();
        let c2 = classify(&ws, &PathBuf::from("big.bin"), &s2);
        assert!(c2.dangerous, "阈值 0 时任何非空文件都危险");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn move_restore_purge_roundtrip() {
        let ws = temp_ws();
        let s = TrashSettings::defaults();
        std::fs::create_dir_all(ws.join("proj/src")).unwrap();
        std::fs::write(ws.join("proj/src/a.txt"), "data").unwrap();
        std::fs::write(ws.join("proj/readme.md"), "readme").unwrap();

        let moved = move_to_trash(&ws, &[PathBuf::from("proj")], &s, "测试删除").unwrap();
        assert_eq!(moved.len(), 1);
        assert!(!ws.join("proj").exists(), "原路径应已移走");
        assert!(trash_root(&ws).join("items").join(&moved[0].id).exists());
        assert_eq!(list(&ws).len(), 1);

        // restore 回原路径
        let restored = restore(&ws, &moved[0].id).unwrap();
        assert_eq!(restored.orig, ws.join("proj").to_string_lossy());
        assert!(ws.join("proj/src/a.txt").exists(), "恢复后内容应完整");
        assert_eq!(list(&ws).len(), 0, "恢复后清单应清空");

        // purge：先移入再清
        move_to_trash(&ws, &[PathBuf::from("proj")], &s, "再删").unwrap();
        assert_eq!(purge_all(&ws), 1);
        assert!(!trash_root(&ws).join("items").exists() || list(&ws).is_empty());

        // 已删除条目 restore → 报错
        assert!(restore(&ws, "不存在").is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn purge_expired_respects_ttl() {
        let ws = temp_ws();
        let s = TrashSettings::defaults();
        std::fs::write(ws.join("x.txt"), "x").unwrap();
        move_to_trash(&ws, &[PathBuf::from("x.txt")], &s, "t").unwrap();
        // TTL 很大 → 不清理
        assert_eq!(purge_expired(&ws, 7), 0);
        assert_eq!(list(&ws).len(), 1);
        // 手动把条目时间改旧（模拟 8 天前）
        let mut items = load_manifest(&ws);
        items[0].trashed_at = crate::chrono_lite::unix_secs() - 8 * 24 * 3600;
        save_manifest(&ws, &items).unwrap();
        assert_eq!(purge_expired(&ws, 7), 1);
        assert_eq!(list(&ws).len(), 0);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn move_rejects_outside_workspace() {
        let ws = temp_ws();
        let s = TrashSettings::defaults();
        // 工作区外绝对路径（如临时目录）→ 拒绝
        let outside =
            std::env::temp_dir().join(format!("abb-trash-outside-{}", uuid::Uuid::new_v4()));
        std::fs::write(&outside, "x").unwrap();
        let r = move_to_trash(&ws, std::slice::from_ref(&outside), &s, "t");
        assert!(r.is_err(), "工作区外路径应拒绝移入回收站");
        assert!(outside.exists(), "外部文件应保持原位");
        // `..` 上跳逃逸 → 拒绝
        let escape = ws.join("..").join("esc").join("x.txt");
        std::fs::create_dir_all(escape.parent().unwrap()).unwrap();
        std::fs::write(&escape, "x").unwrap();
        assert!(move_to_trash(&ws, std::slice::from_ref(&escape), &s, "t").is_err());
        assert!(escape.exists());
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn restore_rejects_escape_path() {
        let ws = temp_ws();
        let s = TrashSettings::defaults();
        std::fs::write(ws.join("x.txt"), "x").unwrap();
        let moved = move_to_trash(&ws, &[PathBuf::from("x.txt")], &s, "t").unwrap();
        // 污染清单：orig 改为工作区外的绝对路径 → restore 拒绝且不写外部文件
        let outside = std::env::temp_dir()
            .join(format!("abb-trash-escape-{}", uuid::Uuid::new_v4()))
            .join("evil.txt");
        let mut items = load_manifest(&ws);
        items[0].orig = outside.to_string_lossy().into_owned();
        save_manifest(&ws, &items).unwrap();
        assert!(restore(&ws, &moved[0].id).is_err());
        assert!(!outside.exists(), "越界恢复不得写出工作区外");
        // 相对路径 orig → 拒绝
        let mut items = load_manifest(&ws);
        items[0].orig = "../evil.txt".to_string();
        save_manifest(&ws, &items).unwrap();
        assert!(restore(&ws, &moved[0].id).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn move_rejects_retrash() {
        let ws = temp_ws();
        let s = TrashSettings::defaults();
        std::fs::write(ws.join("x.txt"), "x").unwrap();
        move_to_trash(&ws, &[PathBuf::from("x.txt")], &s, "t").unwrap();
        // 对 .trash 内路径再删 → 拒绝
        let trash_inner = trash_root(&ws).join("items");
        assert!(move_to_trash(&ws, &[trash_inner], &s, "t2").is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn deleted_content_recoverable_from_git_snapshot() {
        // #209 批次 2 集成：删除前快照——首次删除、文件从未提交过也有恢复点
        // （内置 libgit2 自动 init，无系统 git 的机器同样成立）。
        let ws = temp_ws();
        let s = TrashSettings::defaults();
        std::fs::write(ws.join("doc.md"), "误删前内容\n").unwrap();
        let moved = move_to_trash(&ws, &[PathBuf::from("doc.md")], &s, "误删").unwrap();
        assert!(!ws.join("doc.md").exists(), "文件已移入回收站");
        // 恢复点记入清单条目（批次 5）：短 hash，且与 HEAD 一致
        assert_eq!(
            moved[0].snapshot.as_deref().map(str::len),
            Some(7),
            "条目带恢复点"
        );
        let repo = git2::Repository::open(&ws).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(
            moved[0].snapshot.as_deref(),
            Some(&head.id().to_string()[..7]),
            "恢复点 = HEAD 短 hash"
        );
        // 快照先于移动：HEAD 就是被删内容的恢复点
        let repo = git2::Repository::open(&ws).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let blob = head
            .tree()
            .unwrap()
            .get_path(Path::new("doc.md"))
            .unwrap()
            .to_object(&repo)
            .unwrap()
            .peel_to_blob()
            .unwrap();
        assert_eq!(std::str::from_utf8(blob.content()).unwrap(), "误删前内容\n");
        // 回收站本体不入库
        let tree = head.tree().unwrap();
        assert!(
            tree.get_path(Path::new(".trash")).is_err(),
            ".trash/ 被 gitignore 排除"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn git_disabled_skips_snapshot_and_repo_creation() {
        // #209 开关关闭（审查 P2-3 补充）：完全不 init、不快照，删除保护本体不受影响。
        // 直接构造字段不经 Config::load——测试封闭，不受开发机真实配置影响。
        let ws = temp_ws();
        let s = TrashSettings {
            git_enabled: false,
            ..TrashSettings::defaults()
        };
        std::fs::write(ws.join("x.txt"), "x").unwrap();
        let moved = move_to_trash(&ws, &[PathBuf::from("x.txt")], &s, "t").unwrap();
        assert_eq!(moved.len(), 1, "删除保护本体正常工作");
        assert!(!ws.join(".git").exists(), "开关关闭不得创建 .git");
        assert!(
            git_snapshot_sync(&ws, false).unwrap().is_none(),
            "关闭 → Ok(None)"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn systemtime_based_id_unique() {
        let ws = temp_ws();
        let s = TrashSettings::defaults();
        std::fs::write(ws.join("a"), "a").unwrap();
        std::fs::write(ws.join("b"), "b").unwrap();
        let m1 = move_to_trash(&ws, &[PathBuf::from("a")], &s, "t").unwrap();
        let m2 = move_to_trash(&ws, &[PathBuf::from("b")], &s, "t").unwrap();
        assert_ne!(m1[0].id, m2[0].id, "同秒移动 id 也应唯一");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
