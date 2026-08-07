#!/bin/bash
# 生成并导入自签代码签名证书 agent-bridge-dev（一次性）。
# 用途：见 scripts/sign.sh 头注释 —— TCC 隐私授权按 code identity 匹配，
# 自签证书让重编重签后身份稳定（ad-hoc 签名 cdhash 每次变，授权必失配）。
#
# 幂等：已存在同 CN 证书时直接退出。生成的 key/crt/p12 留在 /tmp 不入仓库。
set -euo pipefail

CERT_ID="agent-bridge-dev"
P12_PASS="dev123"

if security find-identity -p codesigning 2>/dev/null | grep -q "$CERT_ID"; then
  echo "[certs] 已存在 $CERT_ID，跳过"
  exit 0
fi

cd /tmp
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout ab_dev.key -out ab_dev.crt -days 3650 \
  -subj "/CN=$CERT_ID/O=SQB" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=codeSigning" 2>/dev/null

# -legacy：新 OpenSSL 的 PKCS12 加密 security 导入不了（MAC verification failed）
openssl pkcs12 -export -legacy -out ab_dev.p12 -inkey ab_dev.key -in ab_dev.crt \
  -passout pass:$P12_PASS 2>/dev/null

security import ab_dev.p12 -k ~/Library/Keychains/login.keychain-db -P $P12_PASS -T /usr/bin/codesign 2>&1 | grep -v "^$"
echo "[certs] 完成，身份: $(security find-identity -p codesigning 2>/dev/null | grep "$CERT_ID" | awk '{print $2}')"
