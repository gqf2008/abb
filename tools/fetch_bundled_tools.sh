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

# 宿主判定：Windows 产物在非 Windows 宿主上**不能执行**（PE），只能断言"存在且非空"。
# 本脚本允许在 macOS/Linux 上用 --platform windows-x64 做"取件链路"的干跑（CI 之外的自证路径）。
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) host_windows=1 ;;
  *) host_windows=0 ;;
esac

# Windows 产物带 .exe 后缀；多调包按名落文件与验活都用它。
if [ "$PLATFORM" = "windows-x64" ]; then suffix=".exe"; else suffix=""; fi

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
      # 多调包（uutils coreutils / diffutils / busybox-w32）：一个包暴露多个工具名，三种形态：
      #   ① 单个裸二进制（busybox-w32，multicall-raw）——asset 本身就是多调 exe；
      #   ② 归档里一个多调二进制（uutils diffutils）；
      #   ③ 归档里"每工具一个 exe"（uutils coreutils 的 Windows zip）+ 一个 coreutils.exe 多调本体。
      # 统一流程：先把**包产物**（output_name）落到 bin/，再按清单为每个名字取"包内同名文件"或
      # 硬链接到包产物；两者都拿不到就报错（绝不静默少一个工具）。
      # 注意：本脚本是 zsh，**未加引号的标量参数不会被词分割**（只有命令替换会）——
      # 所以一律内联 `$(awk …)`，不要先赋值再 `for name in $var`（那样只迭代一次）。
      provider="$tool"
      names_for_platform() {
        awk -F'\t' -v p="$provider" -v plat="$PLATFORM" '$2==p && ($3=="all" || $3==plat) {print $1}' tools/bundled-cli-tools.tsv
      }
      [ -n "$(names_for_platform)" ] || {
        echo "$provider 在 tools/bundled-cli-tools.tsv 里没有任何名字（$PLATFORM）" >&2; exit 1;
      }
      mkdir -p "$tmp/extract/$tool"
      case "$package" in
        multicall-raw) cp "$file" "$tmp/extract/$tool/$output_name" ;;  # 裸 exe：产物名就是 output_name（含 .exe）
        multicall-tar-gz) tar -xzf "$file" -C "$tmp/extract/$tool" ;;
        multicall-tar-xz) tar -xJf "$file" -C "$tmp/extract/$tool" ;;
        multicall-zip) unzip -q "$file" -d "$tmp/extract/$tool" ;;
      esac
      # 1) 包产物：归档里按 output_name 找（可能在带目录的前缀下）；裸 exe 就是它自己。
      product="$tmp/extract/$tool/$output_name"
      [ -f "$product" ] || product="$(find "$tmp/extract/$tool" -type f -name "$output_name" | head -1)"
      [ -n "$product" ] && [ -f "$product" ] || {
        echo "$provider 包里找不到包产物 $output_name" >&2; exit 1;
      }
      cp "$product" "$DEST/bin/$output_name"
      chmod 0755 "$DEST/bin/$output_name" 2>/dev/null || true
      # 2) 按清单为每个名字落文件：优先"包内同名文件"，否则硬链接（失败回落复制）。
      for name in $(names_for_platform); do
        target="$DEST/bin/$name$suffix"
        [ -f "$target" ] && continue
        per="$(find "$tmp/extract/$tool" -type f -name "$name$suffix" | head -1)"
        if [ -n "$per" ]; then
          cp "$per" "$target"
        else
          ln -f "$DEST/bin/$output_name" "$target" 2>/dev/null || cp "$DEST/bin/$output_name" "$target"
        fi
      done
      # 3) 验活：Windows 产物在非 Windows 宿主上只断言"存在且非空"（不能执行 PE）；
      #    Windows 宿主（MSYS/MinGW/Cygwin 的 bash）与 macOS 一样做真执行检查（下面统一做）。
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
  # 词分割/解析双坑（两次踩到，第三次改写法）：
  #  ① zsh **不对标量参数词分割**（`for x in $var` 只迭代一次），而 bash 会 —— 两边行为不同；
  #  ② `$(case … *) … ;; esac)` 在 **bash** 里会把模式后的 `)` 当成 `$( )` 的收尾 ⇒ 语法错
  #     （评审 F4 实测：`bash tools/fetch_bundled_tools.sh` 起不来，而 release.yml 正是用 bash 调它）。
  # 所以统一改成"把名字写进临时文件 + while read"，两种 shell 行为一致。
  if [ "$package" != "${package#multicall-}" ]; then
    awk -F'\t' -v p="$tool" -v plat="$PLATFORM" '$2==p && ($3=="all" || $3==plat) {print $1}' \
      tools/bundled-cli-tools.tsv > "$tmp/check-names.txt"
  else
    echo "$output_name" > "$tmp/check-names.txt"
  fi
  while IFS= read -r name; do
    # 非多调包的 output_name 可能**已含** .exe（rg.exe/coreutils.exe…）⇒ 不能无脑再拼后缀
    #（Windows 干跑实测：拼出 rg.exe.exe 报"缺产物"）。
    case "$name" in
      *"$suffix") target="$DEST/bin/$name" ;;
      *) target="$DEST/bin/$name$suffix" ;;
    esac
    [ -f "$target" ] || { echo "缺产物：$target" >&2; exit 1; }
    if [ "$PLATFORM" = "windows-x64" ] && [ "$host_windows" != "1" ]; then
      # 非 Windows 宿主上的 Windows 产物：不能执行（PE），断言非空即可（真正的执行检查在 Windows CI）。
      [ -s "$target" ] || { echo "$target 为空文件" >&2; exit 1; }
      continue
    fi
    # 真执行验活：**不能依赖 `--version`/`--help`**——uutils diffutils 对两者都报
    # "Unknown option"（与 GNU diff 有差异）。只要求"能执行"：无参跑一次，
    # 退出码 126/127（不可执行/找不到，POSIX 约定）才算失败。
    rc=0
    "$target" </dev/null >/dev/null 2>&1 || rc=$?
    if [ "$rc" -eq 126 ] || [ "$rc" -eq 127 ]; then
      echo "$name 不可执行（rc=$rc）" >&2; exit 1
    fi
  done < "$tmp/check-names.txt"
done < tools/tool-lock.tsv

# 计数 = lock 里该平台的**包**数（多调包一个包提供多个工具名，故不再与工具名数相等）。
[ "$found" -eq "$(awk -F'\t' -v p="$PLATFORM" 'NR>1 && $3==p {n++} END {print n}' tools/tool-lock.tsv)" ] || {
  echo "lock 中 $PLATFORM 的包数不符（found=$found）" >&2; exit 1;
}
echo "  [tools] 已准备 $found 个工具到 $DEST"

# 显式以 0 退出（对称加固，卡 abb-windows-ps1-exit-code-leak-20261012）：现在末尾是 `echo`（内建，返回 0）
# 所以不出问题；但若将来把多调包 smoke 循环挪到末尾，最后一条**原生命令**的非 0 退出码会变成脚本退出码
# （PowerShell 版就是因为这个在 CI 里挂了）。出错路径仍由 `exit 1` 负责。
exit 0
