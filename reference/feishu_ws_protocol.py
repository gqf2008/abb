#!/usr/bin/env python3
"""
飞书 ↔ Claude Code / Codex 轻量桥（内置长连接版，去 lark-cli）

与 lark-cli 版的差异只在「飞书收发」这一层：
- 收事件：自己走飞书长连接 WebSocket（POST /callback/ws/endpoint 拿 wss →
  连上收 protobuf Frame → DATA 帧回 {"code":200} ack），不再 spawn lark-cli event consume。
- 发消息/表情：直接 REST（tenant_access_token + /open-apis/im/v1/...），不再 lark-cli im。
- 配置外置 config.json（app_id/app_secret/owner/bot 标识/默认后端），GUI 可改。

protobuf 帧编解码手写（Frame 仅 9 字段，纯 stdlib），第三方依赖只有 websocket-client
（装在自带 venv）。协议细节见 memory: feishu-ws-protocol。

agent/会话/路由逻辑与 lark-cli 版完全一致：
- 只响应用户本人（owner_open_id）
- 群聊只响应 @机器人 的消息；私聊全响应
- 粘性后端：/codex、/claude 锁定该 chat 后端，之后不带前缀都走它，直到再切
- 多轮上下文：每个 chat_id 固定一个 session UUID（claude --session-id）
- 免批准执行（用户明确选择）：claude --dangerously-skip-permissions /
  codex --dangerously-bypass-approvals-and-sandbox
"""

import json
import os
import re
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import urllib.parse
import uuid
from pathlib import Path

# ── 路径 ──────────────────────────────────────────────────────────
BRIDGE_DIR = Path.home() / "feishu-bridge"
CONFIG_FILE = BRIDGE_DIR / "config.json"
SESSIONS_FILE = BRIDGE_DIR / "sessions.json"
LOG_DIR = BRIDGE_DIR / "logs"

CLAUDE_BIN = "claude"
CODEX_BIN = "codex"
CC_SWITCH_DB = Path.home() / ".cc-switch" / "cc-switch.db"

DEFAULT_BACKEND_FALLBACK = "claude"
AGENT_TIMEOUT_SECS = 600          # 单次 agent 执行超时
FEISHU_MSG_LIMIT = 3500           # 单条飞书消息安全长度（留余量）
RECONNECT_BASE_DELAY = 2          # 断线重连退避基数（秒）
RECONNECT_MAX_DELAY = 60
LIVENESS_FACTOR = 2               # 超过 LIVENESS_FACTOR × PingInterval 无任何帧 → 判定连接死亡
EMOJI_TYPING = "Typing"           # 实测有效：Typing（首字母大写）、DONE（全大写）
EMOJI_DONE = "DONE"

FEISHU_BASE = "https://open.feishu.cn"
WS_ENDPOINT = FEISHU_BASE + "/callback/ws/endpoint"
API_BASE = FEISHU_BASE + "/open-apis"

# ── 日志 ──────────────────────────────────────────────────────────
def log(msg: str) -> None:
    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{ts}] {msg}", flush=True)

# ── 配置 ──────────────────────────────────────────────────────────
class Config:
    """bot 与路由配置，来自 config.json。缺关键项时给出明确报错而不是裸奔。"""
    def __init__(self, path: Path):
        d = {}
        try:
            if path.exists():
                d = json.loads(path.read_text())
            if not isinstance(d, dict):
                log(f"config.json 不是 JSON 对象，忽略（{path}）")
                d = {}
        except Exception as e:
            log(f"config.json 读取失败: {e}")
        self.app_id = d.get("app_id", "")
        self.app_secret = d.get("app_secret", "")
        self.owner_open_id = d.get("owner_open_id", "")
        self.bot_name = d.get("bot_name", "")
        self.bot_open_id = d.get("bot_open_id", "")
        self.default_backend = d.get("default_backend") or DEFAULT_BACKEND_FALLBACK
        if self.default_backend not in ("claude", "codex"):
            # 非法值会在 run_agent 里静默落到 claude 分支，用户以为配了别的后端
            log(f"config.json default_backend={self.default_backend!r} 无效，回退 {DEFAULT_BACKEND_FALLBACK}")
            self.default_backend = DEFAULT_BACKEND_FALLBACK

    def missing(self) -> list[str]:
        out = []
        if not self.app_id: out.append("app_id")
        if not self.app_secret: out.append("app_secret")
        if not self.owner_open_id: out.append("owner_open_id")
        return out

