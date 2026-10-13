//! 字体族解析：把「用哪个字体出图」变成一件**可见**的事。
//!
//! 为什么需要这一层（四条都是实测出来的，不是推测）：
//!
//! 1. **charton 的默认主题在 raster 路径下中文必出豆腐块。** 主题里的 family 是 CSS 栈
//!    字符串（`"Inter, -apple-system, …, 'PingFang SC', 'Microsoft YaHei', …, sans-serif"`），
//!    而 raster 后端的 `get_raster_font` 把**整串**当**一个族名**丢给
//!    `fontdb::Family::Name(...)` ⇒ 永远匹配不上 ⇒ 回落内置 Inter（只有拉丁字形）。
//!    对策：把主题的 `title_family` / `label_family` / `tick_label_family` /
//!    `legend_label_family` 四个字段都设成**单个真实族名**（PNG 后端全图只取一份字体，
//!    所以四个都要设，少一个那条通道就还是豆腐）。
//!
//! 2. **不存在的族名静默回落 Inter。** 实测：`--font NoSuchFontXYZ123` 的输出与默认主题
//!    **逐字节相同**（sha256 一致）。所以「给了族名」不等于「字出来了」。
//!
//! 3. **存在但没有中文字形的族名，同样是静默豆腐。** 实测：`--font Helvetica`（本机有、
//!    无 CJK 字形）⇒ 出一排空心宽框。这是比第 2 条更容易踩的坑：族名是真实存在的，
//!    一切看着都对。所以判据不能是「字体存在」，必须是**「这份字体有这些字的字形」**。
//!
//! 4. **`PingFang SC` 会吞小数点。** 实测：x 轴刻度 `2.0`/`2.5` 在该字体下渲染成 `20`/`25`
//!    （同一张图其余刻度正常；`Hiragino Sans GB` / `Heiti SC` 正常）。SVG 后端 dump 出的
//!    标签字符串是正确的 `2.5`，故问题在 raster 字形层。故 macOS 候选序**排除**它。

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
    /// 该族名的字体**是否有这个字符的字形**（无该族名或取不到字形数据 ⇒ false）。
    fn covers(&self, family: &str, sample: char) -> bool;
}

/// 平台默认候选序（**按优先级**，前面优先）。
///
/// macOS 侧 `PingFang SC` 被**故意排除**：它在本仓库依赖的 charton raster 路径下会吞小数点
/// （见模块文档第 4 条）。`Hiragino Sans GB` 是 macOS 自带、覆盖简体中文、且实测小数点正常。
pub fn default_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "Hiragino Sans GB",
            "Heiti SC",
            "Songti SC",
            "Arial Unicode MS",
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

/// 无 CJK 字体可用时的拉丁兜底（图里没有中文时才会用到它）。
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

/// 基于 fontdb 的真实实现。
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

    /// 真查字形：`glyph_index` 为 `None` ⇒ 这份字体画不出这个字（渲染器会画成方框）。
    ///
    /// 只用 ttf-parser 解析一次、只看一个字符，成本可忽略；换来的是「不会静默出豆腐」。
    fn covers(&self, family: &str, sample: char) -> bool {
        let Some(id) = self.face_of(family) else {
            return false;
        };
        self.db
            .with_face_data(id, |data, index| {
                let Ok(face) = ttf_parser::Face::parse(data, index) else {
                    return false;
                };
                face.glyph_index(sample).is_some()
            })
            .unwrap_or(false)
    }
}

