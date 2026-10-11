# ABB bundled tools

ABB 安装包内置这些固定版本的工具（一包一名的 + 多调包提供多个工具名）：

| tool | version | purpose | license |
|---|---:|---|---|
| `rg` | 15.2.0 | 快速全文检索 | MIT / Unlicense |
| `jq` | 1.8.2 | JSON 处理 | MIT |
| `uv` | 0.12.13 | Python 环境/依赖管理 | MIT / Apache-2.0 |
| `gh` | 2.100.0 | GitHub CLI | MIT |
| `goawk` | 1.32.0 | POSIX awk（以 `awk` 名暴露） | MIT |
| `busybox` | FRP-6075-g169694ebd | busybox-w32（**仅 Windows**）多调 exe：提供 `ps`/`watch`/`wget`/`usleep`/`unix2dos`/`sed`/`echo`/`printf`/`id`/`hexdump`/`md5sum`/`sha1sum`/`sha256sum` | **GPL-2.0**（见 `licenses/busybox-COPYING-GPL2.txt`；源码 <https://github.com/rmyorston/busybox-w32>） |
| `coreutils` | 0.12.0 | uutils coreutils（**多调二进制**，本仓按 `tools/bundled-cli-tools.tsv` 暴露 `tail`/`sleep`/`head`） | MIT |
| `diffutils` | 0.5.0 | uutils diffutils（多调二进制，暴露 `diff`/`cmp`） | MIT / Apache-2.0 |

传递依赖许可也随包：

- `rg` 静态包含 PCRE2 10.45：`licenses/pcre2-LICENCE.txt`，Rust wrapper 许可见
  `licenses/pcre2-COPYING.txt`。
- `jq` 静态包含 Oniguruma：`licenses/jq-oniguruma-COPYING.txt`。

`git`、`bun`、`sed`、`find` 明确不随包：

- `git` 继续作为宿主外部依赖；ABB 内部快照使用 libgit2。
- `sed` / `find` 使用系统版本，避免 GNU/BSD 行为差异。
- `bun` 体积和更新频率不适合默认随包。

**多调包的分发形态**（`coreutils`/`diffutils`）：macOS 官方资产是**单个多调二进制**，fetch 脚本解出
`bin/<包名>` 后按 `tools/bundled-cli-tools.tsv` 在每个工具名上建**硬链接**（同卷、免权限；失败回落复制并告警），
所以 `bin/tail` 与 `bin/coreutils` 是同一 inode。Windows 的 `coreutils` 资产本来就是**每工具一个 exe**，
按名单取件即可；`diffutils` 仍是多调 → 同样走硬链接。验活不依赖 `--version`（uutils diffutils 没有），
只要求无参可执行（rc≠126/127）。

**未随包 / 仅 Windows 随包的工具**（2026-10-11 实核，含 2026-10-11 owner 追加的 busybox 一批）：

| 工具 | 结论 | 判据 |
|---|---|---|
| `ps` | **Windows 随包**（busybox）；macOS/Linux 用宿主自带 | uutils coreutils 0.12.0 的 106 个工具里没有 `ps`（`./coreutils --list` 无命中）⇒ macOS 不重复提供；Windows 由 busybox-w32 提供 |
| `watch` | **Windows 随包**（busybox）；macOS 不随包 | macOS 无官方预编译且 CLI 语义一致的资产（watchexec 有官方预编译但语义不同）⇒ 等价写法 `while true; do <cmd>; sleep N; done`；Windows 由 busybox-w32 提供 |
| `wget` | **Windows 随包**（busybox）；macOS 不随包 | macOS 上 GNU wget 官方无预编译、社区复打包不接受（供应链）⇒ 用 `curl`；Windows 由 busybox-w32 提供 |
| `md5sum` / `sha1sum` / `sha256sum` | **两平台随包** | macOS 宿主没有这三个名字（只有 `md5`/`shasum`）⇒ 由 coreutils 提供；Windows 由 busybox 提供 |
| `sed` / `echo` / `printf` / `id` / `hexdump` | **仅 Windows 随包** | macOS/Linux 宿主自带等价物（且 `echo`/`printf` 多为 shell 内建），**不覆盖**以免行为漂移；Windows 由 busybox-w32 提供 |

macOS 的 `jq 1.8.2` 不使用官方预编译包（其 minOS 为 14），而是从官方源码以
`MACOSX_DEPLOYMENT_TARGET=12.0` 静态构建，保持 ABB 当前 `LSMinimumSystemVersion=12.0`。

## 取件形态与自检

多调包（`coreutils`/`diffutils`/`busybox`）的落地规则（两个 fetch 脚本**同构**实现）：
1. 先把**包产物** `output_name` 放进 `bin/`（裸 exe 就是它自己；归档里按名字递归找——uutils 的
   Windows zip 带一层目录前缀）；
2. 再按 `tools/bundled-cli-tools.tsv` 为每个名字落文件：优先用包内**同名文件**（coreutils 的
   Windows zip 是每工具一个 exe），否则**硬链接**到包产物（diffutils/busybox 是多调），
   硬链接失败回落复制；
3. 两者都拿不到即报错（绝不静默少一个工具）。

**自检入口**：`tools/check_bundled_tools.sh --platform <macos-arm64|windows-x64> --bin-dir <目录>/bin`
断言"期望集合（包产物 ∪ 该平台多调名字）== 目录实际文件"，接在 `release.yml`（两平台）与
`ci.yml` 的 `abb-agent-windows`（PR 级 Windows 证据）。本机（macOS）也可以用
`fetch_bundled_tools.sh --platform windows-x64` 做**取件链路干跑**（PE 产物不执行，只断言非空）。

busybox 的许可与出处见 `licenses/busybox-COPYING-GPL2.txt` + `licenses/busybox-SOURCE-NOTICE.txt`
（仓库、构建标识、sha256；GPL-2.0 分发要求随二进制提供）。

## 布局

构建时由 `fetch_bundled_tools.sh` / `fetch_bundled_tools.ps1` 按
`tool-lock.tsv` 下载并校验 SHA256，输出：

```text
tools/
  bin/        rg, jq, uv, gh, coreutils(+tail/sleep/head 硬链接), diffutils(+diff/cmp 硬链接)
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
tools-dist/test/bin/tail --version      # 多调包的按名调用
tools-dist/test/bin/diff <a> <b>        # diff 不实现 --version/--help，用真实调用验活
```

Windows：

```powershell
./tools/fetch_bundled_tools.ps1 -Platform windows-x64 -Dest tools-dist
```

发布前必须在干净 PATH 下运行 `agent-bridge --dump-tools --require-bundled-tools`，
确认四个命令都解析到随包路径；普通 `--dump-tools` 会显示 `bundled/system/missing` 来源。
