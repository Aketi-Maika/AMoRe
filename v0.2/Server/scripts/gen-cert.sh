#!/bin/sh
# AMoRe v0.2 - 自己署名TLS証明書の作成
#
# 使い方: sh scripts/gen-cert.sh <ServerのIPアドレス> [DNS名]
# 例    : sh scripts/gen-cert.sh 10.40.0.20
#
# certs/server.crt（カメラPC・編集PCへ配布して信頼させる）と certs/server.key（Server外へ出さない）を作る。
# ブラウザは HTTPS（安全なコンテキスト）でなければカメラ・マイクを利用できないため必須。
set -eu

HOST="${1:?使い方: sh scripts/gen-cert.sh <ServerのIPアドレス> [DNS名]}"
DNS_NAME="${2:-}"
DIR="$(cd "$(dirname "$0")/.." && pwd)/certs"
CRT="$DIR/server.crt"
KEY="$DIR/server.key"

if [ -e "$CRT" ] || [ -e "$KEY" ]; then
  echo "既に $CRT または $KEY が存在します。作り直す場合は退避してから再実行してください。" >&2
  exit 1
fi

SAN="IP:${HOST}"
if [ -n "$DNS_NAME" ]; then
  SAN="${SAN},DNS:${DNS_NAME}"
fi

mkdir -p "$DIR"
umask 077
openssl req -x509 -newkey rsa:2048 -sha256 -days 825 -nodes \
  -keyout "$KEY" -out "$CRT" \
  -subj "/CN=AMoRe ${HOST}" \
  -addext "subjectAltName=${SAN}" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
  -addext "extendedKeyUsage=serverAuth"
chmod 600 "$KEY"
chmod 644 "$CRT"

echo "作成しました:"
echo "  証明書: $CRT"
echo "  秘密鍵: $KEY"
openssl x509 -in "$CRT" -noout -subject -enddate -ext subjectAltName
echo "SHA256フィンガープリント（配布先での照合用）:"
openssl x509 -in "$CRT" -noout -fingerprint -sha256
