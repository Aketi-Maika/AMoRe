# AMoRe v0.5 - 共通IP（keepalived / VRRP）Kubernetes 用（DaemonSet・hostNetwork）
#
# ノードごとの設定は AMoRe が ConfigMap amore-keepalived（<ノード名>.conf）に生成する。
# entrypoint.sh が $NODE_NAME の設定を選んで起動し、ConfigMap の更新を検知して読み直す。
FROM alpine:3.22
RUN apk add --no-cache keepalived curl
COPY check_amore.sh notify_state.sh entrypoint.sh /usr/local/bin/
# enable_script_security のため、スクリプトは root 所有・他者は書き込み不可にする。
RUN chown root:root /usr/local/bin/check_amore.sh /usr/local/bin/notify_state.sh /usr/local/bin/entrypoint.sh \
    && chmod 0755 /usr/local/bin/check_amore.sh /usr/local/bin/notify_state.sh /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
