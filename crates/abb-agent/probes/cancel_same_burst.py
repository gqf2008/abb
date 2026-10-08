#!/usr/bin/env python3
"""复现/验证「cancel 与 prompt 同 burst」竞态（本轮评审的阻塞项）。

评审的复现手法：把 `session/prompt` 与 `session/cancel` 放在**同一次 write**（0 间隔）
里送去，配黑洞端点造长回合。修前：`in_flight` 在被 spawn 的任务里抢，而读循环能在
任务被调度前先读到 cancel ⇒ 走「无在途回合」分支，取消被**静默丢弃**，回合一直挂在
黑洞上（0 间隔 4/4 复现、≥0.5ms 全绿）。修后：`in_flight` 在读循环内同步抢 ⇒ 必然看到
在途回合，回合以 stopReason=cancelled 收尾。
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time

BIN = sys.argv[1]
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 5
PORT = 18103

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", PORT))
srv.listen(8)
held = []
stop = threading.Event()


def accept_loop():
    while not stop.is_set():
        try:
            srv.settimeout(0.5)
            conn, _ = srv.accept()
            held.append(conn)
        except socket.timeout:
            continue
        except OSError:
            break


threading.Thread(target=accept_loop, daemon=True).start()

env = dict(os.environ)
env.update({
    "BUZZ_AGENT_PROVIDER": "anthropic",
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{PORT}",
    "ANTHROPIC_API_KEY": "dummy",
    "ANTHROPIC_MODEL": "claude-sonnet-4-5",
})

results = []
for attempt in range(ROUNDS):
    proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, env=env, text=True, bufsize=1)
    out = []
    threading.Thread(target=lambda: [out.append(l.strip()) for l in proc.stdout],
                     daemon=True).start()

    def send(obj):
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 2}})
    send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}})
    deadline = time.time() + 10
    sid = None
    while time.time() < deadline and sid is None:
        for line in list(out):
            if '"sessionId"' in line:
                sid = json.loads(line)["result"]["sessionId"]
        time.sleep(0.02)
    if not sid:
        results.append("会话未建立")
        proc.kill()
        continue

    # 关键：prompt 与 cancel 放**同一次 write**。
    burst = (
        json.dumps({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                    "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "在吗"}]}})
        + "\n"
        + json.dumps({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}})
        + "\n"
    )
    proc.stdin.write(burst)
    proc.stdin.flush()

    deadline = time.time() + 15
    verdict = None
    while time.time() < deadline:
        for line in list(out):
            if '"id":3' in line.replace(" ", ""):
                verdict = line
        if verdict:
            break
        time.sleep(0.05)
    if verdict is None:
        results.append("未收尾（取消被丢弃）")
    elif '"cancelled"' in verdict:
        results.append("cancelled")
    else:
        results.append(f"其他结论：{verdict[:80]}")
    proc.kill()
    try:
        proc.wait(timeout=3)
    except Exception:
        pass

stop.set()
srv.close()
for c in held:
    c.close()

print(f"同 burst（0 间隔）× {ROUNDS}：")
for i, r in enumerate(results, 1):
    print(f"  run {i}: {r}")
# 判定必须**只认 cancelled**：早先的写法把「非 cancelled 的其它结论」算作通过，
# 于是「取消无效但回合自然收尾」会被误报成「竞态已修」（评审指出）。
ok = sum(1 for r in results if r == "cancelled")
bad = [f"run {i}: {r}" for i, r in enumerate(results, 1) if r != "cancelled"]
print(f"\n⇒ cancelled {ok}/{ROUNDS}；{'✅ 竞态已修' if not bad else '❌ 有未生效的取消'}")
for line in bad:
    print(f"   {line}")
