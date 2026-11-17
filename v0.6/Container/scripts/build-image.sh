#!/bin/sh
# AMoRe v0.6 - Kubernetes 用イメージのビルドと書き出し
#
# 使い方（Docker が使えるマシンで実行。例：v0.4 までの Server VM）:
#   sh Container/scripts/build-image.sh
# 作成されるもの:
#   amore:0.6.0             AMoRe 本体（送信画面・管理Web・OBSドックを含む）
#   amore-keepalived:0.6.0  共通IP（keepalived）
#   amore-images-0.6.0.tar  上の2つを書き出したファイル（各ノードへコピーして取り込む）
# 取り込み（各ノードで実行）:
#   sudo k3s ctr images import amore-images-0.6.0.tar
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VERSION=0.6.0

docker build -f "$ROOT/Container/Dockerfile" -t "amore:$VERSION" "$ROOT"
docker build -t "amore-keepalived:$VERSION" "$ROOT/Container/keepalived"
docker save "amore:$VERSION" "amore-keepalived:$VERSION" -o "amore-images-$VERSION.tar"

echo "作成しました: $(pwd)/amore-images-$VERSION.tar"
ls -lh "amore-images-$VERSION.tar"
sha256sum "amore-images-$VERSION.tar"
