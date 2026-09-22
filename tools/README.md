# ABB bundled tools

ABB 安装包内置五个固定版本的工具：

| tool | version | purpose | license |
|---|---:|---|---|
| `rg` | 15.2.0 | 快速全文检索 | MIT / Unlicense |
| `jq` | 1.8.2 | JSON 处理 | MIT |
| `uv` | 0.12.13 | Python 环境/依赖管理 | MIT / Apache-2.0 |
| `gh` | 2.100.0 | GitHub CLI | MIT |
| `wassette` | 0.7.1 | 沙箱化 MCP 工具宿主（wasmtime 运行 Wasm Component） | MIT（另含 Apache-2.0 组件，见 `wassette-NOTICE.txt`） |

传递依赖许可也随包：

- `rg` 静态包含 PCRE2 10.45：`licenses/pcre2-LICENCE.txt`，Rust wrapper 许可见
  `licenses/pcre2-COPYING.txt`。
- `jq` 静态包含 Oniguruma：`licenses/jq-oniguruma-COPYING.txt`。

`git`、`bun`、`sed`、`find` 明确不随包：

- `git` 继续作为宿主外部依赖；ABB 内部快照使用 libgit2。
- `sed` / `find` 使用系统版本，避免 GNU/BSD 行为差异。
- `bun` 体积和更新频率不适合默认随包。

macOS 的 `jq 1.8.2` 不使用官方预编译包（其 minOS 为 14），而是从官方源码以
`MACOSX_DEPLOYMENT_TARGET=12.0` 静态构建，保持 ABB 当前 `LSMinimumSystemVersion=12.0`。

macOS 的 `wassette 0.7.1` 同样不走官方预编译包：官方资产在 hardened runtime 签名下执行
组件时被 macOS 26 代码签名强制以「Code Signature Invalid」SIGKILL（wasmtime 47 的 JIT
不走 MAP_JIT，allow-jit entitlement 也救不了）。改为官方源码构建并打
`tools/wassette-pulley.patch`（wasmtime +pulley feature，macOS 默认 Pulley 解释器、
无 JIT 页；`WASSETTE_INTERPRETER=0` 可切回 JIT）。Windows 继续源码构建但**不打**该补丁
（无此问题，保留 JIT 性能）。

## 布局

构建时由 `fetch_bundled_tools.sh` / `fetch_bundled_tools.ps1` 按
`tool-lock.tsv` 下载并校验 SHA256，输出：

```text
tools/
  bin/        rg, jq, uv, gh, wassette
  licenses/   对应许可证全文
```

macOS 输出到 `ABB.app/Contents/Resources/tools/`，Windows 安装到
`{app}\tools\`。运行时 `deps::composed_path()` 只把这个 `bin` 前置到
agent 子进程 PATH，不修改用户全局 PATH。

## 更新工具

1. 从官方 release 选择目标平台资产并固定 URL/SHA256。
2. 更新 `tool-lock.tsv` 的版本、URL、SHA256。
3. 同步更新本文件和 `licenses/` 中的许可证文本。
4. 运行：

```bash
tools/fetch_bundled_tools.sh --platform macos-arm64 --dest tools-dist/test
tools-dist/test/bin/rg --version
tools-dist/test/bin/jq --version
tools-dist/test/bin/uv --version
tools-dist/test/bin/gh --version
tools-dist/test/bin/wassette --version
```

Windows：

```powershell
./tools/fetch_bundled_tools.ps1 -Platform windows-x64 -Dest tools-dist
```

发布前必须在干净 PATH 下运行 `agent-bridge --dump-tools --require-bundled-tools`，
确认五个命令都解析到随包路径；普通 `--dump-tools` 会显示 `bundled/system/missing` 来源。
