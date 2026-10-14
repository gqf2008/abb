# 系统级对话（syschat）交接文档

> 面向**下一个接手的人**（或新会话）。目标：读完这一份就能继续干，不用重新踩坑。
> 首版写于 `main = 7b8eea5`（2026-10-14 深夜）；**2026-10-15 随卡 `abb-syschat-resize-macos-20261015` 更正 §0/§1/§5.1/§5.2/§6**（`main = d5ddd9e`）；**同日随卡 `abb-v11-message-actions` 更新 §0/§5.2/§7.3**（消息动作从「未做」移到「已做」，评审两条 blocking 的修法见 §5.2）；**同日随卡 `abb-syschat-e3-tokens-20261013` 更新 §0/§5.2/§6/§7.3**（Token 真实用量落地：ACP `_meta.usage` 是真来源，不是估算——上一版「`acp.rs` 没有 usage 来源」的结论是**错的**，见 §6）。**事实以仓库代码/卡为准，本文只是索引与踩坑记录。**

---

## 0. 30 秒现状

- 功能主体**已经可用**：气泡（常驻置顶）+ 对话框（标题栏一行 / 状态栏两行 / 输入区功能区）+ 流式打字机 + 附件/拖拽 + Markdown（含 HTML 媒体）+ 定时发送 + 插入/打断（排队）+ **消息动作**（纯文本块可选中复制；助手轮 ⧉ 复制 / ⟳ 重新生成 / 🔊 朗读）。
- **对话窗缩放已修**（卡 `abb-syschat-resize-macos-20261015`）：根因**更正为「透明的无边框窗在 macOS 没有系统边角缩放」**（上一版写的 `winit is_resizable=false` 是「未 realize」的误读，见 §5.1）⇒ 对话窗**恒画热区 + macOS 自管缩放**。剩**owner 真机复验**（手拖边角的手感；锁屏时合成点击打不进 app）。
- **历史会话归档已做**（卡 `abb-v11-session-archive`）：会话菜单「🗄 归档…」归档当前会话（从列表隐藏）、「🗄 已归档（N）…」进归档视图（点一条 = 取消归档并切回）；**不删任何历史文件**。剩 **owner 真机复验**（点开面板 → 归档 → 已归档 → 找回；锁屏时合成点击打不进 app）。
- **Token 真实用量已做**（卡 `abb-syschat-e3-tokens-20261013`）：状态栏「已用/上限 · 百分比」+ 进度条（≥70% 橙 / ≥90% 红），悬停给精确 token 数**和口径**；值是**真实计数**（执行层从 provider 响应上报的 prompt tokens），不是 `chars/4` 估算。上限来源：设置里 `context_window` 覆盖 > 内置常见模型表 > `—`（不知道就不印数）。
- 还没做：**截屏**、**思考块真机可见**（卡在 `abb-agent`）、**设置窗在 macOS 缩放**（§5.2，根因同 §5.1）。

---

## 1. 怎么自己验收（不用人工点击！）

终端**没有辅助功能权限**（点不了气泡），所以仓库里留了一个**环境变量自查口**：

```sh
export TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-d
cargo build                       # 或 --release + --target aarch64-apple-darwin
BIN=$CARGO_TARGET_DIR/debug/agent-bridge
mkdir -p /Volumes/DataExt/tmp/abb-verify-home
HOME=/Volumes/DataExt/tmp/abb-verify-home ABB_VERIFY_OPEN=1 "$BIN" > /tmp/verify.log 2>&1 &
sleep 12
swift /Volumes/DataExt/tmp/abb-verify/session.swift        # 先查是否锁屏（见下方注意）
grep -a '\[verify\]\|\[selfresize\]' /tmp/verify.log       # 窗口底层事实（is_resizable 等）
swift /tmp/winid.swift | awk -F'\t' '$2=="ABB"'            # 列出 ABB 窗口（拿 window id）
screencapture -x -o -l<id> /tmp/shot.png                   # 按 id 抓图（不需要辅助功能权限）
```

