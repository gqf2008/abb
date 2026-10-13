//! 字体族解析：把「用哪个字体出图」变成一件**可见**的事。
//!
//! charton 的 PNG 后端只用**一份**字体渲染全图（`get_raster_font(&config.font_family)`），
//! 且**没有逐字回退**：字符在这份字体里没有字形时，画出来就是一个空方框。所以「选对字体」
//! 不是审美问题，而是出图正确性的前提。本模块把这件事收口成一个判据：
//!
//! **选中的字体必须画得出图里出现的每一个字符；画不出就以非零退出，并说明缺哪些字。**
//!
//! ## 为什么必须自己查，不能信任渲染层
//!
//! 三条都是实测出来的：
//!
//! 1. **不存在的族名静默回落内置 Inter。** 实测 `--font NoSuchFontXYZ123` 的输出与默认主题
//!    **逐字节相同**（sha256 一致）——「给了族名」完全不能证明「字出来了」。
//! 2. **存在但没有中文字形的族名，静默出方框。** 实测 `--font Helvetica`（本机有、无 CJK 字形）
//!    ⇒ 一排空心宽框。判据因此不能是「字体存在」，必须是「这份字体有这些字的字形」。
//! 3. **存在且覆盖中文的族名，也可能缺个别符号。** 实测 `Hiragino Sans GB`（macOS 自带中文
//!    主力字体）缺 `²` `³` `µ`——`销售额（m²）` 这种再普通不过的轴标题就会出一个方框，
//!    而同一张图其余文字完全正常（所以只看「有没有中文」也抓不到）。这条正是本模块最后
//!    演化成「逐字符校验」的直接原因。
//!
//! 另一条与「吞字」相邻的坑：**`PingFang SC` 会吞小数点**（实测 `2.0`/`2.5` 渲染成 `20`/`25`，
//! 同图其余刻度正常，SVG 后端 dump 出的标签字符串也是正确的 `2.5`，故问题在 raster 字形层）。
//! 故 macOS 候选序**排除** PingFang，并有像素级回归测试钉住。

use std::sync::OnceLock;

use fontdb::{Database, Family, Query, Stretch, Style, Weight};

/// 一次字体解析的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontChoice {
    /// 真正交给 charton 的单个族名。
    pub family: String,
    /// 这个选择是怎么来的（写给用户/日志看，便于排查「为什么是这个字体」）。
    pub reason: &'static str,
}

/// 「本机字体能力」探针。抽象出来是为了单测能注入假字体库：
/// 真实系统字体随机器而变，测试不能依赖本机装了哪些字体。
pub trait FontProbe {
    /// 系统里是否存在该族名。
    fn exists(&self, family: &str) -> bool;
    /// 这些字符里，该族**画不出**的（族不存在 ⇒ 全部算缺失）。
    ///
    /// 按输入顺序、去重返回，便于直接把缺失字符写进错误信息。
    fn missing(&self, family: &str, chars: &[char]) -> Vec<char>;

    /// 该族是否覆盖全部字符（`missing` 的便捷包装）。
    fn covers_all(&self, family: &str, chars: &[char]) -> bool {
        self.missing(family, chars).is_empty()
    }
}

/// 平台默认候选序（**按优先级**，前面优先）。
///
/// macOS 侧两点取舍：
/// - **`PingFang SC` 被排除**：它在 raster 路径下会吞小数点（见模块文档）。
/// - **`Heiti SC` 排在 `Hiragino Sans GB` 前面**：实测两者都覆盖中文，但 Hiragino
///   **缺 `²` `³` `µ`**（`m²` / `µm` 这类轴标题会出方框），Heiti 全覆盖。
///   注意候选序只是**优先级**：真正的判据是下面的逐字符覆盖校验，谁能覆盖全图文字就用谁
///   ——所以机器上装了哪些字体不同，也不会出现「顺序猜错就出方框」。
pub fn default_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "Heiti SC",
            "Songti SC",
            "Arial Unicode MS",
            "Hiragino Sans GB",
            "Noto Sans CJK SC",
        ]
    }
    #[cfg(target_os = "windows")]
    {
        &[
            "Microsoft YaHei",
            "SimHei",
            "Noto Sans CJK SC",
            "Malgun Gothic",
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        &[
            "Noto Sans CJK SC",
            "Noto Sans SC",
            "WenQuanYi Zen Hei",
            "Source Han Sans SC",
            "Droid Sans Fallback",
        ]
    }
}

