# review 严重度契约行（审查收敛门禁）

> 卡：`abb-review-cadence-gate-20261013`。把 `~/.agents/rules/RULE_审查规范.md` 第 7–12 条
> （严重度分档 / 一次一批一审 / 复审只覆盖上轮 blocking / 同一 blocking 第 2 轮未决即升级架构）
> 从人核变成**机器可查**。规则正文不改，本文件只定义可执行契约。

## 契约行

每一条**判决类** `review` 条目（`decision` 为 `approve` 或 `request_changes`）的
`note` **第一个非空行**必须是：

```
findings: blocking=<N> non-blocking=<M>
```

- `blocking`：阻断合并、必须本轮解决的 finding 数（Critical / Important 级）。
- `non-blocking`：不阻断合并、转 follow-up 的 finding 数（Minor 级）。
- 行内两处都是十进制整数，`blocking=` 与 `non-blocking=` 之间至少一个空白。

其余正文照旧写在契约行之后（人读）。

### 可选：逐条标签

如果想逐条标注，用 `- [blocking] …` / `- [non-blocking] …`。**用了就必须自洽**：
标签计数必须与契约行数字相等，否则判红。

### 正确样例

```
findings: blocking=1 non-blocking=2

- [blocking] src/ui.rs:42 断绑后不再更新（B1）
- [non-blocking] 注释里的行号已漂移
- [non-blocking] 变量名 a→authenticated

证据：`bash tools/gates.sh` 全绿；断绑复现脚本见 thread 第 3 条。
```

## 判据（`tools/check_review_cadence.sh` 实现）

1. **契约与 decision 一致**：`blocking>0 ⇒ request_changes`；`blocking=0 ⇒ approve`；
   若另有逐条标签，标签计数须等于契约行数字。
2. **blocking 轮数 ≤ 2**：同一 thread 里契约行 `blocking>0` 的 review 条数不超过 2
   （第 2 轮仍未决 ⇒ 升级为架构问题，不再开第 3、4 轮）。
3. **合并前收口**：存在 `merge_result{"merged":true}` 时，其前最后一次 blocking 轮之后
   必须有一个 `blocking=0` 的 `approve`。

`decision=="comment"` 的 review 不参与上述判定（既不要求契约行，也不计 blocking 轮）。

## 历史条目

契约自本文件落地之日生效。**不改写历史 review 条目**（append-only）：
`ts` 小于契约阈值的 review 视为历史，缺契约行时**跳过并计数、不判红**。

阈值常量在 `tools/check_review_cadence.sh` 的 `CONTRACT_TS_DEFAULT`，
可用 `--contract-ts <epoch>` 或 `ABB_REVIEW_CONTRACT_TS` 覆盖（自测即用它把样本放在阈值前后）。

## 怎么跑

```sh
# 对一条在飞 thread（合并前由协调者执行）
tools/check_review_cadence.sh <thread-id>

# 离线（自测/样本）
tools/check_review_cadence.sh --json tools/review-cadence/samples/ok_basic.json --contract-ts 1000000
```

退出码：`0` 通过；`1` 违规；`2` 用法/工具错误（缺 jq、thread 取不到、JSON 非法）。

守卫自身的自测（合规 + 违规 + 守卫失效模式，共 17 例）随仓提交，并在
`.walgit/ci.toml` 的 `review-cadence-selftest` 任务里执行：

```sh
tools/review-cadence/selftest.sh
```

## 合并前检查（协调者）

协调者在写 `merge_result` 之前，必须对目标 thread 跑一次
`tools/check_review_cadence.sh <thread-id>`；**红则不得合并**。
本检查需要 thread id，CI 任务拿不到该上下文，故不进 `ci.toml` 的每次提交门禁
（判断依据：`RULE_可达性.md`——无法被自动触发的检查如实登记为流程步骤，并在此写明）。