- `ABB_VERIFY_OPEN=1`：**直接打开对话窗**并打印窗口事实。**必须配隔离 HOME**（否则会触发 `platform::heal_autostart()` 改写用户真实 `~/Library/LaunchAgents/`）。
  探针按 **400ms 一档打 5 拍**，并**显式区分** `winit=None`（还没 realize）与 `Some{is_resizable,…}`：`t=400ms` 常是 `None`（`show()` 不同步建窗），`t=800ms` 才有原生窗事实——**别把 `None` 读成 `is_resizable=false`**（§5.1 的坑）。
- ⚠️ **锁屏时合成点击打不进 app**：`CGSSessionScreenIsLocked=1` 时 `CGEvent` 落在 `loginwindow`，`ABB 系统对话` 收不到按下（拖拽/缩放/点按钮类验证必须先解锁）。查锁屏态：`swift /Volumes/DataExt/tmp/abb-verify/session.swift`。
- `swift /tmp/winid.swift`：一个 8 行的 CoreGraphics 枚举脚本（若 `/tmp` 被清掉，见 §7 附录脚本）。
- 无头出图台（**不需要真窗口**）：`src/ui_headless_render_test.rs`，`#[ignore]` 用例，例如
  `bash tools/check_test_isolation.sh --bin agent-bridge -- --ignored dump_frames`；
  出图落 `/Volumes/DataExt/tmp/syschat-shots/`（`short.png` 整窗、`bottom.png` 滚到底、`v11-*.png` 面板/状态区/split）。**同输入 ⇒ 同像素**，可当对稿基准。

---

## 2. 环境与流程铁律（违反会伤到用户机器或浪费时间）

1. **构建产物必须外置**：`export TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-*`（启动卷只剩几十 G）。`bash tools/gates.sh` 会替你设好并跑四门禁。
2. **不要跑裸 `cargo test`**（会写真实 `~/.agent-bridge`）⇒ 用 `bash tools/check_test_isolation.sh`（支持过滤参数）。
3. **不要直接跑 dev GUI**（会改真实 LaunchAgents）⇒ 跑隔离 HOME（§1）。
4. **共用热 target 目录**（`abb-target-d`）能省 20 分钟/轮的冷编译——这是本会话前半段最大时间浪费来源。
5. **walgit 协作**：卡 = `walgit collab entry`；`--parent` **必须是上一条条目的真实 oid**（拿 issue oid 当 parent 会造分叉）。
6. **提交信息不要用反引号**（bash 会当命令替换执行）⇒ 用 `git commit -F <file>`。
7. **owner 已把单条开发的门禁+评审降档**：单条只跑 `cargo build` + 定向隔离测试；**批量交付/上线前**再统一跑四门禁（fmt / clippy `-D warnings` / `build --locked` / 隔离门禁）与独立评审。
8. **本机 clippy 基线本已红**（rustc/clippy 1.99）：只保证**零新增 lint**（与 base 比对 lint 多重集）。
9. **发版/安装**（`scripts/build.sh` 硬编码 `target/`，会落到启动卷 ⇒ 用手动组装）：
   见 §7 附录的安装命令（release 构建 → 组 bundle → `agent-bridge-dev` 签名 + `app-assets/abb.entitlements` → `tools/check_entitlements.sh` → 装 `/Applications/ABB.app`）。

---

## 3. 设计与需求

- **设计稿（视觉唯一事实源）**：`docs/design/syschat-dialog.html`（当前 **v11.1**）。里面那台对话框的 **CSS 数字就是规格**（`.titlebar` 项高 26、`.status` 38、`.composer-zone` padding 10、`.ib` 24、`.send` 30/0 14/12px、`.split` 10px…）。
  - **实时看稿**：`open docs/design/syschat-dialog.html`；浏览器里按 ⌘R 刷新（改稿后建议同时改 `<title>`，否则分不清看的是哪版）。
  - **口径优先级**：设计稿 > 卡面文字（稿与卡冲突时以稿为准；例：`split` 稿 10px / 卡写 8px ⇒ 取 10px）。
- **需求清单（13 条）与分批**：`docs/design/README.md`。
- **v11 新增 7 条**：面板贴按钮上方 / 截屏 / 状态栏用量条 / 窗口非固定宽 + 内容区↔输入区可拉升 / 消息可复制 + 复制·重新生成·朗读 / 历史会话归档 / 新建会话去重。

---

## 4. 已完成（按批，含关键 commit）

