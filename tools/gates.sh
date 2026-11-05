#!/usr/bin/env bash
# 门禁一键入口：**先把构建中间产物赶到 checkout 之外**，再跑 AGENTS.md 定义的门禁——
# **两组共八条**：root 包四条（fmt / clippy / build --locked / 隔离全量测试）+ `crates/abb-chart`
# 四条（同形命令，但都带 `--manifest-path`）。
#
# 为什么 chart 要单独一组：`crates/abb-chart` 是**独立包**（自带 manifest + lock + MSRV 1.85），
# 而 root 的 `Cargo.toml` **没有 `[workspace]`** ⇒ 不带 `--manifest-path` 的 root 命令
# **语义上完全碰不到它**。补这一组之前实测：一次门禁日志 3654 行里 `Running tests/cli_e2e`
# 出现 0 次——chart 的测试红了，本机合并门禁照样全绿。它此前的覆盖只有 GitHub Actions 的
# `abb-chart-lint` / `abb-chart-windows`，而本仓合并门禁**不要求 CI run** ⇒ 缺口就在这里。
#
# 为什么存在：2026-09-17 的 macOS 政策（`/Volumes/DataExt/tmp`）只覆盖 macOS，
# Windows 上照文档执行 = 裸 `cargo` = 中间产物全落 checkout（2026-10-11 实测把仓库撑到
# 38 G：`target/` 32 G + `crates/abb-agent/target/` 2.4 G + 僵尸 `crates/buzz-agent/target/`）。
# 本脚本把「外置 target」变成**默认行为**，并**硬拒绝**落在 checkout 内的 base——
# 这样这条政策才有机器判据（`RULE_规则可执行性.md` 第 1 条）。
#
# 用法：
#   tools/gates.sh                 # 跑两组门禁（root 四条 + crates/abb-chart 四条）
#   tools/gates.sh --print-env     # 只打印解析结果（os、TMPDIR、TMP、TEMP、CARGO_TARGET_DIR、repo）
#   ABB_BUILD_TMP=/e/tmp tools/gates.sh   # 覆盖 base（默认按平台取数据卷；**必须已存在**）
#   ABB_GATES_OS=darwin tools/gates.sh --print-env   # 覆盖平台探测（用于在本机核 macOS 口径）
#
# 两条硬保证（都有阳性对照，见 RULE_可执行性）：
#   1. base 落在 checkout 内 ⇒ 直接失败；
#   2. base 不存在 ⇒ 直接失败（不代建，避免把产物静默写到启动卷/系统盘）。
#
# 退出码：0=门禁全绿；1=政策违规（base 在 checkout 内 / base 不存在）；其它=cargo/测试自身的失败码
#   （两组任一条失败都会立刻以该条的状态码退出，后续门禁不再跑）。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

print_only=0
while [ $# -gt 0 ]; do
  case "$1" in
    --print-env | --dry-run) print_only=1 ;;
    -h | --help)
      # 头注自 `# 门禁一键入口` 起到 `set -euo pipefail` **之前**：不写死行号，改注释不会错位。
      sed -n '/^# 门禁一键入口/,/^set -euo pipefail$/p' "$0" | sed -n '/^set -euo pipefail$/q;p'
      exit 0
      ;;
    *)
      echo "未知参数：$1（支持：--print-env / -h）" >&2
      exit 2
      ;;
  esac
  shift
done

# 平台默认 base：macOS = 数据卷 /Volumes/DataExt/tmp（2026-09-17 政策）；
# Windows（Git Bash/MSYS）本机数据卷是 E:，统一放 E:\tmp；其它平台退回 ${TMPDIR}。
os="$(printf '%s' "${ABB_GATES_OS:-$(uname -s)}" | tr '[:upper:]' '[:lower:]')"
case "$os" in
  darwin*) default_base=/Volumes/DataExt/tmp ;;
  mingw* | msys* | cygwin*) default_base=/e/tmp ;;
  *) default_base="${TMPDIR:-/tmp}" ;;
esac
base="${ABB_BUILD_TMP:-$default_base}"

# 归一：优先用父目录的真实路径拼，避免 `..`/符号链接绕过下面的前缀比对；
# 父目录不存在（首次使用）时退回原样字符串。
if [ -d "$base" ]; then
  base="$(cd "$base" && pwd -P)"
elif [ -d "$(dirname "$base")" ]; then
  base="$(cd "$(dirname "$base")" && pwd -P)/$(basename "$base")"
