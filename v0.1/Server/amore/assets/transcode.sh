#!/bin/sh
# AMoRe v0.1 - MediaMTX の runOnAvailable から起動する変換スクリプト
#
# <id>_in（ブラウザからWebRTCで受信）を読み取り、OBS向けの <id>（H.264・AAC）へ再配信する。
# - 映像: H.264 ならコピー（再エンコードなし）。VP8 等は libx264 で H.264 へ変換する。
# - 音声: Opus を AAC-LC へ変換する。音声なし（映像のみ送信）でも動作する。
# 受信が終わると MediaMTX が SIGINT で終了させる。異常終了時は runOnAvailableRestart で再起動される。
#
# 引数: 入力パス 出力パス 目標kbps 上限kbps fps 標本化周波数 最大ch 音声kbps x264プリセット
# 引数は AMoRe が AMoRe.yaml からカメラごとの mediamtx.yml を生成するときに埋め込む。
# このファイルは AMoRe のバイナリに組み込まれ、起動時に共有ボリュームへ書き出される。
set -eu

IN_PATH="$1"
OUT_PATH="$2"
V_TARGET_K="$3"
V_MAX_K="$4"
FPS="$5"
A_RATE="$6"
A_MAX_CH="$7"
A_BITRATE_K="$8"
X264_PRESET="${9:-ultrafast}"

PORT="${RTSP_PORT:-8554}"
IN_URL="rtsp://127.0.0.1:${PORT}/${IN_PATH}"
OUT_URL="rtsp://127.0.0.1:${PORT}/${OUT_PATH}"

# 受信中の映像コーデックを MediaMTX API で確認し、取得できなければ ffprobe で確認する。
INFO="$(wget -q -T 2 -O - "http://127.0.0.1:9997/v3/paths/get/${IN_PATH}" 2>/dev/null || true)"
case "$INFO" in
  *'"H264"'*) VIDEO_CODEC="h264" ;;
  *'"VP8"'*) VIDEO_CODEC="vp8" ;;
  *)
    VIDEO_CODEC="$(ffprobe -v error -rtsp_transport tcp -select_streams v:0 \
      -show_entries stream=codec_name -of default=nw=1:nk=1 "$IN_URL" 2>/dev/null || true)"
    ;;
esac

if [ "$VIDEO_CODEC" = "h264" ]; then
  echo "[amore] ${IN_PATH} -> ${OUT_PATH}: 映像 H.264 コピー / 音声 AAC 変換"
  set -- -c:v copy
else
  echo "[amore] ${IN_PATH} -> ${OUT_PATH}: 映像 ${VIDEO_CODEC:-不明} -> H.264 変換 / 音声 AAC 変換"
  # GOP を fps と同じ（1秒）にし、OBS の接続開始・復旧時に早く表示できるようにする。
  set -- -c:v libx264 -preset "$X264_PRESET" -tune zerolatency -profile:v high -pix_fmt yuv420p \
    -g "$FPS" -keyint_min "$FPS" -sc_threshold 0 \
    -b:v "${V_TARGET_K}k" -maxrate "${V_MAX_K}k" -bufsize "${V_MAX_K}k"
fi

# モノラル入力を無理にステレオ化しないため、上限が1chの場合のみチャンネル数を指定する。
if [ "$A_MAX_CH" = "1" ]; then
  set -- "$@" -ac 1
fi

exec ffmpeg -hide_banner -nostdin -loglevel warning \
  -fflags nobuffer -flags low_delay -rtsp_transport tcp -i "$IN_URL" \
  -map 0:v:0 -map '0:a:0?' \
  "$@" \
  -c:a aac -profile:a aac_low -b:a "${A_BITRATE_K}k" -ar "$A_RATE" \
  -f rtsp -rtsp_transport tcp "$OUT_URL"
