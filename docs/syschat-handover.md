# 系统级对话（syschat）交接文档

> 面向**下一个接手的人**（或新会话）。目标：读完这一份就能继续干，不用重新踩坑。
> 首版写于 `main = 7b8eea5`（2026-10-14 深夜）；**2026-10-15 随卡 `abb-syschat-resize-macos-20261015` 更正 §0/§1/§5.1/§5.2/§6**（`main = d5ddd9e`）；**同日随卡 `abb-v11-message-actions` 更新 §0/§5.2/§7.3**（消息动作从「未做」移到「已做」，评审两条 blocking 的修法见 §5.2）；**同日随卡 `abb-v11-screenshot` 更新 §0/§5.2/§6/§7.3**（✂ 截屏落地）；**同日随卡 `abb-syschat-e3-tokens-20261013` 更新 §0/§5.2/§6/§7.3**（Token 真实用量落地：ACP `_meta.usage` 是真来源，不是估算——上一版「`acp.rs` 没有 usage 来源」的结论是**错的**，见 §6）；**同日随卡 `abb-settings-window-resize-macos-20261015` 更新 §0/§5.1/§5.2/§6/§7.3**（设置窗在 macOS 也能缩放；顺带修掉「设置窗/弹窗里的 `ImageIcon` 图标**画不出来**」——组件根 `Image` 缺整图裁剪，见 §6）。**2026-10-15 05:44 批量安装**（owner 同意的口径：每做完 2–3 条，批量跑门禁 + 打包安装一次）：`/Applications/ABB.app` 已换成 `main = 2571240` 的 release 构建（marker `abb-guide-v20`），旧 bundle 备份在 `/Volumes/DataExt/tmp/ABB.app.prev-20261015-054438`；四条门禁在**整棵树**上跑过（fmt / clippy 对基线零新增 / `build --locked` / 隔离全量 lib 2 + bin 1272 + 集成 19），**没有独立评审**（本 worktree 只有一个身份，且 owner 已把仪式降档）——照实记在这里，别当成「审过了」。**同日 05:53 再装一次**（思考分片投影落 `main = 525a8d9`）：`/Applications/ABB.app` 换成该 head 的 release 构建（主二进制 `Contents/MacOS/abb-agent` 从此含 `agent_thought_chunk`；旧 bundle 备份 `/Volumes/DataExt/tmp/ABB.app.prev-20261015-055344`），启动日志 harness 装配走新 `abb-agent`、bot 庆小丰 在线、`manual=按需派发（并发闸门：不节流）`。**同日 06:01 第三次装机**（插入 steer 落 `main = 0dce2b6`）：`/Applications/ABB.app` 又换一次 release 构建（主二进制 `abb-agent` 从此含 `_session/steering`；旧 bundle 备份 `/Volumes/DataExt/tmp/ABB.app.prev-20261015-060642`），启动日志 harness 走新 `abb-agent`、bot 庆小丰 在线。⇒ **§5.2 的功能清单至此全部落地**（剩下都是 owner 真机复验 + 两张跟进卡）。**事实以仓库代码/卡为准，本文只是索引与踩坑记录。**

---

## 0. 30 秒现状

