# AMoRe v0.6 - Kubernetes 用イメージ（AMoRe 本体＋送信画面・管理Web・OBSドック）
#
# ビルドコンテキストは v0.6/（Container/scripts/build-image.sh を参照）。
#   docker build -f Container/Dockerfile -t amore:0.6.0 .
# Docker Compose 版（Server/amore/Dockerfile）との違い:
#   - 画面のファイル（Camera/www・OBS/dock・Server/admin）をイメージに含める（ホストのディレクトリをマウントしない）
#   - Docker CLI を含めない（k8s API でカメラの Pod を作る）。代わりに AI の静止画取得用に ffmpeg を含める
#   - 一般ユーザー（UID 10001）で動かす
FROM rust:1-bookworm AS build
WORKDIR /src
# Cargo.lock がある場合は一緒にコピーし、依存バージョンを固定してビルドする。
COPY Server/amore/Cargo.toml Server/amore/Cargo.loc[k] ./
COPY Server/amore/src ./src
COPY Server/amore/assets ./assets
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin amore
COPY --from=build /src/target/release/amore /usr/local/bin/amore
COPY Camera/www /srv/camera
COPY OBS/dock /srv/obs
COPY Server/admin /srv/admin
USER 10001:10001
EXPOSE 8443
ENTRYPOINT ["amore"]
CMD ["--config", "/config/AMoRe.yaml", "serve"]
