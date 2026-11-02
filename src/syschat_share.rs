//! 会话分享的数据层：把一整段会话变成「分享块序列」。
//!
//! ## 为什么单独一个模块
//!
//! 分享要用的东西和界面要用的东西**不是一回事**，混在一起最容易出的错是
//! 「看着用了同一条管线，其实读的是界面模型」。两者差在一个地方：
//!
//! - 界面模型只有 [`crate::syschat::HISTORY_KEEP_TURNS`] 那么多轮（那是**喂模型**的上界），
//! - 磁盘上的 `sessions/<id>.jsonl` 是**全量**。
//!
//! 所以分享必须读文件（`SessionStore::read_history` 本来就是全量、不截断），不能从 UI 的
//! `blocks` 模型反推。这里把「读全量 → 该丢什么」抽成纯函数，让它可离线测。
//!
//! ## 丢什么（写死，不给调用方选）
//!
//! 1. **`thought`（思考过程）不进分享**。那是模型的内部推理，用户点「分享」时的心理预期是
//!    「把我看到的这段对话发出去」，不该顺带泄露推理链。要改这条口径得有明确的用户指令，
//!    不能靠某个参数默认值悄悄打开。
//! 2. **界面 chrome 不进分享**（标题栏、状态行、用量条、输入区、分割条）。这一条由「本模块
//!    只产出消息区块序列」结构性保证——chrome 根本不在 `Turn` 里。
//!
//! ## 与渲染的关系
//!
//! 这里只产出 [`crate::mdblocks::MdBlock`]（与界面**同一条** markdown 解析管线），
//! 不负责排版。渲染层拿到的就是界面用的那种块，公式/表格/图表的渲染分支因此**天然一致**。

// 本模块在 S1（数据层）先落地，接线在 S2/S3（离屏渲染 + 子命令/GUI）。
// 移除这行 allow 的条件是**真的接上了**，而不是「先关警告」。
#![allow(dead_code)]

use crate::mdblocks::{self, MdBlock};
use crate::syschat::Turn;

/// 分享图里的一轮：角色 + 该轮正文解析出的块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareTurn {
    /// `"user"` 或 `"assistant"`（直接取 `Turn.role`，不归一）。
    pub role: String,
    /// 该轮**正文**的块序列。空正文 ⇒ 空 vec（该轮仍然出现在分享里，不吞角色行）。
    pub blocks: Vec<MdBlock>,
}

impl ShareTurn {
    /// 是否助手轮（界面的角色着色/动作行按这个分支）。
    pub fn is_assistant(&self) -> bool {
        self.role == "assistant"
    }
}

/// 把全量轮次变成分享块序列。
///
/// **纯函数**：不读盘、不看环境，因此可以在单测里直接喂构造的轮次。
///
/// `thought` 被丢弃（见模块头「丢什么」）。`HISTORY_KEEP_TURNS` 在这里**完全不参与**——
/// 传进来多少轮就产出多少轮，截断是调用方（渲染层的像素上限）的事，且必须显式记在图上。
pub fn share_blocks(turns: &[Turn]) -> Vec<ShareTurn> {
    turns
        .iter()
        .map(|t| ShareTurn {
            role: t.role.clone(),
            // 注意：**不**用 `t.thought`，也不用 `t.text + t.thought`。
            blocks: mdblocks::parse(&t.text),
        })
        .collect()
}

/// 分享块序列里是否包含任何可见内容（空会话 ⇒ 不该生成一张空图）。
pub fn is_empty(share: &[ShareTurn]) -> bool {
    share.iter().all(|t| t.blocks.is_empty())
}

