# ABB 作为 MCP 总线：架构方向与渠道契约

> 状态：**方向已定（owner 2026-11-02），实施未开始**。本文是契约与分期，不是既成事实。
> 取代 `docs/mcp-resource-notification.md` 里「执行层转发」的旧模型（见该文顶部告示）。

## 0. 一句话

**ABB 是总线**：一边接**支持 ACP 的智能体**，一边接**各种 MCP 渠道服务**（IM / 邮件 /
GitHub issue / GitLab issue / …）。ABB 自己**不再内置任何渠道**，也不再在界面里配置渠道；
界面只配置**模型供应商**。渠道怎么配由**提供 MCP 的那一方**决定，ABB 的义务是**用自然语言
告诉用户该怎么配**。

## 1. 为什么这是对的（而不是上一版的补丁）

旧模型把 MCP 连接放在**执行层**（`abb-agent`），再让执行层用一个**私有 ACP 通知**
`_abb/unstable/resource/updated` 把事件转发给 ABB。两个后果：

1. **标准之外**：ACP 规范里没有这个通知 ⇒ 任何**其它** ACP agent（ki / Claude / Codex）
   都不可能实现它。换执行层，能力就归零。
2. **层级放错**：MCP 连接是 ABB 的关切（它要拿到渠道事件），不是 agent 的关切
   （agent 只需要能调工具）。

2026-10-23 执行层换成 ki 之后，第 ② 层照常收到 `notifications/resources/updated`，
但**第 ④ 层没人发了**（ki 二进制里 `_abb/unstable` 零命中），第 ⑤ 层代码完好却永远收不到输入
——这正是旧模型脆弱性的实证，不是 ki 的缺陷。

## 2. 目标拓扑

```
                         ┌────────────── ABB 宿主进程（总线）───────────────┐
  ┌──────────────┐       │                                                   │
  │ 渠道 MCP 服务 │◄──────┼── MCP client（ABB 自己）                          │
  │ 飞书/邮件/…   │  MCP  │      ├─ tools/call        ← 出站：发消息          │
  │ （外部进程）  │       │      └─ resources/*       ← 入站：订阅收通知       │
  └──────────────┘       │             │                                     │
                         │             ▼                                     │
                         │   入站消息 → 去重 → 节流 → 频道队列               │
                         │                              │                    │
                         │                              ▼                    │
                         │                       回合（session/prompt）       │
                         │                              │  ACP（标准）        │
                         │                              ▼                    │
                         │  ┌───────────────────────────────────────────┐    │
                         │  │  支持 ACP 的智能体（ki / Claude / Codex…）  │    │
                         │  │  只负责「思考 + 调工具」，不认识任何渠道     │    │
                         │  └───────────────────────────────────────────┘    │
                         └───────────────────────────────────────────────────┘
```

三条链**互相独立**，任一环节可替换：

| 链 | 协议 | 谁连谁 | 换掉会怎样 |
|---|---|---|---|
| 入站（渠道 → ABB） | MCP 通知/资源 | **ABB** → 渠道 MCP server | 换执行层**无影响** |
| 出站（ABB → 渠道） | MCP 工具 | **ABB** → 渠道 MCP server | 同上 |
| 对话（ABB ↔ 智能体） | ACP | ABB ↔ agent | 换渠道**无影响** |

> agent 仍然可以自己连同一个 MCP server 调工具（`tools/call`）——那是**工具链**，
> 与上表两条**渠道链**是不同用途的独立连接。

## 3. 渠道 MCP 服务契约（本仓对外定义的部分）

一个「渠道服务」就是一个普通 MCP server，除了 MCP 通用要求外只需满足：

### 3.1 出站（必须）

| 能力 | 形式 | 说明 |
|---|---|---|
| 发文本 | 工具 `send_text`，参数 `{ chat_id, text }` | 最小必需 |
| 发会话内话题回复 | 工具 `send_thread_reply`，参数 `{ chat_id, message_id, text }` | 可选；缺失时回落 `send_text` |

### 3.2 入站（二选一）

| 方案 | 形式 | 适用 |
|---|---|---|
| **A. 资源订阅**（首选） | 资源 `channel://inbox`（或 `channel://<chat_id>/inbox`）+ `notifications/resources/updated` | server 支持 resources |
| **B. 工具轮询**（兜底） | 工具 `poll`，参数 `{ cursor?, limit? }` → `{ messages, next_cursor }` | 不支持 resources 的 server |

