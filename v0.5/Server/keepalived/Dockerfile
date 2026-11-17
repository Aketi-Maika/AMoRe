# AMoRe v0.5 - 共通IP（keepalived / VRRP）
#
# 設定ファイルは AMoRe が cluster.yaml から生成し、共有ボリューム（/shared/keepalived/keepalived.conf）へ書く。
# ホストのネットワークで動かし（compose.yaml の network_mode: host）、VM の NIC に共通IPを載せる。
FROM alpine:3.22
RUN apk add --no-cache keepalived curl
COPY check_amore.sh notify_state.sh entrypoint.sh /usr/local/bin/
# enable_script_security のため、スクリプトは root 所有・他者は書き込み不可にする。
RUN chown root:root /usr/local/bin/check_amore.sh /usr/local/bin/notify_state.sh /usr/local/bin/entrypoint.sh \
    && chmod 0755 /usr/local/bin/check_amore.sh /usr/local/bin/notify_state.sh /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