/// 无合适系统字体时的拉丁兜底——**这是 charton 内置的字体**（不是系统字体）：
/// 它保证 ASCII/CJK 之外的基本符号可读，但没有中文字形。
///
/// 由于它不在系统字体库里，`FontProbe` 查不到它；本模块只在「图里全是 ASCII」时才敢用它
/// （ASCII 是任何真实字体都覆盖的）。
pub const LATIN_FALLBACK: &str = "Inter";

/// 真实系统字体库（扫描一次，常驻）。
///
/// `load_system_fonts` 是重 I/O，探针与多次渲染都会调；缓存避免每次渲染都扫盘。
pub fn system_probe() -> &'static SystemFonts {
    static P: OnceLock<SystemFonts> = OnceLock::new();
    P.get_or_init(|| SystemFonts {
        db: {
            let mut db = Database::new();
            db.load_system_fonts();
            db
        },
    })
}

/// 基于 fontdb + ttf-parser 的真实实现。
pub struct SystemFonts {
    db: Database,
}

impl SystemFonts {
    fn face_of(&self, family: &str) -> Option<fontdb::ID> {
        let name = family.trim();
        if name.is_empty() {
            return None;
        }
        // 先当具体族名查；`sans-serif` 这类 CSS 通用族名再按通用族查。
        let generic = match name.to_lowercase().as_str() {
            "sans-serif" => Some(Family::SansSerif),
            "serif" => Some(Family::Serif),
            "monospace" => Some(Family::Monospace),
            "cursive" => Some(Family::Cursive),
            "fantasy" => Some(Family::Fantasy),
            _ => None,
        };
        let families = [generic.unwrap_or(Family::Name(name))];
        let query = Query {
            families: &families,
            weight: Weight::NORMAL,
            stretch: Stretch::Normal,
            style: Style::Normal,
        };
        self.db.query(&query)
    }
}

impl FontProbe for SystemFonts {
    fn exists(&self, family: &str) -> bool {
        self.face_of(family).is_some()
    }

    /// 真查字形：`glyph_index` 为 `None` ⇒ 这份字体画不出这个字（渲染器会给一个空方框）。
    ///
    /// 整张图的文字只解析**一次** face，再逐字符查——系统字体查表是内存操作，成本可忽略。
    fn missing(&self, family: &str, chars: &[char]) -> Vec<char> {
        let Some(id) = self.face_of(family) else {
            return dedup(chars);
        };
        self.db
            .with_face_data(id, |data, index| {
                let Ok(face) = ttf_parser::Face::parse(data, index) else {
                    return dedup(chars);
                };
                let mut out = Vec::new();
                for c in dedup(chars) {
                    if face.glyph_index(c).is_none() {
                        out.push(c);
                    }
                }
                out
            })
            .unwrap_or_else(|| dedup(chars))
    }
}

/// 去重但保持首次出现顺序（错误信息里按图的文字顺序列缺失字符更像人话）。
fn dedup(chars: &[char]) -> Vec<char> {
    let mut seen = Vec::new();
    for c in chars {
        if !seen.contains(c) {
            seen.push(*c);
        }
    }
    seen
}

/// 视觉上等同空白的字符：不需要任何字体支持，校验时忽略。
///
/// 严格说 Tab/换行不该出现在图表文字里，但把它们当「缺失」报错只会让人困惑。
pub fn is_blank(c: char) -> bool {
    c.is_whitespace() || c == '\u{200b}'
}

