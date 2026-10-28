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
    /// 区间上界列名（`errorbar` / 蜡烛图类 mark 用它给出范围；缺了会静默只画一个点）。
    pub y2: Option<String>,
    /// 标注文字列名（`text` mark 画的就是这一列的值；缺了会画出空标签）。
    pub text: Option<String>,
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
///
/// 两者的**基准目录不同**（也是有意）：
/// - 命令行 `--data` 是「你现在在哪」，基准 = **CWD**（与 `--out` 同口径）。
///   **前提（不是代码保证）**：shell 的 CWD == 会话工作区 —— 受限会话的 guard 用**工作区**
///   作基准，两者只有在这个前提下才重合。当前该前提由 agent 的 shell 工具不提供 `cwd` 参数
///   （进程 CWD = 会话 cwd = 工作区，见 `crates/abb-agent/src/builtin.rs::dev_tools` 的
///   `OsExecutionEnv::with_cwd(workspace)`）维持，评审核对过。若将来后端允许按命令传 cwd，
///   这里要改成从可信来源取基准（否则 `--data a.csv` 会读到 CWD 的文件而 guard 以为在工作区）。
///   规格内容里的 `data` 不受这个前提影响（它绑规格文件目录，见 [`resolve_in_dir`]）。
/// - 规格内的 `data` 是「这个规格自己的配套数据」，基准 = **规格文件所在目录**。
fn load_table(
    spec: &Spec,
    opts: &RenderOptions,
    spec_dir: Option<&Path>,
) -> Result<Table, RenderError> {
    let cwd = std::env::current_dir().map_err(|e| RenderError(format!("取当前目录失败：{e}")))?;
    if let Some(data) = opts
        .data_override
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return table_from_csv_arg(data, &cwd);
    }
    // `rows` 与 `data` 同时给出 ⇒ **硬错**，不静默挑一个。本仓的既定口径是「歧义输入不静默
    // 忽略」（对照 render.rs 里 hist 的 y 给了别的列名就报错）。这里尤其重要：`data` 有值却
    // 因 `rows` 存在而被跳过时，那个字段**连路径校验都不走**——将来谁调换了两者的优先级，
    // 它就立刻变成一个可用的读入口（评审 O2 指出的耦合）。
    if spec.rows.is_some()
        && spec
            .data
            .as_deref()
            .map(str::trim)
            .is_some_and(|d| !d.is_empty())
    {
        return Err(RenderError(
            "规格里同时给了 `data` 和 `rows`；请二选一（`data` = CSV 文件路径，`rows` = 内联数组）。\n\
             写死一个、忽略另一个会让“我明明写了 data 却没生效”变成静默行为，所以这里直接报错。"
                .into(),
        ));
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
    // stdin 输入时没有规格文件目录 ⇒ 退到 CWD（此时 `--spec -` 本身就意味着「用管道」，
    // 而受限会话到不了这里，见 guard 对 `split_shell` 的拒绝）。
    let base = spec_dir.map(Path::to_path_buf).unwrap_or(cwd);
    table_from_csv_in_dir(data, &base)
}

/// 读规格**内容**里的 `data`：必须与规格文件同目录（见 [`resolve_in_dir`]）。
fn table_from_csv_in_dir(data: &str, base: &Path) -> Result<Table, RenderError> {
    let path = resolve_in_dir(data, base, "data")?;
    read_csv(&path, base, "data")
}

/// 读命令行 `--data`：相对路径按 CWD 解析且不得跑出 CWD；绝对路径放行（闸门看得见）。
fn table_from_csv_arg(data: &str, base: &Path) -> Result<Table, RenderError> {
    let path = resolve_arg(data, base, "--data")?;
    read_csv(&path, base, "--data")
}

/// 实际读取并装载 CSV。
fn read_csv(path: &Path, base: &Path, what: &str) -> Result<Table, RenderError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        RenderError(format!(
            "数据文件读取失败：{}（{e}）。`{what}` 的相对路径以 {} 为基准。",
            path.display(),
            base.display()
        ))
    })?;
    table_from_csv(&text).map_err(RenderError)
}

