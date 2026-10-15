# 系统级对话（syschat）交接文档

> 面向**下一个接手的人**（或新会话）。目标：读完这一份就能继续干，不用重新踩坑。
> 首版写于 `main = 7b8eea5`（2026-10-14 深夜）；**2026-10-15 随卡 `abb-syschat-resize-macos-20261015` 更正 §0/§1/§5.1/§5.2/§6**（`main = d5ddd9e`）；**同日随卡 `abb-v11-message-actions` 更新 §0/§5.2/§7.3**（消息动作从「未做」移到「已做」，评审两条 blocking 的修法见 §5.2）；**同日随卡 `abb-v11-screenshot` 更新 §0/§5.2/§6/§7.3**（✂ 截屏落地）；**同日随卡 `abb-syschat-e3-tokens-20261013` 更新 §0/§5.2/§6/§7.3**（Token 真实用量落地：ACP `_meta.usage` 是真来源，不是估算——上一版「`acp.rs` 没有 usage 来源」的结论是**错的**，见 §6）；**同日随卡 `abb-settings-window-resize-macos-20261015` 更新 §0/§5.1/§5.2/§6/§7.3**（设置窗在 macOS 也能缩放；顺带修掉「设置窗/弹窗里的 `ImageIcon` 图标**画不出来**」——组件根 `Image` 缺整图裁剪，见 §6）。**2026-10-15 05:44 批量安装**（owner 同意的口径：每做完 2–3 条，批量跑门禁 + 打包安装一次）：`/Applications/ABB.app` 已换成 `main = 2571240` 的 release 构建（marker `abb-guide-v20`），旧 bundle 备份在 `/Volumes/DataExt/tmp/ABB.app.prev-20261015-054438`；四条门禁在**整棵树**上跑过（fmt / clippy 对基线零新增 / `build --locked` / 隔离全量 lib 2 + bin 1272 + 集成 19），**没有独立评审**（本 worktree 只有一个身份，且 owner 已把仪式降档）——照实记在这里，别当成「审过了」。**同日 05:53 再装一次**（思考分片投影落 `main = 525a8d9`）：`/Applications/ABB.app` 换成该 head 的 release 构建（主二进制 `Contents/MacOS/abb-agent` 从此含 `agent_thought_chunk`；旧 bundle 备份 `/Volumes/DataExt/tmp/ABB.app.prev-20261015-055344`），启动日志 harness 装配走新 `abb-agent`、bot 庆小丰 在线、`manual=按需派发（并发闸门：不节流）`。**同日 06:01 第三次装机**（插入 steer 落 `main = 0dce2b6`）：`/Applications/ABB.app` 又换一次 release 构建（主二进制 `abb-agent` 从此含 `_session/steering`；旧 bundle 备份 `/Volumes/DataExt/tmp/ABB.app.prev-20261015-060642`），启动日志 harness 走新 `abb-agent`、bot 庆小丰 在线。⇒ **§5.2 的功能清单至此全部落地**（剩下都是 owner 真机复验 + 两张跟进卡）。**同日再一批（2026-10-15）**：两张跟进卡的非阻塞项逐条处置（表内 HTML 不再转义、`styled` 只对行内 markdown 块算、⌘V 拦截不看 `busy`、拖放去重补真判据、`~` 口径与 winit 拖放边界如实记录），**已落 main（1d26dcf）并重装 bundle**（表内文本与 📎 tooltip 是用户可见改动）：06:24 装机，/Applications/ABB.app = 1d26dcf 的 release 构建（装机核过新 tooltip 串在二进制里），旧 bundle 备份 /Volumes/DataExt/tmp/ABB.app.prev-20261015-062423，启动日志 harness 装配走新 abb-agent、bot 庆小丰 在线；四条门禁全绿 + 8 处变异自验；**三处 owner 待拍**（`~a~` 是否 GFM 严格 / 关窗是否清待发附件 / 多文件拖入的状态行计数）见 §5.2 与 §6。**同日深夜再一轮（main = ea0429a）**：①角色块「是不是助手」改成显式字段 `role-assistant`（不再按角色名文案判，见 §6），**无用户可见变化 ⇒ 未重装 bundle**；②卡 `abb-syschat-e2-thought-20261013`（E2 思考/正文分离）**审计后 closed**：六条验收 4 ✅ + 2 ◐（块序列不保留「思考→正文→思考」交替、标题文案与稿不同，见 §6）；③卡 `abb-gates-sh-locale-unbound-var-20261012`（`tools/gates.sh` 在 UTF-8 locale 下 `set -u` 失败）**实核为已修（f88a606）后 closed**，并补了机制阳性对照（UTF-8 下 bash 3.2.57 报 unbound variable / exit 127；`LC_ALL=C` 反而 exit 0 —— 按 C locale 复核会漏）。**同日第 3 轮（文档自审，main = 4a91f81 + 本次）**：逐条核了本文里的提交号（30 个全在）、`file:line` 引用、被点名的文件（15 个全在）与 41 个下划线标识符；查出并修掉两类**文档自身的坑**：①`src/ui.rs:8126` 这类**行号引用会随提交漂移**（该处真实行号已到 `:8139`，`:8134@2dfef7a`）⇒ §2 新增「引用用字面量/符号，不用行号」一条；②「`verify-isresizable.png` 已失效」是**查错路径**（截图在数据卷 `/Volumes/DataExt/tmp/syschat-shots/`，`/private/tmp` 只放探针日志）⇒ §5.1 逐字纠偏。**同日第 4 轮（§7.2 装机配方实测 + 两条会骗人的核对手法）**：按 §7.2 实跑「release 构建 → 组 bundle → 签名 → `tools/check_entitlements.sh`」全绿，暂存件 `/Volumes/DataExt/tmp/ABB-stage-5978f34/ABB.app` 就绪（只剩 quit/`mv`/`ditto` 三段）；顺带实测两条**会给出假绿/假红**的核对手法：①同一份源码重编**不是逐字节可复现**（`crates/abb-agent` 零 diff，06:24 那份 `7f1832…` vs 本轮 `938518…`）⇒ 装机后核 sha256 要跟**暂存件**比；②UTF-8 locale 下裸 `grep -a` 查二进制里的**中文串**静默返回 0（`LC_ALL=C` 才是 1）。**同日第 5 轮（用户可见修复：多文件拖入的状态行计数，C2-B）**：`DroppedFile` 逐文件到达 ⇒ 攒 80ms 短批整批落地，状态行报这一批的真实个数（与 📎 一致），不再恒写「1 个」。门禁四道全绿（lib 2 + bin 1281 + 8 ignored + 集成 19）；变异 2 处红后还原。**用户可见 ⇒ 已重装 bundle**：13:28 装机，/Applications/ABB.app = 0057811 的 release 构建（装机后 sha256 与暂存件逐字节一致：abb-agent 4679a071…、agent-bridge 33fa93c5…），旧 bundle 备份 /Volumes/DataExt/tmp/ABB.app.prev-20261015-132837；启动日志 harness 装配走新 abb-agent、gui + --service 两进程都在。**事实以仓库代码/卡为准，本文只是索引与踩坑记录。**

---

## 0. 30 秒现状

