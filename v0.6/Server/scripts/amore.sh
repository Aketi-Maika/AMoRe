#!/bin/sh
# AMoRe v0.6 - Server（Docker Compose）の構築・更新・設定反映を1回のコマンドで行う
#
# 使い方（Server の v0.6/Server で実行。sudo は不要。docker グループのユーザーで実行する）:
#   sh scripts/amore.sh setup [<ServerのIP> [他の IP・DNS名 …]]
#       初回の構築（手順5〜6）。Docker ネットワーク・設定（config/）・TLS証明書が無ければ作り、
#       ビルド → 設定の検証 → 起動 → 確認 まで行う。IP を省略すると config/AMoRe.yaml の server.public_host を使う。
#       冗長構成では、証明書に入れる全 Server の実IP・共通IPを並べる（例: 10.40.0.20 10.40.0.21 149.40.2.1 10.40.0.25）。
#   sh scripts/amore.sh join <拠点> <このServerのIP>    例: sh scripts/amore.sh join site1 10.40.0.21
#       冗長構成の2台目以降。1台目からコピーした config/・certs/ を、この Server の値（server.public_host・
#       cluster.site・監視のラベル）に変えてから setup と同じことを行う。
#   sh scripts/amore.sh update <旧版>            例: sh scripts/amore.sh update v0.5
#       旧版（/opt/AMoRe/<旧版>/Server）の設定・証明書を引き継ぎ、ビルド・検証が通ってから旧版を止めて切り替える。
#   sh scripts/amore.sh rollback <旧版>
#       この版を止め、カメラ一覧（cameras.yaml）を旧版へ戻して旧版を起動する。
#   sh scripts/amore.sh apply
#       config/ の変更を反映する（検証してから amore を作り直す）。
#   sh scripts/amore.sh status
#       状態の確認（コンテナ・/healthz・Docker・イメージ・メトリクス）。
#
# 冗長構成（cluster.enabled）・1Password Connect（secrets.onepassword_connect.enabled）を使う設定なら、
# compose のプロファイル（cluster / op-connect）を自動で付ける。
set -eu

SERVER_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SERVER_DIR"

say() { printf '\n==> %s\n' "$*"; }
die() { printf '\nエラー: %s\n' "$*" >&2; exit 1; }

