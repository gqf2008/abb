# Repository Guidelines

Contributor guide for **ABB (agent-bridge)**, a Rust + Slint menu-bar app that bridges Feishu/WeChat/DingTalk messages to local Claude/Codex agents. It runs as a tray controller (default) or a headless bridge daemon (`--service`).

## Project Structure & Module Organization

- `src/` — Rust source; one module per concern (`agent`, `bridge`, `wechat`, `feishu`, `dingtalk`, `schedule`, `ws`, …). Unit tests live inline at the bottom of each file.
- `ui/app.slint` — Slint UI definition, compiled at build time via `build.rs`.
- `app-assets/` — macOS bundle assets (`Info.plist`, `AppIcon.icns`, tray icons).
- `scripts/` — macOS helpers: `build.sh` (bundle + sign + install), `sign.sh` (re-sign), `make-certs.sh` (dev cert).
- `reference/` — protocol references (e.g., `feishu_ws_protocol.py`).
- `crates/abb-agent/` — **随包执行层的源码（待退役）**。**2026-10-23 起分发的 `abb-agent` 二进制是 ki**（pi-rust 构建后改名，钉 SHA 见 `release.yml`；见 `tools/licenses/ki-NOTICE.txt`）：解析链/签名/安装路径零改动，`resolve_agent_command` 随包优先、PATH `ki` 回落，命令形状恒 `<program> acp serve`（旧 abb-agent 忽略 argv，两代兼容）。**供应商接线**：ki 不收参数、也不读旧的 `ANTHROPIC_*` / `OPENAI_COMPAT_*` env（那是旧 abb-agent 的约定）；ABB 把当前供应商落成一份只含它的 `models.json`（`src/ki_config.rs`，目录经 `KI_CODING_AGENT_DIR` 下发，spawn 前在 `runtime/harness.rs` 落盘）。**不要**再给 ki 注入 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`：实测会让 ki 内置身份先命中、用户选的模型被静默换掉。本目录源码保留待退役（阶段 3，单独批次）；独立包（自带 manifest + lock，root 的 fmt/clippy/test 不碰它）；**GitHub Actions** 里有 `abb-agent-lint`（macOS）与 `abb-agent-windows` 两个 job；命令用 `--manifest-path crates/abb-agent/Cargo.toml`。**granted（授权者）机制已移除**（2026-10-23，`23e2c91`）：准入 owner-only、单执行实例；owner 自定义的 `sandbox_mode` 仍经 `_meta.abbSandbox` 下发，执行层不声明该能力时受限档会话在建会话处 fail-closed 拒建（ki 当前不声明；owner 全权限会话不受影响）。
- `crates/abb-chart/` — **图形生成 CLI**（`abb-chart`）：JSON 图规格 + CSV/内联数据 → PNG，供 agent 用 `"$ABB_BIN" chart render …` 出图、再 `deliver --to-current --file` 发回聊天。同样是独立包（自带 manifest + lock，root 的 fmt/clippy/test 不碰它），**自带 MSRV 1.85**（charton 0.8 是 edition 2024，而 abb-agent 锁 1.78——这是它不放进 abb-agent 当内置工具的原因之一；三条候选路的对比见 `crates/abb-chart/README.md`）。为什么走 `$ABB_BIN` 子命令转发：受限（授权者）会话的白名单只放行 `$ABB_BIN` 的子命令（`src/guard.rs::check_abb_bin`），且 charton 的 raster 依赖不该进托盘进程。**上游 raster 字体路径有 4 个静默缺陷**（CSS 栈族名当单体名 / 未知族名静默回落 / 无逐字回退 / PingFang 吞小数点），本包在应用层用逐字符覆盖校验绕开；上游 issue <https://github.com/wangjiawen2013/charton/issues/126>，仓库内长版归档 `docs/upstream-charton-raster-font-findings.md`。**安全边界（改动前必读）**：规格内容里的 `data` 只能读规格文件**同目录**内的文件（`spec.rs::resolve_in_dir`——闸门看不到文件内容，只能由解析方收口；相对与绝对都受限）；命令行 `--data` 相对限 CWD、绝对放行（命令行在闸门可见范围内）。**GitHub Actions** 里 `abb-chart-lint`（macOS）与 `abb-chart-windows` 两个 job；命令用 `--manifest-path crates/abb-chart/Cargo.toml`。
- `third_party/i-slint-core/` — vendored `i-slint-core` wired via `[patch.crates-io]`. The only local patch is the Windows tray window (message-only → top-level hidden; see `Cargo.toml` comment). Its published tree does not ship everything upstream's repo has — `benches/string.rs` and a font its lib tests `include_bytes!` are missing, so standalone `--all-targets` / `--lib --tests` builds fail for reasons unrelated to the patch; compile coverage of the patch comes from root CI building it as a dependency on windows-latest.
- `src/runtime/**` — 从上游 harness 移植的桥/会话层。移植已完成：当年的自维护 fork（独立包）与它的处置台账已在 0685e66 一并删除；`src/runtime/mod.rs` 头部保留的上游 pin 只是**历史出处标注**（不要据此把 ABB 定制反推为上游行为，也不要重开 rsync 流程）。此处改动按普通代码改动对待——在提交信息与 walgit patch 说明里写清动机与影响面即可，没有额外的台账文件。

Runtime data lives in `~/.agent-bridge/` (the non-empty `AGENT_BRIDGE_HOME` env var overrides the
entire runtime directory; empty means default); per-bot workspaces under
`~/.agent-bridge/workspaces/<bot_key>/`.

## Walgit Collaboration

- **Canonical development remote**: `origin` is the local walgit repository at
  `http://127.0.0.1:8081/gqf2008/abb.git`; `github` is a mirror/release remote only.
- **All work tracking lives in walgit**: create/update issues, patches, reviews,
  merge results and status transitions with `walgit collab` entries under
  `refs/collab/*`. Do not open new GitHub issues or PRs for normal development.
- **The host standard is normative**: agents collaborate by this walgit host's `SKILL.md` (`http://walgit.localhost:8081/SKILL.md`, §0–§7). Where this file and that standard disagree, **the standard wins**; this section is a pointer, not a second source of truth.
- **Identity first (§0/§0a)**: one principal per collaborator, one Ed25519 key, materialized **per worktree** by
  `walgit collab join --principal <proj>-<role>-N --push origin` (the seed lands in that worktree's git dir;
  never share a key, never sign an entry for someone else). This project's convention is
  `abb-worker-N` (author) / `abb-reviewer-N` (acceptance) / `abb-coordinator` (merge & close) — take the
  next free index after the highest registered one. Do the §0a routine before filing or claiming: fetch
  `refs/collab/inbox/*` + `refs/collab/meta/*`, read `walgit collab board`, then claim.
- **Entry contract**: `issue` opens a thread (objective, owner, **machine-checkable acceptance**);
  `comment` + `status: in-progress` records the claim (`owner` / `worktree` / `branch` / `work`);
  `status` values are `in-progress` / `needs-review` / `blocked` / `needs-human` / `done` / `closed`
  (`needs-human` only for what genuinely needs the human); `patch` uses `--base refs/heads/main
  --head refs/heads/<branch>`; `review` uses `decision` (**`approve`** / **`request_changes`**),
  `agent` and `note` (full findings: location / problem / suggestion / evidence). Every entry's
  `--parent` is the previous entry's **real oid**. Reference other threads with
  `--related <oid>` / `--depends-on <oid>`.
- **Independent review is structural (§0b/§4)**: the reviewer must be a **different principal** from
  the patch author (and, for the final review, from the merger). An author's own `approve` never
  satisfies the gate; several roles signing with one shared key destroys the audit trail.
- **Merge & close (§5)**: the coordinator merges locally (**fast-forward preferred**), pushes to
  `origin`, records **one** `merge_result {"merged":true,"oid":"<sha>","result":"merged"}` (that entry
  moves the card to `merged`), then `status: closed`; afterwards remove the card's worktree and branch
  and verify with `git worktree list`. **Before writing `merge_result`, run
  `tools/check_review_cadence.sh <thread-id>` — red means do not merge** (contract + blocking-round
  guard, see `docs/review-findings-contract.md`; its sample self-test runs in `.walgit/ci.toml` as
  `review-cadence-selftest`).
- **Board/CI declarations**: `.walgit/board.toml` and `.walgit/ci.toml` are part of
  the tested tree. Move cards only by appending a signed `status` entry; never edit
  the board to represent a state change.
- **Build intermediates go to `/Volumes/DataExt/tmp`** (user policy, 2026-09-17): the
  228 GiB boot volume must not hold build output. Use
  `CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target` for local gates and
  `export TMPDIR=/Volumes/DataExt/tmp` for anything that makes temp trees. Falling back
  to the default `target/` or `$TMPDIR` fills the boot volume and manifests as an
  `errno=28 (No space left on device)` link failure — not as a code failure.
- **CI**: merge gate = the local gate set below (fmt / clippy / build / `tools/check_test_isolation.sh`)
  + independent review — `merge_rule_eval` on this repo requires no CI run (`base is not protected`).
  This repo keeps **no resident local walgit CI runner**: do not start `tools/run_ci_runner.sh`. Historical
  runs do exist in the collab log (the last one is from 2026-10-09, produced by the `ci-runner` principal),
  so `walgit ci status` may answer with old runs — treat them as **history, not verification of your head**.
  GitHub Actions covers `ci.yml` at PR/mirror level (six jobs) and `release.yml` at tag/release time
  (`anti-spam.yml` / `release-on-acceptance.yml` cover their own narrow paths); the full
  build/sign/notarize path lives in `release.yml`.
  `ci.toml` 的任务**假定 POSIX shell**：每条 `command` 都套了 `sh -c '...'`，目的是把
  "Windows runner 经 `cmd /C` 执行 ⇒ 记 exit 0 且日志为空（**签假绿**）"变成"**响亮的红**"
  （cmd 不认 POSIX 语义 ⇒ `sh` 不存在、**或**收到带单引号的畸形 argv，两条路径都必然非零退出）。
  因此**不要把 Windows 主机指到这些任务上**——Windows 的门禁覆盖走 GitHub Actions `ci.yml` 的
  windows job。背景与实测见卡 `abb-ci-coverage-win-20261007`；**"cmd 侧会响亮失败"这一句是机制推断，
  未在 Windows 实机验证**（本仓当前无 Windows 主机；将来若真启用 Windows runner 须先实跑确认）。
- **Mirror discipline**: push normal heads/tags to `origin` only. The local
  walgit-to-GitHub mirror syncs `refs/heads/*` and `refs/tags/*`; GitHub Actions is
  used for mirror/release artifacts, not day-to-day collaboration.

## Build, Test, and Development Commands

Every cargo command below **must** carry the build-intermediate env prefix (user policy:
build output never lands on the 228 GiB boot volume; on Windows the same rule applies to the
checkout — there `/Volumes` does not exist and intermediates were landing in `target/`):

```sh
# macOS
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target

# Windows（Git Bash；E: 是数据卷）
export TMPDIR=/e/tmp
export CARGO_TARGET_DIR=/e/tmp/abb-target
```

`tools/gates.sh` sets these two for the current platform and then runs the four gates below;
it **refuses a base inside the checkout** (`ABB_BUILD_TMP` overrides the platform default,
`--print-env` shows what it resolved, `ABB_GATES_OS=darwin` reproduces the macOS values on any
host). Prefer it over typing the exports by hand — that is what keeps the policy machine-checkable:

```sh
bash tools/gates.sh            # 四条门禁，中间产物外置
bash tools/gates.sh --print-env
```

- `cargo build` — debug build.
- `cargo run` — run the tray app; `cargo run -- --service` runs the headless daemon.
- `cargo test` — run all unit tests (**do not** run this directly on a live machine: it
  writes the real `~/.agent-bridge`; use `tools/check_test_isolation.sh` instead).
- `cargo clippy --all-targets -- -D warnings` — lint（**macOS 与 Windows 两侧都可作门禁**：
  平台门控死代码已按调用点 cfg 对称清掉，见 card `abb-macos-clippy-gate-20261011`；Windows 侧由
  `ci.yml` 的 windows job 覆盖）。
- `cargo fmt --check` — verify formatting.
- `tools/check_test_isolation.sh` — the full-test gate (isolated HOME; never substitute
  a bare `cargo test`).
- `scripts/build.sh` — build the release macOS bundle into `~/Applications/ABB.app`.
- `scripts/sign.sh` — re-sign with the `agent-bridge-dev` certificate so TCC privacy grants survive rebuilds.

With the two exports above in the shell（或直接 `bash tools/gates.sh`）, the four gate commands are verbatim:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --locked
tools/check_test_isolation.sh
```

## Coding Style & Naming Conventions

- Rust: `snake_case` identifiers, `CamelCase` types/enums; follow `rustfmt` (4-space indent) and keep `cargo clippy` clean.
- Use `//!` module docs and `///` doc comments; explain *why* in comments. Existing comments are often in Chinese — match the language of the file you edit.
- String handling must be UTF-8/char aware (e.g., `agent::truncate` truncates by chars, not bytes).
- Keep `.slint` changes in `ui/app.slint`, consistent with existing component naming.
- 面向用户的文案遵循 [`docs/wording-and-naming.md`](docs/wording-and-naming.md)：ABB 里那个 AI 助理的称呼是「怹」。

## Testing Guidelines

- Framework: built-in Rust unit tests in `#[cfg(test)]` modules at the end of each `src/*.rs`; the root package also has one Rust integration test — `tests/cli_botkey_callsites.rs` — alongside Python mock helpers (e.g. `tests/mock_acp_agent.py`). `crates/abb-agent` carries its own Rust tests (`cargo test --manifest-path crates/abb-agent/Cargo.toml`) plus Python probes under `crates/abb-agent/probes/`; it has no `tests/` directory. `crates/abb-chart` also carries its own Rust tests (`cargo test --manifest-path crates/abb-chart/Cargo.toml`): unit tests per `src/*.rs` plus one integration test (`tests/cli_e2e.rs`, drives the real binary and covers the exit-code contract 0/1/2) and a golden fixture under `tests/golden/`.
- Name tests with `snake_case`, behavior-focused names (e.g., `codex_single_message_no_progress`, `strip_user_mentions`).
- Add tests alongside the code you change and run `cargo test` before pushing.

## Commit & Pull Request Guidelines

- History is short; use imperative, concise subjects, optionally prefixed with the affected area (e.g., `feishu: …`).
- Keep commits focused and explain *why* in the body.
- Walgit patch/PR entries: describe what and why, link the issue thread, and run
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --locked`, and `tools/check_test_isolation.sh` **with
  `TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target`
  exported** (see Build/Test above). The isolated test runner is the full-test gate;
  do not substitute a bare `cargo test` because it can write the real
  `~/.agent-bridge`. Include before/after screenshots for UI
  changes. GitHub PRs are only for mirror/release maintenance.

## Security & Configuration

- **macOS entitlements 是分发的硬前提**（#251）：`app-assets/abb.entitlements` 必须随签名带上
  （bundle 与内部可执行都要），否则 hardened runtime 下相机/麦克风/自动化会被**静默拒绝**
  （连授权弹窗都没有）。`scripts/` 被 `.gitignore` 排除、不在版本控制内，所以本机
  **本机公证统一走仓库内入口 `tools/notarize_app.sh <App.app>`**（`tools/notarize_app.sh --self-test`
  可自测参数转发）：它强制把 `app-assets/abb.entitlements` 传给 `~/scripts/notarize.sh`
  —— 后者用 `--deep --force` 重签，不传就把授权签没 —— 并在重签后跑
  `tools/check_entitlements.sh` 断言最终产物。CI 的 release.yml 同样已接入该守卫。
- `config.json` (contains App Secret) and `*.secret` are gitignored — never commit credentials.
- **`bots[].mcp_servers` 是授权面**：声明一个 MCP server = 授权它以 agent 身份运行——它不受会话档位沙箱约束，
  继承 agent 的白名单 env（含 `$ABB_BIN` = 桥本体绝对路径，agent 用它调 `task`/`job`/`deliver`）并以会话工作区为 cwd，
  因此等于给它在工作区的执行权 + 驱动 CLI 的能力；仍受 Q8（`--proc` 拒）与 `cross_delivery_enabled` 约束。只声明可信 server。
- **`$ABB_BIN` 由 ABB 注入 agent 进程**（`service.rs::abb_bin_env`，两条 spawn 路径都带），并由
  `crates/abb-agent` 的 `child_env::PASSTHROUGH_ENV` 透传给 shell 工具与 MCP 子进程；断言见
  `chat_handles_extra_env_carries_abb_bin`、`public_oneshot_wrapper_*` 与探针 `probes/abb_bin_reaches_agent.py`。
- Don't commit `logs/`, `target/`, or generated `.app` bundles.
- Before touching signing, read `scripts/sign.sh`: usage-description entitlements on the bare binary can cause a startup `SIGKILL`.