ABB 启动时先试 A（`resources/list` 里有 `channel://` + `resources/subscribe` 成功），
失败则退 B。**两者都不可用 ⇒ 该渠道判不可用并明确报错**（不静默降级成「收不到消息但显示在线」）。

### 3.3 入站消息形状（统一）

```json
{ "chat_id": "…", "message_id": "…", "sender_id": "…", "sender_name": "…",
  "text": "…", "ts": 1793612338,
  "attachments": [ { "kind": "image|file", "name": "…", "url_or_path": "…" } ] }
```

字段语义照 `crate::messenger::Messenger` 既有约定（`chat_id` 是渠道内会话键）。
**ABB 不认识渠道差异**：任何在 `mcp.json` 里声明、满足本契约的 server 都能接。

### 3.4 可选能力（缺失即回落）

`get_quoted_message` / `get_chat_info` / `chat_brief` / `create_chat` / `typing` / `done` /
`user_display_name` —— 一一对应 `Messenger` trait 的默认方法，缺失时用 trait 默认实现。

### 3.5 附件（可选，缺失即只能发文本）

工具 `send_attachment`，参数：

```json
{ "chat_id": "…", "name": "报表.csv", "mime": "text/csv",
  "kind": "image|file|video", "data_b64": "<标准 base64>" }
```

**为什么是 base64 内联而不是路径引用**：ABB 与渠道 server 通常是同机子进程，
两段式上传（预签名 URL）对这个场景过重；而路径引用要把 ABB 的文件系统暴露给 server。
这是 Jev 裁定 `base64_inline`（p=0.78 / conf=0.67——它自己也不确定）。

**体积上限（重要）**：单帧 JSON 上限 `mcp_client::MAX_LINE_BYTES` = **32 MB**，
base64 膨胀 33% ⇒ 原文件上限 `MAX_ATTACHMENT_BYTES` = **20 MB**。

ABB 侧会在**发送之前**做预检并给出「附件太大：X MB，上限 Y MB」，
而不是让底层抛笼统的 `LineTooLong`（那个错误与「渠道不支持附件」难以区分）。

缺这个工具时，`Messenger::send_attachment` **显式失败**并说清「该渠道只能发文本」
—— 不假装送达（历史教训：旧默认实现把路径当文本发出去还返回 `Ok`）。

## 4. ABB 侧的职责边界

| ABB **负责** | ABB **不负责** |
|---|---|
| 连接渠道 MCP server、订阅、收事件 | 各渠道的鉴权、协议细节、限流策略 |
| 去重 / 节流 / 开回合 / 投递 | 渠道账号与凭证的保管 |
| 告诉用户「要接某渠道该装什么、怎么配」 | 决定用户该接哪些渠道 |
| 模型供应商配置；以及渠道的**通用**三格（command/args/env） | **任何渠道专属字段**（`WX_TOKEN` 之类——ABB 不认识它们，只原样转发） |

**「告知用户怎么配」是硬要求**，不是文档附属品：用户说「我要接飞书」时，abb 里的 assistant
必须能用自然语言讲清：装哪个 MCP server、`mcp.json` 写什么、缺哪些权限。这条能力要有测试。

### 4.1 怎么配一个渠道（ABB 侧）

配一个 MCP 渠道**只改配置**，不碰代码：

```jsonc
// config.json 里 bots[] 的一项
{
  "name": "我的微信",
  "kind": "mcp",                    // ← 新建 bot 的默认值
  "enabled": true,
  "mcp_channel": {
    "name": "wechat",               // 日志/报错里显示的名字
    "command": "/path/to/abb-channel-wechat",
    "args": ["--dir", "/tmp/wx-channel"],
    "env": [
      { "name": "WX_TOKEN",    "value": "…" },
      { "name": "WX_BASE_URL", "value": "https://ilinkai.weixin.qq.com" },
      { "name": "WX_USER_ID",  "value": "…" }
    ]
  }
}
```

界面里对应「机器人配置」页选一个 bot、类型选 `mcp`，然后填三行：
**可执行文件** / **参数** / **环境变量**（每行 `KEY=VALUE`）。