# config/ の yaml から「最上位のセクション → 子 → enabled」の値を読む（例: cluster の enabled）。
# $1 = 設定のディレクトリ, $2 = セクション, $3 = 子のキー（空ならセクション直下の enabled）
config_enabled() {
  awk -v sec="$2" -v child="$3" '
    /^[^ #]/ { s = ($0 ~ "^" sec ":"); c = (child == "") }
    s && child != "" && /^  [^ #]/ { c = ($0 ~ "^  " child ":") }
    s && c && child == "" && /^  enabled:/ { print $2; exit }
    s && c && child != "" && /^    enabled:/ { print $2; exit }
  ' "$1"/*.yaml 2>/dev/null
}

# config/AMoRe.yaml などから「  キー: 値」の値を読む（引用符を外す）。
config_value() {
  awk -v key="$2" '$1 == key ":" { v = $2; gsub(/"/, "", v); print v; exit }' "$1"/*.yaml 2>/dev/null
}

# 最上位のセクション（例: cluster）を書いた config/ のファイル。
section_file() {
  grep -l "^$1:" config/*.yaml 2>/dev/null | head -1
}

# 設定に合わせた compose のプロファイル。
profiles() {
  p=""
  [ "$(config_enabled "$1" cluster "")" = "true" ] && p="$p --profile cluster"
  [ "$(config_enabled "$1" secrets onepassword_connect)" = "true" ] && p="$p --profile op-connect"
  printf '%s' "${p# }"
}

# amore が healthy になるまで待つ（最大3分）。
wait_healthy() {
  say "起動を待っています"
  i=0
  while [ $i -lt 36 ]; do
    health="$(docker compose ps --format '{{.Health}}' amore 2>/dev/null || true)"
    if [ "$health" = "healthy" ]; then
      echo "amore: healthy"
      return 0
    fi
    sleep 5
    i=$((i + 1))
  done
  docker compose logs --tail 50 amore
  die "amore が healthy になりません（上のログを確認してください）"
}

build_and_check() {
  say "ビルド（初回は Rust のコンパイルに数分〜十数分かかります）"
  docker compose build || die "ビルドに失敗しました。エラー全文を記録して開発側へ連絡してください"
  say "設定の検証"
  docker compose run --rm --no-deps amore check || die "設定の検証に失敗しました（上の項目を config/ で直してから再実行してください）"
}

status() {
  say "コンテナ"
  docker compose ps
  docker ps --filter label=amore.managed=true --format 'table {{.Names}}\t{{.Status}}'
  say "AMoRe"
  printf 'healthz: %s\n' "$(curl -sk --max-time 5 https://127.0.0.1/healthz || echo 応答なし)"
  overview="$(curl -sk --max-time 10 https://127.0.0.1/api/v1/admin/overview || true)"
  case "$overview" in
    *'"docker_version"'*)
      printf '%s\n' "$overview" | grep -o '"docker_version":[^,]*\|"image_ready":[^,]*\|"docker_error":[^,]*' || true
      ;;
    *'"error"'*) echo "管理画面の入室パスワードが ON のため詳細は管理Webで確認してください" ;;
    *) echo "overview を取得できません" ;;
  esac
  metrics="$(curl -sk --max-time 10 https://127.0.0.1/metrics || true)"
  printf '%s\n' "$metrics" | grep -E '^amore_(info|docker_up|cameras|camera_receiving)[ {]' || echo "メトリクスを取得できません（monitoring.prometheus）"
}

setup_cmd() {
  if [ ! -f config/AMoRe.yaml ]; then
    say "設定ファイルを作成（config.example → config/）"
    cp config.example/*.yaml config/
    if [ $# -gt 0 ]; then
      sed -i "s/^  public_host: .*/  public_host: \"$1\"/" config/AMoRe.yaml
    fi
  else
    echo "config/AMoRe.yaml があるため、設定はそのまま使います"
  fi
  host="$(config_value config public_host)"
  [ $# -gt 0 ] || set -- "$host"
  [ "$1" = "$host" ] || echo "注意: 証明書の先頭の IP（$1）と server.public_host（$host）が違います"

  subnet="$(config_value config docker_subnet)"
  if docker network inspect amore_net >/dev/null 2>&1; then
    echo "Docker ネットワーク amore_net はあります"
  else
    say "Docker ネットワーク amore_net を作成（$subnet）"
    docker network create --subnet "$subnet" amore_net
  fi

  if [ -f certs/server.crt ]; then
    echo "certs/server.crt があるため、証明書はそのまま使います"
  else
    say "TLS証明書の作成（$*）"
    sh scripts/gen-cert.sh "$@"
    echo "上の SHA256 フィンガープリントを作業記録に残してください"
  fi

  build_and_check
  P="$(profiles config)"
  say "起動${P:+（$P）}"
  # shellcheck disable=SC2086 # P はプロファイルの引数の並び
  docker compose $P up -d
  wait_healthy
  status
  say "完了：管理Web https://$host/admin/ でカメラを追加してください"
}

cmd="${1:-}"
[ $# -gt 0 ] && shift
case "$cmd" in
  setup)
    setup_cmd "$@"
    ;;

  join)
    [ $# -ge 2 ] || die "拠点の名前と IP を指定してください（例: sh scripts/amore.sh join site1 10.40.0.21）"
    site="$1"
    ip="$2"
    shift 2
    { [ -f config/AMoRe.yaml ] && [ -f certs/server.crt ] && [ -f certs/server.key ]; } || die "先に 1台目の Server から config/ と certs/ をコピーしてください"
    chmod 600 certs/server.key
    say "この Server の値に変更（server.public_host: $ip / cluster.site: $site / 監視のラベル）"
    f="$(section_file server)"
    [ -n "$f" ] && sed -i "s/^  public_host: .*/  public_host: \"$ip\"/" "$f"
    f="$(section_file cluster)"
    [ -n "$f" ] && sed -i "s/^  site: .*/  site: $site/" "$f"
    f="$(section_file monitoring)"
    # 1ファイルの設定（kubernetes の nodes も同じファイル）では字下げが同じ行があるため、監視のラベルは変えない。
    if [ -n "$f" ] && ! grep -q '^kubernetes:' "$f"; then
      sed -i -e "s/^      site: .*/      site: $site/" -e "s/^    host: .*/    host: $(hostname -s)/" "$f"
    else
      echo "注意: monitoring の loki.labels.site・zabbix.host は手で変えてください"
    fi
    setup_cmd "$ip" "$@"
    ;;

  update)
    [ $# -ge 1 ] || die "旧版を指定してください（例: sh scripts/amore.sh update v0.5）"
    old="$(cd "$SERVER_DIR/../.." && pwd)/$1/Server"
    [ -f "$old/config/AMoRe.yaml" ] || die "$old/config/AMoRe.yaml がありません"
    say "現在のコミットとカメラ一覧（作業記録用）"
    git -C "$SERVER_DIR" log --oneline -1 || true
    grep -A30 '^cameras:' "$old"/config/*.yaml 2>/dev/null | head -40 || true
    if [ -f config/AMoRe.yaml ]; then
      echo "config/AMoRe.yaml があるため、設定は引き継ぎません（この版の config/ を使います）"
    else
      say "設定・証明書を $1 から引き継ぎ"
      cp -a "$old/config/." config/
    fi
    [ -f certs/server.crt ] || cp -a "$old/certs/server.crt" "$old/certs/server.key" certs/
    for f in config.example/*.yaml; do
      [ -f "config/$(basename "$f")" ] || echo "（設定例にあり config/ に無いファイル: $(basename "$f")。書かなくても既定値で動きます）"
    done

    build_and_check
    say "$1 を停止"
    (cd "$old" && docker compose --profile cluster --profile op-connect down)
    P="$(profiles config)"
    say "v0.6 を起動${P:+（$P）}"
    # shellcheck disable=SC2086 # P はプロファイルの引数の並び
    docker compose $P up -d
    wait_healthy
    status
    say "完了：送信画面・OBS ドックを再読み込みしてください。問題があれば sh scripts/amore.sh rollback $1"
    echo "旧版のイメージを消す場合: docker image ls 'amore*' で確認して docker image rm"
    ;;

  rollback)
    [ $# -ge 1 ] || die "旧版を指定してください（例: sh scripts/amore.sh rollback v0.5）"
    old="$(cd "$SERVER_DIR/../.." && pwd)/$1/Server"
    [ -f "$old/config/AMoRe.yaml" ] || die "$old/config/AMoRe.yaml がありません"
    say "v0.6 を停止"
    docker compose --profile cluster --profile op-connect down
    cams="$(grep -l '^cameras:' config/*.yaml | head -1)"
    old_cams="$(grep -l '^cameras:' "$old"/config/*.yaml | head -1)"
    if [ -n "$cams" ] && [ -n "$old_cams" ]; then
      say "カメラ一覧を戻す（$cams → $old_cams）"
      cp -a "$cams" "$old_cams"
    fi
    P="$(profiles "$old/config")"
    say "$1 を起動${P:+（$P）}"
    # shellcheck disable=SC2086 # P はプロファイルの引数の並び
    (cd "$old" && docker compose $P up -d)
    echo "v0.6 で monitoring.yaml に quality・latency を書いた場合、$1 は読めないため消してから再実行してください"
    ;;

  apply)
    say "設定の検証"
    docker compose run --rm --no-deps amore check || die "設定の検証に失敗しました（反映していません）"
    P="$(profiles config)"
    say "反映${P:+（$P）}"
    # shellcheck disable=SC2086 # P はプロファイルの引数の並び
    docker compose $P up -d --force-recreate --no-deps amore
    # shellcheck disable=SC2086 # P はプロファイルの引数の並び
    docker compose $P up -d
    wait_healthy
    status
    ;;

  status)
    status
    ;;

  *)
    sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
