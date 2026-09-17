# Repository Guidelines

Contributor guide for **ABB (agent-bridge)**, a Rust + Slint menu-bar app that bridges Feishu/WeChat/DingTalk messages to local Claude/Codex agents. It runs as a tray controller (default) or a headless bridge daemon (`--service`).

## Project Structure & Module Organization

- `src/` — Rust source; one module per concern (`agent`, `bridge`, `wechat`, `feishu`, `dingtalk`, `schedule`, `ws`, …). Unit tests live inline at the bottom of each file.
- `ui/app.slint` — Slint UI definition, compiled at build time via `build.rs`.
- `app-assets/` — macOS bundle assets (`Info.plist`, `AppIcon.icns`, tray icons).
- `scripts/` — macOS helpers: `build.sh` (bundle + sign + install), `sign.sh` (re-sign), `make-certs.sh` (dev cert).
- `reference/` — protocol references (e.g., `feishu_ws_protocol.py`).
- `crates/buzz-agent/` — self-maintained fork (the ACP agent execution layer). Independent package: own manifest + lock, **not** a member of the root package (root `Cargo.toml` has no `[workspace]`), so root `cargo clippy/fmt/test` never touches it — CI has a dedicated `fork-lint` job; run commands with `--manifest-path crates/buzz-agent/Cargo.toml`.
- `third_party/i-slint-core/` — vendored `i-slint-core` wired via `[patch.crates-io]`. The only local patch is the Windows tray window (message-only → top-level hidden; see `Cargo.toml` comment). Its published tree does not ship everything upstream's repo has — `benches/string.rs` and a font its lib tests `include_bytes!` are missing, so standalone `--all-targets` / `--lib --tests` builds fail for reasons unrelated to the patch; compile coverage of the patch comes from root CI building it as a dependency on windows-latest.
- `src/buzz/**` — upstream-sync zone (ported buzz harness). Every change there must be logged in the ledger `docs/buzz-port-sync.md` (处置表); that file also records the fork's known-flaky tests and sync constraints.

Runtime data lives in `~/.agent-bridge/`; per-bot workspaces under `~/.agent-bridge/workspaces/<bot_key>/`.

## Walgit Collaboration

- **Canonical development remote**: `origin` is the local walgit repository at
  `http://127.0.0.1:8081/gqf2008/abb.git`; `github` is a mirror/release remote only.
- **All work tracking lives in walgit**: create/update issues, patches, reviews,
  merge results and status transitions with `walgit collab` entries under
  `refs/collab/*`. Do not open new GitHub issues or PRs for normal development.
- **Entry contract**: `issue` starts a thread; `comment` + `status: in-progress`
  records owner/worktree/branch; `patch` uses `--base refs/heads/main --head
  refs/heads/<branch>`; `review` uses `decision` (`approve` / `needs-changes`),
  `agent`, and `note`; `needs-changes` returns to `status: in-progress`; approve
  keeps `status: needs-review` until merge; after pushing the local merge, append
  `merge_result` with the merged oid and then a second `merge_result` with
  `merged=true`; finish with `status: closed`.
- **Board/CI declarations**: `.walgit/board.toml` and `.walgit/ci.toml` are part of
  the tested tree. Move cards only by appending a signed `status` entry; never edit
  the board to represent a state change.
- **Walgit CI runner**: `ci.toml` is only a declaration; the server does not execute
  it. Start/supervise the local runner with `tools/run_ci_runner.sh` from the stable
  main checkout — it puts `TMPDIR` and `CARGO_TARGET_DIR` on the data volume, which is
  **required**: each run otherwise builds a full `target/` tree in `$TMPDIR` on the
  228 GiB boot volume and eventually dies with `errno=28 (No space left on device)`
  mid-link (observed 2026-09-17). Run **exactly one** runner per host (two runners race
  to claim the same run's tasks). Check results with `walgit ci status --repo .`; a
  missing runner or zero runs is not a pass.
- **Mirror discipline**: push normal heads/tags to `origin` only. The local
  walgit-to-GitHub mirror syncs `refs/heads/*` and `refs/tags/*`; GitHub Actions is
  used for mirror/release artifacts, not day-to-day collaboration.

## Build, Test, and Development Commands

- `cargo build` — debug build.
- `cargo run` — run the tray app; `cargo run -- --service` runs the headless daemon.
- `cargo test` — run all unit tests.
- `cargo clippy --all-targets -- -D warnings` — lint.
- `cargo fmt --check` — verify formatting.
- `scripts/build.sh` — build the release macOS bundle into `~/Applications/ABB.app`.
- `scripts/sign.sh` — re-sign with the `agent-bridge-dev` certificate so TCC privacy grants survive rebuilds.

## Coding Style & Naming Conventions

- Rust: `snake_case` identifiers, `CamelCase` types/enums; follow `rustfmt` (4-space indent) and keep `cargo clippy` clean.
- Use `//!` module docs and `///` doc comments; explain *why* in comments. Existing comments are often in Chinese — match the language of the file you edit.
- String handling must be UTF-8/char aware (e.g., `agent::truncate` truncates by chars, not bytes).
- Keep `.slint` changes in `ui/app.slint`, consistent with existing component naming.

## Testing Guidelines

- Framework: built-in Rust unit tests in `#[cfg(test)]` modules at the end of each `src/*.rs`; the root package has no Rust integration tests — `tests/` only holds Python mock helper scripts (e.g. `tests/mock_acp_agent.py`). The fork `crates/buzz-agent` does have Rust integration tests in `crates/buzz-agent/tests/`.
- Name tests with `snake_case`, behavior-focused names (e.g., `codex_single_message_no_progress`, `strip_user_mentions`).
- Add tests alongside the code you change and run `cargo test` before pushing.

## Commit & Pull Request Guidelines

- History is short; use imperative, concise subjects, optionally prefixed with the affected area (e.g., `feishu: …`).
- Keep commits focused and explain *why* in the body.
- Walgit patch/PR entries: describe what and why, link the issue thread, and run
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --locked`, and `tools/check_test_isolation.sh` (the isolated test
  runner is the full-test gate; do not substitute a bare `cargo test` because it
  can write the real `~/.agent-bridge`). Include before/after screenshots for UI
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
