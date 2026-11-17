#!/bin/sh
# AMoRe v0.6 - Kubernetes（k3s）版の構築・更新を少ないコマンドで行う
#
# 使い方（v0.6/Container で実行）:
#   [Docker が使えるマシン（例: amore01）で]
#   sh scripts/amore-k8s.sh images [ノードのIP …]
#       イメージをビルドし、各ノードへコピーして取り込む（手順5）。ノードを省略すると kubernetes.yaml の nodes の ip。
#       各ノードへ ssh（ユーザー amore）し sudo で取り込むため、ノードごとにパスワードを聞かれることがある。
#
#   [k8s-hq1 で]
#   sh scripts/amore-k8s.sh config <拠点> <接続先IP>      例: config hq 10.40.0.20 / config site1 10.40.0.21
#       拠点の設定（k8s/sites/<拠点>/config/）を設定例から作り、拠点ごとの値（手順8の表）を書き換える。
#   sh scripts/amore-k8s.sh secrets
#       証明書（無ければ作る）と秘密（cluster-secret は自動生成、管理画面・モニターのパスワードは入力）を全拠点に登録する（手順7）。
#   sh scripts/amore-k8s.sh deploy [拠点 …]
#       MetalLB（無ければ導入）・IP プール・共通の部分・拠点を順に適用し、起動を待って確認する（手順4・6・9）。
#       拠点を省略すると k8s/sites/ の全拠点（hq は最後。共通IPを持っていない拠点から）。
#   sh scripts/amore-k8s.sh update <旧版> [拠点 …]       例: update v0.5
#       旧版の拠点の設定を引き継ぎ、deploy と同じ順に適用する（イメージは先に images で取り込んでおく）。
#   sh scripts/amore-k8s.sh status
set -eu

CONTAINER_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$(cd "$CONTAINER_DIR/.." && pwd)"
VERSION=0.6.0
METALLB_URL=https://raw.githubusercontent.com/metallb/metallb/v0.14.9/config/manifests/metallb-native.yaml
NS=amore
cd "$CONTAINER_DIR"

say() { printf '\n==> %s\n' "$*"; }
die() { printf '\nエラー: %s\n' "$*" >&2; exit 1; }

