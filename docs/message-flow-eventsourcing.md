# 消息流事件化：pending 升级评估与阶段划分

参考：deepseek-harness（dsh）的「消息流（append-only SessionEvent 日志）与推理流（agent-loop 派生）分离」设计。本文档记录 ABB 对照评估结论与实施进度。

## 现状语义（评估基线，2.11.0）

pending 是「单条 in-flight 快照 + 重跑」模型，两个崩溃窗口：

```
add(pending) ── agent run ── set_reply(回复落盘) ── send ── remove
    W1: 此区间崩溃                      W2: 此区间崩溃
```

| 窗口 | 现状（阶段 1 前） | 阶段 1 后（已实现 2026-08-17） |
|---|---|---|
| W1（add 后、回复落盘前崩溃） | 重放重跑 agent（at-least-once 双发副作用） | 同（副作用已幂等：history (mid,user) 去重、mark_started 身份校验、pending mid 去重） |
| W2（回复产出后、发送前崩溃） | **回复静默丢失**（remove 在发送前） | **重启补发不重跑**（PendingItem.reply 落盘；补发成功才 remove） |
| 发送成功后 remove 前崩溃 | 回复已发（无窗口概念） | 重启补发一条重复回复（at-least-once 仅重发文本，严格优于重跑；用户可见双发，接受） |
| Cancelled/Err 臂 | 先 remove 再发送（基线） | 同（恢复基线：先摘 pending——remove 若在发送后，崩溃会让已叫停任务被重启续跑，违背叫停不变式） |

三臂时序：**Reply** = set_reply → send → Ok 才 remove；**Cancelled/Err** = remove → 发通知。

## 阶段 2（GitHub 双写幂等）——不适用

评估时针对 GitHub 双写评论的 at-least-once 双发。2.11.0 已全摘 GitHub 集成（PR #73，`src/github.rs` 删除），**跳过**。若未来重接 GitHub，幂等键 = `mid+repo+number`（post 前查已发记录）。

## 阶段 3（完整事件日志）——评估结论：当前收益小，不实施

dsh 式事件溯源（`msg/received` / `agent/started` / `agent/completed` / `delivery/sent` 事件 + 恢复状态机 + checkpoint 裁剪）评估：

**收益**：
- W1 重跑收敛为「不重跑」——但 ABB 后端 resume 语义各异（codex 需真实 tid、pi 无 resume、prime 同 codex 式），「续跑」不可靠；不重跑 = 用户问题不被回答，对 IM 场景更糟
- 副作用去重——**2.11.0 已全部幂等**（见上表），无增量
- 唯一实质收益：架构对齐 dsh（未来扩展面），非功能收益

**代价**：pending.rs → eventlog.rs 重构（append-only jsonl + 序号 + 批窗 + checkpoint 裁剪）、handle 落盘点 4 处、recover_pending 状态机重写、旧 pending.json 迁移、热路径写放大（需 flush checkpoint 合并）。估算 400-600 行 + 测试重写。

**结论**：收益/成本比不成立，**不实施**。替代方向（若未来需要）：history.rs（#49 的摘要日志）升级为事件派生——注入时从事件精确重建上下文而非 300 字截断摘要，是 dsh 式架构在 ABB 更有价值的第一落点。

## 实施记录

- 2026-08-17：阶段 1 完成并推送 main（`54821df` 实现 + `1d8d68f` 审查跟进；268 测试全绿，CI 绿）
- 审查发现并修复：Cancelled 臂 remove 位置回归（叫停不变式）、发送失败/补发不对称判据注释、thread 补发测试、恰发一条断言
