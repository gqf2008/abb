# abb-agent

ABB 的 **ACP 执行层**，由 [rpi](https://github.com/bigfish1913/pi-rust) 的库实现，用于替代
`crates/buzz-agent`（自维护分叉，含 7.9k 行自持 LLM 层与 1.9k 行自持 agent loop）。

设计依据与决策记录：walgit 线程 **`abb-agent-rpi-acp-20261008`**。

## 为什么是独立包

root `Cargo.toml` 没有 `[workspace]`，所以本包与 `crates/buzz-agent` 同构：自带
`Cargo.lock`，root 的 `cargo fmt/clippy/test` 不会碰到它。构建与门禁一律走
`--manifest-path`：

```sh
cargo build  --manifest-path crates/abb-agent/Cargo.toml
cargo test   --manifest-path crates/abb-agent/Cargo.toml --locked
cargo clippy --manifest-path crates/abb-agent/Cargo.toml --all-targets --locked -- -D warnings
cargo fmt    --manifest-path crates/abb-agent/Cargo.toml -- --check
```

CI 侧对应 `.walgit/ci.toml` 的 `abb-agent-lint` 任务（**真的跑测试**，不是 `--no-run`）。

按仓库策略，构建产物不要落在启动卷上：

```sh
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target
```

## 与 buzz-agent 的语义差异（都是决策，不是遗漏）

| 面 | 处置 | 理由 |
| --- | --- | --- |
| OS 沙箱档位 | **不实现、不声明** `_meta.abbSandbox` | 权限是操作系统的事，agent 不碰 |
| `session/request_permission` | **不发问** | 授权约定由 `AGENTS.md` 承担 |
| `_meta.steering` | **声明 `supported: false`** | 本刀未实现 `_session/steering`；abb 那边该标志是写该方法的唯一闸门，报 true 只会让「回合中追加消息」多一次无效往返 |
| LLM 层 / agent loop | 交给 rpi（`rpi-ai` / `rpi-agent`） | 不自持 7.9k 行 `llm.rs` |
| 内置工具 | 第二刀已接 `rpi-tools`（`dev__*`，见下节） | `rpi-tools = "0.3.16"`，与参照物同源 |
| 输出上限 | 默认 **65_536**，认 `BUZZ_AGENT_MAX_OUTPUT_TOKENS` | 与 fork 默认值一致；写小了会变成**静默截断**（见下） |

`initialize` 因此**不含** `_meta.abbSandbox`：abb 侧 `parse_abb_sandbox_modes` 得到
`None` → `SandboxSupport::Unsupported`。这是**如实声明**——声明支持却不执行，正是
abb 注释里点名的安全事故形态。

## 进程形状

stdin 读一行、分发一行；**只有回合**（`session/prompt`）被派到独立任务，读循环始终可读。
`in_flight` 在**读循环内同步抢**（不是在被派出的任务里）——否则同一 burst 中紧随其后的
`session/cancel` 会被读循环先读到，而那时任务还没被调度，取消会被「无在途回合」分支丢弃
（`probes/cancel_same_burst.py` 就是这个复现：修前 0 间隔 6/6 丢弃，修后 8/8 收尾）。

取消的完整语义要注意一个库层事实：`Agent::abort()` 只作用于**当前 run 的 per-run
token**。若 cancel 在回合任务**被 poll 之前**就到，那一次 abort 会落空——所以回合循环里按
取消标记**每轮补发 abort**，并挂一个 100ms 的 tick 作固定唤醒源（卡在网络上时不会再有事件
来唤醒我们）。

## 环境契约（以 abb 侧 `src/agent.rs::buzz_provider_env` 真值为准）

| 变量 | 取值 / 作用 |
| --- | --- |
| `BUZZ_AGENT_PROVIDER` | `anthropic` 或 `openai`（`openai-chat`/`openai-responses`/`openrouter`/`deepseek` 是**配置 kind**，env 里一律归并成 `openai`） |
| `ANTHROPIC_API_KEY` | 必填（显式取用，不依赖 rpi 的 env 兜底） |
| `ANTHROPIC_BASE_URL` | 可选；自定义/自建网关，**填主机根**（如 `https://gw.example.com`）。缺省 `https://api.anthropic.com` |
| `ANTHROPIC_MODEL` | **必填**；任意 id（见下） |
| `OPENAI_COMPAT_API_KEY` / `OPENAI_COMPAT_MODEL` | **必填**（openai 家族） |
| `OPENAI_COMPAT_BASE_URL` | 可选；缺省 `https://api.openai.com` |
| `OPENAI_COMPAT_API` | `responses` 走 responses 端点，其余（含 `openrouter`/`deepseek` 两个预置）都是 chat |
| `BUZZ_AGENT_MAX_ROUNDS` | 回合上界（abb 默认送 200） |
| `BUZZ_AGENT_MAX_OUTPUT_TOKENS` | 可选；单回合输出上限。abb **今天不注入**，但被替代的 fork 会读它（默认 65536），故同样认 |
| `ABB_AGENT_FAUX_TEXT` | **本包自己的离线通道**：命中即用 rpi faux provider（不联网、无凭据），供 smoke 与测试 |
| `ABB_AGENT_FAUX_TOOL` | 离线通道之二：JSON `{"name":…,"arguments":…}`，先发一次工具调用再把 `ABB_AGENT_FAUX_TEXT` 当文本回复。用于无 key 验证「模型 → 工具 → 结果回灌」 |
| `RUST_LOG` | 日志级别（默认 `info`；**日志只走 stderr**，stdout 是 ACP 协议通道） |

⚠️ 两个 `ABB_AGENT_FAUX_*` 是**进程级覆盖**：只要环境里存在，就会**静默**把生产从真供应商切到
离线 faux（abb 起 agent 时只 `env(k, v)` 注入、不 `env_clear()`，所以外部导出的变量会一路透传）。
abb 主体零引用它们，但排查时先确认它们不存在。

### 模型 id 不查目录（重要）

模型是用 `Model::new(id, name, api, provider, base_url)` **显式构造**的，与 rpi 官方对
`models.json` 条目的做法（`rpi-cli/src/config.rs::provider_to_models`）一致：**id 由调用方
给，rpi 不限制命名空间**。所以自定义网关（one-api、自建反代…）用厂商原生 id 是正常用法。

模型的 `context_window` 保持 0（同官方对未声明条目的处理），但 `max_tokens` **必须给正数**：
`clamp_max_tokens_to_context` 在 `context_window == 0` 时返回 `max(1, max_tokens)`，
于是 `0` 会静默变成「只准回 1 个 token」。故默认 `8192`（`DEFAULT_MAX_OUTPUT_TOKENS`）。

模型/API key 缺失时**直接报错**（`session/new` 返回 -32000），不静默挑一个——被替代的
`crates/buzz-agent/src/config.rs` 在同路径就是 `"config: ANTHROPIC_MODEL required"` 硬失败。
静默替用户挑模型等于替用户花钱。

### base_url 语义（anthropic 与 openai 不一样！）

**anthropic：填主机根。** rpi 自己拼 `/v1/messages`（`format!("{base}/v1/messages")`），所以
`https://gw.example.com` → `/v1/messages`，而 `https://gw.example.com/v1` → **`/v1/v1/messages`**。
这与被替代的 fork 同构（它也是 `{base}/v1/messages`），不是回归，但是个陷阱：
单元测试一度拿 `…/v1` 当「可用网关」的样本，那是在示范一个会坏掉的形状（评审指出后已改）。

**openai：两边一致。**

abb 的预置端点是**带版本前缀**的（`openrouter` → `https://openrouter.ai/api/v1`，
`deepseek` → `https://api.deepseek.com/v1`），而 rpi 对 `/v1` 结尾只再补
`/chat/completions`（对 host 根才补 `/v1/chat/completions`）——所以能正确拼成
`https://api.deepseek.com/v1/chat/completions`。

## MCP（刀 1）

rpi **没有 MCP**（全仓零命中、`rpi-mcp` workspace 成员已被删除），而 abb 的 tool surface 主要
就是两个 MCP server：`abb-events`（= abb 二进制自身 `mcp-events` 子命令）与 `wassette`
（Wasm 组件工具宿主）。本包用 **`rmcp`**（与被替代的 `crates/buzz-agent` 同源）接上它们。

- 入口是 `session/new` 的 `params.mcpServers`（形状与 `src/buzz/acp.rs` 的 `McpServer` 逐字对应：
  `{name, command, args, env:[{name,value}]}`）。**第一刀连 `params` 都不读**，abb 送的 server
  列表被静默丢弃——现在会解析、`mcpServers` 形状不对则回 `-32005` 而不是当成空列表。
- 工具以 **`{server}__{tool}`** 的限定名暴露给模型（与被替代组件一致：其 `SEP` 即 `__`，
  且它同样拒绝 bare 名含 `__` 的工具），调用时回退到 bare 名发给 MCP。名字/描述/schema 的
  上界（名字 ≤128、限定名 ≤64、工具数 ≤128、描述 ≤1024B、schema ≤4096B）逐条对齐
  被替代组件；**一处有意差异**：那边遇到非法名字或重名是 `Err`（整轮 session 失败），
  这边是**记 ERROR 后跳过**（与 abb「一个坏插件不能阻断其余」同取向）。重名不再静默：
  第二个同名工具会打 ERROR 说明它永不可达（rpi 按 `find` 取首个匹配，重名不报错）。
- **装配有界**：单 server 20s（`initialize` 与 `tools/list` **合计**）、全体 30s（取小者）。
  **单个 server 连不上不拖垮整轮装配**，记 ERROR 后跳过。两个上界都落在 abb 的
  `session/new` RPC 预算（60s）之内；装配本身**不再占用读循环**（`session/new` 与回合一样
  走独立任务，否则 abb 那边只有 5s 宽限的「停止」会被拖过）。
- 子进程环境是白名单（`env_clear()` + [`PASSTHROUGH_ENV`] + abb 按 server 下发的
  `spec.env`）：否则每个 MCP server（含 wassette 这类第三方组件宿主）默认就能看到 abb 注入的
  供应商 API key。工作目录取会话工作区（`session/new` 的 `cwd`，不存在时**如实报 cwd 错**而不是
  把它读成「命令不存在」），server 的 stderr **继承**（落到 /dev/null 的话，「server 起不来」
  的原因只剩我们那句 spawn 错误）。
  **白名单是「可用性 vs 凭据面」的取舍**（与被替代组件同源）：`HTTP(S)_PROXY`/`ALL_PROXY`
  可能带凭据、`SSH_AUTH_SOCK` 会让子进程能用用户的 SSH agent——对「能出网、能用 git」是必需的，
  但意味着 wassette 这类第三方宿主也拿到它们。要收紧就得改 `PASSTHROUGH_ENV`。
- 连接的生命周期绑在 session 上（`RunningService` 一旦 drop 就会关掉子进程）。**例外**：本
  进程自己**优雅退出**（stdin EOF）时不成立——不理会 EOF 的 MCP 子进程会被留下（`PPID=1`）：
  关子进程的 kill 任务 spawn 在块 `block_on` 已返回的 runtime 上，没被 poll。
  （装配超时被放弃的子进程不属此列：实测在预算到期的**同一瞬间**就被 `kill()`，无窗口。）
  abb 的监督路径（`shutdown()` / `Drop` 都是 `killpg`）不受影响，单独启动本包时要自己收尾。
- MCP `isError: true` 走 `Err(AgentError::Tool)` 路径，让 loop 编码成错误工具结果（不伪装成功）；
  工具结果里的图片分两种情形处理，**都不会出现与事实不符的占位文本**：
  - **openai 家族**（chat / responses）：真发图（chat 已端到端抓包；responses 仅有源码依据，**未端到端验**）。
    这要求模型的 `input` 声明 `Image`（`provider.rs` 的 `custom_model` 已声明）——rpi 按 `Model::input`
    门控，不声明就被换成 `(see attached image)` 占位。
  - **anthropic**：目前**发不出去**（rpi-ai 0.3.16 把 `Content::Image` 硬编码成 `Text("(see attached
    image)")`，上游注释写明 emit image block 仍是 TODO）⇒ 本包自己降级成**如实**的一行文本
    （`[image 未传给模型：…]`），并把 `input` 保持为 `[Text]`。声明 `Image` 反而更差：那会把「如实
    说明被省略」换成「指着一张不存在的图」。上游修好后把 `delivers_tool_result_images` 翻真即可。
  单个工具结果受预算约束：**文本 + 图片合计** 8 MiB、**整条结果的文本** 50 KiB（两个额度都
  **累计**，与 fork 的 `used`/`text_used` 同义），超限中间省略；超预算的图片降级成一行说明；
  音频等 rpi 侧无对应 part 的类型同样留一行痕迹，不静默吞。

握手仍然「先装配、再应答」（abb 在 `session/new` 应答之后才发 prompt，工具必须在 prompt 之前
就位），但装配已经在独立任务里，读循环全程可读。

尚未做（刀 2 之后视需要）：`notifications/tools/list_changed` 的刷新、每个 `tools/call` 的独立超时、
断线重连、`session/close`（本包没有这个渠道：`session/close` 回 `-32601`，MCP 子进程随会话线性
累积——与被替代组件同构）。另注意：MCP 子进程的孤儿回收（abb 侧有 `src/orphan_mcp.rs` 专治
wassette 孤儿，且是 windows-only）本包未处理；被 abb 监督时靠 abb 的 `killpg` 兜住，单独启动或
外部单个 pid 被杀时会留孤儿（见上文「例外」）。

## 约定链（刀 2 首批）

`AGENTS.md` 逐级加载 + `~/AGENTS.md` 全局层，语义/上限/截断**逐条对齐**被替代的
`crates/buzz-agent/src/hints.rs`（同一份用户约定，换执行层不该换行为）：

- 链路 = **git 根 → … → 会话工作目录**（`session/new` 的 `cwd`，即频道工作区；**不是**进程 cwd
  ——同机器上不同频道本就该看不同目录的约定），再把 `$HOME/AGENTS.md` 作为**全局层插在最前**
  （home 已在链上时不重复插）；非 git 目录退化成「cwd + 全局层」；
- 上限 **128 KiB**、**按字符边界**截断（UTF-8 安全）；内容拼在 abb 下发的系统提示**之后**
  （与 fork 的 `format!("{base}\n\n{hints_text}")` 同序）；没有任何约定时不注入空标题；
  诚实说明：与 fork 一样是「**逐层留额度**」的写法，所以真实上界是 ≤128 KiB **+2 字节**（分隔符）；
- ⚠️ **`BUZZ_AGENT_NO_HINTS=1` 是硬前提**：abb 对 **granted（授权者）** 会话只能在**进程级**
  收口它（`src/service.rs:180-234`：hints 发生在 `session/new` 之前，per-session 的 `_meta`
  管不到）⇒ 本包不认它就等于**把 owner 的 `~/AGENTS.md` 静默泄漏给授权者**。
  读法**逐条对齐 fork**（`parse::<u8>()`，不 trim）：未设置/`0` ⇒ 开；**任何非零**（`1`/`2`/`01`/`+1`）
  ⇒ 关；**读不懂的取值 ⇒ 拒绝启动（exit 2）**，与 fork 的 `die()` 同款——这一条曾被写成
  「只认字面 `1`」，实测方向是**静默 fail-open**（`true`/`2`/`01` 会把 owner 的约定发给授权者），
  已按参照物改正。关掉时默认系统提示照旧（关的是约定链，不是提示）。
- `session/new` 不带 `cwd`（空串）时按**原值**处理：链退化成「进程工作目录那一层」，**不**向祖先链
  扩散——与参照物的**链函数**同行为（上一版用 `current_dir()` 回落，实测会多读进程 cwd 的 git 根到 cwd
  整条链）。abb 的生产路径恒下发非空 cwd（`src/buzz/pool.rs` 的 `session_cwd`），这条只是对齐边界。
  **三处如实登记的残差**（评审实测、当前均不可达）：①参照物在 `session/new` 层就**拒建**空/空白/相对
  `cwd`（`INVALID_PARAMS`），本包不拒、按上面那条读一层——若将来 abb 真下发空 cwd，两边行为不同；
  ②空白串在本包 `SessionNewParams::parse` 里被规范化为空串 ⇒ 对 `"   "` 读一层；参照物的链函数则把
  空白串当**相对路径**处理，其结果依赖进程 cwd（评审实测：进程 cwd 是 git 根时为 1 层，不是固定 0 层），
  即「空白串 ≠ 空串」这一点两边不同，方向随环境而变；③env 值非 UTF-8 时两边都因 `var().ok()` 落到默认值（= 开），这一格与参照物**相同**
  但同样是 fail-open 方向，未测。
- **白名单相对上一版（刀 2 首批）对 MCP 子进程是加宽**：共享白名单把 `EDITOR`/`VISUAL`/`PAGER`/
  `GIT_PAGER` 也给了 MCP 子进程（它们是非凭据的 git/编辑器变量，为内置工具的 shell 加的）。
  另外 `session/new` 的 `spec.env` 在标记之后应用、优先级更高 ⇒ 若 abb 将来在该处声明
  `ABB_AGENT_CONTEXT`（含设成空串）可覆盖标记；次序与参照物完全同形，abb 今天不下发它，如实登记。
- ⚠️ **已知边界（内置工具落地后已按工具面重核，结论如下）**：内置工具已经上线（`dev__read`/`dev__shell`
  等能主动读文件与跑命令），所以「`NO_HINTS=1` 只关**自动注入**、不关 agent 主动读」这条现在是**实的**：
  今天它不构成泄漏，靠的是 abb 的 P2.3 硬闸（未声明 `_meta.abbSandbox` 的 agent 一律不接受限会话）把
  granted 会话拦在门外，而不是靠本包的读面限制（本包读面只与参照物 FullAccess 档持平）。
  **若将来 abb-agent 声明档位**，必须连同 `read_roots`/写入 roots 一起实现，不能只补声明——这条仍未做，
  如实登记为待办（`src/task_proc.rs` 的 proc 闸同理：靠 `ABB_AGENT_CONTEXT` 标记，不是安全边界）。

### 技能与 `load_skill`（同一批落地）

系统提示按被替代组件的三段结构给出：`# Additional Instructions` → `## Project Hints`（`AGENTS.md`
链）→ `## Available Skills`（技能名 + 描述）→ 一行「用 `load_skill` 读正文」。两边都空时**不产出任何
标题**（不留空壳段落）。

- 发现（对齐 `crates/buzz-agent/src/hints.rs`）：`<cwd>/.agents/skills`、`.goose/skills`、`.claude/skills`
  + `~/.agents/skills`；`SKILL.md` 的 YAML frontmatter 取 `name`/`description`（**无 `name` 不成技能**）；
  **同名先发现的赢**；子目录自带 `SKILL.md` 的不再下钻（那是另一个技能）；支持文件全部预枚举。
- 读取（对齐 `crates/buzz-agent/src/builtin.rs`）：工具名是**裸名 `load_skill`**（参照物直接 push 进
  工具表，不经 `dev__` —— 改名会让技能正文里写的 `load_skill(name: …)` 提示失效）；`{name}` 取整个技能
  （**剥掉 frontmatter**、附 `## Supporting Files` 清单），`{name}/relative/path` 取支持文件；
  两者合计 **32 KiB** 上限（按字符边界截断）；找不到/越界给出带「可用技能/可用文件」清单的错误结果。
- **越界保护**：支持文件必须**已在预枚举清单里**，且读取前 canonicalize 后仍在**技能目录（发现时的
  真实路径）**内 —— 拦的是「支持文件本身是/被换成指向外面的符号链接」。**残留限制**（与参照物
  **同形**，已登记）：①整个**技能目录**被换成指向外面的符号链接；②支持文件是与外部文件**同 inode
  的硬链接**。这两条不拦，因为参照物的判据也只到这一层。
- **无技能时不暴露 `load_skill`**（参照物只在 `!skills.is_empty()` 时 push）；支持文件成功结果的
  **外框**与参照物逐字一致（`# Loaded: skill/rel → File loaded into context.`）；`{name}/`（相对
  路径为空）走**支持文件**分支并报错列出可用文件（`split_once('/')` 的语义）；越界拒绝
  （`refusing to load … resolves outside the skill directory`）与读取失败
  （`could not read "skill/rel": …`，**不回显绝对路径**）的措辞同样逐字对齐。
- **平台层差异（如实登记）**：`load_skill: missing required argument "name"` 这条在本包**端到端不可达**
  ——rpi 会先按 schema 校验拦下（模型看到的是 `schema error: Validation failed …`），本包这句只在
  直接调工具对象时出现（所有工具同款）。
- ⚠️ **`BUZZ_AGENT_NO_HINTS` 同时关掉技能清单与 `load_skill`**（参照物：hints 关时 skills 是空 vec ⇒
  工具拿不到任何技能）。**未实现**：参照物在受限档还有 `read_roots` 校验（技能常在 `~/.agents/skills`，
  在工作区之外）——本包不实现档位，与「授权责任不变」一致，见上文 granted 隔离待办。
- 与 MCP 工具**同名**时（比如某个 server 也叫 `load_skill`）：摘掉 MCP 侧并打 ERROR（rpi 按名字取首个，
  不处理就会静默遮蔽）。

## 内置工具（刀 2 第二批）

用 `rpi-tools`（`read`/`write`/`edit`/`bash`/`grep`/`find`/`ls` + `OsExecutionEnv`）补上被替代组件的
能力面：第一刀只有 MCP 工具（abb-events / wassette），模型**没有文件读写与 shell**。
`OsExecutionEnv::with_cwd(会话 cwd)` → `ExecutionToolContext` → `create_*_tool`，全部以
**`dev__{name}` 限定名**暴露（沿用被替代组件的 `dev` 伪命名空间，避免 owner 约定/技能里写过的
工具名静默失效）。

与被替代组件的差异（逐条登记，不假装同形）：

| 维度 | 被替代组件 | 本包 |
| --- | --- | --- |
| 集合 | **6 个**：`shell`/`read`/`write`/`ls`/`glob`/`delegate` | 前五个**同名**（`glob` 由 rpi 的 `find` 工具提供——它的入参就是 glob 模式）+ `edit`/`grep`（rpi 增量）；**`delegate`（子代理委派）本包没有** |
| shell 工具参数 | `{command, timeout_secs}` | 同名同形：schema 里把 rpi 的 `timeout` 改名成 `timeout_secs`，调用时再翻回去 |
| shell 超时 | 默认 120s、clamp 1..=600 | 同（`default_timeout=120`；模型给的 `timeout_secs` 也 clamp 进 1..=600，`0`/负数/非法值落回 120） |
| shell 的环境 | 白名单（凭据不进子进程）+ **写入 `ABB_AGENT_CONTEXT=1`** | 同：`inherit_env=false` + `child_env::passthrough_map()`（与 MCP 子进程共用同一份白名单），且**标记由我们写入**（abb 的 Q8 proc 闸以它为主判据：少了它「agent 派生的 shell 去建 `--proc` 任务」会 fail-open） |
| shell 截断后的全量输出 | — | rpi 会把全量输出写到临时文件（`full_output_path`）并把路径给模型；该文件在 `TMPDIR` 下实测 `644`/目录 `755`（TMPDIR 若是共享目录，同机其它用户可读）；登记未改 |
| `read` 的 `offset` | **0-based** | **1-based**（rpi 工具的语义；description 里写得明白，模型自洽） |
| 写入限定 | FullAccess 档也限定在会话 workspace（拒绝对路径与 `..` 逃逸，**并拒绝末段符号链接**） | **不放宽**：父目录先建后 canonicalize 再比前缀，末段是符号链接直接拒；`..`、符号链接父目录/文件、工作区外绝对路径一律拒 |
| 读的限定 | FullAccess 档允许绝对路径；受限档按 `read_roots` | 与 FullAccess 一致；**受限档的 roots 策略仍由 abb 的闸门承担**（本包不实现档位） |
| 开关 | `BUZZ_AGENT_DEV_TOOLS=0` 关闭 | 同一变量同一读法（`parse::<u8>()`，默认 1，读不懂拒绝启动） |
| Windows | shell 经 git-bash | 同（rpi 的 bash 工具在 Windows 也是 bash/POSIX，无 cmd 回退）；不额外暴露 `powershell` |

**已知能力缺口**：`delegate`（子代理委派）本包没有对应实现 —— 换执行层时这是**能力缺口**而不是等价替换。
另外 `dev__write`/`dev__edit` 的 description 里明写了「路径限定在会话工作区」，与真实约束一致。

与参照物**有意更紧**的一处：本包允许**落在工作区内**的绝对路径（参照物一律拒绝绝对路径）——
rpi 的 write/edit 会把相对路径先解成绝对路径再落盘，拒掉绝对路径反而会破坏正常写入；
「不得出工作区」这条本身没有放宽。

## 工具调用通知（刀 2 第四批）

`session/update` 的 `tool_call` / `tool_call_update`：**字段名、状态取值与 `content` 的嵌套形状**对齐
被替代组件的四个发射点（`crates/buzz-agent/src/agent.rs::emit_pending/emit_in_progress/
emit_completed/emit_failed`）。**但不是逐字等价**——下面把可达状态与三处差异一并写清。

| 时机 | 载荷 | 本包是否可达 |
| --- | --- | --- |
| 模型刚发出调用 | `tool_call`：`toolCallId` / `title`（模型看到的**限定名**）/ `kind: "other"` / `status: "pending"` / `rawInput` | ✅ 每条调用 |
| 开始执行 | `tool_call_update`：`toolCallId` / `status: "in_progress"` | ✅ 每条调用（见差异 1） |
| 调用执行过 | `tool_call_update`：`status: "completed"` + `content: [{type:"content",content:{type:"text",text}}]` + `rawOutput: {isError}` | ✅ 成功与「工具自己报错」都走这条 |
| 压根没执行 / 传输失败 | `tool_call_update`：`status: "failed"` + `rawOutput: {error}` | ❌ **本包产不出**（见差异 1） |

终态语义与参照物一致：`completed` = 「调用**执行过**」（工具自己报错也算，错误在 `rawOutput.isError`）；
`failed` = 「压根没执行 / 传输失败」。非文本结果按参照物的 `as_text_lossy` 渲染成一行占位
（`[image: <mime>, <n> base64 bytes]`），不静默丢。

**不投影** `ToolExecutionUpdate`（工具进度）：参照物没有对应发射点，多发就是**新行为**而不是等价替换。

### 与参照物的差异（三处，根因在 rpi 的事件模型，不在投影逻辑）

1. rpi 只有「要执行了」一个前态、没有参照物那种「已识别 / 已开始执行」的分界 ⇒ 本包**总会**发
   `in_progress`，且**从不发 `failed`**。参照物在「参数形状非法 / 未授权 / 被取消」这类**压根不执行**的
   形状上只发 `pending` + `failed`（实测 unknown-tool 形状：参照物 2 条、本包 3 条）。
2. 输出被截断导致工具**未执行**时，参照物**不发任何通知**；rpi 仍会发 Start ⇒ 本包发
   `pending → in_progress → completed(isError:true)`（3 条 vs 参照物 0 条，内容是「未执行」的原因）。
3. **取消**正在跑的工具调用：参照物补发 `failed: "cancelled"`；本包实测是 `completed`（rpi 中止时照常
   发 `ToolExecutionEnd`，终态里是工具自己的返回）——**没有悬空 `in_progress`**，但状态取值不同
   （探针场景 4 实测）。

**另有多处正文层面的差异**（字段形状与语义一致、**正文措辞不同**，故**不得**当作同形）；至少两类：
①错误正文——本包经 rpi 的工具错误路径会带 `tool error: ` 前缀，参照物直接给文本；本包自建工具
（写入限定等）的错误正文是中文，参照物对应实现是英文；②**成功回执**——同状态同字段下正文也不同
（例如本包 `dev__write` 回 "Successfully wrote to <绝对路径>"、`dev__shell` 回 rpi 的截断/退出摘要，
参照物是它自己的措辞）。这里只举两例、不穷举：**`content.text` 不做逐字对齐**。

**`agent_message_chunk` 少一个 `messageId`**：参照物的文本投影带 `messageId`，本包不带（abb 侧不读它）；
这条属刀 2 第一批的范围，在此一并登记。

**未验证**：参照物「**未授权**（permission denied）」形状的条数只有源码依据（它的 `emit_failed`），
本机没有能触发该形状的客户端；同一「压根不执行」类已由 unknown-tool 形状实测覆盖。

## 失败与取消的语义

- **provider 失败**（401/429/5xx/超时）：回 **JSON-RPC error**（`-32002`，message 带真实原因）。
  注意 rpi 的 `Agent::prompt` 会把 `LoopOutcome::Failed` 折叠成 `Ok(())`，所以本包从事件流里的
  `error_message` 自己认。**不能**回 `stopReason: "end_turn"` + 空文本：abb 会把空文本当
  「纯工具回合、不投递」并 `record_success` ⇒ 用户收不到东西、系统记成功。
  `stopReason` 也不许自造取值——abb 只认
  `end_turn` / `cancelled` / `max_tokens` / `max_turn_requests` / `refusal`。
- **取消**：`session/cancel`（通知，无 `id`，**不产生应答**）置位正在跑的回合并 `abort()`，
  回合以 `stopReason: "cancelled"` 收尾。无在途回合时忽略。
- 并发提交同一会话的第二个回合会被拒（`-32004`）：abb 按频道串行，这是协议误用。
- **超过输出上限是静默截断**：rpi 把 `StopReason::Length` 归入 `Completed`、`error_message`
  为空 ⇒ 本包只能报成 `stopReason: "end_turn"`，abb 看到 `end_turn` 就 `record_success`。
  所以上限**必须**与被替代组件对齐（65_536），写小了就是「长回答被截半截而系统记成功」。
- **stdin EOF 即退出**：父进程关掉 stdin 视为收摊，**在途回合的应答不会被等**。
  写 smoke / 手测时要注意（见下）。

## 当前进度

**已实现并验证**：`initialize`、`session/new`、`session/prompt`（回文本）、`session/cancel`
（回合中可用，含同 burst）、`session/update` 的 `agent_message_chunk`、provider 失败如实报错、
anthropic 与 openai 两个家族（含自定义网关 + 任意模型 id）、**`session/new` 的三项契约
（`cwd` / `systemPrompt` / `mcpServers`）**、**MCP 客户端（工具经 MCP 真执行并回灌）**。

**尚未实现（第二刀）**：

- `_session/steering`（现在如实声明为不支持，走 abb 的 cancel+merge 回退）；
- 逐 token 流式（当前与 buzz-agent 一致：非流式，整条 `agent_message_chunk`）；
- `StopReason::Length`（截断）目前会被报成 `end_turn`（abb 的 `max_tokens` 分支收不到）。
- **openai-chat + 官方 OpenAI 推理模型**可能因字段名被拒：rpi 默认发 `max_tokens`，而被替代的
  fork 对 openai 发 `max_completion_tokens`。**未实测**（无 key/无网络），仅登记为风险。

`BUZZ_AGENT_MAX_ROUNDS` 的护栏代码已就位且**边界与被替代组件对齐**（在 `TurnStart` 即本轮
请求之前按**已完成**的请求数判，做满 `max_rounds` 次就不再开新一轮），但**当前不可达**：
没有工具时一个回合只会有 1 次模型请求（评审用计数探针实测：3 个回合共 3 次 POST），
超界分支不会被触发。接上工具后才成为实际护栏。

## 离线 smoke

**注意**：`printf ... | abb-agent` 会在回合跑完前关闭 stdin，进程随即退出、
**在途回合的应答被丢弃**（只会看到 2 行）。要保持 stdin 打开：

```sh
BIN="$CARGO_TARGET_DIR/debug/abb-agent"
{ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"abb-1","prompt":[{"type":"text","text":"你好"}]}}'
  sleep 2
} | ABB_AGENT_FAUX_TEXT="来自 faux provider 的回复" "$BIN"
```

期望输出（顺序敏感，共 4 行）：`initialize` 应答（`steering.supported=false`、无
`abbSandbox`）→ `sessionId` → `session/update`（`agent_message_chunk`）→
`stopReason: "end_turn"`。

## 端到端探针（`probes/`）

单测用替身，探针跑**真二进制**——两者职责不同。十一条探针各自对应一条被评审反证过的行为
（计数以 `probes/*.py` 里的 `main` 脚本为准）：

| 探针 | 覆盖 |
| --- | --- |
| `cancel_during_turn.py` | 回合在途时读循环仍可服务 `initialize`；cancel 延迟 ≈0 收尾成 `cancelled` |
| `cancel_same_burst.py` | prompt 与 cancel 在同一次 write（0 间隔）时取消不被丢弃（修前 6/6 丢弃） |
| `provider_error_is_visible.py` | provider 失败回 JSON-RPC error，而非「成功 + 空文本」 |
| `openai_family_round_trip.py` | openai 家族端到端：URL 拼法、鉴权头、任意厂商模型 id、文本送达 |
| `mcp_tool_round_trip.py` | MCP 工具真被调用（判据是假 server 写下的 `tools/call` 记录）与结果回灌 |
| `mcp_isolation_and_budget.py` | 子进程 env/cwd 隔离；装配预算（单 20s / 共 30s）与读循环可读 |
| `mcp_image_result_reaches_model.py` | openai 路径图片真进请求体；anthropic 路径**如实交代**（无与事实不符的占位） |
| `builtin_tools_round_trip.py` | 内置工具真落盘（文件系统判据）／`..` 与工作区外绝对路径写不出去／`dev__shell` 真执行／`DEV_TOOLS=0` 真关 |
| `tool_call_notifications.py` | 工具调用通知真发出（`pending → in_progress → completed/failed`，含 MCP 工具）；成功那条带 `content`/`rawOutput` |
| `load_skill_round_trip.py` | 技能清单进系统提示（三段结构）／裸名 `load_skill` 真读到正文与支持文件／越界读被拒／无技能时不暴露该工具／`NO_HINTS=1` 时技能与约定链同关 |
| `hints_and_no_hints.py` | 六个场景：约定链真进请求体（全局层在前）／非零即关／读不懂的取值不发请求且 exit 2／空 cwd 只读一层 |

判定已收紧（只认 `cancelled`／断言不得出现 `result`）——早先出现过「非 cancelled 的其它结论
被算作通过」的假通过窗口。

详见 `probes/README.md`。
