# syntax=docker/dockerfile:1

########## build: compile the Rust backend ##########
# pin to bookworm so the binary's glibc matches the runtime image exactly
FROM rust:1-slim-bookworm AS build
WORKDIR /build
# stub build first so dependencies are cached as their own layer
COPY rust/Cargo.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release
COPY rust/src ./src
# touch: host files keep their mtime, which can be older than the stub build
# above — cargo would then think nothing changed and keep the stub binary
RUN touch src/main.rs && cargo build --release

########## runtime: slim image with static ffmpeg + yt-dlp ##########
FROM debian:bookworm-slim
# one layer: toolchain + ffmpeg + ffprobe, nothing left over.
# ffmpeg is the GPL build (includes libx264 for trim re-encoding) with its
# shared libs kept next to it under /opt/ffmpeg — far smaller than a fully
# static build (which is ~350 MB) and than Debian's apt ffmpeg.
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl xz-utils ca-certificates binutils \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -m -u 1000 snapclip \
 && curl -fsSL https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl-shared.tar.xz -o /tmp/ff.tar.xz \
 && mkdir /tmp/ff \
 && tar -xJf /tmp/ff.tar.xz -C /tmp/ff --strip-components=1 \
 && mkdir -p /opt/ffmpeg \
 && mv /tmp/ff/bin /opt/ffmpeg/bin \
 && mv /tmp/ff/lib /opt/ffmpeg/lib \
 && strip --strip-all /opt/ffmpeg/bin/ffmpeg /opt/ffmpeg/bin/ffprobe \
 && ln -s /opt/ffmpeg/bin/ffmpeg /opt/ffmpeg/bin/ffprobe /usr/local/bin/ \
 && rm -rf /tmp/ff /tmp/ff.tar.xz
# standalone yt-dlp binary (self-contained, needs no runtime installed). If
# YouTube starts failing with "Sign in to confirm...", rebuild the image to
# pull a newer yt-dlp release.
ADD --chmod=0755 https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux /usr/local/bin/yt-dlp

ENV HOST=0.0.0.0 PORT=8000 \
    SNAPCLIP_DOWNLOADS=/data/downloads \
    SNAPCLIP_NO_OPEN=1
WORKDIR /app
COPY static /app/static
COPY --from=build /build/target/release/snapclip /usr/local/bin/snapclip
RUN mkdir -p /data/downloads && chown -R snapclip:snapclip /data /app
USER snapclip
VOLUME /data/downloads
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/snapclip"]