- 功能主体**已经可用**：气泡（常驻置顶）+ 对话框（标题栏一行 / 状态栏两行 / 输入区功能区）+ 流式打字机 + 附件/拖拽 + Markdown（含 HTML 媒体）+ 定时发送 + 插入/打断（排队）+ **消息动作**（纯文本块可选中复制；助手轮 ⧉ 复制 / ⟳ 重新生成 / 🔊 朗读）。
- **对话窗缩放已修**（卡 `abb-syschat-resize-macos-20261015`）：根因**更正为「透明的无边框窗在 macOS 没有系统边角缩放」**（上一版写的 `winit is_resizable=false` 是「未 realize」的误读，见 §5.1）⇒ 对话窗**恒画热区 + macOS 自管缩放**。剩**owner 真机复验**（手拖边角的手感；锁屏时合成点击打不进 app）。
- **历史会话归档已做**（卡 `abb-v11-session-archive`）：会话菜单「🗄 归档…」归档当前会话（从列表隐藏）、「🗄 已归档（N）…」进归档视图（点一条 = 取消归档并切回）；**不删任何历史文件**。剩 **owner 真机复验**（点开面板 → 归档 → 已归档 → 找回；锁屏时合成点击打不进 app）。
- **Token 真实用量已做**（卡 `abb-syschat-e3-tokens-20261013`）：状态栏「已用/上限 · 百分比」+ 进度条（≥70% 橙 / ≥90% 红），悬停给精确 token 数**和口径**；值是**真实计数**（执行层从 provider 响应上报的 prompt tokens），不是 `chars/4` 估算。上限来源：设置里 `context_window` 覆盖 > 内置常见模型表 > `—`（不知道就不印数）。
- **截屏已做**（卡 `abb-v11-screenshot`）：功能区 ✂ → 选区/选窗（`screencapture -i`）→ 落进待发附件（与 📎/⌘V 同一条落地路径）。缺「屏幕录制」授权时**先探权限、不拍**，状态行给三要素引导（哪项权限 / 去哪儿开 / 开完要重启）。
- **设置窗在 macOS 也能缩放**（卡 `abb-settings-window-resize-macos-20261015`）：与对话窗同一套口径（恒画热区 + macOS 自管缩放，两个窗**各一份** `SelfResize`——下界不同）。顺带修掉一个**一直存在但没人报**的缺陷：设置窗与两个弹窗里的 `ImageIcon` 图标**从来没画出来过**（组件根 `Image` 缺「整图裁剪」⇒ `fit()` 出 NaN，见 §6）；无头台现在是**第一条能渲染设置窗**的用例。剩 **owner 真机复验**（拖设置窗边角 + 看设置窗/弹窗图标是否出来）。
- **手动档 task 执行已改按需派发**（卡 `abb-task-workers-manual-not-a-pool-20261015`，`4cb33d0` + `2571240`）：`task_workers.manual` 默认 **0 = 不节流**（写 N 才是「同时最多 N 条」），手动档只起 1 条 poller、认领到就起执行体；工作区指引那句「并发额度有限……它不是不限量的池子」已删。**注意**：这个面被 owner 反复说过四次，前三次都只在改数字（1 → 4 → 128），**根因是它当时是「固定池」**；另外前三次改动在他机器上一度都没生效（旧 bundle 停在 02:21，指引里一直写着 `manual:1`）——所以本轮**装了包**。
- **插入（steer）已做**（卡 `abb-agent-project-steering-20261015`，`0dce2b6` + 06:01 装机）：执行层实现 `_session/steering`（回 `injected`）并在 `initialize` 声明 `steering.supported=true`，生成中追加的消息经 rpi 的 steering 队列插进在途回合（不另起一轮）。**§5.2 已无未做项**。**思考块真机可见也已做**（卡 `abb-agent-project-thinking-20261014`，`525a8d9` + 05:53 装机）：卡面写的第 2 件「打开 `thinking_level`」实核**不成立**（包里根本没有这个开关，rpi 也不按它改请求参数；owner 的 provider 是 DeepSeek `deepseek-flash`，本来就回 `reasoning_content`）——真正丢思考的是 `event_updates` 没有 `MessageUpdate` 分支。剩 owner 真机复验。

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
| 收尾 | `abb-agent-project-thinking-20261014` / `abb-agent-project-steering-20261015` | 思考分片投影（`ThinkingDelta` → `agent_thought_chunk`，`525a8d9`）+ 插入 `_session/steering`（`0dce2b6`）+ 两次装机（见 §5.2） |

---

## 5. 卡住的 / 未完成的（含硬证据）

