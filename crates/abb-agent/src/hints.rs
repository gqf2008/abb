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
//! **硬前置**，不是可选优化。读法与语义**逐条对齐** fork 的 `parse_env(…, 0u8)? == 0`：
//! 未设置/`0` ⇒ 开；**任何非零** ⇒ 关；读不懂 ⇒ 拒绝启动（见 [`hints_enabled_from`]）。
//! 注意方向：**不能**退化成「只认字面 `1`」——那会把 `true`/`2`/`01` 当「没关」，属静默
//! fail-open（评审实测过这条退化路径）。

use std::path::{Path, PathBuf};

use crate::provider::EnvSource;

/// 进程级开关：**任何非零值**都表示完全不加载约定链（授权者会话用）；
/// 读不懂的值按参照物口径**拒绝启动**（见 [`hints_enabled_from`]）。
pub const NO_HINTS_ENV: &str = "BUZZ_AGENT_NO_HINTS";

/// 约定文本的字节上限（与 fork 同值）。
const MAX_HINTS_BYTES: usize = 128 * 1024;

/// 相对会话工作目录的约定文件名。
const HINTS_FILE: &str = "AGENTS.md";

/// `BUZZ_AGENT_NO_HINTS` 的读法：**逐条对齐 fork**。
///
/// fork 是 `parse_env("BUZZ_AGENT_NO_HINTS", 0u8)? == 0`，而 `parse_env` 用 `str::parse`
/// 且**不 trim**；读不懂的值在 `Config::from_env()` 里被 `die()`（= `exit(2)`）。
/// 也就是：「未设置 / `0` ⇒ 开；任何**非零**（`1`/`2`/`01`/`+1`/` 1 `）⇒ 关；读不懂 ⇒ 响亮失败」。
///
/// 本包为什么不能只认字面 `1`（上一版就是那么写的，被评审实测证伪）：那个写法把
/// `2`/`01`/`true`/`""` 全当「没关」，而 `true`/`2`/`01` 在 fork 下是关——方向是
/// **静默 fail-open**（owner 的 `~/AGENTS.md` 会被发给授权者）。
/// 回到「非零即关 + 读不懂就拒绝启动」，两边就不再有「关与不关」的判定差。
pub fn hints_enabled_from(value: Option<&str>) -> Result<bool, String> {
    // fork 的 `env()` 对未设置返回 default（默认值 0）而不是报错。
    let raw = value.unwrap_or("0");
    match raw.parse::<u8>() {
        Ok(0) => Ok(true),
        Ok(_) => Ok(false),
        Err(error) => Err(format!(
            "{NO_HINTS_ENV}={raw:?} 无法解析为 0..=255（{error}）——按被替代组件同款口径拒绝启动：\
             该变量只在进程级收口「授权者看不到 owner 私有约定」，读不懂就不能拿它赌"
        )),
    }
}

