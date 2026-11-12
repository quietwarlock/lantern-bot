# Lantern server (no window, web page on 8080) for Linux / Unraid. Build: docker build -t lantern:1.11.1 .
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked --no-default-features --features server --bin lantern-server \
 && cp target/release/lantern-server /lantern-server

FROM debian:bookworm-slim
ARG YTDLP_URL=https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux
ARG DENO_URL=https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata curl unzip \
 && mkdir -p /usr/local/share/lantern \
 && curl -fsSL -o /usr/local/share/lantern/yt-dlp "$YTDLP_URL" \
 && chmod 755 /usr/local/share/lantern/yt-dlp \
 && curl -fsSL -o /tmp/deno.zip "$DENO_URL" \
 && unzip -q /tmp/deno.zip -d /usr/local/bin \
 && chmod 755 /usr/local/bin/deno \
 && rm /tmp/deno.zip \
 && apt-get purge -y curl unzip && apt-get autoremove -y \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /lantern-server /usr/local/bin/lantern-server
COPY docker/entrypoint.sh /usr/local/bin/lantern-entrypoint
RUN chmod 755 /usr/local/bin/lantern-entrypoint /usr/local/bin/lantern-server
# Settings, data, log, library index, and the self-updating yt-dlp all live in /config.
ENV LANTERN_DIR=/config \
    HOME=/config \
    XDG_CACHE_HOME=/config/.cache \
    DENO_DIR=/config/.cache/deno \
    TZ=Etc/UTC \
    LANTERN_WEB_PORT=8080
# The web page (1.9). Unraid's Docker tab shows a WebUI button for it.
EXPOSE 8080
LABEL net.unraid.docker.webui="http://[IP]:[PORT:8080]/"
VOLUME ["/config"]
ENTRYPOINT ["/usr/local/bin/lantern-entrypoint"]
