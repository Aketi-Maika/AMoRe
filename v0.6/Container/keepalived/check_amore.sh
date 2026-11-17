#!/bin/sh
# AMoRe v0.6 - このノードで AMoRe が動いているかの確認（keepalived の vrrp_script。Kubernetes 用）
# AMoRe の Pod は hostPort（k8s/sites/*/amore.yaml の 18443）を持つ。AMoRe の Pod が無い・応答しない
# ノードは失敗し、共通IPを持たない（共通IPは AMoRe とカメラの Pod がそろったノードだけが受け持つ）。
# entrypoint.sh が書いた NODE_IP・AMORE_HOST_PORT を読む。
# shellcheck source=/dev/null # 起動時に entrypoint.sh が作るファイル
[ -r /etc/keepalived/amore.env ] && . /etc/keepalived/amore.env
code=$(curl -sk --max-time 2 -o /dev/null -w '%{http_code}' "https://${NODE_IP:-127.0.0.1}:${AMORE_HOST_PORT:-18443}/healthz") || exit 1
[ "$code" = "200" ]
