//! 图规格（JSON）模型与渲染编排。
//!
//! ## 规格长什么样
//!
//! ```json
//! {
//!   "title": "季度销售额",
//!   "mark": "bar",
//!   "x": "季度",
//!   "y": "销售额",
//!   "color": "区域",
//!   "data": "sales.csv",
//!   "types": { "季度": "category" },
//!   "width": 1000, "height": 600,
//!   "theme": "light",
//!   "x_label": "季度", "y_label": "销售额（万元）"
//! }
//! ```
//!
//! `data`（外部文件，路径相对规格文件所在目录）与 `rows`（内联数组）二选一；
//! 也可以什么都不给，用 `x`/`y`/`color` 之外的内联 `rows`。
//!
//! ## 尺寸语义（容易被 `scale_factor` 坑到，故写死口径）
//!
//! charton 的 `with_size(w, h)` 是**逻辑尺寸**，实际像素 = `w × scale_factor`（默认 **2.0**）。
//! 所以 `with_size(600, 400)` 产出的是 **1200×800** 的 PNG（实测 IHDR）。
//! 本 CLI 的口径是「`--width/--height` 就是**最终 PNG 的像素数**」，实现上
//! `with_size(w, h)` + `with_scale_factor(1.0)`，不做任何隐式放大——否则用户按参数
//! 算出的图会被静默放大一倍，而交付物尺寸又受平台限制（飞书图片上限）。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use charton::prelude::IntoLayered;
use serde::Deserialize;

use crate::data::{table_from_csv, table_from_json_rows, table_to_dataset, Table};
use crate::fonts;
use crate::render;

/// 未指定尺寸时的输出宽度（像素）。
pub const DEFAULT_WIDTH: u32 = 1000;
/// 未指定尺寸时的输出高度（像素）。
pub const DEFAULT_HEIGHT: u32 = 600;
/// 尺寸上下限：下限太小会挤成一团，上限防手滑写出 50000 像素打爆内存
/// （1000×600 的 RGBA 缓冲约 2.4 MB，5000×5000 就是 100 MB）。
pub const MIN_DIMENSION: u32 = 120;
pub const MAX_DIMENSION: u32 = 8000;

/// 规格（JSON）模型。除 `mark` 外全部可选。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// 图种：见 [`crate::render::resolve_mark`] 的白名单与别名。
    pub mark: String,
    pub title: Option<String>,
    /// x 列名。
    pub x: Option<String>,
    /// y 列名。
    pub y: Option<String>,
    /// 分组/着色列名。
    pub color: Option<String>,
    /// 外部数据文件（CSV）；相对规格文件所在目录解析。
    pub data: Option<String>,
    /// 内联数据：`[{"列名": 值}, …]`。
    pub rows: Option<serde_json::Value>,
    /// 列类型覆盖：`{"列名": "number"|"category"}`。
    #[serde(default)]
    pub types: BTreeMap<String, String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `light`（默认）或 `dark`。
    pub theme: Option<String>,
    /// 字体族（`--font` 覆盖它）。
    pub font: Option<String>,
    pub x_label: Option<String>,
    pub y_label: Option<String>,
}

/// 渲染选项（CLI 参数解析后的产物）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderOptions {
    /// 输出 PNG 路径。
    pub out: PathBuf,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub theme: Option<String>,
    /// `--font`（覆盖规格里的 `font`）。
    pub font: Option<String>,
    /// `--data`（覆盖规格里的 `data`）。
    pub data_override: Option<String>,
    /// 同时写出 SVG 便于本地排查（不发给用户：平台图片通道不认 svg）。
    pub debug_svg: Option<PathBuf>,
}

/// 渲染失败。
///
/// 只有一种失败形态（消息即全部信息）：本 CLI 的失败都是「用户/模型能照着改」的，
/// 加类型层次没有收益，反而让调用方多写匹配。
#[derive(Debug)]
pub struct RenderError(pub String);

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RenderError {}

