//! #130 上下文超长自动分段压缩。
//!
//! 后端（claude/codex/pi）因上下文超长返回错误时，ABB 自动把该会话历史分段压缩：
//! 旧段（更早轮次）用同后端一次性 LLM 摘要（失败回退确定性截断），保留最近 M 轮
//! 原文，换新会话注入压缩块后自动重试当前消息，用户无感续聊。原超长会话文件先备份
//! （`<key>.precompress-<ts>.jsonl`）再压缩，审计/回溯不丢。
//!
//! 编排点在 `agent::run` 的 `AttemptErr::Failed` 兜底分支（见 agent.rs #130 注释）；
//! 本模块只负责纯逻辑：错误识别、分段压缩、压缩块渲染、重试 prompt 重建——均可单测。

use crate::agent::Backend;
use crate::history::{History, HistoryEntry};

/// 上下文超长错误特征（大小写不敏感子串匹配）。覆盖三后端常见文案：
/// claude `prompt is too long` / `maximum context length` / `context window`、
/// codex `context_length_exceeded` / `token limit`、pi errorMessage 同族。
/// 刻意不收录限流/网络类文案（rate limit / timeout / connection），防误伤。
const OVERFLOW_PATTERNS: &[&str] = &[
    "prompt is too long",
    "prompt too long",
    "maximum context length",
    "context length exceeded",
    "context_length_exceeded",
    "context window",
    "token limit",
];

/// 识别上下文超长类错误（fail-safe：识别不到的不触发，其它错误走既有逻辑）。
pub fn is_context_overflow(err: &str) -> bool {
    let lower = err.to_lowercase();
    OVERFLOW_PATTERNS.iter().any(|p| lower.contains(p))
}

/// 单段摘要交给 LLM 的条目数上限（分段大小，可配置语义：摘要分段大小）。
const SEG_ENTRIES: usize = 10;
/// 单轮压缩最多走 LLM 的段数：超出部分确定性截断（LLM 延迟有界，不卡主链路太久）。
const MAX_LLM_SEGMENTS: usize = 3;

/// 压缩结果统计（日志/用户提示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressStats {
    /// 压缩前条目数。
    pub before: usize,
    /// 压缩后条目数。
    pub after: usize,
    /// 保留原文的条目数（最近 M 轮）。
    pub kept: usize,
    /// 是否至少有一段走了 LLM 摘要。
    pub llm: bool,
}

/// LLM 分段摘要专用系统提示（不出现在用户可见回复；要求结构化保留关键信息）。
const SUMMARY_SYS: &str = "你是会话历史压缩助手。请把以下对话记录压缩为结构化分段摘要，\
必须保留：用户意图、关键决策、已达成结论、待办/未完成事项、重要事实/偏好。\
不要回复对话内容本身，只输出摘要文本，控制在 500 字以内。";

/// 压缩该会话历史：旧段摘要（LLM 尽力 + 确定性兜底）+ 最近 `keep_rounds` 轮原文。
/// 历史不够长（无旧段可压）返回 None（调用方不重试，按普通错误处理）。
/// 压缩前先备份原文件（`backup_original`，审计/回溯）；压缩后 `overwrite` 原子写回。
/// 触发条件阈值：条目数必须显著大于保留量（至少多出 4 条旧条目），避免空转。
pub async fn compress(
    hist: &History,
    backend: Backend,
    keep_rounds: usize,
    use_llm: bool,
) -> Option<CompressStats> {
    let all = hist.entries();
    let keep = keep_rounds.saturating_mul(2); // 一轮 ≈ 用户+助手两条条目
    if all.len() < keep + 4 || all.is_empty() {
        return None; // 历史不够长 / 无历史，无需压缩
    }
    let (old, recent) = all.split_at(all.len() - keep);
    if old.is_empty() {
        return None;
    }
    hist.backup_original();

    let segs: Vec<&[HistoryEntry]> = old.chunks(SEG_ENTRIES).collect();
    let llm_count = if use_llm {
        segs.len().min(MAX_LLM_SEGMENTS)
    } else {
        0
    };
    let mut summary_entries: Vec<HistoryEntry> = Vec::new();
    let mut llm_used = false;
    for (i, seg) in segs.iter().enumerate() {
        if i < llm_count {
            let text = seg_text(seg);
            if let Ok(sum) = crate::agent::one_shot_text(backend, SUMMARY_SYS, &text, 20).await {
                let sum = sum.trim();
                if !sum.is_empty() {
                    summary_entries.push(HistoryEntry {
                        mid: format!("__ctxcmp_{i}"),
                        user: true,
                        backend: backend.name().to_string(),
                        text: format!("[历史摘要] {sum}"),
                        ts: crate::chrono_lite::unix_secs(),
                    });
                    llm_used = true;
                    continue;
                }
            }
        }
        // 确定性兜底：每条旧条目压成摘要行（保证不卡死主链路）
        for e in seg.iter() {
            summary_entries.push(deterministic_entry(e, backend));
        }
    }
    let after_len = summary_entries.len() + recent.len();
    let mut merged = summary_entries;
    merged.extend_from_slice(recent);
    hist.overwrite(&merged);
    Some(CompressStats {
        before: all.len(),
        after: after_len,
        kept: recent.len(),
        llm: llm_used,
    })
}

