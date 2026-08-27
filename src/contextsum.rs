//! #130 上下文超长自动分段压缩。
//!
//! 长对话会话（群聊/连续任务/多轮工具调用）累积后，后端（claude/codex/pi）会因上下文
//! 超长报错（claude "prompt is too long"、codex "context length exceeded"、pi errorMessage
//! 超长）。本模块在错误出口识别这类错误，把该会话历史按轮次分段：**旧段 → 结构化摘要**
//! （优先同后端 LLM 一次性摘要任务，失败回落确定性截断），**最近 M 轮保留原文**，
//! 产出「压缩上下文块」写入 `history/<escaped>.ctxsum`；桥随后换新会话重试本条消息，
//! 后续消息注入也优先用压缩块（原 jsonl 保留供审计，不删除）。
//!
//! 安全/降级：
//! - 识别不到特征串不触发（不误伤限流/网络错误）；每会话只压缩一次（ctxsum 存在即跳过）；
//! - 摘要任务失败 → 确定性截断兜底，绝不卡死主链路；
//! - 压缩只读当前 chat 自己的 history；摘要不跨会话/跨 bot 投递、不落日志明文；
//! - 摘要内容为用户对话的提炼，遵循与会话同样的隔离边界。

use std::path::Path;

/// 压缩后注入/落盘的块文件后缀（与 `.jsonl` 同目录，escape_key 无 '.' 可精确匹配）。
const CTXSUM_SUFFIX: &str = ".ctxsum";