### 5.1 对话窗不能缩放（**已修**：卡 `abb-syschat-resize-macos-20261015`，剩 owner 真机复验）
**根因（2026-10-15 实测更正——上一版的「`winit is_resizable=false`」结论作废）**：决定项是**透明度**，不是 `is_resizable`。
1. 那个 `false` 是**误读**：来自 `with_winit_window(..).unwrap_or((false, false))` 在窗口**未 realize** 时顶的默认值。`show()` **不同步建原生窗**（`set_visible` 只把 `visible` 存进 attributes、把窗口登记为 inactive window），自查口 400ms 一档实测：`t=400ms ⇒ winit=None`，`t=800ms ⇒ Some is_resizable=true is_decorated=false`（`/private/tmp/verify-a1.log`、`verify-a2.log`）。⇒ **`is_resizable` 既不是假、也不是「能不能拖边角」的判据。**
2. **透明的无边框窗在 macOS 拿不到系统边角缩放**：同参数 A/B（最小复现件 `/private/tmp/minwin` + `/private/tmp/probe_min2.swift`：`CGEvent` 拖右缘（若干 inset）+ `CGWindowListCopyWindowInfo` 量宽）——`with_transparent(true)`：**0/2** 次；只差「不透明」的同参数窗：**2/2** 次；两者 `is_resizable` 都读到 `true`。该 A/B 由上一轮会话跑出、**结果未落盘**，本卡亦未独立复现（锁屏期点击打不进 app，见 §1 注意）。
3. ABB 两个窗都是 Slint `no-frame` ⇒ winit 后端一律 `with_transparent(true)`（`wants_transparent`，圆角靠它）⇒「交给系统缩放」在本仓**根本不存在**。于是 `360069f`(v11c) → `ecc627d` → `7b8eea5` 那条「macOS 关掉热区、交给系统」是**死路**：关掉热区 = 完全不能缩放。
4. 仍成立但**不解释拖不动**的三条背景事实：winit macOS 无边框窗样式位含 `Resizable`（`window_delegate.rs:542-544`）；`drag_resize_window` 在 macOS 返回 `NotSupported`（`:1179`）；属性钩子里的 `.with_resizable(true)` 会被 Slint 随后写入的 `attributes.resizable` 覆盖（`winitwindowadapter.rs:266`）。

**修法（已落地）**：对话窗**恒画热区**（`syschat_resize_hit_areas_enabled()` → `true`），macOS 走**自管缩放**（`syschat_resize_self_managed()` → `cfg!(target_os = "macos")` + `SelfResize`/`ResizeSession`：按屏幕坐标跟指针算尺寸，先尺寸后位置）；**设置窗同日接上同一条自管线**（`settings_resize_hit_areas_enabled()` → `true`、`settings_resize_self_managed()` → macOS 自管、自己的 `SelfResize::with_min(880, 620)`，卡 `abb-settings-window-resize-macos-20261015`，见 §5.2）；`show_window_and_focus()` 里「show 之后补 `set_resizable(true)`」整段删掉（无效时机，且 Slint 建窗时按 `min != max` 自己定 resizable）。
**守卫**：`ui::tests::resize_hit_areas_follow_platform_rule`（结构：两个窗都恒画 / 两窗各有自管接线与自己的下界 / 自管 = macOS / 设置窗钩子必须跟 `CursorMoved` + 三条收尾）+ 无头几何 `ui_headless_render_test::{self_managed_hit_zones_route_press_with_pointer_coords, settings_self_managed_hit_zones_route_press_with_pointer_coords, v11c_resize_zones_hit_all_edges_and_corners, resize_hot_zone_eats_titlebar_top_band}` + 跨文件常量 `syschat::tests::settings_window_min_constants_match_slint`。
**证据（`/private/tmp` 会被清，脚本可按 §1 重建）**：`verify-a1.log`/`verify-a2.log`（Some/None + `is_resizable`）、`verify-0120.log`/`verify-0245.log`（`[selfresize] begin dir=east` + `ret=true` + 其后数百次 `cursor_moved`）、`syschat-shots/verify-isresizable.png`（对话窗真开出来）。
**未验证（owner 项）**：真机手拖边角「窗口真的变大变小 + 手感/发不发飘」，以及自管缩放与最大化（`toggle_syschat_maximize`）的互动。合成点击**必须先解锁屏**。