/// 确定性摘要条目：保留首尾关键信息，超长截断（char 安全）。
fn deterministic_entry(e: &HistoryEntry, backend: Backend) -> HistoryEntry {
    let head = crate::agent::truncate(&e.text, 120);
    let text = if e.user {
        format!("[历史摘要] 用户：{head}")
    } else {
        format!("[历史摘要] 助手：{head}")
    };
    HistoryEntry {
        mid: format!("__ctxcmp_{}", e.mid),
        user: true,
        backend: backend.name().to_string(),
        text,
        ts: e.ts,
    }
}

/// 段落文本（LLM 摘要输入）：按轮标注来源；单条截断到 500 字符、整段封顶 8K 字符，
/// 防「段内单条 20K 上限的超大条目」让摘要调用自身再次超长。
fn seg_text(seg: &[HistoryEntry]) -> String {
    let mut s = String::new();
    for (i, e) in seg.iter().enumerate() {
        s.push_str(&format!(
            "[第{}条 {}] {}\n",
            i + 1,
            if e.user { "用户" } else { "助手" },
            crate::agent::truncate(&e.text, 500)
        ));
        if s.chars().count() >= 8000 {
            break;
        }
    }
    s
}

/// 把压缩后的历史渲染成注入块（[历史上下文（已压缩）] 头 + 条目行）。
/// 压缩后文件很小（摘要 + 最近几轮），无需预算切分，全量渲染。
pub fn build_block(hist: &History) -> String {
    let entries = hist.entries();
    let mut block = String::from(
        "[历史上下文（已压缩）]\n（以下为本会话历史压缩后的分段摘要与近期原文，供衔接背景；请基于最新消息继续）\n\n",
    );
    for e in &entries {
        block.push_str(if e.user { "用户: " } else { "助手: " });
        block.push_str(&e.text);
        block.push('\n');
    }
    block.push('\n');
    block
}