CFG = Config(CONFIG_FILE)

# ══ 极简 proto2 编解码（pbbp2.Frame，仅 9 字段）══════════════════
def _varint(n: int) -> bytes:
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)

def _read_varint(buf: bytes, i: int) -> tuple[int, int]:
    shift = res = 0
    while i < len(buf):
        b = buf[i]; i += 1
        res |= (b & 0x7F) << shift
        if not (b & 0x80):
            return res, i
        shift += 7
    raise ValueError("truncated varint")

def _tag(field: int, wt: int) -> bytes:
    return _varint((field << 3) | wt)

def _enc_var(field: int, val: int) -> bytes:
    return _tag(field, 0) + _varint(val)

def _enc_len(field: int, data) -> bytes:
    if isinstance(data, str):
        data = data.encode()
    return _tag(field, 2) + _varint(len(data)) + data

def encode_frame(seqid: int, logid: int, service: int, method: int,
                 headers: list, payload: bytes = b"") -> bytes:
    out = bytearray()
    out += _enc_var(1, seqid)
    out += _enc_var(2, logid)
    out += _enc_var(3, service)
    out += _enc_var(4, method)
    for k, v in headers:
        out += _enc_len(5, _enc_len(1, k) + _enc_len(2, v))
    if payload:
        out += _enc_len(8, payload)
    return bytes(out)

def decode_frame(buf: bytes) -> dict:
    f = {"headers": [], "payload": b""}
    i, n = 0, len(buf)
    while i < n:
        key, i = _read_varint(buf, i)
        field, wt = key >> 3, key & 7
        if wt == 0:
            val, i = _read_varint(buf, i)
            if field == 1: f["SeqID"] = val
            elif field == 2: f["LogID"] = val
            elif field == 3: f["service"] = val
            elif field == 4: f["method"] = val
        elif wt == 2:
            ln, i = _read_varint(buf, i)
            data = buf[i:i+ln]; i += ln
            if field == 5:
                hk = hv = None; j = 0
                while j < len(data):
                    k2, j = _read_varint(data, j)
                    f2, wt2 = k2 >> 3, k2 & 7
                    if wt2 == 2:
                        l2, j = _read_varint(data, j)
                        v2 = data[j:j+l2]; j += l2
                        if f2 == 1: hk = v2.decode("utf-8", "replace")
                        elif f2 == 2: hv = v2.decode("utf-8", "replace")
                    elif wt2 == 0:
                        _, j = _read_varint(data, j)
                    elif wt2 == 1:
                        j += 8
                    elif wt2 == 5:
                        j += 4
                    else:
                        break
                if hk is not None:
                    # 缺 value 的 header 规整成空串，避免回 ack 重编码时 len(None) 崩
                    f["headers"].append((hk, hv if hv is not None else ""))
            elif field == 8:
                f["payload"] = data
        elif wt == 1:            # fixed64：跳过而非中断，前向兼容新字段
            i += 8
        elif wt == 5:            # fixed32
            i += 4
        else:
            break
    return f

def frame_header(f: dict, key: str):
    for k, v in f["headers"]:
        if k == key:
            return v
    return None

