#!/bin/sh
# AMoRe v0.5 - keepalived の起動（Kubernetes の DaemonSet）
# ConfigMap amore-keepalived の <ノード名>.conf を使う。AMoRe が適用するまで待ち、更新されたら読み直させる。
set -eu

: "${NODE_NAME:?NODE_NAME（spec.nodeName）を環境変数で渡してください}"
SRC="/etc/amore-keepalived/${NODE_NAME}.conf"
CONF=/etc/keepalived/keepalived.conf
mkdir -p /shared/keepalived/state /etc/keepalived
# 確認スクリプト（check_amore.sh）へ渡す値。keepalived はスクリプトへ環境変数を渡さないためファイルにする。
printf 'NODE_IP=%s\nAMORE_HOST_PORT=%s\n' "${NODE_IP:-127.0.0.1}" "${AMORE_HOST_PORT:-18443}" > /etc/keepalived/amore.env
# 前回の状態は使わない（起動直後は BACKUP として始まる）。
rm -f /shared/keepalived/state/*

waited=0
while [ ! -s "$SRC" ]; do
  if [ $((waited % 60)) -eq 0 ]; then
    echo "AMoRe が ConfigMap amore-keepalived に ${NODE_NAME}.conf を作成するのを待っています（kubernetes.yaml の nodes にこのノードがあるか確認してください）"
  fi
  sleep 5
  waited=$((waited + 5))
done

cp "$SRC" "$CONF"
keepalived --dont-fork --log-console --log-detail --use-file "$CONF" &
PID=$!
trap 'kill -TERM "$PID" 2>/dev/null; wait "$PID"; exit 0' TERM INT

# ConfigMap の更新（kubelet が数十秒で反映する）を検知して読み直させる。
while kill -0 "$PID" 2>/dev/null; do
  sleep 10
  if [ -s "$SRC" ] && ! cmp -s "$SRC" "$CONF"; then
    cp "$SRC" "$CONF"
    kill -HUP "$PID"
    echo "$(date '+%Y-%m-%d %H:%M:%S') 設定を読み直しました"
  fi
done
wait "$PID"