### 5.2 其余未做
| 项 | 状态 | 下一步 |
|---|---|---|
| **截屏 `✂`** | **已做**（卡 `abb-v11-screenshot`，2026-10-15 落 main） | 口径＝**系统工具 + 先探权限**：`.slint` 的 ✂ → `composer-todo("截图")` → `screenshot::capture()`。采集交给 `/usr/sbin/screencapture -i -x <tmp.png>`（交互式：拖框选区 / 空格切窗口 / Esc 取消），产物落系统临时目录 → `syschat::land_attachments` 复制进工作区附件目录并进待发 → 删临时文件。**未授权不拍**：`CGPreflightScreenCaptureAccess`（只探不弹）为假就 `CGRequestScreenCaptureAccess` 触发授权，并直接回「需要权限」+ 引导文案（静默拍出来的是缺窗口的错图，比报错更难发现）。结局四态都有回话（截到 / 取消 / 缺权限 / 失败），**取消不算错误**。判据＝纯函数（参数拼装 / 结局判定 / PNG 头 / 引导文案 / 临时文件名）+ 跨文件接线守卫（Slint 功能名与 `screenshot::ACTION_NAME` 逐字一致、Rust 分支真调 `capture()`、未知功能名仍有占位回话）。**为什么用子进程而不是原生 API**：卡里把原生（手写 objc_msgSend）列首选、子进程列退路；交互式选区/选窗是子进程的内建能力，原生路径要么只能拍全屏、要么得自写一套覆盖全屏的选区 UI，TCC 归属则是同一条责任链（子进程的「责任进程」= 派它的 app）。将来若要「无交互直拍全屏/单窗」，正路才是原生 `CGWindowListCreateImage` + ImageIO。剩 **owner 真机复验**：点 ✂ 拖框截一张 → 看它进待发并发出；以及未授权时那句引导 |
| **消息动作**（可选中复制 / 复制 / 重新生成 / 朗读） | **已做**（卡 `abb-v11-message-actions`，2026-10-15 合入 main） | 口径＝**分层**：无行内样式的文本块换只读 `TextInput`（可选中 + ⌘C），助手轮整轮 `⧉ 复制` 兜底；**含行内样式的块**（表格/媒体/粗体/行内码/链接/`~~` 删除线）**仍不可选**——`TextInput` 渲染不了样式，宁可不选也不回退 B 批的样式。动作行只带**真实历史下标**、点击时才由 `syschat::action_target` 解析（**不许用「负值＝最后一条助手轮」哨兵**：第二轮收尾后模型里有两条同值哨兵，会一起解析到同一轮 ⇒ 点第一轮操作了第二轮，评审判 Critical）。剩 **owner 真机复验三项**：① 真机剪贴板端到端（⌘V 粘出）；② 真机拖选高亮 + ⌘C；③ 真机 `say` 出声与 ⏹ 停止（锁屏期合成点击打不进 app；`say` 这条单测走 `/bin/sleep` 替身，只证「隐藏路径会停进程」） |
| **历史会话归档** | **已做**（卡 `abb-v11-session-archive`，2026-10-15 落 main） | 口径＝**隐藏 + 可找回，不删数据**：`SessionMeta.archived`（`#[serde(default)]` ⇒ 旧 `sessions.json` 缺字段按未归档）；`SessionStore::list()` 过滤归档、`archived()` 只列归档；归档**当前**会话时 `ensure_current` 把 current 落到最近活动的未归档会话（一条不剩则起一条新的、沿用模型选择，**不擅自取消归档**）；`set_archived` 幂等（同值/未知 id 不落盘）。UI：第一行两义（普通「🗄 归档…」/ 归档视图「← 返回会话列表」）、第二行「🗄 已归档（N）…」；文案与「哪份列表 + 清空当前项」的判据都在 Rust 纯函数里（`session_panel_text` / `session_menu_view`）。单测 5 条（归档过滤/找回/不删文件、current 迁移、幂等不落盘、旧 JSON 兼容、两视图文案）+ ui 行判据 + 无头「第二行真的画出来」+ 出图。剩 **owner 真机复验**（三项点击路径；锁屏期合成点击打不进 app） |
| **Token 真实用量** | **已做**（卡 `abb-syschat-e3-tokens-20261013`，2026-10-15 落 main） | 口径＝**真实计数，不是估算**：`crates/abb-agent`（rpi 驱动）本来就从 provider 响应里解出 `AssistantMessage.usage`，只是**没往外报** ⇒ 现在随 `session/prompt` 结果 `_meta.usage`（camelCase `input/output/cacheRead/cacheWrite/total`）上报，根侧 `runtime::acp::parse_usage` 收下（缺字段/全零 ⇒ `None`，绝不编数），经 `pool::PromptResult::final_usage` → harness 旁路表 → `oneshot` → `syschat::TurnEnd::Done{usage}` → 写 `SessionMeta.context_tokens`（**prompt 侧** `input＋cacheRead＋cacheWrite`，不含 output）。上限：`providers[].context_window`（`#[serde(default)]`，旧 config 兼容）**覆盖**内置常见模型表（子串匹配、长键在前；匹配不到 ⇒ `None` ⇒ 印 `—`）。UI 文案/档位/提示全是纯函数（`context_usage_text` / `usage_level` / `context_usage_tip`），渲染半是无盘注入（`ui::apply_syschat_usage`）⇒ 无头台可断言任意档位。剩 **owner 真机复验**：真机跑一轮看数字随回合变化 + 设置窗「上下文窗口」填/清（`0` = 用内置表）|
| **思考块真机可见** | **已做**（卡 `abb-agent-project-thinking-20261014`，2026-10-15 落 main = `525a8d9` 并装机） | 根因＝`crates/abb-agent/src/acp.rs::event_updates` **没有 `AgentEvent::MessageUpdate` 分支** ⇒ rpi 已解好的 `ThinkingDelta` 被整段丢弃（GUI 的折叠块恒空）。修法：新增 `thinking_updates()`，**只投影 `ThinkingDelta`** 成 `agent_thought_chunk`（`content.type=text`）；`ThinkingStart/End` 无可累加文本、空 delta 是噪音 ⇒ 都不发；TextDelta 不许伪装成思考。**如实登记**：被替代组件不发这条通知 ⇒ 这是**新增行为**，不是等价替换。**卡面「打开 `thinking_level`」作废**：包里没有这个开关（`grep -rn thinking_level crates/abb-agent/src` 零命中），owner 的 provider 本来就回 reasoning ⇒ 无开关可开。上一版交接文档写「默认 `Off`」是外推误读。证据：单测 2 条 + 新探针 `probes/thinking_chunks_reach_client.py`（对 debug 与**装机 release 二进制**均绿）+ 变异 2 处（`agent_thought_chunk→agent_message_chunk` 在 `:1235` 红；`ThinkingDelta→TextDelta` 在 `:1234`/`:1269` 红且探针红）。剩 **owner 真机复验**：跟怹对话（`deepseek-flash`）应看到折叠「思考过程 · N.Ns」块 + 思考打字机；换成不回 reasoning 的模型（如 `deepseek-chat`）则不该出现 |
| **插入（steer）** | **已做**（卡 `abb-agent-project-steering-20261015`，2026-10-15 落 main = `0dce2b6` 并装机） | 根因＝执行层（`crates/abb-agent`）**从未实现** `_session/steering`：它在 `initialize` 里如实报 `steering.supported=false`，而 abb 那边这个标志是写该方法的**唯一闸门** ⇒ d10 卡交付的 GUI/发送端（`push_message` → `try_native_steer`）永远拿不到能插入的后端，生产恒走 cancel+merge 排队。修法：新增 Request 分支 + `Server::steer`——把 `{sessionId, prompt:[{type:text}]}` 转成 rpi 的 `AgentMessage::User` 入 `Agent::steer()` 队列，回 `{"outcome":"injected"}`（abb 只认 `injected`/`startedNewTurn`，空 `{}` 会被当拒收）；标志翻真。闸门：只在 `in_flight` 为真时收，无在途回合回 `-32600`（收下只会在**下一轮**冒出来 = 插话跑到下一条消息前面，比响亮回错更糟）、未知会话 `-32001`、无文本 `-32602`。证据：单测 2 条（声明==实现的双向锁 + 慢流 faux 插入，断言同回合模型被再叫一次且只有一条回合应答）+ 新探针 `probes/steering_inserts_into_in_flight_turn.py`（假网关第 1 跳吐字后卡住等握手，判据是**第 2 跳请求体里带着插入文本**且只有 1 条回合应答；反面：无在途回合回 `-32600` 且零模型请求）+ 变异 3 处（`agent.steer` 改 no-op / outcome 改 `queued` / 去掉在途闸门 各自红后还原）。剩 **owner 真机复验**：生成中发一条，看这条是**插入**（同轮回答接着变）还是排队——插入路径下收发会显示为插入 |
| **设置窗在 macOS 缩放** | **已做**（卡 `abb-settings-window-resize-macos-20261015`，2026-10-15 落 main） | 口径＝**与对话窗同一套，但两窗各一份**：`.slint` 里 `WindowResizeZones`（6px 边 / 14px 角 / 只认左键，逐条同 `PixelWindowResize` 几何）恒画，`resize-self-managed: true`（macOS）时热区按下改发 `self-resize-begin(dir, x, y)`；Rust 侧为设置窗单独建一个 `SelfResize::with_min(SETTINGS_MIN_W, SETTINGS_MIN_H)`（下界不同**不能共用**：共用会「拖这个窗动那个窗」），`CursorMoved` 跟指针 + 松手/失焦/关窗三条收尾都挂进**既有**的那个 winit 钩子（单槽 `set`，另起一个会顶掉未保存确认框）。同时修掉**图标空白**：`ImageIcon` 补上整图裁剪（组件根 `Image` 缺 `source-clip-*`，见 §6）。守卫：源码接线（含钩子里的 `CursorMoved`/收尾）+ 无头几何/路由/反向对照 + 跨文件下界常量。剩 **owner 真机复验**：真机拖设置窗边角（手感 + 与最大化/标题栏拖拽的互动）、以及设置窗与两个弹窗的图标是否出来（锁屏期合成点击打不进 app） |
| 跟进卡 | — | `abb-syschat-b-followups` / `abb-syschat-c-followups`（B/C 批评审的非阻塞项） |

