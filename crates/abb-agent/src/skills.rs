//! 技能发现与按需读取（`load_skill` 的数据面）。
//!
//! 语义与上限**逐条对齐**被替代的 `crates/buzz-agent/src/hints.rs`（发现）与
//! `crates/buzz-agent/src/builtin.rs`（读取）：同一批技能目录，换执行层不该换行为。
//!
//! - 发现：`<cwd>/.agents/skills`、`<cwd>/.goose/skills`、`<cwd>/.claude/skills` + `~/.agents/skills`；
//!   按 `SKILL.md` 的 frontmatter 取 `name`/`description`；**按名字去重**（先发现的赢）；
//!   子目录自己带 `SKILL.md` 的不再下钻（那是另一个技能）；支持文件全部预枚举。
//! - 读取：`name` 取整个技能（剥掉 frontmatter），`name/rel/path` 取支持文件；
//!   两者合计 32 KiB 上限（按**字符边界**截断）。
//! - 越界保护：支持文件必须**已经在预枚举清单里**且 canonicalize 后仍在**技能目录（发现时的
//!   真实路径）**内。**它拦的是「支持文件本身是/被换成指向外面的符号链接」**，**不是**
//!   「技能目录整体被换成指向外面的符号链接」（那时两边 canonicalize 都落到外面、前缀照样成立）
//!   或「支持文件是与外部同 inode 的硬链接」——这两条与参照物**同形**，如实登记为残留限制。
//! - 参照物另有受限档 `read_roots` 校验（技能常在 `~/.agents/skills`，在工作区之外），本包
//!   不实现档位（owner 裁定「授权责任不变」），见 README 的登记。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 技能正文（含 `## Supporting Files` 段）的字节上限（与参照物同值）。
pub const MAX_SKILL_BODY_BYTES: usize = 32 * 1024;

/// 会话工作目录下的技能目录（与参照物同序）。
const SKILL_DIRS: &[&str] = &[".agents/skills", ".goose/skills", ".claude/skills"];

/// 全局技能目录（相对 home）。
const HOME_SKILL_DIR: &str = ".agents/skills";

#[derive(Clone, Debug)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// `SKILL.md` 的绝对路径。
    pub path: PathBuf,
    /// 技能目录树里除 `SKILL.md` 外的所有文件（预枚举，供 `load_skill` 按相对路径匹配）。
    pub supporting_files: Vec<PathBuf>,
}

/// 跨平台 home（与约定链同款：Windows 只有 `USERPROFILE`）。
fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// 按**字符边界**截断，不按字节硬切。
fn truncate_at_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// 剥掉 YAML frontmatter（`---\n…\n---\n` 之后才是正文）。
pub fn strip_frontmatter(raw: &str) -> &str {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return raw;
    };
    let Some(close) = rest.find("\n---") else {
        return raw;
    };
    let after = &rest[close + 4..];
    after.strip_prefix('\n').unwrap_or(after)
}

/// 解析 frontmatter 里的 `name` / `description`（缺 `name` 就不是技能）。
fn parse_frontmatter(content: &str) -> Option<(String, String)> {
    let rest = content.strip_prefix("---\n")?;
    let close = rest.find("\n---")?;
    let block = &rest[..close];
    let map: std::collections::HashMap<String, serde_yaml::Value> =
        serde_yaml::from_str(block).ok()?;
    let name = map
        .get("name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let description = map
        .get("description")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    Some((name, description))
}

/// 递归收集支持文件（不下钻到「自己带 `SKILL.md`」的子目录）。
fn collect_supporting_files(skill_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    collect_supporting_files_impl(skill_dir, &mut out, &mut visited);
    out.sort();
    out
}

fn collect_supporting_files_impl(
    current: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) {
    let Ok(canonical) = current.canonicalize() else {
        return;
    };
    if !visited.insert(canonical) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    let mut items: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|e| e.path())
        .collect();
    items.sort();
    for path in items {
        // 用 `metadata`（跟随符号链接）：技能目录里指向别处的链接文件也要能读到。
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if path.join("SKILL.md").is_file() {
                continue; // 那是另一个技能
            }
            collect_supporting_files_impl(&path, out, visited);
        } else if metadata.is_file()
            && path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md")
        {
            out.push(path);
        }
    }
}