# ══ 飞书直连客户端（token + REST + 长连接）════════════════════════
class FeishuClient:
    def __init__(self, app_id: str, app_secret: str):
        self.app_id = app_id
        self.app_secret = app_secret
        self._token = None
        self._token_expire = 0.0
        self._lock = threading.Lock()

    def _request(self, method: str, url: str, body: dict | None = None,
                 auth: bool = False, timeout: int = 30, _retried: bool = False) -> dict:
        headers = {"Content-Type": "application/json; charset=utf-8"}
        if auth:
            headers["Authorization"] = "Bearer " + self.tenant_token()
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                result = json.loads(r.read().decode("utf-8", "replace"))
        except urllib.error.HTTPError as e:
            # 飞书 4xx/5xx 的 JSON body 里才有真正的 code/msg，必须捞出来
            try:
                detail = e.read().decode("utf-8", "replace")[:300]
            except Exception:
                detail = ""
            raise RuntimeError(f"HTTP {e.code} {url}: {detail}")
        # token 失效类错误码：清掉缓存，下次调用重新获取
        # （secret 轮换/服务端吊销后，旧 token 会连打到本地过期才自愈，最长 ~2h）
        if auth and isinstance(result, dict) and result.get("code") in (99991661, 99991663):
            with self._lock:
                self._token = None
                self._token_expire = 0.0
            if not _retried:
                # 换新 token 立即重试一次：否则本次调用静默失败——回复整段丢失，
                # 多条分段时用户只收到缺了开头的半截回复
                log(f"token 被拒（code={result.get('code')}），换新 token 重试一次: {method} {url}")
                return self._request(method, url, body, auth, timeout, _retried=True)
        return result

    def _post(self, url: str, body: dict, auth: bool = False) -> dict:
        return self._request("POST", url, body, auth)

    def tenant_token(self) -> str:
        # 快路径先读；过期才把 HTTP 请求放到锁外做，避免慢请求阻塞所有线程
        with self._lock:
            tok, exp = self._token, self._token_expire
        if tok and time.time() < exp - 60:
            return tok
        r = self._post(API_BASE + "/auth/v3/tenant_access_token/internal",
                       {"app_id": self.app_id, "app_secret": self.app_secret})
        if r.get("code") != 0:
            raise RuntimeError(f"tenant_access_token 失败: {r.get('code')} {r.get('msg')}")
        tok = r.get("tenant_access_token")
        if not tok:
            raise RuntimeError(f"tenant_access_token 响应缺 token: {str(r)[:200]}")
        with self._lock:
            self._token = tok
            self._token_expire = time.time() + int(r.get("expire") or 7200)
        return tok

    # ── bot 身份自动发现（config 没配 bot_name/bot_open_id 时兜底）──
    def bot_info(self) -> tuple[str, str]:
        r = self._request("GET", API_BASE + "/bot/v3/info", auth=True)
        if r.get("code") != 0:
            raise RuntimeError(f"bot/v3/info 失败: {r.get('code')} {r.get('msg')}")
        bot = r.get("bot") or {}
        return bot.get("app_name") or "", bot.get("open_id") or ""

    # ── 发消息（bot 身份，token 即 bot）──
    def send_text(self, chat_id: str, text: str) -> None:
        # 预留 16 字符给多条分段序号前缀（（i/N）\n），保证加完前缀仍 ≤ limit
        chunks = _split_text(text, FEISHU_MSG_LIMIT - 16)
        for i, chunk in enumerate(chunks):
            if len(chunks) > 1:
                chunk = f"（{i+1}/{len(chunks)}）\n{chunk}"
            try:
                r = self._post(
                    API_BASE + "/im/v1/messages?receive_id_type=chat_id",
                    {"receive_id": chat_id, "msg_type": "text",
                     "content": json.dumps({"text": chunk}, ensure_ascii=False)},
                    auth=True)
                if r.get("code") != 0:
                    log(f"发送失败 code={r.get('code')} msg={r.get('msg')}")
            except Exception as e:
                log(f"发送飞书异常: {e}")

    # ── 表情 ──
    def add_reaction(self, message_id: str, emoji_type: str):
        try:
            r = self._post(
                API_BASE + f"/im/v1/messages/{message_id}/reactions",
                {"reaction_type": {"emoji_type": emoji_type}}, auth=True)
            if r.get("code") == 0:
                return (r.get("data") or {}).get("reaction_id")
            log(f"加表情失败 {emoji_type}: code={r.get('code')}")
        except Exception as e:
            log(f"加表情异常 {emoji_type}: {e}")
        return None

    def del_reaction(self, message_id: str, reaction_id) -> None:
        if not reaction_id:
            return
        try:
            r = self._request(
                "DELETE",
                API_BASE + f"/im/v1/messages/{message_id}/reactions/{reaction_id}",
                auth=True, timeout=15)
            if r.get("code") != 0:
                log(f"删表情失败: code={r.get('code')} msg={r.get('msg')}")
        except Exception as e:
            log(f"删表情异常: {e}")

    # ── 长连接握手 ──
    def ws_handshake(self) -> tuple[str, dict]:
        r = self._post(WS_ENDPOINT,
                       {"AppID": self.app_id, "AppSecret": self.app_secret})
        code = r.get("code")
        if code != 0:
            raise RuntimeError(f"ws endpoint 失败: code={code} msg={r.get('msg')}")
        data = r.get("data") or {}
        return data.get("URL"), (data.get("ClientConfig") or {})

