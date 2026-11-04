# 内置渠道退役（S4）——**已于 2026-11-04 执行完毕**

> 状态：**已执行**（2026-11-04，分支 `retire/builtin-channels`）。
> 本文保留为**执行记录 + 缺口台账**——步骤部分已成历史，末尾的「已确认的功能缺口」
> 是当前仍有效的部分。
>
> 回滚：整批改动在 `git tag pre-retire-builtin` 之前的 `main` 上完好，
> `git checkout pre-retire-builtin` 即回到退役前状态。

## 退役前的现状（历史记录，这些文件**已删除**）

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

## 功能缺口 —— **2026-11-04 当日全部补齐**

退役当天下午发现 7 项能力随渠道实现一起消失。到当晚全部补完，逐项列证据：

| 缺口 | 修法 | commit | 测试/变异 |
|---|---|---|---|
| **改群资料** `update_chat` | 契约新增工具（底层早就在，只是没暴露） | `fa56a8a` | crate 声明面断言 |
| **解散群** `delete_chat` | 同上 | `25ae515` | 同上 |
| **查 bot 身份** `bot_info` | 同上（GUI「拉取」按钮恢复） | `1e63447` | 同上 |
| **引用/回复内容** | 契约补 `ChannelMessage.parent_id`，渠道产出、ABB 回查 | `b174c34` | 渠道侧 3 条 + 变异 2 红 |
| **未读提醒** | `unread.report` 补回，MCP 入站时写 | `e225009` | 2 条 + 双向变异 |
| **授权码** | 消费端接回（去掉 `is_p2p` 判据，安全性靠「不匹配即 NotFound」） | `11464b0` | 1 条 + 变异 |
| **准入白名单** | 不在 `owner_ids` 的消息按退役前语义丢弃，`allow_ungranted` 可放开 | `f8b0da8` | 4 条 + 变异 |

### 三处判据/语义的变更（不是简单恢复）

1. **`parent_id` 是新增契约字段**——退役前飞书的 `parent_id` 只在内置入口内部消化，
   MCP 路径需要它**穿过总线**，所以必须加进契约。加了字段却没让渠道产出，
   曾经导致「引用功能整个是死的」（评审 blocking，见 `b174c34` 的说明）。
2. **未读提醒不再判 `chat_type == "p2p"`**——MCP 的 `chat_type` 恒为 `"mcp"`。
   依据是「渠道 server 已过滤未 @ 的消息」（Jev `always_notify` p=0.94/conf=0.92）。
3. **准入白名单默认丢弃**——这与退役前一致，但**默认值本身是个取舍**：
   静默扩大用户 bot 的暴露面比多一个字段危险。要放开写 `"allow_ungranted": true`。

### 仍然存在的边界

- **群管理工具的实际覆盖**（2026-11-04 逐格核对，不是估计）：

  | 工具 | 飞书 | 钉钉 | 微信 |
  |---|---|---|---|
  | `create_chat` | ✅ | ✅ | —（无建群 API）|
  | `get_chat_info` | ✅ | ✅ | —（无群概念）|
  | `update_chat` | ✅ | ✅（`d80b042` 补）| — |
  | `delete_chat` | ✅ | —（平台无解散 API，退役前也没有）| — |
  | `bot_info` | ✅ | — | — |

  调未声明的工具会拿到「服务未声明 `xxx`」的**明确报错**，不是静默失败。
- **`d80b042` 之前这里写的是「只有飞书实现」——那句话不准**：钉钉的建群/查群一直都在，
  而它的 `update_chat` 是**漏搬**（底层搬了、工具没暴露），不是能力边界。
  这个错误被用户的一句追问发现，说明「边界」和「遗漏」必须逐格核对，不能凭印象归类。
- 准入白名单在 **ABB 侧**判（`admit_by`），渠道 crate 不实现。所以一个直连渠道
  server 的客户端不受它约束——这是「准入归渠道」与「准入归 ABB」之间的现存缝隙。

## 契约可补的能力（建议顺序）

按「用户可感知程度」排，不是实现难度：

1. `update_chat` / `delete_chat` —— 虚拟 Bot 全生命周期（现在只能建，不能改/删）
2. 群生命周期事件（`chat_deleted`）—— 恢复自动注销登记
3. `sender_allowlist` —— 恢复「陌生人静默丢弃」
4. 未读提醒的触发点 —— 需要明确是 ABB 侧算还是渠道 side 报

## 不做什么

- **不删 `attachments.rs` 的渠道分支**：它是入站附件的落盘逻辑，MCP 渠道也要用（只是 `source` 字段值不同）。
- **不删 `bridge/` 的渠道相关代码**：`Ev` 是渠道无关的，MCP 渠道也在用。
- **不改 `SenderRole` / 准入逻辑**：与渠道实现无关。

## 授权方式

owner 说「做阶段 N」即可，不必一次批准全部。建议至少先满足前置条件 2 和 3。
