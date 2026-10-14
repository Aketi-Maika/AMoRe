#!/bin/sh
# AMoRe v0.3 - 録画ファイルの変換スクリプト
#
# MediaMTX の runOnRecordSegmentComplete から、録画の区切りが閉じるたびに起動される。
# 録画中の形式（fMP4 / MPEG-TS）のファイルを、再エンコードなしで MP4・MKV・MOV へ変換する。
# あわせて、同じカメラのディレクトリで保存期間を過ぎたファイルを削除する。
#
# 引数: 区切りファイル 変換先(mp4|mkv|mov) 元を残すか(0|1) 保存期間(分。0は削除しない)
# このファイルは AMoRe のバイナリに組み込まれ、起動時に共有ボリュームへ書き出される。
set -u

SEG="$1"
FMT="$2"
KEEP="${3:-0}"
RETENTION_MIN="${4:-0}"

log() {
  echo "[amore] convert: $*"
}

if [ ! -f "$SEG" ]; then
  log "ファイルがありません: $SEG"
  exit 0
fi

case "$FMT" in
  mp4|mov) EXTRA="-movflags +faststart" ;;
  mkv) EXTRA="" ;;
  *) log "変換しません（形式: $FMT）"; exit 0 ;;
esac

DIR="$(dirname "$SEG")"
BASE="${SEG%.*}"
SRC_EXT="${SEG##*.}"
OUT="${BASE}.${FMT}"
TMP="${BASE}.converting.${FMT}"

# fMP4 の録画（拡張子 .mp4）を MP4 へ変換する場合は同じ名前になるため、元を退避してから置き換える。
if [ "$OUT" = "$SEG" ]; then
  ORIG="${BASE}.segment.${SRC_EXT}"
else
  ORIG="$SEG"
fi

# shellcheck disable=SC2086 # EXTRA は空またはオプション列
if ffmpeg -hide_banner -nostdin -loglevel error -y -i "$SEG" -map 0 -c copy $EXTRA "$TMP"; then
  if [ "$ORIG" != "$SEG" ]; then
    mv -f "$SEG" "$ORIG"
  fi
  mv -f "$TMP" "$OUT"
  if [ "$KEEP" != "1" ] && [ "$ORIG" != "$OUT" ]; then
    rm -f "$ORIG"
  fi
  log "変換しました: $OUT"
else
  rm -f "$TMP"
  log "変換に失敗しました。元のファイルを残します: $SEG"
fi

# 保存期間を過ぎたファイルを削除する（MediaMTX は自分が書いた区切りファイルしか消さないため）。
if [ "$RETENTION_MIN" -gt 0 ] 2>/dev/null; then
  find "$DIR" -type f -mmin "+$RETENTION_MIN" \
    \( -name '*.mp4' -o -name '*.mkv' -o -name '*.mov' -o -name '*.ts' \) -delete 2>/dev/null || true
fi