---

## 6. 关键技术结论（**别再重走弯路**）

**截屏（v11 ✂）**
- **GUI 进程的 `PATH` 不含 `/usr/sbin`**（launchd 给的环境）⇒ 调 `screencapture` 必须写绝对路径 `/usr/sbin/screencapture`；写命令名就是「命令找不到」，而且**只在用户点下去时才暴露**。
- **没有「屏幕录制」授权时不要拍**：macOS 会静默把窗口内容从图里去掉（或直接拒绝），用户拿到一张错图却看不出错。先 `CGPreflightScreenCaptureAccess()`（只探不弹）⇒ 假就 `CGRequestScreenCaptureAccess()`（唯一的授权入口）+ 状态行给引导。
- 引导必须讲三件事：**哪项权限**（屏幕录制）、**去哪儿开**（系统设置 → 隐私与安全性 → 屏幕录制 → 勾 ABB）、**开完要重启 ABB**（TCC 对已运行进程不是立刻生效——「我明明勾了」多半缺的就是这句）。
- 结局判定：**没有 PNG 文件就不许说成功**。取消 = 没文件 + 没 stderr ⇒ 报「已取消」（不是失败）；文件在但不是 PNG（半张图）⇒ 失败，别让它进待发。
- 截屏产物与 📎/⌘V 走**同一条落地路径**（`syschat::land_attachments` → 工作区附件目录 → 待发列表）：另开一条会让「文件名唯一化 / 落地失败回滚 / 待发行 UI」三处各漂一份。
- **未验**：真机点 ✂ 实际截一张（本会话屏幕锁着，合成点击打不进 app，见 §1）。

