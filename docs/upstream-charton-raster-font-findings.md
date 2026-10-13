# 上游 charton raster 后端的字形缺陷（复现清单）

面向 `charton`（<https://github.com/wangjiawen2013/charton>）的缺陷报告汇总。本仓 `crates/abb-chart`
在接入过程中实测到 5 个问题，全部集中在 **CPU 光栅（raster / `png` feature）后端的字体路径**。
主机：macOS 15 / arm64；`charton 0.8.4`；features `["png"]`（即 `tiny-skia` + `ab_glyph` + `fontdb`）。

逐条给出最小复现、观察、期望与代码指针。**F1–F4 都会让用户看到错误产物**（豆腐块 / 空方框 / 数字出错），
且都是**静默**的——渲染不报错、退出码为 0。

> 行号取自本地 clone 的 `main`（与 `0.8.4` 高度接近）。若与你的版本对不上，按**函数名**检索。

## 环境准备（复现用）

```toml
# Cargo.toml
[dependencies]
charton = { version = "0.8.4", default-features = false, features = ["png"] }
```

```rust
// 通用渲染骨架：title 用待测文字，只改这一处即可复现全部 4 条
use charton::prelude::*;

fn render(title: &str, family: Option<&str>) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let ds = Dataset::new()
        .with_column("x", vec![1.0, 2.0, 3.0])?
        .with_column("y", vec![2.0, 3.0, 5.0])?;
    let lc: LayeredChart = Chart::build(ds)?
        .mark_point()?
        .encode((alt::x("x"), alt::y("y")))?
        .with_title(title)
        .with_size(400, 200)   // 注意：实际产出 800×400，见 F5
        .into();
    let lc = match family {
        Some(f) => lc.configure_theme(|t| {
            t.with_title_family(f)
                .with_label_family(f)
                .with_tick_label_family(f)
                .with_legend_label_family(f)
        }),
        None => lc,
    };
    Ok(lc.to_png()?)
}
```

---

## F1（严重）默认主题的字体族是 CSS 栈字符串，raster 后端把它当**单个族名**查询 ⇒ 中文必出豆腐块

**复现**

```rust
render("销售额趋势", None)?.save_png("f1.png")   // 默认主题，不指定 family
```

**观察**

标题渲染为一排竖长条 / 方框（`assets` 里内置的 `Inter` 只有拉丁字形）。同一次渲染里的
ASCII 文字（轴刻度数字 `10` `20`）完全正常，所以很容易被当成「字体没装」。

**根因**

`Theme::default()` 的四个 family 字段是一个 **CSS 字体栈字符串**（`src/theme.rs`，`font_stack` 变量）：

```
"Inter, -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial,
 'PingFang SC', 'Microsoft YaHei', Ubuntu, Cantarell, 'Noto Sans', sans-serif"
```

而 raster 后端取字体的入口把**整串**当**一个族名**交给 `fontdb`：

```rust
// src/core/utils.rs —— get_raster_font(family: &str)
let families = [fontdb::Family::Name(family)];   // ← family 是整个栈字符串
let query = fontdb::Query { families: &families, /* … */ };
if let Some(id) = sys_db.query(&query) { /* 命中才用 */ }
// 查不到 ⇒ 落到内置 Inter
```

栈字符串永远匹配不上任何真实家族名 ⇒ 一定走 fallback。调用点在 raster 后端是**全图唯一**的字体来源：

```rust
// src/render/backend/raster.rs
let font = crate::core::utils::get_raster_font(&config.font_family);
```

**期望**

与同库的 PDF 路径保持一致：**按逗号拆栈、逐个尝试**，全部失败才用内置字体。
PDF 路径就是这么写的（`src/core/utils.rs::resolve_pdf_font`，注释即 *names are tried left to right*）：

```rust
for name in family.split(',') {
    let name = name.trim().trim_matches(['\'', '"']).trim();
    /* 映射 generic 族 → fontdb::Family → query → 命中即返回 */
}
```

**影响（在我们的产品里）**

中文标题/轴标签/图例**默认全废**，而调用方（LLM agent）看到的是「渲染成功」。我们目前只能把四个
family 字段各自设成一个**单体族名**来绕开，即在应用层重写了本该由库做的栈解析。

## F2（严重）不存在的族名静默回落内置字体，不报错

**复现**

```rust
let a = render("Hello", Some("NoSuchFontXYZ123"))?;  // 不存在的族名
let b = render("Hello", None)?;                      // 默认主题
assert_eq!(sha256(&a), sha256(&b));                  // ← 实测成立
```

**观察**

两者输出 **逐字节相同**（sha256 一致）。也就是「用户显式换了字体」这个动作被完全吞掉。

**根因**

`get_raster_font` 的慢路径查不到时直接落到内置 `Inter`（`src/core/utils.rs`）：

```rust
// Ultimate fallback
if let Some(font) = map.get("sans-serif") { return font.clone(); }
panic!("No fonts available. …")   // 连 panic 都只在「内置字体也加载失败」时才触发
```

**期望**

两种选择都比现在好，建议至少给其一：
1. 返回 `Err`（或 `Option`）让调用方决定；或
2. 提供「严格模式」开关 / `Theme` 上的 `font_family_strict`，未知族名即失败。

