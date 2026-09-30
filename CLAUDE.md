# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**ABB (agent-bridge)** — a Rust + Slint menu-bar app that bridges Feishu / WeChat / DingTalk bots to local Claude / Codex / Pi agents. Users chat with the bot; the bridge spawns the configured local agent per bot and relays replies back. Everything runs locally (runtime data in `~/.agent-bridge/`), no servers.

`AGENTS.md` is the contributor guide (structure, style, commit/PR conventions) — follow it. This file covers the architecture that spans multiple files and the operational commands.

## Commands

- `cargo build` / `cargo run` — build/run the tray GUI (default mode).
- `cargo run -- --service` — run the headless bridge daemon.
- `cargo test` — all tests; `cargo test <name>` — single test. ~150 inline `#[cfg(test)]` tests, no `tests/` dir. **CI does not run tests** (release.yml only builds) — run them locally before pushing.
- `cargo clippy --all-targets -- -D warnings` — lint; `cargo fmt --check` — format check.
- Manual smoke-testing without the GUI (all read `~/.agent-bridge/config.json`):
  - `agent-bridge job list|add|del` — scheduled-task CLI (also the one agents are told to call).
  - `agent-bridge deliver --bot <key> --chat <id> --text ...` — cross-chat delivery (requires the setting enabled).
  - `agent-bridge session reset <chat_id>` — new agent session for a chat.
  - Hidden debug flags: `--dump-config`, `--dump-perms`, `--fetch-bot-info`, `--wx-qr-test`.
  - The `job`/`deliver`/`session` CLIs resolve bot/chat from `AGENT_BRIDGE_BOT_KEY` / `AGENT_BRIDGE_CHAT_ID` env — the same vars the bridge injects when spawning agents.
- Release: pushing a `v*` tag triggers `.github/workflows/release.yml` — macOS arm64 DMG (codesign + notarize + staple) and Windows Inno Setup exe (`app-assets/ABB.iss`, optional signtool). `workflow_dispatch` builds artifacts only, no release. macOS is **Apple Silicon only**. Version comes from `Cargo.toml`; `Info.plist` is rewritten in CI.

## Architecture

### One binary, three roles (`src/main.rs`)

1. **Tray GUI** (default) — Slint tray icon + multi-bot settings window; it is also the **watchdog for the service**: spawns it, tracks pid, restarts on crash (`src/install.rs`). No launchd/systemd — cross-platform by design.
2. **`--service`** — headless daemon (pure tokio): loads config, starts one event loop per enabled bot, processes messages.
3. **Short-lived CLIs** (`job`, `deliver`, `session`) — invoked by spawned agents via `$ABB_BIN` (absolute path injected at spawn).

Single-instance guards use `flock` (Windows named mutex); GUI and service use separate locks (`src/single_instance.rs`).

### Message flow

```
channel client (feishu WS / wechat long-poll / dingtalk WS)
  → Messenger trait (src/messenger.rs, per-bot implementation by bot.kind)
  → Bridge (src/bridge.rs, one per bot)
      → access control → control-command interception → pending.json → per-chat queue
      → spawn agent subprocess in ~/.agent-bridge/workspaces/<bot_key>/  (src/agent.rs)
      → agent reply → Messenger.send_text (segmenting, thread replies, typing/DONE reactions)
```

- **Feishu**: WS long connection; minimal hand-rolled proto2 codec in `src/proto.rs`, byte-level aligned with `reference/feishu_ws_protocol.py`; REST for sending (`src/feishu.rs`, tenant_access_token cached).
- **WeChat**: Tencent ilink HTTP endpoints, QR login → long-poll `getupdates` (`src/wechat.rs`).
- **DingTalk**: Stream-mode WS; each reconnect re-registers for a one-time 90s ticket (`src/dingtalk.rs`).

### Bridge logic (`src/bridge.rs` — the core, 2k+ lines)