**渠道配置由提供 MCP 的一方决定**：ABB 不认识 `WX_TOKEN` 是什么，它只是把 `env`
原样传给子进程。所以接一个新渠道不需要改 ABB——写一个新的 MCP server 就行。

#### 三个已有渠道的配置对照

每个渠道的 `env` 由它自己的 server 定义（ABB 只转发）。实测的完整清单：

| 渠道 | `command` | `args` | `env` |
|---|---|---|---|
| 微信 | `abb-channel-wechat` | `--dir <二维码/登录态目录>` | `WX_TOKEN`、`WX_BASE_URL`、`WX_USER_ID`（可选 `WX_CDN_BASE_URL`） |
| 飞书 | `abb-channel-feishu` | 无 | `FS_APP_ID`、`FS_APP_SECRET` |
| 钉钉 | `abb-channel-dingtalk` | 无 | `DT_APP_ID`、`DT_APP_SECRET`、`DT_ROBOT_CODE` |

**为什么只有微信用 `--dir`**：微信要扫码登录，二维码 PNG 需要一个落地目录
（`login` 工具把图写到那里、路径打到 stderr）。飞书与钉钉用 `app_id`/`app_secret`
换 `tenant_token` / Stream 凭证，不需要本地目录 ⇒ **它们忽略命令行参数**
（传了也不报错，但不起作用）。

**微信的特殊之处：凭证不是一开始就有的。** 流程是：

1. `mcp_channel` 只配 `command` + `args`，`env` 先留空；
2. 调 `login` 工具 → 二维码写到 `--dir`；
3. 扫码后调 `login_status` → `confirmed` 时 token 打到 **stderr**（不放进工具结果，
   因为工具结果会被 ABB 落进会话历史）；
4. 把 stderr 里的三项填进 `mcp_channel.env`，重启 bot。

飞书/钉钉没有这一步——凭证来自开放平台后台，直接填 `env` 即可。

#### `owner_ids`：谁是管理员

MCP 渠道的 `sender_id` 是**渠道服务自报**的字符串，ABB 没有任何平台侧鉴权手段。
所以**默认全部受限**（fail-closed）——「接一个渠道」绝不等于「自动交出全权限」。

要恢复管理能力（免 @ 开关、`/trash` 回收站），必须在配置里显式声明：

```jsonc
"mcp_channel": {
  "name": "feishu",
  "command": "/path/to/abb-channel-feishu",
  "owner_ids": ["ou_abc123", "ou_def456"],   // ← 谁是管理员
  "env": [ … ]
}
```

**四个必须知道的语义**：

| 行为 | 说明 |
|---|---|
| 空/不写 `owner_ids` | **全部受限**（默认，安全侧） |
| 名单含该 `sender_id` | 该发送者是 `Owner`（全权限） |
| 匹配**大小写敏感** | `Alice` ≠ `alice`（sender_id 是标识符，不是人名） |
| 空 `sender_id` **永不算 owner** | 哪怕名单里也写了空串（那是畸形配置） |

**怎么找到自己的 `sender_id`**：随便发一条消息，看 ABB 日志里 `[bridge] 收到消息 … sender=`（或渠道 server 的 stderr）。
填进去、重启 bot 即生效。

#### 从内置渠道迁到 MCP 渠道

**内置渠道已于 2026-11-04 退役**（`src/{feishu,wechat,dingtalk,ws,proto}.rs` 全部删除）。
所以现在**只有 `kind = "mcp"` 能启动**：配置文件里留着 `kind = "feishu"` 的 bot
会在启动时报错并指到本节，不会静默降级。

**一条飞书 bot 的迁移步骤**：

1. 记下原 bot 的 `app_id` / `app_secret`（`config.json` 里那一项）
2. 新建一个 bot，`kind` 填 `mcp`，`mcp_channel` 填：
   - `command`: `abb-channel-feishu` 的绝对路径
   - `env`: `FS_APP_ID` / `FS_APP_SECRET`（就是第 1 步那两个）
   - `owner_ids`: 填你自己的 `sender_id`（否则管理功能不可用，见上）
3. 停用原 bot（`enabled: false`），启用新 bot
4. 发一条消息验证收发

