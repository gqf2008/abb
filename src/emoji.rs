//! 系统对话「表情」面板的**纯逻辑**：内置常用 emoji 分组 + 一屏分页。
//!
//! 为什么不引依赖 / 不读系统 emoji 数据：面板只需要「一屏 40 个左右、可分组、可翻页」，
//! 引 emoji 库会带来数据体积与版本漂移；这里手写一份**只含单码位**的常用集合即可。
//!
//! ## 单码位不变式（为什么重要）
//!
//! 每个 emoji 都必须是**恰好一个 Unicode 字符**：组合序列（ZWJ 家庭/职业，如 👨‍👩‍👧）与
//! 变体选择序列（如 U+2764 + U+FE0F）在 Slint 的一个文本格里会被拆成多个字形，宽度/居中都会坏；
//! 无头软件渲染的测试字体也不认这些序列（会画成 tofu 或半截）。本模块的单测把这条钉死
//!（负向：塞一个带 U+FE0F 的串 ⇒ 红）。
//!
//! ## 与 UI 的分工
//!
//! 分组/翻页/裁剪（越界夹取）都在这里算（可单测）；`ui/app.slint` 只负责把
//! [`page_cells`] 的结果摊到 8×4 的网格上——**不在 .slint 里重算分页**。

/// 网格列数（`.slint` 里 `col: Math.mod(idx, GRID_COLS)` 必须与它一致）。
pub const GRID_COLS: usize = 8;
/// 网格行数（`.slint` 里 `row: idx / GRID_COLS` 的上界）。
pub const GRID_ROWS: usize = 4;
/// 每页格子数（= [`GRID_COLS`] × [`GRID_ROWS`]，**单一事实源**，不要另写 32）。
pub const PAGE_SIZE: usize = GRID_COLS * GRID_ROWS;

/// 一个 emoji 分组。
pub struct Category {
    /// 面板分类 tab 上的短名。
    pub name: &'static str,
    /// 该组的 emoji（每个必须是**单码位**，见模块头注）。
    pub emojis: &'static [&'static str],
}

/// 内置分组（顺序 = 面板 tab 顺序）。
pub static CATEGORIES: &[Category] = &[
    Category {
        name: "常用",
        emojis: &[
            "😀", "😃", "😄", "😁", "😆", "😅", "😂", "🤣", "😊", "🙂", "🙃", "😉", "😌", "😍",
            "🥰", "😘", "😋", "😛", "😜", "🤪", "🤗", "🤔", "🤨", "😐", "😑", "😶", "🙄", "😏",
            "😮", "😯", "😪", "😴", "😷", "🤒", "🤕", "🤢", "🤧", "🥳", "🥺", "😢",
        ],
    },
    Category {
        name: "手势",
        emojis: &[
            "👍", "👎", "👌", "✌", "✊", "👊", "👏", "🙌", "👐", "🤝", "🙏", "💪", "👈", "👉",
            "👆", "👇", "✋", "🤚", "🖐", "🖖", "👋", "🤞", "🤘", "🤙", "☝", "✍", "💅", "👀", "👁",
            "👂", "👃", "👅", "👄", "🧠", "🦷", "🦴", "🦵", "🦶", "🖕", "🤲",
        ],
    },
    Category {
        name: "动物",
        emojis: &[
            "🐶", "🐱", "🐭", "🐹", "🐰", "🦊", "🐻", "🐼", "🐨", "🐯", "🦁", "🐮", "🐷", "🐸",
            "🐵", "🐔", "🐧", "🐦", "🐤", "🦄", "🐝", "🦋", "🐌", "🐞", "🐜", "🦗", "🕷", "🦂",
            "🐢", "🐍", "🦎", "🐙", "🦑", "🦀", "🐠", "🐟", "🐡", "🐬", "🐳", "🐋",
        ],
    },
    Category {
        name: "食物",
        emojis: &[
            "🍎", "🍊", "🍋", "🍌", "🍉", "🍇", "🍓", "🍒", "🍑", "🍍", "🥝", "🍅", "🥑", "🍆",
            "🥕", "🌽", "🌶", "🥒", "🥬", "🥦", "🍄", "🥜", "🍞", "🥐", "🥖", "🧀", "🥚", "🍳",
            "🥞", "🍔", "🍟", "🍕", "🌭", "🍿", "🍩", "🍪", "🎂", "🍰", "🍫", "🍬",
        ],
    },
    Category {
        name: "符号",
        emojis: &[
            "❤", "💔", "💕", "✨", "⭐", "🌟", "💫", "💥", "💦", "🔥", "🎉", "🎊", "🎁", "🎈",
            "✅", "❌", "⚠", "❗", "❓", "💡", "🔔", "📌", "📎", "✂", "📁", "📅", "⏰", "⏳", "🔍",
            "🔒", "🔑", "📷", "🎵", "🎬", "💰", "📈", "📉", "☕", "🍺", "🎮",
        ],
    },
];