/// 从环境源判定约定链开关（可注入版本，便于单测不改进程全局 env）。
///
/// `Err` = 配置错误，调用方应当**响亮失败**（`main` 里退出，与 fork 的 `die()` 同款）：
/// 静默降级会把「授权者会不会看到 owner 私有约定」变成掷骰子。
pub fn hints_enabled(env: &dyn EnvSource) -> Result<bool, String> {
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

/// 约定链 + 技能清单（一次把两者算出来：系统提示要用，`load_skill` 也要用同一份技能表）。
///
/// 结构与被替代组件一致：`# Additional Instructions` → `## Project Hints`（AGENTS.md 链）→
/// `## Available Skills`（名字 + 描述）→ 一句「用 `load_skill` 读正文」。
/// **两边都空时不产出任何标题**（避免一个空壳段落占上下文）。
pub fn hints_and_skills(cwd: &Path) -> (String, Vec<crate::skills::SkillEntry>) {
    hints_and_skills_with_home(cwd, home_dir().as_deref())
}

/// 可注入 home 的版本（单测用）。
pub fn hints_and_skills_with_home(
    cwd: &Path,
    home: Option<&Path>,
) -> (String, Vec<crate::skills::SkillEntry>) {
    let hints_text = load_hint_files(cwd, home);
    let skills = crate::skills::discover_with_home(cwd, home);
    if hints_text.is_empty() && skills.is_empty() {
        return (String::new(), skills);
    }
    let mut out = String::from("# Additional Instructions\n");
    if !hints_text.is_empty() {
        out.push_str("\n## Project Hints\n");
        out.push_str(&hints_text);
        out.push('\n');
    }
    if !skills.is_empty() {
        out.push_str("\n## Available Skills\n");
        for skill in &skills {
            out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        out.push_str(
            "\nUse the `load_skill` tool to read the full content of a skill before using it.\n",
        );
    }
    (out, skills)
}

/// **仅测试用**：只含 `AGENTS.md` 链、**不含技能段**。
///
/// 生产路径走 [`hints_and_skills`]（技能清单与技能表必须来自同一次发现）。这里保留是因为
/// 链路语义的单测要能单独钉住「只读 AGENTS.md」的行为；`#[cfg(test)]` 是为了让将来的调用点
/// 不可能误用它而**静默丢技能段**（评审指出的风险）。
#[cfg(test)]
pub fn hints_section(cwd: &Path) -> String {
    hints_section_with_home(cwd, home_dir().as_deref())
}

/// 直接产出要给模型的约定段落（空串 = 没有任何约定需要注入）。
///
/// **不给标题**：fork 的 `# Additional Instructions` 段里还含技能列表，
/// 本包首批只做约定链（技能与 `load_skill` 随内置工具那批落地），所以标题交给调用方
/// 按「有没有内容」决定，避免出现一个空标题。
///
/// `cwd` 按调用方给的原值使用（**包括空串**）：空串时与 fork 同行为——`chain` 退化成
/// `[""]`，只读进程工作目录那一层，**不**向其祖先链扩散。上一版在这里用
/// `current_dir()` 回落，实测会多读进程 cwd 的 git 根到 cwd 整条链（与参照物不同）。
/// 可注入 home 的版本（单测用，避免依赖跑测机器的 `~`）。
#[cfg(test)]
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

    /// 读法逐条对齐 fork：`parse::<u8>()`（不 trim + 非零即关 + 读不懂就报错）。
    ///
    /// 上一版「只认字面 1」被评审实测证伪：`2`/`01`/`true`/`""` 在 fork 下是**关**、在本包
    /// 是「开」，方向是静默 fail-open（owner 的约定会被发给授权者）。
    #[test]
    fn no_hints_reading_matches_the_replaced_component() {
        assert_eq!(
            hints_enabled_from(None),
            Ok(true),
            "未设置必须开启（fork 默认 0）"
        );
        assert_eq!(hints_enabled_from(Some("0")), Ok(true));
        for off in ["1", "2", "255", "01", "+1", "0001"] {
            assert_eq!(
                hints_enabled_from(Some(off)),
                Ok(false),
                "{off:?} 应判为关闭"
            );
        }
        // fork 的 `parse::<u8>()` 不 trim ⇒ 带空白的 `" 1 "` 也是配置错误（不是「关」）。
        for bad in ["", "true", "yes", "-1", "256", "1\n", " 1 "] {
            assert!(
                hints_enabled_from(Some(bad)).is_err(),
                "{bad:?} 在 fork 下是配置错误（exit 2），本包不能静默当没关"
            );
        }
    }

    /// 空 cwd 的回落必须与 fork 同行为：只读**进程工作目录那一层**，不向祖先链扩散。
    ///
    /// 上一版在 acp.rs 里用 `current_dir()` 回落，实测会多读「进程 cwd 的 git 根 → cwd」
    /// 整条链（评审用差分台比出来）。这里把回落目标本身钉住：传 `""` 时链退化成 `[""]`。
    #[test]
    fn empty_cwd_reads_only_one_layer_like_the_replaced_component() {
        let root = temp_dir("emptycwd");
        std::fs::create_dir_all(root.join(".git")).expect("建 .git");
        let sub = root.join("inner");
        std::fs::create_dir_all(&sub).expect("建子目录");
        write(&root.join("AGENTS.md"), "根约定不该被空 cwd 读到");
        write(&sub.join("AGENTS.md"), "单层约定");

        let previous = std::env::current_dir().expect("取进程 cwd");
        // 把进程 cwd 换到 sub：模拟「空 cwd ⇒ 相对路径读当前目录」。
        std::env::set_current_dir(&sub).expect("切 cwd");
        let text = hints_section_with_home(Path::new(""), None);
        std::env::set_current_dir(previous).expect("还原 cwd");

        assert!(text.contains("单层约定"), "{text}");
        assert!(
            !text.contains("根约定不该被空 cwd 读到"),
            "空 cwd 不得向祖先链扩散：{text}"
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
