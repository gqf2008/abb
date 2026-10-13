//! ABB 的图形生成 CLI（`abb-chart`）。
//!
//! 定位：agent 在会话里需要出图时，由桥侧 `"$ABB_BIN" chart …` 转发到本程序，产出 PNG
//! 落到会话工作区，再由 `"$ABB_BIN" deliver --to-current --file <png>` 发回聊天。
//! 本包只做「图规格 → PNG」，不碰聊天通道、不碰权限（授权由桥的 guard 负责）。
//!
//! ## 分层
//!
//! - [`cli`]：参数解析与用法文本（手写，与仓库既有 CLI 风格一致，不引 clap）。
//! - [`data`]：CSV / JSON 行 → charton `Dataset`（类型推断：全列可解析为数字 ⇒ 数值列，否则分类列）。
//! - [`fonts`]：字体族解析。**这是本包最容易被忽略却最致命的一段**，见 [`fonts`] 模块文档。
//! - [`spec`]：图规格（JSON）模型 + 「规格 → 渲染」的编排。
//!
//! ## 为什么输出只承诺 PNG
//!
//! 三个聊天平台的图片通道白名单都是 png/jpg/jpeg/gif/bmp，svg/webp 会被降级成文件卡片
//! （见 `src/messenger.rs` 的 `feishu_send_plan` / `dingtalk_send_plan` / `wechat_media_kind`）。
//! 所以对外只承诺 PNG；SVG 仅作为本地排查手段（`--debug-svg`）。
//!
//! ## 已知上游缺陷（本包据此做了规避，**不要**当成我们的 bug 去改）
//!
//! 1. 默认主题的字体族是 CSS 栈字符串（`"Inter, …, 'PingFang SC', …"`），而 raster 后端的
//!    `get_raster_font` 把它当**单个族名**查询 ⇒ 永远匹配不上 ⇒ 静默回落内置 Inter（纯拉丁）
//!    ⇒ 中文出「豆腐块」。上游 PDF 路径（`resolve_pdf_font`）已按逗号拆栈，raster 没有。
//!    规避：本包把主题的四个 family 字段都设成**单个真实族名**（见 [`fonts`]）。
//! 2. `PingFang SC`（macOS 中文主力字体）在 raster 路径下会**吞掉小数点**：`2.5` 渲染成 `25`。
//!    同一张图其余刻度正常。故本包的 macOS 候选序**排除** `PingFang SC`。
//! 3. 不存在的族名静默回落 Inter（不报错）。规避：本包先自己用 fontdb 探测，缺了就**显式失败**。

pub mod cli;
pub mod data;
pub mod fonts;
pub mod render;
pub mod spec;

pub use cli::{parse_args, Invocation, UsageError};
pub use spec::{render_to_png, RenderError, RenderOptions};
