//! 图规格 → charton 图对象。
//!
//! 这里是「薄映射层」：把 JSON 里的一小片声明式词汇翻成 charton 的 builder 调用。
//! charton 只提供 Rust builder API（没有 serde 规格），所以这层必须自己写。
//!
//! ## 为什么 mark 用字符串白名单而不是 `serde` 枚举
//!
//! 模型经常写出近似词（`scatter`/`column`/`histogram`）。用一个显式别名表 + 未知值
//! **列出可选项**报错，比反序列化失败（只说 "unknown variant"）对模型友好得多。
//!
//! ## 上游坑（本模块的两处规避，都有实测依据）
//!
//! - **柱状图的 x 必须是离散（分类）列**。实测：线性 x 直接报
//!   `Encoding("bar chart expects [Discrete] scale for channel X, but found Linear")`。
//!   所以 x 是数值列时会给出**可操作的**错误（提示用 `types` 覆盖成分类），而不是把
//!   charton 的原文抛给模型。
//! - 字体族必须逐个字段设成**单个族名**：PNG 后端全图只取一份字体（`get_raster_font`），
//!   四个 family 字段少设一个，那条通道就还是豆腐。见 [`crate::fonts`]。

use charton::prelude::*;

use crate::spec::Spec;

/// 一个通道在这个 mark 下的要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// 用不上（给了也忽略不了：会上报冲突）。
    No,
    /// 可给可不给（如小提琴图的 x：不给就画成单列）。
    Optional,
    /// 必须给。
    Required,
    /// 由 mark 自动生成，数据里**没有**这一列（如直方图的 `count`）。
    Generated(&'static str),
}

/// 图种规划。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkPlan {
    /// 规范名。
    pub name: &'static str,
    pub x: Channel,
    pub y: Channel,
}

/// 解析 mark 名（含常见别名）并给出通道要求。
///
/// **通道要求是实测出来的，不是照着直觉写的**。逐条实测记录（charton 0.8.4）：
///
/// | mark | 只给 x | 只给 y | x+y | 说明 |
/// | --- | --- | --- | --- | --- |
/// | hist | ✗ `hist chart requires both x and y` | ✗ | ✓（y = `count`） | 直方图要 x+y，**y 是转换生成的 `count` 列**，数据里不存在 |
/// | density | ✓ | ✗ | ✓ | 密度图只要 x（密度生成到 y） |
/// | violin | ✗ | ✓ | ✓ | 小提琴图只要 y（x 由 recipe 生成）；x 给了也可 |
/// | boxplot | ✗ | ✗ `boxplot chart requires both x and y` | ✓ | 箱线图要 x+y |
/// | rule | ✗ | ✗ `rule chart requires both x and y` | ✓ | 参考线要 x+y |
/// | point/line/bar/area/errorbar/text | ✗ | ✗ | ✓ | 双变量 |
///
/// 「hist 只要 x」「rule 只要 y」是最容易凭直觉写错的两条——所以这里按实测表来，
/// 并用单测钉住（`mark_channel_requirements_match_empirical_probe`）。
pub fn plan_mark(raw: &str) -> Result<MarkPlan, String> {
    let key = raw.trim().to_ascii_lowercase();
    let (name, x, y) = match key.as_str() {
        "point" | "scatter" | "dot" => ("point", Channel::Required, Channel::Required),
        "line" | "curve" | "trend" => ("line", Channel::Required, Channel::Required),
        "bar" | "column" => ("bar", Channel::Required, Channel::Required),
        "area" => ("area", Channel::Required, Channel::Required),
        "hist" | "histogram" => ("hist", Channel::Required, Channel::Generated("count")),
        "density" | "kde" => ("density", Channel::Required, Channel::No),
        "box" | "boxplot" | "box-plot" => ("box", Channel::Required, Channel::Required),
        "violin" => ("violin", Channel::Optional, Channel::Required),
        "errorbar" | "error-bar" => ("errorbar", Channel::Required, Channel::Required),
        "rule" | "hline" | "vline" => ("rule", Channel::Required, Channel::Required),
        "text" | "label" => ("text", Channel::Required, Channel::Required),
        other => {
            return Err(format!(
                "不认识的 mark「{other}」。可用：point（散点）、line（折线）、bar（柱状）、\
                 area（面积）、hist（直方图）、density（密度）、box（箱线）、violin（小提琴）、\
                 errorbar（误差棒）、rule（参考线）、text（文字标注）"
            ))
        }
    };
    Ok(MarkPlan { name, x, y })
}

