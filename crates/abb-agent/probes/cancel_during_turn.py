#!/usr/bin/env python3
"""复现/验证 abb-agent 的取消链路（评审反证 (b)）。

做法：把 anthropic 的 base_url 指到一个「接受连接但永不响应」的本地黑洞，
造出一个**长时间不返回**的回合，然后在回合进行中：
  1) 送一条 `initialize`（id=4）—— 验证读循环没有被回合并行占死；
  2) 送 `session/cancel` —— 验证取消真的能把回合收尾成 stopReason=cancelled。

第一版（串行 dispatch）在这里的表现是：id=4 与 cancel 都不被处理，
回合一直挂到客户端超时。
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time

BIN = sys.argv[1]
BLACKHOLE_PORT = 18099

# 黑洞：接受连接，读掉请求，永不回响应。
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", BLACKHOLE_PORT))
srv.listen(8)
held = []
stop = threading.Event()


def accept_loop():
    while not stop.is_set():
        try:
            srv.settimeout(0.5)
            conn, _ = srv.accept()
            held.append(conn)  # 保持连接、不回包
        except socket.timeout:
            continue
        except OSError:
            break


threading.Thread(target=accept_loop, daemon=True).start()

env = dict(os.environ)
env.update({
    "BUZZ_AGENT_PROVIDER": "anthropic",
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{BLACKHOLE_PORT}",
    "ANTHROPIC_API_KEY": "dummy-key-for-blackhole",
    # 必填：abb 未配置模型时不注入 ANTHROPIC_MODEL，而 abb-agent 按被替代 fork 的语义
    # **硬失败**（不静默挑一个）——所以探针必须显式给。
    "ANTHROPIC_MODEL": "claude-sonnet-4-5",
    "RUST_LOG": "info",
})

proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, env=env, text=True, bufsize=1)

events = []  # (elapsed, line)


def reader():
    for line in proc.stdout:
        events.append((time.time() - t0, line.strip()))


threading.Thread(target=reader, daemon=True).start()
t0 = time.time()


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def wait_for(pred, timeout):
    """pred 收**去空格后**的行——JSON 里的空格是我们自己排版出来的，不该参与匹配。"""
    end = time.time() + timeout
    while time.time() < end:
        for el, line in list(events):
            if pred(line.replace(" ", "")):
                return el, line
        time.sleep(0.05)
    return None, None


send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 2}})
wait_for(lambda l: '"id":1,' in l, 10)
send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}})
_, created = wait_for(lambda l: '"sessionId"' in l, 10)
session_id = json.loads(created)["result"]["sessionId"]
print(f"[t={time.time()-t0:.1f}s] 会话建立：{session_id}")

# 回合开始（会打到黑洞，长时间不返回）。
t_prompt = time.time()
send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
      "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "在吗"}]}})
time.sleep(2.0)
in_flight = not any('"id":3,' in l.replace(" ", "") for _, l in events)
print(f"[t={time.time()-t0:.1f}s] 回合在途（id=3 未应答）：{in_flight}")

# ① 回合进行中送 initialize：读循环若被占死，这里必超时。
t_probe = time.time()
send({"jsonrpc": "2.0", "id": 4, "method": "initialize", "params": {"protocolVersion": 2}})
el, line = wait_for(lambda l: '"id":4,' in l, 10)
print(f"[t={time.time()-t0:.1f}s] 回合中 initialize 应答："
      f"{'有' if line else '**超时：读循环被占死**'}（回合开始于 t={t_prompt-t0:.1f}s，"
      f"应答到达 t={el:.1f}s ⇒ 延迟 {el-(t_prompt-t0):.1f}s）" if line else
      f"[t={time.time()-t0:.1f}s] 回合中 initialize 应答：**超时：读循环被占死**")

# ② 取消这个回合。
t_cancel = time.time()
send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}})
el, line = wait_for(lambda l: '"id":3,' in l, 25)
if line:
    print(f"[t={time.time()-t0:.1f}s] 取消后回合收尾（取消发出于 t={t_cancel-t0:.1f}s，"
          f"延迟 {el-(t_cancel-t0):.1f}s）：{line}")
else:
    print(f"[t={time.time()-t0:.1f}s] 取消后回合仍未收尾（超时）")

stop.set()
proc.stdin.close()
try:
    err = proc.stderr.read()
except Exception:
    err = ""
try:
    proc.wait(timeout=5)
except subprocess.TimeoutExpired:
    proc.kill()
srv.close()
for c in held:
    c.close()

print("--- agent stderr ---")
print("\n".join(l for l in err.splitlines() if l.strip())[:1500])
print("--- 全部出站 ---")
for el, line in events:
    print(f"  [{el:5.1f}s] {line[:160]}")