def _split_text(text: str, limit: int) -> list[str]:
    if len(text) <= limit:
        return [text]
    chunks, cur, cur_len = [], [], 0
    for line in text.splitlines(keepends=True):
        # 单行就超过 limit（无换行的长输出）：先冲刷当前块，再硬切长行
        while len(line) > limit:
            if cur:
                chunks.append("".join(cur))
                cur, cur_len = [], 0
            chunks.append(line[:limit])
            line = line[limit:]
        if cur_len + len(line) > limit and cur:
            chunks.append("".join(cur))
            cur, cur_len = [], 0
        cur.append(line)
        cur_len += len(line)
    if cur:
        chunks.append("".join(cur))
    return chunks

# ── 群聊 @机器人 判定 ─────────────────────────────────────────────
def bot_is_mentioned(mentions) -> bool:
    """mentions 里是否 @了本机器人（name/open_id 任一命中即算，双重冗余）。

    v2 事件 mention 结构：{key, id:{open_id,user_id,union_id}, name, tenant_key}。
    id 里只有 open_id 能和 bot_open_id 对上（user_id/union_id 类型不同，永不命中）。
    """
    if not mentions:
        return False
    for m in mentions:
        if not isinstance(m, dict):
            continue
        if CFG.bot_name and (m.get("name") or "").strip() == CFG.bot_name:
            return True
        mid = m.get("id") or {}
        if isinstance(mid, dict):
            if CFG.bot_open_id and mid.get("open_id") == CFG.bot_open_id:
                return True
        if CFG.bot_open_id and m.get("open_id") == CFG.bot_open_id:
            return True
    return False

# ── 消息 content 文本提取 ─────────────────────────────────────────
def _extract_text(msg_type, parsed: dict) -> str:
    """按消息类型从 content JSON 提取纯文本。

    text 直取 text 字段；post（富文本，粘贴带格式内容时飞书客户端自动转这个类型）
    拼接 title + 各段 text 片段；其余类型（image/file/…）返回 ""，由上层静默跳过。
    """
    if msg_type == "post":
        parts = []
        title = parsed.get("title")
        if title:
            parts.append(str(title))
        for para in parsed.get("content") or []:
            if isinstance(para, list):
                parts.append("".join(str(e.get("text") or "")
                                     for e in para if isinstance(e, dict)))
        return "\n".join(p for p in parts if p).strip()
    t = parsed.get("text")
    return t if isinstance(t, str) else ""

# ── 会话持久化 ─────────────────────────────────────────────────────
class SessionStore:
    """每个 chat_id 对应 {backend, session_id, started}，实现多轮上下文 + 粘性后端。"""
    def __init__(self, path: Path):
        self.path = path
        self._lock = threading.Lock()
        self._data = self._load()

    def _load(self) -> dict:
        try:
            if self.path.exists():
                data = json.loads(self.path.read_text())
                if not isinstance(data, dict):
                    log("session 文件不是 JSON 对象，重置")
                    return {}
                # 丢弃坏条目（手改/旧格式/schema 漂移）：非 dict 条目会让
                # entry.get 抛 AttributeError，曾把该 chat 的 busy 槽永久泄漏
                bad = [k for k, v in data.items() if not isinstance(v, dict)]
                if bad:
                    log(f"session 文件含 {len(bad)} 条坏条目，丢弃: {[str(k)[:12] for k in bad[:3]]}")
                    data = {k: v for k, v in data.items() if isinstance(v, dict)}
                return data
        except Exception as e:
            log(f"session 文件读取失败，重置: {e}")
        return {}

    def _save(self) -> None:
        try:
            # 原子写：tmp + rename，防止崩溃/被杀留下半截 JSON 导致全量 session 重置
            tmp = self.path.with_name(self.path.name + ".tmp")
            tmp.write_text(json.dumps(self._data, ensure_ascii=False, indent=2))
            os.replace(tmp, self.path)
        except Exception as e:
            log(f"session 文件写入失败: {e}")

    def ensure_session(self, chat_id: str) -> str:
        with self._lock:
            entry = self._data.get(chat_id)
            if entry and entry.get("session_id"):
                return entry["session_id"]
            sid = str(uuid.uuid4())
            self._data[chat_id] = {"backend": CFG.default_backend,
                                   "session_id": sid, "started": False}
            self._save()
            return sid

    def mark_started(self, chat_id: str) -> None:
        with self._lock:
            if chat_id in self._data:
                self._data[chat_id]["started"] = True
                self._save()

    def is_started(self, chat_id: str) -> bool:
        with self._lock:
            return bool(self._data.get(chat_id, {}).get("started"))

    def get_backend(self, chat_id: str) -> str:
        with self._lock:
            return self._data.get(chat_id, {}).get("backend") or CFG.default_backend

    def set_backend(self, chat_id: str, backend: str) -> None:
        with self._lock:
            entry = self._data.get(chat_id)
            if not entry:
                entry = {"backend": CFG.default_backend,
                         "session_id": str(uuid.uuid4()), "started": False}
                self._data[chat_id] = entry
            entry["backend"] = backend
            self._save()

