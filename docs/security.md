# 安全边界与配置

> 本文是 `AGENTS.md` §1/§5 里「安全、凭据、授权面」红线的展开。
> **全局规则（`~/.ki/agent/AGENTS.md`）优先于本文**；本文只补本项目特有的边界。


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
