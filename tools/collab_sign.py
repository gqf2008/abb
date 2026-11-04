#!/usr/bin/env python3
"""给一条 collab 条目做客户端签名，输出可直接喂 `collab_publish` 的 JSON。

## 为什么有这个脚本

`collab_publish` **从不代签**（它拿不到私钥）。条目必须先在本机签好再发。
没有它就只能每次手写 openssl 命令 + 手拼 canonical JSON，而那正是
2026-11-02 出过「伪造 rid」事故的地方。

## canonical 规则（唯一正确形态）

1. 把 `sig` 的**值**置成空串（**键必须保留**——删掉键会签出 verified:false）
2. 对象键按字节排序
3. 去掉全部空白
4. `ensure_ascii=False`（中文按 UTF-8 原样）

实测过：拿真账本里 399 条已验证的条目，用「sig 置空串」规则逐条验签**全部通过**，
用「去掉 sig 键」规则**全部失败**。

## 用法

    python3 tools/collab_sign.py <私钥 hex 文件> <条目 JSON>

条目 JSON 里不用带 `sig`（会自动加）；其余字段（version/kind/id/actor/ts/parent/body）
由调用方给全。
"""

import json
import subprocess
import sys
import tempfile
from pathlib import Path


def canonical(entry: dict) -> bytes:
    """canonical 字节：sig 值置空串 + 键按字节排序 + 无空白。"""
    e = dict(entry)
    e["sig"] = ""  # 键必须保留
    return json.dumps(
        e, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def pkcs8_from_seed(seed: bytes) -> bytes:
    """32 字节 ed25519 seed → PKCS8 DER。

    前缀是 RFC 8410 的 OneAsymmetricKey 外壳（固定 16 字节），后面接 4 字节
    OCTET STRING 头 + 32 字节 seed。
    """
    if len(seed) != 32:
        raise SystemExit(f"seed 必须是 32 字节，实际 {len(seed)}")
    return bytes.fromhex("302e020100300506032b657004220420") + seed


def sign(entry: dict, seed: bytes) -> dict:
    der = pkcs8_from_seed(seed)
    msg = canonical(entry)
    with tempfile.TemporaryDirectory() as td:
        kp = Path(td) / "k.der"
        kp.write_bytes(der)
        # **消息必须走 `-in` 文件，不能走 stdin**：`pkeyutl -rawin` 要先知道消息长度
        # （ed25519 是 oneshot 签名），管道输入拿不到 ⇒ 报
        # `unable to determine file size for oneshot operation`（实测）。
        mf = Path(td) / "m.bin"
        mf.write_bytes(msg)
        p = subprocess.run(
            [
                "openssl", "pkeyutl", "-sign", "-rawin",
                "-inkey", str(kp), "-in", str(mf),
            ],
            capture_output=True,
        )
        if p.returncode != 0:
            raise SystemExit(f"openssl 签名失败：{p.stderr.decode(errors='replace')}")
        sig = p.stdout
    out = dict(entry)
    out["sig"] = "ed25519:" + __import__("base64").b64encode(sig).decode()
    return out


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    seed = bytes.fromhex(Path(sys.argv[1]).read_text().strip())
    entry = json.loads(Path(sys.argv[2]).read_text())
    print(json.dumps(sign(entry, seed), ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
