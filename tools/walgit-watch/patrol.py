#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""walgit 看板巡检（只读）。

两件事：
  1) 增量：比对本地 walgit 镜像 refs/collab/* 的上次快照，打印新增/变更/删除的协作条目。
  2) 存量：扫描本地镜像里所有 inbox 条目，找出「自家线程已 approve 未收口 / 已 needs-changes 未退回」
     这类**需要动作**的线程（增量比对看不到它们，因为条目早就在基线里了）。

无任何输出 = 无需动作、也无需报告（调用方据此保持静默）。

设计约束：
  - 只读：只做 git for-each-ref / ls-remote / fetch / cat-file，绝不执行 merge_result / status closed 等不可逆动作。
  - 不依赖 walgit CLI（collab board/report/thread 在本机 >120s 超时），只走镜子仓库的 git。
  - 状态文件：patrol-refs.json（ref -> sha）、patrol-backlog.json（thread -> 已报告的 action key）。
  - 只允许输出，不修改除上述两个状态文件以外的任何文件。
"""
import json
import os
import subprocess
import sys

BASE = os.path.dirname(os.path.abspath(__file__))
MIRROR = os.path.join(BASE, "abb-mirror.git")
STATE = os.path.join(BASE, "patrol-refs.json")
BACKLOG_STATE = os.path.join(BASE, "patrol-backlog.json")
OWN = os.path.join(BASE, "own-threads.json")
ME = "abb-win"
MAX_LINES = 20
BRIEF_KEYS = ("note", "work", "title", "summary", "branch", "head", "decision", "status")


def git(args, timeout=180, data=None):
    p = subprocess.run(["git", "-C", MIRROR] + args, capture_output=True, text=True,
                       encoding="utf-8", errors="replace", timeout=timeout, input=data)
    return p.returncode, p.stdout, p.stderr


def load_json(path, default):
    try:
        with open(path, "r", encoding="utf-8-sig") as f:
            return json.load(f)
    except Exception:
        return default


def save_json(path, obj):
    try:
        with open(path, "w", encoding="utf-8") as f:
            json.dump(obj, f, ensure_ascii=False)
    except Exception:
        pass


def remote_map():
    rc, out, err = git(["ls-remote", "origin", "refs/collab/*"], timeout=120)
    if rc != 0:
        raise RuntimeError("ls-remote rc=%s %s" % (rc, err.strip()[:300]))
    m = {}
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) == 2 and parts[1].startswith("refs/collab/"):
            m[parts[1]] = parts[0]
    return m


def fetch(refs):
    if not refs:
        return
    for i in range(0, len(refs), 40):
        spec = ["+%s:%s" % (r, r) for r in refs[i:i + 40]]
        git(["fetch", "--no-tags", "--quiet", "origin"] + spec)


def decode(sha):
    rc, out, err = git(["cat-file", "-p", sha], timeout=60)
    if rc != 0 or not out.strip():
        return None
    try:
        return json.loads(out)
    except Exception:
        return None


def brief(body, skip=()):
    if not isinstance(body, dict):
        return ""
    for k in BRIEF_KEYS:
        if k in skip:
            continue
        v = body.get(k)
        if isinstance(v, str) and v.strip():
            s = v.strip().replace("\r", " ").replace("\n", " ")
            return s[:120]
    return ""


# ---------------------------------------------------------------- 存量扫描

def local_inbox():
    rc, out, err = git(["for-each-ref", "--format=%(objectname)", "refs/collab/inbox"])
    if rc != 0:
        return []
    shas = [s.strip() for s in out.splitlines() if s.strip()]
    if not shas:
        return []
    rc, data, err = git(["cat-file", "--batch"], timeout=300, data="\n".join(shas) + "\n")
    if rc != 0:
        return []
    entries = []
    i = 0
    lines = data.split("\n")
    while i < len(lines):
        header = lines[i]
        i += 1
        if not header.strip():
            continue
        parts = header.split()
        if len(parts) < 3:
            continue
        size = int(parts[2])
        # 对象体可能跨多行：按字节数重组近似（JSON 里一般无裸换行）
        body = lines[i]
        i += 1
        # 简单稳妥：用 size 校验，不足则继续拼
        while len(body.encode("utf-8", errors="replace")) < size and i < len(lines):
            body += "\n" + lines[i]
            i += 1
        try:
            entries.append(json.loads(body))
        except Exception:
            pass
        # 跳过 batch 输出里对象之间的空行
        while i < len(lines) and lines[i] == "":
            i += 1
    return entries


def backlog_items(own_set):
    entries = local_inbox()
    by_thread = {}
    for e in entries:
        tid = e.get("id")
        if not tid:
            continue
        by_thread.setdefault(str(tid), []).append(e)

    items = []
    for tid, es in by_thread.items():
        es.sort(key=lambda x: x.get("ts") or 0)
        if tid not in own_set and not any(x.get("actor") == ME for x in es):
            continue
        last_status = None
        last_status_ts = 0
        for e in es:
            if e.get("kind") == "status":
                b = e.get("body") or {}
                st = b.get("status")
                if st:
                    last_status = str(st)
                    last_status_ts = e.get("ts") or 0
        if last_status != "needs-review":
            continue
        reviews = [e for e in es if e.get("kind") == "review" and (e.get("ts") or 0) > last_status_ts]
        if not reviews:
            continue
        approve = [e for e in reviews if (e.get("body") or {}).get("decision") == "approve"]
        changes = [e for e in reviews if (e.get("body") or {}).get("decision") == "needs-changes"]
        merged_by_other = [e for e in es if e.get("kind") == "merge_result" and e.get("actor") != ME]
        if approve:
            newest = max(approve, key=lambda x: x.get("ts") or 0)
            action = "close" if not merged_by_other else "skip-merged-by-other"
            items.append((tid, action, newest.get("actor"), newest.get("ts"),
                          (newest.get("body") or {}).get("note", "")))
        elif changes:
            newest = max(changes, key=lambda x: x.get("ts") or 0)
            items.append((tid, "reopen", newest.get("actor"), newest.get("ts"),
                          (newest.get("body") or {}).get("note", "")))
    items.sort(key=lambda x: x[3] or 0)
    return items



def dispatch_items(own_set):
    """扫描全局，找出「需要派人去协同完成」的线程：
      - review  : last status = needs-review 且其后没有任何 review -> 派独立评审人
      - claim   : 自家（owner=abb-win / own-threads）的 open issue 且尚无 in-progress -> 派认领实现
    返回 [(tid, action, actor, ts)]。
    """
    entries = local_inbox()
    by_thread = {}
    for e in entries:
        tid = e.get("id")
        if tid:
            by_thread.setdefault(str(tid), []).append(e)

    items = []
    for tid, es in by_thread.items():
        es.sort(key=lambda x: x.get("ts") or 0)
        has_issue = any(e.get("kind") == "issue" for e in es)
        last_status = None
        last_status_ts = 0
        for e in es:
            if e.get("kind") == "status":
                st = (e.get("body") or {}).get("status")
                if st:
                    last_status = str(st)
                    last_status_ts = e.get("ts") or 0
        if last_status == "needs-review":
            after = [e for e in es if e.get("kind") == "review" and (e.get("ts") or 0) > last_status_ts]
            if not after:
                items.append((tid, "review", str(es[-1].get("actor") or "-"), last_status_ts))
        elif last_status in (None, "open", "in-progress"):
            # 迁移线程的首条 entry 不一定是 issue（ts 同批、排序不定），按「线程内含 issue」判定
            if not has_issue:
                continue
            owner = None
            for e in es:
                b = e.get("body") or {}
                if isinstance(b, dict) and b.get("owner"):
                    owner = str(b.get("owner"))
            mine = (owner == ME) or (tid in own_set)
            if mine and last_status != "in-progress":
                items.append((tid, "claim", owner or ME, last_status_ts))
    items.sort(key=lambda x: x[3] or 0)
    return items


def main():
    no_state = "--no-state" in sys.argv
    own = load_json(OWN, [])
    if not isinstance(own, list):
        own = []
    own_set = set(str(x) for x in own)

    prev = load_json(STATE, None)
    rem = remote_map()

    if prev is None:
        if not no_state:
            save_json(STATE, rem)
        print("(baseline 建立：%d 个 refs，下次起只报增量)" % len(rem))
        return 0

    changed = sorted(r for r, v in rem.items() if prev.get(r) != v)
    deleted = sorted(r for r in prev if r not in rem)

    ci = [r for r in changed if r.startswith("refs/collab/ci-artifacts/")]
    nonci = [r for r in changed if not r.startswith("refs/collab/ci-artifacts/")]
    if nonci:
        fetch(nonci)

    # ---- 增量部分
    out_lines = []
    new_own = []
    if changed or deleted:
        head = "[walgit 巡检] 变更 refs %d" % len(changed)
        if ci:
            head += "（其中 ci-artifacts %d）" % len(ci)
        if deleted:
            head += "，删除 %d" % len(deleted)
        out_lines.append(head)
        n = 0
        for r in nonci:
            o = decode(rem[r])
            if not o or not o.get("kind"):
                out_lines.append("· %s" % r.replace("refs/collab/", ""))
                n += 1
                continue
            if n >= MAX_LINES:
                out_lines.append("· …其余 %d 条见 patrol-refs.json" % (len(nonci) - n))
                break
            kind = str(o.get("kind"))
            tid = str(o.get("id") or "-")
            actor = str(o.get("actor") or "-")
            body = o.get("body") or {}
            if not isinstance(body, dict):
                body = {}
            extra = ""
            if kind == "status":
                extra = " -> %s" % body.get("status", "?")
            elif kind == "review":
                extra = " -> %s" % body.get("decision", "?")
            tag = ""
            if actor != ME and (kind == "issue" or tid in own_set):
                tag = "  [需处理]"
            b = brief(body, skip=("status", "decision"))
            out_lines.append("· %s %s <- %s%s%s%s" % (kind, tid, actor, extra, tag,
                                                      ("  | " + b) if b else ""))
            n += 1
            if actor == ME and tid and tid not in own_set and tid != "-":
                own_set.add(tid)
                new_own.append(tid)
        for r in deleted:
            out_lines.append("· DELETED %s" % r.replace("refs/collab/", ""))

    # ---- 存量部分
    # 注意：**不做去重**。未收口的线程每轮都会再报一次 —— 宁可重复，也不允许因为「上轮报过」
    # 而静默丢掉一个待动作项（2026-09-22 实测：去重态会让一次漏读永久丢事件）。
    action_lines = []
    try:
        items = backlog_items(own_set)
    except Exception as e:
        items = []
        action_lines.append("[walgit 存量扫描失败] %s" % str(e)[:200])
    for tid, action, actor, ts, note in items:
        if action == "close":
            action_lines.append("[需收口] %s  last=needs-review, approve by %s @%s%s"
                                % (tid, actor, ts, ("  | " + note[:100]) if note else ""))
        elif action == "reopen":
            action_lines.append("[需退回] %s  last=needs-review, needs-changes by %s @%s%s"
                                % (tid, actor, ts, ("  | " + note[:100]) if note else ""))
        else:
            action_lines.append("[让位] %s  已被 %s 之外的人合并，勿重复收口" % (tid, ME))

    # ---- 待派单部分（协同办）
    dispatch_lines = []
    try:
        for tid, action, actor, ts in dispatch_items(own_set):
            if action == "review":
                dispatch_lines.append("[待派单·评审] %s  last=needs-review，由 %s 提出，尚无 review"
                                      % (tid, actor))
            elif action == "claim":
                dispatch_lines.append("[待派单·认领] %s  自家 open issue，尚无 in-progress" % tid)
    except Exception as e:
        dispatch_lines.append("[walgit 派单扫描失败] %s" % str(e)[:200])

    if out_lines:
        print("\n".join(out_lines))
    if action_lines:
        print("[walgit 巡检·需动作] %d 条" % len(action_lines))
        for l in action_lines:
            print("· " + l)
    if dispatch_lines:
        print("[walgit 巡检·待派单] %d 条（回合须 task add 派子代理去干，见 patrol-rules.md §5）"
              % len(dispatch_lines))
        for l in dispatch_lines:
            print("· " + l)

    if not no_state:
        save_json(STATE, rem)
        if new_own:
            try:
                with open(OWN, "w", encoding="utf-8") as f:
                    json.dump(sorted(own_set), f, ensure_ascii=False)
            except Exception:
                pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