# ── CC Switch provider ─────────────────────────────────────────────
def _read_active_cc_provider() -> dict | None:
    if not CC_SWITCH_DB.exists():
        return None
    try:
        con = sqlite3.connect(f"file:{CC_SWITCH_DB}?mode=ro", uri=True, timeout=3)
        row = con.execute(
            "SELECT settings_config FROM providers WHERE app_type='claude' AND is_current=1"
        ).fetchone()
        con.close()
    except Exception as e:
        log(f"读 CC Switch db 失败: {e}")
        return None
    if not row:
        return None
    try:
        cfg = json.loads(row[0])
    except (json.JSONDecodeError, TypeError):
        return None
    env = cfg.get("env") or {}
    return {k: v for k, v in env.items() if k.startswith("ANTHROPIC_")}

# ── 调用后端 agent ─────────────────────────────────────────────────
def run_agent(backend: str, prompt: str, session_id: str, resume: bool) -> tuple[bool, str]:
    """返回 (ok, reply)。prompt 走 stdin，避免多行/-开头被 argparse 误判。"""
    if not prompt:
        return False, "（空消息，没收到内容）"

    if backend == "codex":
        cmd = [CODEX_BIN, "exec", "--dangerously-bypass-approvals-and-sandbox"]
        run_env = None
    else:  # claude 宿主直跑：继承 ~/.claude 全部配置 + 注入当前 provider
        cc_env = _read_active_cc_provider()
        if not cc_env or "ANTHROPIC_BASE_URL" not in cc_env or "ANTHROPIC_AUTH_TOKEN" not in cc_env:
            return False, ("⚠️ 读不到宿主 CC Switch 当前 claude provider 配置"
                           f"（{CC_SWITCH_DB} 不可读或无激活项）。请在 CC Switch 选好 claude provider。")
        cmd = [CLAUDE_BIN, "-p", "--dangerously-skip-permissions"]
        cmd += ["--resume", session_id] if resume else ["--session-id", session_id]
        run_env = {**os.environ, **cc_env}

    log(f"调用 {backend}(host) session={session_id[:8]} prompt={prompt[:60]!r}")
    try:
        proc = subprocess.run(cmd, input=prompt, capture_output=True, text=True,
                              timeout=AGENT_TIMEOUT_SECS, cwd=str(Path.home()), env=run_env)
    except subprocess.TimeoutExpired:
        return False, f"⚠️ {backend} 执行超时（>{AGENT_TIMEOUT_SECS}s）"
    except FileNotFoundError:
        return False, f"⚠️ 找不到命令：{cmd[0]}（PATH 问题）"
    except Exception as e:
        return False, f"⚠️ {backend} 调用异常: {e}"

    out = (proc.stdout or "").strip()
    err = (proc.stderr or "").strip()
    if proc.returncode != 0:
        detail = err or out or f"exit {proc.returncode}"
        return False, f"⚠️ {backend} 出错（exit {proc.returncode}）:\n{detail[:800]}"
    if not out:
        return False, f"⚠️ {backend} 没有输出。{('stderr: ' + err[:400]) if err else ''}"
    return True, out

