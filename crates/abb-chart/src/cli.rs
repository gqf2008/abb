//! 参数解析与用法文本（手写，与仓库既有 CLI 风格一致；不引 clap 以免多一棵依赖树）。
//!
//! 帮助文本是**产品的一部分**：调用方是模型，它经常先探 `--help` 再决定怎么写参数。
//! 所以帮助里要有一个能直接抄的完整例子，而不是只有旗标清单。

use std::path::PathBuf;

use crate::spec::{RenderOptions, MAX_DIMENSION, MIN_DIMENSION};

/// 解析结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Invocation {
    /// 打印帮助后以 0 退出。
    Help(String),
    /// 执行渲染。
    Render {
        /// 规格文件；`None` = 从 stdin 读。
        spec: Option<PathBuf>,
        options: RenderOptions,
    },
}

/// 用法错误（以退出码 2 结束，与「渲染失败」= 1 区分，便于脚本/agent 分辨）。
#[derive(Debug, Clone, PartialEq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// 顶层用法。
pub fn top_help() -> String {
    format!(
        "\
abb-chart —— 把 JSON 图规格 + 数据渲染成 PNG（ABB 的图形生成 CLI）

用法：
  abb-chart render --spec <spec.json|-> [选项]

子命令：
  render    渲染一张图，输出 PNG

选项（render）：
  --spec <路径|->     图规格 JSON；`-` 表示从标准输入读（需要管道，受限会话里不许用）
  --out <路径.png>    输出文件；缺省 = 规格文件同目录同名 .png（stdin 时 = ./chart.png）
  --data <路径.csv>   覆盖规格里的 data（相对路径按规格文件所在目录解析）
  --width <像素>      输出宽度，{MIN_DIMENSION}..={MAX_DIMENSION}，缺省 1000
  --height <像素>     输出高度，{MIN_DIMENSION}..={MAX_DIMENSION}，缺省 600
  --theme <light|dark>  配色，缺省 light
  --font <族名>       字体族；缺省按平台挑中文字体（见下）
  --debug-svg <路径>  额外写出 SVG（仅供本地排查，不要发给用户）

  -h, --help          帮助（`render -h` 看 render 的说明）

尺寸口径：--width/--height **就是最终 PNG 的像素数**（不做隐式 2 倍放大）。

字体：图表里的中文需要真实存在的中文字体。缺省按平台挑：
  macOS   Hiragino Sans GB → Heiti SC → Songti SC → Arial Unicode MS → Noto Sans CJK SC
  Windows Microsoft YaHei → SimHei → Noto Sans CJK SC → Malgun Gothic
  Linux   Noto Sans CJK SC → Noto Sans SC → WenQuanYi Zen Hei → Source Han Sans SC
macOS 的 PingFang SC **被故意排除**：它在渲染路径下会把小数点吞掉（2.5 画成 25）。
指定了本机不存在的族名会**直接报错**（渲染层遇到未知族名会静默改用内置西文字体，
中文会变成方框——那种失败到你把图发出去才会发现）。

完整例子（可直接抄）：
  cat > spec.json <<'EOF'
  {{
    \"title\": \"季度销售额\",
    \"mark\": \"bar\",
    \"x\": \"季度\",
    \"y\": \"销售额\",
    \"data\": \"sales.csv\",
    \"types\": {{ \"季度\": \"category\" }},
    \"width\": 1000, \"height\": 600
  }}
  EOF
  abb-chart render --spec spec.json --out sales.png

出图后怎么发给用户（在 ABB 会话里）：
  \"$ABB_BIN\" deliver --to-current --file sales.png --text \"图好了\"
"
    )
}

