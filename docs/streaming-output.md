# 流式打字机输出（issue #42）

> 结论日期：2026-08-13。
> 飞书/钉钉 bot 可选「流式打字机输出」：agent 多轮输出累积进**同一条消息原地更新**；
> 关掉则任务完成后只发最终结果。**微信恒为只发最终结果**（需求方拍板，无开关）。

## 1. 行为矩阵

| 通道 | 开关（设置 → bot → 流式输出） | 中途输出 | 最终结果 |
|---|---|---|---|
| 飞书 | 开（默认） | 打字机卡片原地滚动 | 同一条卡片定格 |
| 飞书 | 关 | 不发 | 单条消息 |
| 钉钉 | 开（默认） | AI 卡片流式原地滚动 | 同一张卡片定格 |
| 钉钉 | 关 | 不发 | 单条消息 |
| 微信 | 无此开关 | 不发 | 单条消息 |

例外与回落：

- **飞书话题消息**：卡片实体发不进话题（create 接口不支持 thread），话题内保持逐条回复中途输出（现状行为）。
- **开局失败回落**：飞书未开通 cardkit 权限、钉钉未配置卡片模板 ID 时，该任务自动回落为逐条发送中途输出，日志给出配置提示；不会报错崩任务。
- **收尾失败回落（PR#43 审查 M1）**：最终全文更新未送达（限流/网络/token 竞态）时，权威最终结果回落为独立消息发出——用户不会看到停在中途进度的「假完成」卡片；平台侧卡片等 10 分钟闲置自关，期间显示近终态内容。
- **更新节流**：500ms 最小间隔（钉钉平台下限 ~500ms；飞书单卡 10 次/s）。间隔内的输出攒着，下次更新/收尾时一起上屏。
- **停止词打断**：流式消息定格为「⏹ 已停止」（带上已输出的中途内容）。
- 定时任务结果单条推送，不受开关影响。

## 2. 飞书：开通 CardKit 权限

流式打字机用飞书 CardKit 流式卡片（创建实体 → 发消息 → PUT 元素内容累积更新 → 关流式）：

1. [飞书开放平台](https://open.feishu.cn) → 进入你的应用 → **权限管理**，搜索并开通：
   - `cardkit:card:write`（更新卡片实体）
   - `im:message:send_as_bot` 应该已开（发消息基础权限）
2. **发布新版本**（权限变更必须发版才生效）：版本管理与发布 → 创建版本 → 提交发布（企业自建应用管理员审批后即生效）。
3. 无需改 ABB 配置：开关默认开，权限到位后打字机自动生效；未开通时自动回落逐条发送。

平台限制：卡片实体仅可发送一次；流式闲置 10 分钟自动关闭（ABB 任务结束会主动关）；流式卡片不可转发；需飞书客户端 7.20+。

## 3. 钉钉：创建 AI 卡片模板

流式打字机用钉钉互动卡片高级版（`createAndDeliver` 投放实例 → `PUT /v1.0/card/streaming` 累积更新 → `isFinalize` 收尾）：

1. [钉钉开放平台](https://open.dingtalk.com) → 进入你的企业内部应用 → **互动卡片**（卡片平台）→ 新建模板：
   - 模板里放一个 **markdown 组件**，变量名必须是 **`content`**（ABB 按这个名字写内容）。
   - 保存后复制**模板 ID**（形如 `xxxxx-xxxx-xxxx.schema` 或数字 ID）。
2. ABB 设置窗 → 选中钉钉 bot → **卡片模板 ID** 填入 → 保存（自动重启服务）。
3. 留空 = 流式不可用，自动回落逐条发送。

真机联调待确认项（实现已按腾讯 WeKnora 生产代码逐字段核对；任一环节不符都会走回落，不崩任务）：

- 投递体的 `userIdType` / `callbackType` 不在 Apifox schema 字段列表（见于官方示例/联调文），需真机确认回包不报参数错。
- 官方建议流式 content 单请求 ≤1KB、总量 ≤3KB；coding agent 回复常态超限。若平台硬限制，长回复的 update/finalize 会失败并走「收尾失败回落」（最终结果仍独立送达）。后续视真机结果考虑截断或超阈值直接回落逐条。

## 4. 实现索引

- 配置：`config.rs` `BotConfig.stream_output`（默认开）/ `ding_card_template_id`；`stream_output_enabled()` 是单一事实源（微信恒 false）。
- 抽象：`messenger.rs` `stream_begin/update/finalize`（微信默认 None 不支持）。
- 飞书：`feishu.rs` `card_create_streaming/card_send/card_update_content/card_close_streaming`（CardKit 四步，sequence 由 messenger 侧 per-card 递增）。
- 钉钉：`dingtalk.rs` `card_create_and_deliver/card_streaming_update`（单聊 IM_ROBOT / 群聊 IM_GROUP 两种投递模型）。
- 编排：`bridge.rs` `on_progress`（门控 + 累积 + 节流 + 回落）与 `finalize_stream`（Reply/Cancelled/Err 三态收尾）；run 结束先排空 progress 通道再收尾，保证每轮输出不丢。
