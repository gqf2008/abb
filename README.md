# ABB — 把聊天软件变成你的 AI 助理

> 在飞书 / 微信 / 钉钉里，直接和随包自带的 AI agent 对话——零安装、零配置命令行。
> 不用切窗口、不用开网页、不用把数据交出去。

ABB 是一个住在你菜单栏（Windows 托盘）里的小助手：你把它接上飞书 / 微信 / 钉钉的机器人后，
**在聊天里就能使唤最强大、最懂你的本地 AI**——查资料、写代码、整理文档、定时提醒，它都干。

---

## 为什么选 ABB

- **就在你聊天的地方干活**：不用在浏览器和聊天软件之间来回切。微信里 @ 一下，它立刻回复。
- **开箱即用**：执行层随包自带（buzz-agent），装完 ABB 就能聊，不需要再装任何命令行工具。
- **用的是你自己的模型**：接任意 Anthropic 原生 / OpenAI 兼容供应商（DeepSeek、通义、Kimi……），
  想用哪个用哪个。
- **数据不出本机**：消息和模型都在你的电脑上流转，不经过第三方中转。
- **会记会话、能打断**：多轮上下文不会丢，说一声「停」就能叫停。
- **一句话创建定时任务**：`每天 9:30 提醒我开站会` → 到点自动把结果发回聊天。
- **多平台、多机器人**：飞书、微信、钉钉可以同时挂，每个 bot 独立配置自己的模型。

## 它长什么样

```
你在微信里发：  明天上午 10 点提醒我交周报
机器人在聊天里回：⏰ 定时任务已创建：明天 10:00 提醒「交周报」
第二天 10:00 机器人主动发：⏰ 定时提醒

交周报
```

查资料、写周报、整理会议纪要、让 AI 定时巡检——都是这样一句句聊出来的。

## 一分钟上手