impl From<String> for RenderError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// 从规格文本渲染出 PNG 字节。
///
/// `spec_dir` 用于解析规格里的相对 `data` 路径（stdin 输入时为 `None` ⇒ 相对进程 CWD）。
pub fn render_to_png(
    spec_text: &str,
    spec_dir: Option<&Path>,
    opts: &RenderOptions,
) -> Result<Vec<u8>, RenderError> {
    let spec: Spec = serde_json::from_str(spec_text)
        .map_err(|e| RenderError(format!("规格不是合法 JSON：{e}")))?;

    // 尺寸：规格 + 覆盖，再统一 clamp（越界要报错而不是静默改写）。
    let width = opts.width.or(spec.width).unwrap_or(DEFAULT_WIDTH);
    let height = opts.height.or(spec.height).unwrap_or(DEFAULT_HEIGHT);
    for (name, value) in [("width", width), ("height", height)] {
        if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&value) {
            return Err(RenderError(format!(
                "{name}={value} 超出允许范围 {MIN_DIMENSION}..={MAX_DIMENSION}。\
                 过小会挤成一团，过大会占用大量内存（尺寸就是最终 PNG 像素数）。"
            )));
        }
    }

    let table = load_table(&spec, opts, spec_dir)?;
    let ds = table_to_dataset(&table, &spec.types)?;

    // 字体：先把「图上会出现的全部字符」收集起来，再让字体层逐个校验覆盖
    //（raster 路径没有逐字回退，缺一个字形就是一个空方框）。
    let required = spec_required_chars(&spec, &table);
    let choice = fonts::resolve_default(opts.font.as_deref(), spec.font.as_deref(), &required)?;

    let mut lc = render::build(&spec, ds, (width, height))?;
    lc = apply_theme(lc, opts.theme.as_deref().or(spec.theme.as_deref()))?;
    let family = choice.family.clone();
    lc = lc.configure_theme(move |t| {
        // 四个字段都要设：PNG 后端全图只取一份字体，漏一个那条通道就还是豆腐。
        t.with_title_family(family.clone())
            .with_label_family(family.clone())
            .with_tick_label_family(family.clone())
            .with_legend_label_family(family)
    });

    if let Some(svg_path) = &opts.debug_svg {
        let svg = lc
            .to_svg()
            .map_err(|e| RenderError(format!("调试 SVG 生成失败：{e}")))?;
        std::fs::write(svg_path, svg)
            .map_err(|e| RenderError(format!("调试 SVG 写入 {} 失败：{e}", svg_path.display())))?;
    }

    lc.to_png()
        .map_err(|e| RenderError(format!("渲染失败：{e}")))
}

/// 主题：本 CLI 只开 `light` / `dark` 两档（聊天里看图，不需要更多花样）。
fn apply_theme(
    lc: charton::prelude::LayeredChart,
    theme: Option<&str>,
) -> Result<charton::prelude::LayeredChart, RenderError> {
    use charton::prelude::{Theme, ThemeMode};
    let mode = match theme.map(|s| s.trim().to_ascii_lowercase()) {
        None => return Ok(lc),
        Some(t) if t == "light" || t == "默认" => ThemeMode::Light,
        Some(t) if t == "dark" || t == "暗色" => ThemeMode::Dark,
        Some(other) => {
            return Err(RenderError(format!(
                "不认识的主题「{other}」；只支持 `light` 或 `dark`"
            )))
        }
    };
    Ok(lc.configure_theme(|t: Theme| t.with_mode(mode)))
}

