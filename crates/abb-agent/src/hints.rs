//! 约定链（`AGENTS.md` 逐级加载）与 `BUZZ_AGENT_NO_HINTS` 硬前置。
//!
//! 语义与上限**逐条对齐**被替代的 `crates/buzz-agent/src/hints.rs`：这是「同一份用户约定，
//! 换执行层不该换行为」的载体——链路顺序、全局层位置、128 KiB 上限、UTF-8 安全截断都一样。
//!
//! ## 为什么必须同时认 `BUZZ_AGENT_NO_HINTS`
//!
//! abb 对**授权者（granted）**会话只能**在进程级**收口这件事：fork 侧的 hints 发生在
//! `session/new` 之前，per-session 的 `_meta` 管不到（`src/service.rs:180-234` 的注释写得很
//! 明白，它给 granted 的 agent 进程多塞一个 `BUZZ_AGENT_NO_HINTS=1`）。
//! 本包若不认它，owner 的 `~/AGENTS.md` 与技能就会静默泄漏给授权者——所以这个变量是
//! **硬前置**，不是可选优化：读法与语义都按 fork（`parse_env("BUZZ_AGENT_NO_HINTS", 0) == 0`
//! ⇒ 默认开、只认字面 `1` 之外的值也同样只是「没关」，见 [`hints_enabled`]）。

use std::path::{Path, PathBuf};

use crate::provider::EnvSource;

/// 进程级开关：值为 `1` 时**完全不加载**约定链（授权者会话用）。
pub const NO_HINTS_ENV: &str = "BUZZ_AGENT_NO_HINTS";

/// 约定文本的字节上限（与 fork 同值）。
const MAX_HINTS_BYTES: usize = 128 * 1024;

/// 相对会话工作目录的约定文件名。
const HINTS_FILE: &str = "AGENTS.md";

/// `BUZZ_AGENT_NO_HINTS` 的读法：只有字面 `1` 表示关闭。
///
/// 与 fork 一致（它把该变量解析成 u8 并判 `== 0`）：任何其它取值（含 `0`、空串、非法值）
/// 都视为「没关」。**为什么不把 `true`/`yes` 也算关闭**：abb 只发 `1`，多认形式会让
/// 「gate 是否生效」多出没人维护的分支；要扩就与 abb 一起扩。
pub fn hints_enabled_from(value: Option<&str>) -> bool {
    value.map(|raw| raw.trim()) != Some("1")
}

/// 从环境源判约定链开关（可注入版本，便于单测不改进程全局 env）。
pub fn hints_enabled(env: &dyn EnvSource) -> bool {
    hints_enabled_from(env.get(NO_HINTS_ENV).as_deref())
}

/// 跨平台 home：与 fork 同样用 `dirs`（Windows 上只有 `USERPROFILE`，没有 `HOME`——
/// 手写单一 env 读法会让 Windows 丢掉全局约定层与 `~/.agents/skills`）。
fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// 找到会话工作目录所属的 git 根（`.git` 目录或 worktree 的 `.git` 文件都算）。
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        current = current.parent()?.to_path_buf();
    }
}

/// 按**字符边界**截断，不按字节硬切（UTF-8 安全是仓库约定）。
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

/// 组装 `AGENTS.md` 链路并拼接内容。
///
/// 顺序（与 fork 一致）：**git 根 → … → 会话工作目录**，即越靠近 cwd 的约定越靠后
/// （后写的内容对模型更“近”）；再把 `~/AGENTS.md` 作为**全局层**插到最前，除非 home
/// 本身已在链上（例如会话目录就在 home 下——那时它已经在链里，插两次会重复）。
///
/// 非 git 目录下退化成「只有 cwd 一层」+ 全局层——与 fork 相同，不因为找不到仓库就不加载。
fn load_hint_files(cwd: &Path, home: Option<&Path>) -> String {
    let mut chain = match find_git_root(cwd) {
        Some(root) => {
            let mut chain: Vec<PathBuf> = cwd
                .ancestors()
                .take_while(|dir| dir.starts_with(&root))
                .map(Path::to_path_buf)
                .collect();
            // `ancestors()` 是 cwd → 根；反过来才是加载顺序。
            chain.reverse();
            chain
        }
        None => vec![cwd.to_path_buf()],
    };

    if let Some(home) = home {
        if !chain.iter().any(|dir| dir == home) {
            chain.insert(0, home.to_path_buf());
        }
    }

    let mut result = String::new();
    for dir in &chain {
        let Ok(content) = std::fs::read_to_string(dir.join(HINTS_FILE)) else {
            continue;
        };
        if !result.is_empty() {
            result.push_str("\n\n");
        }
        let remaining = MAX_HINTS_BYTES.saturating_sub(result.len());
        if remaining == 0 {
            break;
        }
        if content.len() <= remaining {
            result.push_str(&content);
        } else {
            result.push_str(truncate_at_boundary(&content, remaining));
            break;
        }
    }
    result
}

