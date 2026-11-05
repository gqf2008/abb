# 构建、测试、提交

> 本文是 `AGENTS.md` §0 参数表里「构建/检查/测试命令」的展开。
> 章程只要求「合并前跑 §3」；**具体命令与坑在这里**。

## 构建与开发命令

Every cargo command below **must** carry the build-intermediate env prefix (user policy:
build output never lands on the 228 GiB boot volume; on Windows the same rule applies to the
checkout — there `/Volumes` does not exist and intermediates were landing in `target/`):

```sh
# macOS
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-$(basename "$PWD")   # 每 worktree 一个

# Windows（Git Bash；E: 是数据卷）
export TMPDIR=/e/tmp
export CARGO_TARGET_DIR=/e/tmp/abb-target-$(basename "$PWD")   # 每 worktree 一个
```

`tools/gates.sh` sets these two for the current platform and then runs the gates below **in two
groups**: the root package's four, plus the same four with `--manifest-path crates/abb-chart`
(that package has its own manifest + lock + MSRV 1.85 and no `[workspace]` covers it, so the root
commands cannot see it; its target dir is `…-chart` so cargo's file lock never shares with root);
it **refuses a base inside the checkout** (`ABB_BUILD_TMP` overrides the platform default,
`--print-env` shows what it resolved, `ABB_GATES_OS=darwin` reproduces the macOS values on any
host). Prefer it over typing the exports by hand — that is what keeps the policy machine-checkable:

```sh
bash tools/gates.sh            # 八条门禁（root 四条 + crates/abb-chart 四条），中间产物外置
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

With the two exports above in the shell（或直接 `bash tools/gates.sh`）, the root group's four gate commands are verbatim
(`tools/gates.sh` then reruns the same set against `crates/abb-chart` via `--manifest-path`):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --locked
tools/check_test_isolation.sh
```

## 编码风格与命名

- Rust: `snake_case` identifiers, `CamelCase` types/enums; follow `rustfmt` (4-space indent) and keep `cargo clippy` clean.
- Use `//!` module docs and `///` doc comments; explain *why* in comments. Existing comments are often in Chinese — match the language of the file you edit.
- String handling must be UTF-8/char aware (e.g., `agent::truncate` truncates by chars, not bytes).
- Keep `.slint` changes in `ui/app.slint`, consistent with existing component naming.
- 面向用户的文案遵循 [`docs/wording-and-naming.md`](docs/wording-and-naming.md)：ABB 里那个 AI 助理的称呼是「怹」。

## 测试

- Framework: built-in Rust unit tests in `#[cfg(test)]` modules at the end of each `src/*.rs`; the root package also has one Rust integration test — `tests/cli_botkey_callsites.rs` — alongside Python mock helpers (e.g. `tests/mock_acp_agent.py`). `crates/abb-agent` carries its own Rust tests (`cargo test --manifest-path crates/abb-agent/Cargo.toml`) plus Python probes under `crates/abb-agent/probes/`; it has no `tests/` directory. `crates/abb-chart` also carries its own Rust tests (`cargo test --manifest-path crates/abb-chart/Cargo.toml`): unit tests per `src/*.rs` plus one integration test (`tests/cli_e2e.rs`, drives the real binary and covers the exit-code contract 0/1/2) and a golden fixture under `tests/golden/`.
- Name tests with `snake_case`, behavior-focused names (e.g., `codex_single_message_no_progress`, `strip_user_mentions`).
- Add tests alongside the code you change and run `cargo test` before pushing.

## 提交与 PR

- History is short; use imperative, concise subjects, optionally prefixed with the affected area (e.g., `feishu: …`).
- Keep commits focused and explain *why* in the body.
- Walgit patch/PR entries: describe what and why, link the issue thread, and run
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --locked`, and `tools/check_test_isolation.sh` **with
  `TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-$(basename "$PWD")`
  exported** (see Build/Test above). The isolated test runner is the full-test gate;
  do not substitute a bare `cargo test` because it can write the real
  `~/.agent-bridge`. Include before/after screenshots for UI
  changes. GitHub PRs are only for mirror/release maintenance.
