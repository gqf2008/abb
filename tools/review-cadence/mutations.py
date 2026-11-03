#!/usr/bin/env python3
"""变异清单：每个变异「改哪个文件的哪一段」，供 mutation-selftest.sh 驱动。

## 为什么单独成文件

shell 里的引号/转义对「跨行替换 + 中文 + 正则元字符」极其脆：本清单里 M6 一开始写在
bash 的 `sed -E` 表达式里，`/^    exit 2$/` 会同时命中文件里其它 `exit 2`
（比如 `cd "$repo_root" || exit 2`），把脚本改崩而不是改到目标分支 —— 于是「变异逃逸」
是假的。改成 Python 的整段字符串替换后，语义精确：**匹配不到就报错，而不是猜**。

## 每一项

  name    给人看的名字
  file    相对 repo 根的文件
  old     必须**恰好出现一次**的原文（出现 0 次或多次都算清单过期，直接报错）
  new     替换成什么
  why     这条变异代表哪种回归（对应哪条不变量）
"""

MUTATIONS = [
    {
        "name": "M7 MCP 调用失败也允许回落 CLI",
        "file": "tools/check_review_cadence.sh",
        "old": 'if [ "$mcp_rc" -ne 3 ]; then',
        "new": 'if [ "$mcp_rc" -gt 9 ]; then',
        "why": "本卡核心不变量：MCP 坏了不许换数据源判绿（评审用过的变异）",
    },
    {
        "name": "M5 判定依据硬编码成「已同步」",
        "file": "tools/check_review_cadence.sh",
        "old": 'basis="⚠ walgit CLI + 未同步（陈旧本地 refs，best-effort）"',
        "new": 'basis="walgit CLI + 已同步 refs/collab/*（过渡路径）"',
        "why": "「真同步过」与「拿陈旧 refs」不能在输出里长得一样（评审 M5）",
    },
    {
        "name": "M1 去掉 verdict_emitted 守卫",
        "file": "tools/check_review_cadence.sh",
        "old": 'if [ "$verdict_emitted" != "1" ] && [ "$rc" -eq 0 ]; then rc=2; fi;',
        "new": ':;',
        "why": "内部错误不得伪装成绿（set -u + EXIT trap 会把失败吞成 0）",
    },
    {
        "name": "M2 MCP 分支永远走不到",
        "file": "tools/check_review_cadence.sh",
        "old": 'if [ -n "${WALGIT_MCP_TOKEN:-}" ] && [ -f "$mcp_py" ]; then',
        "new": 'if false; then',
        "why": "本地没有 walgit 时门禁必须仍可用（本卡唯一目的）",
    },
    {
        "name": "M3 缺凭据也返回 4（破坏回落契约）",
        "file": "tools/review-cadence/mcp_thread.py",
        "old": "        return 3\n",
        "new": "        return 4\n",
        "why": "rc=3（缺凭据，可回落）与 rc=4（调用失败，不许回落）必须分开",
    },
    {
        "name": "M6 parents 守卫同步失败不再 fail-closed",
        "file": "tools/check_collab_parents.sh",
        "old": "    exit 2\n  fi\n  fetched=1",
        "new": "    :\n  fi\n  fetched=1",
        "why": "读到陈旧 refs 时不许判「全部合规」（与本卡门禁同源的陈旧假绿）",
        # 这个守卫的自测入口与 review-cadence 不同；跑错文件会让它永远"逃逸"。
        "runner": "tools/check_collab_parents.sh --self-test",
        # 这个 runner 的失败行用的是 `❌ …` / `── 自测失败`，没有 FAIL/FATAL 字样。
        # 判定「被咬住」要认它的标记，否则会被误判成「runner 起不来」。
        "fail_marker": "^(FAIL|FATAL|❌)|自测失败",
    },
    {
        "name": "M8 MCP 端点退回写死本机地址",
        "file": "tools/review-cadence/mcp_thread.py",
        "old": "    url, src = resolve_mcp_url()",
        "new": '    url, src = os.environ.get("WALGIT_MCP_URL", "http://127.0.0.1:8081/mcp").strip(), "hardcoded"',
        "why": "本机 walgit 服务退役后，写死 127.0.0.1 的端点必须被测试咬住（迁移即失效）",
    },
]

# 默认自测入口（没写 runner 的变异都用它）
DEFAULT_RUNNER = "tools/review-cadence/selftest.sh"


if __name__ == "__main__":
    # 供 shell 驱动：每行 `name<TAB>file<TAB>why`
    for m in MUTATIONS:
        print(f"{m['name']}\t{m['file']}\t{m.get('runner', DEFAULT_RUNNER)}\t{m.get('fail_marker', '')}")
