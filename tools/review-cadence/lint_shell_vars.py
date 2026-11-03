#!/usr/bin/env python3
"""扫「`$var` 紧跟非 ASCII 字符」这种 bash 陷阱，命中则按 `文件:行` 逐行输出。

## 为什么必须是一个独立的检查（card abb-d68-local-commands-20261102）

写 `echo "pass=$pass）"`（变量后面直接跟全角括号/逗号）时，bash 会把多字节字符
算进变量名，于是报 `pass�: unbound variable`。真正危险的不是报错本身，而是：

1. `set -u` 触发后，**EXIT trap 里看到的 `$?` 是 0**；
2. 因此脚本以 **0** 退出 —— 门禁内部出错，却对外报「通过」。

本仓已经踩过三次（`$pass）` / `$fail）` / `$mcp_rc）`，其中 `mcp_rc` 那次直接
造成「MCP 调用失败 ⇒ 门禁 exit 0 判绿」，正是最不能接受的那类假绿。
所以把它做成机器可查的 lint，而不是靠记性。

## 用法

    git ls-files '*.sh' | python3 tools/review-cadence/lint_shell_vars.py

stdin 每行一个文件路径；命中输出 `path:line`，无命中输出空（退出码 0）。

## 不算违规的情况

- 行首（允许前置空白）是 `#` 的注释行：注释里提到这个模式本身不是 bug。
- 已经写成 `${var}` 的：`{` 不是标识符字符，天然不会被匹配。
"""

import pathlib
import re
import sys

# `$` + 标识符 + 任意非 ASCII 字节。用字段拼接构造，避免本文件自己被匹配。
_PATTERN = re.compile(r"\$" + r"([A-Za-z_][A-Za-z0-9_]*)" + r"([^\x00-\x7f])")


def scan(path: pathlib.Path) -> list[str]:
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return []
    hits: list[str] = []
    for lineno, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        if _PATTERN.search(line):
            hits.append(f"{path}:{lineno}")
    return hits


def main() -> int:
    bad: list[str] = []
    for raw in sys.stdin:
        name = raw.strip()
        if not name:
            continue
        bad.extend(scan(pathlib.Path(name)))
    for item in bad:
        print(item)
    return 0


if __name__ == "__main__":
    sys.exit(main())