| 批 | 卡片 | 交付 |
|---|---|---|
| 基础 | `abb-syschat-bubble-20261012` | 气泡 + 对话框 + 历史落盘 + 可达性预检（`4c56ed1`） |
| A | `abb-syschat-a-model-session-20261013` | 会话粒度模型选择 + 会话历史（`05012a0`） |
| B | `abb-syschat-b-markdown-20261013` | `src/mdblocks.rs`（markdown→块）+ GUI 块渲染（Rust 侧 `StyledText`）（`369a92a`） |
| C1 | `abb-syschat-c-attachments-20261013` | 附件/剪贴板/📎 多选 + 落工作区 + 路径进 prompt（`71dd477`） |
| C2 | `abb-syschat-c2-dragdrop` | Finder 拖拽（**winit 自带 drag destination**，零 AppKit 代码）（`b5e9973`） |
| D | `abb-syschat-d-composer-bubble-20261013` | 输入区新布局 + 3+N chips + 气泡按下反馈/跟随（`cb8022e`） |
| D2 | `abb-syschat-d2-titlebar-20261013` | ①②③ 进标题栏 + 无头渲染回归台（`978a948`） |
| D3 | `abb-syschat-d3-v10-align-20261014` | 按 v10 1:1 对齐（`213d0f2`） |
| D5/D6 | `abb-syschat-d5-hide-on-blur` / `d6-titlebar-buttons-broken` | 失焦隐藏 + 不进 Dock；**三钮回调接线根因**（`97e24ec`） |
| D7 | `abb-syschat-d7-md-media-broken` | HTML `<img>/<audio>/<video>` 解析 + 路径基准统一到工作区（`f8dc32c`） |
| D4 | `abb-syschat-d4-bubble-follow` | 气泡吸附窗口四角（单一事实源＝窗口矩形 + 4 条防环）（`7eb42fa`） |
| D8 | `abb-syschat-d8-stream-thought-types` | 思考+正文打字机（客户端 `Typewriter` 承重）（`bc9a959`） |
| D10 | `abb-syschat-d10-insert-interrupt` | 生成中插入/打断（steer + 排队）（`9382c9c`） |
| D9 | `abb-syschat-d9-emoji-more` | emoji 面板 + 定时发送（`28c28d3`） |
| v11a | `abb-v11-bubble-follow-lag` / `v11-panels-align` | 跟手改挂 common-modes 时钟；会话/模型面板 + 状态区按稿（`869592d`） |
| v11b | `abb-v11-new-session-dedupe` / `v11-popup-anchor` / `v11-resizable-split` | 新建去重；面板贴按钮；分割条+比例持久化（`3c9624a`） |
| v11c | `abb-v11-resize-maximize-macos` | 自管缩放/最大化 + 三处视觉回归（`360069f`） |
| — | — | 缩放回滚/更正（`ecc627d` → `252d027` → `7b8eea5`，见 §5） |

---

## 5. 卡住的 / 未完成的（含硬证据）

### 5.1 对话窗不能缩放（**已修**：卡 `abb-syschat-resize-macos-20261015`，剩 owner 真机复验）
**根因（2026-10-15 实测更正——上一版的「`winit is_resizable=false`」结论作废）**：决定项是**透明度**，不是 `is_resizable`。
1. 那个 `false` 是**误读**：来自 `with_winit_window(..).unwrap_or((false, false))` 在窗口**未 realize** 时顶的默认值。`show()` **不同步建原生窗**（`set_visible` 只把 `visible` 存进 attributes、把窗口登记为 inactive window），自查口 400ms 一档实测：`t=400ms ⇒ winit=None`，`t=800ms ⇒ Some is_resizable=true is_decorated=false`（`/private/tmp/verify-a1.log`、`verify-a2.log`）。⇒ **`is_resizable` 既不是假、也不是「能不能拖边角」的判据。**
2. **透明的无边框窗在 macOS 拿不到系统边角缩放**：同参数 A/B（最小复现件 `/private/tmp/minwin` + `/private/tmp/probe_min2.swift`：`CGEvent` 拖右缘（若干 inset）+ `CGWindowListCopyWindowInfo` 量宽）——`with_transparent(true)`：**0/2** 次；只差「不透明」的同参数窗：**2/2** 次；两者 `is_resizable` 都读到 `true`。该 A/B 由上一轮会话跑出、**结果未落盘**，本卡亦未独立复现（锁屏期点击打不进 app，见 §1 注意）。
3. ABB 两个窗都是 Slint `no-frame` ⇒ winit 后端一律 `with_transparent(true)`（`wants_transparent`，圆角靠它）⇒「交给系统缩放」在本仓**根本不存在**。于是 `360069f`(v11c) → `ecc627d` → `7b8eea5` 那条「macOS 关掉热区、交给系统」是**死路**：关掉热区 = 完全不能缩放。
4. 仍成立但**不解释拖不动**的三条背景事实：winit macOS 无边框窗样式位含 `Resizable`（`window_delegate.rs:542-544`）；`drag_resize_window` 在 macOS 返回 `NotSupported`（`:1179`）；属性钩子里的 `.with_resizable(true)` 会被 Slint 随后写入的 `attributes.resizable` 覆盖（`winitwindowadapter.rs:266`）。