**设置窗图标 / Slint 组件根 `Image` 的整图裁剪（2026-10-15 实证，卡 `abb-settings-window-resize-macos-20261015`）**
- **现象**：设置窗（以及 `TeamDialog`/`VirtualBotDialog`）里所有 `ImageIcon` 图标**画不出来**；拿无头软件渲染器渲设置窗时直接 panic（`euclid-0.22.14/src/size.rs:335` unwrap None）。
- **根因链**：`ImageIcon` 是「`Image` 当**组件根** + `source: root.icon`（动态属性）」⇒ 编译器把它降成 `ClippedImage`，却**不**合成 `source-clip-width/height` 的默认绑定（上游 `default_geometry::adjust_image_clip_rect` 只在「有绑定或属性被用到」时才补默认值，组件根的依赖分析看不到这层——上游 1.18.1 仍如此）⇒ 运行期裁剪恒 `0×0` ⇒ i-slint-core `fit()` 的 `adjust_for_tiling()` 算 `o.width * (target.width/o.width)` = `0 * inf` = **NaN** ⇒ software 渲染器在 `Rect::cast` 上 unwrap panic；femtovg/skia 按 NaN 几何**画不出**（生产默认 femtovg ⇒ 这些图标一直是空白，**静默**，不报错）。
- **纠偏**：在 `ImageIcon` 里**显式**写下编译器本该合成的那两条绑定（`source-clip-width: root.icon.width - root.source-clip-x`；判据 = 设置窗的无头渲染用例——裁剪退回 0×0 就 panic）。实测依据：探针打出 40 个 `ClippedImage` 全为 `clip=(0,0,0,0)`。
- **不适用**：直接 `Image { source: <动态> }`（如 `QrDialog` 的二维码）**不受影响**——它会降成 `ImageItem`，`source_clip()` 返回 `None`（无头实测该窗**零**个 `ClippedImage`）。⇒ 判据是「是不是组件根」，不是「source 动态不动态」。
- **教训**：这类缺陷**既不报错也不崩 GUI**（只在 software 后端才崩），只能靠「新窗口先无头渲一张」发现。

