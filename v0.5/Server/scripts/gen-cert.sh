#!/bin/sh
# AMoRe v0.5 - 自己署名TLS証明書の作成
#
# 使い方: sh scripts/gen-cert.sh <ServerのIPアドレス> [IPアドレス・DNS名 …]
# 例（1台）    : sh scripts/gen-cert.sh 10.40.0.20
# 例（冗長構成）: sh scripts/gen-cert.sh 10.40.0.20 10.40.0.21 149.40.2.1 10.40.0.25
#                 （全 Server の実IP・カメラ用の共通IP・OBS 用の共通IP。作った1枚を全 Server で共用する）
#
# certs/server.crt（カメラPC・編集PCへ配布して信頼させる）と certs/server.key（Server外へ出さない）を作る。
# ブラウザは HTTPS（安全なコンテキスト）でなければカメラ・マイクを利用できないため必須。
# 冗長構成では Server 同士の通信もこの証明書で相手を確かめるため、全 Server で同じ証明書を使う。
set -eu

if [ $# -lt 1 ]; then
  echo "使い方: sh scripts/gen-cert.sh <ServerのIPアドレス> [IPアドレス・DNS名 …]" >&2
  exit 1
fi
HOST="$1"
DIR="$(cd "$(dirname "$0")/.." && pwd)/certs"
CRT="$DIR/server.crt"
KEY="$DIR/server.key"

if [ -e "$CRT" ] || [ -e "$KEY" ]; then
  echo "既に $CRT または $KEY が存在します。作り直す場合は退避してから再実行してください。" >&2
  exit 1
fi

# IPv4 の形式なら IP、それ以外は DNS 名として SAN に入れる。
SAN=""
for name in "$@"; do
  if printf '%s' "$name" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$'; then
    entry="IP:${name}"
  else
    entry="DNS:${name}"
  fi
  SAN="${SAN:+${SAN},}${entry}"
done

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