/// 装载数据：`--data` > 规格 `data`（文件）> 内联 `rows`。
///
/// `--data` 放在 `rows` 之前是有意的：命令行参数是「更晚、更明确」的意图，
/// 给了它就该压过规格里写死的任何数据源（与 `--font` 压过 `font` 同一口径）。
fn load_table(
    spec: &Spec,
    opts: &RenderOptions,
    spec_dir: Option<&Path>,
) -> Result<Table, RenderError> {
    if let Some(data) = opts
        .data_override
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return table_from_csv_file(data, spec_dir);
    }
    if let Some(rows) = &spec.rows {
        return table_from_json_rows(rows).map_err(RenderError);
    }
    let Some(data) = spec
        .data
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(RenderError(
            "规格里没有数据：给 `data`（CSV 文件路径）或 `rows`（内联数组）之一".into(),
        ));
    };
    table_from_csv_file(data, spec_dir)
}

/// 读一个 CSV 文件并装载（相对路径按规格文件目录解析）。
fn table_from_csv_file(data: &str, spec_dir: Option<&Path>) -> Result<Table, RenderError> {
    let path = resolve_data_path(data, spec_dir);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        RenderError(format!(
            "数据文件读取失败：{}（{e}）。`data`/`--data` 的相对路径按规格文件所在目录解析。",
            path.display()
        ))
    })?;
    table_from_csv(&text).map_err(RenderError)
}

/// `data` 路径解析：绝对路径原样用；相对路径先相对规格文件目录（stdin 时相对 CWD）。
pub fn resolve_data_path(data: &str, spec_dir: Option<&Path>) -> PathBuf {
    let p = Path::new(data);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match spec_dir {
        Some(dir) => dir.join(p),
        None => p.to_path_buf(),
    }
}

