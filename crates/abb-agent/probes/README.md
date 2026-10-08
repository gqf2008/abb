# 端到端探针（需要真二进制，故不入 `cargo test`）

这两条脚本是**评审反证 (a)(b) 的复现实验**，跑的是构建产物而不是单测替身。用法：

```sh
export TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target
cargo build --manifest-path crates/abb-agent/Cargo.toml
python3 crates/abb-agent/probes/cancel_during_turn.py \
    "$CARGO_TARGET_DIR/debug/abb-agent"
python3 crates/abb-agent/probes/provider_error_is_visible.py \
    "$CARGO_TARGET_DIR/debug/abb-agent"
```

- `cancel_during_turn.py`：把 `ANTHROPIC_BASE_URL` 指到一个「接受连接但永不回响应」的
  本地黑洞，造出长回合，然后在回合进行中送 `initialize`（验证读循环没被占死）与
  `session/cancel`（验证收尾成 `stopReason:"cancelled"`）。
  **修前表现**：两者都不被处理，回合挂到客户端超时。
- `provider_error_is_visible.py`：起一个只回 401 的本地假端点，断言回合应答是
  JSON-RPC **error**（而不是「成功 + 空文本」）。顺带用日志验证 `ANTHROPIC_MODEL`
  与 `ANTHROPIC_BASE_URL` 真的生效。
  **修前表现**：`stopReason:"end_turn"` + 零文本 + 零日志 ⇒ abb 当「纯工具回合」不投递
  并 `record_success`。

- `cancel_same_burst.py`：把 `session/prompt` 与 `session/cancel` 放进**同一次 write**
  （0 间隔）配黑洞端点。修前 `in_flight` 在被派出的任务里抢，读循环能在任务被调度前先读到
  cancel ⇒ 取消被静默丢弃（0 间隔 6/6）；修后 8/8 收敛成 `cancelled`。
- `openai_family_round_trip.py`：起一个假 OpenAI 兼容网关（回最小 SSE），断言请求打到
  `{base}/v1/chat/completions`、鉴权头是 abb 注入的 key、请求体里的 model 就是我们给的
  **任意厂商 id**，且文本被发成 `session/update` 并以 `end_turn` 收尾。这条覆盖的是 abb 把
  `openai-chat`/`openrouter`/`deepseek` 全归并成 `BUZZ_AGENT_PROVIDER=openai` 的真实路径。

- `mcp_tool_round_trip.py`（配 `fake_mcp_server.py`）：刀 1 的核心验收。按 `session/new` 的
  `mcpServers` 起一个真 MCP server（stdio JSON-RPC），用 `ABB_AGENT_FAUX_TOOL` 让模型发一次
  工具调用，**判据是假 server 写下的 `tools/call` 记录**——从 agent 的文本输出无法区分
  「工具被调用」与「模型自己编了答案」。
- `fake_mcp_server.py`：上述探针用的最小 MCP server（`initialize` / `tools/list` / `tools/call`）。
  另有 `--dump-env <path>`（落盘自己的 cwd + 全量 env）与 `--hang-init`（`initialize` 永不回包）
  两个开关，供下面那条隔离/预算探针使用。
- `mcp_isolation_and_budget.py`（配 `fake_mcp_server.py`）：第四轮评审（`abb-reviewer-61`）判
  needs-changes 的两类偏离的复现实验。场景 1 断言**子进程真的没继承供应商凭据**（判据是假
  server 自己写下的 env，不是读代码）、`spec.env` 生效、`cwd` 落在会话工作区；场景 2 用一个
  `--hang-init` 的 server 把装配预算耗满，断言同一时刻发出的 `initialize` **0.0x 秒**就拿到应答
  （读循环没被占）且 `session/new` 在单 server 预算（20s）附近收尾。
  **修前表现**：子进程 env 里有 `ANTHROPIC_API_KEY`、cwd 是 abb-agent 自己的；挂住的 server
  让 `session/new` 吃掉 35s 且期间 `session/cancel` 与 `initialize` 各被拖 19s（abb 的「停止」
  只有 5s 宽限）。场景 3 用**两个**挂死 server 顶到 30s 总预算，并在装配已跑 1s 之后才发
  `initialize`（避开「恰好赶在装配开始前答完」的假通过）。
