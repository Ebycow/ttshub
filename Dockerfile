# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# 依存とビルド成果物はビルドキャッシュに置き、イメージには残さない
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/ttshub /ttshub

# Open JTalk の声: MMDAgent の Mei と Takumi（名古屋工業大学、CC BY 3.0）
FROM debian:bookworm-slim AS hts-voices
ARG MMDAGENT_EXAMPLE_SHA256=f702f2109a07dca103c7b9a5123a25c6dda038f0d7fcc899ff0281d07e873a63
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSLo /tmp/mmdagent.zip https://sourceforge.net/projects/mmdagent/files/MMDAgent_Example/MMDAgent_Example-1.8/MMDAgent_Example-1.8.zip/download \
 && echo "${MMDAGENT_EXAMPLE_SHA256}  /tmp/mmdagent.zip" | sha256sum -c - \
 && unzip -q /tmp/mmdagent.zip 'MMDAgent_Example-1.8/Voice/mei/*' 'MMDAgent_Example-1.8/Voice/takumi/*' -d /tmp \
 && mkdir /voices \
 && mv /tmp/MMDAgent_Example-1.8/Voice/mei /tmp/MMDAgent_Example-1.8/Voice/takumi /voices/

FROM debian:bookworm-slim
# openjtalk プラグインが使う open_jtalk と辞書（/var/lib/mecab/dic/open-jtalk/naist-jdic）と声（/usr/share/hts-voice）
# hts-voice-nitech-jp-atr503-m001 は contrib にあるので、contrib も有効にする
RUN sed -i 's/^Components: main$/Components: main contrib/' /etc/apt/sources.list.d/debian.sources \
 && apt-get update \
 && apt-get install -y --no-install-recommends open-jtalk open-jtalk-mecab-naist-jdic hts-voice-nitech-jp-atr503-m001 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=hts-voices /voices/ /usr/share/hts-voice/
COPY --from=build /ttshub /usr/local/bin/ttshub
# カタログのスナップショットなどを置く場所
RUN mkdir -p /var/lib/ttshub && chown 10001:10001 /var/lib/ttshub
USER 10001
EXPOSE 8790
ENTRYPOINT ["ttshub"]