- 功能主体**已经可用**：气泡（常驻置顶）+ 对话框（标题栏一行 / 状态栏两行 / 输入区功能区）+ 流式打字机 + 附件/拖拽 + Markdown（含 HTML 媒体）+ 定时发送 + 插入/打断（排队）+ **消息动作**（纯文本块可选中复制；助手轮 ⧉ 复制 / ⟳ 重新生成 / 🔊 朗读）。
- **对话窗缩放已修**（卡 `abb-syschat-resize-macos-20261015`）：根因**更正为「透明的无边框窗在 macOS 没有系统边角缩放」**（上一版写的 `winit is_resizable=false` 是「未 realize」的误读，见 §5.1）⇒ 对话窗**恒画热区 + macOS 自管缩放**。剩**owner 真机复验**（手拖边角的手感；锁屏时合成点击打不进 app）。
- **历史会话归档已做**（卡 `abb-v11-session-archive`）：会话菜单「🗄 归档…」归档当前会话（从列表隐藏）、「🗄 已归档（N）…」进归档视图（点一条 = 取消归档并切回）；**不删任何历史文件**。剩 **owner 真机复验**（点开面板 → 归档 → 已归档 → 找回；锁屏时合成点击打不进 app）。
- **Token 真实用量已做**（卡 `abb-syschat-e3-tokens-20261013`）：状态栏「已用/上限 · 百分比」+ 进度条（≥70% 橙 / ≥90% 红），悬停给精确 token 数**和口径**；值是**真实计数**（执行层从 provider 响应上报的 prompt tokens），不是 `chars/4` 估算。上限来源：设置里 `context_window` 覆盖 > 内置常见模型表 > `—`（不知道就不印数）。
- **截屏已做**（卡 `abb-v11-screenshot`）：功能区 ✂ → 选区/选窗（`screencapture -i`）→ 落进待发附件（与 📎/⌘V 同一条落地路径）。缺「屏幕录制」授权时**先探权限、不拍**，状态行给三要素引导（哪项权限 / 去哪儿开 / 开完要重启）。
- **设置窗在 macOS 也能缩放**（卡 `abb-settings-window-resize-macos-20261015`）：与对话窗同一套口径（恒画热区 + macOS 自管缩放，两个窗**各一份** `SelfResize`——下界不同）。顺带修掉一个**一直存在但没人报**的缺陷：设置窗与两个弹窗里的 `ImageIcon` 图标**从来没画出来过**（组件根 `Image` 缺「整图裁剪」⇒ `fit()` 出 NaN，见 §6）；无头台现在是**第一条能渲染设置窗**的用例。剩 **owner 真机复验**（拖设置窗边角 + 看设置窗/弹窗图标是否出来）。
- **手动档 task 执行已改按需派发**（卡 `abb-task-workers-manual-not-a-pool-20261015`，`4cb33d0` + `2571240`）：`task_workers.manual` 默认 **0 = 不节流**（写 N 才是「同时最多 N 条」），手动档只起 1 条 poller、认领到就起执行体；工作区指引那句「并发额度有限……它不是不限量的池子」已删。**注意**：这个面被 owner 反复说过四次，前三次都只在改数字（1 → 4 → 128），**根因是它当时是「固定池」**；另外前三次改动在他机器上一度都没生效（旧 bundle 停在 02:21，指引里一直写着 `manual:1`）——所以本轮**装了包**。
- **角色块身份判据已收口**（卡 `abb-wording-role-label-kind-20261013`，`ea0429a`）：`.slint` 原以 `b.text == "怹"` 判「这一行是不是助手」，角色名一改就恒假、着色静默失效（「ABB → 怹」那次就漏改过消费侧）。现在身份是 `SysChatBlockRow.role-assistant` 字段，由 `ui::role_row(label, assistant)` 给；`ui/app.slint` 里 `b.text ==` 零命中（有跨文件守卫钉住）。渲染输出逐字不变 ⇒ **未重装 bundle**。
- **插入（steer）已做**（卡 `abb-agent-project-steering-20261015`，`0dce2b6` + 06:01 装机）：执行层实现 `_session/steering`（回 `injected`）并在 `initialize` 声明 `steering.supported=true`，生成中追加的消息经 rpi 的 steering 队列插进在途回合（不另起一轮）。**§5.2 已无未做项**。**思考块真机可见也已做**（卡 `abb-agent-project-thinking-20261014`，`525a8d9` + 05:53 装机）：卡面写的第 2 件「打开 `thinking_level`」实核**不成立**（包里根本没有这个开关，rpi 也不按它改请求参数；owner 的 provider 是 DeepSeek `deepseek-flash`，本来就回 `reasoning_content`）——真正丢思考的是 `event_updates` 没有 `MessageUpdate` 分支。剩 owner 真机复验。
- **渲染修复（2026-10-15，owner 反馈历史会话里看到的）**：①**表格**整块化（列宽跟内容、整表一个横向 Flickable——旧的「左列被裁 + 列间距巨大 + 每行独立横滑」已修）；②**图片**块高度进布局（不再盖住后续文字）；③**数学**块级与**行内**公式都**真排版**（块级走 `ratex-*`；行内走 `parley` inline box 混排，等价取舍见 §6）；④**空会话不再被归档**（旧版会把归档列表塞满空壳）。见 §5.2 / §6。

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
**当前 head 的实测读数**（06:47，debug，**锁屏下也能跑**；留档 `/private/tmp/verify-head-0646.log` 7 行＝1 条 `[single-instance] 拿到 gui 锁` + 6 条 `[verify]`）：
`pre-show` 与 `t=400ms` 都是 `winit=None`（后者已 `slint_size=520×560` ⇒ Slint 侧建好了、原生窗还没 realize）⇒
`t=800ms…2000ms` 稳定为 `winit=Some is_resizable=true is_decorated=false inner=1040×1120 hit_areas=true self_managed=true`；
同期 `CGWindowListCopyWindowInfo` 报 `(475,124,520,560)`（逻辑点 = inner 的一半，2× 缩放）
⇒ **「窗口建起来了、热区开着」这条不需要解锁**；要解锁的只有「拖它 / 点它」。

- `ABB_VERIFY_OPEN=1`：**直接打开对话窗**并打印窗口事实。**必须配隔离 HOME**（否则会触发 `platform::heal_autostart()` 改写用户真实 `~/Library/LaunchAgents/`）。
  探针按 **400ms 一档打 5 拍**，并**显式区分** `winit=None`（还没 realize）与 `Some{is_resizable,…}`：`t=400ms` 常是 `None`（`show()` 不同步建窗），`t=800ms` 才有原生窗事实——**别把 `None` 读成 `is_resizable=false`**（§5.1 的坑）。
- ⚠️ **锁屏时合成点击打不进 app**（拖拽/缩放/点按钮类验证必须先解锁）。查锁屏态：`swift /Volumes/DataExt/tmp/abb-verify/session.swift`。
  **2026-10-15 实核（当前 head，两个负向读数）**：① 全局 `CGEvent`（`cghidEventTap`）与
  ② **直达进程的 `CGEventPostToPid(pid,…)`** 都无效——两者各发一次「移到右缘 → 按下 → 拖 10 拍 → 松手」后，
  探针日志**行数不变**（无 `[selfresize] begin`）、`CGWindowListCopyWindowInfo` 复读的矩形**一像素没动**（`X=475 Y=124 W=520 H=560`）。
  ⇒ 「打不进 app」不是某一种投递方式的锅，**别指望换 API 绕过**。
  ③ **更快的判定（先跑这个，别先做实验）**：AX 命中测试把**任意**坐标都解析成登录窗——
  `AXUIElementCopyElementAtPosition(syswide, x, y)` ⇒ `pid=173 role=AXWindow title=登录`（锁屏期）。
  即锁屏期**命中面整体**属于 loginwindow，合成输入不可能落到应用上；这条判据本身就是「现在能不能做界面自动化」的答案，
  不依赖任何应用日志（本机 `AXIsProcessTrusted=true`，所以**不是**辅助功能权限造成的假阴性）。
- ⚠️ **解锁 ≠ 能合成输入（2026-10-15 13:1x 实测，第三因子：辅助功能授权）**：屏幕已解锁
  （`IOConsoleLocked=false`）后，探针窗也真开出来了（`onscreen=true`、520×560 @ (475,124)、`hit_areas=true`），
  但三种合成输入**全部静默失效**：
  ① `AXIsProcessTrusted()=false`（本会话的宿主是 codex app-server daemon，不是带 TCC 授权的终端 app）；
  ② `CGEvent.post(.cghidEventTap)` 发 `mouseMoved` 到 (10,10)/(1400,900)/(300,400) 三个点，80ms 后读
  `NSEvent.mouseLocation()` **纹丝不动**（无报错、无位移 ⇒ 被丢，不是坐标系问题）；
  ③ `osascript → System Events` 读窗口/`click at` 报 **-25211「osascript 不允许辅助访问」**，`click at` 只回 `missing value`。
  ⇒ 「打不进 app」现在要分**三个**因子排查：会话锁屏 / 窗口不在最上层 / 宿主无辅助功能权限——三者现象一样（日志零条），
  但解法完全不同。前两个已实测复现，第三个就是本条。
  另：`tell System Events to set frontmost of process … to true` **能**用（AppleEvents，不走 AX）——它会把 app 拉到前台，
  但**不代表**后续 click 也能用；别拿「frontmost 变了」当「能点」的证据。