- `mcp_image_result_reaches_model.py`（配 `fake_mcp_server.py --image-data`）两个场景：
  ①假 OpenAI 网关第一跳回 `tool_calls`、第二跳抓请求体，断言 MCP 回的图片以
  `data:image/png;base64,…` 真的进了模型请求，且没有被换成 `(see attached image)`；
  ②假 anthropic 端点（`/v1/messages` 事件流）断言走 anthropic 时**没有**这个误导性占位，
  而是本包自己写的 `[image 未传给模型：…]`。
  **修前表现**：`Model::new` 只给 `input=[Text]` ⇒ openai 路径图片被换成占位（图真丢）；
  把 `input` 改成 `[Text, Image]` 后 anthropic 路径反而变成「指着一张不存在的图」（上游把
  `Content::Image` 硬编码成 `(see attached image)`）。所以两条路径现在分开处理。
  **未覆盖**：openai **responses** 路径的图片（仅有源码依据，无端到端）。
- `builtin_tools_round_trip.py`：内置工具（`dev__*`）的端到端验收，**判据是文件系统**而不是
  agent 的文本：`dev__write` 写相对路径必须真出现文件（含自动建父目录），`../` 逃逸与工作区外
  绝对路径必须**不落盘**，`dev__shell` 必须真执行命令（`echo … > shell.txt`），
  `BUZZ_AGENT_DEV_TOOLS=0` 时既没有内置工具也没有文件产出；末段符号链接指向工作区外时写入必须被拒
  且目标文件内容原封不动；`dev__shell` 的 `env` 快照里**不得**出现宿主凭据（`ANTHROPIC_API_KEY`
  探针哨兵）而 `PATH` 仍在；`dev__edit` 真改文件内容。
- `tool_call_notifications.py`：工具调用通知的端到端验收，**判据是 abb-agent 自己的 stdout**
  （abb 会收到的那些 `session/update`）：成功调用必须看到 `tool_call`（`pending` + `title` 限定名 +
  `rawInput`）→ `in_progress` → `completed`（带 `content` 与 `rawOutput.isError:false`）；被拒的调用必须
  看到 `failed` + `rawOutput.error`；MCP 工具同样有通知（证明不是只给内置工具打的补丁）。
- `load_skill_round_trip.py`：技能发现 + `load_skill` 的端到端验收，判据是**假 anthropic 端点抓到的
  请求体**（第一跳看工具表与 system，第二跳看模型拿到的工具结果）：技能清单/描述真进系统提示、工具名是
  **裸名** `load_skill`、正文被读到且 **frontmatter 被剥掉**、支持文件按 `demo/references/foo.md` 读到、
  越界形式 `demo/../../secrets.md` **拿不到工作区外内容**且给出可纠偏错误、**无技能的工作区里不暴露
  `load_skill`**、`BUZZ_AGENT_NO_HINTS=1` 时工具表里没有 `load_skill` 且技能正文标记在整个请求体里
  都不出现。
- `hints_and_no_hints.py`：约定链的端到端证据（判据是假 anthropic 端点收到的 `system` 字段）。
  场景 1 断言会话目录的 `AGENTS.md` 与 `$HOME/AGENTS.md` 都进了请求、且全局层在前；场景 2
  断言 `BUZZ_AGENT_NO_HINTS=1`（abb 给 granted 会话的进程级收口）时**两者都不得出现**而默认
  系统提示仍在；场景 3 断言没有 `AGENTS.md` 时不注入空标题；场景 4 断言 `NO_HINTS=2` 也判关；
  场景 5 断言 `true`/`yes`/`-1`/`256`/`" 1 "` 这类取值**不发任何模型请求且以 2 退出**（不能静默
  fail-open）；场景 6 断言空 cwd 时只读进程工作目录那一层、不向祖先链扩散（钉 `acp.rs` 接线）。
  **修前表现**：没有任何约定链（系统提示里只有 abb 下发的或默认那一句），granted 会话与 owner
  会话完全同形（本该被区分）。探针自带隔离的 `HOME`/`USERPROFILE`，不会读跑测机器的真实
  `~/AGENTS.md`。

全部只在 `/tmp` 与本地回环上活动，不访问外网、不写仓库文件。