**注意**：两个 bot 用**同一套凭证**同时跑会双收消息（同一个飞书应用只能有一条长连接）。
所以迁移时必须**先停旧的再启新的**，不能并行试。

**回滚**：**代码级回滚**，不是换配置。旧实现已删除，回滚要动 git：

```sh
git checkout pre-retire-builtin   # 退役前最后一个提交（tag 在仓库里）
```

这不是「把旧 bot 重新启用」那种轻量操作——之前本节写的就是那句，它是错的
（当时还没执行退役）。默认没有成本更低的回滚路径，所以**迁移前先确认新渠道能收发**。

**微信的差异**：微信 MCP 渠道的凭证来自扫码（`login` 工具），不是 `app_secret`；
且有 `--dir` 参数（二维码落地目录）。其余步骤相同。

### 4.2 已有的渠道实现

| 实现 | 位置 | 状态 |
|---|---|---|
| 目录渠道（参考实现） | `tools/channel-mcp-dir/` | ✅ 可用；`sh tools/channel-mcp-dir/verify.sh` 一条命令自检 |
| 微信 | `crates/abb-channel-wechat/` | ✅ 可用（`send_text` / `poll` / `send_attachment` / `login` / `login_status`） |
| 飞书 / 钉钉 | 仍在 `src/feishu.rs` / `src/dingtalk.rs` | 未搬；照微信的路径搬（见 §5） |

微信渠道的用法：

```sh
# 1. 起一个 bot，第一次先扫码
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"login","arguments":{}}}' \
  | abb-channel-wechat --dir /tmp/wx
# 二维码在 /tmp/wx/wechat-qr.png，用微信扫

# 2. 轮询状态，confirmed 后 token 会打到 stderr
echo '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"login_status","arguments":{}}}' \
  | abb-channel-wechat --dir /tmp/wx

# 3. 把拿到的三项写进上面 config.json 的 mcp_channel.env
```

**token 过期时会发生什么**（实测，非推断）：微信侧返回 `errcode -14`，`poll` 工具回

```
poll 失败：ilink 会话超时（errcode -14），需重新扫码登录
```

即**明确说「要重新扫码」**，而不是返回空消息列表让人以为「没新消息」。

这条差别是可运维性的关键：前者你会去扫码，后者你会以为渠道正常但没人在说话。

## 5. 迁移分期（**减法在最后，且需 owner 授权**）

| 期 | 内容 | 是否可逆 |
|---|---|---|
| **S1** | ABB 内置 MCP client；新增 `McpChannelMessenger`（`impl Messenger`），`bot.kind = "mcp"` | 纯增量，可逆 |
| **S2** | 渠道契约稳定 + 至少一个真实渠道 MCP server 跑通（入站+出站） | 纯增量，可逆 |
| **S3** | 界面去掉渠道配置，只留模型供应商；渠道配置引导改由 assistant 自然语言输出 | 可逆（UI 回退） |
| **S4** | 内置 `feishu` / `wechat` / `dingtalk` 退役（`src/feishu.rs` 2206 行、`src/wechat.rs` 1640、`src/dingtalk.rs` 1764，另 `messenger.rs` 三个 impl 与 `bridge/` 里的耦合） | **不可逆，需 owner 逐项授权删除** |

现状量化（2026-11-02）：

```
src/ 总计 112,590 行
  src/feishu.rs        2206   ┐
  src/wechat.rs        1640   ├ 协议实现，S4 才动
  src/dingtalk.rs      1764   ┘
  src/messenger.rs       ~850  （含三个 impl；trait 本身保留）
  src/bridge/mod.rs     5520  （通用层保留，渠道耦合点待清）
  ui/app.slint          5892  （渠道配置面在 S3 移除）
引用渠道实现的文件：feishu 23 个 / wechat 13 个 / dingtalk 14 个
```

**S4 之前不删任何文件。** 前置条件是 S1–S3 完成且真实渠道跑通——否则是拿掉能用的去换没验证的。

## 6. 与本次 D68 改动的关系

两者正交、可叠加：

- D68（已完成）：**协作面**从本地 CLI 改为 MCP（这是仓库**开发流程**的事）
- 本文：**运行面**从内置渠道改为 MCP 渠道（这是**产品架构**的事）

共同点只有一个：都把 MCP 当成与「某一种实现」解耦的稳定接口。