**Slint 前向引用 + 两窗两份 `SelfResize`（同上卡）**
- `.slint` **不允许前向引用元素**：`WindowResizeZones`/`ResizeZone` 必须声明在**用到它的两个窗口之前**（放到之后编译直接报 `Unknown element 'WindowResizeZones'`——实测踩过，不是猜的）。
- 两个窗**各要一份** `SelfResize`：下界不同（对话窗 `340×380` / 设置窗 `880×620`），共用一份会「拖这个窗动那个窗」；下界随窗给（`SelfResize::with_min`），跨文件常量守卫 `syschat::tests::settings_window_min_constants_match_slint`。
- 自管会话的「接线」**只能靠源码守卫**：会话要真 winit 句柄才起得来（`SelfResize::begin` → `with_winit_window`），无头台没有句柄 ⇒ 拖不出会话（同 `self_resize_pauses_and_resumes_bubble_follow` 的理由）；所以设置窗钩子里的 `CursorMoved`/三条收尾是**结构断言**钉住的。

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
T=$CARGO_TARGET_DIR/aarch64-apple-darwin/release
# stage 每次换一个带 head 短 sha 的新目录名（本机工具沙箱**拒绝 `rm -rf`**；换名字同样保证干净起步）
B=/Volumes/DataExt/tmp/ABB-stage-<head>/ABB.app; mkdir -p "$B/Contents/MacOS" "$B/Contents/Resources"
cp "$T/agent-bridge" "$T/abb-agent" "$B/Contents/MacOS/"
cp app-assets/Info.plist "$B/Contents/Info.plist"; cp app-assets/AppIcon.icns "$B/Contents/Resources/"
cp LICENSE "$B/Contents/Resources/ABB-LICENSE.txt"
for f in "$B/Contents/MacOS/agent-bridge" "$B/Contents/MacOS/abb-agent"; do
  codesign -s "agent-bridge-dev" --force --timestamp=none --entitlements app-assets/abb.entitlements "$f" >/dev/null; done