**影响**：用户以为换了字体，实际没换；在中文场景下等于又回到 F1（豆腐块），且没有任何信号。

## F3（严重）没有逐字符回退：字体缺某个字形时静默画**空方框**

raster 路径对整张图只加载**一份** `FontArc`，所以字符在这份字体里没有字形时，没有任何补齐机制。
抽象成一句：**只要「命中了一个字体」，就不再关心这个字符它画不画得出来。**

**F3a：命中了字体但无 CJK 字形**

```rust
render("销售额", Some("Helvetica"))   // 本机有 Helvetica，无 CJK 字形
→ 一排空心宽框
```

**F3b：命中了中文字体，但它缺个别符号**（更隐蔽）

```rust
render("销售额（m²）", Some("Hiragino Sans GB"))   // macOS 自带中文主力字体
→ 「²」是空方框，同一行其余中文完全正常
```

实测该字体在本机的字形缺失集合：**`²` `³` `µ`**（`Heiti SC` / `Songti SC` / `Arial Unicode MS` /
`PingFang SC` 都不缺）。这类字符在两个字符宽度以内就能让一个再普通不过的轴标题（`m²`、`µm`、
`℃`、`Ω`）出现方框，而**图里其余内容全都正常**——所以「字体存在」和「字体能画中文」这两类判断
都抓不到它。

**期望**

任一即可：
1. 逐字符 fallback 链（用 fontdb 的 `query` 结果做 per-glyph 回退；或在 `fontdb::Database` 上
   按缺字能力挑选 face）；
2. 或者在 API 上暴露「这份字体覆盖哪些字符」的查询，让调用方能自己校验后拒绝出图。

**影响**：用户拿到一张「大部分正常、某处一个方框」的图，通常要到聊天里被人指出来才发现。

## F4（严重）`PingFang SC` 在 raster 路径下**吞掉小数点**

**复现**

```rust
render("2.5", Some("PingFang SC"))   // 标题
```

**观察**

渲染成 `25`（小数点消失）。同一张图里其余刻度 `1.0`/`1.5`/`3.0`/`3.5`/`4.0` 与 y 轴整数刻度
**全部正常**；受影响的只有含 `2` 的那两个标签。排查结论：

- **不是标签生成问题**：同一个图的 SVG 后端 `to_svg()` 输出的 `<text>` 内容是**正确的 `2.5`**；
- **是 raster 字形层问题**：默认字体（内置 `Inter`）下 `2.0`/`2.5` 完全正确；
- 单字形渲染（`"."`、`"2.5"`、`"25"`）都画得出来 ⇒ 不是「点画不出」，是定位/字距把它挤掉了；
- 像素列分组（x 轴刻度带）：该字体下 `2.0`/`2.5` 这一组只有 **2 段墨迹**（`2` 与 `5`），
  而 `Inter` 下同位置是 **3 段**。
- 同机对照：`Hiragino Sans GB`、`Heiti SC` 下 `2.5` 正常显示。

**期望**：raster 与 SVG 后端对同一字符串的渲染结果应一致（至少不该丢字符）。

**影响**：小数刻度静默变成错误的整数（`2.5`→`25`、`2.0`→`20`），数值被读成另一个数——
这是四个问题里**唯一会产出"数值上错误"的图**的，比豆腐块更危险。

## F5（轻微 / 文档）`with_size(w, h)` 的实际像素是 `w × scale_factor`

**复现**

```rust
let lc = /* … */ .with_size(600, 400);
lc.to_png()?;      // IHDR 实测：1200 × 800（scale_factor 默认 2.0）
```

**观察**：`LayeredChart::scale_factor` 默认 `2.0`（`src/core/composite.rs`，High-DPI 目的），
`to_png` 走 `(width * scale_factor) as u32`。

**期望**：不算 bug，但当前「`with_size` = 输出像素」的直觉是错的，容易让调用方按参数估算产物大小
（而下游常有尺寸上限）。建议在 `with_size` / `to_png` 的文档里写明「逻辑尺寸 × `scale_factor`
= 实际像素」，或提供 `with_pixel_size(w, h)` 这样的显式 API。

---

## 附：修复现状（我们的规避，供上游参考）

| 缺陷 | 我们的规避（在 `crates/abb-chart` 里） |
|---|---|
| F1 | 把 `title/label/tick_label/legend_label` **四个 family 字段**各设成**单个真实族名**（少设一个该通道仍出豆腐） |
| F2 | 自己用 `fontdb` 探测族名是否存在，未知即退出码 1 |
| F3 | **逐字符校验**：收集图上会出现的全部字符（标题/轴标题/列名/分类值），用 `ttf-parser` 的 `glyph_index` 查覆盖；缺任意一个即退出码 1，并报出缺哪些字符（带码点） |
| F4 | macOS 候选序**排除** `PingFang SC`，并有像素级回归测试（默认字体下 `2.5` 必须留下 3 组墨迹） |
| F5 | 固定 `scale_factor(1.0)`，让 CLI 的 `--width/--height` 就是最终像素 |

这些都是**应用层绕过**，成本不低（F1 需要在四个字段上重复设置、F3 需要自己维护一份字符收集逻辑）。
如果上游在 raster 后端补上「栈解析 + 逐字回退 + 严格模式」，这几层都能删掉。