/// 解析最终字体族。
///
/// 优先级：`explicit`（`--font`）> `spec_font`（规格里的 `font`）> 平台候选序 > 拉丁兜底。
///
/// **判据只有一个：选中的字体必须画得出 `required` 里的每个字符。** 三类失败都对应
/// 用户可见的、值得拒绝出图的情况：
/// - 显式给的族名不存在（上游会静默换成 Inter）；
/// - 显式给的族名存在，但缺其中某些字符（上游会静默画方框）；
/// - 候选序里没有任何一个能覆盖全图文字（此时给出「谁差哪几个字」）。
///
/// `required`：图中会出现的全部字符（轴标题/图例项/分类值都算）。为空（纯图形/无文字）
/// 时只要求族存在。
pub fn resolve(
    explicit: Option<&str>,
    spec_font: Option<&str>,
    candidates: &[&str],
    required: &[char],
    probe: &dyn FontProbe,
) -> Result<FontChoice, String> {
    let required = dedup(
        &required
            .iter()
            .copied()
            .filter(|c| !is_blank(*c))
            .collect::<Vec<_>>(),
    );

    // 可用的候选（存在且覆盖全图文字）——显式失败时用它给建议。
    let usable: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|c| probe.exists(c) && probe.covers_all(c, &required))
        .collect();
    let suggestion = || {
        if usable.is_empty() {
            "本机候选序里也没有能覆盖这些字的字体；可安装一个更全的中文字体后用 `--font <族名>` 指定"
                .to_string()
        } else {
            format!("本机可用：`{}`", usable.join("`、`"))
        }
    };

    if let Some(requested) = explicit
        .or(spec_font)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !probe.exists(requested) {
            return Err(format!(
                "字体族「{requested}」在本机不存在。请用系统里真实存在的族名，或去掉该参数让我们\
                 按图的文字自动挑。注意：不存在的族名在渲染层会被**静默**换成内置的西文字体\
                 （中文会变成方框），所以这里直接拒绝。{}",
                suggestion()
            ));
        }
        let missing = probe.missing(requested, &required);
        if !missing.is_empty() {
            return Err(format!(
                "字体族「{requested}」在本机存在，但画不出这些字符：{}（渲染出来会是空方框）。\
                 请换一个覆盖它们的字体，或把这些字符从标题/图例里去掉。{}",
                quote_chars(&missing),
                suggestion()
            ));
        }
        return Ok(FontChoice {
            family: requested.to_string(),
            reason: "显式指定",
        });
    }

    if let Some(name) = usable.first() {
        return Ok(FontChoice {
            family: (*name).to_string(),
            reason: "平台默认候选序命中（已按图的文字逐字校验覆盖）",
        });
    }

    // 没有任何系统候选能覆盖全图文字。唯一的兜底是 charton 内置的 Inter——
    // 它不在系统字体库里（查不到），但它一定覆盖 ASCII；所以只有「图里全是 ASCII」
    // 才敢用它。否则如实报错，把「谁差哪几个字」写清楚。
    if required.iter().all(|c| c.is_ascii()) {
        return Ok(FontChoice {
            family: LATIN_FALLBACK.to_string(),
            reason: "图内只有 ASCII，用内置拉丁字体",
        });
    }

    let detail: Vec<String> = candidates
        .iter()
        .filter(|c| probe.exists(c))
        .map(|c| {
            let missing = probe.missing(c, &required);
            format!("{}（缺 {}）", c, quote_chars(&missing))
        })
        .collect();
    let installed = if detail.is_empty() {
        "（本机一个候选字体都没装）".to_string()
    } else {
        format!("已装的候选字体各缺：{}", detail.join("；"))
    };
    Err(format!(
        "图里含自定义字体画不出的字符，且本机没有能覆盖它们的字体。缺的是：{}。{installed}。\
         请安装覆盖这些字符的字体（或用 `--font <族名>` 指定一个更全的），或把这些字符\
         从标题/图例/轴标题里去掉。此处**拒绝出图**：继续渲染只会得到若干空方框，\
         而用户要到聊天里看到图才会发现。",
        quote_chars(&required)
    ))
}

/// 把字符列表写成便于阅读的形态（带上码点，方框/近似字形不至于看混）。
fn quote_chars(chars: &[char]) -> String {
    chars
        .iter()
        .map(|c| format!("`{c}`(U+{:04X})", *c as u32))
        .collect::<Vec<_>>()
        .join("、")
}