/// 解析最终字体族。
///
/// 优先级：`explicit`（`--font`）> `spec_font`（规格里的 `font`）> 平台候选序 > 拉丁兜底。
///
/// 三类硬失败（都对应用户可见的、值得拒绝出图的错误）：
/// - 显式给的族名**不存在**（上游会静默换成 Inter）；
/// - 需要中文、显式给的族名**存在但画不出中文**（上游会静默出方框）；
/// - 需要中文、候选序里**没有任何**既存在又能画中文的字体。
///
/// `sample`：需要验证的中文字符（`None` = 图里没有中文，无需中文能力）。
pub fn resolve(
    explicit: Option<&str>,
    spec_font: Option<&str>,
    candidates: &[&str],
    sample: Option<char>,
    probe: &dyn FontProbe,
) -> Result<FontChoice, String> {
    // 候选序里第一个「该有的能力都有」的字体（列出来给用户做建议）。
    let usable: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|c| probe.exists(c) && sample.is_none_or(|ch| probe.covers(c, ch)))
        .collect();
    let suggestion = || {
        if usable.is_empty() {
            "本机候选序里也没有可用的中文字体；请安装一个中文字体后用 `--font <族名>` 指定"
                .to_string()
        } else {
            format!("本机可用：{}", usable.join("`、`"))
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
                 按平台默认挑选。注意：不存在的族名在渲染层会被**静默**换成内置的西文字体\
                 （中文会变成方框），所以这里直接拒绝。{}",
                suggestion()
            ));
        }
        if let Some(ch) = sample {
            if !probe.covers(requested, ch) {
                return Err(format!(
                    "字体族「{requested}」在本机存在，但**没有中文字形**（用它渲染「{ch}」会得到方框）。\
                     请换成含中文字形的字体。{}",
                    suggestion()
                ));
            }
        }
        return Ok(FontChoice {
            family: requested.to_string(),
            reason: "显式指定",
        });
    }

    if let Some(name) = usable.first() {
        return Ok(FontChoice {
            family: (*name).to_string(),
            reason: "平台默认候选序命中",
        });
    }

    if sample.is_some() {
        return Err(format!(
            "图里含中日韩文字，但本机没有找到任何**能画出这些字**的字体（已按序试过：{}；\
             逐个查过字形，光「字体存在」不算）。请安装一个中文字体，或用 `--font <族名>` \
             显式指定。此处**拒绝出图**：继续渲染只会得到一排方框，而用户要到聊天里看到图\
             才会发现。",
            candidates.join("`、`")
        ));
    }

    // 无中文需求：直接用拉丁兜底（charton 内置 Inter）。
    Ok(FontChoice {
        family: LATIN_FALLBACK.to_string(),
        reason: "图内无中日韩文字，用拉丁兜底",
    })
}

/// 平台默认解析（生产路径）。
pub fn resolve_default(
    explicit: Option<&str>,
    spec_font: Option<&str>,
    sample: Option<char>,
) -> Result<FontChoice, String> {
    resolve(
        explicit,
        spec_font,
        default_candidates(),
        sample,
        system_probe(),
    )
}

/// 文本里第一个 CJK（中日韩）字符——用它当「这份字体能不能画中文」的样本。
///
/// 只取一个字符做探针：同一份中文字体对汉字的覆盖是整体的，逐字验证只会白花时间。
/// 覆盖范围：CJK 统一表意（含扩展 A）、CJK 标点、假名、谚文、全角形式。
pub fn first_cjk(text: &str) -> Option<char> {
    text.chars().find(|c| is_cjk(*c))
}