codesign -s "agent-bridge-dev" --force --timestamp=none --identifier "agent-bridge" \
  --entitlements app-assets/abb.entitlements "$B" >/dev/null
./tools/check_entitlements.sh "$B"           # #251：三项 TCC entitlements 必须在
osascript -e 'quit app "ABB"' 2>/dev/null; sleep 3
pkill -f "/Applications/ABB.app/Contents/MacOS/agent-bridge" 2>/dev/null; sleep 2
pgrep -fl "ABB.app/Contents/MacOS" || echo "(进程已清干净)"
mv /Applications/ABB.app "/Volumes/DataExt/tmp/ABB.app.prev-$(date +%Y%m%d-%H%M%S)"   # 备份 + 移走（一条命令；别省！）
ditto "$B" /Applications/ABB.app; open /Applications/ABB.app
# 装完核一遍「机器上换的确实是这次那份」：串在不在 + 主二进制 sha256 + 启动日志
strings /Applications/ABB.app/Contents/MacOS/abb-agent | grep -c agent_thought_chunk
grep -a "harness 装配" ~/.agent-bridge/logs/service.out | tail -1
```
> ⚠️ `app-assets/abb.entitlements` **必须带上**（相机/麦克风/自动化），否则 hardened runtime 下静默拒绝。

### 7.3 接手第一小时建议
1. 读 §1，自己开窗抓一张图，建立「我能自己验收」的闭环（这一步能省掉后面所有「让用户截图」的往返）。
2. §5.1 缩放**已修**（卡 `abb-syschat-resize-macos-20261015`：对话窗恒画热区 + macOS 自管缩放；设置窗同日接上同一套，见 §5.2），剩**真机手感**复验；**别再拿 `is_resizable` 取证**（它既不假也不是判据，见 §5.1）。
3. §5.2 的清单**已全部落地**（缩放 ✅ / 设置窗缩放 ✅ / 消息动作 ✅ / 归档 ✅ / 用量(E3) ✅ / 截屏 ✅ / 思考块 ✅ / 插入(steer) ✅）；剩下的活是**owner 真机复验**（§5.2 每行末尾写了各自要看什么）与两张跟进卡（`abb-syschat-b-followups` / `abb-syschat-c-followups`）。
4. **动 `.slint` 里任何窗口/图标之后，先让无头台渲一张**（设置窗那批图标就是「一直画不出来但谁也不报错」，见 §6）；`.slint` 还有两条硬规矩：组件必须声明在使用点**之前**（前向引用直接编译失败）、同一个窗口**只能有一个** `on_winit_window_event` 钩子（单槽 `set`，另起一个会静默顶掉前一个）。
5. 每做完 2–3 条，**批量**跑四门禁 + 独立评审 + 打包安装一次（owner 已同意这条降档口径）。
   **装包这一半别漏**：2026-10-15 的教训是「代码改了、文案改了，但机器上的 bundle 还停在 3 小时前」——owner 读到的
   工作区指引因此一直写着旧数字（`manual:1`），改动在他那里从未生效。改造完用户**看得见的那份载体**（bundle 里的
   工作区指引 / GUI）之后，**要装包**（§7.2），并核对 `strings /Applications/ABB.app/Contents/MacOS/agent-bridge | grep abb-guide-v`
   与启动日志，而不是只看仓库里的 `cargo build`。本轮装包记录见 §0 顶部。