/// 重建重试 prompt：受限说明/指令文件保持最外层（安全不变量），压缩块插在它们之后、
/// 用户文本之前。`original` 应已剥掉旧注入历史块（agent.rs 里精确替换过）。
pub fn rebuild_retry_prompt(
    original: &str,
    restrict: bool,
    agents: &str,
    compressed_block: &str,
) -> String {
    let mut tail = original;
    if restrict {
        if let Some(r) = tail.strip_prefix(crate::config::RESTRICT_PREAMBLE) {
            tail = r;
        }
    }
    if !agents.is_empty() {
        if let Some(r) = tail.strip_prefix(agents) {
            tail = r;
        }
    }
    let mut out = String::new();
    if restrict {
        out.push_str(crate::config::RESTRICT_PREAMBLE);
    }
    if !agents.is_empty() {
        out.push_str(agents);
    }
    out.push_str(compressed_block);
    out.push_str(tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_history(dir: &str, key: &str) -> (History, std::path::PathBuf) {
        let base =
            std::env::temp_dir().join(format!("abb-ctxcmp-{}-{}", uuid::Uuid::new_v4(), dir));
        let _ = std::fs::create_dir_all(&base);
        (History::open_in(&base, key), base)
    }

    #[test]
    fn overflow_patterns_match_backend_errors() {
        // claude
        assert!(is_context_overflow("Error: prompt is too long"));
        assert!(is_context_overflow("maximum context length exceeded"));
        assert!(is_context_overflow(
            "the conversation exceeds the context window"
        ));
        // codex
        assert!(is_context_overflow("context_length_exceeded"));
        assert!(is_context_overflow("Token limit reached for this model"));
        // 大小写不敏感
        assert!(is_context_overflow("PROMPT IS TOO LONG"));
        // 不误伤限流/网络/普通错误
        assert!(!is_context_overflow("rate limit exceeded, retry later"));
        assert!(!is_context_overflow("connection timed out"));
        assert!(!is_context_overflow("permission denied"));
        assert!(!is_context_overflow(""));
    }

    #[tokio::test]
    async fn compress_keeps_recent_rounds_and_summarizes_old_deterministically() {
        let (h, base) = temp_history("det", "oc_x");
        for i in 0..16 {
            h.append_user(&format!("u{i}"), "claude", &format!("问题 {i}"));
            h.append_assistant(&format!("u{i}"), "claude", &format!("回答 {i}"));
        }
        // 32 条 → 保留 2 轮（4 条），28 条旧条目确定性压缩
        let stats = compress(&h, Backend::Claude, 2, false)
            .await
            .expect("应压缩");
        assert_eq!(stats.before, 32);
        assert_eq!(stats.kept, 4);
        assert!(!stats.llm, "use_llm=false 不走 LLM");
        let entries = h.entries();
        assert_eq!(entries.len(), stats.after);
        // 最近 2 轮原文完整保留
        assert!(entries.iter().any(|e| e.text.contains("问题 15")));
        assert!(entries.iter().any(|e| e.text.contains("回答 15")));
        assert!(entries.iter().any(|e| e.text.contains("问题 14")));
        // 旧条目成为 [历史摘要] 行（用户/助手均有）
        let old_texts: Vec<&str> = entries
            .iter()
            .map(|e| e.text.as_str())
            .filter(|t| t.starts_with("[历史摘要]"))
            .collect();
        assert!(old_texts.iter().any(|t| t.contains("用户")));
        assert!(old_texts.iter().any(|t| t.contains("助手")));
        // 备份存在（审计/回溯）
        let backups: Vec<_> = std::fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("precompress"))
            .collect();
        assert_eq!(backups.len(), 1, "压缩前应备份原始历史");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn compress_skips_short_history() {
        let (h, base) = temp_history("short", "oc_y");
        h.append_user("u1", "claude", "问题 1");
        h.append_assistant("u1", "claude", "回答 1");
        let stats = compress(&h, Backend::Claude, 6, false).await;
        assert!(stats.is_none(), "历史不足 keep+4 条不压缩");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rebuild_prompt_keeps_safety_blocks_outermost() {
        let agents = "[指令文件]\n（三级 AGENTS.md）\n\n规则\n\n";
        let original = format!("{}{}用户你好", crate::config::RESTRICT_PREAMBLE, agents);
        let block = "[历史上下文（已压缩）]\n（说明）\n\n用户: 摘要\n\n";
        let out = rebuild_retry_prompt(&original, true, agents, block);
        // 受限说明在最前，其次指令文件，再压缩块，最后用户文本
        assert!(out.starts_with(crate::config::RESTRICT_PREAMBLE));
        let i_restrict = out.find("[受限模式]").unwrap();
        let i_agents = out.find("[指令文件]").unwrap();
        let i_block = out.find("[历史上下文（已压缩）]").unwrap();
        let i_user = out.find("用户你好").unwrap();
        assert!(
            i_restrict < i_agents && i_agents < i_block && i_block < i_user,
            "顺序：受限 > 指令文件 > 压缩块 > 用户文本"
        );
        assert!(!out.contains("（以下是本会话切换前"), "旧注入块头不应残留");
    }

    #[test]
    fn rebuild_prompt_without_restrict_and_agents() {
        let original = "直接问个问题";
        let block = "[历史上下文（已压缩）]\n（说明）\n\n用户: 摘要\n\n";
        let out = rebuild_retry_prompt(original, false, "", block);
        assert_eq!(out, format!("{block}直接问个问题"));
    }

    #[test]
    fn build_block_renders_summary_and_recent_entries() {
        let (h, base) = temp_history("block", "oc_z");
        h.append_user("u1", "claude", "旧问题");
        h.append_assistant("u1", "claude", "旧回答");
        let block = build_block(&h);
        assert!(block.starts_with("[历史上下文（已压缩）]"));
        assert!(block.contains("用户: 旧问题"));
        assert!(block.contains("助手: 旧回答"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
