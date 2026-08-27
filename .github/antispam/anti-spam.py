#!/usr/bin/env python3
"""ABB 防广告评论扫描器。

扫描 issue 评论 + PR review 评论中的推广 spam：
- 高置信（命中 2+ 特征）→ 自动删除 + 记录审计
- 中置信（命中 1 特征）→ 仅输出报告（workflow 日志），不删除
- 协作者/机器人/owner 评论一律跳过

权限说明：GITHUB_TOKEN 可删评论（issues/pull-requests write）。
block 用户需要带 user scope 的 PAT——本脚本只提示，不执行 block。
"""
import json, os, re, subprocess, sys

REPO = os.environ.get("REPO", "")
GH = ["gh", "api"]
BOTS = {"github-actions[bot]", "dependabot[bot]", "renovate[bot]", "gqf2008"}

# 推广关键词（中文高频）
AD_KEYWORDS = [
    "加微信", "加v", "加 q", "客服", "低价", "代充", "代购", "返利", "秒到",
    "破解", "外挂", "免费领取", "注册送", "点击领取", "内部渠道", "稳定出",
    "包赔", "带单", "导师", "稳赚", "日入", "躺赚", "兼职", "刷单",
]
# 外链域名白名单（项目相关/常见正常链接）
ALLOW_DOMAINS = [
    "github.com", "raw.githubusercontent.com", "gist.github.com", "api.github.com",
    "developers.openai.com", "code.claude.com", "docs.anthropic.com",
    "docs.astral.sh", "crates.io", "docs.rs", "rust-lang.org",
    "learn.microsoft.com", "docs.rs", "npmjs.com", "nodejs.org",
]

def run_gh(args):
    r = subprocess.run(GH + args, capture_output=True, text=True)
    if r.returncode != 0:
        return None
    return r.stdout

def fetch_comments(endpoint):
    out = run_gh([endpoint, "--paginate", "--jq", ".[] | {id, user: .user.login, body, created_at}"])
    if not out:
        return []
    return [json.loads(l) for l in out.splitlines() if l.strip()]

def collaborators():
    out = run_gh(["repos/" + REPO + "/collaborators", "--jq", ".[].login"])
    return set(out.splitlines()) if out else set()

def score(c, collab):
    body = c.get("body") or ""
    user = c.get("user") or ""
    if user in BOTS or user in collab:
        return 0, []
    hits = []
    # 特征1：外链（指向非白名单域名）
    links = re.findall(r"https?://([^/\s\"'>]+)", body)
    bad_links = [d for d in links if not any(d == w or d.endswith("." + w) for w in ALLOW_DOMAINS)]
    if bad_links:
        hits.append(f"外链({bad_links[:2]})")
    # 特征2：推广关键词
    kw = [k for k in AD_KEYWORDS if k.lower() in body.lower()]
    if kw:
        hits.append(f"关键词({kw[:3]})")
    # 特征3：加密货币地址 / 乱码
    if re.search(r"\b(1[1-9A-HJ-NP-Za-km-z]{25,34}|0x[0-9a-fA-F]{40}|T[A-Za-z0-9]{33})\b", body):
        hits.append("加密货币地址")
    if len(hits) == 0 and body and len(set(body)) < 8 and len(body) > 20:
        hits.append("疑似乱码")
    return len(hits), hits

def delete_comment(cid, kind):
    ep = "issues/comments" if kind == "issue" else "pulls/comments"
    subprocess.run(GH + [f"repos/{REPO}/{ep}/{cid}", "-X", "DELETE"], capture_output=True, text=True)

def main():
    collab = collaborators()
    audit = []
    for kind, ep in [("issue", "repos/%s/issues/comments" % REPO),
                     ("pr", "repos/%s/pulls/comments" % REPO)]:
        for c in fetch_comments(ep):
            n, hits = score(c, collab)
            if n >= 2:
                delete_comment(c["id"], kind)
                audit.append(f"[已删除] {c['user']} @{c['created_at']} {kind} #{c['id']} 特征:{hits}")
                print(f"DELETED {c['user']} {c['id']} {hits}", file=sys.stderr)
            elif n == 1:
                print(f"LOW-SUSPECT {c['user']} {c['id']} {hits} (未删，仅报告)", file=sys.stderr)
    if audit:
        print("== 本次删除 ==")
        print("\n".join(audit))
    else:
        print("OK: 未发现需删除的广告评论")

if __name__ == "__main__":
    main()
