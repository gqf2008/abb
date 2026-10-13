# abb-chart

ABB 的**图形生成 CLI**：把一份 JSON 图规格 + 数据渲染成 **PNG**，供 agent 在聊天会话里出图后
用 `deliver` 发回给用户。

> 在 ABB 里你不需要直接调它：agent 用的是 `"$ABB_BIN" chart render …`，由桥的子命令转发到这里
> （见 `src/main.rs::run_chart_cli`）。本文件既是包的说明，也是转发链路的落地说明。

## 它解决什么问题

用户在聊天里说「把今年的销售额画个图」时，agent 需要一个**能出图的命令**。ABB 里这个位置有三条
候选路，只有一条对：

| 候选 | 结论 |
|---|---|
| 做成 `abb-agent` 的**内置工具** | ✗ charton 0.8 是 edition 2024（需要 rustc ≥1.85），而 `abb-agent` 锁 `rust-version = "1.78"`；渲染 panic 会砸掉会话进程；更要命的是会逼 `dev_tools()` 为 read-only 档开特例，违背「agent 不碰权限」的设计 |
| 发布成随包**第三方二进制**（进 `tools/bin`） | ✗ `tools/tool-lock.tsv` 要求外部可取件的 url+sha256（自产 CLI 会鸡生蛋）；`src/deps.rs` 的单测断言每个名字的 provider 必须在 lock 的包里 |
| **独立包 + `$ABB_BIN chart` 转发**（本方案） | ✓ 与 `crates/abb-agent` 同构（自带 manifest + lock，root 的 fmt/clippy/test 不碰它）；授权面只需在 `guard.rs` 的白名单加一条；渲染崩溃不影响托盘进程 |

## 用法

```sh
abb-chart render --spec spec.json [--out chart.png] [--data d.csv]
                 [--width 1000] [--height 600] [--theme light|dark]
                 [--font "Hiragino Sans GB"] [--debug-svg debug.svg]
```

`abb-chart --help` 有可直接抄的完整例子；`abb-chart render --help` 列全部规格字段。

规格（JSON）：

```json
{
  "title": "季度销售额趋势",
  "mark": "bar",
  "x": "季度",
  "y": "销售额",
  "data": "sales.csv",
  "types": { "季度": "category" },
  "x_label": "季度",
  "y_label": "销售额（万元）",
  "width": 900,
  "height": 520
}
```

- `mark`：`point` / `line` / `bar` / `area` / `hist` / `density` / `box` / `violin` /
  `errorbar` / `rule` / `text`（也认 `scatter` / `column` / `histogram` / `kde` / `boxplot` /
  `hline` 等别名）。
- 数据来源三选一：规格里的 `data`（CSV）、规格里的 `rows`（内联 JSON 数组）、命令行的 `--data`。
- **路径安全边界（改动前必读，这里踩过两个阻塞级洞）**：
  | 入口 | 规则 |
  |---|---|
  | 规格内容里的 `data` | **只能读规格文件所在目录内的文件**（相对、绝对都要落在该目录里；含符号链接） |
  | 命令行 `--data` | 相对路径按 **CWD** 解析且不得跑出；**绝对路径可用**（闸门看得见命令行） |
  | `--spec` / `--out` / `--debug-svg` | 闸门（`guard`）直接校验命令行参数 |
  
  为什么规格内容连绝对路径都不许：受限（授权者）会话的闸门**只能看到命令行**，看不到规格文件
  **内容**，所以「规格里的路径」是闸门的盲区，只能由解析它的这一方收口。两个实测过的洞：
  ① `"data": "../secret.csv"`；② 只堵 `..` 之后仍成立的 `"data": "/abs/secret.csv"`
  （「绝对路径交给 guard」这句话不成立——guard 看不到该字段）。规则因此取**局部可证**的形式：
  「规格只能读它自己所在目录里的数据」，于是「规格在工作区内 ⇒ 它读的数据也在工作区内」不依赖
  任何环境变量或 CWD。需要读别处数据时用 `--data`（换到闸门看得见的通道）。
  实现见 `spec.rs::resolve_in_dir` / `resolve_arg`，e2e 钉子
  `spec_data_cannot_escape_its_directory`、`spec_data_absolute_path_outside_its_directory_is_rejected`。
- 列类型自动推断（非空值全是数字 ⇒ 数值列）；`types` 可显式覆盖。
- 入口文件名 `abb-chart` 保持中性（不叫 `chart`：PATH 上太泛），由桥转发时补上语义。

### 通道要求（实测表，不是照直觉写的）

| mark | 需要的通道 |
|---|---|
| `hist` | `x` = 要统计的列；`y` 由渲染器生成（固定叫 `count`，**数据里没有这一列**） |
| `density` | 只要 `x`（密度生成到 y） |
| `violin` | 只要 `y`（x 由 recipe 生成） |
| `box` / `rule` / `errorbar` | `x` 与 `y` 都要 |
| 其余（`point`/`line`/`bar`/`area`/`text`） | `x` 与 `y` 都要 |