**修法（已落地）**：对话窗**恒画热区**（`syschat_resize_hit_areas_enabled()` → `true`），macOS 走**自管缩放**（`syschat_resize_self_managed()` → `cfg!(target_os = "macos")` + `SelfResize`/`ResizeSession`：按屏幕坐标跟指针算尺寸，先尺寸后位置）；设置窗仍只在 Windows/Linux 画（`settings_resize_hit_areas_enabled()`——它的热区接 `drag_resize_window`，macOS 上是死区）；`show_window_and_focus()` 里「show 之后补 `set_resizable(true)`」整段删掉（无效时机，且 Slint 建窗时按 `min != max` 自己定 resizable）。
**守卫**：`ui::tests::resize_hit_areas_follow_platform_rule`（结构：对话窗恒画 / 两窗口径分开 / 自管 = macOS）+ 无头几何 `ui_headless_render_test::{self_managed_hit_zones_route_press_with_pointer_coords, v11c_resize_zones_hit_all_edges_and_corners, resize_hot_zone_eats_titlebar_top_band}`。
**证据（`/private/tmp` 会被清，脚本可按 §1 重建）**：`verify-a1.log`/`verify-a2.log`（Some/None + `is_resizable`）、`verify-0120.log`/`verify-0245.log`（`[selfresize] begin dir=east` + `ret=true` + 其后数百次 `cursor_moved`）、`syschat-shots/verify-isresizable.png`（对话窗真开出来）。
**未验证（owner 项）**：真机手拖边角「窗口真的变大变小 + 手感/发不发飘」，以及自管缩放与最大化（`toggle_syschat_maximize`）的互动。合成点击**必须先解锁屏**。

