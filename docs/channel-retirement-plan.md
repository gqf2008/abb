# 内置渠道退役方案（S4）——**未执行，等 owner 授权**

> 状态：**未开工**。本文只是把代价和步骤写清楚，让授权时不需要再探索。
> 授权粒度：本方案按「阶段」拆分，每个阶段可单独批准/拒绝。

## 现状

| 模块 | 行数 | 被哪些文件引用（不含自身） |
|---|---|---|
| `src/feishu.rs` | 2206 | ui.rs、bridge/mod.rs、dingtalk.rs、messenger.rs、wechat.rs |
| `src/dingtalk.rs` | 1764 | ui.rs、bridge/mod.rs、bridge/recover.rs、service.rs、messenger.rs |
| `src/wechat.rs` | 1640 | ui.rs、bridge/mod.rs、bridge/recover.rs、service.rs、messenger.rs、attachments.rs |
| `src/ws.rs` | 276 | service.rs |
| **合计** | **5886** | |

外加：
- `messenger.rs` 的三个 `impl Messenger`（FeishuMessenger / WeixinMessenger / DingTalkMessenger）
- `service.rs::run_bot` 的三条事件循环分支（`ws_loop` / `weixin_loop` / `dingtalk::stream_loop`）
- `config.rs` 的渠道凭证字段（`app_id`/`app_secret`/`wx_*`/`ding_*`/`owner_open_id`）
- `ui/app.slint` 的渠道表单（20 处 `kind == "feishu|wechat|dingtalk"` 条件块）

## 前置条件（**2026-11-03 已满足第 1、2 条**）

1. ✅ MCP 总线可用，且有可用渠道（`tools/channel-mcp-dir/` 参考实现 + 全链路测试）
2. ✅ **三个内置渠道各自有了等价的独立实现**（不是参考实现，是真实搬迁）：

| 渠道 | 独立实现 | 入站方式 | 与内置的差异 |
|---|---|---|---|
| 微信 | `crates/abb-channel-wechat/` | `poll` | 无 |
| 飞书 | `crates/abb-channel-feishu/` | `subscribe` | 无 |
| 钉钉 | `crates/abb-channel-dingtalk/` | `subscribe` | 无（`botstatus` 上报改由 ABB 侧做） |

**三条跨包 e2e 在门禁里都有真凭据**：

```
✅ 微信跨包 e2e 真的跑了（Poll 选路）
✅ 飞书跨包 e2e 真的跑了（Subscribe 选路）
✅ 钉钉跨包 e2e 真的跑了（Subscribe 选路）
```

这证明 ABB 的 `negotiate` 在**三种真实能力组合**下都能正确选路。

3. ❌ **owner 亲手验证过**：跑一次真渠道（扫码登录 + 收一条 + 发一条），确认与内置实现行为一致。

**第 3 条现在是唯一的阻塞项**，而且它只能由 owner 做——我无法代替「用户觉得这样能不能用」。

## 前置条件 2 的满足意味着什么

之前不敢退役，是因为「删掉能用的代码换一个理论上能接的总线」是净损失。
现在三个渠道都有**等价的独立实现**，退役不再是「失去功能」，而是「换进程边界」。

但仍有 residual risk 需要 owner 判断：
- 独立 server 多了进程管理（崩溃/重启/僵尸）
- 配置文件从 `config.json` 的字段变成 `mcp_channel.env`（迁移成本）
- 内置实现里的边缘行为（如飞书的群管理、钉钉的 @ 逻辑）是否在搬迁中完整保留

## 分阶段步骤（建议顺序）

### 阶段 0：只删死代码，不动行为（**可逆**）
- `src/ws.rs` 只有 `service.rs` 一个引用点 → 若飞书退役则一并删；否则保留。
- 风险最低，收益 ~276 行。

### 阶段 1：退役**一个**渠道（建议先钉钉，行数居中且用户最少）
1. 删 `src/dingtalk.rs`
2. 删 `messenger.rs::DingTalkMessenger` 及其 `impl Messenger`
3. `service.rs`：删 `is_dingtalk()` 分支
4. `config.rs`：把 `ding_*` 字段标 `#[serde(default)]` 保留（**不删字段**——旧配置文件还得能读）
5. `ui/app.slint`：删钉钉的条件块
6. 迁移提示：`config.rs::load` 里检测到 `kind == "dingtalk"` 时，日志警告 + GUI 显示
   「该渠道已退役，请改用 MCP 渠道」

**关键**：旧配置**必须还能加载**，只是该 bot 不启动。删字段会让用户的 config.json 直接报错。

### 阶段 2/3：同法退役微信 / 飞书

### 阶段 4：清理
- 删 `config.rs` 里的渠道凭证字段（此时才真的删）
- 更新 `docs/mcp-bus-architecture.md` 的现状量化
- `arch-check` 数字大幅下降

## 每个阶段的门禁

- 八条门禁全绿（这是底线，不是充分条件）
- `cargo run -- --service` 能起来（旧 config 有已退役渠道时，只警告不崩）
- GUI 能打开、设置页能显示（不能留下引用了已删类型的 slint 代码）
- **旧配置文件可加载**（专门写一条测试：含 `kind=dingtalk` 的 config 能 parse 且该 bot 被跳过）

## 不做什么

- **不删 `attachments.rs` 的渠道分支**：它是入站附件的落盘逻辑，MCP 渠道也要用（只是 `source` 字段值不同）。
- **不删 `bridge/` 的渠道相关代码**：`Ev` 是渠道无关的，MCP 渠道也在用。
- **不改 `SenderRole` / 准入逻辑**：与渠道实现无关。

## 授权方式

owner 说「做阶段 N」即可，不必一次批准全部。建议至少先满足前置条件 2 和 3。
