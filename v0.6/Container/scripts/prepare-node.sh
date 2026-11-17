#!/bin/sh
# AMoRe v0.6 - k3s ノード（Rocky Linux 9）の準備と k3s の導入（手順2〜3）を1回のコマンドで行う
#
# 使い方:
#   最初のノード : sudo sh scripts/prepare-node.sh <拠点> <このノードのIP>                  例: hq 10.40.0.41
#   参加するノード: sudo sh scripts/prepare-node.sh <拠点> <このノードのIP> <最初のノードのIP>  例: hq 10.40.0.42 10.40.0.41
#     参加するノードでは、最初のノードで表示されたトークンの入力を求める（環境変数 K3S_TOKEN でも渡せる）。
#
# 行うこと（何度実行してもよい）:
#   - パッケージの更新、ゲストエージェント（qemu-guest-agent）
#   - UDP 受信バッファ（sysctl）
#   - firewalld：k3s・Pod/Service の範囲・AMoRe（HTTPS・WebRTC・SRT・keepalived の確認用 18443）・MetalLB、
#     他のノードとの VRRP（ノードの一覧は kubernetes.yaml の nodes の ip）
#   - /etc/rancher/k3s/config.yaml の作成と k3s（stable）の導入。ラベル amore.site=<拠点>
set -eu

[ "$(id -u)" -eq 0 ] || { echo "sudo で実行してください" >&2; exit 1; }
[ $# -ge 2 ] || { sed -n '4,7p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 1; }
SITE="$1"
NODE_IP="$2"
JOIN_IP="${3:-}"
CONTAINER_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$(cd "$CONTAINER_DIR/.." && pwd)"

say() { printf '\n==> %s\n' "$*"; }

nodes_file="$CONTAINER_DIR/k8s/sites/hq/config/kubernetes.yaml"
[ -f "$nodes_file" ] || nodes_file="$ROOT/Server/config.example/kubernetes.yaml"
ALL_IPS="$(awk '$1 == "ip:" { print $2 }' "$nodes_file")"
IFACE="$(ip -o -4 addr show | awk -v ip="$NODE_IP" '{ split($4, a, "/") } a[1] == ip { print $2; exit }')"
[ -n "$IFACE" ] || { echo "IP $NODE_IP を持つ NIC がありません（このノードの IP を確認してください）" >&2; exit 1; }

say "パッケージの更新・ゲストエージェント"
dnf -y update
dnf -y install qemu-guest-agent git
systemctl enable --now qemu-guest-agent

say "UDP 受信バッファ（/etc/sysctl.d/90-amore.conf）"
printf 'net.core.rmem_max=16777216\nnet.core.rmem_default=8388608\n' > /etc/sysctl.d/90-amore.conf
sysctl --system >/dev/null

say "firewalld"
fw() { firewall-cmd --permanent "$@" >/dev/null; }
fw --add-port=6443/tcp --add-port=2379-2380/tcp --add-port=10250/tcp --add-port=8472/udp   # k3s・kubelet・flannel VXLAN
fw --zone=trusted --add-source=10.42.0.0/16 --add-source=10.43.0.0/16                       # Pod・Service の範囲
fw --add-service=https --add-port=8189-8192/udp --add-port=8890-8893/udp --add-port=18443/tcp # AMoRe・keepalived の確認用
fw --add-port=7946/tcp --add-port=7946/udp                                                   # MetalLB
# shellcheck disable=SC2086 # ALL_IPS は IP の並び
for ip in $ALL_IPS; do
  [ "$ip" = "$NODE_IP" ] && continue
  fw --add-rich-rule="rule family=\"ipv4\" source address=\"$ip\" protocol value=\"vrrp\" accept"  # keepalived
done
firewall-cmd --reload
firewall-cmd --list-all

say "k3s の設定（/etc/rancher/k3s/config.yaml。NIC $IFACE・拠点 $SITE）"
mkdir -p /etc/rancher/k3s
umask 077
{
  if [ -z "$JOIN_IP" ]; then
    echo "cluster-init: true"
  else
    echo "server: https://$JOIN_IP:6443"
  fi
  echo "node-ip: $NODE_IP"
  echo "flannel-iface: $IFACE"
  echo "node-label:"
  echo "  - amore.site=$SITE"
  echo "disable:"
  echo "  - servicelb"
  echo "  - traefik"
  echo "tls-san:"
  # shellcheck disable=SC2086 # ALL_IPS は IP の並び
  for ip in $ALL_IPS; do echo "  - $ip"; done
  echo 'write-kubeconfig-mode: "0600"'
  echo "# 拠点間VPN越しの etcd のため、通知間隔と選挙の待ち時間を長めにする"
  echo "etcd-arg:"
  echo "  - heartbeat-interval=500"
  echo "  - election-timeout=5000"
} > /etc/rancher/k3s/config.yaml.new
if [ -n "$JOIN_IP" ]; then
  token="${K3S_TOKEN:-}"
  if [ -z "$token" ]; then
    printf '最初のノード（%s）のトークンを入力: ' "$JOIN_IP"
    stty -echo 2>/dev/null || true
    read -r token || token=""
    stty echo 2>/dev/null || true
    echo
  fi
  [ -n "$token" ] || { echo "トークンがありません" >&2; exit 1; }
  echo "token: $token" >> /etc/rancher/k3s/config.yaml.new
fi
mv /etc/rancher/k3s/config.yaml.new /etc/rancher/k3s/config.yaml

say "k3s の導入（stable）"
curl -sfL https://get.k3s.io | INSTALL_K3S_CHANNEL=stable sh -s - server
k3s --version | head -1
k3s kubectl get nodes -L amore.site -o wide || true
if [ -z "$JOIN_IP" ]; then
  say "参加するノードで入力するトークン（管理台帳で管理し、本書・リポジトリに記載しない）"
  cat /var/lib/rancher/k3s/server/node-token
fi
say "完了：k3s の版を作業記録に残してください。パッケージを更新したため、全ノードの導入後に1台ずつ sudo reboot してください"
