# 项目结构与模块组织

> 本文是 `AGENTS.md` 的模块清单。规模数字的权威来源是
> [`architecture.md`](architecture.md)（由 `tools/arch-check.sh` 校验，漂了会红）。


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
