# SnapClip — Video Downloader & Trimmer

A local web tool to download **just the part you want** from **YouTube, Facebook, TikTok**
(and any other site supported by [yt-dlp](https://github.com/yt-dlp/yt-dlp)):

paste link → real-time preview → pick the cut → the trimmed part is generated in a
single request and streamed straight to your browser. **The server keeps nothing** —
the scratch files live only for the duration of the request, then they're deleted.

Backend: **Rust** (axum) driving `yt-dlp` + `FFmpeg` as subprocesses. No Python.
Trimmed clips use yt-dlp's `--download-sections`, so only the cut region is
fetched from the platform — not the whole video.

## Run

```bash
cd snapclip/rust
cargo run --release
```

or double-click `start.bat`. Your browser opens at `http://127.0.0.1:<port>`.

- `yt-dlp` and `ffmpeg` must be on `PATH` (or point at them via `SNAPCLIP_YTDLP`
  / `SNAPCLIP_FFMPEG`).
- MP4 — Best / 1080p / 720p / 480p / 360p · trimmed clips are re-encoded for
  frame-accurate start/end (full downloads stay fast stream-copy)
- MP3 — 320 / 192 / 128 kbps, with title + thumbnail embedded

## Docker

Rust backend + FFmpeg + a standalone yt-dlp binary in one image:

```bash
docker compose up --build        # http://localhost:8000
```

No volumes needed — there is nothing to persist.

## Deploying

> **This app is stateless by design**, which makes hosting simple: any platform
> that can run the container works. Note the request duration equals the
> generation time of the clip, so very long videos need a host with generous
> request timeouts.
>
> - **Vercel** — supported via [container services](https://vercel.com/kb/guide/deploy-rust-on-vercel-with-docker)
>   (the container runs as an autoscaling Function). Best for clips; the
>   function time limit caps how long a single generation may run.
> - **Railway / Render** — "Deploy from Dockerfile", done. No time limits.
> - **Fly.io** — `fly launch` then `fly deploy`.
> - **VPS** — `docker compose up -d` behind your reverse proxy.

## Notes

- Use only for content you have the right to download — personal use, respect
  copyright and each platform's terms of service.
- Private, age-restricted, or members-only videos won't work (they'd need cookies).
- If YouTube starts failing with "Sign in to confirm…", update the extractor:
  rebuild the Docker image (it pulls the latest yt-dlp), or download a newer
  `yt-dlp` binary for local runs.

## Files

```
snapclip/
├── rust/            # axum backend — the whole server
│   ├── Cargo.toml
│   └── src/main.rs
├── Dockerfile       # rust backend + ffmpeg + yt-dlp in one image
├── docker-compose.yml
├── start.bat        # one-click local run (Windows)
├── static/index.html# The whole UI (trim slider, formats)
└── downloads/       # legacy output dir — unused since the stateless redesign
```

### Server env vars

| Var | Default | Purpose |
|---|---|---|
| `HOST` | `127.0.0.1` | bind address (`0.0.0.0` in Docker) |
| `PORT` | `8765` | listen port (`8000` in Docker) |
| `SNAPCLIP_TMP` | system temp | where request-scoped scratch dirs are created |
| `SNAPCLIP_YTDLP` | `yt-dlp` | yt-dlp binary to invoke |
| `SNAPCLIP_FFMPEG` | search `PATH` | ffmpeg binary to invoke |
| `SNAPCLIP_NO_OPEN` | unset | `1` = don't auto-open the browser |
