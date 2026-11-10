# AMoRe v0.1 - 接続・設定管理サーバー
FROM rust:1-bookworm AS build
WORKDIR /src
# Cargo.lock がある場合は一緒にコピーし、依存バージョンを固定してビルドする。
COPY Cargo.toml Cargo.loc[k] ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# カメラごとのコンテナを操作するための Docker CLI（ソケットはcompose.yamlでマウントする）。
COPY --from=docker:29-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=build /src/target/release/amore /usr/local/bin/amore
EXPOSE 8443
ENTRYPOINT ["amore"]
CMD ["--config", "/config/AMoRe.yaml", "serve"]
