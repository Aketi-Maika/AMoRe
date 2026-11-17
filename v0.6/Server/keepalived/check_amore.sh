#!/bin/sh
# AMoRe v0.6 - 共通IPを受け持てるかの確認（keepalived の vrrp_script）
# この Server の AMoRe（https://127.0.0.1/healthz。compose.yaml で 443 に公開）が応答しなければ失敗し、
# keepalived は共通IPを手放して他の Server に移す。
code=$(curl -sk --max-time 2 -o /dev/null -w '%{http_code}' https://127.0.0.1/healthz) || exit 1
[ "$code" = "200" ]