### 5.2 其余未做
| 项 | 状态 | 下一步 |
|---|---|---|
| **截屏 `✂`** | 只有占位（按下写「尚未实现」） | 卡 `abb-v11-screenshot`；做原生截屏需 **macOS 屏幕录制 TCC 权限**，要先探权限 + 被拒给可读引导；`screencapture` 子进程是可行退路 |
| **消息动作**（可选中复制 / 复制 / 重新生成 / 朗读） | **已做**（卡 `abb-v11-message-actions`，2026-10-15 合入 main） | 口径＝**分层**：无行内样式的文本块换只读 `TextInput`（可选中 + ⌘C），助手轮整轮 `⧉ 复制` 兜底；**含行内样式的块**（表格/媒体/粗体/行内码/链接/`~~` 删除线）**仍不可选**——`TextInput` 渲染不了样式，宁可不选也不回退 B 批的样式。动作行只带**真实历史下标**、点击时才由 `syschat::action_target` 解析（**不许用「负值＝最后一条助手轮」哨兵**：第二轮收尾后模型里有两条同值哨兵，会一起解析到同一轮 ⇒ 点第一轮操作了第二轮，评审判 Critical）。剩 **owner 真机复验三项**：① 真机剪贴板端到端（⌘V 粘出）；② 真机拖选高亮 + ⌘C；③ 真机 `say` 出声与 ⏹ 停止（锁屏期合成点击打不进 app；`say` 这条单测走 `/bin/sleep` 替身，只证「隐藏路径会停进程」） |
| **历史会话归档** | **已做**（卡 `abb-v11-session-archive`，2026-10-15 落 main） | 口径＝**隐藏 + 可找回，不删数据**：`SessionMeta.archived`（`#[serde(default)]` ⇒ 旧 `sessions.json` 缺字段按未归档）；`SessionStore::list()` 过滤归档、`archived()` 只列归档；归档**当前**会话时 `ensure_current` 把 current 落到最近活动的未归档会话（一条不剩则起一条新的、沿用模型选择，**不擅自取消归档**）；`set_archived` 幂等（同值/未知 id 不落盘）。UI：第一行两义（普通「🗄 归档…」/ 归档视图「← 返回会话列表」）、第二行「🗄 已归档（N）…」；文案与「哪份列表 + 清空当前项」的判据都在 Rust 纯函数里（`session_panel_text` / `session_menu_view`）。单测 5 条（归档过滤/找回/不删文件、current 迁移、幂等不落盘、旧 JSON 兼容、两视图文案）+ ui 行判据 + 无头「第二行真的画出来」+ 出图。剩 **owner 真机复验**（三项点击路径；锁屏期合成点击打不进 app） |
| **Token 真实用量** | **已做**（卡 `abb-syschat-e3-tokens-20261013`，2026-10-15 落 main） | 口径＝**真实计数，不是估算**：`crates/abb-agent`（rpi 驱动）本来就从 provider 响应里解出 `AssistantMessage.usage`，只是**没往外报** ⇒ 现在随 `session/prompt` 结果 `_meta.usage`（camelCase `input/output/cacheRead/cacheWrite/total`）上报，根侧 `runtime::acp::parse_usage` 收下（缺字段/全零 ⇒ `None`，绝不编数），经 `pool::PromptResult::final_usage` → harness 旁路表 → `oneshot` → `syschat::TurnEnd::Done{usage}` → 写 `SessionMeta.context_tokens`（**prompt 侧** `input＋cacheRead＋cacheWrite`，不含 output）。上限：`providers[].context_window`（`#[serde(default)]`，旧 config 兼容）**覆盖**内置常见模型表（子串匹配、长键在前；匹配不到 ⇒ `None` ⇒ 印 `—`）。UI 文案/档位/提示全是纯函数（`context_usage_text` / `usage_level` / `context_usage_tip`），渲染半是无盘注入（`ui::apply_syschat_usage`）⇒ 无头台可断言任意档位。剩 **owner 真机复验**：真机跑一轮看数字随回合变化 + 设置窗「上下文窗口」填/清（`0` = 用内置表）|
| **思考块真机可见** | GUI 全就绪（折叠块 + 耗时 + 打字机），**执行层不发** | 卡 `abb-agent-project-thinking-20261014`：`crates/abb-agent` 的 `event_updates` 未映射 rpi 的 `ThinkingDelta`，且 `thinking_level` 默认 `Off`（打开会改模型请求参数，建议默认关 + 设置项） |
| **插入（steer）** | GUI + 发送端已就绪；**生产恒走排队** | `abb-agent` 如实上报 `steering.supported=false`（`_session/steering` 回 `-32601`）⇒ 要真插入得换声明支持该扩展的适配器，或改 `abb-agent` |
| **设置窗在 macOS 缩放** | 未做 | 它的热区接 `drag_resize_window`（macOS `NotSupported`）= 死区 ⇒ 要可缩放得给它也接自管循环（`SelfResize` 现在只绑在对话窗的 `on_self_resize_begin` 上）；§5.1 的更正说明同样适用 |
| 跟进卡 | — | `abb-syschat-b-followups` / `abb-syschat-c-followups`（B/C 批评审的非阻塞项） |

---

## 6. 关键技术结论（**别再重走弯路**）