/// 平台默认解析（生产路径）。
pub fn resolve_default(
    explicit: Option<&str>,
    spec_font: Option<&str>,
    required: &[char],
) -> Result<FontChoice, String> {
    resolve(
        explicit,
        spec_font,
        default_candidates(),
        required,
        system_probe(),
    )
}

/// 该字符是否属于 CJK（中日韩）区段。
///
/// 用于「图里只有 ASCII」这个兜底判据与其他调用方：CJK 字符**不可能**来自内置 Inter，
/// 所以有 CJK 就不允许走拉丁兜底。
pub fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    matches!(cp,
        0x2E80..=0x2EFF      // CJK 部首补充
        | 0x3000..=0x303F    // CJK 标点（含 、。「」）
        | 0x3040..=0x30FF    // 平假名/片假名
        | 0x3130..=0x318F    // 谚文字母
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 基本区
        | 0xAC00..=0xD7AF    // 谚文音节
        | 0xF900..=0xFAFF    // CJK 兼容表意
        | 0xFF00..=0xFFEF    // 全角形式
        | 0x20000..=0x2FA1F  // CJK 扩展 B 及以后
    )
}

/// 文本里是否含 CJK 字符。
pub fn contains_cjk(text: &str) -> bool {
    text.chars().any(is_cjk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 假字体库：`族名 → 能画出的字符集合`（未列出的字符一律算缺失）。
    struct Fake(HashMap<&'static str, Vec<char>>);

    impl Fake {
        fn new(entries: &[(&'static str, &str)]) -> Self {
            let mut m = HashMap::new();
            for (name, covers) in entries {
                m.insert(*name, covers.chars().collect());
            }
            Self(m)
        }
    }

    impl FontProbe for Fake {
        fn exists(&self, family: &str) -> bool {
            self.0.contains_key(family)
        }
        fn missing(&self, family: &str, chars: &[char]) -> Vec<char> {
            let Some(covers) = self.0.get(family) else {
                return dedup(chars);
            };
            dedup(chars)
                .into_iter()
                .filter(|c| !covers.contains(c))
                .collect()
        }
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// 显式指定的族名不存在 ⇒ 必须报错（上游是静默回落 Inter，实测 `--font NoSuchFontXYZ123`
    /// 与默认输出逐字节相同）。这条测试就是那个实测的回归钉子。
    #[test]
    fn explicit_missing_family_is_an_error_not_a_silent_fallback() {
        let probe = Fake::new(&[("Heiti SC", "中文")]);
        let err = resolve(
            Some("NoSuchFontXYZ123"),
            None,
            &["Heiti SC"],
            &chars("中文"),
            &probe,
        )
        .unwrap_err();
        assert!(err.contains("NoSuchFontXYZ123"), "{err}");
        assert!(err.contains("不存在"), "{err}");
        // 报错要顺带告诉对方能用什么，否则模型只能瞎猜。
        assert!(err.contains("Heiti SC"), "要给出可用建议：{err}");
    }

    /// **回归钉子**：族名存在但没有中文字形（实测 Helvetica）⇒ 必须报错。
    #[test]
    fn existing_font_without_cjk_glyphs_is_rejected() {
        let probe = Fake::new(&[("Helvetica", "abc"), ("Heiti SC", "中文")]);
        let err = resolve(
            Some("Helvetica"),
            None,
            &["Heiti SC"],
            &chars("中文"),
            &probe,
        )
        .unwrap_err();
        assert!(err.contains("画不出"), "{err}");
        assert!(err.contains("中"), "要说清缺哪个字：{err}");
    }

    /// **回归钉子（本模块的起因）**：字体覆盖中文但缺个别符号。
    /// 实测 `Hiragino Sans GB` 缺 `²` `³` `µ`——`销售额（m²）` 会出一个方框，
    /// 而图里其余中文完全正常，所以「有没有中文字体」这种粗判据抓不到它。
    #[test]
    fn font_covering_cjk_but_missing_a_symbol_is_rejected() {
        // Hiragino 覆盖中文但不覆盖 ²（与实测一致）；Heiti 全覆盖。
        let probe = Fake::new(&[
            ("Hiragino Sans GB", "销售额m（）"),
            ("Heiti SC", "销售额m（）²"),
        ]);
        let err = resolve(
            None,
            None,
            &["Hiragino Sans GB", "Heiti SC"],
            &chars("销售额（m²）"),
            &probe,
        )
        .unwrap();
        // 关键是**候选序不该选中 Hiragino**：它缺 ²，而 Heiti 覆盖全部。
        assert_eq!(err.family, "Heiti SC");

        // 显式指定 Hiragino 时，必须报错到「缺 ²」这一级，而不是默默画方框。
        let err = resolve(
            Some("Hiragino Sans GB"),
            None,
            &["Heiti SC"],
            &chars("销售额（m²）"),
            &probe,
        )
        .unwrap_err();
        assert!(err.contains("²"), "{err}");
        assert!(
            err.contains("U+00B2"),
            "要带码点，避免方框与近似字形看混：{err}"
        );
    }

    /// **允许的组合**：唯一的候选缺某个字符，但图里并没有用那个字符 ⇒ 正常出图。
    /// （判据必须是「图里实际用到的字符」，不是「字体够不够全」。）
    #[test]
    fn a_font_missing_unused_chars_is_fine() {
        let probe = Fake::new(&[("Hiragino Sans GB", "销售额m（）²")]);
        let choice = resolve(None, None, &["Hiragino Sans GB"], &chars("销售额"), &probe).unwrap();
        assert_eq!(choice.family, "Hiragino Sans GB");
    }

    /// 规格里的 `font` 与 `--font` 同等对待（都不许静默回落）。
    #[test]
    fn spec_font_is_validated_too() {
        let probe = Fake::new(&[]);
        let err = resolve(None, Some("Bogus"), &[], &chars("x"), &probe).unwrap_err();
        assert!(err.contains("Bogus"), "{err}");
    }

    /// `--font` 覆盖规格里的 `font`。
    #[test]
    fn explicit_font_beats_spec_font() {
        let probe = Fake::new(&[("Heiti SC", "中文"), ("Songti SC", "中文")]);
        let choice = resolve(
            Some("Heiti SC"),
            Some("Songti SC"),
            &[],
            &chars("中文"),
            &probe,
        )
        .unwrap();
        assert_eq!(choice.family, "Heiti SC");
        assert_eq!(choice.reason, "显式指定");
    }

    /// 候选序按优先级取第一个覆盖全图文字的。
    #[test]
    fn first_covering_candidate_wins() {
        let probe = Fake::new(&[("Heiti SC", "中文")]);
        let choice = resolve(
            None,
            None,
            &["Hiragino Sans GB", "Heiti SC"],
            &chars("中文"),
            &probe,
        )
        .unwrap();
        assert_eq!(choice.family, "Heiti SC");
    }

    /// 空白字符不要求有字形（否则「标题末尾有个空格」会变成字体错误）。
    #[test]
    fn blank_characters_are_not_required() {
        let probe = Fake::new(&[("Heiti SC", "中文")]);
        let choice = resolve(None, None, &["Heiti SC"], &chars("中文 \n\t"), &probe).unwrap();
        assert_eq!(choice.family, "Heiti SC");
    }

    /// **macOS 候选序必须排除 `PingFang SC`**：它在本仓库依赖的 charton raster 路径下会吞小数点
    /// （实测 `2.0`/`2.5` 渲染成 `20`/`25`）。这条断言防的是「有人觉得 PingFang 更漂亮就把它
    /// 加回候选序」——那会让所有小数刻度静默出错别字。
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_candidates_never_include_pingfang() {
        assert!(
            !default_candidates().iter().any(|c| c.contains("PingFang")),
            "PingFang SC 会吞小数点（2.5 → 25），不许进候选序：{:?}",
            default_candidates()
        );
    }

    /// 有中文但没有任何能画中文的字体 ⇒ 硬失败（而不是静默出一排方框）。
    #[test]
    fn cjk_text_without_any_cjk_capable_font_is_an_error() {
        let probe = Fake::new(&[("SomeLatin", "abc")]);
        let err = resolve(None, None, &["SomeLatin"], &chars("中文"), &probe).unwrap_err();
        assert!(err.contains("方框"), "错误信息要说清后果：{err}");
        assert!(err.contains("SomeLatin"), "要说清候选都缺什么：{err}");
    }

    /// 纯 ASCII 图在没有任何中文字体时**不该**失败——回落到内置拉丁字体正常出图。
    #[test]
    fn ascii_only_spec_falls_back_instead_of_failing() {
        let probe = Fake::new(&[]);
        let choice = resolve(
            None,
            None,
            &["NoSuchCJK"],
            &chars("Sales 2024 (Q1) 12.5%"),
            &probe,
        )
        .unwrap();
        assert_eq!(choice.family, LATIN_FALLBACK);
    }

    /// 但含非 ASCII（如 `²`）时，哪怕没有中文也不许回落到 Inter（它不一定有该字形）。
    #[test]
    fn non_ascii_without_covering_font_is_rejected_even_without_cjk() {
        let probe = Fake::new(&[]);
        let err = resolve(None, None, &["NoSuchCJK"], &chars("m²"), &probe).unwrap_err();
        assert!(err.contains("²"), "{err}");
    }

    /// 空/纯空白参数按「没给」处理，不要当成一个叫 "" 的族名去查。
    #[test]
    fn blank_font_is_treated_as_absent() {
        let probe = Fake::new(&[("Heiti SC", "中文")]);
        let choice = resolve(Some("   "), Some(""), &["Heiti SC"], &chars("中文"), &probe).unwrap();
        assert_eq!(choice.family, "Heiti SC");
    }

    /// 没有文字要渲染（纯图形）时，只要族存在即可。
    #[test]
    fn empty_requirement_only_needs_existence() {
        let probe = Fake::new(&[("Heiti SC", "")]);
        let choice = resolve(None, None, &["Heiti SC"], &[], &probe).unwrap();
        assert_eq!(choice.family, "Heiti SC");
    }

    #[test]
    fn contains_cjk_covers_the_common_cases() {
        for text in ["销售额", "趋势（月）", "売上", "매출", "，", "。"] {
            assert!(contains_cjk(text), "{text} 应判为 CJK");
        }
        for text in ["Sales", "2.5", "café", "a-b_c", "²", ""] {
            assert!(!contains_cjk(text), "{text} 不应判为 CJK");
        }
    }

    /// 码点标注只在报错时出现，形态要稳定（否则测试与用户都要重读）。
    #[test]
    fn quote_chars_includes_codepoint() {
        assert_eq!(quote_chars(&['²']), "`²`(U+00B2)");
        assert_eq!(quote_chars(&['中', 'µ']), "`中`(U+4E2D)、`µ`(U+00B5)");
    }

    /// **真实系统自检（唯一依赖本机字体的测试）**：默认候选必须能覆盖一张真实图表里
    /// 会出现的文字（中文 + 数字 + 小数 + 常见单位/排版符号）。
    ///
    /// 这条是「Hiragino 缺 `²³µ`」那个坑的守门人：候选序改回把 Hiragino 放第一时，
    /// 若本机没有更全的候选可用，这条会红。
    #[test]
    fn real_system_default_covers_a_realistic_chart_text_set() {
        let probe = system_probe();
        let required = chars(
            "销售额（万元） 2024 上半年区域对比 华东华北华南 月份 Q1 12.5% -3.2 m² µg ㎡ ℃ ≈ ≤ ≥ → — “引号”",
        );
        match resolve(None, None, default_candidates(), &required, probe) {
            Ok(choice) => {
                // 选中者必须真的覆盖（explain 一下缺什么，便于排查机器差异）。
                let missing = probe.missing(&choice.family, &required);
                assert!(
                    missing.is_empty(),
                    "选中的 {} 缺 {}",
                    choice.family,
                    quote_chars(&missing)
                );
            }
            Err(e) => panic!("本机默认候选无法覆盖真实图表文字：{e}"),
        }
    }
}
