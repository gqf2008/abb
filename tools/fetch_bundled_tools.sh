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

下载 rg/jq/uv/gh 到 <目录>/bin，并复制许可证到 <目录>/licenses。
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
  "$DEST/bin/$output_name" --version >/dev/null
done < tools/tool-lock.tsv

[ "$found" -eq 4 ] || { echo "lock 中 $PLATFORM 应恰好有 4 个工具，实际 $found" >&2; exit 1; }
echo "  [tools] 已准备 $found 个工具到 $DEST"