/// 兼容旧名（返回 `(规范名, 是否需要 x, 是否需要 y)`）。
pub fn resolve_mark(raw: &str) -> Result<(&'static str, bool, bool), String> {
    let plan = plan_mark(raw)?;
    Ok((
        plan.name,
        plan.x == Channel::Required,
        plan.y == Channel::Required,
    ))
}

/// 建图。`ds` 必须已经按规格装载好；`size` 是**最终输出的像素尺寸**（已由调用方解析
/// 完「命令行 > 规格 > 缺省」并校验过，这里不再自己猜一遍——那正是曾经让
/// `--width` 生效不了的原因）。
pub fn build(spec: &Spec, ds: Dataset, size: (u32, u32)) -> Result<LayeredChart, String> {
    let plan = plan_mark(&spec.mark)?;
    let mark = plan.name;
    let x_col = spec.x.as_deref().map(str::trim).unwrap_or("");
    let y_col = spec.y.as_deref().map(str::trim).unwrap_or("");

    if plan.x == Channel::Required && x_col.is_empty() {
        return Err(format!("mark `{mark}` 需要 `x`（要画哪一列）"));
    }
    if plan.y == Channel::Required && y_col.is_empty() {
        return Err(format!("mark `{mark}` 需要 `y`（要画哪一列）"));
    }
    // 直方图的 y 是转换生成的 `count`（数据里没有这一列）。用户显式给了一个**别的**列名时
    // 直接报错，而不是默默忽略——否则他以为自己在按某个列画，实际画的是计数。
    if let Channel::Generated(generated) = plan.y {
        if !y_col.is_empty() && y_col != generated {
            return Err(format!(
                "mark `{mark}` 的 `y` 由渲染器自己生成（固定叫 `{generated}`），数据里没有这一列；\
                 请不要给 `y`（`{mark}` 只需要 `x` = 要统计的那一列）。"
            ));
        }
    }

    // `errorbar` 只给 `y` 会**静默只画一个点**（实测：没有任何误差棒，图看着像坏了）；
    // `text` 不给文字列会画出**空标签**（实测：三个空串）。这两种「画得出来但没意义」的
    // 组合必须硬错——本功能最容易的失败模式就是“出了张图，内容是错的”。
    if mark == "errorbar" && spec.y2.as_deref().map(str::trim).is_none_or(str::is_empty) {
        return Err(
            "mark `errorbar` 需要 `y2`（区间上界列）：只给 `y` 会只画一个点、没有误差棒。\n\
             例如数据有 `均值`/`下界`/`上界` 三列时写 \"y\": \"均值\", \"y2\": \"上界\"；\n\
             若下界与上界是两列（没有单个上界列），请改用 `rule` 或先把区间整理成 y/y2 两列。"
                .into(),
        );
    }
    if mark == "text"
        && spec
            .text
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    {
        return Err(
            "mark `text` 需要 `text`（要标注的那一列列名）：不给会画出空标签。\n\
             例如 \"text\": \"名称\" 会把该列的值写在 (x, y) 位置上。"
                .into(),
        );
    }

    // 分组轴必须是离散列的图种：提前给出可操作的提示，别把 charton 的原文
    // （"expects [Discrete] scale for channel X, but found Linear"）抛给模型——
    // 它不知道「离散」对应规格里的哪个开关。
    if matches!(mark, "bar" | "box" | "violin")
        && !x_col.is_empty()
        && column_is_numeric(&ds, x_col)
    {
        return Err(format!(
            "mark `{mark}` 的 `x` 列「{x_col}」是数值列，但这类图的分组轴必须是分类轴。\
             若它其实是名义型数字（年份/月份/编号），在规格里加：\
             `\"types\": {{ \"{x_col}\": \"category\" }}`。"
        ));
    }

    // 编码：按通道的存在情况选 1/2/3 元组（charton 支持元组编码）。
    let mut base = Chart::build(ds).map_err(|e| format!("数据集装载失败：{e}"))?;
    let use_x = matches!(plan.x, Channel::Required | Channel::Optional) && !x_col.is_empty();
    let y_e = match plan.y {
        Channel::No => None,
        Channel::Generated(name) => Some(y_enc(name, None)),
        Channel::Required => Some(y_enc(y_col, spec.y_label.as_deref())),
        // Optional 的 y 目前没有图种用到（violin 的 y 是 Required）；给了就当 Required 处理。
        Channel::Optional => (!y_col.is_empty()).then(|| y_enc(y_col, spec.y_label.as_deref())),
    };
    let x_e = use_x.then(|| x_enc(x_col, spec.x_label.as_deref()));

    base = match (x_e, y_e, spec.color.as_deref()) {
        (Some(xe), Some(ye), Some(c)) => base
            .encode((xe, ye, alt::color(c)))
            .map_err(|e| format!("编码失败（x/{x_col}、y/{y_col}、color/{c}）：{e}"))?,
        (Some(xe), Some(ye), None) => base
            .encode((xe, ye))
            .map_err(|e| format!("编码失败（x/{x_col}、y/{y_col}）：{e}"))?,
        (Some(xe), None, Some(c)) => base
            .encode((xe, alt::color(c)))
            .map_err(|e| format!("编码失败（x/{x_col}、color/{c}）：{e}"))?,
        (Some(xe), None, None) => base
            .encode((xe,))
            .map_err(|e| format!("编码失败（x/{x_col}）：{e}"))?,
        (None, Some(ye), _) => base
            .encode((ye,))
            .map_err(|e| format!("编码失败（y/{y_col}）：{e}"))?,
        (None, None, _) => return Err("规格既没有 x 也没有 y，没有可画的通道".into()),
    };

    // `y2` / `text` 用**链式 encode** 补上（charton 的 `IntoEncoding::apply` 是合并语义：
    // 每次只设自己的通道，所以多次调用可以叠加，不必把它们塞进巨大的元组组合里）。
    if let Some(y2) = spec.y2.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        base = base
            .encode((alt::y2(y2),))
            .map_err(|e| format!("编码失败（y2/{y2}）：{e}"))?;
    }
    if let Some(text) = spec
        .text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        base = base
            .encode((alt::text(text),))
            .map_err(|e| format!("编码失败（text/{text}）：{e}"))?;
    }

    // mark：每个分支返回不同的 `Chart<Mark…>`，各自立刻收敛成 `LayeredChart` 以便统一。
    let title = spec.title.clone().unwrap_or_default();
    let styled = |c: LayeredChart| -> LayeredChart {
        let c = if title.is_empty() {
            c
        } else {
            c.with_title(title.clone())
        };
        // 输出像素 = with_size × scale_factor；这里固定 1.0，让「参数 = 最终像素」成立。
        c.with_size(size.0, size.1).with_scale_factor(1.0)
    };

    let lc: LayeredChart = match mark {
        "point" => styled(base.mark_point().map_err(enc_err)?.into()),
        "line" => styled(base.mark_line().map_err(enc_err)?.into()),
        "bar" => styled(base.mark_bar().map_err(enc_err)?.into()),
        "area" => styled(base.mark_area().map_err(enc_err)?.into()),
        "hist" => styled(base.mark_hist().map_err(enc_err)?.into()),
        "density" => styled(base.mark_density().map_err(enc_err)?.into()),
        "box" => styled(base.mark_boxplot().map_err(enc_err)?.into()),
        "violin" => styled(base.mark_violin().map_err(enc_err)?.into()),
        "errorbar" => styled(base.mark_errorbar().map_err(enc_err)?.into()),
        "rule" => styled(base.mark_rule().map_err(enc_err)?.into()),
        "text" => styled(base.mark_text().map_err(enc_err)?.into()),
        other => return Err(format!("mark「{other}」未接线")),
    };
    Ok(lc)
}