# ── 桥 ─────────────────────────────────────────────────────────────
class Bridge:
    def __init__(self, fs: FeishuClient):
        self.fs = fs
        self.sessions = SessionStore(SESSIONS_FILE)
        self._seen: set[str] = set()
        self._busy: set[str] = set()
        self._busy_lock = threading.Lock()

    def should_respond(self, ev: dict) -> bool:
        # 飞书事件里应用/bot 发的消息 sender_type 是 "app"（不是 "bot"），用户消息才是 "user"；
        # 只判 "bot" 会把应用消息当用户输入透传（自回声死循环）。再按 open_id 双保险拦自己。
        if ev.get("sender_type") in ("app", "bot"):
            return False
        if CFG.bot_open_id and ev.get("sender_id") == CFG.bot_open_id:
            return False
        if ev.get("sender_id") != CFG.owner_open_id:
            return False
        if ev.get("chat_type") == "group":
            if not bot_is_mentioned(ev.get("mentions")):
                return False
        if not (ev.get("content") or "").strip():
            return False
        return True

    def handle(self, ev: dict) -> None:
        mid = ev.get("message_id") or ""
        chat_id = ev.get("chat_id", "")
        content = (ev.get("content") or "").strip()
        if not chat_id or not mid:
            return

        # message_id 去重（飞书会重发未 ack 的消息；检查+标记须在锁内原子完成，
        # 否则重发与首发并发时会重复跑 agent）
        with self._busy_lock:
            if mid in self._seen:
                return
            self._seen.add(mid)
            if len(self._seen) > 5000:
                self._seen = set(list(self._seen)[-2500:])

        text = re.sub(r"@_user_\d+", "", content).strip()
        if not text:
            log(f"chat {chat_id[:10]} 跳过空/非文本消息 mid={mid[:12]}")
            return

        m = re.match(r"^/(codex|claude)(?:[\s　]+(.*))?$", text, re.I | re.S)
        if m and not (m.group(2) or "").strip():
            # 裸 /codex、/claude：纯切换确认，不跑 agent，随时可用（不占 busy 槽）
            backend = m.group(1).lower()
            self.sessions.ensure_session(chat_id)
            self.sessions.set_backend(chat_id, backend)
            other = "claude" if backend == "codex" else "codex"
            self.fs.send_text(chat_id,
                f"✅ 已切到 **{backend}**。后续消息都走 {backend}（发 /{other} 切回）。")
            log(f"chat {chat_id[:10]} 切后端 -> {backend}")
            return

        # 要跑 agent 的消息：先占 busy 槽，再做路由副作用——
        # 否则「消息因忙被丢弃，但后端已被切走」
        with self._busy_lock:
            if chat_id in self._busy:
                log(f"chat {chat_id[:10]} 忙，丢弃并发消息: {text[:40]!r}")
                return
            self._busy.add(chat_id)

        typing_rid = self.fs.add_reaction(mid, EMOJI_TYPING)   # 内部已吞异常，必返回
        try:
            # 会话读写必须罩在 try 里：sessions.json 条目损坏（手改/旧格式）会让
            # .get 抛 AttributeError；若漏出 try 外，busy 槽永不释放，
            # 该 chat 被静默忽略直到重启（已复现：entry 为 str 时 100% 泄漏）
            session_id = self.sessions.ensure_session(chat_id)
            if m:
                backend = m.group(1).lower()
                prompt = (m.group(2) or "").strip()
                self.sessions.set_backend(chat_id, backend)
            else:
                backend = self.sessions.get_backend(chat_id)
                prompt = text
            resume = self.sessions.is_started(chat_id)
            log(f"收到 ({backend}) chat={chat_id[:10]}: {prompt[:60]!r}")
            ok, reply = run_agent(backend, prompt, session_id, resume)
            if ok and backend == "claude":
                self.sessions.mark_started(chat_id)
            self.fs.send_text(chat_id, reply)
            log(f"已回复 chat={chat_id[:10]} ok={ok} 长度={len(reply)}")
        except Exception as e:
            # 别静默变聋：给用户一个可见错误（busy 在 finally 必释放）
            log(f"处理消息异常 chat={chat_id[:10]} mid={mid[:12]}: {e}")
            self.fs.send_text(chat_id, f"⚠️ 桥内部错误: {e}")
        finally:
            self.fs.del_reaction(mid, typing_rid)
            self.fs.add_reaction(mid, EMOJI_DONE)
            with self._busy_lock:
                self._busy.discard(chat_id)

    # ── 把 v2 事件 payload 规整成旧 ev dict，复用 should_respond ──
    def on_event_payload(self, payload: bytes) -> None:
        try:
            body = json.loads(payload.decode("utf-8", "replace"))
        except json.JSONDecodeError:
            return
        if not isinstance(body, dict):
            return
        header = body.get("header")
        if not isinstance(header, dict):
            # 非 2.0 信封（如 1.0 版事件订阅）：留诊断日志，别静默变聋
            log(f"忽略非 2.0 事件信封: {str(body)[:200]}")
            return
        if header.get("event_type") != "im.message.receive_v1":
            return                      # 只处理消息接收事件，其它（reaction/task…）忽略
        event = body.get("event") or {}
        if not isinstance(event, dict):
            return
        sender = event.get("sender") or {}
        message = event.get("message") or {}
        if not isinstance(message, dict):
            return
        sender_id = (sender.get("sender_id") or {}).get("open_id")
        # content 是 JSON 字符串，按消息类型取文本
        raw_content = message.get("content") or ""
        text = ""
        if isinstance(raw_content, str):
            try:
                parsed = json.loads(raw_content)
            except json.JSONDecodeError:
                parsed = None
            if isinstance(parsed, dict):
                text = _extract_text(message.get("message_type"), parsed)
            elif parsed is None and raw_content:
                text = raw_content      # 非 JSON 原样兜底
        ev = {
            "message_id": message.get("message_id"),
            "chat_id": message.get("chat_id"),
            "chat_type": message.get("chat_type"),
            "sender_type": sender.get("sender_type"),
            "sender_id": sender_id,
            "content": text,
            "mentions": message.get("mentions"),
        }
        if ev["chat_type"] == "group":
            log(f"[群] chat={str(ev['chat_id'])[:10]} bot@={bot_is_mentioned(ev.get('mentions'))} "
                f"mentions={[(m.get('name'), (m.get('id') or {}).get('open_id')) for m in (ev.get('mentions') or []) if isinstance(m, dict)]}")
        if not self.should_respond(ev):
            return
        threading.Thread(target=self.handle, args=(ev,), daemon=True).start()

