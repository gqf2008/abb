//! 三级 AGENTS.md 指令文件（abb 级 → bot 级 → session 级）注入。
//!
//! 任何会话每次任务都要「依次读取」三级指引来指导 LLM 行为。可靠实现 = 桥把文件
//! 内容**每轮全量注入 prompt**（不依赖各后端 CLI 的 cwd 自动加载——claude 读
//! CLAUDE.md、codex 读 AGENTS.md、pi 两者都读，那是 bot 级指引的兜底通道，与桥注入
//! 互补不冲突；abb 级与 session 级不在后端自动加载范围，必须显式注入）。
//!
//! - abb 级：`~/.agent-bridge/AGENTS.md`（全局，所有 bot 所有会话）。
//! - bot 级：`~/.agent-bridge/workspaces/<bot_key>/AGENTS.md`（现有引导文件，
//!   ensure_workspace_guide 生成、marker 保护用户追加——全量读入，~1KB 重复可接受）。
//! - session 级：`~/.agent-bridge/workspaces/<bot_key>/sessions/<escaped_key>.AGENTS.md`
//!   （owner 手工编辑；key 转义复用 history::escape_key——转义输出不含 '.'，
//!   `<escaped>.AGENTS.md` 文件名解析无歧义，且天然落在 guard 的 `.agents.md` 写禁规则内）。
//!
//! 缺失文件静默跳过；全缺返回空串（调用方据此跳过整个块注入，避免每轮刷日志）。
//! 单文件超 [`FILE_CAP_CHARS`] 截断 + 标记（防巨型文件撑爆 prompt）。

use std::path::Path;

/// 单文件注入上限（字符；CJK 一字符一计）。超限截断 + [`TRUNC_MARKER`]。
pub const FILE_CAP_CHARS: usize = 8192;
/// 截断标记：提示内容不完整（模型应意识到后面还有没读到的部分）。
pub const TRUNC_MARKER: &str = "\n\n（以下内容超出 8KB 上限，已截断）\n";

/// 超限截断 + 标记（指令文件 / 会话摘要共用；≤cap 原样返回，超限截到恰好 cap）。
pub fn cap_content(content: &str, cap: usize) -> String {
    if content.chars().count() > cap {
        format!("{}{}", crate::agent::truncate(content, cap), TRUNC_MARKER)
    } else {
        content.to_string()
    }
}

/// 生产入口：以 `~/.agent-bridge` 为根收集三级 AGENTS.md 注入块。
pub fn collect_block(bot_key: &str, session_key: &str) -> String {
    collect_block_at(&crate::bridge_dir(), bot_key, session_key)
}

/// 可测核心：任意 base 目录（生产 = `~/.agent-bridge`；测试注入 temp 根——现有
/// bridge 测试断言 prompt 精确相等，真实 ~/.agent-bridge/AGENTS.md 若存在会破坏它们）。
/// 按 abb → bot → session 顺序拼接；缺文件/空文件静默跳过；
/// 全缺返回空串（调用方跳过注入）。失败读（权限等）按缺失处理——指令文件是增强能力，
/// 读不到不该阻塞聊天主链路。
pub(crate) fn collect_block_at(base: &Path, bot_key: &str, session_key: &str) -> String {
    // #194：虚拟 Bot 群的 session 级指令目录 = 其独立工作区的 sessions/
    let chat = session_key.split(':').next().unwrap_or(session_key);
    let vb_session_dir = crate::virtualbot::vb_dir_for(bot_key, chat).map(|d| d.join("sessions"));
    collect_block_at_with(base, bot_key, session_key, vb_session_dir.as_deref())
}