- ⚠️ **点击前必须查 z-order（同一进程里也有遮挡）**：本机 ToDesk 全屏盖着、Otty 又在其上；ABB 激活后，
  探针自己的**设置窗**（980×680 @ (245,94)）把对话窗标题栏的 ⤢ 坐标也盖住了（`attop` 实测最上层是它）。
  所以「点按钮」前先跑一遍 attop 判最上层，**不是我们的目标窗就不要点**——否则一次误点会落进 ToDesk（远控），
  风险不可控。
- ⚠️ **同一份隔离 HOME 只能跑一个探针实例**：单实例是 `flock`，锁文件 = `$HOME/.agent-bridge/.gui.lock`。
  上一轮留下的探针还在跑时，新实例只打一行
  `[single-instance] 已有一个 agent-bridge「gui」实例在运行（flock 拿不到锁…）。本实例退出。` 然后 **exit 0**
  （不报错、不开窗）——看着像「探针坏了」。先收干净：`ps -axo pid,command | grep 'abb-target-.*/debug/agent-bridge'` → `kill <pid>`（或换一个 HOME）；
  跑完**自己收掉**，别留给下一个人踩。
- **解锁后怎么验「手脚」（缩放/最大化/选中复制/say）**——三件都要真窗口：
  ① **先让窗口被点过**（人手点一次，把 app 带到前台）：这是**经验做法**，不是已分离的结论——
  本轮实测到「0 条 `begin`」时**两个因子同时在**（会话锁屏 **且** app 没被点过），两者**没有**单独分离过
  （锁屏那一侧现在有 AX 命中测试作硬判据；「解锁但未激活」这一侧待补测）。无论哪一因子，现象都一样：
  日志里 `[selfresize] begin` 与 `cursor_moved` **双双 0 条**——**别把这读成「缩放没接线」**；
  ② 已知窗口矩形（`swift /tmp/listwin2.swift` 列 `(x,y,w,h)`）后发一次合成拖拽：
  本机现成脚本：`swift /private/tmp/drag_once.swift 992 127 -140`（参数 x y dx，10 拍、每拍 30ms），
  随后 `grep -a '\[selfresize\] begin' /tmp/verify.log` **应出现一条** `begin dir=<east|north-east|…> at (x,y)`（日志点在 `src/ui.rs` 的 `SelfResize` 入口；
  **注意**：这条字符串在当前 head 的 release 二进制里也在，但 GUI 进程的 stdout 不落 `~/.agent-bridge/logs/gui.out`（那份停在 10-12）⇒ 要留证就用 §1 的 debug 探针 + `> /tmp/verify.log`）；
  ③ 再用 `listwin2.swift` 复读一次矩形：**w/h 应变大**（这才叫「端到端生效」；只有单测几何=没验到手），
  最大化同理（点三钮的 ⤢ 后工作区尺寸应等于 AppKit `visibleFrame`）。
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
5. **引用仓内代码用「字面量 / 符号」，不要用行号**：本仓改动频繁，`file:line` 一两个提交就漂
   （§5.1 里那句 `[selfresize] begin …` 就漂过两次：`:8126` → `:8134` → `:8139`）。写文档/注释时给
   **引号里的串**或函数名（`grep -n '<串>' <file>` 一次定位）；确实要写行号时必须**同时**写
   「写于哪个 commit」，否则下一个读者会照着错行号去读别的代码。上游 crate（winit / i-slint-*）
   的行号可留：它们按版本锁定，不随本仓漂。
6. **walgit 协作**：卡 = `walgit collab entry`；`--parent` **必须是上一条条目的真实 oid**（拿 issue oid 当 parent 会造分叉）。
7. **提交信息不要用反引号**（bash 会当命令替换执行）⇒ 用 `git commit -F <file>`。
8. **owner 已把单条开发的门禁+评审降档**：单条只跑 `cargo build` + 定向隔离测试；**批量交付/上线前**再统一跑四门禁（fmt / clippy `-D warnings` / `build --locked` / 隔离门禁）与独立评审。
9. **本机 clippy 基线本已红**（rustc/clippy 1.99）：只保证**零新增 lint**（与 base 比对 lint 多重集）。
10. **发版/安装**（`scripts/build.sh` 硬编码 `target/`，会落到启动卷 ⇒ 用手动组装）：
   见 §7 附录的安装命令（release 构建 → 组 bundle → `agent-bridge-dev` 签名 + `app-assets/abb.entitlements` → `tools/check_entitlements.sh` → 装 `/Applications/ABB.app`）。

---

10. **流程口径（owner 2026-10-15 明确）**：**发版前不做独立评审**；单条开发只要
    `cargo build` + 定向隔离测试即可。**发版（release）那一次**再统一做：全量四门禁
    （fmt / clippy `-D warnings` / `build --locked` / 隔离门禁）+ 独立评审 + 真机验收。
    ⇒ 平时别为了「评审」停下手上的活，也别在单条上跑全量门禁。

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
| 收尾2 | `abb-wording-role-label-kind-20261013` / `abb-syschat-e2-thought-20261013`（审计）/ `abb-gates-sh-locale-unbound-var-20261012`（实核已修） | 角色身份字段化（`ea0429a`）；E2 六条验收逐条审计；gates.sh locale 补证据（均只见卡片与文档） |
| 收尾 | `abb-agent-project-thinking-20261014` / `abb-agent-project-steering-20261015` | 思考分片投影（`ThinkingDelta` → `agent_thought_chunk`，`525a8d9`）+ 插入 `_session/steering`（`0dce2b6`）+ 两次装机（见 §5.2） |

---

## 5. 卡住的 / 未完成的（含硬证据）

### 5.1 ✅ 已修（2026-10-15 17:27）——根因：**布局把窗口最大尺寸钉成了 520×560**

**一句话**：`SystemChatWindow` 的子树里有一批**固定 px 宽**的元素（`convo-text-width` 一系、分割条、
功能区…），Slint 由它们推出根布局的最大尺寸 ≈ `preferred`（520×560）⇒ 窗口的 `contentMaxSize`
被钉在 520×560：**只能缩小、不能放大**。于是任何放大请求（⤢ 到工作区 1470×923、向外拖边角、
`set_size(700×700)`）都被 AppKit 夹回原尺寸——表现为「⤢ 无效」「向外拖不动」，而**缩小是好的**。

**决定性实验（两次，都不需要人工点击）**：
```
ABB_VERIFY_SET_SIZE=1 ABB_VERIFY_SIZE=400x400   → after = 400×400    ✅ 落地（比 520×560 小）
ABB_VERIFY_SET_SIZE=1 ABB_VERIFY_SIZE=1470x923  → after = 520×560    ❌ 被夹（比它大）
# 加 max-width/max-height: 100000px 之后：
ABB_VERIFY_SET_SIZE=1 ABB_VERIFY_SIZE=1470x923  → after = 1470×923   ✅ 落地
ABB_VERIFY_TOGGLE_MAX=1                         → before 475,124 520×560 → after 0,33 1470×923 ✅
```
**修法**：组件级加 `max-width: 100000px; max-height: 100000px;`（比任何屏幕都大 ⇒ 等价于「无上界」）。

**⚠️ 本节以下的历史内容全部作废**（保留作踩坑记录）：`is_resizable=false`、透明窗拿不到系统缩放、
`setFrame` 被 Slint 覆盖、坐标口径不一致、气泡跟随/失焦隐藏是元凶……**这些结论都是错的**，
真因只有上面那一条。**方法教训**：写窗口几何之前，先问「**这个值是不是被约束夹住了**」
（用「一个更小的值能否落地」一刀就能分辨 no-op 与 clamp）——比读 macOS/winit/Slint 源码快得多。