/// 估算分享内容的总字符数（渲染层按它决定要不要先警告「这会是一张很长的图」）。
///
/// 只数正文文本，不数公式 TeX 与图片——那是渲染层的事，这里只给一个量级。
pub fn total_chars(share: &[ShareTurn]) -> usize {
    share
        .iter()
        .flat_map(|t| t.blocks.iter())
        .map(|b| b.text.chars().count() + b.cells.iter().map(|c| c.chars().count()).sum::<usize>())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: &str, text: &str, thought: &str) -> Turn {
        Turn {
            role: role.into(),
            text: text.into(),
            thought: thought.into(),
            ts: 0,
        }
    }

    #[test]
    fn thought_never_reaches_the_share() {
        // 这条是模块的核心承诺：分享不该顺带泄露推理链。
        let turns = vec![turn("assistant", "答案在这里", "我在想一件不该外传的事")];
        let share = share_blocks(&turns);
        let all = format!("{share:?}");
        assert!(!all.contains("不该外传"), "思考过程泄露进了分享数据：{all}");
        assert!(all.contains("答案在这里"), "正文不该丢");
    }

    #[test]
    fn every_turn_is_kept_regardless_of_ui_keep_limit() {
        // 界面只留 HISTORY_KEEP_TURNS 轮，分享必须全量。
        // 这里刻意造 3 倍于界面保留数的轮次，证明本函数不裁。
        let n = crate::syschat::HISTORY_KEEP_TURNS * 3;
        let turns: Vec<Turn> = (0..n)
            .map(|i| {
                turn(
                    if i % 2 == 0 { "user" } else { "assistant" },
                    &format!("第{i}条"),
                    "",
                )
            })
            .collect();
        let share = share_blocks(&turns);
        assert_eq!(share.len(), n, "分享必须包含全部轮次，不是界面保留的那一段");
    }

    #[test]
    fn role_is_taken_verbatim_so_ui_branching_matches() {
        // 界面按 `role == "assistant"` 分支；这里不归一大小写/别名，
        // 否则同一轮在界面与分享图里会走不同的渲染分支。
        let share = share_blocks(&[turn("assistant", "x", ""), turn("user", "y", "")]);
        assert!(share[0].is_assistant());
        assert!(!share[1].is_assistant());
    }

    #[test]
    fn empty_text_yields_no_blocks_but_keeps_the_turn() {
        // 空正文（例如只有工具调用的一轮）不该产出块，但轮次本身要留着，
        // 否则分享图里会少掉一条角色行，看起来像"对话断层"。
        let share = share_blocks(&[turn("assistant", "", "")]);
        assert_eq!(share.len(), 1);
        assert!(share[0].blocks.is_empty());
    }

    #[test]
    fn markdown_is_parsed_by_the_same_pipeline_as_the_ui() {
        // 复用 `mdblocks::parse` ⇒ 公式/表格/代码块走界面同一条分支。
        // 这里只钉住"确实解析成了结构化块"，具体渲染在渲染层测。
        let md = "## 标题\n\n| a | b |\n|:--|--:|\n| 1 | 2 |\n\n```rust\nlet x = 1;\n```\n";
        let share = share_blocks(&[turn("assistant", md, "")]);
        let kinds: Vec<&str> = share[0].blocks.iter().map(|b| b.kind.as_str()).collect();
        assert!(kinds.contains(&"h2"), "标题没解析出来：{kinds:?}");
        assert!(kinds.contains(&"table"), "表格没解析出来：{kinds:?}");
        assert!(kinds.contains(&"code"), "代码块没解析出来：{kinds:?}");
    }

    #[test]
    fn block_math_is_preserved_for_the_math_branch() {
        // 块级公式必须作为 math 块到达渲染层，**不能**退化成纯文本——
        // 卡片要求"公式正确渲染"，而渲染层是按 kind 分支的。
        let share = share_blocks(&[turn("assistant", "$$\nE = mc^2\n$$", "")]);
        let kinds: Vec<&str> = share[0].blocks.iter().map(|b| b.kind.as_str()).collect();
        assert!(kinds.contains(&"math"), "块级公式没成为 math 块：{kinds:?}");
    }

    #[test]
    fn empty_session_is_reported_as_empty() {
        // 调用方据此拒绝生成一张空图（而不是给用户一张空白 PNG）。
        assert!(is_empty(&[]));
        assert!(is_empty(&share_blocks(&[turn("assistant", "", "")])));
        assert!(!is_empty(&share_blocks(&[turn("assistant", "有内容", "")])));
    }

    #[test]
    fn total_chars_counts_body_text_and_table_cells() {
        // 判据：表格块的正文在 `cells` 里（`text` 为空）。若漏数单元格，
        // 两张内容长短差很多的表格会得到同一个数。所以用两张表对比——
        // 这比钉一个魔数稳（不依赖分隔符行/对齐列怎么解析）。
        let small = share_blocks(&[turn("assistant", "| a | b |\n|--|--|\n| c | d |", "")]);
        let big = share_blocks(&[turn(
            "assistant",
            "| aaaaaaaaaa | bbbbbbbbbb |\n|--|--|\n| cccccccccc | dddddddddd |",
            "",
        )]);
        assert!(
            total_chars(&big) > total_chars(&small),
            "表格单元格没被计入量级估算：大表 {} 不大于小表 {}",
            total_chars(&big),
            total_chars(&small)
        );
        assert_eq!(total_chars(&[]), 0, "空会话应为 0");
    }

    #[test]
    fn malformed_jsonl_lines_are_skipped_not_fatal() {
        // jsonl 是追加写的：进程被杀可能留下半行。分享不该因为一行坏数据整段失败。
        // 这条测的是「调用方怎么读文件」的契约，用 Turn 的反序列化直接验。
        let good = serde_json::to_string(&turn("user", "ok", "")).unwrap();
        let text = format!("{good}\n{{\"role\":\"user\",\"te\n\n{good}\n");
        let parsed: Vec<Turn> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Turn>(l).ok())
            .collect();
        assert_eq!(parsed.len(), 2, "坏行应被跳过、好行照常保留");
    }
}
