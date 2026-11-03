#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""walgit 看板巡检（只读）。

三件事：
  1) 增量：比对本地 walgit 镜像 refs/collab/* 的上次快照，打印新增/变更/删除的协作条目。
  2) 存量：扫描「快照 ∪ inbox」里的全部条目，找出「自家线程已 approve 未收口 / 已 request_changes
     未退回」这类**需要动作**的线程（增量比对看不到它们，因为条目早就在基线里了）。
  3) 派单：找出「有人交了 patch 在等人审」「自家 open issue 无人推进」的线程。

无任何输出 = 无需动作、也无需报告（调用方据此保持静默）。

设计约束：
  - 只读：只做 git for-each-ref / ls-remote / fetch / cat-file，绝不执行 merge_result / status closed 等不可逆动作。
  - 不依赖 walgit CLI（collab board/report/thread 在本机 >120s 超时），只走镜子仓库的 git。
  - 读取面 = refs/collab/meta/snapshot（折叠历史） ∪ refs/collab/inbox/*（尾部），
    见 walgit D1_PROTOCOL §9.2。只读 inbox 的读者在 fold/gc 之后会缺折叠历史。
  - 状态文件：patrol-refs.json（ref -> sha）、own-threads.json（自家线程，与 watch.ps1 共用）。
  - 只允许输出，除上述两个状态文件以外不修改任何文件。
"""
import json
import os
import subprocess
import sys

BASE = os.path.dirname(os.path.abspath(__file__))
MIRROR = os.path.join(BASE, "abb-mirror.git")
STATE = os.path.join(BASE, "patrol-refs.json")
OWN = os.path.join(BASE, "own-threads.json")
SNAPSHOT_REF = "refs/collab/meta/snapshot"
ME = "abb-win"
MAX_LINES = 20
BRIEF_KEYS = ("note", "work", "title", "summary", "branch", "head", "decision", "status")
# walgit 协议（D1_COLLAB_DESIGN）的 review decision 是 request_changes；
# 本仓历史条目里也出现过 needs-changes，两种都认，避免换一个写法就永远不报警。
CHANGE_DECISIONS = ("request_changes", "needs-changes")


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
    """tmp + replace：半截写入不会把状态文件留着坏掉（坏状态 = 基线重置 = 静默漏事件）。"""
    tmp = path + ".tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(obj, f, ensure_ascii=False)
        os.replace(tmp, path)
    except Exception:
        try:
            os.remove(tmp)
        except Exception:
            pass


def entry_key(e):
    """线程序主键：walgit 按 (ts, actor, oid) 升序（D1_PROTOCOL §6.2）。

    同一秒写入的条目很常见（迁移/批量 push），只按 ts 比较会把「同秒的 review」
    误判成「在 status 之前」，从而漏报或重复派单。
    """
    return (e.get("ts") or 0, str(e.get("actor") or ""), str(e.get("_oid") or ""))


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


def local_ref_map():
    rc, out, err = git(["for-each-ref", "--format=%(objectname) %(refname)", "refs/collab"])
    if rc != 0:
        return {}
    m = {}
    for line in out.splitlines():
        parts = line.split(" ", 1)
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
        oid = parts[0]
        size = int(parts[2])
        # 对象体可能跨多行（walgit 条目就是 pretty-printed JSON）：按字节数重组
        body = lines[i]
        i += 1
        # 简单稳妥：用 size 校验，不足则继续拼
        while len(body.encode("utf-8", errors="replace")) < size and i < len(lines):
            body += "\n" + lines[i]
            i += 1
        try:
            e = json.loads(body)
            if isinstance(e, dict):
                e["_oid"] = oid        # 排序/去重用，不写入任何输出
                entries.append(e)
        except Exception:
            pass
        # 跳过 batch 输出里对象之间的空行
        while i < len(lines) and lines[i] == "":
            i += 1
    return entries


def snapshot_entries():
    """读 refs/collab/meta/snapshot（fold 后的历史）。没有该 ref = 尚未折叠。"""
    rc, out, err = git(["cat-file", "-p", SNAPSHOT_REF], timeout=300)
    if rc != 0:
        return []
    try:
        doc = json.loads(out)
    except Exception as e:
        raise RuntimeError("meta/snapshot 不是合法 JSON: %s" % e)
    if not isinstance(doc, dict) or doc.get("version") != 1 or doc.get("kind") != "collab_snapshot":
        raise RuntimeError("meta/snapshot 文档不合规（version/kind）")
    entries = []
    for rec in doc.get("entries") or []:
        if not isinstance(rec, dict):
            continue
        oid = rec.get("oid")
        raw = rec.get("json")
        if not isinstance(oid, str) or not isinstance(raw, str):
            continue
        try:
            e = json.loads(raw)
        except Exception:
            continue
        if isinstance(e, dict):
            e["_oid"] = oid
            entries.append(e)
    return entries


def all_entries():
    """读取面 = 快照（折叠历史） ∪ inbox 尾部，按 oid 去重（D1_PROTOCOL §9.2）。"""
    by_oid = {}
    rest = []
    for e in local_inbox():
        oid = e.get("_oid")
        if oid:
            by_oid[oid] = e
        else:
            rest.append(e)
    for e in snapshot_entries():
        oid = e.get("_oid")
        if oid and oid not in by_oid:
            by_oid[oid] = e
    return list(by_oid.values()) + rest


def last_status_entry(es):
    """最新 status（按协议顺序，ts 相同时用 actor/oid 破平）。"""
    last = None
    for e in es:
        if e.get("kind") != "status":
            continue
        if not (e.get("body") or {}).get("status"):
            continue
        if last is None or entry_key(e) >= entry_key(last):
            last = e
    return last


def review_decision(e):
    return str((e.get("body") or {}).get("decision") or "")


def note_text(e):
    n = (e.get("body") or {}).get("note", "")
    return n if isinstance(n, str) else str(n)


def merged_results(es):
    """真正成功的合并记录（merged:true）；failed merge 不能当「已合并」让位。"""
    out = []
    for e in es:
        if e.get("kind") != "merge_result":
            continue
        if (e.get("body") or {}).get("merged") is True:
            out.append(e)
    return out


def backlog_items(own_set, entries=None):
    entries = all_entries() if entries is None else entries
    by_thread = {}
    for e in entries:
        tid = e.get("id")
        if not tid:
            continue
        by_thread.setdefault(str(tid), []).append(e)

    items = []
    for tid, es in by_thread.items():
        es.sort(key=entry_key)
        if tid not in own_set and not any(x.get("actor") == ME for x in es):
            continue
        st = last_status_entry(es)
        if st is None or str((st.get("body") or {}).get("status")) != "needs-review":
            continue
        sk = entry_key(st)
        reviews = [e for e in es if e.get("kind") == "review" and entry_key(e) > sk]
        if not reviews:
            continue
        # 只认**最新**的结论性 review：先 approve、后又 request_changes 的线程不能收口。
        decisive = [e for e in reviews if review_decision(e) in ("approve",) + CHANGE_DECISIONS]
        if not decisive:
            continue
        newest = max(decisive, key=entry_key)
        decision = review_decision(newest)
        if decision == "approve":
            merged_by_other = [e for e in merged_results(es) if e.get("actor") != ME]
            action = "skip-merged-by-other" if merged_by_other else "close"
        else:
            action = "reopen"
        items.append((tid, action, newest.get("actor"), newest.get("ts"),
                      note_text(newest), decision))
    items.sort(key=lambda x: x[3] or 0)
    return items


def dispatch_items(own_set, entries=None):
    """扫描全局，找出「需要派人去协同完成」的线程：
      - review  : last status = needs-review 且其后没有任何 review -> 派独立评审人
      - claim   : 自家（owner=abb-win / own-threads）的 open issue 且尚无 in-progress -> 派认领实现
    返回 [(tid, action, actor, ts)]。
    """
    entries = all_entries() if entries is None else entries
    by_thread = {}
    for e in entries:
        tid = e.get("id")
        if tid:
            by_thread.setdefault(str(tid), []).append(e)

    items = []
    for tid, es in by_thread.items():
        es.sort(key=entry_key)
        # 已合并的线程不再派评审/认领；只欠 status 收口的由 backlog 报「需收口」。
        if merged_results(es):
            continue
        has_issue = any(e.get("kind") == "issue" for e in es)
        st = last_status_entry(es)
        last_status = str((st.get("body") or {}).get("status")) if st else None
        if last_status == "needs-review":
            sk = entry_key(st)
            after = [e for e in es if e.get("kind") == "review" and entry_key(e) > sk]
            if not after:
                items.append((tid, "review", str(st.get("actor") or "-"), st.get("ts") or 0))
        elif last_status in (None, "open", "in-progress"):
            # 迁移线程的首条 entry 不一定是 issue（ts 同批、排序不定），按「线程内含 issue」判定
            if not has_issue:
                continue
            owner = None
            # 协议：owner 只来自 status 条目（comment 里的 owner 不算）。
            for e in es:
                if e.get("kind") != "status":
                    continue
                b = e.get("body") or {}
                if isinstance(b, dict) and b.get("owner"):
                    owner = str(b.get("owner"))
            mine = (owner == ME) or (tid in own_set)
            if mine and last_status != "in-progress":
                items.append((tid, "claim", owner or ME, (st.get("ts") or 0) if st else 0))
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
        missing = []
        if not no_state:
            # 首次运行：先把镜像补齐（快照 + inbox 尾部）。只保存 ls-remote 结果而不 fetch，
            # 会让一个 init --bare 的镜像永远没有条目，存量扫描此后一直静默为空。
            local = local_ref_map()
            missing = sorted(r for r, v in rem.items()
                             if local.get(r) != v
                             and not r.startswith("refs/collab/ci-artifacts/"))
            fetch(missing)
            save_json(STATE, rem)
        print("(baseline 建立：%d 个 refs，补拉 %d 个，下次起只报增量)" % (len(rem), len(missing)))
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
            if n >= MAX_LINES:
                out_lines.append("· …其余 %d 条见 patrol-refs.json" % (len(nonci) - n))
                break
            o = decode(rem[r])
            if not o or not o.get("kind"):
                out_lines.append("· %s" % r.replace("refs/collab/", ""))
                n += 1
                continue
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
    entries = None
    entries_err = None
    try:
        entries = all_entries()
    except Exception as e:
        entries_err = e
    if entries_err is not None:
        action_lines.append("[walgit 存量扫描失败] %s" % str(entries_err)[:200])
    else:
        try:
            items = backlog_items(own_set, entries)
        except Exception as e:
            items = []
            action_lines.append("[walgit 存量扫描失败] %s" % str(e)[:200])
        for tid, action, actor, ts, note, decision in items:
            if action == "close":
                action_lines.append("[需收口] %s  last=needs-review, approve by %s @%s%s"
                                    % (tid, actor, ts, ("  | " + note[:100]) if note else ""))
            elif action == "reopen":
                action_lines.append("[需退回] %s  last=needs-review, %s by %s @%s%s"
                                    % (tid, decision, actor, ts,
                                       ("  | " + note[:100]) if note else ""))
            else:
                action_lines.append("[让位] %s  已被 %s 之外的人合并，勿重复收口" % (tid, ME))

    # ---- 待派单部分（协同办）
    dispatch_lines = []
    if entries_err is not None:
        dispatch_lines.append("[walgit 派单扫描失败] %s" % str(entries_err)[:200])
    else:
        try:
            for tid, action, actor, ts in dispatch_items(own_set, entries):
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
                save_json(OWN, sorted(own_set))
            except Exception:
                pass
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:
        # 输出面为空会被调用方当成「无需动作」；崩溃必须显式说一句，不能静默。
        print("[walgit 巡检失败] %s: %s" % (type(e).__name__, str(e)[:300]))
        sys.exit(1)