**用量（E3）**
- 上一版写「`acp.rs` 没有任何 usage 来源（goose 注释明说不报）」是**针对旧适配器的过时结论**：本仓执行层是 `crates/abb-agent`（rpi 驱动），rpi 的 provider mapper 会把 `usage` 填进 `AssistantMessage`（anthropic 填 `input/cache_read/cache_write`，openai/faux 同类），**只是没往外转发**。⇒ 别退到 `chars/4` 估算，真值就在手边。
- 口径：**prompt 侧** `input + cacheRead + cacheWrite`（与 rpi 自己的 `cache_stats.rs` 同口径），**不含 output**——「上下文占用」问的是「这一轮请求送进去多少」，不是「产出多少」。
- 「缺」与「0」不同义：`used = None`（还没报过）≠ 占用 0；`limit` 的 `0`（旧配置「没设」）归一成未知。任何一格缺就印 `—`，**不许填数**。
- 这个接缝的读半（`syschat::context_usage`）会读真实 `~/.agent-bridge` + `config.json` ⇒ **纯读**：状态文件不存在就直接返回 `(None, None)`，目录/锁文件都不建（不然全量单测门禁会红在「隔离 HOME 下多出 `syschat/history.lock`」）。渲染半**必须**是注入式（`ui::apply_syschat_usage`），否则无头渲染台的断言会随开发机真实会话变红。

**Slint / slint-pixel**
- `@markdown("{x}")` **不是插值模板**（要以 `\{` 起），且即便插值也只按**纯文本**注入 ⇒ 行内样式必须在 **Rust 侧** 用 `StyledText::from_markdown`（B 批评审 B1/B2；`src/mdblocks.rs::styled`）。
- Slint **没有 `audio`/`video` 元素** ⇒ 音视频走**媒体卡 + 系统播放器**。
- **`PixelTitleBar` 是上游封闭组件**：宿主必须自己转发 `minimize` / `toggle-maximize` / `drag-start`（只写 `close-window` 会导致三钮点了没反应——D6 根因）。
- `Tooltip.text` 是 styled-text：`Tooltip { text: @markdown("\{root.tip}") }`。
- 在布局容器里写 `x:`/`width: parent.width + Npx` 容易触发 **width ←→ layoutinfo 绑定循环**（编译器警告「可能运行时 panic」）⇒ 分割线两端那 2px 就是窗口黑描边，别去盖。
- 消息区每个会换行的文本**必须带 px 宽绑定**（`convo-text-width`，由 Rust 写），否则估高偏小 ⇒ 叠字/滞底算不到底。
- 无头出图台里的字体可能缺 emoji/符号字形（真机用系统字体），**布局可信、字形别下结论**。

**macOS 窗口**
- `no-frame: true` + winit macOS：样式位 `Borderless | Resizable | Miniaturizable`，但**系统边角缩放不是自动就有**——决定项是**透明度**：winit 对 Slint `no-frame` 窗恒 `with_transparent(true)`，而**透明的无边框窗没有系统边角缩放**（同参数 A/B：透明 0/2、不透明 2/2，见 §5.1）。`is_resizable=true` **推不出**「能拖边角」。
- 要能拖就只能**自绘热区 + 自管缩放**（`SelfResize`/`ResizeSession`：屏幕坐标跟指针算尺寸、先尺寸后位置）；`drag_resize_window` 在 macOS **`NotSupported`**，接它等于画个死区。
- **锁屏时合成点击（`CGEvent`）落 `loginwindow`**：任何「点按钮/拖边角」类验证前先查 `CGSSessionScreenIsLocked`（§1）。
- **窗口是首次 `show()` 才创建**（`with_winit_window` 之前返回 `None`、尺寸 0×0）。
- **拖拽/缩放期间 winit 不交窗口事件**（系统进 event-tracking 模式）⇒ 「跟手」类需求必须挂 **common-modes 时钟**（D4 的跟手修复）。
- 失焦判定**不能用「窗口是否还是 key」**：`windowDidResignKey:` 早于 app 失活 ⇒ 要延后 + 查 `NSApp isActive`（D5）。
- 保持 **accessory 激活策略**（`Info.plist` 的 `LSUIElement`）⇒ 不进 Dock；`platform::hide_dock/activate_app/app_is_active` 已封装。

**执行层 / 流式**
- 流式 sink **照 `resource_events` 模式**做（`Option<UnboundedSender<..>>` + 自由函数便于直测 + 在唯一 spawn Ok 臂安装）；`TurnChunk{ session_id, kind: ChunkKind, text }` 已带 kind，按 `turn_session` 过滤。
- 客户端 `Typewriter`（`step = max(1, pending/2)`，按 char 边界）是**承重件**：后端可能一次吐一大段。
- `Reply` 收尾要**等两路打字机推完**再以 `final_text` 整段重建（去重口径）。
- `land_attachments` / `resolve_media_path` / `split.json` / `bubble.json` 一类持久化都**去抖 + 失败静默**（与既有口径一致）。