### 5.1 对话窗不能缩放（**已修**：卡 `abb-syschat-resize-macos-20261015`，剩 owner 真机复验）
**根因（2026-10-15 实测更正——上一版的「`winit is_resizable=false`」结论作废）**：决定项是**透明度**，不是 `is_resizable`。
1. 那个 `false` 是**误读**：来自 `with_winit_window(..).unwrap_or((false, false))` 在窗口**未 realize** 时顶的默认值。`show()` **不同步建原生窗**（`set_visible` 只把 `visible` 存进 attributes、把窗口登记为 inactive window）。**可复算证据**（当前 head 的 debug 构建，2026-10-15 06:26 与 06:27 各跑一次）：`pre-show` 恒 `winit=None`；`t=400ms` 一档**不稳定**（一次 `None`、一次已 `Some`——上一版把它写成定论，属**过度具体**，别再照抄）；`t=800ms` 起恒 `Some is_resizable=true is_decorated=false inner=1040×1120 hit_areas=true self_managed=true`。⇒ **`is_resizable` 既不是假、也不是「能不能拖边角」的判据**（它恒为真）。
2. **透明的无边框窗在 macOS 拿不到系统边角缩放**：同参数 A/B（最小复现件 `/private/tmp/minwin` + `/private/tmp/probe_min2.swift`：`CGEvent` 拖右缘（若干 inset）+ `CGWindowListCopyWindowInfo` 量宽）——`with_transparent(true)`：**0/2** 次；只差「不透明」的同参数窗：**2/2** 次；两者 `is_resizable` 都读到 `true`。该 A/B 由上一轮会话跑出、**结果未落盘**，本卡亦未独立复现（锁屏期点击打不进 app，见 §1 注意）。
3. ABB 两个窗都是 Slint `no-frame` ⇒ winit 后端一律 `with_transparent(true)`（`wants_transparent`，圆角靠它）⇒「交给系统缩放」在本仓**根本不存在**。于是 `360069f`(v11c) → `ecc627d` → `7b8eea5` 那条「macOS 关掉热区、交给系统」是**死路**：关掉热区 = 完全不能缩放。
4. 仍成立但**不解释拖不动**的三条背景事实：winit macOS 无边框窗样式位含 `Resizable`（`window_delegate.rs:542-544`）；`drag_resize_window` 在 macOS 返回 `NotSupported`（`:1179`）；属性钩子里的 `.with_resizable(true)` 会被 Slint 随后写入的 `attributes.resizable` 覆盖（`winitwindowadapter.rs:266`）。

**修法（已落地）**：对话窗**恒画热区**（`syschat_resize_hit_areas_enabled()` → `true`），macOS 走**自管缩放**（`syschat_resize_self_managed()` → `cfg!(target_os = "macos")` + `SelfResize`/`ResizeSession`：按屏幕坐标跟指针算尺寸，先尺寸后位置）；**设置窗同日接上同一条自管线**（`settings_resize_hit_areas_enabled()` → `true`、`settings_resize_self_managed()` → macOS 自管、自己的 `SelfResize::with_min(880, 620)`，卡 `abb-settings-window-resize-macos-20261015`，见 §5.2）；`show_window_and_focus()` 里「show 之后补 `set_resizable(true)`」整段删掉（无效时机，且 Slint 建窗时按 `min != max` 自己定 resizable）。
**守卫**：`ui::tests::resize_hit_areas_follow_platform_rule`（结构：两个窗都恒画 / 两窗各有自管接线与自己的下界 / 自管 = macOS / 设置窗钩子必须跟 `CursorMoved` + 三条收尾）+ 无头几何 `ui_headless_render_test::{self_managed_hit_zones_route_press_with_pointer_coords, settings_self_managed_hit_zones_route_press_with_pointer_coords, v11c_resize_zones_hit_all_edges_and_corners, resize_hot_zone_eats_titlebar_top_band}` + 跨文件常量 `syschat::tests::settings_window_min_constants_match_slint`。
**证据（`/private/tmp` 会被清，脚本可按 §1 重建；每份都注明产它的构建）**：
- **可复算（当前 head，debug）**：`verify-80dca07.log`（7 行）与 `verify-80dca07-drag.log`（同样 7 行 + 一次合成拖拽）——前者 `pre-show`=`None` → `t=400ms`=`None` → `t=800ms…2000ms`=`Some is_resizable=true is_decorated=false inner=1040×1120 hit_areas=true self_managed=true`；后者 `t=400ms` 已是 `Some`（同一份代码两次跑，说明**那一档本来就不稳定**），且**拖拽后日志里 `begin`/`cursor_moved` 各 0 条**——06:27 实测：窗口 `(475,124,520,560)`、`swift /private/tmp/drag_once.swift 992 127 -140` ⇒ **锁屏期合成事件打不进 app**（这是当前唯一障碍）。
- **中间构建（不可按当前 head 复算）**：`verify-a1.log`/`verify-a2.log`（各 8 行，01:07 的 A/B：含「试验：set_resizable(false)」探针 ⇒ 其后 `is_resizable=false`；当前 head **没有**这条探针）；`verify-0120.log`（334 行：`t=400…2000` + **323 条** `[selfresize] cursor_moved (…)`）；`verify-0245.log`（**11 行**：`t=400…2000` + **恰好一条** `[selfresize] begin dir=north-east at (508,11)`，**无** `cursor_moved`、**无** `ret=`）。⚠️ 上一版把 `verify-0245.log` 描述成「`begin dir=east` + `ret=true` + 其后数百次 `cursor_moved`」——**逐项与文件不符**，且 `ret=` / `cursor_moved` 这两个日志点在当前 head 上**不存在**（当前 head 唯一相关日志点是 `[selfresize] begin dir={dir} at ({x},{y})`，写在 `src/ui.rs` 自管缩放的按键分支里；**别按行号找**——本节原写 `:8126`，到 `2dfef7a` 已是 `:8134`、到 `ea0429a` 是 `:8139`，行号每两次提交就漂）⇒ 它们只能证明「当时（中间构建、00:10/00:31）合成拖拽确实能让自管会话开起来」，**不能**作为当前 head 的复现。
- `syschat-shots/verify-isresizable.png`：**在盘上**——`/Volumes/DataExt/tmp/syschat-shots/verify-isresizable.png`（107 KB，00:02 产出），画面就是当时代码画出来的对话窗（无边框、标题栏三钮、状态行、空态文案、输入区）。
  ⚠️ **纠偏（2026-10-15 实核）**：上一版写「该目录在盘上已不存在（`ls /private/tmp/syschat-shots` 空）⇒ 证据已失效」——**查错了路径**：出图目录恒在**数据卷**（`src/ui_headless_render_test.rs::SHOTS = /Volumes/DataExt/tmp/syschat-shots`，§1 同）；`/private/tmp` 只是**探针日志**（`verify-*.log`）的落点。⇒ 这张图**没失效**，但它的证据范围要收窄：它产自 **00:02 的中间构建**，只能证明「那天窗真开出来了」，**不能**证明当前 head 的缩放/最大化可用；要当前 head 的等价画面，按 §1 跑无头出图（同目录 `short.png` 等，同输入同像素）。