fn scan_skill_dir(dir: &Path, seen: &mut HashSet<String>, skills: &mut Vec<SkillEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        // 跟随符号链接（`DirEntry::file_type` 对链接会给出 Symlink ⇒ is_dir() 为假）。
        .filter(|path| std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false))
        .collect();
    subdirs.sort();

    for subdir in subdirs {
        let skill_md = subdir.join("SKILL.md");
        let Ok(content) = std::fs::read_to_string(&skill_md) else {
            continue;
        };
        let Some((name, description)) = parse_frontmatter(&content) else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue; // 同名先发现的赢
        }
        skills.push(SkillEntry {
            name,
            description,
            supporting_files: collect_supporting_files(&subdir),
            path: skill_md,
        });
    }
}

/// 发现技能（`cwd` 优先，然后 `~/.agents/skills`）。
pub fn discover(cwd: &Path) -> Vec<SkillEntry> {
    discover_with_home(cwd, home_dir().as_deref())
}

/// 可注入 home 的版本（单测用）。
pub fn discover_with_home(cwd: &Path, home: Option<&Path>) -> Vec<SkillEntry> {
    let mut seen = HashSet::new();
    let mut skills = Vec::new();
    for suffix in SKILL_DIRS {
        scan_skill_dir(&cwd.join(suffix), &mut seen, &mut skills);
    }
    if let Some(home) = home {
        scan_skill_dir(&home.join(HOME_SKILL_DIR), &mut seen, &mut skills);
    }
    skills
}

/// 读一个技能（或它的支持文件）。
///
/// `request` 形如 `name` 或 `name/relative/path`。错误信息里带可用技能名/支持文件清单
/// （与参照物同款：模型据此自我纠偏，而不是拿到一句干巴巴的失败）。
pub fn load(request: &str, skills: &[SkillEntry]) -> Result<String, String> {
    // 切分与归一化的**次序与参照物逐条一致**：先 `split_once('/')`，再只对**相对路径**那半
    // 把 `\` 归一成 `/`（技能名不归一）。
    // 次序反了会有两个可观测差异：全反斜杠的路径本包能读而参照物读不到；技能名里含 `\` 的
    // 技能本包读不到而参照物能读（评审实测）。
    let (skill_name, rel_path) = match request.split_once('/') {
        Some((name, rest)) => (name.to_string(), Some(rest.replace('\\', "/"))),
        None => (request.to_string(), None),
    };
    let entry = skills
        .iter()
        .find(|skill| skill.name == skill_name)
        .ok_or_else(|| {
            let available: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
            format!("load_skill: skill {skill_name:?} not found. Available: {available:?}")
        })?;
    let skill_dir = entry.path.parent().ok_or_else(|| {
        format!("load_skill: could not determine skill directory for {skill_name:?}")
    })?;

    let Some(rel_path) = rel_path else {
        return Ok(render_skill(entry));
    };

    let matched = entry.supporting_files.iter().find(|file| {
        file.strip_prefix(skill_dir)
            .map(|rel| rel.to_string_lossy().replace('\\', "/") == rel_path)
            .unwrap_or(false)
    });
    let Some(file) = matched else {
        let available: Vec<String> = entry
            .supporting_files
            .iter()
            .filter_map(|file| {
                file.strip_prefix(skill_dir)
                    .ok()
                    .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            })
            .collect();
        return Err(if available.is_empty() {
            format!("load_skill: skill {skill_name:?} has no supporting files.")
        } else {
            format!(
                "load_skill: file {rel_path:?} not found in skill {skill_name:?}. Available: {available:?}"
            )
        });
    };

    // 越界保护：预枚举清单是按发现时的路径算的，读取前再核一次**真实路径**仍在技能目录内。
    // 它拦的是「**支持文件本身**是/被换成指向外面的符号链接」；**整个技能目录**被换成符号链接、
    // 或支持文件是与外部同 inode 的**硬链接**，这两种形状这里拦不住（与参照物同形，见模块文档
    // 的残留限制）。参照物在受限档另有 `read_roots` 校验，本包不实现档位。
    let canonical_dir = skill_dir.canonicalize().map_err(|error| {
        format!("load_skill: could not canonicalize skill directory for {skill_name:?}: {error}")
    })?;
    let canonical_file = file
        .canonicalize()
        .map_err(|error| format!("load_skill: could not canonicalize {file:?}: {error}"))?;
    if !canonical_file.starts_with(&canonical_dir) {
        return Err(format!(
            "load_skill: refusing to load {skill_name:?}/{rel_path}: \
             resolves outside the skill directory"
        ));
    }
    let content = std::fs::read_to_string(&canonical_file).map_err(|error| {
        // 措辞与参照物逐字一致：**不**回显绝对路径（评审实测两端文本不同）。
        format!("load_skill: could not read {skill_name:?}/{rel_path}: {error}")
    })?;
    // 成功结果的**外框**与参照物逐字一致（模型据此知道这份内容已经进上下文）。
    let output =
        format!("# Loaded: {skill_name}/{rel_path}\n\n{content}\n\n---\nFile loaded into context.");
    Ok(if output.len() > MAX_SKILL_BODY_BYTES {
        truncate_at_boundary(&output, MAX_SKILL_BODY_BYTES).to_string()
    } else {
        output
    })
}

