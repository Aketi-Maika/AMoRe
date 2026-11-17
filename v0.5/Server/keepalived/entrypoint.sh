#!/bin/sh
# AMoRe v0.5 - keepalived の起動
# AMoRe が設定ファイルを書き出すまで待ってから keepalived を起動する。
set -eu

CONF=/shared/keepalived/keepalived.conf
mkdir -p /shared/keepalived/state

waited=0
while [ ! -s "$CONF" ]; do
  if [ $((waited % 60)) -eq 0 ]; then
    echo "AMoRe が $CONF を作成するのを待っています（cluster.enabled と cluster.keepalived.enabled を確認してください）"
  fi
  sleep 2
  waited=$((waited + 2))
done

# 前回の状態は使わない（起動直後は BACKUP として始まる）。
rm -f /shared/keepalived/state/*
exec keepalived --dont-fork --log-console --log-detail --use-file "$CONF"
