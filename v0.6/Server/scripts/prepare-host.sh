#!/bin/sh
# AMoRe v0.6 - Server VM（Rocky Linux 9）の準備（手順3〜4）を1回のコマンドで行う
#
# 使い方: sudo sh scripts/prepare-host.sh [--vrrp-peer <相手の Server の IP> …]
#   - パッケージの更新、ゲストエージェント（qemu-guest-agent）・git
#   - firewalld：443/tcp と、AMoRe.yaml の WebRTC・SRT のポート範囲（UDP）。--vrrp-peer を付けると冗長構成の VRRP も許可する
#   - UDP 受信バッファ（sysctl）
#   - Docker CE・Compose の導入と、実行したユーザー（sudo の前のユーザー）の docker グループへの追加
# 終わったら再起動する（sudo reboot）。再ログイン後は sudo なしで docker を使える。
# 何度実行してもよい（済んでいるものはそのまま）。
set -eu

[ "$(id -u)" -eq 0 ] || { echo "sudo で実行してください（例: sudo sh scripts/prepare-host.sh）" >&2; exit 1; }
SERVER_DIR="$(cd "$(dirname "$0")/.." && pwd)"
USER_NAME="${SUDO_USER:-amore}"

say() { printf '\n==> %s\n' "$*"; }

peers=""
while [ $# -gt 0 ]; do
  case "$1" in
    --vrrp-peer)
      [ $# -ge 2 ] || { echo "--vrrp-peer の後に IP を指定してください" >&2; exit 1; }
      peers="$peers $2"
      shift 2
      ;;
    *)
      echo "不明な引数: $1" >&2
      exit 1
      ;;
  esac
done

# ポートの範囲（config/ があればその値、無ければ設定例の値）。
cfg="$SERVER_DIR/config/AMoRe.yaml"
[ -f "$cfg" ] || cfg="$SERVER_DIR/config.example/AMoRe.yaml"
range() {
  awk -v key="$1" '
    $1 == key ":" { r = 1; next }
    r && $1 == "first:" { f = $2 }
    r && $1 == "last:" { print f "-" $2; exit }
  ' "$cfg"
}
webrtc="$(range webrtc_udp_ports)"
srt="$(range srt_ports)"

say "パッケージの更新・ゲストエージェント・git"
dnf -y update
dnf -y install qemu-guest-agent git dnf-plugins-core
systemctl enable --now qemu-guest-agent

say "firewalld（443/tcp・WebRTC ${webrtc}/udp・SRT ${srt}/udp${peers:+・VRRP の相手${peers}}）"
firewall-cmd --permanent --add-service=https
firewall-cmd --permanent --add-port="${webrtc}/udp"
firewall-cmd --permanent --add-port="${srt}/udp"
# shellcheck disable=SC2086 # peers は IP の並び
for p in $peers; do
  firewall-cmd --permanent --add-rich-rule="rule family=\"ipv4\" source address=\"$p\" protocol value=\"vrrp\" accept"
done
firewall-cmd --reload
firewall-cmd --list-all

say "UDP 受信バッファ（/etc/sysctl.d/90-amore.conf）"
printf 'net.core.rmem_max=16777216\nnet.core.rmem_default=8388608\n' > /etc/sysctl.d/90-amore.conf
sysctl --system >/dev/null
sysctl net.core.rmem_max net.core.rmem_default

say "Docker CE・Compose"
if [ ! -f /etc/yum.repos.d/docker-ce.repo ]; then
  dnf config-manager --add-repo https://download.docker.com/linux/rhel/docker-ce.repo
fi
dnf -y install docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker
usermod -aG docker "$USER_NAME"

say "確認（作業記録に残す）"
docker version --format 'Docker {{.Server.Version}}'
docker compose version
printf 'SELinux: %s / 時刻同期: %s\n' "$(getenforce)" "$(timedatectl show -p NTPSynchronized --value)"
say "完了：sudo reboot で再起動し、$USER_NAME で再ログインしてから sh scripts/amore.sh setup <ServerのIP> を実行してください"