/// 技能正文 + `## Supporting Files` 段（合计受 32 KiB 上限约束）。
fn render_skill(entry: &SkillEntry) -> String {
    let raw = std::fs::read_to_string(&entry.path)
        .map_err(|error| format!("load_skill: could not read {:?}: {error}", entry.path))
        .unwrap_or_default();
    let mut output = strip_frontmatter(&raw).to_string();
    if !entry.supporting_files.is_empty() {
        let skill_dir = entry.path.parent().unwrap_or(&entry.path);
        output.push_str("\n\n## Supporting Files\n\n");
        for file in &entry.supporting_files {
            if let Ok(rel) = file.strip_prefix(skill_dir) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                output.push_str(&format!(
                    "- {rel} (load_skill(name: \"{}/{rel}\"))\n",
                    entry.name
                ));
            }
        }
    }
    if output.len() > MAX_SKILL_BODY_BYTES {
        return truncate_at_boundary(&output, MAX_SKILL_BODY_BYTES).to_string();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abb-skills-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write_skill(root: &Path, dir: &str, name: &str, description: &str, body: &str) -> PathBuf {
        let skill_dir = root.join(".agents/skills").join(dir);
        std::fs::create_dir_all(&skill_dir).expect("建技能目录");
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md,
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .expect("写 SKILL.md");
        skill_md
    }

    #[test]
    fn discovers_cwd_then_home_and_dedupes_by_name() {
        let cwd = temp_dir("discover");
        let home = temp_dir("discover-home");
        write_skill(&cwd, "a", "alpha", "来自 cwd", "正文 A");
        write_skill(&home, "b", "beta", "来自 home", "正文 B");
        // 同名技能：cwd 的先发现 ⇒ 赢；home 的同名被忽略。
        write_skill(&home, "alpha-dup", "alpha", "来自 home 的同名", "不该出现");

        let skills = discover_with_home(&cwd, Some(&home));
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"], "{names:?}");
        assert_eq!(skills[0].description, "来自 cwd");
    }

    /// 没有 frontmatter / 没有 name 的目录不是技能。
    #[test]
    fn ignores_dirs_without_frontmatter_name() {
        let cwd = temp_dir("noname");
        let dir = cwd.join(".agents/skills/plain");
        std::fs::create_dir_all(&dir).expect("建目录");
        std::fs::write(dir.join("SKILL.md"), "没有 frontmatter 的正文").expect("写");
        let dir2 = cwd.join(".agents/skills/onlydesc");
        std::fs::create_dir_all(&dir2).expect("建目录");
        std::fs::write(
            dir2.join("SKILL.md"),
            "---\ndescription: 只有描述\n---\n正文",
        )
        .expect("写");
        assert!(discover_with_home(&cwd, None).is_empty());
    }

    /// 支持文件被预枚举，且嵌套技能不被下钻。
    #[test]
    fn enumerates_supporting_files_without_descending_into_nested_skills() {
        let cwd = temp_dir("support");
        let skill_md = write_skill(&cwd, "demo", "demo", "带支持文件", "正文");
        let skill_dir = skill_md.parent().expect("技能目录").to_path_buf();
        std::fs::create_dir_all(skill_dir.join("references")).expect("建子目录");
        std::fs::write(skill_dir.join("references/foo.md"), "参考内容").expect("写");
        // 嵌套技能：自己带 SKILL.md ⇒ 不下钻（其文件不算本技能的支持文件）
        std::fs::create_dir_all(skill_dir.join("nested")).expect("建嵌套目录");
        std::fs::write(
            skill_dir.join("nested/SKILL.md"),
            "---\nname: nested\n---\n",
        )
        .expect("写");
        std::fs::write(skill_dir.join("nested/hidden.txt"), "不该被枚举").expect("写");

        let skills = discover_with_home(&cwd, None);
        assert_eq!(skills.len(), 1, "嵌套技能不该被当作支持文件来源");
        let rels: Vec<String> = skills[0]
            .supporting_files
            .iter()
            .map(|file| {
                file.strip_prefix(&skill_dir)
                    .expect("相对路径")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(rels, vec!["references/foo.md"], "{rels:?}");
    }

    #[test]
    fn load_returns_body_without_frontmatter_and_lists_supporting_files() {
        let cwd = temp_dir("load");
        let skill_md = write_skill(&cwd, "demo", "demo", "描述", "这是正文 MARKER");
        let skill_dir = skill_md.parent().expect("目录").to_path_buf();
        std::fs::create_dir_all(skill_dir.join("references")).expect("建目录");
        std::fs::write(skill_dir.join("references/foo.md"), "参考内容 REF").expect("写");

        let skills = discover_with_home(&cwd, None);
        let body = load("demo", &skills).expect("读技能");
        assert!(body.starts_with("这是正文 MARKER"), "{body}");
        assert!(
            !body.contains("description: 描述"),
            "frontmatter 必须剥掉：{body}"
        );
        assert!(body.contains("## Supporting Files"), "{body}");
        assert!(
            body.contains("load_skill(name: \"demo/references/foo.md\")"),
            "{body}"
        );

        let sub = load("demo/references/foo.md", &skills).expect("读支持文件");
        // 外框与参照物逐字一致。
        assert_eq!(
            sub,
            "# Loaded: demo/references/foo.md\n\n参考内容 REF\n\n---\nFile loaded into context."
        );
        // 归一化只作用在**相对路径**那半、且切分只认 `/`（与参照物逐条同）：
        // `demo/references\foo.md` 能读到；而整条都用反斜杠的 `demo\references\foo.md`
        // 连切分都发生不了 ⇒ 两边都判「技能不存在」（不是本包更宽）。
        let mixed = load("demo/references\\foo.md", &skills).expect("混合分隔符");
        assert!(mixed.contains("参考内容 REF"), "{mixed}");
        let all_backslash =
            load("demo\\references\\foo.md", &skills).expect_err("整体反斜杠不切分（与参照物同）");
        assert!(all_backslash.contains("not found"), "{all_backslash}");
    }

    #[test]
    fn load_errors_are_actionable_and_do_not_leak_paths_outside() {
        let cwd = temp_dir("errors");
        let skill_md = write_skill(&cwd, "demo", "demo", "描述", "正文");
        let skill_dir = skill_md.parent().expect("目录").to_path_buf();
        std::fs::write(skill_dir.join("ref.md"), "可读的支持文件").expect("写");
        let skills = discover_with_home(&cwd, None);
        let missing = load("nope", &skills).expect_err("缺技能要报错");
        assert!(
            missing.contains("not found") && missing.contains("demo"),
            "{missing}"
        );

        // 越界路径：不在预枚举清单里 ⇒ 报 not found 并列出可用的支持文件（模型可自我纠偏）。
        let escape = load("demo/../secret.txt", &skills).expect_err("越界路径要报错");
        assert!(escape.contains("not found"), "{escape}");
        assert!(
            escape.contains("ref.md"),
            "错误信息要给出可用文件：{escape}"
        );

        // 清单外的工作区外绝对路径同样拿不到（不支持绝对路径形式 ⇒ 落到同一分支）。
        let absolute = load("demo/../../etc/hosts", &skills).expect_err("越界要报错");
        assert!(absolute.contains("not found"), "{absolute}");

        // 没有任何支持文件时的错误也要说清（措辞与参照物逐字一致，含句号）。
        write_skill(&cwd, "bare", "bare", "无支持文件", "正文");
        let skills = discover_with_home(&cwd, None);
        let no_files = load("bare/whatever.md", &skills).expect_err("要报错");
        assert!(no_files.contains("has no supporting files."), "{no_files}");

        // `demo/`（相对路径为空）走**支持文件**分支、不是整技能分支 ⇒ 报错并列出可用文件
        // （参照物 `split_once('/')` 的语义；上一版把它当成整技能读了）。
        let trailing = load("demo/", &skills).expect_err("空相对路径要报错");
        assert!(trailing.contains("file \"\" not found"), "{trailing}");
        assert!(trailing.contains("ref.md"), "{trailing}");
    }

    /// 技能名**不**做反斜杠归一（参照物只归一相对路径那半）：名字里带 `\\` 的技能能按原名读到。
    #[test]
    fn skill_names_are_not_backslash_normalized() {
        let cwd = temp_dir("backslash-name");
        write_skill(&cwd, "bs", r"we\ird", "名字里有反斜杠", "正文 BS");
        let skills = discover_with_home(&cwd, None);
        assert_eq!(skills.len(), 1);
        let loaded = load(r"we\ird", &skills).expect("按原名读");
        assert!(loaded.contains("正文 BS"), "{loaded}");
        // 归一化过的那半（`we/ird`）不是这个名字 ⇒ 找不到。
        assert!(load("we/ird", &skills).is_err());
    }

    /// 支持文件发现后被换成指向技能目录外的符号链接 ⇒ 读取时必须拒绝。
    #[test]
    fn load_refuses_supporting_file_swapped_to_outside_symlink() {
        let cwd = temp_dir("symlink");
        let skill_md = write_skill(&cwd, "demo", "demo", "描述", "正文");
        let skill_dir = skill_md.parent().expect("目录").to_path_buf();
        std::fs::write(skill_dir.join("ref.md"), "原始内容").expect("写");
        let skills = discover_with_home(&cwd, None);

        let outside = temp_dir("symlink-outside").join("secret.md");
        std::fs::write(&outside, "工作区外的秘密").expect("写");
        #[cfg(unix)]
        {
            std::fs::remove_file(skill_dir.join("ref.md")).expect("删原文件");
            std::os::unix::fs::symlink(&outside, skill_dir.join("ref.md")).expect("建链接");
        }
        #[cfg(not(unix))]
        return;

        let result = load("demo/ref.md", &skills);
        assert!(
            result.is_err()
                && result
                    .unwrap_err()
                    .contains("resolves outside the skill directory"),
            "越界读必须拒绝"
        );
    }

    #[test]
    fn strips_frontmatter_only_when_present() {
        assert_eq!(strip_frontmatter("---\nname: x\n---\n正文"), "正文");
        assert_eq!(strip_frontmatter("没有 frontmatter"), "没有 frontmatter");
        assert_eq!(strip_frontmatter("---\nname: x\n"), "---\nname: x\n");
    }

    /// 32 KiB 上限按字符边界截断。
    #[test]
    fn oversized_body_is_truncated_on_char_boundary() {
        let cwd = temp_dir("big");
        let body = "中".repeat(MAX_SKILL_BODY_BYTES);
        write_skill(&cwd, "big", "big", "描述", &body);
        let skills = discover_with_home(&cwd, None);
        let loaded = load("big", &skills).expect("读技能");
        assert!(loaded.len() <= MAX_SKILL_BODY_BYTES, "{}", loaded.len());
        assert!(loaded.chars().all(|c| c == '中'), "不得切出半个字符");
    }
}