fn enc_err(e: charton::error::ChartonError) -> String {
    format!("该 mark 与当前编码不兼容：{e}")
}

/// x 通道（可选轴标题）。
fn x_enc(col: &str, label: Option<&str>) -> charton::encode::x::X {
    match label.map(str::trim).filter(|s| !s.is_empty()) {
        Some(l) => alt::x(col).with_label(l),
        None => alt::x(col),
    }
}

/// y 通道（可选轴标题）。
fn y_enc(col: &str, label: Option<&str>) -> charton::encode::y::Y {
    match label.map(str::trim).filter(|s| !s.is_empty()) {
        Some(l) => alt::y(col).with_label(l),
        None => alt::y(col),
    }
}

/// 该列是否为数值列。
///
/// 用 charton 自己的 `get_column::<f64>` 判断（它内部就是列向量变体匹配），
/// 不去戳 `core::data::ColumnVector` 这种内部类型——上游改内部结构不该弄坏我们。
fn column_is_numeric(ds: &Dataset, name: &str) -> bool {
    ds.get_column::<f64>(name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试默认尺寸（多数用例不关心尺寸；关心尺寸的走端到端测试）。
    const T: (u32, u32) = (400, 300);

    /// 测试用建图：固定尺寸，省得每个用例都写一遍。
    fn build_sized(spec: &Spec, ds: Dataset) -> Result<LayeredChart, String> {
        build(spec, ds, T)
    }

    /// `LayeredChart` 没有 `Debug`，所以不能用 `unwrap_err()`；这个帮手把「期望失败」
    /// 写成一句明确断言（成功即 panic，失败返回消息）。
    fn err_of<T>(r: Result<T, String>) -> String {
        match r {
            Ok(_) => panic!("这一步本该失败，却成功了"),
            Err(e) => e,
        }
    }

    #[test]
    fn unknown_mark_lists_alternatives() {
        let err = resolve_mark("pie3d").unwrap_err();
        assert!(err.contains("point"), "报错要列出可选项：{err}");
    }

    #[test]
    fn mark_aliases_map_to_canonical_names() {
        assert_eq!(resolve_mark("Scatter").unwrap().0, "point");
        assert_eq!(resolve_mark("HISTOGRAM").unwrap().0, "hist");
        assert_eq!(resolve_mark("kde").unwrap().0, "density");
        assert_eq!(resolve_mark("boxplot").unwrap().0, "box");
        assert_eq!(resolve_mark("hline").unwrap().0, "rule");
    }

    /// **通道要求是实测钉住的表**（charton 0.8.4 逐条跑出来的，见 `plan_mark` 的表格）。
    /// 最容易按直觉写错的两条：hist **要** y（生成的 `count`）、rule **要** x。
    #[test]
    fn mark_channel_requirements_match_empirical_probe() {
        let p = plan_mark("hist").unwrap();
        assert_eq!((p.x, p.y), (Channel::Required, Channel::Generated("count")));
        let p = plan_mark("density").unwrap();
        assert_eq!((p.x, p.y), (Channel::Required, Channel::No));
        let p = plan_mark("violin").unwrap();
        assert_eq!((p.x, p.y), (Channel::Optional, Channel::Required));
        let p = plan_mark("rule").unwrap();
        assert_eq!((p.x, p.y), (Channel::Required, Channel::Required));
        let p = plan_mark("boxplot").unwrap();
        assert_eq!((p.x, p.y), (Channel::Required, Channel::Required));
        let p = plan_mark("point").unwrap();
        assert_eq!((p.x, p.y), (Channel::Required, Channel::Required));
    }

    fn spec(mark: &str, x: Option<&str>, y: Option<&str>) -> Spec {
        Spec {
            mark: mark.into(),
            x: x.map(str::to_string),
            y: y.map(str::to_string),
            ..Default::default()
        }
    }

    /// **回归钉子**：柱状图 + 数值 x 必须给出**可操作的**错误（提示 `types` 覆盖），
    /// 而不是把 charton 原文 `expects [Discrete] scale …` 抛出去。
    #[test]
    fn bar_with_numeric_x_suggests_type_override() {
        let t = crate::data::table_from_csv("year,v\n2024,1\n2025,2\n").unwrap();
        let ds = crate::data::table_to_dataset(&t, &Default::default()).unwrap();
        let err = err_of(build_sized(&spec("bar", Some("year"), Some("v")), ds));
        assert!(err.contains("\"types\""), "要给出 types 覆盖的写法：{err}");
        assert!(err.contains("category"), "{err}");
    }

    /// 分类 x 的柱状图正常出图对象（不报错）。
    #[test]
    fn bar_with_category_x_builds() {
        let t = crate::data::table_from_csv("month,v\n1月,1\n2月,2\n").unwrap();
        let ds = crate::data::table_to_dataset(&t, &Default::default()).unwrap();
        assert!(build_sized(&spec("bar", Some("month"), Some("v")), ds).is_ok());
    }

    #[test]
    fn missing_required_channel_is_an_error() {
        let t = crate::data::table_from_csv("v\n1\n2\n").unwrap();
        let ds = crate::data::table_to_dataset(&t, &Default::default()).unwrap();
        let err = err_of(build_sized(&spec("bar", Some("v"), None), ds));
        assert!(err.contains('y'), "{err}");
    }

    /// hist 只需要 `x`（`y` 由渲染器生成为 `count`）：不给 y 就该能建图。
    #[test]
    fn hist_needs_only_x() {
        let ds = || {
            let t = crate::data::table_from_csv("v\n1\n2\n3\n").unwrap();
            crate::data::table_to_dataset(&t, &Default::default()).unwrap()
        };
        if let Err(e) = build_sized(&spec("hist", Some("v"), None), ds()) {
            panic!("hist 只需 x，却失败：{e}");
        }
        // 给了 `count` 本身也接受（它就是生成列的名字）。
        assert!(build_sized(&spec("hist", Some("v"), Some("count")), ds()).is_ok());
        // 给了一个**别的**列名必须报错：否则用户以为按某列画，实际画的是计数。
        let err = err_of(build_sized(&spec("hist", Some("v"), Some("v")), ds()));
        assert!(err.contains("count"), "{err}");
    }

    /// density 只要 x（密度生成到 y）：连 color 都不给也能建图。
    #[test]
    fn density_needs_only_x() {
        let t = crate::data::table_from_csv("v\n1\n2\n3\n").unwrap();
        let ds = crate::data::table_to_dataset(&t, &Default::default()).unwrap();
        if let Err(e) = build_sized(&spec("density", Some("v"), None), ds) {
            panic!("density 只需 x，却失败：{e}");
        }
    }

    /// violin 只要 y（x 由 recipe 生成）——与 density 恰好相反。
    #[test]
    fn violin_needs_only_y() {
        let t = crate::data::table_from_csv("v\n1\n2\n3\n").unwrap();
        let ds = crate::data::table_to_dataset(&t, &Default::default()).unwrap();
        if let Err(e) = build_sized(&spec("violin", None, Some("v")), ds) {
            panic!("violin 只需 y，却失败：{e}");
        }
    }

    /// **实测过的两个「画得出来但没意义」的组合必须硬错**：
    /// `errorbar` 缺 `y2` ⇒ 只画一个点、没有误差棒；`text` 缺文字列 ⇒ 三个空标签。
    /// 判别力：删掉对应检查 ⇒ 这两条红。
    #[test]
    fn errorbar_without_range_and_text_without_column_are_hard_errors() {
        let t = crate::data::table_from_csv("k,v,l\na,1,甲\nb,2,乙\n").unwrap();
        let ds = || crate::data::table_to_dataset(&t, &Default::default()).unwrap();

        let err = err_of(build_sized(&spec("errorbar", Some("k"), Some("v")), ds()));
        assert!(err.contains("y2"), "errorbar 缺 y2 要指明：{err}");

        // 给了 y2 就能建图
        let mut with_y2 = spec("errorbar", Some("k"), Some("v"));
        with_y2.y2 = Some("v".into());
        if let Err(e) = build_sized(&with_y2, ds()) {
            panic!("errorbar 给全 y/y2 应能建图：{e}");
        }

        let err = err_of(build_sized(&spec("text", Some("k"), Some("v")), ds()));
        assert!(err.contains("text"), "text mark 缺文字列要指明：{err}");

        let mut with_text = spec("text", Some("k"), Some("v"));
        with_text.text = Some("l".into());
        if let Err(e) = build_sized(&with_text, ds()) {
            panic!("text mark 给全通道应能建图：{e}");
        }
    }

    /// rule 要 **x 和 y 两个**（实测：只给 y 会报
    /// `rule chart requires both x and y encodings`）。
    #[test]
    fn rule_requires_both_axes() {
        let t = crate::data::table_from_csv("k,v\na,1\nb,2\n").unwrap();
        let ds = crate::data::table_to_dataset(&t, &Default::default()).unwrap();
        if let Err(e) = build_sized(&spec("rule", Some("k"), Some("v")), ds) {
            panic!("rule 给全 x+y 应能建图：{e}");
        }
    }
}