**⚠️ 2026-10-15 14:44 接手复测（协调者，探针口径）：⤢ 仍不生效——「已做」是乐观结论。**
证据（`ABB_VERIFY_OPEN=1 ABB_VERIFY_KEEP_VISIBLE=1 ABB_VERIFY_TOGGLE_MAX=1`，隔离 HOME，debug 构建）：
```
[verify] t=800ms…2000ms: winit=Some is_resizable=true is_decorated=false inner=1040×1120 hit_areas=true self_managed=true
[verify-toggle-max] before  = x475 y124 w520 h560
[verify-toggle-max] called toggle_syschat_maximize
[verify-toggle-max] after#1 = x0   y231 w520 h560     ← 位置动了、**尺寸没动**
[verify-toggle-max] after#2 = x0   y231 w520 h560
```
⇒ **窗口只被移到别处，尺寸纹丝不动**（就是 owner 说的「⤢ 无效」）。而且**接上原子 `setFrame` 之后仍是这个结果**（`e066740` 已把 `platform::set_window_frame` 接进 `SelfResize::apply` 与 `toggle_syschat_maximize`），所以问题**不在「两个请求打架」**，而更可能在**算出来的目标矩形本身**（`next` 的 w/h 就等于当前尺寸 520×560，且落点 `y=231` 可疑）。
**下一步（不用任何人工点击，闭环已通）**：把探针扩成打印 `work`（`platform::visible_work_area`）与 `next`（`MaximizeState::toggle` 的输出）+ `apply_window_rect_atomic` 的返回值，再跑 `ABB_VERIFY_TOGGLE_MAX=1` 读三行即可定位（怀疑点：`visible_work_area` 的坐标翻转/`primary_h` 取值、或 `MaximizeState` 把「已在最大化」判成了「还原」而回了一个陈旧矩形）。
**✅ 2026-10-15 15:15 进一步定位（探针扩印三个矩形 + 返回值，已提交）**：
```
cur  = x0   y231  w520  h560
work = x0   y33   w1470 h923      ← 工作区算对了（扣掉菜单栏 y=33）
next = x0   y33   w1470 h923      ← 目标矩形也算对了
atomic_setFrame_ok = true         ← 原生 setFrame 自称成功
之后 cur 仍是 x0 y231 520×560      ← 窗口没变
```
⇒ **不是「目标矩形算错」**，而是**原生写框被 Slint 覆盖**：Slint 的 winit 适配层自己记着窗口尺寸
（`slint_size` 与 winit 一致），原生 `setFrame:` 之后它在下一拍又按自己的模型写回 520×560。
**❌ 2026-10-15 15:33 按「删原生路径、只用 Slint API 分两拍」改完仍失败**（提交见下）：
```
before = 475,124 520×560
cur/next 同前：work=0,33 1470×923 → next=0,33 1470×923
after#1 = 0,231 520×560     ← 位置也不是我们写的 33，尺寸更没变
after#2 = 0,231 520×560
```
⇒ **连 Slint 自己的 `set_size`/`set_position` 都没落地**（位置被写成 `y=231` 而不是我们请求的 `33`，
尺寸纹丝不动）。这不像「写被拒」，更像**有另一个写方在改这张窗**：`y=231` 与气泡 `y=247` 只差 16，
高度方向可疑地接近**锚定/跟随**那条链（D4：窗动 ⇒ 气泡吸附；开窗锚定：气泡 ⇒ 窗）。
**✅ 2026-10-15 15:36 对照实验（新增探针 `ABB_VERIFY_SETTINGS=1`，已提交）——本条的定性钥匙**：
对**设置窗**做与对话窗**完全相同**的写入实验（`set_size(1000,700)`，都在 min 之上）：
```
[verify-settings] t=4500ms facts=Some((true, false, 2000×1400)) size=2000×1400   ← 落且保持
[verify-settings] 回读 after: 2000×1400                                          ← 没被打回
```
⇒ **设置窗（同样 `no-frame` + 透明 + is_resizable=true + is_decorated=false）写入能落地，对话窗不能。**
所以：**问题不在 macOS、不在无边框/透明、不在「原生 vs Slint」**，而在**对话窗自己这一侧**（别的写方，或它当时不是「可见且可写」的状态）。

**⚠️ 同时暴露一个测量方法上的坑（必须先排除）**：探针运行里对话窗**不在 on-screen 窗口列表**
（只列出 44×44 的气泡）——**隐藏/未显示的窗口，geometry 写入本来就可能不落地**。所以上面那些
「⤢ 无效」的读数**可能部分是探针环境的假象**。**下一步顺序**：
1. 先让探针里的对话窗**真正可见**（查 `is_visible()`；`ABB_VERIFY_KEEP_VISIBLE` 是否真把「失焦即隐藏」（D5）
   压住了）——**这一步不做，后面的几何结论都不可信**；
2. 再跑 `ABB_VERIFY_TOGGLE_MAX=1`；若仍不落地，给**所有**几何写点加 tag 日志，一次看出谁写回 0,231/520×560
   （重点怀疑 `FollowPump`/开窗锚定/`KEEP_VISIBLE` 复显）；
3. 收口方向：让「最大化/自管缩放」成为**唯一**允许写对话窗几何的路径（缩放会话期间其余让路，D4 已有 `paused`）。

**❌ 2026-10-15 15:45 去掉气泡跟随（owner 口径）后 ⤢ 仍不落地**：`ABB_VERIFY_TOGGLE_MAX=1`
（`vis=true active=true`）→ `next=0,33,1470×923`，`after=0,231,520×560` ⇒ **跟随不是元凶**。
另外注意一个**强线索**：请求 `y=33`，实测落在 **y=231**（差 198）——**位置也没按请求落地，且差值是常数**
⇒ 高度怀疑**坐标系口径不一致**（`work` 来自 AppKit 的 `visibleFrame`，而 `set_position` 是 winit 的
左上原点；两者若混用，位置会整体偏移，尺寸则可能被 macOS 按「新位置放不下」的旧逻辑夹住）。
**下一步（优先级从这开始）**：先把 `work=(0,33,1470,923)` 这个矩形**用三种口径各打印一次**
（AppKit `frame`/`visibleFrame`、winit `current_monitor().position()/size()`、`primary_monitor().size()`），
对齐到同一坐标系再写；这一步没做之前，任何几何写入的读数都别当结论。
**🔑 2026-10-15 15:55 决定性读数（这一条把范围缩到最小）**：
```
[verify-set-size] work-area visible=Some((0.0,33.0,1470.0,923.0)) monitor=Some((0,0,1470,956))
[verify-set-size] before=Some(0,124 520×560)  physical=1040×1120
[verify-set-size] requested logical 1470x923
[verify-set-size] after#1=Some(0,**231** 520×560) physical=1040×1120   ← 只 set_size，却**位置动了**(124→231)
[verify-set-size] after#2=Some(0,231 520×560)
[verify-toggle-max] 下一拍已 set_size(1470,923) → 现 size=PhysicalSize{1040,1120}   ← 写进去了、无效
```
1. **`Window::set_size` 对对话窗是彻底 no-op**（尺寸一次都没变过），而同 API 在**设置窗**上有效
   （`1000×700` → 落地并保持）⇒ **不是平台问题，是这张窗自己的状态**。
2. **只要 `set_size`，位置却会变**（124→231）⇒ 说明**另有一条路径在根据「尺寸变化」重摆窗口**，
   而且**尺寸写本身被丢掉了**。⇒ 最可能：这张窗的 Slint/winit 侧尺寸被**固定/绑定**（或它的
   `min==max` 之类被当成不可缩放），任何 `set_size` 都被忽略，而重摆那条路照常跑。
3. 两窗 `.slint` 的尺寸属性**表面一致**（`no-frame` + `preferred-*` + `min-*`，都无 `max-*`）；
   **下一步（最省事）**：把设置窗与对话窗的**窗口级声明逐行 diff**（含 `callback self-resize-begin`
   / `resize-hit-areas` / `resize-self-managed` 三个属性各自声明在**组件内哪个位置**——对话窗那三个
   是在 `Rectangle` 之后才声明的，设置窗在 `Rectangle` 之前，**声明位置不同**可能让 Slint 的
   窗口属性求值顺序不同）；或者干脆**把对话窗的窗口级声明整段换成设置窗那份**（只改 min 值），
   这是「路 1 照抄」的最小可用形态。

**本轮已按 owner 口径改掉的**：① **气泡跟随整体去掉**（`FollowPump` 不再驱动任何几何，只保留事件记录）；
② **气泡与对话框互斥**（开窗藏气泡、失焦/关窗后气泡回来——`bubble_visible(enabled, dialog_visible)`）。

**下一步（强烈建议新开会话做，二十分钟能定性）**：
1. 在 `apply_window_rect_atomic` 已删的现状下，给**所有** `set_size`/`set_position`/`setFrame` 调用点
   加带 tag 的日志（谁写的、写的什么值），再跑 `ABB_VERIFY_TOGGLE_MAX=1` ⇒ 一眼看出**是谁把窗口写回 0,231/520×560**；
2. 重点怀疑 `FollowPump`（气泡跟随）与开窗锚定（`anchored_dialog_pos`）在窗口几何变化时互相驱动；
   以及 `ABB_VERIFY_KEEP_VISIBLE` 的复显逻辑是否也在写窗口。
3. 定位后：让「最大化/自管缩放」成为**唯一**允许写窗口几何的路径（其余路径在缩放会话期间让路，D4 已有 `paused` 机制可复用）。