/// 分类 tab 文案（按 [`CATEGORIES`] 顺序）。
pub fn category_names() -> Vec<&'static str> {
    CATEGORIES.iter().map(|c| c.name).collect()
}

/// 分类数。
pub fn category_count() -> usize {
    CATEGORIES.len()
}

/// 把分类下标夹进合法范围（面板状态由 Rust 持有，越界一律回落到 0）。
pub fn clamp_cat(cat: i32) -> usize {
    if cat < 0 {
        0
    } else {
        (cat as usize).min(category_count().saturating_sub(1))
    }
}

/// 某分类的总页数（空分类按 1 页算，避免除零/0 页）。
pub fn page_count(cat: usize) -> usize {
    match CATEGORIES.get(cat) {
        Some(c) if !c.emojis.is_empty() => c.emojis.len().div_ceil(PAGE_SIZE),
        _ => 1,
    }
}

/// 把页码夹进该分类的合法范围。
pub fn clamp_page(cat: usize, page: i32) -> usize {
    let last = page_count(cat) - 1;
    if page < 0 {
        0
    } else {
        (page as usize).min(last)
    }
}

/// 某分类某页的 emoji（**只含该页实际内容**，末页可不足 [`PAGE_SIZE`] 个）。
pub fn page_cells(cat: usize, page: usize) -> Vec<&'static str> {
    let Some(c) = CATEGORIES.get(cat) else {
        return Vec::new();
    };
    let page = clamp_page(cat, page as i32);
    let start = page * PAGE_SIZE;
    c.emojis
        .iter()
        .skip(start)
        .take(PAGE_SIZE)
        .copied()
        .collect()
}

