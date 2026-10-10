# Repository Guidelines

Contributor guide for **ABB (agent-bridge)**, a Rust + Slint menu-bar app that bridges Feishu/WeChat/DingTalk messages to local Claude/Codex agents. It runs as a tray controller (default) or a headless bridge daemon (`--service`).

## Project Structure & Module Organization

- `src/` — Rust source; one module per concern (`agent`, `bridge`, `wechat`, `feishu`, `dingtalk`, `schedule`, `ws`, …). Unit tests live inline at the bottom of each file.
- `ui/app.slint` — Slint UI definition, compiled at build time via `build.rs`.
- `app-assets/` — macOS bundle assets (`Info.plist`, `AppIcon.icns`, tray icons).
- `scripts/` — macOS helpers: `build.sh` (bundle + sign + install), `sign.sh` (re-sign), `make-certs.sh` (dev cert).
- `reference/` — protocol references (e.g., `feishu_ws_protocol.py`).
- `crates/abb-agent/` — **唯一执行层**（owner 与授权者会话都走它；由 rpi 库驱动；MCP / 约定链 / 内置工具 / 技能 / 工具通知已对齐被替代组件）。独立包（自带 manifest + lock，root 的 fmt/clippy/test 不碰它）；**GitHub Actions** 里有 `abb-agent-lint`（macOS）与 `abb-agent-windows` 两个 job（这是 abb-agent 的两个 job；`ci.yml` 共六个，其余为 fmt / test / check-macos / test-macos）；`.walgit/ci.toml` 里 abb-agent 只有 `abb-agent-lint` 一条，不等同于覆盖 Windows；命令用 `--manifest-path crates/abb-agent/Cargo.toml`。角色不再决定执行层：`src/service.rs::pick_command_by_sandbox` 已退化为恒选 **owner 槽**（两槽已同一；随包 `abb-agent` 缺失时两槽同回落 PATH `pi-acp`），`_meta.abbSandbox` 三档由 abb-agent 自己实现并声明；原用于回滚的执行层覆盖开关（fork 时代的 config 字段）已一并删除。
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
  and verify with `git worktree list`.
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
- **Mirror discipline**: push normal heads/tags to `origin` only. The local
  walgit-to-GitHub mirror syncs `refs/heads/*` and `refs/tags/*`; GitHub Actions is
  used for mirror/release artifacts, not day-to-day collaboration.

## Build, Test, and Development Commands

Every cargo command below **must** carry the build-intermediate env prefix (user policy:
build output never lands on the 228 GiB boot volume):

```sh
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target
```

- `cargo build` — debug build.
- `cargo run` — run the tray app; `cargo run -- --service` runs the headless daemon.
- `cargo test` — run all unit tests (**do not** run this directly on a live machine: it
  writes the real `~/.agent-bridge`; use `tools/check_test_isolation.sh` instead).
- `cargo clippy --all-targets -- -D warnings` — lint.
- `cargo fmt --check` — verify formatting.
- `tools/check_test_isolation.sh` — the full-test gate (isolated HOME; never substitute
  a bare `cargo test`).
- `scripts/build.sh` — build the release macOS bundle into `~/Applications/ABB.app`.
- `scripts/sign.sh` — re-sign with the `agent-bridge-dev` certificate so TCC privacy grants survive rebuilds.

With the two exports above in the shell, the four gate commands are verbatim:

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

## Testing Guidelines

- Framework: built-in Rust unit tests in `#[cfg(test)]` modules at the end of each `src/*.rs`; the root package also has two Rust integration tests — `tests/cli_botkey_callsites.rs` and `tests/mcp_events_stdio.rs` — alongside Python mock helpers (e.g. `tests/mock_acp_agent.py`). `crates/abb-agent` carries its own Rust tests (`cargo test --manifest-path crates/abb-agent/Cargo.toml`) plus Python probes under `crates/abb-agent/probes/`; it has no `tests/` directory.
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
- Don't commit `logs/`, `target/`, or generated `.app` bundles.
- Before touching signing, read `scripts/sign.sh`: usage-description entitlements on the bare binary can cause a startup `SIGKILL`.
