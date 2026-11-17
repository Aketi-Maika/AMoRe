#!/bin/sh
# AMoRe v0.6 - 共通IPの受け持ち状態を AMoRe へ伝える（keepalived の notify）
# 引数: 種別（INSTANCE / GROUP） 名前 状態（MASTER / BACKUP / FAULT / STOP） 優先度
# 共有ボリュームの /shared/keepalived/state/<インスタンス名> に状態を書き、AMoRe が読む。
TYPE="${1:-}"
NAME="${2:-}"
STATE="${3:-}"
[ "$TYPE" = "INSTANCE" ] || exit 0
case "$NAME" in
  AMORE_CAMERA|AMORE_OBS) ;;
  *) exit 0 ;;
esac
mkdir -p /shared/keepalived/state
echo "$STATE" > "/shared/keepalived/state/$NAME.tmp" && mv "/shared/keepalived/state/$NAME.tmp" "/shared/keepalived/state/$NAME"
echo "$(date '+%Y-%m-%d %H:%M:%S') $NAME -> $STATE"