/// 直接产出要给模型的约定段落（空串 = 没有任何约定需要注入）。
///
/// **不给标题**：fork 的 `# Additional Instructions` 段里还含技能列表，
/// 本包首批只做约定链（技能与 `load_skill` 随内置工具那批落地），所以标题交给调用方
/// 按「有没有内容」决定，避免出现一个空标题。
pub fn hints_section(cwd: &Path) -> String {
    hints_section_with_home(cwd, home_dir().as_deref())
}

/// 可注入 home 的版本（单测用，避免依赖跑测机器的 `~`）。
pub fn hints_section_with_home(cwd: &Path, home: Option<&Path>) -> String {
    load_hint_files(cwd, home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abb-hints-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("建父目录");
        }
        std::fs::write(path, content).expect("写文件");
    }

    #[test]
    fn only_literal_one_disables_hints() {
        assert!(hints_enabled_from(None), "默认必须开启（与 fork 同）");
        assert!(hints_enabled_from(Some("0")));
        assert!(hints_enabled_from(Some("")));
        assert!(
            hints_enabled_from(Some("true")),
            "多认形式会多出没人维护的分支"
        );
        assert!(!hints_enabled_from(Some("1")));
        assert!(
            !hints_enabled_from(Some(" 1 ")),
            "abb 发的是精确的 \"1\"，两端空白不该改变判定"
        );
    }

    /// 链路顺序：git 根 → 中间目录 → cwd；全局层在最前。
    #[test]
    fn chain_is_root_then_cwd_with_home_first() {
        let root = temp_dir("chain");
        std::fs::create_dir_all(root.join(".git")).expect("建 .git");
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).expect("建子目录");
        write(&root.join("AGENTS.md"), "根约定");
        write(&root.join("a/AGENTS.md"), "中间约定");
        write(&sub.join("AGENTS.md"), "cwd 约定");
        let home = temp_dir("chain-home");
        write(&home.join("AGENTS.md"), "全局约定");

        let text = hints_section_with_home(&sub, Some(&home));
        let order = ["全局约定", "根约定", "中间约定", "cwd 约定"];
        let mut last = 0usize;
        for (i, marker) in order.iter().enumerate() {
            let at = text
                .find(marker)
                .unwrap_or_else(|| panic!("缺 {marker}：{text}"));
            assert!(at >= last, "第 {i} 项顺序不对：{text}");
            last = at;
        }
        assert!(!text.contains("AGENTS.md"), "不该把文件名当内容写进去");
    }

    /// home 已在链上时不重复插入（否则 `~/AGENTS.md` 会被加载两次）。
    #[test]
    fn home_is_not_duplicated_when_already_in_chain() {
        let home = temp_dir("dup");
        std::fs::create_dir_all(home.join(".git")).expect("建 .git");
        let sub = home.join("project");
        std::fs::create_dir_all(&sub).expect("建子目录");
        write(&home.join("AGENTS.md"), "只该出现一次");
        let text = hints_section_with_home(&sub, Some(&home));
        assert_eq!(text.matches("只该出现一次").count(), 1, "{text}");
    }

    /// 非 git 目录也要加载 cwd + 全局层（fork 同款退化，不因没有仓库就静默不加载）。
    #[test]
    fn falls_back_to_cwd_plus_home_without_git() {
        let cwd = temp_dir("nogit");
        write(&cwd.join("AGENTS.md"), "cwd 约定");
        let home = temp_dir("nogit-home");
        write(&home.join("AGENTS.md"), "全局约定");
        let text = hints_section_with_home(&cwd, Some(&home));
        assert!(
            text.contains("全局约定") && text.contains("cwd 约定"),
            "{text}"
        );
        assert!(text.find("全局约定") < text.find("cwd 约定"), "{text}");
    }

    #[test]
    fn no_hints_files_yield_empty_string() {
        let cwd = temp_dir("empty");
        let home = temp_dir("empty-home");
        assert_eq!(hints_section_with_home(&cwd, Some(&home)), "");
    }

    /// 超上限必须按**字符边界**截断（不能切出半个汉字）。
    #[test]
    fn oversize_hints_are_truncated_on_char_boundary() {
        let cwd = temp_dir("big");
        // 每字 3 字节：128 KiB 上限不会落在字符边界上。
        let body = "中".repeat(MAX_HINTS_BYTES / 3 + 1024);
        write(&cwd.join("AGENTS.md"), &body);
        let text = hints_section_with_home(&cwd, None);
        assert!(text.len() <= MAX_HINTS_BYTES, "超上限：{}", text.len());
        assert!(text.starts_with("中"));
        assert!(text.chars().all(|c| c == '中'), "截断不能在中间切出坏字符");
        // UTF-8 合法性由构造本身保证（String），这里再核一次长度与预期一致。
        assert!(text.len() >= MAX_HINTS_BYTES - 3);
    }
}