# 設定のある拠点（hq を最後にする）。
sites() {
  for d in k8s/sites/*/; do
    s="$(basename "$d")"
    [ -f "$d/config/AMoRe.yaml" ] && [ "$s" != hq ] && echo "$s"
  done
  [ -f k8s/sites/hq/config/AMoRe.yaml ] && echo hq
  return 0
}

# yaml の「  キー: 値」の最初の値（引用符を外す）。
yaml_value() {
  awk -v key="$2" '$1 == key ":" { v = $2; gsub(/"/, "", v); print v; exit }' "$1"
}

# kubernetes.yaml の nodes の ip。
node_ips() {
  f=k8s/sites/hq/config/kubernetes.yaml
  [ -f "$f" ] || f="$ROOT/Server/config.example/kubernetes.yaml"
  awk '$1 == "ip:" { print $2 }' "$f"
}

show_status() {
  say "ノード・Pod"
  kubectl get nodes -L amore.site
  kubectl -n "$NS" get pods -o wide -L amore.site,amore.camera
  say "Service（拠点の接続先IP・共通IP）"
  kubectl -n "$NS" get svc -o wide
  say "共通IPの受け持ち（keepalived）"
  kubectl -n "$NS" logs ds/amore-keepalived --all-containers --prefix --tail 200 2>/dev/null | grep -E 'MASTER|BACKUP|FAULT' | tail -6 || echo "（keepalived のログがありません）"
}

cmd="${1:-}"
[ $# -gt 0 ] && shift
case "$cmd" in
  images)
    # shellcheck disable=SC2046 # IP の並び
    [ $# -gt 0 ] || set -- $(node_ips)
    [ $# -gt 0 ] || die "ノードの IP がわかりません（引数で指定してください）"
    say "イメージのビルド（初回は Rust のコンパイルに数分〜十数分かかります）"
    (cd "$ROOT" && sh Container/scripts/build-image.sh) || die "ビルドに失敗しました。エラー全文を記録して開発側へ連絡してください"
    tar="$ROOT/amore-images-$VERSION.tar"
    for n in "$@"; do
      say "ノード $n へコピーして取り込み"
      scp "$tar" "amore@$n:/tmp/"
      ssh -t "amore@$n" "sudo k3s ctr images import /tmp/amore-images-$VERSION.tar && rm -f /tmp/amore-images-$VERSION.tar && sudo k3s crictl images | grep amore"
    done
    say "完了：各ノードに amore:$VERSION・amore-keepalived:$VERSION があること（上の表示）。sha256 を作業記録に残してください"
    ;;

  config)
    [ $# -eq 2 ] || die "拠点と接続先IPを指定してください（例: sh scripts/amore-k8s.sh config hq 10.40.0.20）"
    site="$1"
    host="$2"
    dir="k8s/sites/$site/config"
    [ -d "k8s/sites/$site" ] || die "k8s/sites/$site がありません（拠点のマニフェストを先に用意してください）"
    [ -f "$dir/AMoRe.yaml" ] && die "$dir/AMoRe.yaml があります（作り直す場合は消してから実行してください）"
    say "$dir を設定例から作成"
    cp "$ROOT"/Server/config.example/*.yaml "$dir/"
    sed -i \
      -e "s/^  public_host: .*/  public_host: \"$host\"/" \
      -e 's/^  runtime: docker/  runtime: kubernetes/' \
      -e 's/^  docker_subnet: .*/  docker_subnet: "10.42.0.0\/16"   # Pod の範囲/' \
      "$dir/AMoRe.yaml"
    sed -i \
      -e 's/^  enabled: false/  enabled: true/' \
      -e "s/^  site: .*/  site: $site/" \
      -e 's/^  shared_secret: .*/  shared_secret: file:\/config\/secret\/cluster-secret/' \
      "$dir/cluster.yaml"
    sed -i -e "s/^  metallb_pool: .*/  metallb_pool: amore-$site/" "$dir/kubernetes.yaml"
    sed -i -e "s/^      site: .*/      site: $site/" -e "s/^    host: .*/    host: amore-$site         # Zabbix に登録したホスト名/" "$dir/monitoring.yaml"
    # 管理画面・モニターはパスワードを必要にする（値は secrets で登録する Secret のファイル）。
    awk '
      /^  [a-z]/ { scope = $1; sub(/:$/, "", scope) }
      (scope == "admin" || scope == "monitor") && /^    password_required:/ { print "    password_required: true"; next }
      (scope == "admin" || scope == "monitor") && /^    password:/ { print "    password: file:/config/secret/" scope "-password"; next }
      { print }
    ' "$dir/access.yaml" > "$dir/access.yaml.tmp" && mv "$dir/access.yaml.tmp" "$dir/access.yaml"
    say "設定例との差（拠点ごとの値）"
    for f in "$dir"/*.yaml; do
      diff -u "$ROOT/Server/config.example/$(basename "$f")" "$f" || true
    done
    echo
    echo "録画する場合は kubernetes.yaml の recordings_claim に amore-$site-recordings を書き、k8s/sites/$site/recordings.yaml の NAS を設定してください"
    echo "kubernetes.yaml の nodes・cluster.yaml の peers・共通IP は全拠点で同じにしてください（設定例のままなら変更不要）"
    ;;

  secrets)
    # shellcheck disable=SC2046 # 拠点の並び
    set -- $(sites)
    [ $# -gt 0 ] || die "拠点の設定がありません（先に config を実行してください）"
    kubectl get ns "$NS" >/dev/null 2>&1 || kubectl apply -k k8s/base
    certs="$ROOT/Server/certs"
    if [ ! -f "$certs/server.crt" ]; then
      ips=""
      for s in "$@"; do ips="$ips $(yaml_value "k8s/sites/$s/config/AMoRe.yaml" public_host)"; done
      c="k8s/sites/$1/config/cluster.yaml"
      ips="$ips $(yaml_value "$c" camera_vip | cut -d/ -f1) $(yaml_value "$c" obs_vip | cut -d/ -f1)"
      say "証明書の作成（$ips）"
      # shellcheck disable=SC2086 # ips は IP の並び
      sh "$ROOT/Server/scripts/gen-cert.sh" $ips
      echo "上の SHA256 フィンガープリントを作業記録に残し、カメラPC・編集PC・管理PCで信頼登録してください"
    fi
    kubectl -n "$NS" create secret generic amore-tls \
      --from-file=server.crt="$certs/server.crt" --from-file=server.key="$certs/server.key" \
      --dry-run=client -o yaml | kubectl apply -f -

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    chmod 700 "$tmp"
    # 既に登録済みの値は引き継ぐ（cluster-secret は全拠点で同じ値にする）。
    for key in cluster-secret admin-password monitor-password; do
      kubectl -n "$NS" get secret "amore-$1-secrets" -o "jsonpath={.data.$key}" 2>/dev/null | base64 -d > "$tmp/$key" 2>/dev/null || true
      [ -s "$tmp/$key" ] || rm -f "$tmp/$key"
    done
    [ -f "$tmp/cluster-secret" ] || openssl rand -base64 48 > "$tmp/cluster-secret"
    for key in admin-password monitor-password; do
      if [ -f "$tmp/$key" ]; then prompt="（Enter で今の値のまま）"; else prompt=""; fi
      printf '%s を入力%s: ' "$key" "$prompt"
      stty -echo 2>/dev/null || true
      read -r value || value=""
      stty echo 2>/dev/null || true
      echo
      if [ -n "$value" ]; then printf '%s\n' "$value" > "$tmp/$key"; fi
    done
    keys=""
    for k in "$tmp"/*; do keys="$keys $(basename "$k")"; done
    for s in "$@"; do
      say "Secret amore-$s-secrets を登録（$keys ）"
      kubectl -n "$NS" create secret generic "amore-$s-secrets" --from-file="$tmp" --dry-run=client -o yaml | kubectl apply -f -
    done
    echo "Claude API キー等を加える場合は、同じ Secret に --from-file=<キー>=<ファイル> で加えてください"
    ;;

  deploy | update)
    if [ "$cmd" = update ]; then
      [ $# -ge 1 ] || die "旧版を指定してください（例: sh scripts/amore-k8s.sh update v0.5）"
      old="$ROOT/../$1/Container/k8s/sites"
      shift
      for d in "$old"/*/; do
        s="$(basename "$d")"
        if [ -f "$d/config/AMoRe.yaml" ] && [ ! -f "k8s/sites/$s/config/AMoRe.yaml" ]; then
          say "拠点 $s の設定を旧版から引き継ぎ"
          mkdir -p "k8s/sites/$s/config"
          cp -a "$d/config/." "k8s/sites/$s/config/"
        fi
      done
    fi
    # shellcheck disable=SC2046 # 拠点の並び
    [ $# -gt 0 ] || set -- $(sites)
    [ $# -gt 0 ] || die "拠点の設定がありません（先に config を実行してください）"

    if ! kubectl get ns metallb-system >/dev/null 2>&1; then
      say "MetalLB の導入"
      kubectl apply -f "$METALLB_URL"
      kubectl -n metallb-system rollout status deploy/controller --timeout=300s
      kubectl -n metallb-system wait --for=condition=Ready pod -l component=speaker --timeout=300s
    fi
    say "IP プール・共通の部分（namespace・権限・keepalived）"
    kubectl apply -f k8s/metallb/pools.yaml
    kubectl apply -k k8s/base
    kubectl -n "$NS" get secret amore-tls >/dev/null 2>&1 || die "Secret amore-tls がありません（先に secrets を実行してください）"

    for s in "$@"; do
      say "拠点 $s を適用"
      kubectl -n "$NS" get secret "amore-$s-secrets" >/dev/null 2>&1 || echo "注意: Secret amore-$s-secrets がありません（file:/config/secret/… の参照が NG になります）"
      kubectl apply -k "k8s/sites/$s"
      kubectl -n "$NS" rollout restart "deployment/amore-$s"
      kubectl -n "$NS" rollout status "deployment/amore-$s" --timeout=300s || die "amore-$s が起動しません（kubectl -n $NS logs deploy/amore-$s を確認してください）"
      kubectl -n "$NS" exec "deploy/amore-$s" -- amore --config /config/AMoRe.yaml check || echo "注意: 拠点 $s の設定の検証で NG があります（上の表示）"
    done
    show_status
    ;;

  status)
    show_status
    ;;

  *)
    sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