**其它**
- 媒体/图片的相对路径基准是 **`~/.agent-bridge/workspaces/syschat/`**（不是进程 CWD）；`file://` 要按绝对路径处理。
- 经验沉淀目录：`~/.agents/rules/`（本会话新增了若干 LESSON：Slint 组件属性、macOS 拖拽期无事件、后端整条回传要客户端补字、`timeout_at`+`watch` 忙等、Windows 平台门控 dead_code 等）。

---

## 7. 附录

### 7.1 枚举 ABB 窗口的脚本（`/tmp/winid.swift`）
```swift
import CoreGraphics
import Foundation
let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
for w in info {
    let owner = w[kCGWindowOwnerName as String] as? String ?? ""
    let name  = w[kCGWindowName as String] as? String ?? ""
    if owner.contains("ABB") || name.contains("系统对话") {
        print("\(w[kCGWindowNumber as String] as? Int ?? 0)\t\(owner)\t\(name)\t\(w[kCGWindowBounds as String] ?? [:])")
    }
}
```

### 7.2 构建 + 安装（手动组装，产物外置）
```sh
cd /Volumes/DataExt/GitHub/abb
export TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-release
cargo build --release --locked --target aarch64-apple-darwin
cargo build --release --manifest-path crates/abb-agent/Cargo.toml --target aarch64-apple-darwin
T=$CARGO_TARGET_DIR/aarch64-apple-darwin/release; B=/Volumes/DataExt/tmp/ABB-stage/ABB.app
rm -rf /Volumes/DataExt/tmp/ABB-stage; mkdir -p "$B/Contents/MacOS" "$B/Contents/Resources"
cp "$T/agent-bridge" "$T/abb-agent" "$B/Contents/MacOS/"
cp app-assets/Info.plist "$B/Contents/Info.plist"; cp app-assets/AppIcon.icns "$B/Contents/Resources/"
cp LICENSE "$B/Contents/Resources/ABB-LICENSE.txt"
for f in "$B/Contents/MacOS/agent-bridge" "$B/Contents/MacOS/abb-agent"; do
  codesign -s "agent-bridge-dev" --force --timestamp=none --entitlements app-assets/abb.entitlements "$f" >/dev/null; done
codesign -s "agent-bridge-dev" --force --timestamp=none --identifier "agent-bridge" \
  --entitlements app-assets/abb.entitlements "$B" >/dev/null
./tools/check_entitlements.sh "$B"           # #251：三项 TCC entitlements 必须在
osascript -e 'quit app "ABB"' 2>/dev/null; sleep 2
pkill -f "/Applications/ABB.app/Contents/MacOS/agent-bridge" 2>/dev/null; sleep 1
cp -R /Applications/ABB.app "/Volumes/DataExt/tmp/ABB.app.prev-$(date +%Y%m%d-%H%M%S)"   # 备份（别省！）
rm -rf /Applications/ABB.app; ditto "$B" /Applications/ABB.app; open /Applications/ABB.app
```
> ⚠️ `app-assets/abb.entitlements` **必须带上**（相机/麦克风/自动化），否则 hardened runtime 下静默拒绝。

### 7.3 接手第一小时建议
1. 读 §1，自己开窗抓一张图，建立「我能自己验收」的闭环（这一步能省掉后面所有「让用户截图」的往返）。
2. §5.1 缩放**已修**（卡 `abb-syschat-resize-macos-20261015`：对话窗恒画热区 + macOS 自管缩放），剩**真机手感**复验；**别再拿 `is_resizable` 取证**（它既不假也不是判据，见 §5.1）。
3. 按 §5.2 从上往下做（**缩放 ✅ / 消息动作 ✅ / 归档 ✅ / 用量(E3) ✅ 已完成**；剩：截屏 → 设置窗缩放；思考/steer 要动 `abb-agent`，可最后做）。
4. 每做完 2–3 条，**批量**跑四门禁 + 独立评审 + 打包安装一次（owner 已同意这条降档口径）。