**建议的修法（下一步就试这个）**：原生 `setFrame` 成功后**同时把 Slint 自己的尺寸/位置也同步**
（`window.set_size` + `window.set_position`），让两层模型一致；或干脆放弃原生路径，回到
「位置与尺寸分两拍写」（上一轮实测过两种顺序同拍都失败，但**分拍**是当时能生效的形态）。
验证闭环仍是 `ABB_VERIFY_TOGGLE_MAX=1`（每次 12 秒读 3 行，**不需要人工点击**）。

**另两条环境结论（纠正上一版的「锁屏」归因）**：① 屏幕**没锁**（`CGSessionCopyCurrentDictionary`：`kCGSSessionOnConsoleKey=1`、无 `CGSSessionScreenIsLocked` 键）；真正的拦路虎是**本进程没有「辅助功能」授权** ⇒ `CGEventPost` 合成点击被丢弃（`osascript click at` 报 `-25211` 同一件事）——所以「锁屏」**不是**根因。② 隔离运行时对话窗**不在 on-screen 窗口列表里**（只列出 44×44 的气泡），尽管日志显示它有 `inner=1040×1120` ⇒ 「自己开窗量图」这条路还差一步（`ABB_VERIFY_KEEP_VISIBLE` 也没让它留下），要么让它真正显示、要么只靠探针读数。

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
| 跟进卡 | **已收口**（2026-10-15 落 main，见 §6 末条） | `abb-syschat-b-followups`（O1–O4）/ `abb-syschat-c-followups`（O1–O6 + C2 A–E）的**非阻塞项**逐条处置：修了表内 HTML 转义、`styled` 白算、⌘V 拦截（busy/repeat）、附件半截目标清理、7 处冗余 `allow`、拖放去重判据；其余**如实记录**（不可达分支/解析器口径/上游边界/按设计保留）。剩**三条 owner 待拍**：① `~a~` 单波浪要不要改 GFM 严格（现在画删除线，见 §6）；② 关窗是否清待发附件（现在保留）；③ 思考块标题文案要不要按稿从「思考过程 · X.Xs」改成「思考 · 用时 X.Xs」（稿 `details.think`，见 §6）。原第③条（多文件拖入状态行计数）**已修**：一次拖放逐文件到达的 `DroppedFile` 现在攒一个 80ms 短批再整批落地，状态行报**这一批的真实个数**，与 📎 待发行一致（见 §6「C2-B」条）。同批另收口两张卡：`abb-wording-role-label-kind-20261013`（角色身份字段化，`ea0429a`，无用户可见变化）与 `abb-syschat-e2-thought-20261013`（E2 审计，2 条 ◐ 见 §6）。 |
| **表格/图片/数学渲染 + 空会话归档** | **已做**（owner 2026-10-15 反馈，见 §6「渲染修复」） | 四件事：①**表格整块化**（列宽跟内容、整表一个 Flickable、行高取该行最高格——旧的左列被裁/列间距巨大/每行独立横滑已修）；②**图片块高度显式入布局**（裸 `Rectangle` 不被子 `Image` 撑高，旧版只报 ~30px ⇒ 图片盖住后续内容）；③**数学**：块级公式 `ratex-*`（KaTeX 的 Rust 移植，字体内嵌）真排版成 SVG；**行内公式用 `parley` inline box 与文字混排**（与 Slint 同一排版引擎；代价：这些段落不可拖选 + 宽度变化要重排，owner 已确认）；④**空会话不归档**（`archive_current` 闸门）。判据：mdblocks 2 条（数学三层 + 孤立 `$` 反例）、mathrender 2 条（常见公式能排 + 非法 TeX 回落）、inlineflow 5 条（基线对齐/窄宽多行/非法回落/空输入/拼回原文）、syschat 1 条（空会话拒绝归档）、ui 3 条（整表合一块且列宽跟内容、图片块报高度、`mathp` 按坐标渲染）。顺带清了 owner 盘上 20 条空的「新会话」归档项（`sessions.json.bak-empty-archive-*` 有备份）。剩 **owner 真机复验**：4 列 upstream 表能不能左右滑、图片后面文字是否完整、行内/块级公式是否好看、拖窗口时含公式段落是否正确重排。 |

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

**B/C 批非阻塞收口（2026-10-15，卡 `abb-syschat-b-followups` / `abb-syschat-c-followups`）**
- **表格单元格是纯文本**：表内 `Event::Text`/`Code`/`Html` 一律**不转义、不过行内 markdown**，因为 GUI 那一支就是 `Text { text: c }`。表内 HTML 曾经走 `push_escaped` ⇒ `| <b>x</b> |` 会把反斜杠**画出来**。
- **行内单波浪 `~a~` 会被画成删除线**（实测，非本仓取舍）：pulldown-cmark 0.13.4 的 `MaybeEmphasis` 分支里，`~` 成对且**没开** `ENABLE_SUBSCRIPT` 就归 `Strikethrough`；我们只开了 `ENABLE_STRIKETHROUGH`，于是 `~a~` 与 `~~a~~` 解析后**同形**（都重发成 `~~a~~`）。字面量波浪号**不受影响**：`~` 要贴着非空白字符才成对，`3~4`/`好的~`/`~/a`/`a ~ b ~ c`/`把文件放到 ~/a 和 ~/b` 全部保持纯文本（`push_escaped` 转义），逐条钉在 `tilde_is_escaped_when_literal_and_detected_when_a_style` 里。要不要改成 GFM 严格（只有 `~~` 是删除线）需要换解析口径，**待 owner 拍**。
- **`dest_strip_title` 的「空格 + 引号」分支取不到输入**：`Tag::Link/Image` 的 `dest_url` 本就不含 title（title 是同一个 tag 的独立字段），把它换成恒等也不会有用例变红。保留为**防御性**分支，契约由直接喂串的单测 `dest_strip_title_pins_title_contract` 钉住；集成用例 `path_with_spaces_not_truncated` 只证「带空格路径不被切」。
- **`styled` 只对行内 markdown kind 计算**（`mdblocks::inline_markdown_kind`，与 `selectable_plain` 同源）：`code` 用原文、`table|hr|image|audio|video` 根本不读 `styled`；旧写法对每个块都算一遍 = 大代码块白解析一遍。
- **多文件拖入的状态行按「批」报数（C2-B 已修）**：winit 对一次拖放**逐文件**发 `DroppedFile`，旧接线每个事件各自落地一次 ⇒ 状态行被最后一条覆盖成「已附加 1 个文件」，而 📎 待发行显示 N。现在 `DropBatcher` 把同窗事件攒起来，80ms 单发定时器整批落地：状态行报**这一批**的真实个数（= 📎 的增量），且 `land_attachments` 的整批回滚语义正好覆盖「一批里有一个失败全退」。判据：纯逻辑 `drop_batcher_counts_whole_gesture`（3 个事件 ⇒ 批 3 个、顺序保持、取走即清空）+ 源码接线守卫 `dropped_files_are_batched_before_landing`（必须走攒批、不许退回每事件直呼 `land_drop`）；变异 2 处红后还原（`take()` 只回最后一个 / 接线退回直呼）。
- **⌘V 的文件拦截不再看 `busy`**：待发列表与「有没有在生成」无关，而旧口径在 busy 期放行 ⇒ 文件路径被 `TextInput` 当文本粘进输入框；长按 repeat 现在**只吞不重复落**（每次都落会得到 `-1`/`-2` 重复附件）。判据是纯函数 `ui::cmd_v_action`，接线另有源码守卫。
- **winit 的拖放只注册 `NSFilenamesPboardType`**（0.30.13 `window_delegate.rs:666-669`），取不到即 `return false`（`:376`/`:405`）⇒ 只提供别的拖放类型的源**一个事件都不会有**（浏览器直拖图片可能属此类）。**源码已实核，行为后果未在本机实测**（锁屏期做不了真机拖放）；📎 的 tooltip 只写正向可用路径（📎 / 拖入窗口 / ⌘V），不写这句未验证的负面结论。
- **待发附件跨「关窗 → 重开」保留**（与输入框草稿同寿命：`hide_syschat` 不清 `draft` 也不清待发，两者都只在**成功发送**时清）。worker 卡面写的「重开为空」是**未执行的预测**，按设计保留。
- **附件落地半截文件**：`std::fs::copy` 是「先建目标再写」，写到一半失败（ENOSPC/EIO）会留下内容不完整的文件，而回滚只清成功项 ⇒ `land_one` 在 copy 失败分支 best-effort 删目标。该分支**测试无法确定性触发**（可诱导的失败都发生在目标的 `open` 上），注释里如实写明，不声称有判据。