fi
case "$base" in
  /*) ;;
  *) base="$PWD/$base" ;;
esac

export TMPDIR="$base"
# **每个 worktree 用自己的 target 目录**（卡 abb-target-dir-per-worktree-20261022）。
#
# 为什么：原来固定写 `$base/abb-target`，多个 worktree 并行时全部指向同一个目录，
# cargo 的文件锁互相饿等——实测单次 `cargo build` 卡 25+ 分钟无进展、系统 load 到 12.4、
# `check_test_isolation.sh` 连续超时（测试本身只要 15 秒），而这些超时**看起来像测试失败**。
#
# 命名用 worktree 目录名：它天然唯一、可读（能一眼看出是哪个 worktree 的产物），且同一个
# worktree 反复跑仍是同一个路径 ⇒ 增量缓存照旧生效。
# 例外：`ABB_TARGET_SHARED=1` 时退回共用目录（给「只有一个 worktree、想省磁盘」的场景留退路）。
if [ "${ABB_TARGET_SHARED:-0}" = "1" ]; then
  export CARGO_TARGET_DIR="$base/abb-target"
else
  export CARGO_TARGET_DIR="$base/abb-target-$(basename "$repo_root")"
fi
# Windows 原生进程（rustc/link、cmd）**不认 `TMPDIR`**（那是 POSIX 约定），只看 `TMP`/`TEMP`：
# 不补这两个，link/rustc 的 scratch 仍留在系统盘 C: 的 `%TEMP%`。cygpath 只在 MSYS/Cygwin 存在，
# 正好当平台判据，并把 `/e/tmp` 转成原生 `E:\tmp`。
if command -v cygpath >/dev/null 2>&1; then
  native_base="$(cygpath -w "$base")"
  export TMP="$native_base"
  export TEMP="$native_base"
fi

repo_real="$(cd "$repo_root" && pwd -P)"
case "$base" in
  "$repo_real" | "$repo_real"/*)
    echo "❌ 构建中间产物不许落在 checkout 内：base=${base}（repo=${repo_real}）" >&2
    echo "   macOS: ABB_BUILD_TMP=/Volumes/DataExt/tmp；Windows: ABB_BUILD_TMP=/e/tmp" >&2
    echo "   详见 AGENTS.md 的 Build/Test 段与线程 abb-build-tmp-windows-20261011。" >&2
    exit 1
    ;;
esac

if [ "$print_only" = 1 ]; then
  echo "os=$os"
  echo "TMPDIR=$TMPDIR"
  echo "TMP=${TMP:-<unset>}"
  echo "TEMP=${TEMP:-<unset>}"
  echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
  echo "repo=$repo_real"
  exit 0
fi

# **base 必须已存在**：数据卷挂载点/临时根目录是运维先备好的，不由本脚本代建。
# 代建会把产物静默写到启动卷/系统盘上（`/Volumes/DataExt` 没挂载时，`mkdir -p` 会在
# 启动卷上建出一个同名目录，然后照常构建）——那正是这条政策要禁的形态。
if [ ! -d "$base" ]; then
  echo "❌ base 目录不存在：$base" >&2
  echo "   数据卷没挂载？本脚本不会替你创建，以免产物静默落到启动卷/系统盘。" >&2
  echo "   先准备好该目录（macOS: mkdir -p /Volumes/DataExt/tmp；Windows: mkdir -p /e/tmp），" >&2
  echo "   或用 ABB_BUILD_TMP 指到一个已存在的 checkout 外目录。" >&2
  exit 1
fi

# 只在真跑门禁时建产物子目录：`--print-env`（含 macOS 口径探测）不写磁盘。
mkdir -p "$CARGO_TARGET_DIR"

# `crates/abb-chart` 的 target **另开一个子目录**：与 root 共用同一个目录会重演上面那段
# 「cargo 文件锁互相饿等」（不同包、不同 feature 集，锁的粒度却是整目录）。
chart_manifest="$repo_root/crates/abb-chart/Cargo.toml"
chart_target="$CARGO_TARGET_DIR-chart"
mkdir -p "$chart_target"

cd "$repo_root"

# **渠道 crate 必须先建**：root 组第 4 条（`check_test_isolation.sh`）会跑
# `src/mcp_channel_dir_e2e_test.rs` 里那三条 `abb_negotiates_real_*`，
# 它们要找渠道二进制。建在后面 ⇒ 那三条 e2e 永远 skip。
# 评审 a4040fdf 抓到的就是这个：我把 export 放在了测试跑完之后。
# crates/abb-channel 同样是独立包，root 的四条也碰不到它。
#
# **为什么必须加这一组**（2026-11-03 实测）：那个包从建起来到功能做完，
# 一直是「有测试、但没人跑」——`cargo test` 得显式带 `--manifest-path` 才会跑到它。
# 这正是本仓 `docs/false-green-checklist.md` 第 4 种形态
# （**守卫覆盖面没跟上代码增长**）：代码加了新包，门禁清单没跟上，等于没有守卫。
#
# 不用 `check_test_isolation.sh`：那个守卫查「测试不得写 ~/.agent-bridge」，
# 而这个包的测试只起子进程、不碰运行目录。
# 目标目录另开 `-wechat` 子目录，理由同 chart（cargo 的锁粒度是整目录）。
# ── 三个渠道合一个二进制（2026-11-04）────────────────────────────────
#
# 过去这里是三组同构的 `--manifest-path` 门禁（wechat / feishu / dingtalk），
# 因为三个渠道各是一个独立包。现在它们合成 `crates/abb-channel` 一个包、
# 一个二进制（`--transport` 分派），所以门禁也收成一组。
#
# **为什么合并**：三份二进制各自链一次 `rustls + ring + webpki-roots`，
# 那约 4 MB 的 HTTPS 地板付了三遍（实测 5.5 + 5.1 + 4.7 = 15.3 MB 未 strip）。
# 合起来链接一次 ⇒ 实测 6.9 MB（strip 后 5.5）。业务代码一行没改，契约没动。
#
# 独立包的理由不变：自带 manifest + lock，root 的四条碰不到它。
# 目标目录另开 `-channel` 子目录，理由同 chart（cargo 的锁粒度是整目录）。
channel_manifest="$repo_root/crates/abb-channel/Cargo.toml"
channel_target="$CARGO_TARGET_DIR-channel"
mkdir -p "$channel_target"

echo "== ABB 门禁：crates/abb-channel（独立包，中间产物外置于 ${channel_target}）=="
(
  export CARGO_TARGET_DIR="$channel_target"
  cargo fmt --manifest-path "$channel_manifest" --check
  cargo clippy --manifest-path "$channel_manifest" --locked --all-targets -- -D warnings
  cargo build --manifest-path "$channel_manifest" --locked
  # 集成测试要起真子进程（tests/*_contract.rs、tests/transport_dispatch.rs），
  # `--locked` 保证不临时拉新版本。
  cargo test --manifest-path "$channel_manifest" --locked
)

# 把产物路径告诉 root 的测试：`src/mcp_channel_dir_e2e_test.rs` 里三条
# `abb_negotiates_real_*` 要起真子进程验「ABB 的协商器能接上它」。
# 三个渠道共用同一个 `abb-channel`，靠 `--transport` 分派 ⇒ 只传一个路径。
#
# 不传的话那三条 e2e 永远走 `[skip]`——而它们看起来在门禁里。
CHANNEL_BIN="$channel_target/debug/abb-channel"
if [ -x "$CHANNEL_BIN" ]; then
  export ABB_CHANNEL_BIN="$CHANNEL_BIN"
else
  echo "⚠️  没构建出 $CHANNEL_BIN ⇒ root 的三条渠道 e2e 将跳过（不是失败）"
fi

echo "== ABB 门禁：root 包（中间产物外置于 ${CARGO_TARGET_DIR}）=="
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
# 隔离测试的输出要**留下来**：下面那条断言要 grep 里面的标记。
# 用 tee 而不是重定向，是为了失败时人还能看到完整测试输出。
iso_log="${CARGO_TARGET_DIR}/iso-test.log"
mkdir -p "$(dirname "$iso_log")"
tools/check_test_isolation.sh 2>&1 | tee "$iso_log"

# **断言渠道跨包 e2e 真的跑了**（评审 902033a 的 non-blocking）。
#
# 为什么需要这一条：`src/mcp_channel_dir_e2e_test.rs` 里那三条 e2e 只有在
# 「拿到 `abb-channel` 二进制、真起了子进程、走完协商」之后才打印
# `ABB_*_E2E_RAN`。而它们**找不到产物时会静默 return（走 skip）**——
# 测试照样 `ok`，门禁照样绿。
#
# 前面虽然把渠道 crate 的构建前移到了 root 组之前、也 export 了 `ABB_CHANNEL_BIN`，
# 但**没有任何东西检查这个标记有没有真的出现**。export 哪天再退化（顺序改了、
# 路径改了、产物没建），门禁会**静默变绿**——这正是
# `docs/false-green-checklist.md` 第 8 种形态。
#
# 所以这里真的消费它。产物不存在是**环境问题**（不是代码问题），报 ⚠️ 并让人看见；
# 产物存在但标记没出现，那是**门禁接线坏了**，报红。
# **判据看产物文件本身，不看 env 变量**：export 退化时 env 为空，
# 若用 env 判就会误判成"环境没产物"而降级成警告——变异实测逃逸过。
# 产物是否存在是客观事实，env 是否传是接线问题。
if [ -x "$channel_target/debug/abb-channel" ]; then
    if grep -q 'ABB_WECHAT_E2E_RAN' "$iso_log"; then
        echo "✅ 微信跨包 e2e 真的跑了（找到 ABB_WECHAT_E2E_RAN 标记）"
    else
        echo "❌ 产物存在但微信 e2e 没跑：日志里没有 ABB_WECHAT_E2E_RAN" >&2
        echo "   这不是环境问题——${ABB_WECHAT_BIN} 是可执行的，测试却走了 skip。" >&2
        echo "   查 gates.sh 里渠道 crate 组与 export 的顺序（必须先构建再 export，再跑 root 测试）。" >&2
        exit 1
    fi
else
    echo "⚠️  没有微信 crate 产物（${channel_target}/debug/abb-channel）⇒ 该 e2e 未跑"
    echo "   这是环境问题，不是代码问题；但门禁对这条 e2e 的覆盖是**缺失**的。"
fi

# 飞书同理：判据看**产物文件本身**（不看 env——env 退化时看它会把接线问题误判成环境问题，
# 微信那次变异实测逃逸过）。
if [ -x "$channel_target/debug/abb-channel" ]; then
    if grep -q 'ABB_FEISHU_E2E_RAN' "$iso_log"; then
        echo "✅ 飞书跨包 e2e 真的跑了（订阅路径协商成功）"
    else
        echo "❌ 产物存在但飞书 e2e 没跑：日志里没有 ABB_FEISHU_E2E_RAN" >&2
        echo "   查 gates.sh 里渠道 crate 组与 export 的顺序（必须先构建再 export，再跑 root 测试）。" >&2
        exit 1
    fi
fi

# 钉钉同理（判据看产物文件，不看 env）。
if [ -x "$channel_target/debug/abb-channel" ]; then
    if grep -q 'ABB_DINGTALK_E2E_RAN' "$iso_log"; then
        echo "✅ 钉钉跨包 e2e 真的跑了（订阅路径协商成功）"
    else
        echo "❌ 产物存在但钉钉 e2e 没跑：日志里没有 ABB_DINGTALK_E2E_RAN" >&2
        exit 1
    fi
fi

# 兜底：清掉 TMPDIR 里陈旧的测试夹具（见脚本头注；根因修复在 src/testtmp.rs 的 RAII 归属）。
tools/clean_test_fixtures.sh

# crates/abb-chart 是独立包，root 的四条碰不到它（见头注）⇒ 这里补一组 `--manifest-path` 门禁。
# 测试不用 tools/check_test_isolation.sh：那个守卫查的是「测试不得写 ~/.agent-bridge」（root 包
# 有大量运行数据落点），而 chart 只管出图、不碰运行目录；它要的是**离线可跑**（`--offline`）：
# 随包分发的 CLI 不该为了跑单测去联网。cargo 跑测试二进制时 cwd 就是包根，manifest 仍用绝对
# 路径（免得将来有人在这段前面 `cd` 到别处，静默跑错包）。
echo "== ABB 门禁：crates/abb-chart（独立包，中间产物外置于 ${chart_target}）=="
(
  export CARGO_TARGET_DIR="$chart_target"
  cargo fmt --manifest-path "$chart_manifest" --check
  cargo clippy --manifest-path "$chart_manifest" --locked --all-targets -- -D warnings
  cargo build --manifest-path "$chart_manifest" --locked
  cargo test --manifest-path "$chart_manifest" --offline
)

# 协作门禁（crates/* 之外，跑得快、不联网、不编译）：这两条守的是「合并闸门本身」，
# 之前只在 .walgit/ci.toml 里跑，本地 gates.sh 漏了 —— 而协调者合并前跑的是 gates.sh
# （评审 NB-6）。放在最后：前面任何一条失败都不会掩盖它们。
echo "== ABB 门禁：协作闸门自测（不联网、不编译）=="
tools/review-cadence/selftest.sh
tools/check_collab_parents.sh --self-test
# 架构文档防腐：docs/architecture.md 里的规模数字与结构断言必须与代码一致。
# 不加这条的话，图会烂成「看着对、其实早不是这样」——而人不会主动去核数字。
# 它不联网、不编译，只读行数与几个符号，毫秒级。
tools/arch-check.sh
# 注释声称的事实要与代码一致。2026-11-03 一天里三次「改了代码没改叙述」
# 全部由人工评审才发现（正文「上面五条」没跟着变七条/八条、删了模块级 allow
# 但模块文档还写着它、紧词表但判据注释留着旧词表）。
#
# 机器能查的数字从来没漏过（arch-check 断言每个数字出现恰好一次）；漏的全是散文。
# 这个脚本是最小版本，只查几条**对应过真实事故**的断言——不是通用语义检查。
tools/check-prose-claims.sh
# 「被门禁裸路径调用的脚本必须是 755」——同一个错 2026-11-02 一天犯了两次
# （mutation-selftest.sh 与 arch-check.sh 先后提交成 644）。两次都是**本地绿、
# 干净检出红**（chmod 只改工作区，git 索引仍 644），所以「跑一遍门禁」发现不了它。
# 它读的是 git ls-files 的 mode，不是文件系统权限。
# gates.sh 自己的**顺序约束**：crate 组 → 各自 export → root 组。
# 这个顺序 2026-11-03 一天错了两次（微信的 export 在测试之后、飞书的整组在 root 之后），
# 两次都表现为「跨包 e2e 静默 skip」——而 skip 默认等于通过。
tools/check_gates_order.sh
tools/check_script_modes.sh
# 「渠道 crate 声明的工具名」与「契约 TOOL_* 常量」逐字对齐。
# 写错一个字母的后果不是报错而是**静默降级**：ABB 调 get_quoted_message、
# crate 声明 get_quote_message ⇒ has_tool 返回 false ⇒ 回落 trait 默认（返回 None）
# ⇒ 引用消息悄悄不进 prompt，日志里什么也看不到。
# 双向查：crate 多出契约没有的名字（拼错/自造）、契约有名字但没人实现。
tools/check_channel_tool_names.sh
# 渠道二进制必须仍是**一个**（`--transport` 分派三渠道）。
# 2026-11-04 把三个 crate 合成一个，动机是体积：三份二进制各自链一次
# rustls+ring+webpki-roots，约 4 MB 的 HTTPS 地板付了三遍（15.3 MB → 6.9 MB）。
# 拆回去在功能/测试里**都不会报错**，只在发版体积上悄悄变差 ⇒ 让脚本盯。
tools/check_channel_layout.sh
# `app-assets/ABB.iss` 在 macOS 上**编译不了**（ISCC 只有 Windows 有），所以它的语法错
# 在本地门禁里完全看不见，只有发版时才会在 Windows release job 里炸。
# 2026-11-05 真实事故：给 [UninstallRun] 加 abb-channel 时把 `TASK: "abb-channel.exe";`
# 插进了 [Code] 的 Pascal 函数体 ⇒ 本地 40 项全绿，Windows 安装包直接没产出。
# 这条只查文本层能确定的错（段名白名单/缩进段头/段专属行串段/begin-end 配对），
# 不假装是 ISCC——但那几类恰好是"插错位置"的全部形态。
tools/check_iss_structure.sh
# 变异自测：证明上面那些自测**真的能咬住回归**（评审 B3 的教训——手工变异逃逸过两次）。
# 它在临时副本里改代码、跑自测、销毁，主检出不受影响。
tools/review-cadence/mutation-selftest.sh

echo "✅ 八条门禁 + 渠道布局/工具名/文案断言 + 协作闸门自测全绿（root 四条 + crates/abb-channel 四条 + crates/abb-chart 四条 + review-cadence/parents 自测）；checkout 内未产生 target/（中间产物在 ${CARGO_TARGET_DIR} 与 ${chart_target}）；测试夹具残留已清扫"
