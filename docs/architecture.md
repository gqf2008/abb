# ABB 现状架构（可核对）

> **本文的每个数字都由 `tools/arch-check.sh` 从代码里读出来。** 改代码后跑它：
> 不一致会报错并指名哪一行过期（`bash tools/arch-check.sh`）。
> 这样图不会腐烂成"看着对但早就不是这样"。
>
> 最后核对：见本文末尾的「核对记录」。

## 1. 两个进程，各持一把单实例锁

```
              ┌─────────────────────────────────────────────────┐
  用户点图标 → │  agent-bridge（托盘 GUI，Slint）                 │  锁名 "gui"
              │  规模见 §6（本文所有行数只在那里给一次）           │
              │  职责：窗口、托盘、设置、系统对话框(syschat)      │
              └────────────────────┬────────────────────────────┘
                                   │ 看门狗每 2s 检查（logs/service.desired）
                                   │ spawn 子进程 `agent-bridge --service`
                                   ▼
              ┌─────────────────────────────────────────────────┐
              │  agent-bridge --service（无头桥守护）            │  锁名 "service"
              │  src/service.rs 2739 行                          │
              │  职责：连 IM、收消息、开回合、投递、定时任务      │
              └─────────────────────────────────────────────────┘
```

`single_instance.rs`：两把锁互不干扰 —— 所以 GUI 崩了 service 照跑（这是历史上
"进程还在但界面消失"那个 bug 的解释）。

另有一次性入口（都在 `src/main.rs`）：
`--camera-probe`（相机探测）、`--dump-tools`（随包工具自检）、CLI 子命令（`task`/`deliver`/`session` …）。

## 2. 四条独立的数据链

这是**最重要的一张图**。四条链的依赖方向不同，换掉任何一条不影响其它三条。

```
 ① 入站（渠道 → ABB → agent）
 飞书 WS ──┐
 微信轮询 ─┼─→ bridge.on_payload / on_weixin / on_dingtalk
 钉钉 Stream┘        │  src/bridge/mod.rs 537 / recover.rs 195,276
                     ▼
              去重 → 频道串行锁 → 排队
                     ▼
         runtime/pool.rs（按会话槽位选 agent）
                     ▼
         runtime/acp.rs session_prompt ──ACP(JSON-RPC/stdio)──→ 执行层
                                                              （随包 = ki）

 ② 出站（agent 结果 → 渠道）
 回合返回 → messenger.rs（`Messenger` trait，3 个实现）
            ├─ FeishuMessenger  (260 行起)
            ├─ WeixinMessenger  (439 行起)
            └─ DingTalkMessenger(553 行起)
         失败 → outbox.rs 落盘待补发（同一 chat 下次入站时 flush）

 ③ 主动投递（CLI / 定时任务 → 渠道）
 deliver.rs（2564 行）→ 自环判定 → outbox / 直发
 定时任务走 task_run.rs：两条执行通道 Schedule / Manual

 ④ MCP（两个方向，别混）
 ABB → agent：session/new 里**声明** mcpServers（ABB 只声明，不解释语义）
 ABB → 渠道 MCP 服务：**新方向，尚未接线**（src/mcp_bus.rs 只有契约与帧）
```

## 3. 真正存在的接缝（换实现只动这里）

| 接缝 | 位置 | 实现数 |
|---|---|---|
| `Messenger` trait | `src/messenger.rs` | 3（飞书/微信/钉钉）+ 2 测试假件 |
| 执行层命令解析 | `src/service.rs::resolve_acp_commands` | 随包优先 → PATH `ki` 回落 |
| ACP 协议层 | `src/runtime/acp.rs` | 1（手写 JSON-RPC，行数见 §6） |
| 渠道 MCP 契约 | `src/mcp_bus.rs` | 0（**只有契约，S1b 下半才接真管道**） |

## 4. 沙箱与准入（两个不同的东西，容易混）

```
准入（能不能跟 bot 说话）        沙箱（agent 能动多少）
config.rs access_allows           SandboxMode
  ├─ owner（白名单）                 ├─ Auto
  ├─ granted（授权码激活）            ├─ ReadOnly
  └─ 其它 → 拦截                     ├─ WorkspaceWrite
                                    └─ FullAccess（默认）
guard.rs 2673 行：删除保护 + 受限会话闸
runtime/acp.rs 把档位经 `_meta.abbSandbox` 下发；执行层不声明该能力时，
受限档会话在建会话处 fail-closed 拒建。
```

## 5. 运行时数据都在一个目录

```
$AGENT_BRIDGE_HOME 或 ~/.agent-bridge/        （main.rs:97 bridge_dir）
├─ config.json           凭证与 bot 配置（0600）
├─ logs/{service,gui,bridge,update}.out     日志
├─ workspaces/<bot_key>/                    每个 bot 一个工作目录
│   ├─ AGENTS.md / CLAUDE.md                约定链（GUIDE_MARKER 版本化）
│   └─ <sanitize(chat_id)>/
│       ├─ mcp.json                         渠道级 MCP 声明
│       └─ sessions.json / history/
└─ tasks/<bot_key>/{tasks,tasks-state}.json 任务定义与运行态
```

## 6. 代码规模（了解重心在哪）

> **本文的规模数字只在本节给一次**，别处只指路。这样同一个数字不会出现两个值。
>
> 取当前值：`bash tools/arch-check.sh --print`（它从代码里读，手抄必错）。
> `bash tools/arch-check.sh` 会断言本文与代码一致，不一致就红。

| 分区 | 行数 |
|---|---|
| `src/` 顶层 | 见下表 |
| `src/runtime/`（从上游移植的会话层）**全目录 13727** | 主要 5 个：acp 4888 / harness 4332 / pool 2173 / queue 1536 / oneshot 571；其余 4 个小文件合计 227 |
| `src/` 合计 | **114075** |
| `ui/app.slint` | 5892 |
| GUI 三件合计（ui.rs + syschat.rs + 渲染台） | **25549** |

最大的六个文件：`ui.rs` 12297、`syschat.rs` 7664、`ui_headless_render_test.rs` 5588、
`task_run.rs` 4563、`platform.rs` 4096、`main.rs` 3444。

> **重心在 GUI**：`ui.rs` + `syschat.rs` + 渲染台三件（见上表）比整个 `src/runtime/` 全目录还大。
> 所以「ABB 是个桥」这个说法在代码量上不成立：它一半是桌面应用。

## 7. 现在缺什么（诚实清单）

| 缺 | 状态 |
|---|---|
| ABB 自己连渠道 MCP | 只有契约（`mcp_bus.rs`），没有真管道 |
| 界面配置渠道 | 还在（`ui.rs` 里有渠道配置面），S3 才移除 |
| 内置渠道退役 | **没动**。feishu 2206 / wechat 1640 / dingtalk 1764 行全在 |
| 渠道 MCP 服务的真实样本 | 0 个。契约没有经过任何真实 server 验证 |

## 核对记录

```
2026-11-02  首次建立。由 tools/arch-check.sh 生成数字。
```