1. **下载**：去 [Releases](https://github.com/gqf2008/abb/releases) 拿对应平台的安装包
   - macOS（**仅 Apple Silicon**，arm64）：打开 `ABB-*.dmg`，把 `ABB.app` 拖进「应用程序」，打开即可
     > Intel Mac 暂不支持，请勿下载（arm64 包在 Intel 上无法启动）
   - Windows：运行 `ABB-Setup-*.exe` 安装，开始菜单点「ABB」启动
2. **接上你的 bot**：点菜单栏的 ABB 图标 → 设置 → 按提示添加飞书 / 微信 / 钉钉机器人
   （微信扫码登录即可，全程无需命令行）
3. **开聊**：在 bot 私聊里发第一条消息，剩下的交给它

> 第一次使用按系统提示授权「完全磁盘访问 / 辅助功能」等权限即可，之后开机自启、后台常驻。

## 支持的功能

| 能力 | 说明 |
|---|---|
| 多通道接入 | 飞书（官方长连接）、微信（扫码登录）、钉钉（企业内部应用） |
| 随包执行层 | 内置 buzz-agent，零安装；模型走 Anthropic 原生 / OpenAI 兼容供应商（设置里自助添加） |
| 模型供应商 | 设置 →「供应商」里可选并预置：`anthropic`（原生）、`openai-chat`（任意 OpenAI 兼容端点，含本地 Ollama / 自建网关）、`openai-responses`、`openrouter`（免填 `https://openrouter.ai/api/v1`）、`deepseek`（免填 `https://api.deepseek.com/v1`）；预置端点留空即用官方，填了以你填的为准（代理/网关） |
| 内置开发工具 | 安装包随带固定版本 `rg` / `jq` / `uv` / `gh`，仅注入 agent 子进程 PATH；`git` / `bun` / `sed` / `find` 仍使用系统或用户环境 |
| 多轮会话 | 每个 bot 独立记忆，不会串味；任务进行中发 `/cancel`（或「停止」等自然词）立即取消，无任务时发 `/cancel` 会给明确提示；聊天发 `/new` 立即新建会话（清空上下文，无需重启） |
| 引用/回复上下文 | 引用一条消息再 @ bot 时，自动读取被引用消息内容（飞书按 parent_id 拉取、微信 ref_msg、钉钉 repliedMsg）带进 agent，回复不会脱离上文 |
| 定时任务 | 自然语言创建：`每天 9 点`、`每 30 分钟`、`工作日 10 点`……中文 cron 全支持 |
| 结果主动推送 | 任务到点自动把结果发回聊天，不用你盯着 |
| 开机自启 | 托盘常驻，崩溃自动拉起 |
| 多机器人 | 飞书、微信、钉钉同时在线，各自独立配置 |
| 跨会话投递 | 设置里开启后，agent 可把消息/任务结果投递到其它 bot 的会话（`agent-bridge deliver --bot <key> --chat <id> --text ...`）；附件由接收端平台**真发送**（飞书全类型；微信图片/文件/视频，音频按文件发；钉钉单聊/群聊图片与 doc/docx/xlsx/pdf/zip/rar 文件。不支持的类型显式报错，不再只发本地路径文本）；支持定时任务多目标（`job add --to bot:chat`） |
| 当前会话发附件 | agent 把文件发回**正在对话的会话**用 `agent-bridge deliver --to-current --file <路径>`（桥的回复通道只发文本，附件必须显式发；同会话投递的自环保护不受影响） |
| 删除保护 | agent 删除 → 移入工作区回收站（`/trash list` 查看、`/trash restore` 恢复、TTL 自动清理）；危险删除（≥50MB / 含源码）拦截并需 `/trash confirm` 二次确认；有 git 时自动快照留痕 |

## 隐私

你的聊天内容、密钥、模型调用全部只在本机处理（数据目录 `~/.agent-bridge`），
**不会上传到任何 ABB 服务器**——ABB 自己根本没有服务器。

### 授权者隔离（安全默认）

通过授权码进来的**授权者**（非 owner）驱动 agent 时默认走**受限模式**（设置 →
机器人配置 → 「授权者 agent 隔离」，默认开启）：

- 只能读/写该 bot 的**工作区**（`~/.agent-bridge/workspaces/<bot>/`）内文件；
- 可执行的命令仅限 `$ABB_BIN`（定时任务/投递）、只读 git 与少量只读命令；
- **不能联网**、不能访问工作区外任何路径（`.ssh`、config.json 凭证、主目录等
  一律拦截并记录）。由随包 agent 的受限执行剖面硬闸（工作区可写根 + shell argv
  白名单，会话建立时经能力协商下发；随包 agent 过旧无该能力时授权者会话直接拒答）。

owner 会话不受影响（保持本机全权限）；信任的团队成员可在设置窗关闭该开关
（授权者恢复与 owner 同权限）。

### 删除保护（回收站，默认开启）

设计目标：agent 的每个删除动作都**可撤销、可追溯**（#88，bot 配置页可调，默认全开）。
当前状态：授权者（受限）会话已由随包 agent 白名单硬闸禁止一切删除；owner 会话的
自动拦截正随单后端化迁移（见本节末「实现方式」），以下三层为迁移完成后的完整形态：

- **回收站（兜底层）**：agent 在工作区内执行删除 → 不直接删，移入 `workspace/.trash/`，
  保留 **7 天**（可配置 TTL）后自动清理。聊天里发 `/trash list` 查看、`/trash restore <id>`
  恢复、`/trash purge` 清空过期项。
- **危险删除确认（拦截层）**：删除 ≥50MB（可配置阈值）或含代码特征（.py/.rs/.go/.js/
  package.json/Cargo.toml 等，可配置扩展名）的路径 → **拦截并等待二次确认**——agent
  回复会说明拦截原因，你在聊天里发 `/trash confirm <路径>` 确认后才会移入回收站。
- **git 时光机（留痕层）**：工作区已有 `.git` 时，删除前后自动 `git add -A` 快照
  （回收站清理同样留痕），即使过了 TTL 也有历史可回退。

实现方式：回收站与 git 留痕由桥侧 trash 模块提供，`trash` CLI 与聊天指令供手动管理
（`agent-bridge trash list|restore|purge|confirm`）；授权者（受限）会话的删除由随包
agent 的 shell 白名单硬闸直接禁止。owner 会话删除的自动拦截原挂在已退役的 Claude/Codex
CLI hook 上，正随单后端化迁移进随包 agent——迁移完成前 owner 删除暂不自动进回收站。

## 常见问题

- **Q：支持 Intel Mac 吗？**
  A：暂不支持。当前 macOS 版仅提供 Apple Silicon（M 系列芯片，arm64）安装包；Intel Mac 上无法运行。

- **Q：支持哪些模型？**
  A：agent 随包自带、开箱即用；模型在设置里加任意 Anthropic 原生 / OpenAI 兼容供应商（DeepSeek、Kimi、通义等）即可。
- **Q：定时任务没反应？**
  A：确认托盘里服务在运行（绿色状态）；任务结果会发回你创建任务时的会话。
- **Q：能同时挂几个机器人？**
  A：能。飞书 / 微信 / 钉钉各挂几个都行，每个 bot 独立工作目录，互不干扰。

## 更新

新版本会发布在 [Releases](https://github.com/gqf2008/abb/releases)，macOS 包已签名并公证，
Windows 包为免管理员安装。有问题欢迎提 [Issue](https://github.com/gqf2008/abb/issues)。

---

## 从源码构建（开发者）

仓库依赖同作者的 [slint-pixel](https://github.com/gqf2008/slint-pixel) 组件库，
Cargo 以 `../slint-pixel` 路径依赖引用，克隆后需放到同级目录再构建：

```sh
git clone https://github.com/gqf2008/abb.git
git clone https://github.com/gqf2008/slint-pixel.git
cd abb && cargo build --release
```

## 开发者文档

- [任务模型（复盘 + 重新设计）](docs/task-model.md)：`job` / 后台子代理 / 常驻进程三层的统一术语、数据模型与分阶段落地（草案，#307）。
- [会话隔离机制（群 / 话题 / 用户）](docs/session-isolation.md)：三平台 chat_id 规则、隔离矩阵、话题维度结论与最小改动方案。
- 跨会话投递（issue #21）：开关 `config.json#cross_delivery_enabled`（设置窗可勾选，默认关）；agent 用 `$ABB_BIN deliver` 投递，CLI 入队 `~/.agent-bridge/deliveries.json`（0600），service 消费循环经路由表发送；微信目标失败落其 outbox 补发、其余失败回源报错；同来源/目标/文本（+附件 sha256）10 分钟防循环去重，定时任务（`job add --to`）豁免。

## License

ABB 本体采用 **MIT**，全文见 [LICENSE](LICENSE)。

随包/内嵌的第三方组件保留各自的许可证，不适用本仓库的 MIT。**安装包里附带的**：

- `ABB-LICENSE.txt` — ABB 本体（MIT）
- `buzz-LICENSE.txt` — `crates/buzz-agent/`（buzz 分叉，Apache-2.0）
- `tools/licenses/` — 随包工具 `rg` / `jq` / `uv` / `gh` 及其传递依赖

**只在源码树里**（不出现在安装包中）：

- `third_party/i-slint-core/LICENSES/` — Slint 组件库授权
- `src/buzz/UPSTREAM-LICENSE.txt` — 上游 buzz 移植部分的许可说明