**角色块身份 / E2 审计 / gates.sh locale（2026-10-15，main = ea0429a）**
- **身份判据不许用展示文案**（卡 `abb-wording-role-label-kind-20261013`）：角色块的「是不是助手」现在是 `SysChatBlockRow.role-assistant`（bool），
  由 `ui::role_row(label, assistant)` 给——`rows_for` 从历史的 `role` 字段推导、`stream_rows` 恒助手、其余三个构造点显式 `false`。
  旧写法 `color: b.text == "怹" ? …` 把**角色名文案**当身份判据：改名就恒假、着色静默失效（「ABB → 怹」那次作者漏改消费侧，评审 blocking 抓到过）。
  守卫两条：`role_identity_is_role_field_not_label_text`（同一 label 能取两种身份、名字叫「怹」≠ 助手）+ 跨文件
  `role_colour_keys_on_identity_not_on_text`（`ui/app.slint` 里 `b.text ==` 零命中 **且** 着色确由 `b.role-assistant` 决定 **且** 角色分支仍在——
  只看「零命中」会被「整个分支被删」白过）。变异 3 处红后还原（身份写反 / 着色退回文案判据 / 判据留住但取错色）。
  **无用户可见变化（同文案下取色逐字一致）⇒ 未重装 bundle**，/Applications/ABB.app 仍是 1d26dcf。
- **E2（思考/正文分离）审计结论**（卡 `abb-syschat-e2-thought-20261013`，closed）：六条验收 **4 ✅ + 2 ◐**。
  ✅：①执行层只把 `ThinkingDelta` 投成 `agent_thought_chunk`（根侧按 `ChunkKind::Thought/Text` 记段，不合成字符串再猜）；
  ④耗时口径＝**首末思考分片的墙钟差、逐段累加**（进行中的最后一段用 `now`，更早的段用自身 `last`，不被 now 重复放大）+ `format_elapsed`（<60s `X.Xs`，否则 `X.Xm`）；
  ⑤四类边界都有单测（空 delta 丢弃 / 只有思考 `finish("")` 后仍在 / 只有正文 / 多段思考）；⑥四道门禁全绿。
  ◐：**②块序列不保留交替**——累积模型里交替是被保住的（单测断言 `[Thought,Text,Thought,Text]` 与逐段文本），但渲染层把同类思考段汇成**一个**思考块置于正文之前
  ⇒「思考→正文→思考」显示成 思考(A+C) + 正文(B)。**原因是历史模型 `Turn.thought` 是单字段**（逐段交替不可持久化 ⇒ 实时与重开视图会不一致），要真做需给 `Turn` 加 segments（schema + 迁移）；
  **③视觉细节与稿不同**：稿 `details.think` 是 2px 实线 + `body{border-top:2px dashed}`，**Slint 没有 dashed 边框**（实核：`i-slint-compiler-1.18.0` 与 vendored `third_party/i-slint-core` 里 `dashed|border-style|BorderStyle` **零命中**，`Rectangle` 只文档化 `border-width`/`border-color`）⇒ 实现用 1px 实线 + radius 6、无分隔虚线；标题文案「思考过程 · 3.2s」（进行中「思考中」）vs 稿「思考 · 用时 X.Xs」（**待 owner 拍**）。
  变异 2 处红后还原：`text_of` 去掉 kind 过滤 ⇒ 分段用例红（`src/syschat.rs:3774`）；执行层把 `agent_thought_chunk` 投成 `agent_message_chunk` ⇒ 投影用例红（`crates/abb-agent/src/acp.rs:1300`）。
- **`tools/gates.sh` 的 UTF-8 locale 坑是「已修」不是「待修」**（卡 `abb-gates-sh-locale-unbound-var-20261012`，closed）：修法在 `f88a606`（`${CARGO_TARGET_DIR}` 把变量名封住）。
  判据可原样复跑：`rg -n '\$[A-Za-z_][A-Za-z0-9_]*[）、，。；：？！「」【】（）]' tools/gates.sh` ⇒ 0 命中；`LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 bash tools/gates.sh --print-env` ⇒ exit 0。
  机制阳性对照（证明「按 `LC_ALL=C` 复核会漏」）：`LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 /bin/bash -c 'set -u; X=/tmp; echo "== $X）=="'` ⇒ `unbound variable` / **exit 127**，同句 `LC_ALL=C` ⇒ **exit 0**。
  ⇒ 本仓门禁证据若在 UTF-8 locale 下拿到 exit 0，是**真跑过**四条（不是脚本自己死在第一行）。

**渲染修复（2026-10-15，owner 反馈：表格 / 图片 / 数学 / 空会话归档）**
- **Slint 里「裸 `Rectangle` 不会被它的子项撑高」是图片 bug 的根因**：`Rectangle{ width:100%; Image{ width:100%; height:200px } }` 只向布局报 **~30px**（实测 `dbg-col-h`：无图 `51`、`role+图片` `81`），图片却实画 ~200px ⇒ 后续块全被图片盖住、滞底也算不到底。修法：Rust 侧用 `image::image_dimensions`（**只读头部**）拿源尺寸，把**宽高比**喂给 `.slint`，块高显式写 `max(120px, min(360px, root.convo-text-width * b.img-aspect))`（未知比例回落 200px）。**同类坑**：表格单元格与数学块也都得用**命名元素**（`cell-txt.preferred-height`）显式算高度——凡「高度靠子项决定」的块都不能交给裸容器。
- **表格必须整表一个块**：旧版（`4bb9cbb`）一行一个块、每块一个 Flickable、每列 `max(均分,132px)` ⇒ 左列被裁 + 列间距巨大 + 各行能独立横滑。现在 `ui::push_block_rows` 把**连续** `table` 块合成一个渲染行；列宽在 Rust 侧按字符类别估（`table_col_widths`：CJK/全角 12px/字、其余 6.6px/字，+10px 内边距，夹 `[44,240]`）；整表**一个**横向 Flickable。⇒ 列宽跟内容走、行高取该行最高格、所有列一起滑。`mdblocks` 仍是一行一个 `table` 块（解析层不动，测试面不受影响）。
- **行内公式：自建混排层（2026-10-15，`src/inlineflow.rs`）——关键实核是「Slint 自己就用 parley」**：
  - Slint 1.18 的文字排版走 **parley**（`third_party/i-slint-core/textlayout/sharedparley.rs`，`shared-parley` feature）；`parley` 0.11 **原生支持 inline box**（`InlineBox{id,index,width,height}` → `PositionedInlineBox{x,y,..}`，语义 = **box 底边贴基线**，见 `parley-0.11.1/src/layout/line.rs` 的 `y = baseline - height`）。
  - ⇒ 把 `parley` 加成**直接依赖**，`cargo build` **只编译 agent-bridge**（零新增编译单元；parley/fontique/swash/skrifa 已在 i-slint-core 依赖树里，同 `pulldown-cmark`）。断行与字体（fontique 解析系统字体）与 Slint 同源。
  - 做法：`mdblocks` 在段落的**无标记纯文本**（`MdBlock::plain`）上记下 `(字节偏移, TeX)`（`MdBlock::math`；行内码仍留在 `text` 里做退路）→ `inlineflow` 用 parley 排「文字 + 每公式一个 in-flow box」，取出每行的**文字片段的 x/基线**与**公式框的 x**，自己按「文字 metrics ∪ 公式 ascent/depth」算行高与基线 → `.slint` 新块 `mathp` 按坐标**绝对定位**（文字仍交给 Slint `Text`，公式交给 `Image`）。
  - **试过但不行的路**：Slint 的 `FlexboxLayout`（1.18 有，taffy 实现）**没有 baseline 对齐**（`CrossAxisAlignment` 只有 `auto/stretch/start/end/center`，实核 `i-slint-common-0.11/enums.rs`）⇒ 公式对不上文字基线，直接排除。
  - **两个已确认的取舍**：①`mathp` 段落的文字**不能拖选/复制**（不是流式文本；兜底是助手轮的 `⧉ 复制`）；②宽度变化要**原地重排**（`ui::relayout_flow_rows`，源信息存在行的 `math-off`/`math-tex` 里 ⇒ 不重建会话、不丢流式状态）。
  - **别重走的路**：GitHub/crates.io 上**没有**许可宽松的「Slint + Markdown + KaTeX」库可用——Slint 上游 PR #12664（draft）的「图片」只是 alt 文本、「数学」只是 `Style::Math` 文本样式；能真正行内混排的 `sred`（GPL-3.0）与 `lpmwfx/rich-text-editor`（EUPL-1.2）都是 copyleft，与 MIT 的 ABB 不兼容。