/// 图上会出现的**全部字符**（去重，保持出现顺序）。
///
/// 覆盖范围要完整，漏一处就会出现方框：
/// - 规格里的可见文字：标题、轴标题；
/// - **列名**：柱状图/箱线图的分组轴刻度与图例名就是列名（或分类值）；
/// - **分类值**：图例项与轴刻度实际画的是它们；
/// - 数值列不用管：刻度由渲染器按字体本该有的数字生成（且 `²` 这类单位通常在轴标题里，
///   已经包含在上面）。
///
/// 去重后通常只有几十个字符，逐字查字体开销可忽略。
fn spec_required_chars(spec: &Spec, table: &Table) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    let mut push = |text: &str| {
        for c in text.chars() {
            if !fonts::is_blank(c) && !out.contains(&c) {
                out.push(c);
            }
        }
    };
    for text in [
        spec.title.as_deref(),
        spec.x_label.as_deref(),
        spec.y_label.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        push(text);
    }
    for name in &table.columns {
        push(name);
    }
    for cell in table.rows.iter().flatten() {
        if let crate::data::Cell::Text(t) = cell {
            push(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        PathBuf::from("/Volumes/DataExt/tmp/abbchart-test")
    }

    fn write_spec(name: &str, body: &str) -> PathBuf {
        let d = dir();
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn unknown_field_in_spec_is_rejected() {
        let err = render_to_png(
            r#"{"mark":"point","x":"a","y":"b","rows":[{"a":1,"b":2}],"titel":"typo"}"#,
            None,
            &RenderOptions {
                out: dir().join("x.png"),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.0.contains("titel"), "拼错的字段名要报出来：{err}");
    }

    #[test]
    fn missing_data_is_an_error() {
        let err = render_to_png(
            r#"{"mark":"point","x":"a","y":"b"}"#,
            None,
            &RenderOptions::default(),
        )
        .unwrap_err();
        assert!(err.0.contains("data"), "{err}");
    }

    #[test]
    fn out_of_range_size_is_rejected_not_clamped() {
        let err = render_to_png(
            r#"{"mark":"point","x":"a","y":"b","rows":[{"a":1,"b":2}],"width":99999}"#,
            None,
            &RenderOptions::default(),
        )
        .unwrap_err();
        assert!(err.0.contains("width"), "{err}");
    }

    #[test]
    fn unknown_theme_is_rejected() {
        let built = render::build(
            &Spec {
                mark: "point".into(),
                x: Some("a".into()),
                y: Some("b".into()),
                ..Default::default()
            },
            table_to_dataset(
                &table_from_json_rows(&serde_json::json!([{"a":1,"b":2}])).unwrap(),
                &Default::default(),
            )
            .unwrap(),
            (400, 300),
        )
        .unwrap();
        // LayeredChart 没有 Debug ⇒ 不能用 unwrap_err；手工取错误。
        let err = match apply_theme(built, Some("neon")) {
            Ok(_) => panic!("未知主题本该失败"),
            Err(e) => e,
        };
        assert!(err.0.contains("neon"), "{err}");
    }

    #[test]
    fn data_path_resolution_prefers_spec_dir() {
        let p = resolve_data_path("d.csv", Some(Path::new("/spec/dir")));
        assert_eq!(p, PathBuf::from("/spec/dir/d.csv"));
        let abs = resolve_data_path("/tmp/d.csv", Some(Path::new("/spec/dir")));
        assert_eq!(abs, PathBuf::from("/tmp/d.csv"));
    }

    /// 图上会出现的字符要**收全**：标题、轴标题、列名、分类值。
    /// 漏掉分类值就会漏掉图例里那些字（「标题英文、图例中文」是最常见的组合）。
    #[test]
    fn required_chars_collect_titles_columns_and_category_values() {
        let spec = Spec {
            mark: "bar".into(),
            title: Some("Sales".into()),
            x_label: Some("季度".into()),
            ..Default::default()
        };
        let table =
            table_from_json_rows(&serde_json::json!([{"k":"华东","v":1},{"k":"华南","v":2}]))
                .unwrap();
        let got: String = spec_required_chars(&spec, &table).into_iter().collect();
        // 标题是 "Sales"，所以要求里不该出现「销」——这条断言同时钉住「别把不该有的塞进来」。
        for needle in ['S', 'a', '季', '度', 'k', 'v', '华', '东', '南'] {
            assert!(got.contains(needle), "缺字符 {needle}（得到 {got}）");
        }
        assert!(!got.contains('销'), "标题里没有「销」，不该被要求：{got}");
    }

    /// 去重：同一个字出现多次只查一次（否则每个分类值都重复查）。
    #[test]
    fn required_chars_are_deduplicated() {
        let spec = Spec {
            mark: "bar".into(),
            title: Some("中中中".into()),
            ..Default::default()
        };
        let table = table_from_json_rows(&serde_json::json!([{"中":"中","v":1}])).unwrap();
        assert_eq!(spec_required_chars(&spec, &table), vec!['中', 'v']);
    }

    /// 纯英文图不该被要求中文字体（也不会因为「没有中文字体」而失败）。
    #[test]
    fn all_latin_spec_requires_no_cjk() {
        let spec = Spec {
            mark: "point".into(),
            title: Some("Sales".into()),
            ..Default::default()
        };
        let table = table_from_json_rows(&serde_json::json!([{"k":"east","v":1}])).unwrap();
        let chars = spec_required_chars(&spec, &table);
        assert!(!chars.iter().any(|c| fonts::is_cjk(*c)));
    }

    /// 空白不算「图上出现的字符」（否则标题末尾一个空格就能让字体校验失败）。
    #[test]
    fn required_chars_skip_blank() {
        let spec = Spec {
            mark: "point".into(),
            title: Some("a b".into()),
            ..Default::default()
        };
        let table = table_from_json_rows(&serde_json::json!([{"k":"e","v":1}])).unwrap();
        let chars = spec_required_chars(&spec, &table);
        assert!(!chars.contains(&' '));
    }

    /// 端到端：内联数据 → PNG，且尺寸 = 参数（不被 scale_factor 放大）。
    #[test]
    fn end_to_end_png_has_requested_pixel_size() {
        let png = render_to_png(
            r#"{"mark":"point","title":"Sales","x":"x","y":"y","width":320,"height":200,
                "rows":[{"x":1,"y":2},{"x":2,"y":3},{"x":3,"y":5}]}"#,
            None,
            &RenderOptions::default(),
        )
        .unwrap();
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            (320, 200),
            "输出像素必须等于参数"
        );
    }

    /// **确定性**：同一规格渲染两次必须逐字节一致（否则 golden/回归测试无法建立）。
    #[test]
    fn rendering_is_byte_deterministic() {
        let body = r#"{"mark":"bar","title":"Sales","x":"k","y":"v","width":300,"height":180,
                       "rows":[{"k":"a","v":3},{"k":"b","v":5},{"k":"c","v":2}]}"#;
        let a = render_to_png(body, None, &RenderOptions::default()).unwrap();
        let b = render_to_png(body, None, &RenderOptions::default()).unwrap();
        assert_eq!(a, b, "两次渲染必须逐字节一致");
    }

    /// **回归钉子：小数点不能被吞掉。**
    ///
    /// 背景（实测）：`PingFang SC`（macOS 中文主力字体）在 charton 的 raster 路径下会把
    /// `2.5` 画成 `25`——同一张图其余刻度正常，SVG 后端 dump 出的标签字符串也是正确的
    /// `2.5`，所以问题在字形层。本包的对策是**把 PingFang 排除在候选序之外**
    /// （见 `fonts::macos_candidates_never_include_pingfang`），而这条测试是那个对策的
    /// 效果判据：默认字体下，标题里的每个字形（含小数点）都必须各自留下独立墨迹。
    ///
    /// 判定方式：只看水平**中间 40%**（标题是居中绘制的）与顶部 25% 行，数「墨迹列分组」
    /// 个数。各字形之间有空白列，所以 `2.5` = 3 组、`25` = 2 组、`128.5` = 5 组；
    /// 一旦小数点被吞，第一组数字会少一个。
    #[test]
    fn decimal_point_survives_in_default_font() {
        fn ink_groups(png: &[u8]) -> usize {
            let img = image::load_from_memory(png).unwrap().to_luma8();
            let (w, h) = (img.width(), img.height());
            let (x0, x1) = ((w as f32 * 0.3) as u32, (w as f32 * 0.7) as u32);
            let rows = (h as f32 * 0.25) as u32;
            let mut groups = 0usize;
            let mut in_ink = false;
            for x in x0..x1 {
                let has_ink = (0..rows).any(|y| img.get_pixel(x, y).0[0] < 200);
                if has_ink && !in_ink {
                    groups += 1;
                }
                in_ink = has_ink;
            }
            groups
        }

        let render_title = |title: &str| -> Vec<u8> {
            let body = format!(
                r#"{{"mark":"point","title":"{title}","x":"x","y":"y",
                     "width":400,"height":200,
                     "rows":[{{"x":1,"y":2}},{{"x":2,"y":3}}]}}"#
            );
            render_to_png(&body, None, &RenderOptions::default()).unwrap()
        };

        let two_five = ink_groups(&render_title("2.5"));
        let two_five_nodot = ink_groups(&render_title("25"));
        assert_eq!(two_five_nodot, 2, "`25` 应是 2 个字形（基线自检）");
        assert_eq!(
            two_five, 3,
            "`2.5` 应是 3 个字形；得到 {two_five} 说明小数点被吞了（默认字体选错？）"
        );
        assert_eq!(
            ink_groups(&render_title("128.5")),
            5,
            "`128.5` 应是 5 个字形"
        );
    }

    /// `data` 指向相对路径时按规格文件目录解析（端到端，带真实文件）。
    #[test]
    fn external_csv_is_resolved_against_spec_dir() {
        let d = dir();
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("e2e.csv"), "k,v\na,1\nb,2\n").unwrap();
        let spec = write_spec(
            "e2e.json",
            r#"{"mark":"bar","x":"k","y":"v","data":"e2e.csv","width":300,"height":180}"#,
        );
        let text = std::fs::read_to_string(&spec).unwrap();
        let png = render_to_png(&text, spec.parent(), &RenderOptions::default()).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "应为 PNG");
    }
}