/// **规格文件内容里的路径**解析：不论相对还是绝对，都必须落在 `base` 内。
///
/// ## 为什么这里连绝对路径都不许（安全洞，实测两次，别在没读这段的情况下放宽）
///
/// 受限（授权者）会话的守卫只能看到**命令行**：它校验 `--spec` 这个**文件本身**在工作区内，
/// 但看不到规格文件**内容**里写的 `data`。所以：
/// - 第一版实现允许 `..` ⇒ 工作区内 `spec.json` 写 `"data": "../secret.csv"` 就能读工作区外
///   文件并把内容画进图里（实测图里张榜着外部文件内容）；
/// - 只堵 `..` 还不够：写 `"data": "/abs/outside.csv"`（**绝对路径**）同样读到工作区外，
///   而“绝对路径交给 guard 把关”这句话**不成立**——guard 根本看不到这个字段（独立评审实测）。
///
/// 结论：**内容里的路径是闸门的盲区**，只能由解析它的这一方自己收口。收口方式不是去猜
/// 会话工作区在哪（abb-chart 拿不到可信的工作区：`--confine` 由转发层加也挡不住 agent 自己
/// `cd` 到别处、或伪造环境变量），而是用一个**本地可证**的规则：
///
/// > **规格文件只能读它自己所在目录里的数据。**
///
/// 这样「规格在工作区内 ⇒ 它读到的数据也在工作区内」是纯局部推理，不依赖任何环境变量、
/// 不依赖 CWD、也不依赖调用者是 owner 还是受限会话。
///
/// **实现约束**：`starts_with` 的两侧**都必须是 `canonicalize` 的产物**（`/etc` → `/private/etc`
/// 这类挂载映射、软链、`..` 都在这一步归一到真实路径）。谁把某一边换成未 canonicalize 的路径，
/// 这个前缀判定立刻失效。
///
/// **已知平台限制（Windows，未实测）**：`std::fs::canonicalize` 在 Windows 上对**长路径**返回
/// verbatim 形态（`\\?\C:\…`）。本函数的前提是 `base` 由调用方（主程序）**规范化过一次**再传进来
/// （`--spec` 的目录同理），此时两侧形态一致；若出现「同一目录、一侧带前缀一侧不带」，比较会
/// **不再相等（拒绝，方向是安全的 fail-closed）**，代价是可能误拒一个合法路径。
/// 这与本仓既有做法一致（`src/task_store.rs` 挡 symlink 逃逸用的也是朴素 `canonicalize` +
/// `starts_with`，全仓没有引入 `dunce` 之类的路径规范化依赖），且新增的 `abb-chart-windows`
/// CI job 就是暴露这条差异的机器判据（本机 macOS 无法复现 verbatim 形态）。
/// 需要用别处的数据时走**命令行 `--data`**（它在命令行上、闸门看得见；受限会话里工作区外的
/// 绝对路径会被 guard 拒绝，owner 不受限）——即「能力没被拿掉，只是换了个闸门看得见的位置」。
pub fn resolve_in_dir(raw: &str, base: &Path, what: &str) -> Result<PathBuf, RenderError> {
    let p = Path::new(raw);
    let base_real = std::fs::canonicalize(base)
        .map_err(|e| RenderError(format!("基准目录不可用（{}）：{e}", base.display())))?;
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_real.join(p)
    };
    match std::fs::canonicalize(&joined) {
        Ok(real) if real.starts_with(&base_real) => Ok(real),
        Ok(real) => Err(RenderError(format!(
            "`{what}` 指向规格文件所在目录之外（已拒绝）：{raw} ⇒ {}（基准：{}）。\n\
             规格文件里的 `data` 只能读**同一目录**下的文件——因为这个字段在输出给受限会话的\
             闸门里是看不见的，只能由解析它的这一方收口。要用别处的数据，请：\n\
             \x20 * 把数据放到规格文件同目录（推荐），或把 `data` 写成同目录的相对路径；\n\
             \x20 * 或用命令行 `--data <路径>`（绝对路径也可，它走闸门可见的通道）。",
            real.display(),
            base_real.display()
        ))),
        Err(e) => Err(RenderError(format!(
            "`{what}` 解析失败：{}（{e}）。相对路径以规格文件所在目录为基准，且必须在其中。",
            joined.display()
        ))),
    }
}