Per bot, in order: owner-only access control (re-read from `config.json` **on every message** so grant/revoke is immediate) → control-command interception **before** entering the agent (`/new` resets the session; natural stop-words set a cancel flag to kill the running task) → persist to `pending.json` → per-chat serial queue (one `tokio::sync::Mutex` per chat key, messages queue rather than drop) → spawn agent → send reply.

Zero-regex parsing (mentions are hand-parsed); all string handling is UTF-8 char-aware (`agent::truncate` truncates by chars).

### Cross-process state — everything is files under `~/.agent-bridge/`

Spawned agents are child processes and **cannot reach the service's in-memory Messenger**, so all IPC is file-based:

- `config.json` (0600) — multi-bot schema (see `src/config.rs` doc); GUI writes, service hot-reads. In-process write lock + atomic tmp-rename writes (`main::atomic_write_text`).
- `workspaces/<bot_key>/` — the agent's cwd (isolation key = bot `name`). Contains `sessions.json` (**per-backend slots**: claude `--resume` UUID vs codex thread_id vs pi `--session-id` are mutually incompatible — switching backend on the same slot mixes conversations), `jobs.json`, `pending.json` (in-flight recovery: replayed after crash), `pending_outbox.json` (WeChat-only: proactive pushes rejected with `ret=-2` when the context_token is stale are buffered until the next inbound message), `attachments/`, plus a **workspace guide** (`agent.rs::ensure_workspace_guide`, versioned `GUIDE_MARKER`) telling the agent how to use the job/deliver CLIs.
- `deliveries.json` — cross-chat delivery queue: CLI enqueues under an `flock`/`LockFileEx` file lock, the service polls and sends (`src/deliver.rs`).
- `logs/bot-status.json` — service writes connection state, GUI reads it for the tray icon.

### Scheduling

`src/schedule.rs` persists jobs and evaluates a hand-written 5-field Chinese cron (no cron crate), local time UTC+8. Natural-language scheduling is **not parsed by the bridge**: the workspace guide instructs the agent to call `$ABB_BIN job add ...` and exit immediately — never loop/sleep in the agent, or that chat's queue is blocked. Job results are pushed back to the creating chat (multi-target `--to` supported).

### GUI (`src/ui.rs`, `ui/app.slint`)

Slint 1.17; `ui/app.slint` is compiled by `build.rs`. **The `renderer-skia` feature is required — do not remove or change it** (see the comment in `Cargo.toml`: femtovg renders black in the macOS accessory process; the software renderer breaks tray icons). Settings window: left bot list + right editor, edits on a working copy (`Rc<RefCell<Vec<BotConfig>>>`), written to `config.json` only on save. macOS-specific code uses raw `objc_msgSend` FFI (zero-dependency policy, no objc crate) in `src/platform.rs` / `src/permreq.rs`.

## Conventions & Pitfalls

- Code comments are mostly Chinese — match the language of the file you edit.
- **Never add a timeout to agent execution.** The bridge is a push model: it waits as long as the agent runs (a 600s cap was removed by user decision, 2026-08-07 — see `src/agent.rs` doc).
- Atomic writes: tmp file + rename everywhere (config/sessions/jobs/botstatus).
- Windows specifics: `windows_subsystem = "windows"` (no console; `log!` swallows stdout write errors), child spawns use `CREATE_NO_WINDOW`, and PATH lookup must handle `PATHEXT` (see `deps::find_in_path`).
- `AGENTS.md` mentions `scripts/` (macOS build/sign helpers) — it is gitignored ("开发脚本不入库") and absent from a fresh clone; treat as local-only legacy.
- `docs/session-isolation.md` explains chat_id rules per platform and Feishu topic (`chat_id:thread_id`) isolation — read it before touching session keys.
- `.gitignore` covers `config.json`, `*.secret`, `logs/`, `app-assets/Output/`, `scripts/` — never commit credentials or build artifacts.