/// 可测核心（vb_session_dir 注入：虚拟 Bot 群 = 其独立工作区的 sessions/，None =
/// bot 级 sessions/）。任意 base 目录（生产 = `~/.agent-bridge`；测试注入 temp 根——
/// 现有 bridge 测试断言 prompt 精确相等，真实 ~/.agent-bridge/AGENTS.md 若存在会破坏
/// 它们）。按 abb → bot → session 顺序拼接；缺文件/空文件静默跳过；
/// 全缺返回空串（调用方跳过注入）。失败读（权限等）按缺失处理——指令文件是增强能力，
/// 读不到不该阻塞聊天主链路。
fn collect_block_at_with(
    base: &Path,
    bot_key: &str,
    session_key: &str,
    vb_session_dir: Option<&Path>,
) -> String {
    let bot_dir = base.join("workspaces").join(bot_key);
    // #194：虚拟 Bot 群的 session 级指令落在自己的 vb/<uuid>/sessions/ 下（会话记录
    // 独立目录的一部分）。vb 目录根的 AGENTS.md 由后端按 cwd 原生发现（claude 读
    // CLAUDE.md、codex 读 AGENTS.md），不走注入；bot 级仍注入（「可读 bot 工作目录」
    // 的指令层）。
    let session_dir = vb_session_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| bot_dir.join("sessions"));
    let levels: [(&str, std::path::PathBuf); 3] = [
        ("abb 级", base.join("AGENTS.md")),
        ("bot 级", bot_dir.join("AGENTS.md")),
        (
            "session 级",
            session_dir.join(format!(
                "{}.AGENTS.md",
                crate::history::escape_key(session_key)
            )),
        ),
    ];
    let mut sections: Vec<String> = Vec::new();
    for (label, path) in levels {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        let content = cap_content(&content, FILE_CAP_CHARS);
        sections.push(format!("── {label}：{} ──\n{content}", path.display()));
    }
    let mut block = String::from(
        "[指令文件]\n（以下是三级 AGENTS.md 指引，按 abb → bot → session 依次读取，冲突时以 session 级为准）\n\n",
    );
    if sections.is_empty() {
        block.push_str(HOST_GUARD);
    } else {
        block.push_str(&sections.join("\n\n"));
        block.push_str("\n\n");
        block.push_str(HOST_GUARD);
    }
    block.push_str("\n\n");
    block
}