/// 翻页行的页码文案，如 `"1/2"`（页码从 1 起，与面板显示一致）。
pub fn page_label(cat: usize, page: usize) -> String {
    let page = clamp_page(cat, page as i32);
    format!("{}/{}", page + 1, page_count(cat))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 承重不变式：每个 emoji 都必须是**恰好一个 Unicode 字符**。
    /// - 正例：内置全集都通过。
    /// - 负向（判别力）：把 `"❤️"`（U+2764 + U+FE0F）当成一个「字符」塞进来 ⇒ 本用例如实报错。
    #[test]
    fn emoji_are_single_codepoint() {
        fn check(list: &[&str], who: &str) {
            for e in list {
                assert_eq!(
                    e.chars().count(),
                    1,
                    "{who} 里的 {e:?} 不是单码位（组合序列/变体选择会拆字形）"
                );
            }
        }
        for c in CATEGORIES {
            check(c.emojis, c.name);
        }
        // 负向对照：上面那个断言真的能拒绝多码位串。
        let bad = "❤️"; // 常被写成一个整体，实为 2 个码位
        assert_eq!(bad.chars().count(), 2, "该负向样本的码位数本身变了");
        assert!(
            std::panic::catch_unwind(|| check(&[bad], "负向")).is_err(),
            "多码位样本竟未触发断言 ⇒ 上面的不变式是空转"
        );
    }

    /// 同一分类内不得有重复 emoji（重复会让面板看起来「少了一个」）。
    #[test]
    fn emoji_unique_within_category() {
        for c in CATEGORIES {
            let mut seen = std::collections::HashSet::new();
            for e in c.emojis {
                assert!(seen.insert(*e), "{} 里 {e} 重复", c.name);
            }
        }
    }

    /// 分页必须**不重不漏**地覆盖整组：逐页取 cells 拼起来 == 原集合。
    /// 判别力（负向）：把 [page_cells] 的 `skip(page * PAGE_SIZE)` 改成 `skip(0)` ⇒ 本测红
    /// （首页会盖住后面的页、拼接后缺项）。
    #[test]
    fn pages_cover_all_without_gap_or_dupe() {
        for (i, c) in CATEGORIES.iter().enumerate() {
            let mut flat: Vec<&str> = Vec::new();
            for p in 0..page_count(i) {
                flat.extend(page_cells(i, p));
            }
            assert_eq!(flat, c.emojis, "分类「{}」分页拼接与原集合不一致", c.name);
        }
    }

    /// 末页长度 = 总数 % PAGE_SIZE（整除时 = PAGE_SIZE），页数 = ceil。
    #[test]
    fn last_page_length_matches_remainder() {
        for (i, c) in CATEGORIES.iter().enumerate() {
            let pages = page_count(i);
            let last = page_cells(i, pages - 1);
            let expect = if c.emojis.len() % PAGE_SIZE == 0 {
                PAGE_SIZE
            } else {
                c.emojis.len() % PAGE_SIZE
            };
            assert_eq!(last.len(), expect, "分类「{}」末页长度不对", c.name);
            // 除末页外都必须是满页
            for p in 0..pages.saturating_sub(1) {
                assert_eq!(page_cells(i, p).len(), PAGE_SIZE);
            }
        }
    }

    /// 越界夹取：分类越界回落 0，页码越界夹到首/末页；page_label 与夹取后的页一致。
    /// 判别力（负向）：去掉 [clamp_cat] 的 `< 0` 分支 ⇒ 负数分类会 panic/越界，本测红。
    #[test]
    fn out_of_range_clamps() {
        assert_eq!(clamp_cat(-1), 0);
        assert_eq!(clamp_cat(9999), category_count() - 1);
        assert_eq!(clamp_page(0, -5), 0);
        assert_eq!(clamp_page(0, 9999), page_count(0) - 1);
        assert_eq!(
            page_label(0, 9999),
            format!("{}/{}", page_count(0), page_count(0))
        );
        assert!(
            page_cells(9999, 0).is_empty(),
            "非法分类应返回空页而不是 panic"
        );
    }

    /// 分组内容与 tab 名一一对应、都非空（面板至少有一屏可点）。
    #[test]
    fn categories_are_named_and_non_empty() {
        assert_eq!(category_names().len(), category_count());
        for c in CATEGORIES {
            assert!(!c.name.is_empty());
            assert!(
                c.emojis.len() >= PAGE_SIZE,
                "「{}」不足一屏（{} 个），面板会有空格",
                c.name,
                c.emojis.len()
            );
        }
    }

    /// `ui/app.slint` 的网格常量必须与这里的 [`GRID_COLS`] / [`GRID_ROWS`] 对齐
    /// （`.slint` 里写的是字面量，改了 Rust 不改 Slint ⇒ 格子数与每页数脱节，视觉静默错位）。
    ///
    /// 判别力（负向）：把 slint 里的 `idx / 8` 改成 `idx / 7` ⇒ 本用例红。
    #[test]
    fn slint_grid_matches_rust_constants() {
        let raw = crate::platform::src_lf(include_str!("../ui/app.slint"));
        assert!(
            raw.contains(&format!("row: idx / {GRID_COLS};")),
            "app.slint 的行号除法与 GRID_COLS({GRID_COLS}) 不一致"
        );
        assert!(
            raw.contains(&format!("col: Math.mod(idx, {GRID_COLS});")),
            "app.slint 的列号取模与 GRID_COLS({GRID_COLS}) 不一致"
        );
        // 页大小不是随手写的常量：必须是列×行。
        assert_eq!(PAGE_SIZE, GRID_COLS * GRID_ROWS);
    }
}