`box`/`violin`/`bar` 的 `x` 是**分组轴**，必须是分类列：名义型数字（年份/月份/编号）要用
`"types": {"年份": "category"}` 声明，否则会得到一条明确的、可照做的报错。

## 上游（charton 0.8.4）已知坑与本包的规避

这几条都是**实测**出来的，写在这里是因为它们决定了本包的默认设置，改动前请先读：

1. **默认主题下中文必出豆腐块。** 主题的 family 是 CSS 栈字符串（`"Inter, …, 'PingFang SC', …"`），
   而 raster 后端的 `get_raster_font` 把**整串**当**一个族名**查 ⇒ 永远匹配不上 ⇒ 回落内置
   Inter（纯拉丁）。上游 PDF 路径已按逗号拆栈（`resolve_pdf_font`），raster 路径没有。
   → 本包把主题的 `title_family`/`label_family`/`tick_label_family`/`legend_label_family`
   **四个字段**都设成单个真实族名（PNG 后端全图只取一份字体，漏一个那条通道就还是豆腐）。
2. **不存在的族名静默回落 Inter。** 实测 `--font NoSuchFontXYZ123` 的输出与默认主题**逐字节
   相同**。→ 本包先用 fontdb 探测，缺了就以退出码 1 显式失败。
3. **存在但没有中文字形的族名，同样是静默豆腐。** 实测 `--font Helvetica`（本机有、无 CJK 字形）
   ⇒ 一排空心宽框。→ 判据不是「字体存在」，而是**用 ttf-parser 查这份字体画不画得出那个字**。
   进一步：**覆盖中文的字体也可能缺个别符号**。实测 `Hiragino Sans GB`（macOS 自带中文主力字体）
   缺 `²` `³` `µ`——`销售额（m²）` 这种普通轴标题就会出一个方框，而同一张图其余文字完全正常，
   所以「有没有中文字体」这类粗判据抓不到。→ 最终判据是**逐字符校验**：收集图上会出现的
   全部字符（标题/轴标题/列名/分类值），选中的字体必须**每一个都画得出**；缺了就以退出码 1
   报出「缺哪些字（带码点）」，并给出本机可用的替代字体。macOS 候选序因此把 `Heiti SC`
   （实测全覆盖）排在 `Hiragino Sans GB` 之前——但候选序只是优先级，真正的判据是逐字符覆盖，
   所以换台机器、字体不同，也不会因为「顺序猜错」而静默出方框。
4. **`PingFang SC` 会吞小数点。** 实测 x 轴刻度 `2.0`/`2.5` 被渲染成 `20`/`25`（同图其余刻度
   正常；`Hiragino Sans GB`/`Heiti SC` 正常；SVG 后端 dump 出的标签字符串是正确的 `2.5`，
   所以问题在 raster 字形层）。→ macOS 候选序**排除** PingFang，并有单测钉住
   （`macos_candidates_never_include_pingfang`）+ 像素级回归测试
   （`decimal_point_survives_in_default_font`）。

顺带记一条尺寸语义：charton 的 `with_size(w, h)` 是**逻辑尺寸**，实际像素 = `w × scale_factor`
（默认 **2.0**，实测 `with_size(600,400)` 产出 1200×800）。本包口径是「`--width/--height` 就是
最终 PNG 像素数」，实现为 `with_size(w,h)` + `with_scale_factor(1.0)`。

## 测试

```sh
export TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target
cargo test --manifest-path crates/abb-chart/Cargo.toml
```

- 单元测试：参数/规格/数据装载/字体解析/通道表；其中三条是**实测回归钉子**（不存在的族名、
  无中文字形的族名、小数点被吞）。
- `tests/cli_e2e.rs`：跑**真实二进制**，覆盖退出码契约（0/1/2）、`--spec -`、缺省 `--out` 落点、
  目录自动创建、golden 规格（`tests/golden/cjk_bar.*`，中文标题 + 中文轴标题 + 小数刻度的
  人工目视参考产物）。

> 真实系统字体依赖：只有 `fonts::tests::real_system_has_a_usable_cjk_candidate` 与
> `decimal_point_survives_in_default_font` 依赖本机装有中文字体（macOS/Windows/Linux 主流发行版
> 都自带）。其余测试用可注入的假字体库，不受本机字体影响。

## 分发

- macOS：`crates/abb-chart/target/aarch64-apple-darwin/release/abb-chart` 拷进
  `ABB.app/Contents/MacOS/`，与 `abb-agent` 一并 `codesign`（`release.yml`）。
- Windows：`.iss` 拷到 `{app}\` 根、`win-build-installer.ps1` 构建 + 新鲜度断言、
  `tools/check_dynamic_crt.sh` 一并检查（缺 DLL 会在第一次真出图时才炸）。

两平台都必须落在主程序**同目录**：`agent-bridge chart` 按 `current_exe` 同目录解析
（`bundled_chart_path_in`），找不到就**显式报错**（不静默降级——静默成功会让模型以为图已画好，
而用户什么也收不到）。