/// #164 宿主护栏（无条件注入）：agent 持有全权限 bash（owner 会话），必须有底线约束
/// ——实测 pi 在任务执行中自行 `taskkill /F /IM agent-bridge.exe` 杀宿主 ABB 导致
/// 「启动→恢复→再杀」死循环。护栏只约束行为（LLM 可能不遵守，真正兜底是
/// recover_pending 的冻结阈值），但能显著降低自作主张概率；不依赖用户指令文件存在。
const HOST_GUARD: &str = "── 宿主护栏（ABB 无条件）──\n\
- 禁止 kill / taskkill / Stop-Process 本机 agent-bridge / ABB 进程（含 /IM、/PID 方式）\n\
- 禁止修改 ~/.agent-bridge 下的宿主配置文件（config.json、teams.json 等），禁止迁移/删除该数据目录\n\
- 需要重启服务或修改宿主配置时，请直接告知用户操作，不要自行执行";

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一 temp 根目录（每个测试独立，避免并发互踩）。
    fn temp_base(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("abb-agentsmd-{name}-{}", uuid::Uuid::new_v4()))
    }

    fn write(p: &Path, content: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn collects_three_levels_in_order() {
        let base = temp_base("order");
        write(&base.join("AGENTS.md"), "# 全局规则\n- 一切以安全为先\n");
        write(
            &base.join("workspaces/b1/AGENTS.md"),
            "# 工作区指引\n- 只在本目录工作\n",
        );
        write(
            &base.join("workspaces/b1/sessions/oc_1%3Aomt_2.AGENTS.md"),
            "# 会话指令\n- 本次任务按步骤来\n",
        );
        let block = collect_block_at(&base, "b1", "oc_1:omt_2");
        assert!(block.starts_with("[指令文件]"), "块头: {block}");
        // 段标签用 "── " 前缀查找（块头指令文案含 "session 级" 字样，裸 find 会误命中）
        let p_abb = block.find("── abb 级").unwrap();
        let p_bot = block.find("── bot 级").unwrap();
        let p_ses = block.find("── session 级").unwrap();
        assert!(p_abb < p_bot && p_bot < p_ses, "顺序 abb → bot → session");
        assert!(block.contains("# 全局规则"));
        assert!(block.contains("# 工作区指引"));
        assert!(block.contains("# 会话指令"));
        assert!(block.contains("依次读取"), "指令文案");
        assert!(block.contains("以 session 级为准"), "冲突优先级说明");
        assert!(block.ends_with("\n\n"), "块尾与后续段隔空行");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn skips_missing_files_silently() {
        let base = temp_base("skip");
        // 全缺 → 只含宿主护栏（#164 无条件注入，不再返回空串）
        let empty_block = collect_block_at(&base, "b1", "k1");
        assert!(
            empty_block.contains("宿主护栏") && !empty_block.contains("── abb 级"),
            "全缺时应只剩护栏: {empty_block}"
        );
        // 只有 bot 级 → 块从 bot 级开始，不出现 abb/session 段
        write(&base.join("workspaces/b1/AGENTS.md"), "只有 bot");
        let block = collect_block_at(&base, "b1", "k1");
        assert!(block.contains("── bot 级"));
        assert!(
            !block.contains("── abb 级") && !block.contains("── session 级"),
            "缺文件段不出现: {block}"
        );
        // 空文件视为缺失
        write(&base.join("AGENTS.md"), "   \n");
        let block2 = collect_block_at(&base, "b1", "k1");
        assert!(!block2.contains("── abb 级"), "空文件跳过");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn truncates_oversized_file_with_marker() {
        let base = temp_base("trunc");
        let big = "长".repeat(FILE_CAP_CHARS + 5000);
        write(&base.join("AGENTS.md"), &big);
        let block = collect_block_at(&base, "b1", "k1");
        assert!(block.contains(TRUNC_MARKER), "超限应带截断标记");
        // 内容恰好截到上限（块中唯一的「长」来自文件内容，路径/标签无此字）
        assert_eq!(
            block.matches("长").count(),
            FILE_CAP_CHARS,
            "内容截断到恰好上限"
        );
        assert!(
            block.find(TRUNC_MARKER).unwrap() < block.find("── bot 级").unwrap_or(usize::MAX),
            "abb 段截断标记后无后续内容段（其它文件缺失）"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn escaped_key_used_in_filename() {
        // session_key 含 ':' → 读的是 sessions/<escaped>.AGENTS.md（%3A）
        let base = temp_base("esc");
        let session_file = base.join("workspaces/b1/sessions/oc_1%3Aomt_2.AGENTS.md");
        write(&session_file, "话题会话指令");
        let block = collect_block_at(&base, "b1", "oc_1:omt_2");
        assert!(block.contains("话题会话指令"), "按转义文件名读取");
        // 未转义文件名（错误放置）读不到
        let wrong = base.join("workspaces/b1/sessions/oc_1:omt_2.AGENTS.md");
        assert!(!wrong.exists());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn multiple_files_each_capped_independently() {
        // 两个超限文件各自截断（不共享预算）
        let base = temp_base("multi");
        let big1 = "a".repeat(FILE_CAP_CHARS + 100);
        let big2 = "b".repeat(FILE_CAP_CHARS + 100);
        write(&base.join("AGENTS.md"), &big1);
        write(&base.join("workspaces/b1/AGENTS.md"), &big2);
        let block = collect_block_at(&base, "b1", "k1");
        assert_eq!(
            block.matches(TRUNC_MARKER).count(),
            2,
            "两个超限文件各自截断"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// #194：虚拟 Bot 群的 session 级指令从 vb/<uuid>/sessions/ 收集（注入 vb 目录
    /// 参数时），不再取 bot 级 sessions/；bot 级仍注入（可读 bot 工作目录的指令层）。
    #[test]
    fn collect_block_uses_vb_session_dir_when_given() {
        let base = temp_base("vbdir");
        write(&base.join("workspaces/b1/AGENTS.md"), "bot 级规则");
        let vb_sessions = base.join("workspaces/b1/vb/uuid-1/sessions");
        write(&vb_sessions.join("oc_vb.AGENTS.md"), "vb 专属指令");

        // 传入 vb 目录：session 级取 vb/sessions/
        let block = collect_block_at_with(&base, "b1", "oc_vb", Some(&vb_sessions));
        assert!(
            block.contains("vb 专属指令"),
            "session 级取 vb 目录: {block}"
        );
        assert!(block.contains("bot 级规则"), "bot 级仍注入");

        // 不传 vb 目录（非虚拟会话）：session 级回到 bot 级 sessions/，vb 文件不泄入
        let block2 = collect_block_at_with(&base, "b1", "oc_vb", None);
        assert!(
            !block2.contains("vb 专属指令"),
            "非虚拟路径不得读 vb 会话指令: {block2}"
        );
        std::fs::remove_dir_all(&base).ok();
    }
}