/// 识别上下文超长类错误（claude/codex/pi 特征串；大小写不敏感）。
/// 刻意不用过于宽泛的 "too long"/"context" 单独匹配——限流/网络类错误文案也可能含
/// "too long"（如 "request took too long"），误触发会白白压缩历史。
pub fn is_context_too_long(e: &str) -> bool {
    let lower = e.to_lowercase();
    const PATTERNS: &[&str] = &[
        "prompt is too long",
        "maximum context length",
        "context window",
        "context length",
        "context exceeded",
        "input is too long",
        "token limit",
        "too many tokens",
        "the input length",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// 压缩报告（日志/用户提示用）。
#[derive(Debug, Clone, PartialEq)]
pub struct CompressReport {
    /// 压缩掉的旧条目数。
    pub compressed: usize,
    /// 保留原文条数。
    pub kept: usize,
    /// 生成的摘要段数。
    pub summaries: usize,
    /// 是否用了 LLM 摘要（false = 全部确定性截断兜底）。
    pub used_llm: bool,
}

/// ctxsum 文件路径：workspace/history/<escaped>.ctxsum。
pub fn ctxsum_path(workspace: &Path, key: &str) -> std::path::PathBuf {
    let esc = crate::history::escape_key(key);
    workspace
        .join("history")
        .join(format!("{esc}{CTXSUM_SUFFIX}"))
}

/// 读取压缩上下文块（注入用）。不存在 → None。
pub fn ctxsum_block_at(workspace: &Path, key: &str) -> Option<String> {
    std::fs::read_to_string(ctxsum_path(workspace, key)).ok()
}

/// 执行压缩：分段 → LLM 摘要（失败回落确定性截断）→ 保留近期原文 → 写 ctxsum。
/// 幂等：ctxsum 已存在时直接返回 None（调用方据此跳过二次压缩，防循环）。
#[allow(clippy::too_many_arguments)] // 与 agent::run 同款参数集
pub async fn compress(
    workspace: &Path,
    key: &str,
    backend: crate::agent::Backend,
    chat_id: &str,
    bot_key: &str,
    role: crate::config::SenderRole,
    runner: &dyn crate::agent::AgentRunner,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<CompressReport, String> {
    let path = ctxsum_path(workspace, key);
    if path.exists() {
        return Err("该会话已压缩过（ctxsum 已存在），跳过".into());
    }
    let cfg = crate::config::Config::load().unwrap_or_default();
    let keep_recent = cfg.context_keep_recent.max(1);
    let segment_size = cfg.context_segment_size.max(2);

    let hist = crate::history::History::open_in(&workspace.join("history"), key);
    let entries = hist.entries();
    if entries.len() <= keep_recent {
        return Err(format!(
            "历史条目 {} 条不足（保留 {} 条后无旧段可压缩）",
            entries.len(),
            keep_recent
        ));
    }
    // 分段：按「用户/助手轮对」切，避免把一轮拆进两个摘要段。
    let (old, recent) = entries.split_at(entries.len() - keep_recent);
    let pair_size = segment_size - (segment_size % 2); // 偶数（轮对）
    let pair_size = pair_size.max(2);

    let mut summaries: Vec<String> = Vec::new();
    let mut used_llm = false;
    for chunk in old.chunks(pair_size) {
        let seg_text = render_segment(chunk);
        let sum = summarize_segment(
            backend,
            chat_id,
            key,
            bot_key,
            role,
            runner,
            cancel.clone(),
            &seg_text,
        )
        .await;
        match sum {
            Some(s) if !s.trim().is_empty() => {
                used_llm = true;
                summaries.push(s.trim().to_string());
            }
            _ => {
                // 确定性截断兜底：每条旧条目 → 摘要行（截 240 字）
                let mut lines: Vec<String> = Vec::new();
                for e in chunk {
                    let who = if e.user { "用户" } else { "助手" };
                    lines.push(format!("{who}: {}", crate::agent::truncate(&e.text, 240)));
                }
                summaries.push(lines.join("；"));
            }
        }
    }

    let mut block = String::from(
        "[历史上下文·压缩版]\n（本会话上下文过长已自动压缩：旧消息已摘要，最近对话保留原文；请基于摘要与近期内容继续）\n\n",
    );
    block.push_str("## 旧对话摘要\n");
    for (i, s) in summaries.iter().enumerate() {
        block.push_str(&format!("- [段{}] {}\n", i + 1, s));
    }
    block.push_str("\n## 近期原文\n");
    for e in recent {
        let who = if e.user { "用户: " } else { "助手: " };
        block.push_str(who);
        block.push_str(&e.text);
        block.push('\n');
    }
    block.push('\n');

    let _ = std::fs::create_dir_all(workspace.join("history"));
    std::fs::write(&path, &block).map_err(|e| format!("写压缩块失败 {}: {e}", path.display()))?;

    Ok(CompressReport {
        compressed: old.len(),
        kept: recent.len(),
        summaries: summaries.len(),
        used_llm,
    })
}

/// 段文本渲染（供摘要任务输入）。
fn render_segment(chunk: &[crate::history::HistoryEntry]) -> String {
    let mut s = String::new();
    for e in chunk {
        let who = if e.user { "用户" } else { "助手" };
        s.push_str(&format!("{who}: {}\n", e.text));
    }
    s
}

/// 用同后端一次性摘要任务压缩一段；失败/取消 → None（调用方走确定性兜底）。
#[allow(clippy::too_many_arguments)] // 与 agent::run 同款参数集
async fn summarize_segment(
    backend: crate::agent::Backend,
    chat_id: &str,
    key: &str,
    bot_key: &str,
    role: crate::config::SenderRole,
    runner: &dyn crate::agent::AgentRunner,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    seg_text: &str,
) -> Option<String> {
    let prompt = format!(
        "你是 ABB 的会话摘要器。把以下对话压缩为结构化摘要，必须保留：用户意图、关键决策、\
         已达成结论、待办/未完成事项、重要事实与用户偏好。输出 3-6 行要点，纯摘要，\
         不要对话格式、不要寒暄、不要引用原文大段。\n\n{seg_text}"
    );
    let sum_sid = uuid::Uuid::new_v4().to_string();
    match runner
        .run(
            backend, &prompt, &sum_sid, false, chat_id, key, bot_key, role,
            None, // 不占会话槽位（摘要任务无回存语义）
            None, // 摘要进度不进用户可见通道
            cancel,
        )
        .await
    {
        Ok(crate::agent::RunOutcome::Reply { reply, .. }) => {
            // 摘要会话自身转录：pi 会落盘 .pi-sessions（占位 UUID 匹配不到真实 id），
            // 交给 tidy 兜底；claude/codex 转录在后端私有目录，物理不可达，跳过。
            Some(reply)
        }
        Ok(crate::agent::RunOutcome::Cancelled) => None,
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_too_long_variants() {
        for s in [
            "Error: prompt is too long",
            "claude: maximum context length exceeded",
            "context window exceeded",
            "codex: context length 128000 tokens",
            "input is too long",
            "token limit reached",
            "too many tokens in the request",
            "the input length exceeds the model's context",
        ] {
            assert!(is_context_too_long(s), "应识别: {s}");
        }
        // 不误伤：限流/网络/超时类
        for s in [
            "rate limit exceeded",
            "request took too long, timed out",
            "connection reset by peer",
            "No conversation found",
            "already in use",
        ] {
            assert!(!is_context_too_long(s), "不应识别: {s}");
        }
    }

    #[test]
    fn ctxsum_path_shape() {
        let p = ctxsum_path(Path::new("/ws"), "oc_abc");
        assert!(p.ends_with("history/oc_abc.ctxsum"));
    }

    #[test]
    fn deterministic_fallback_shape() {
        // render_segment + truncate 行为（无 LLM 路径由桥测试覆盖）
        let seg = render_segment(&[
            crate::history::HistoryEntry {
                mid: "m1".into(),
                user: true,
                backend: "codex".into(),
                text: "帮我看看这个 bug".into(),
                ts: 1,
            },
            crate::history::HistoryEntry {
                mid: "m1".into(),
                user: false,
                backend: "codex".into(),
                text: "已修复".into(),
                ts: 2,
            },
        ]);
        assert!(seg.contains("用户: 帮我看看这个 bug"));
        assert!(seg.contains("助手: 已修复"));
    }
}
