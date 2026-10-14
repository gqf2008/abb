# 系统级对话（syschat）交接文档

> 面向**下一个接手的人**（或新会话）。目标：读完这一份就能继续干，不用重新踩坑。
> 写于 `main = 7b8eea5`（2026-10-14 深夜）。**事实以仓库代码/卡为准，本文只是索引与踩坑记录。**

---

## 0. 30 秒现状

- 功能主体**已经可用**：气泡（常驻置顶）+ 对话框（标题栏一行 / 状态栏两行 / 输入区功能区）+ 流式打字机 + 附件/拖拽 + Markdown（含 HTML 媒体）+ 定时发送 + 插入/打断（排队）。
- **最痛的一条没修完**：**对话窗不能缩放**。根因已用自查口证明是 `winit is_resizable=false`（见 §5.1），修法已写进代码但**未验证**（见 §5.2）。
- 还没做：**截屏**、**消息动作（复制/重新生成/朗读）**、**历史会话归档**、**Token 真实用量**、**思考块真机可见**（卡在 `abb-agent`）、**设置窗缩放**。

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
grep -a '\[verify\]\|\[resize\]' /tmp/verify.log          # 窗口底层事实（is_resizable 等）
swift /tmp/winid.swift | awk -F'\t' '$2=="ABB"'            # 列出 ABB 窗口（拿 window id）
screencapture -x -o -l<id> /tmp/shot.png                   # 按 id 抓图（不需要辅助功能权限）
```

- `ABB_VERIFY_OPEN=1`：**直接打开对话窗**并打印窗口事实。**必须配隔离 HOME**（否则会触发 `platform::heal_autostart()` 改写用户真实 `~/Library/LaunchAgents/`）。
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

### 5.1 对话窗不能缩放（**优先修这条**）
**已证实的根因链**（都自己跑出来过，不是猜）：
1. `ABB_VERIFY_OPEN=1` 打印：`winit is_resizable=false is_decorated=false size=0x0`
   ⇒ 窗口在**窗口管理器层面就是不可缩放的**（拖边角永远没反应）。这解释「设置窗能缩、对话窗不能」——两者唯一的差别就在这一位。
2. 建窗时 `with_winit_window` 返 `None`、尺寸 `0x0` ⇒ **Slint 是首次 `show()` 才建原生窗**，所以「建窗后立刻 `set_resizable(true)`」是**无效时机**。
3. 属性钩子里的 `.with_resizable(true)` **会被 Slint 随后写入的 `attributes.resizable` 覆盖**（Slint `winitwindowadapter.rs:266`）。
4. 相关的三条**背景事实**：winit macOS 无边框窗样式位含 `Resizable`（`window_delegate.rs:542-544`）；`drag_resize_window` 在 macOS **返回 `NotSupported`**（`:1179`）；**自绘缩放热区会抢掉系统自带的无边框窗边角缩放**（所以 macOS 上**不要画**热区——设置窗就是这么做的，这也是「设置窗能缩」的原因）。

**已写但未验证的修法**：把 `set_resizable(true)` 放到 `show_window_and_focus()` 的 `show()` **之后**（`main` 里已有此代码）。
**未验证的原因**：隔离 HOME 下自查口跑时 `[resize]` 日志没出现、窗口也没真正出现（`ABB_VERIFY_OPEN` 分支现在的顺序：打印事实 → 设尺寸 → show；疑似 show 路径没走到，或窗口创建时机仍在更晚）。
**下一步（建议）**：
- 用 §1 的流程在**真机**跑一次，看 `[resize]` 是否出现、窗口是否出来、拖边角是否可缩；
- 若 `with_winit_window` 在 `show()` 后仍是 `None`，就在**首个 UI tick 后**再置一次（或挂 `WindowEvent::Resized/Moved` 首次到达时置一次）；
- macOS 上**保持热区关闭**（`resize_hit_areas_enabled() = !macos`），让它走系统缩放；**最大化**因为没有系统绿点，仍需自管（`toggle_syschat_maximize`）。

### 5.2 其余未做
| 项 | 状态 | 下一步 |
|---|---|---|
| **截屏 `✂`** | 只有占位（按下写「尚未实现」） | 卡 `abb-v11-screenshot`；做原生截屏需 **macOS 屏幕录制 TCC 权限**，要先探权限 + 被拒给可读引导；`screencapture` 子进程是可行退路 |
| **消息动作**（可选中复制 / 复制 / 重新生成 / 朗读） | 未做 | 卡 `abb-v11-message-actions`；**Slint 的 `Text` 默认不可选中**，卡里要求先定方案（只读 `TextInput` / 自绘选择） |
| **历史会话归档** | 未做 | 卡 `abb-v11-session-archive`；`SessionMeta` 加 `archived` 要**向后兼容**旧 `sessions.json`；**不删数据** |
| **Token 真实用量** | 状态栏进度条**结构已就位**，值是占位 | 卡 `abb-syschat-e3-tokens-20261013`；**`acp.rs` 没有任何 usage 来源**（goose 注释明说不报）⇒ 首选 ACP 响应里的真实 usage，退路 `chars/4` **并标注为估算**；`providers[].model` 无 `context_window` ⇒ 内置常见模型表 + 设置可覆盖 |
| **思考块真机可见** | GUI 全就绪（折叠块 + 耗时 + 打字机），**执行层不发** | 卡 `abb-agent-project-thinking-20261014`：`crates/abb-agent` 的 `event_updates` 未映射 rpi 的 `ThinkingDelta`，且 `thinking_level` 默认 `Off`（打开会改模型请求参数，建议默认关 + 设置项） |
| **插入（steer）** | GUI + 发送端已就绪；**生产恒走排队** | `abb-agent` 如实上报 `steering.supported=false`（`_session/steering` 回 `-32601`）⇒ 要真插入得换声明支持该扩展的适配器，或改 `abb-agent` |
| **设置窗在 macOS 缩放** | 未做 | 与 §5.1 同因（`is_resizable`）；同一修法可一并覆盖 |
| 跟进卡 | — | `abb-syschat-b-followups` / `abb-syschat-c-followups`（B/C 批评审的非阻塞项） |

---

## 6. 关键技术结论（**别再重走弯路**）

**Slint / slint-pixel**
- `@markdown("{x}")` **不是插值模板**（要以 `\{` 起），且即便插值也只按**纯文本**注入 ⇒ 行内样式必须在 **Rust 侧** 用 `StyledText::from_markdown`（B 批评审 B1/B2；`src/mdblocks.rs::styled`）。
- Slint **没有 `audio`/`video` 元素** ⇒ 音视频走**媒体卡 + 系统播放器**。
- **`PixelTitleBar` 是上游封闭组件**：宿主必须自己转发 `minimize` / `toggle-maximize` / `drag-start`（只写 `close-window` 会导致三钮点了没反应——D6 根因）。
- `Tooltip.text` 是 styled-text：`Tooltip { text: @markdown("\{root.tip}") }`。
- 在布局容器里写 `x:`/`width: parent.width + Npx` 容易触发 **width ←→ layoutinfo 绑定循环**（编译器警告「可能运行时 panic」）⇒ 分割线两端那 2px 就是窗口黑描边，别去盖。
- 消息区每个会换行的文本**必须带 px 宽绑定**（`convo-text-width`，由 Rust 写），否则估高偏小 ⇒ 叠字/滞底算不到底。
- 无头出图台里的字体可能缺 emoji/符号字形（真机用系统字体），**布局可信、字形别下结论**。

**macOS 窗口**
- `no-frame: true` + winit macOS：样式位 `Borderless | Resizable | Miniaturizable`；**系统边角缩放本来就有**，前提是 (a) `resizable` 为真、(b) **没有自绘热区去抢**。
- `drag_resize_window` 在 macOS **`NotSupported`**（所以「自绘热区 + 它」那条路在 macOS 是死的）。
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
2. 修 §5.1 缩放（先取证再改，别猜）。
3. 按 §5.2 从上往下做（建议顺序：缩放 → 消息动作 → 归档 → 用量(E3) → 截屏；思考/steer 要动 `abb-agent`，可最后做）。
4. 每做完 2–3 条，**批量**跑四门禁 + 独立评审 + 打包安装一次（owner 已同意这条降档口径）。