# ── 长连接主循环（握手→连接→收帧→ack，断线自动重连）────────────────
def ws_loop(bridge: Bridge, fs: FeishuClient) -> None:
    import websocket           # venv 里的 websocket-client
    backoff = RECONNECT_BASE_DELAY
    while True:
        ws = None
        try:
            url, conf = fs.ws_handshake()
            if not url:
                raise RuntimeError("ws endpoint 响应缺少 URL")
            q = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)
            try:
                service_id = int(q["service_id"][0])
            except (KeyError, IndexError, ValueError):
                raise RuntimeError(f"ws URL 缺少 service_id 参数: {url[:120]}")
            try:
                ping_interval = int(conf.get("PingInterval") or 90)
            except (TypeError, ValueError):
                ping_interval = 90
            if ping_interval <= 0:
                ping_interval = 90
            log(f"WS 握手成功，连接中（ping={ping_interval}s）…")
            ws = websocket.create_connection(url, timeout=30)
            ws.settimeout(ping_interval)   # 超时即触发一次 ping / 活性检查
            log("WS 已连接，监听 im.message.receive_v1")
            last_ping = last_rx = time.time()
            while True:
                try:
                    msg = ws.recv()
                except websocket.WebSocketTimeoutException:
                    msg = None
                if msg is None:
                    now = time.time()
                    # 活性看门狗：超过 LIVENESS_FACTOR 个心跳间隔没收到任何帧
                    # （含 pong），说明连接已半死（对端崩溃/NAT 断），主动重连。
                    # 只发 ping 不收 pong 的循环会永远卡住（send 进死 socket 不报错）。
                    if now - last_rx >= ping_interval * LIVENESS_FACTOR:
                        raise ConnectionError(
                            f"心跳超时：{ping_interval * LIVENESS_FACTOR}s 未收到任何帧")
                    if now - last_ping >= ping_interval:
                        ping = encode_frame(0, 0, service_id, 0, [("type", "ping")])
                        ws.send(ping, opcode=0x2)
                        last_ping = now
                    continue
                last_rx = time.time()       # 任何入站帧都证明连接活着
                if not isinstance(msg, (bytes, bytearray)):
                    continue
                backoff = RECONNECT_BASE_DELAY   # 收到帧 = 连接已验证可用，重置退避
                try:
                    frame = decode_frame(bytes(msg))
                except (ValueError, IndexError) as e:
                    # 毒帧：不 ack（让飞书按未达重发几次后放弃），但连接保住
                    log(f"帧解析失败，丢弃该帧: {e}")
                    continue
                method = frame.get("method")
                if method == 0:            # CONTROL：ping 忽略；pong 可能带新 ClientConfig
                    if frame_header(frame, "type") == "pong" and frame.get("payload"):
                        try:
                            cc = json.loads(frame["payload"].decode("utf-8", "replace"))
                            pi = int(cc.get("PingInterval") or 0)
                            if pi > 0 and pi != ping_interval:
                                ping_interval = pi
                                ws.settimeout(ping_interval)
                                log(f"服务端更新 PingInterval -> {pi}s")
                        except Exception:
                            pass
                    continue
                payload = frame.get("payload") or b""
                # 原 Frame 换 payload 重编码发回作 ack（协议要求，不回会被重发/断开）
                ack = encode_frame(frame.get("SeqID", 0), frame.get("LogID", 0),
                                   frame.get("service", service_id),
                                   frame.get("method", 1), frame["headers"],
                                   json.dumps({"code": 200}).encode())
                try:
                    ws.send(ack, opcode=0x2)
                except Exception as e:
                    log(f"回 ack 失败: {e}")
                    raise
                try:
                    bridge.on_event_payload(payload)
                except Exception as e:
                    log(f"处理事件异常: {e}")
        except Exception as e:
            log(f"WS 异常断开: {type(e).__name__} {e}；{backoff}s 后重连")
        finally:
            try:
                if ws is not None:
                    ws.close()
            except Exception:
                pass
        time.sleep(backoff)
        backoff = min(backoff * 2, RECONNECT_MAX_DELAY)

