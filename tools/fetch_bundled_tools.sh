#!/bin/zsh
# 按 tools/tool-lock.tsv 下载并校验随包工具。
# 用法：tools/fetch_bundled_tools.sh --platform macos-arm64 --dest <dir>
set -euo pipefail

cd "$(dirname "$0")/.."

PLATFORM=""
DEST=""

usage() {
  cat <<'EOF'
用法: tools/fetch_bundled_tools.sh --platform <macos-arm64> --dest <目录>

下载 rg/jq/uv/gh 与随包 CLI 工具（coreutils/diffutils 的多调二进制，见
tools/bundled-cli-tools.tsv）到 <目录>/bin，并复制许可证到 <目录>/licenses。
版本、URL、SHA256、包内路径全部来自 tools/tool-lock.tsv。
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --platform) PLATFORM="${2:-}"; shift 2 ;;
    --dest) DEST="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "未知参数：$1" >&2; usage >&2; exit 2 ;;
  esac
done

[ -n "$PLATFORM" ] || { usage >&2; exit 2; }
[ -n "$DEST" ] || { usage >&2; exit 2; }
[ -f tools/tool-lock.tsv ] || { echo "缺 tools/tool-lock.tsv" >&2; exit 1; }

tmp="$(mktemp -d "${TMPDIR:-/tmp}/abb-bundled-tools.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$DEST/bin" "$DEST/licenses"
cp tools/licenses/* "$DEST/licenses/"

found=0
while IFS=$'\t' read -r tool version platform package url sha256 inner_path output_name; do
  [ "$tool" = "tool" ] && continue
  [ "$platform" = "$PLATFORM" ] || continue
  found=$((found + 1))

  echo "  [tools] $tool $version -> $output_name"
  file="$tmp/${output_name}.${package}"
  curl --fail --location --silent --show-error --retry 3 --connect-timeout 15 -o "$file" "$url"

  actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  if [ "$actual" != "$sha256" ]; then
    echo "SHA256 校验失败：$tool $version" >&2
    echo "  expected=$sha256" >&2
    echo "  actual  =$actual" >&2
    exit 1
  fi

  mkdir -p "$tmp/extract/$tool"
  case "$package" in
    raw)
      cp "$file" "$DEST/bin/$output_name"
      ;;
    tar.gz)
      entry="$(tar -tzf "$file" | awk -v p="$inner_path" '$0==p || $0 ~ ("/" p "$") {print; exit}')"
      [ -n "$entry" ] || { echo "包内找不到 $inner_path（$tool）" >&2; exit 1; }
      tar -xOzf "$file" "$entry" >"$DEST/bin/$output_name"
      ;;
    zip)
      entry="$(unzip -Z1 "$file" | awk -v p="$inner_path" '$0==p || $0 ~ ("/" p "$") {print; exit}')"
      [ -n "$entry" ] || { echo "包内找不到 $inner_path（$tool）" >&2; exit 1; }
      unzip -p "$file" "$entry" >"$DEST/bin/$output_name"
      ;;
    source-tar)
      # jq 官方预编译 macOS arm64 从 1.8.2 起 minOS=14，与 ABB 的 macOS 12
      # 下限不兼容；从官方源码以 MACOSX_DEPLOYMENT_TARGET=12.0 静态构建。
      tar -xzf "$file" -C "$tmp/extract/$tool" \
        --exclude 'jq-1.8.2/docs' --exclude 'jq-1.8.2/tests'
      build="$tmp/extract/$tool/jq-1.8.2"
      build_log="$tmp/jq-build.log"
      (
        cd "$build"
        MACOSX_DEPLOYMENT_TARGET=12.0 ./configure \
          --with-oniguruma=builtin --disable-maintainer-mode \
          --disable-shared --enable-static
        # docs 被排除后 manpage 子目标会失败；-k 继续并只认最终 jq 产物。
        MACOSX_DEPLOYMENT_TARGET=12.0 make -k -j4 || true
      ) >"$build_log" 2>&1
      src="$build/$inner_path"
      if [ ! -x "$src" ]; then
        tail -120 "$build_log" >&2
        echo "源码构建未产出 $inner_path（$tool）" >&2
        exit 1
      fi
      cp "$src" "$DEST/bin/$output_name"
      ;;
    multicall-raw|multicall-tar-gz|multicall-tar-xz|multicall-zip)
      # 多调包（uutils coreutils / diffutils）：
      #  - macOS 资产是**单个多调二进制** ⇒ 解到 bin/<provider>，再按清单建硬链接
      #    （同卷免权限；硬链接失败回落复制并告警——绝不静默少一个工具）；
      #  - Windows 的 coreutils 资产是**每工具一个 exe** ⇒ 直接取 <name>.exe，不需要链接；
      #    diffutils 仍是多调 ⇒ 走硬链接分支。
      provider="$tool"
      # 注意：本脚本是 zsh，**未加引号的标量参数不会被词分割**（只有命令替换会）。
      # 所以这里一律用内联 `$(awk ...)`，不要先赋值再 `for name in $var`（那样只迭代一次）。
      [ -n "$(awk -F'\t' -v p="$provider" -v plat="$PLATFORM" '$2==p && ($3=="all" || $3==plat) {print $1}' tools/bundled-cli-tools.tsv)" ] || {
        echo "$provider 在 tools/bundled-cli-tools.tsv 里没有任何名字" >&2; exit 1;
      }
      case "$package" in
        multicall-raw) cp "$file" "$tmp/extract/$tool/$tool" ;;
        multicall-tar-gz) tar -xzf "$file" -C "$tmp/extract/$tool" ;;
        multicall-tar-xz) tar -xJf "$file" -C "$tmp/extract/$tool" ;;
        multicall-zip) unzip -q "$file" -d "$tmp/extract/$tool" ;;
      esac
      if [ "$PLATFORM" = "windows-x64" ]; then
        # 原生 Windows 资产：包内若本来就按工具名分文件（如 uutils coreutils），取件即可；
        # 否则（多调 exe，如 busybox-w32）先把多调本体放到 bin/$output_name，再按名单建硬链接。
        for name in $(awk -F'\t' -v p="$provider" -v plat="$PLATFORM" '$2==p && ($3=="all" || $3==plat) {print $1}' tools/bundled-cli-tools.tsv); do
          per="$(find "$tmp/extract/$tool" -name "$name.exe" 2>/dev/null | head -1)"
          [ -n "$per" ] || { echo "$provider 包里没有 $name.exe" >&2; exit 1; }
          cp "$per" "$DEST/bin/$name.exe"
        done
        if [ ! -f "$DEST/bin/${output_name%.exe}.exe" ] && [ -f "$tmp/extract/$tool/$tool" ]; then
          cp "$tmp/extract/$tool/$tool" "$DEST/bin/$output_name"
          for name in $(awk -F'\t' -v p="$provider" -v plat="$PLATFORM" '$2==p && ($3=="all" || $3==plat) {print $1}' tools/bundled-cli-tools.tsv); do
            [ -f "$DEST/bin/$name.exe" ] && continue
            ln -f "$DEST/bin/$output_name" "$DEST/bin/$name.exe" 2>/dev/null || cp "$DEST/bin/$output_name" "$DEST/bin/$name.exe"
          done
        fi
      else
        src="$(find "$tmp/extract/$tool" -type f -name "$provider" | head -1)"
        [ -n "$src" ] || { echo "$provider 包里没有多调二进制 $provider" >&2; exit 1; }
        cp "$src" "$DEST/bin/$provider"
        chmod 0755 "$DEST/bin/$provider"
        for name in $(awk -F'\t' -v p="$provider" -v plat="$PLATFORM" '$2==p && ($3=="all" || $3==plat) {print $1}' tools/bundled-cli-tools.tsv); do
          ln -f "$DEST/bin/$provider" "$DEST/bin/$name" 2>/dev/null || {
            echo "  [tools] 硬链接 $name -> $provider 失败，回落复制" >&2
            cp "$DEST/bin/$provider" "$DEST/bin/$name"
          }
        done
      fi
      ;;
    *)
      echo "不支持的 package 类型：$package（$tool）" >&2
      exit 1
      ;;
  esac
  chmod 0755 "$DEST/bin/$output_name"
  if [ "$PLATFORM" = "macos-arm64" ]; then
    minos="$(otool -l "$DEST/bin/$output_name" | awk '/LC_BUILD_VERSION/{f=1} f&&/minos/{print $2; exit}')"
    major="${minos%%.*}"
    minor="${minos#*.}"
    [ "$minor" = "$minos" ] && minor=0
    case "$major" in
      ''|*[!0-9]*) echo "无法读取 minOS：$tool" >&2; exit 1 ;;
    esac
    case "$minor" in
      ''|*[!0-9]*) echo "无法读取 minOS：$tool" >&2; exit 1 ;;
    esac
    if [ "$major" -gt 12 ] || { [ "$major" -eq 12 ] && [ "$minor" -gt 0 ]; }; then
      echo "$tool minOS=$minos > 12.0，与 ABB 的 LSMinimumSystemVersion 不兼容" >&2
      exit 1
    fi
  fi
  case "$package" in
    multicall-*)
      # 多调包：**逐个名字**验活（这同时验证了硬链接/按名取件都对）。
      # 注意不能拿包名当工具名跑（`diffutils --version` 会得到 "utility not supported"）。
      for name in $(awk -F'\t' -v p="$tool" -v plat="$PLATFORM" '$2==p && ($3=="all" || $3==plat) {print $1}' tools/bundled-cli-tools.tsv); do
        # 验活：**不能依赖 `--version`/`--help`**——uutils diffutils 对两者都报
        # "Unknown option"（与 GNU diff 行为有差异）。只要求"能执行"：
        # 无参跑一次，退出码 126/127（不可执行/找不到，POSIX 约定）才算失败。
        rc=0
        "$DEST/bin/$name" </dev/null >/dev/null 2>&1 || rc=$?
        if [ "$rc" -eq 126 ] || [ "$rc" -eq 127 ]; then
          echo "$name 不可执行（rc=$rc，多调包 $tool 的按名调用不通）" >&2; exit 1
        fi
      done
      ;;
    *)
      "$DEST/bin/$output_name" --version >/dev/null
      ;;
  esac
done < tools/tool-lock.tsv

# 计数 = lock 里该平台的**包**数（多调包一个包提供多个工具名，故不再与工具名数相等）。
[ "$found" -eq "$(awk -F'\t' -v p="$PLATFORM" 'NR>1 && $3==p {n++} END {print n}' tools/tool-lock.tsv)" ] || {
  echo "lock 中 $PLATFORM 的包数不符（found=$found）" >&2; exit 1;
}
echo "  [tools] 已准备 $found 个工具到 $DEST"