/// **命令行参数里的路径**解析：相对路径必须在 `base` 内；**绝对路径原样放行**。
///
/// 与 [`resolve_in_dir`] 的差别就是绝对路径这一条，原因是**闸门看得见命令行**：
/// 受限会话里 guard 已经会拒绝工作区外的绝对路径（含符号链接，它用 canonicalize），
/// 所以这里放行不会开洞；owner 会话则本就有全权限（`--data /data/sales.csv` 这类需求保留）。
/// 内容里的路径没有这个前提，故那个函数不放行绝对路径。
pub fn resolve_arg(raw: &str, base: &Path, what: &str) -> Result<PathBuf, RenderError> {
    let p = Path::new(raw);
    if p.is_absolute() {
        // 放行绝对路径，但**照样展开软链**：读的是 canonical 目标、报错里也是真实路径。
        // 为什么（评审 O1）：不展开的话 `--data <工作区内某个指向外部的软链>` 会被 abb-chart
        // 直接读出去——受限会话有 guard 兜（它会 canonicalize argv），但 abb-chart 自己就
        // 一点防御都没有，任何不带 guard 的调用者（owner 直跑、将来新调用方）都会跟着走。
        // 展开不改变「绝对路径放行」的语义，只是不再把软链当不透明字符串。
        // 目标不存在时 canonicalize 会失败 ⇒ 原样返回，让读取阶段去报「文件不存在」。
        return Ok(std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
    }
    let base_real = std::fs::canonicalize(base)
        .map_err(|e| RenderError(format!("基准目录不可用（{}）：{e}", base.display())))?;
    let joined = base_real.join(p);
    match std::fs::canonicalize(&joined) {
        Ok(real) if real.starts_with(&base_real) => Ok(real),
        Ok(real) => Err(RenderError(format!(
            "`{what}` 指向基准目录之外（已拒绝）：{raw} ⇒ {}（基准：{}）。\
             相对路径不得跑出基准目录；确实要读别处的数据，请写**绝对路径**\
             （受限会话只允许工作区内的绝对路径）。",
            real.display(),
            base_real.display()
        ))),
        Err(e) => Err(RenderError(format!(
            "`{what}` 解析失败：{}（{e}）。相对路径以 {} 为基准，且不得跑出该目录。",
            joined.display(),
            base_real.display()
        ))),
    }
}

/// **渲染器一定会画**的字符（数值刻度、以及被当作分类渲染的数值单元格）。
///
/// 为什么必须单独列出来（实测过的判据缺口）：刻度文字**不来自规格里的可见文字**，而是由
/// 渲染器/`fmt_number` 现算的。只收集「标题/轴标题/列名/文本单元格」会漏掉它们——
/// 实测一张标题为 `ABC`、列为 `cat`/`delta` 的柱状图，图上实际画了 `1` `2` `3` `-` `0` `5`；
/// 一张含 1e20 量级的散点图，轴上出现了 `1.0E20` 这样的文字（`E` 也是现算的）。
/// 漏收集的后果与前几轮的方框坑同类：字体缺某个数字字形时会静默变方框。
///
/// 内容取「默认路径**能**产出的字符」：
/// - 数字与小数点、负号：`2024.0`、`-10`；
/// - 科学计数法的指数符号：`1.0E20`（实测）。
///
/// **故意不含** `µ`/`k`/`M`/`B`/`T`：那些只在 charton 的 `Abbreviation::Compact` 下才出现，
/// 而本 CLI 没有暴露任何数值格式选项（`Spec` 里没有 formatter 字段），默认路径产不出它们。
/// 把它们加进来只会让 `Hiragino Sans GB`（实测缺 `µ`）在**每张图**上都被判不合格——那是
/// 过度收紧，不是更安全。将来若给 CLI 加上格式选项，必须同步把对应字符加进这份表。
pub const RENDERER_TEXT_BUDGET: &[char] = &[
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', '-', '+', 'e', 'E',
];

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
    // 先放渲染器必画的字符（数值刻度现算，不来自规格文字），再收规格里的可见文字。
    let mut out: Vec<char> = RENDERER_TEXT_BUDGET.to_vec();
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
    // `text` mark 画的是该列**值**（已在下面按单元格收集），
    // `y2`/`text` 的**列名**本身也会作为轴/图例文字出现吗？不会——但列名可能是默认轴标题，
    // 保险起见一并收集（多收集只会让判据更严，不会漏方框）。
    for name in [spec.y2.as_deref(), spec.text.as_deref()]
        .into_iter()
        .flatten()
    {
        push(name);
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

    /// 测试落地目录：取平台临时目录 + pid，**不得写死本机绝对路径**。
    ///
    /// 曾经写死 `/Volumes/DataExt/tmp/abbchart-test`（本机数据卷），在 GitHub runner 上
    /// 该前缀不存在且不可创建 ⇒ `create_dir_all` 报 `PermissionDenied`，CI 的
    /// `abb-chart-lint` 因此长期假红（而本机永远绿）。同文件其他用例早已统一用
    /// `temp_dir() + pid`，这里与之一致。
    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!("abbchart-test-{}", std::process::id()))
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

    /// 相对路径以基准目录解析（正常路径正常放行）。
    #[test]
    fn relative_path_is_resolved_against_the_base() {
        let dir = std::env::temp_dir().join(format!("abb-chart-base-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("d.csv"), "a,b\n1,2\n").unwrap();
        let got = resolve_in_dir("d.csv", &dir, "data").unwrap();
        assert_eq!(got, std::fs::canonicalize(dir.join("d.csv")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **安全回归钉子（阻塞级）**：规格内容里的相对路径跑出规格目录必须被拒。
    ///
    /// 实测过的洞：受限（授权者）会话的闸门只看命令行、看不到规格文件**内容**，
    /// 于是工作区内的 `spec.json` 写 `"data": "../secret.csv"` 就能读到工作区外的文件
    /// 并把内容画进图里。判别力：把 `resolve_in_dir` 里的 `starts_with` 校验删掉 ⇒ 本测试红。
    #[test]
    fn spec_content_relative_path_escaping_is_rejected() {
        let root = std::env::temp_dir().join(format!("abb-chart-esc-{}", std::process::id()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(root.join("secret.csv"), "a\n1\n").unwrap();

        let err = resolve_in_dir("../secret.csv", &ws, "data").unwrap_err();
        assert!(err.0.contains("规格文件所在目录之外"), "{err}");
        assert!(err.0.contains("../secret.csv"), "要说清是哪个值：{err}");
        // 报错要给出路：告诉对方用 --data（闸门可见的通道）。
        assert!(err.0.contains("--data"), "要给出可行替代：{err}");
    }

    /// **安全回归钉子（第二个阻塞级洞）**：规格内容里的**绝对**路径同样必须被拒。
    ///
    /// 只堵 `..` 是不够的（独立评审实测）：`"data": "/abs/outside.csv"` 一样能读到工作区外，
    /// 而「绝对路径交给 guard 把关」不成立 —— guard 看不到规格内容这个字段。
    /// 判别力：让 `resolve_in_dir` 对绝对路径提前 return ⇒ 本测试红。
    #[test]
    fn spec_content_absolute_path_is_rejected_too() {
        let root = std::env::temp_dir().join(format!("abb-chart-abs-{}", std::process::id()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(root.join("secret.csv"), "a\n1\n").unwrap();

        let outside = root.join("secret.csv");
        let err = resolve_in_dir(outside.to_str().unwrap(), &ws, "data").unwrap_err();
        assert!(err.0.contains("规格文件所在目录之外"), "{err}");

        // 同目录内的绝对路径则放行（它没跑出去）。
        std::fs::write(ws.join("ok.csv"), "a\n1\n").unwrap();
        let inside = ws.join("ok.csv");
        assert!(resolve_in_dir(inside.to_str().unwrap(), &ws, "data").is_ok());
    }

    /// 命令行 `--data` 的语义：相对路径限 CWD，**绝对路径放行**（闸门看得见命令行，
    /// 受限会话的 guard 会拒绝工作区外的绝对路径；owner 保留全权限）。
    #[test]
    fn cli_data_allows_absolute_but_confines_relative() {
        let root = std::env::temp_dir().join(format!("abb-chart-arg-{}", std::process::id()));
        let cwd = root.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join("outside.csv"), "a\n1\n").unwrap();

        // 绝对路径放行
        let abs = root.join("outside.csv");
        assert!(resolve_arg(abs.to_str().unwrap(), &cwd, "--data").is_ok());
        // 相对路径跑出去 ⇒ 拒
        let err = resolve_arg("../outside.csv", &cwd, "--data").unwrap_err();
        assert!(err.0.contains("基准目录之外"), "{err}");
    }

    /// **回归钉子（评审 O1）**：`--data` 的绝对路径也要展开软链。
    ///
    /// 不展开的话 `--data <工作区内指向外部的软链>` 会被直接读出去——受限会话有 guard 兜，
    /// 但 abb-chart 自己毫无防御。判别力：把 `resolve_arg` 的绝对分支改回 `return Ok(p.to_path_buf())`
    /// ⇒ 本测试红。
    #[cfg(unix)]
    #[test]
    fn cli_absolute_data_expands_symlinks() {
        let root = std::env::temp_dir().join(format!("abb-chart-argsym-{}", std::process::id()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(root.join("secret.csv"), "a\n1\n").unwrap();
        std::os::unix::fs::symlink(root.join("secret.csv"), ws.join("link.csv")).unwrap();

        let got = resolve_arg(ws.join("link.csv").to_str().unwrap(), &ws, "--data").unwrap();
        let want = std::fs::canonicalize(root.join("secret.csv")).unwrap();
        assert_eq!(got, want, "绝对路径也要展开成真实路径（否则跟着软链走）");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **回归钉子（评审 O2）**：`data` 与 `rows` 同时给出必须硬错，不静默挑一个。
    ///
    /// 尤其重要：`rows` 存在时 `data` 会被**跳过**（连路径校验都不走），
    /// 将来谁调换两者优先级，它就立刻变成可用的读入口。判别力：删掉那条冲突检查 ⇒ 本测试红。
    #[test]
    fn data_and_rows_together_is_a_hard_error() {
        let dir = std::env::temp_dir().join(format!("abb-chart-both-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("d.csv"), "a\n1\n").unwrap();

        let spec_text = r#"{"mark":"bar","x":"a","y":"a","data":"d.csv","rows":[{"a":1}]}"#;
        let err = render_to_png(
            spec_text,
            Some(&dir),
            &RenderOptions {
                out: dir.join("o.png"),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.0.contains("二选一"), "{err}");

        // 只给 rows 则正常（用 point：bar 要求分类 x，与本测试无关，别把两条规则混在一起）。
        let only_rows = r#"{"mark":"point","x":"a","y":"b","rows":[{"a":1,"b":2},{"a":2,"b":3}]}"#;
        assert!(render_to_png(only_rows, Some(&dir), &RenderOptions::default()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用**符号链接**逃逸同样要被拦住（字符串前缀比较拦不住这种，故实现用 canonicalize）。
    #[cfg(unix)]
    #[test]
    fn symlink_escaping_the_base_is_rejected() {
        let root = std::env::temp_dir().join(format!("abb-chart-link-{}", std::process::id()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(root.join("secret.csv"), "a\n1\n").unwrap();
        std::os::unix::fs::symlink(root.join("secret.csv"), ws.join("link.csv")).unwrap();
        std::os::unix::fs::symlink(root.clone(), ws.join("linkdir")).unwrap();

        assert!(resolve_in_dir("link.csv", &ws, "data").is_err(), "软链文件");
        assert!(
            resolve_in_dir("linkdir/secret.csv", &ws, "data").is_err(),
            "软链目录"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `a/../b` 这种**没跑出去**的相对路径要正常放行（别把合法写法一起拒了）。
    #[test]
    fn relative_path_with_inner_dotdot_is_allowed_when_it_stays_inside() {
        let dir = std::env::temp_dir().join(format!("abb-chart-inner-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("d.csv"), "a,b\n1,2\n").unwrap();
        let got = resolve_in_dir("sub/../d.csv", &dir, "data").unwrap();
        assert!(got.ends_with("d.csv"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在时给的是「读取/解析失败」，不是「跑出基准目录」——别把排查方向指错。
    #[test]
    fn missing_file_reports_resolution_failure_not_escape() {
        let dir = std::env::temp_dir().join(format!("abb-chart-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = resolve_in_dir("nope.csv", &dir, "data").unwrap_err();
        assert!(err.0.contains("解析失败"), "{err}");
        assert!(!err.0.contains("之外"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// **回归钉子（本轮自查发现的判据缺口）**：数值刻度的字符是渲染器现算的，
    /// 规格文字里可能一个数字都没有，但它们照样会被画上去，必须参与字体校验。
    ///
    /// 实测反例（缺口存在时）：标题 `ABC` + 列 `cat`/`delta` 的柱状图，图上画了
    /// `1` `2` `3` `-` `0` `5`，而旧实现收集到的集合里**一个数字都没有**。
    /// 判别力：把 `RENDERER_TEXT_BUDGET` 从收集里去掉 ⇒ 本测试红。
    #[test]
    fn renderer_generated_numeric_labels_are_always_required() {
        let spec = Spec {
            mark: "bar".into(),
            title: Some("ABC".into()),
            ..Default::default()
        };
        let table = table_from_json_rows(&serde_json::json!([{"cat":1,"delta":-3.2}])).unwrap();
        let chars = spec_required_chars(&spec, &table);
        for c in ['0', '3', '9', '-', '.'] {
            assert!(
                chars.contains(&c),
                "数字/符号 {c} 必须在校验集合里：{:?}",
                chars
            );
        }
    }

    /// 这份底线**不得**包含只在 `Abbreviation::Compact` 下才出现的字符（`µ`/`k`/`M`）：
    /// 本 CLI 没有数值格式选项，包含它们会让「实测缺 `µ` 的 Hiragino」在每张图上都被误判。
    #[test]
    fn renderer_budget_excludes_chars_we_cannot_emit() {
        for c in ['µ', 'k', 'M', 'B', 'T'] {
            assert!(
                !RENDERER_TEXT_BUDGET.contains(&c),
                "{c} 不在默认路径的产出里，加进来会造成误判"
            );
        }
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
        let chars = spec_required_chars(&spec, &table);
        // 规格文字里的字各出现一次（底线字符在集合开头，这里只看规格那部分）。
        let from_spec: Vec<char> = chars
            .iter()
            .copied()
            .filter(|c| !RENDERER_TEXT_BUDGET.contains(c))
            .collect();
        assert_eq!(from_spec, vec!['中', 'v']);
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

        // 判据用**相对差**：`2.5` 必须比 `25` 多一个墨迹组；绝对值（每组恰好 2 个字宽）
        // 取决于字体/字号/kerning 的光栅化结果，不是不变量——windows-latest 上 `25` 就连成
        // 1 组（2026-10-28 实测：25=1、2.5=2、1285=2、128.5=3），macOS 默认字体下是 2/3/4/5。
        // 判别力：把小数点吞掉（`render_title("2.5")` 换成 `"25"`）⇒ 两组数相等 ⇒ 必红。
        let g25 = ink_groups(&render_title("25"));
        let g2_5 = ink_groups(&render_title("2.5"));
        assert_eq!(
            g2_5,
            g25 + 1,
            "小数点必须多出一个墨迹组（得 {g2_5} vs {g25}）：小数点被吞了？"
        );
        let g1285 = ink_groups(&render_title("1285"));
        let g128_5 = ink_groups(&render_title("128.5"));
        assert_eq!(
            g128_5,
            g1285 + 1,
            "小数点必须多出一个墨迹组（得 {g128_5} vs {g1285}）：小数点被吞了？"
        );

        // macOS 设计基线（按 macOS 默认字体 + 该平台光栅化量出来的绝对值）。
        // Windows 字体下绝对组数不成立，故只在 macOS 保留；覆盖不丢——上面两条相对判据
        // 在两个平台都跑。
        #[cfg(target_os = "macos")]
        {
            assert_eq!(g25, 2, "`25` 应是 2 个字形（macOS 设计基线）");
            assert_eq!(g2_5, 3, "`2.5` 应是 3 个字形（macOS 设计基线）");
            assert_eq!(g128_5, 5, "`128.5` 应是 5 个字形（macOS 设计基线）");
        }
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
