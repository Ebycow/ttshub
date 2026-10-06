# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# 依存とビルド成果物はビルドキャッシュに置き、イメージには残さない
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/ttshub /ttshub

FROM debian:bookworm-slim
COPY --from=build /ttshub /usr/local/bin/ttshub
# カタログのスナップショットなどを置く場所
RUN mkdir -p /var/lib/ttshub && chown 10001:10001 /var/lib/ttshub
USER 10001
EXPOSE 8790
ENTRYPOINT ["ttshub"]