/// render 子命令用法。
pub fn render_help() -> String {
    "\
abb-chart render —— 渲染一张图

用法：
  abb-chart render --spec <spec.json|-> [--out 路径.png] [--data 路径.csv]
                   [--width N] [--height N] [--theme light|dark] [--font 族名]
                   [--debug-svg 路径]

图规格（JSON）字段：
  mark    必填。point/line/bar/area/hist/density/box/violin/errorbar/rule/text
          （也认 scatter/column/histogram/kde/boxplot/hline 等常见别名）
  title   图标题
  x / y   列名（hist/density 只需 x；rule 只需 y）
  color   分组着色列名
  data    CSV 文件路径（相对规格文件所在目录）；或改用 rows 给内联数据
  rows    内联数据：[{\"列名\": 值}, …]
  types   列类型覆盖：{\"列名\": \"number\"|\"category\"}。名义型数字（年份/月份/编号）
          要用柱状图/箱线图时必须这样声明，否则 x 会是连续轴而这类图要求分类轴。
  width / height  输出像素（同命令行）
  theme   light | dark
  font    字体族（--font 覆盖它）
  x_label / y_label  轴标题（默认用列名）

数据要求：
  - CSV 首行是列名；空字段视为缺失（不会当 0）。
  - 列类型自动推断：非空值全都能解析成数字 ⇒ 数值列，否则分类列；types 可覆盖。
"
    .to_string()
}

/// 解析命令行（不含程序名）。
pub fn parse_args(args: &[String]) -> Result<Invocation, UsageError> {
    if args.is_empty() {
        // 无参 = 让人/模型看到用法，不是错误。
        return Ok(Invocation::Help(top_help()));
    }
    match args[0].as_str() {
        "-h" | "--help" | "help" => return Ok(Invocation::Help(top_help())),
        "render" => {}
        other => {
            return Err(UsageError(format!(
                "不认识的子命令「{other}」。目前只有 `render`（用 `abb-chart --help` 看用法）"
            )))
        }
    }

    let rest = &args[1..];
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Invocation::Help(render_help()));
    }

    let mut spec: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut data: Option<String> = None;
    let mut options = RenderOptions::default();

    let mut i = 0usize;
    // 取下一个值；缺了就是用法错（错在旗标上，报错要带上旗标名）。
    let value = |i: &mut usize, flag: &str| -> Result<String, UsageError> {
        *i += 1;
        rest.get(*i)
            .cloned()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| UsageError(format!("{flag} 缺少取值")))
    };

    while i < rest.len() {
        let arg = rest[i].clone();
        match arg.as_str() {
            "--spec" => spec = Some(PathBuf::from(value(&mut i, "--spec")?)),
            "--out" => out = Some(PathBuf::from(value(&mut i, "--out")?)),
            "--data" => data = Some(value(&mut i, "--data")?),
            "--width" => {
                let raw = value(&mut i, "--width")?;
                options.width =
                    Some(raw.parse::<u32>().map_err(|_| {
                        UsageError(format!("--width 需要正整数像素值，收到「{raw}」"))
                    })?);
            }
            "--height" => {
                let raw = value(&mut i, "--height")?;
                options.height = Some(raw.parse::<u32>().map_err(|_| {
                    UsageError(format!("--height 需要正整数像素值，收到「{raw}」"))
                })?);
            }
            "--theme" => options.theme = Some(value(&mut i, "--theme")?),
            "--font" => options.font = Some(value(&mut i, "--font")?),
            "--debug-svg" => options.debug_svg = Some(PathBuf::from(value(&mut i, "--debug-svg")?)),
            other => {
                return Err(UsageError(format!(
                    "不认识的参数「{other}」（用 `abb-chart render --help` 看可用项）"
                )))
            }
        }
        i += 1;
    }

    // 给了 --data 就要落到规格里：本 CLI 用「规格文本重写」而不是第二套装载路径，
    // 这样 --data 与规格里的 data 走完全相同的代码（不会出现只有一方支持的边角）。
    if let Some(path) = data {
        options.data_override = Some(path);
    }
    if let Some(path) = &spec {
        if path.as_os_str() != "-" && !path.exists() {
            return Err(UsageError(format!(
                "规格文件不存在：{}（要读标准输入请用 `--spec -`）",
                path.display()
            )));
        }
    }
    options.out = out.unwrap_or_default();
    Ok(Invocation::Render { spec, options })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, UsageError> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&owned)
    }

    fn render_invocation(args: &[&str]) -> (Option<PathBuf>, RenderOptions) {
        match parse(args).unwrap() {
            Invocation::Render { spec, options } => (spec, options),
            Invocation::Help(h) => panic!("期望 render，得到帮助：{h}"),
        }
    }

    /// 无参要打印帮助（模型常直接跑一次看用法），而不是报「缺参数」。
    #[test]
    fn no_args_prints_help() {
        assert!(matches!(parse(&[]).unwrap(), Invocation::Help(_)));
    }

    #[test]
    fn help_flags_work_at_both_levels() {
        assert!(matches!(parse(&["--help"]).unwrap(), Invocation::Help(_)));
        assert!(matches!(
            parse(&["render", "-h"]).unwrap(),
            Invocation::Help(_)
        ));
    }

    /// 顶层帮助必须带一个可抄的完整例子（帮助是给模型看的）。
    #[test]
    fn top_help_contains_a_copyable_example() {
        let h = top_help();
        assert!(h.contains("abb-chart render --spec"), "{h}");
        assert!(
            h.contains("deliver --to-current --file"),
            "要写清出图后怎么发：{h}"
        );
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        let err = parse(&["draw"]).unwrap_err();
        assert!(err.0.contains("render"), "{err}");
    }

    #[test]
    fn unknown_flag_is_usage_error() {
        let err = parse(&["render", "--size", "1"]).unwrap_err();
        assert!(err.0.contains("--size"), "{err}");
    }

    #[test]
    fn missing_flag_value_reports_the_flag() {
        let err = parse(&["render", "--spec"]).unwrap_err();
        assert!(err.0.contains("--spec"), "{err}");
    }

    #[test]
    fn non_numeric_dimension_is_usage_error() {
        let err = parse(&["render", "--width", "wide"]).unwrap_err();
        assert!(err.0.contains("--width"), "{err}");
    }

    #[test]
    fn flags_are_parsed_into_options() {
        // 规格文件必须真实存在（`--spec` 会做存在性检查，见 missing_spec_file_is_usage_error）。
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let spec_path = tmp.path().to_string_lossy().to_string();
        let (spec, opts) = render_invocation(&[
            "render",
            "--spec",
            &spec_path,
            "--out",
            "/tmp/a.png",
            "--width",
            "800",
            "--height",
            "400",
            "--theme",
            "dark",
            "--font",
            "Heiti SC",
            "--debug-svg",
            "/tmp/a.svg",
        ]);
        assert_eq!(spec, Some(PathBuf::from(&spec_path)));
        assert_eq!(opts.out, PathBuf::from("/tmp/a.png"));
        assert_eq!(opts.width, Some(800));
        assert_eq!(opts.height, Some(400));
        assert_eq!(opts.theme.as_deref(), Some("dark"));
        assert_eq!(opts.font.as_deref(), Some("Heiti SC"));
        assert_eq!(opts.debug_svg, Some(PathBuf::from("/tmp/a.svg")));
    }

    #[test]
    fn missing_spec_file_is_usage_error() {
        let err = parse(&["render", "--spec", "/nonexistent/spec.json"]).unwrap_err();
        assert!(err.0.contains("不存在"), "{err}");
    }

    /// `--spec -` = stdin，不做存在性检查（它就是要把管道接上）。
    #[test]
    fn dash_spec_means_stdin() {
        let (spec, _) = render_invocation(&["render", "--spec", "-"]);
        assert_eq!(spec, Some(PathBuf::from("-")));
    }

    #[test]
    fn data_override_is_carried() {
        let (_, opts) = render_invocation(&["render", "--spec", "-", "--data", "d.csv"]);
        assert_eq!(opts.data_override.as_deref(), Some("d.csv"));
    }
}
