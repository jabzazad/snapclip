# SnapClip — Video Downloader & Trimmer

A local web tool to download videos from **YouTube, Facebook, TikTok** (and any other
site supported by [yt-dlp](https://github.com/yt-dlp/yt-dlp)) — with a built-in
**trim editor**: pick a start/end time before downloading, and choose **MP4** (with
resolution) or **MP3** (with bitrate).

Backend: **Rust** (axum) driving `yt-dlp` + `FFmpeg` as subprocesses. No Python.

## Run

```bash
cd snapclip/rust
cargo run --release
```

Your browser opens at `http://127.0.0.1:<port>` automatically.

- `yt-dlp` and `ffmpeg` must be on `PATH` (or point at them via `SNAPCLIP_YTDLP`
  / `SNAPCLIP_FFMPEG`).
- Files are saved via your browser like any normal download. The server keeps them
  for **1 hour** (re-downloadable from "Recent downloads"), then cleans up.

## Docker

Rust backend + static FFmpeg + standalone yt-dlp in one image:

```bash
docker compose up --build        # http://localhost:8000
```

Finished files persist in the `snapclip-downloads` volume and are cleaned up
after 1 hour, as usual.

> **Note on Vercel:** Vercel is serverless-only — it can't run Docker containers
> or this app at all (downloads/encodes run for minutes and need a real
> filesystem). Deploy the container to any Docker host instead:
>
> - **Railway / Render** — "Deploy from Dockerfile", done.
> - **Fly.io** — `fly launch` (detects the Dockerfile) then `fly deploy`.
> - **VPS** — `docker compose up -d` behind your reverse proxy.

## Features

- Paste a link → see title, channel, duration and a video preview (YouTube embed,
  thumbnail for other sites)
- **Trim editor**: drag two handles on the timeline or type exact times (`1:30`),
  quick presets (Full / 15s / 30s / 60s) — the preview player follows the
  selection (plays from start, stops at end) on YouTube and other sites
- **MP4** — Best / 1080p / 720p / 480p / 360p · trimmed clips are re-encoded for
  frame-accurate start/end (full downloads stay fast stream-copy)
- **MP3** — 320 / 192 / 128 kbps, with title + thumbnail embedded
- Live progress bar (percent, speed, ETA)
- Works with any yt-dlp supported site

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
├── Dockerfile       # rust backend + static ffmpeg + yt-dlp in one image
├── docker-compose.yml
├── static/index.html# The whole UI (trim slider, formats, progress)
└── downloads/       # temporary job files (auto-deleted after 1h)
```

### Server env vars

| Var | Default | Purpose |
|---|---|---|
| `HOST` | `127.0.0.1` | bind address (`0.0.0.0` in Docker) |
| `PORT` | `8765` | listen port (`8000` in Docker) |
| `SNAPCLIP_DOWNLOADS` | `./downloads` | where job files are written |
| `SNAPCLIP_YTDLP` | `yt-dlp` | yt-dlp binary to invoke |
| `SNAPCLIP_FFMPEG` | search `PATH` | ffmpeg binary to invoke |
| `SNAPCLIP_NO_OPEN` | unset | `1` = don't auto-open the browser |