/// 该字符是否属于 CJK 区段。
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
    first_cjk(text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 假字体库：`族名 → 能画出的字符集合`。空集合 = 存在但什么都画不出。
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
        fn covers(&self, family: &str, sample: char) -> bool {
            self.0
                .get(family)
                .is_some_and(|chars| chars.contains(&sample))
        }
    }

    /// 显式指定的族名不存在 ⇒ 必须报错（上游是静默回落 Inter，实测 `--font NoSuchFontXYZ123`
    /// 与默认输出逐字节相同）。这条测试就是那个实测的回归钉子。
    #[test]
    fn explicit_missing_family_is_an_error_not_a_silent_fallback() {
        let probe = Fake::new(&[("Hiragino Sans GB", "中")]);
        let err = resolve(
            Some("NoSuchFontXYZ123"),
            None,
            &["Hiragino Sans GB"],
            Some('中'),
            &probe,
        )
        .unwrap_err();
        assert!(err.contains("NoSuchFontXYZ123"), "{err}");
        assert!(err.contains("不存在"), "{err}");
        // 报错要顺带告诉对方能用什么，否则模型只能瞎猜。
        assert!(err.contains("Hiragino Sans GB"), "要给出可用建议：{err}");
    }

    /// **回归钉子**：族名存在但没有中文字形（实测 Helvetica）⇒ 必须报错。
    /// 否则输出的是一排「看着像占位符」的方框，而用户要到聊天里看到图才发现。
    #[test]
    fn existing_font_without_cjk_glyphs_is_rejected() {
        let probe = Fake::new(&[("Helvetica", "abc"), ("Heiti SC", "中")]);
        let err = resolve(Some("Helvetica"), None, &["Heiti SC"], Some('中'), &probe).unwrap_err();
        assert!(err.contains("没有中文字形"), "{err}");
        assert!(err.contains("Heiti SC"), "要建议一个能画中文的：{err}");
    }

    /// 但同一个 Helvetica，在**图里没有中文**时是合法选择（不该被拦）。
    #[test]
    fn existing_font_without_cjk_is_fine_when_no_cjk_needed() {
        let probe = Fake::new(&[("Helvetica", "abc")]);
        let choice = resolve(Some("Helvetica"), None, &[], None, &probe).unwrap();
        assert_eq!(choice.family, "Helvetica");
    }

    /// 规格里的 `font` 与 `--font` 同等对待（都不许静默回落）。
    #[test]
    fn spec_font_is_validated_too() {
        let probe = Fake::new(&[]);
        let err = resolve(None, Some("Bogus"), &[], None, &probe).unwrap_err();
        assert!(err.contains("Bogus"), "{err}");
    }

    /// `--font` 覆盖规格里的 `font`。
    #[test]
    fn explicit_font_beats_spec_font() {
        let probe = Fake::new(&[("Heiti SC", "中"), ("Hiragino Sans GB", "中")]);
        let choice = resolve(
            Some("Heiti SC"),
            Some("Hiragino Sans GB"),
            &[],
            Some('中'),
            &probe,
        )
        .unwrap();
        assert_eq!(choice.family, "Heiti SC");
        assert_eq!(choice.reason, "显式指定");
    }

    /// 候选序按优先级取第一个存在的。
    #[test]
    fn first_existing_candidate_wins() {
        let probe = Fake::new(&[("Heiti SC", "中")]);
        let choice = resolve(
            None,
            None,
            &["Hiragino Sans GB", "Heiti SC"],
            Some('中'),
            &probe,
        )
        .unwrap();
        assert_eq!(choice.family, "Heiti SC");
    }

    /// 候选序里「存在但画不出中文」的字体必须被**跳过**（换下一个能画的），
    /// 而不是因为「它存在」就选它 —— 正是 Helvetica 那一类坑。
    #[test]
    fn candidate_without_cjk_glyphs_is_skipped() {
        let probe = Fake::new(&[("Malgun Gothic", "abc"), ("Microsoft YaHei", "中")]);
        let choice = resolve(
            None,
            None,
            &["Malgun Gothic", "Microsoft YaHei"],
            Some('中'),
            &probe,
        )
        .unwrap();
        assert_eq!(choice.family, "Microsoft YaHei");
    }

    /// **macOS 候选序必须排除 `PingFang SC`**：它在本仓库依赖的 charton raster 路径下会吞小数点
    /// （实测 `2.0`/`2.5` 渲染成 `20`/`25`）。这条断言防的是「有人觉得 PingFang 更漂亮就把
    /// 它加回候选序」——那会让所有小数刻度静默出错别字。
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
        let err = resolve(None, None, &["SomeLatin"], Some('中'), &probe).unwrap_err();
        assert!(err.contains("方框"), "错误信息要说清后果：{err}");
    }

    /// 纯英文图找不到中文字体时**不该**失败——回落到拉丁字体正常出图。
    #[test]
    fn latin_only_spec_falls_back_instead_of_failing() {
        let probe = Fake::new(&[]);
        let choice = resolve(None, None, &["NoSuchCJK"], None, &probe).unwrap();
        assert_eq!(choice.family, LATIN_FALLBACK);
    }

    /// 空/纯空白参数按「没给」处理，不要当成一个叫 "" 的族名去查。
    #[test]
    fn blank_font_is_treated_as_absent() {
        let probe = Fake::new(&[("Heiti SC", "中")]);
        let choice = resolve(Some("   "), Some(""), &["Heiti SC"], Some('中'), &probe).unwrap();
        assert_eq!(choice.family, "Heiti SC");
    }

    #[test]
    fn contains_cjk_covers_the_common_cases() {
        for text in ["销售额", "趋势（月）", "売上", "매출", "，", "。"] {
            assert!(contains_cjk(text), "{text} 应判为 CJK");
        }
        for text in ["Sales", "2.5", "café", "a-b_c", ""] {
            assert!(!contains_cjk(text), "{text} 不应判为 CJK");
        }
    }

    /// 取出的样本必须是第一个 CJK 字符（探针要拿它去问字体）。
    #[test]
    fn first_cjk_returns_the_first_cjk_char() {
        assert_eq!(first_cjk("Q1 销售额"), Some('销'));
        assert_eq!(first_cjk("Sales"), None);
    }

    /// 真实系统探针的自检：本机至少有一个平台候选能画中文，且它确实被 `resolve` 选中。
    ///
    /// 这条是**唯一**依赖真实系统字体的测试；它的作用是把「候选表在真机上真的可用」
    /// 钉住——上面那些假探针测试保证不了这一点。
    #[test]
    fn real_system_has_a_usable_cjk_candidate() {
        let probe = system_probe();
        let choice = resolve(None, None, default_candidates(), Some('中'), probe);
        assert!(
            choice.is_ok(),
            "本机候选序里没有任何能画中文的字体：{choice:?}（macOS 预期 Hiragino Sans GB）"
        );
    }
}