def main() -> None:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    log("=== 飞书桥启动 ===")
    log("版本: 内置 WS 长连接（去 lark-cli）")
    missing = CFG.missing()
    if missing:
        log(f"⚠️ config.json 缺少必填项: {', '.join(missing)}（{CONFIG_FILE}）")
        log("请填好 app_id / app_secret / owner_open_id 后重启。")
        sys.exit(1)
    log(f"只响应: {CFG.owner_open_id}  默认后端: {CFG.default_backend}  bot: {CFG.bot_name}")
    fs = FeishuClient(CFG.app_id, CFG.app_secret)
    # bot 身份（群聊 @ 判定要用）：config 没配就从 bot/v3/info 自动发现
    if not CFG.bot_name or not CFG.bot_open_id:
        try:
            name, open_id = fs.bot_info()
            CFG.bot_name = CFG.bot_name or name
            CFG.bot_open_id = CFG.bot_open_id or open_id
            log(f"bot 身份自动发现: name={CFG.bot_name} open_id={CFG.bot_open_id}")
        except Exception as e:
            log(f"⚠️ bot/v3/info 自动发现失败: {e}")
    if not CFG.bot_name and not CFG.bot_open_id:
        log("⚠️ 未配置且未能自动发现 bot_name/bot_open_id，群聊 @机器人 将无法识别（私聊不受影响）")
    try:
        import websocket  # noqa: F401  （venv 里的 websocket-client）
    except ImportError:
        log("缺少 websocket-client：请用 ~/feishu-bridge/venv/bin/python 启动，"
            "或 python3 -m pip install websocket-client")
        sys.exit(1)
    bridge = Bridge(fs)
    ws_loop(bridge, fs)

if __name__ == "__main__":
    main()