- **数学渲染器（`ratex`，MIT）**：`ENABLE_MATH` 只让 `pulldown` 发 `InlineMath`/`DisplayMath` 事件，真排版另起一层——`ratex-parser`→`ratex-layout`→`ratex-svg`（`standalone`+`embed-fonts`，KaTeX TTF 内嵌 ~580K）出**自包含 SVG** → 写 `syschat/math-cache/<sha256>-<字号×100>.svg` → Slint（已带 resvg）栅格化。**缓存键必须含字号**（行内 13px / 块级 15px 同公式分开存）。`MathImage` 同时给 `w/h/ascent/depth`——行内混排靠 `ascent/depth` 贴基线。**失败绝不吞字**：非法 TeX ⇒ `None` ⇒ `.slint` 回落等宽源码。
- **`ENABLE_MATH` 的副作用边界**：开它之后表格单元格也会收到 math 事件，漏处理就会**静默丢字** ⇒ 表内分支必须先 `push_str` 原文（单测 `math_inline_display_and_in_table` 钉住）；孤立 `$`（`$5`/`$HOME`）仍是字面量（反例单测 `math_option_does_not_eat_lone_dollar`）。
- **空会话不归档**：GUI「🗄 归档…」改走 `syschat::archive_current()`——`is_empty_session` 为真就直接回「无需归档」，**不落盘、不新建**。为什么必须挡：归档空会话后 `ensure_current` 发现「没有未归档会话」会**立刻再造一条空会话** ⇒「归档空会话 → 新建空会话」能无限刷（owner 盘上实测 25 条会话里 20 条是这么来的空壳）。判据在 `syschat` 单测里做了正反两面（空 ⇒ 不归档且索引一字未动；有内容 ⇒ 必须能归档）。
- **「清空」必须把索引也复位**（`clear_session`）：`sessions.json` 的 `title` = 「首条用户消息」，以前只删 `sessions/<id>.jsonl`、**不动索引** ⇒ 列表里留下「有旧标题、没内容」的**幽灵会话**（`is_empty_session` 判空、列表却还写着旧标题）。owner 盘上实测两条：`hi`、`😀`（创建后有轮次 ⇒ 标题已落；后来在对话窗点「清空」⇒ 文件没了、索引没动；再被归档就成了那个样子）。现在清空同时把该条目复位成空会话形态（`title` → 「新会话」、`context_tokens` → 0；`id`/`created_at`/`provider` 不动——换 id 会把 `current` 弄丢）。判据 `clear_session_resets_title_so_no_titled_ghost_remains`（清后必须被判空会话、幂等）。

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
# 版本号必须与 Cargo.toml 一致（`scripts/build.sh` 有这一步，手工配方以前漏了 ⇒ 「关于」里
# 一直显示 Info.plist 模板里的旧值 2.0.3）。**必需在签名之前改**，改完再签（plist 被签名封住）。
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
sed -i '' "s|<string>2\.[0-9]*\.[0-9]*</string>|<string>$VERSION</string>|g" "$B/Contents/Info.plist"
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
>
> ⚠️ **装机后核 sha256 要跟「刚 ditto 过去的那份暂存件」比，别跟「重新编一份」比**：
> 同一份源码两次构建**不是逐字节可复现的**——实测 `crates/abb-agent` 在 `1d26dcf` 与 `5978f34` 之间**零 diff**，
> 但 06:24 装机的那份 `abb-agent` 是 `7f1832…`、本轮重编同一份源码得到 `938518…`。**跨 target 目录/跨次构建就会变**。
> 正确判据：`shasum -a256 /Applications/ABB.app/Contents/MacOS/*` 与 staging 目录里的同名文件**逐字节相等**（`ditto` 保字节）。
>
> ⚠️ **在二进制里核「中文串在不在」，裸 `grep` 会静默骗你**：UTF-8 locale 下 `grep -a -c '添加附件' <binary>` 返回 **0**，
> 而 `LC_ALL=C grep -a -c …` 返回 **1**（同一份文件；本仓已有同类教训 `LESSON_grep默认跳过非UTF8文件…` 的追加）。
> 用 `LC_ALL=C grep -a` 或按字节数（python `data.count(pat.encode())`）。本轮实测：新构建与已装机两份二进制里
> `添加附件（可多选，也可拖入窗口或 ⌘V 粘贴）`、`[selfresize] begin dir=`、`agent_thought_chunk`、`_session/steering` **都在（各 1）**。
>
> 📦 **最新暂存件**（2026-10-15 23:2x，`main = a2cab99`，含表格/图片/行内公式/空会话归档/清空幽灵修复）：`/Volumes/DataExt/tmp/ABB-stage-a2cab99/ABB.app`（**已装机**；主二进制 sha256 `agent-bridge c1cdd30e…` / `abb-agent 7eefbf49…`，与装机后逐字节一致）；更早的 `ABB-stage-0057811`、`ABB-stage-5978f34` 仍在盘上作历史。旧 bundle 备份：`ABB.app.prev-20261015-232738` / `…-180631`。
> 要用的时候只剩最后三段（quit / `mv` 备份 / `ditto` + `open`）。

### 7.3 接手第一小时建议
1. 读 §1，自己开窗抓一张图，建立「我能自己验收」的闭环（这一步能省掉后面所有「让用户截图」的往返）。
2. §5.1 缩放**已修**（卡 `abb-syschat-resize-macos-20261015`：对话窗恒画热区 + macOS 自管缩放；设置窗同日接上同一套，见 §5.2），剩**真机手感**复验；**别再拿 `is_resizable` 取证**（它既不假也不是判据，见 §5.1）。
3. §5.2 的清单**已全部落地**（缩放 ✅ / 设置窗缩放 ✅ / 消息动作 ✅ / 归档 ✅ / 用量(E3) ✅ / 截屏 ✅ / 思考块 ✅ / 插入(steer) ✅）；剩下的活是**owner 真机复验**（§5.2 每行末尾写了各自要看什么）与两张跟进卡（`abb-syschat-b-followups` / `abb-syschat-c-followups`，**均已收口**）。板上与本专题相关的 open 卡到 `ea0429a` 为止已清空（角色身份 `abb-wording-role-label-kind-20261013`、E2 审计 `abb-syschat-e2-thought-20261013`、`tools/gates.sh` locale `abb-gates-sh-locale-unbound-var-20261012` 三张本批 closed；`abb-syschat-resize-doc-residue-20261015` 已于 `2dfef7a` 收口）。
4. **动 `.slint` 里任何窗口/图标之后，先让无头台渲一张**（设置窗那批图标就是「一直画不出来但谁也不报错」，见 §6）；`.slint` 还有两条硬规矩：组件必须声明在使用点**之前**（前向引用直接编译失败）、同一个窗口**只能有一个** `on_winit_window_event` 钩子（单槽 `set`，另起一个会静默顶掉前一个）。
5. 每做完 2–3 条，**批量**跑四门禁 + 独立评审 + 打包安装一次（owner 已同意这条降档口径）。
   **装包这一半别漏**：2026-10-15 的教训是「代码改了、文案改了，但机器上的 bundle 还停在 3 小时前」——owner 读到的
   工作区指引因此一直写着旧数字（`manual:1`），改动在他那里从未生效。改造完用户**看得见的那份载体**（bundle 里的
   工作区指引 / GUI）之后，**要装包**（§7.2），并核对 `strings /Applications/ABB.app/Contents/MacOS/agent-bridge | grep abb-guide-v`
   与启动日志，而不是只看仓库里的 `cargo build`。本轮装包记录见 §0 顶部。
