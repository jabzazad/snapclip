//! SnapClip — Rust backend (axum). Stateless by design:
//! paste link → preview → pick the cut → the trimmed part is generated inside
//! one request and streamed straight to the browser. Nothing is stored — the
//! scratch dir lives only for the duration of the request, then it's deleted.
//!
//! Download engine: yt-dlp (subprocess, `--download-sections` fetches only the
//! cut region). Trim / MP3 conversion: FFmpeg (subprocess).
//! Run:  cargo run --release   ->  http://127.0.0.1:<port>
//!
//! Env vars: HOST, PORT, SNAPCLIP_TMP, SNAPCLIP_YTDLP, SNAPCLIP_FFMPEG,
//!           SNAPCLIP_COOKIES / SNAPCLIP_COOKIES_FILE (yt-dlp cookies for
//!           platforms that block cloud IPs), SNAPCLIP_PROXY (yt-dlp --proxy),
//!           SNAPCLIP_NO_OPEN=1 (don't auto-open the browser)

use std::{
    net::{IpAddr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use axum::{
    body::Body,
    extract::{Query as AxQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{fs, process::Command};
use tokio_stream::StreamExt;

const YTDLP_TIMEOUT: Duration = Duration::from_secs(90);

// scratch padding around the cut so the frame-accurate ffmpeg pass has
// material on both sides (yt-dlp's section cut is keyframe-aligned, not exact)
const SECTION_PAD: f64 = 5.0;

// ---------------------------------------------------------------- context ---

#[derive(Clone)]
struct Ctx {
    ytdlp: String,
    ffmpeg: Option<PathBuf>,
    cookies: Option<PathBuf>,
    proxy: Option<String>,
}

// the whole UI is compiled into the binary, so the server runs correctly
// from any working directory (hosts don't always start it in /app)
const INDEX_HTML: &str = include_str!("../../static/index.html");

// ------------------------------------------------------------------- main ---

#[tokio::main]
async fn main() {
    let host = env_or("HOST", "127.0.0.1");
    let port: u16 = match std::env::var("PORT") {
        Ok(p) => p.parse().unwrap_or(8765),
        Err(_) => free_port(8765),
    };

    let ffmpeg = find_ffmpeg();
    if ffmpeg.is_none() {
        eprintln!(
            "WARNING: FFmpeg not found — checked $SNAPCLIP_FFMPEG, {}, then $PATH",
            FFMPEG_CANDIDATES.join(", ")
        );
    }

    let ctx = Ctx {
        ytdlp: find_ytdlp(),
        ffmpeg,
        cookies: resolve_cookies(),
        proxy: std::env::var("SNAPCLIP_PROXY").ok().filter(|p| !p.trim().is_empty()),
    };
    println!("  yt-dlp : {}", ctx.ytdlp);
    println!(
        "  ffmpeg : {}",
        ctx.ffmpeg
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "NOT FOUND".into())
    );
    println!(
        "  cookies: {}",
        ctx.cookies
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "none".into())
    );

    let app = Router::new()
        .route("/", get(home))
        .route("/api/config", get(api_config))
        .route("/api/info", post(api_info))
        .route("/api/stream", post(api_stream))
        .route("/api/download", get(api_download))
        .with_state(ctx);

    let addr = SocketAddr::new(host.parse().unwrap_or(IpAddr::from([127, 0, 0, 1])), port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));

    let shown = if host == "0.0.0.0" { "127.0.0.1" } else { &host };
    println!("  SnapClip running at  http://{shown}:{port}");
    maybe_open_browser(&host, port);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

/// Stop serving on Ctrl+C or SIGTERM, letting in-flight downloads finish —
/// platforms that scale to zero (e.g. Vercel containers) send SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("ctrl_c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn free_port(start: u16) -> u16 {
    (start..start + 20)
        .find(|&p| TcpListener::bind(("127.0.0.1", p)).is_ok())
        .unwrap_or(start)
}

fn maybe_open_browser(host: &str, port: u16) {
    if std::env::var("SNAPCLIP_NO_OPEN").as_deref() == Ok("1") {
        return;
    }
    if host != "127.0.0.1" && host != "localhost" {
        return;
    }
    let url = format!("http://{host}:{port}");
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1200));
        open_url(&url);
    });
}

fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let res = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let res = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let res = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(any(target_os = "windows", target_os = "macos", all(unix, not(target_os = "macos"))))]
    let _ = res;
    #[cfg(not(any(target_os = "windows", target_os = "macos", all(unix, not(target_os = "macos")))))]
    let _ = url;
}

// ---------------------------------------------------------------- ffmpeg ---

/// Resolve an external binary: SNAPCLIP_* env override, then well-known
/// absolute locations (hosts may run the container with a stripped PATH),
/// then a PATH scan.
fn find_bin(env_key: &str, name: &str, extra: &[&str]) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env_key) {
        let p = PathBuf::from(&p);
        if p.is_file() {
            return Some(p);
        }
    }
    for c in extra {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(&exe))
        .find(|p| p.is_file())
}

/// Absolute locations probed before falling back to PATH — kept in one place
/// so the startup warning can print exactly what was checked.
const FFMPEG_CANDIDATES: &[&str] = &[
    "/usr/local/bin/ffmpeg",
    "/usr/bin/ffmpeg",
    "/opt/ffmpeg/bin/ffmpeg",
    "/bin/ffmpeg",
];

fn find_ffmpeg() -> Option<PathBuf> {
    find_bin("SNAPCLIP_FFMPEG", "ffmpeg", FFMPEG_CANDIDATES)
}

fn find_ytdlp() -> String {
    find_bin(
        "SNAPCLIP_YTDLP",
        "yt-dlp",
        &["/usr/local/bin/yt-dlp", "/usr/bin/yt-dlp"],
    )
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or_else(|| "yt-dlp".into())
}

/// yt-dlp cookies for platforms that block cloud IPs (YouTube's bot check):
/// `SNAPCLIP_COOKIES_FILE` points at a Netscape-format cookies file, or
/// `SNAPCLIP_COOKIES` holds the file contents and is materialized to a temp
/// file — env vars are the only practical way to ship cookies to a host like
/// Vercel.
fn resolve_cookies() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SNAPCLIP_COOKIES_FILE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
        eprintln!("WARNING: SNAPCLIP_COOKIES_FILE does not exist: {p:?}");
    }
    let raw = std::env::var("SNAPCLIP_COOKIES").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let dir = std::env::var("SNAPCLIP_TMP")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("WARNING: could not create cookie dir {dir:?}: {e}");
        return None;
    }
    let p = dir.join("snapclip-cookies.txt");
    match std::fs::write(&p, raw) {
        Ok(()) => Some(p),
        Err(e) => {
            eprintln!("WARNING: could not write cookie file to {p:?}: {e}");
            None
        }
    }
}

fn ytdlp(ctx: &Ctx, extra: &[String]) -> Command {
    let mut c = Command::new(&ctx.ytdlp);
    c.args([
        "--ignore-config",
        "--no-playlist",
        "--playlist-items",
        "1",
        "--no-warnings",
        "--color",
        "never",
        "--retries",
        "3",
    ]);
    if let Some(ff) = &ctx.ffmpeg {
        c.arg("--ffmpeg-location").arg(ff);
    }
    if let Some(ck) = &ctx.cookies {
        c.arg("--cookies").arg(ck);
    }
    if let Some(px) = &ctx.proxy {
        c.arg("--proxy").arg(px);
    }
    c.args(extra);
    c.stdin(Stdio::null());
    c
}

// ---------------------------------------------------------------- helpers ---

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn err_from_text(text: &str, default: &str) -> String {
    let stripped = strip_ansi(text);
    let line = stripped
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .unwrap_or(default);
    let line = line.strip_prefix("ERROR: ").unwrap_or(line);
    line.chars().take(300).collect()
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": msg.into() }))).into_response()
}

fn jstr(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn tname(sec: f64) -> String {
    let s = sec.round() as i64;
    let (h, m, s) = (s / 3600, s % 3600 / 60, s % 60);
    let mut out = String::new();
    if h > 0 {
        out += &format!("{h}h");
    }
    if m > 0 || h > 0 {
        out += &format!("{m}m");
    }
    out + &format!("{s}s")
}

fn hms(sec: f64) -> String {
    let s = (sec.max(0.0)).round() as u64;
    format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

fn section_range(start: f64, end: f64, duration: f64, pad: f64) -> (f64, f64) {
    let s = (start - pad).max(0.0);
    let e = if duration > 0.0 {
        end.min(duration) + pad
    } else {
        end + pad
    };
    (s, e)
}

fn final_name(title: &str, start: f64, end: f64, is_mp3: bool) -> String {
    let mut base = String::new();
    let mut last_sep = false;
    for c in title.trim().chars() {
        if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || (c as u32) < 0x20 {
            if !last_sep {
                base.push('_');
                last_sep = true;
            }
        } else {
            base.push(c);
            last_sep = false;
        }
    }
    if base.is_empty() {
        base = "video".into();
    }
    let mut base: String = base.chars().take(80).collect();
    base = base.trim_matches(['.', ' ']).to_string();
    if base.is_empty() {
        base = "video".into();
    }
    if end >= 0.0 && end - start > 0.2 {
        base += &format!(" [{}-{}]", tname(start), tname(end));
    }
    base + if is_mp3 { ".mp3" } else { ".mp4" }
}

fn scratch_dir() -> PathBuf {
    let base = std::env::var("SNAPCLIP_TMP")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join(format!("snapclip-{}", uuid::Uuid::new_v4().simple()))
}

// deleted when the streamed response ends (guard lives inside the body stream)
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let p = self.0.clone();
        tokio::spawn(async move {
            let _ = fs::remove_dir_all(p).await;
        });
    }
}

// ------------------------------------------------------------------- api ---

#[derive(Deserialize)]
struct UrlIn {
    url: String,
}

async fn home() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML).into_response()
}

async fn api_config(State(ctx): State<Ctx>) -> Response {
    Json(json!({ "ffmpeg": ctx.ffmpeg.is_some(), "name": "SnapClip" })).into_response()
}

async fn api_info(State(ctx): State<Ctx>, Json(req): Json<UrlIn>) -> Response {
    if ctx.ffmpeg.is_none() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FFmpeg not found — install FFmpeg or set SNAPCLIP_FFMPEG",
        );
    }
    let url = req.url.trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return err(
            StatusCode::BAD_REQUEST,
            "Please paste a full video link starting with http:// or https://",
        );
    }
    let mut cmd = ytdlp(&ctx, &["-J".to_string(), "--skip-download".to_string(), url.clone()]);
    let out = match tokio::time::timeout(YTDLP_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(_)) => return err(StatusCode::BAD_REQUEST, "Could not read this link"),
        Err(_) => return err(StatusCode::GATEWAY_TIMEOUT, "Fetching video info timed out"),
    };
    if !out.status.success() {
        let msg = err_from_text(&String::from_utf8_lossy(&out.stderr), "Could not read this link");
        return err(StatusCode::BAD_REQUEST, msg);
    }
    let mut data: Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "Could not parse video info"),
    };
    if data["_type"] == "playlist" {
        match data["entries"].as_array().and_then(|a| a.first()).cloned() {
            Some(e) => data = e,
            None => return err(StatusCode::BAD_REQUEST, "No videos found in this link"),
        }
    }
    if data["is_live"].as_bool().unwrap_or(false) {
        return err(StatusCode::BAD_REQUEST, "Live streams are not supported");
    }
    let duration = data["duration"].as_f64().unwrap_or(0.0);
    if duration <= 0.0 {
        return err(
            StatusCode::BAD_REQUEST,
            "Couldn't determine video duration — is this a video link?",
        );
    }

    let extractor = data
        .get("extractor_key")
        .or_else(|| data.get("extractor"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let platform = if extractor.contains("youtube") {
        "youtube"
    } else if extractor.contains("facebook") {
        "facebook"
    } else if extractor.contains("tiktok") {
        "tiktok"
    } else {
        "other"
    };

    let title = {
        let t = jstr(&data, "title");
        if t.is_empty() { "video".to_string() } else { t }
    };
    let uploader = ["uploader", "channel", "uploader_id"]
        .iter()
        .find_map(|k| {
            let s = jstr(&data, k);
            if s.is_empty() { None } else { Some(s) }
        })
        .unwrap_or_default();
    let webpage = {
        let w = jstr(&data, "webpage_url");
        if w.is_empty() { url } else { w }
    };

    Json(json!({
        "id": jstr(&data, "id"),
        "title": title,
        "uploader": uploader,
        "duration": duration,
        "thumbnail": jstr(&data, "thumbnail"),
        "webpage_url": webpage,
        "platform": platform,
    }))
    .into_response()
}

async fn api_stream(State(ctx): State<Ctx>, Json(req): Json<UrlIn>) -> Response {
    let mut cmd = ytdlp(
        &ctx,
        &[
            "-f".to_string(),
            "b[ext=mp4]/b".to_string(),
            "--get-url".to_string(),
            req.url.trim().to_string(),
        ],
    );
    let out = match tokio::time::timeout(YTDLP_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(_)) => return err(StatusCode::NOT_FOUND, "No direct preview stream available"),
        Err(_) => return err(StatusCode::GATEWAY_TIMEOUT, "Resolving preview stream timed out"),
    };
    if !out.status.success() {
        return err(StatusCode::NOT_FOUND, "No direct preview stream available");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    match text.split_whitespace().filter(|t| t.starts_with("http")).last() {
        Some(u) => Json(json!({ "url": u })).into_response(),
        None => err(StatusCode::NOT_FOUND, "No direct preview stream available"),
    }
}

#[derive(Deserialize)]
struct DownloadQuery {
    url: String,
    #[serde(default = "d_mp4")]
    fmt: String, // mp4 | mp3
    #[serde(default = "d_best")]
    quality: String, // mp4: best|1080|720|480|360   mp3: 320|192|128
    #[serde(default)]
    start: f64, // seconds
    #[serde(default = "d_end")]
    end: f64, // seconds; -1 = full video
    #[serde(default)]
    duration: f64, // video duration (seconds), from /api/info
    #[serde(default)]
    title: String, // video title, from /api/info
}

fn d_mp4() -> String {
    "mp4".into()
}
fn d_best() -> String {
    "best".into()
}
fn d_end() -> f64 {
    -1.0
}

fn validate(q: &mut DownloadQuery) -> Result<(), String> {
    q.url = q.url.trim().to_string();
    if !q.url.starts_with("http://") && !q.url.starts_with("https://") {
        return Err("Please paste a full video link starting with http:// or https://".into());
    }
    if q.fmt != "mp4" && q.fmt != "mp3" {
        return Err("Format must be mp4 or mp3".into());
    }
    if q.fmt == "mp4" && !matches!(q.quality.as_str(), "best" | "1080" | "720" | "480" | "360") {
        return Err("Invalid video quality".into());
    }
    if q.fmt == "mp3" && !matches!(q.quality.as_str(), "320" | "192" | "128") {
        return Err("Invalid audio bitrate".into());
    }
    if q.start < 0.0 {
        q.start = 0.0;
    }
    if q.duration > 0.0 {
        if q.start > q.duration {
            return Err("Start time is past the end of the video".into());
        }
        q.end = q.end.min(q.duration);
    }
    if q.end >= 0.0 && q.end - q.start <= 0.2 {
        return Err("Trim range is too short (minimum 0.2s)".into());
    }
    Ok(())
}

/// GET /api/download — generates the clip inside this request and streams it
/// straight back as a download. The scratch dir is deleted when it ends.
async fn api_download(State(ctx): State<Ctx>, AxQuery(mut q): AxQuery<DownloadQuery>) -> Response {
    if ctx.ffmpeg.is_none() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FFmpeg not found — install FFmpeg or set SNAPCLIP_FFMPEG",
        );
    }
    if let Err(msg) = validate(&mut q) {
        return err(StatusCode::BAD_REQUEST, msg);
    }

    let work = scratch_dir();
    if let Err(e) = fs::create_dir_all(&work).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not create scratch dir: {e}"),
        );
    }

    match generate(&ctx, &work, &q).await {
        Ok((out_path, out_name)) => match serve_file(work.clone(), out_path, out_name).await {
            Ok(resp) => resp,
            Err(e) => {
                let _ = fs::remove_dir_all(&work).await;
                err(StatusCode::INTERNAL_SERVER_ERROR, e)
            }
        },
        Err(e) => {
            let _ = fs::remove_dir_all(&work).await;
            err(StatusCode::BAD_REQUEST, e)
        }
    }
}

async fn generate(ctx: &Ctx, work: &Path, q: &DownloadQuery) -> Result<(PathBuf, String), String> {
    let is_mp3 = q.fmt == "mp3";
    let trim = q.end >= 0.0 && q.end - q.start > 0.2;

    let mut extra: Vec<String> = if is_mp3 {
        // yt-dlp converts to mp3 itself (needed for thumbnail embedding)
        vec![
            "-x".into(),
            "--audio-format".into(),
            "mp3".into(),
            "--audio-quality".into(),
            format!("{}K", q.quality),
            "--embed-metadata".into(),
            "--embed-thumbnail".into(),
        ]
    } else {
        let h = q.quality.as_str();
        let sel = if h == "best" {
            "bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b".to_string()
        } else {
            format!("bv*[ext=mp4][height<={h}]+ba[ext=m4a]/bv*[height<={h}]+ba/b[height<={h}]")
        };
        vec!["-f".into(), sel, "--merge-output-format".into(), "mp4".into()]
    };
    let mut sec_start = 0.0;
    if trim {
        // the section is padded because yt-dlp's own section cut is
        // keyframe/segment-aligned, not exact — the precise cut happens in
        // the ffmpeg pass below, which needs material on both sides
        let (s, e) = section_range(q.start, q.end, q.duration, SECTION_PAD);
        sec_start = s;
        extra.extend(["--download-sections".into(), format!("*{}-{}", hms(s), hms(e))]);
    }
    extra.extend([
        "-o".into(),
        work.join("src.%(ext)s").to_string_lossy().into_owned(),
        q.url.clone(),
    ]);

    let mut cmd = ytdlp(ctx, &extra);
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("Could not run yt-dlp: {e}"))?;
    if !out.status.success() {
        return Err(err_from_text(&String::from_utf8_lossy(&out.stderr), "Download failed"));
    }
    let src = find_src(work)
        .await
        .ok_or_else(|| "Downloaded file not found".to_string())?;

    let out_name = final_name(&q.title, q.start, q.end, is_mp3);
    let out_path = work.join(&out_name);

    if is_mp3 && !trim {
        // full mp3: yt-dlp already converted it — just rename into place
        fs::rename(&src, &out_path)
            .await
            .map_err(|e| format!("Could not finalize MP3: {e}"))?;
        return Ok((out_path, out_name));
    }

    let ffmpeg = ctx.ffmpeg.clone().ok_or_else(|| "FFmpeg not found".to_string())?;
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);
    if trim {
        // the section file starts at sec_start, so the requested start is
        // an offset inside it
        cmd.arg("-ss").arg(format!("{:.3}", q.start - sec_start));
        cmd.arg("-i").arg(&src);
        cmd.arg("-t").arg(format!("{:.3}", q.end - q.start));
        if is_mp3 {
            // lossless copy-cut — snaps to the nearest mp3 frame (~26ms);
            // -map 0 keeps the embedded thumbnail, id3v2.3 keeps tags
            // readable everywhere
            cmd.args(["-c", "copy", "-map", "0", "-id3v2_version", "3"]);
        } else {
            // re-encode for frame-accurate start/end — stream copy would
            // snap the cut to the nearest keyframe, so the clip wouldn't
            // match the selected times exactly
            cmd.args([
                "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart",
            ]);
        }
    } else {
        cmd.arg("-i").arg(&src);
        cmd.args(["-c", "copy", "-movflags", "+faststart"]);
    }
    cmd.arg(&out_path);

    let out2 = cmd
        .output()
        .await
        .map_err(|e| format!("Could not run ffmpeg: {e}"))?;
    if !out2.status.success() {
        return Err(err_from_text(&String::from_utf8_lossy(&out2.stderr), "Processing failed"));
    }
    Ok((out_path, out_name))
}

async fn find_src(work: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    let mut rd = fs::read_dir(work).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        let is_src = p
            .file_name()
            .map(|n| n.to_string_lossy().starts_with("src."))
            .unwrap_or(false)
            && p.is_file();
        if !is_src {
            continue;
        }
        let len = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        if best.as_ref().map_or(true, |(l, _)| len > *l) {
            best = Some((len, p));
        }
    }
    best.map(|(_, p)| p)
}

async fn serve_file(work: PathBuf, out_path: PathBuf, out_name: String) -> Result<Response, String> {
    let meta = fs::metadata(&out_path)
        .await
        .map_err(|_| "Generated file is missing".to_string())?;
    let file = fs::File::open(&out_path)
        .await
        .map_err(|_| "Generated file could not be opened".to_string())?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_LENGTH, meta.len().to_string().parse().unwrap());
    headers.insert(header::CONTENT_TYPE, content_type(&out_name).parse().unwrap());
    headers.insert(
        header::CONTENT_DISPOSITION,
        disposition(&out_name).parse().unwrap(),
    );
    // the guard moves into the stream's closure, so the scratch dir is
    // deleted exactly when the response has been fully sent
    let guard = Cleanup(work);
    let stream = tokio_util::io::ReaderStream::new(file).map(move |chunk| {
        let _ = &guard;
        chunk
    });
    Ok((StatusCode::OK, headers, Body::from_stream(stream)).into_response())
}

const FILENAME_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'.').remove(b'-').remove(b'_');

fn disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        utf8_percent_encode(name, FILENAME_SET)
    )
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tname_formats_durations() {
        assert_eq!(tname(0.0), "0s");
        assert_eq!(tname(45.0), "45s");
        assert_eq!(tname(90.0), "1m30s");
        assert_eq!(tname(3661.0), "1h1m1s");
        assert_eq!(tname(59.6), "1m0s");
    }

    #[test]
    fn final_name_sanitizes_title() {
        assert_eq!(final_name("My: Video/Title?", 0.0, -1.0, false), "My_ Video_Title_.mp4");
        assert_eq!(final_name("", 0.0, -1.0, true), "video.mp3");
        assert_eq!(final_name("a", 10.0, 40.0, false), "a [10s-40s].mp4");
        assert_eq!(final_name("x", 10.0, 10.1, true), "x.mp3"); // too short = no range
        // 100-char title is cut to 80 chars
        assert_eq!(final_name(&"a".repeat(100), 0.0, -1.0, false), "a".repeat(80) + ".mp4");
    }

    #[test]
    fn section_range_pads_and_clamps() {
        assert_eq!(section_range(0.0, 10.0, 60.0, 5.0), (0.0, 15.0));
        assert_eq!(section_range(10.0, 20.0, 60.0, 5.0), (5.0, 25.0));
        assert_eq!(section_range(58.0, 60.0, 60.0, 5.0), (53.0, 65.0)); // may pass the end
        // mp3 sections are exact — no padding
        assert_eq!(section_range(2.0, 7.0, 19.0, 0.0), (2.0, 7.0));
    }

    #[test]
    fn hms_formats_timestamps() {
        assert_eq!(hms(0.0), "0:00:00");
        assert_eq!(hms(65.0), "0:01:05");
        assert_eq!(hms(3661.0), "1:01:01");
        assert_eq!(hms(-3.0), "0:00:00");
    }

    #[test]
    fn validate_checks_input() {
        let mut q: DownloadQuery =
            serde_json::from_str(r#"{"url":"https://x","fmt":"avi"}"#).unwrap();
        assert!(validate(&mut q).is_err());
        let mut q: DownloadQuery = serde_json::from_str(r#"{"url":"ftp://x"}"#).unwrap();
        assert!(validate(&mut q).is_err());
        let mut q: DownloadQuery =
            serde_json::from_str(r#"{"url":"https://x","fmt":"mp4","quality":"2160"}"#).unwrap();
        assert!(validate(&mut q).is_err());
        let mut q: DownloadQuery = serde_json::from_str(
            r#"{"url":"https://x","fmt":"mp4","quality":"360","start":5,"end":5.1}"#,
        )
        .unwrap();
        assert!(validate(&mut q).is_err()); // trim range too short
    }

    #[test]
    fn download_defaults_match_api_contract() {
        let q: DownloadQuery = serde_json::from_str(r#"{"url":"https://x"}"#).unwrap();
        assert_eq!(q.fmt, "mp4");
        assert_eq!(q.quality, "best");
        assert_eq!(q.start, 0.0);
        assert_eq!(q.end, -1.0);
        assert_eq!(q.duration, 0.0);
        assert_eq!(q.title, "");
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\x1b[0;90m[download]\x1b[0m x"), "[download] x");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn err_from_text_takes_last_error_line() {
        assert_eq!(err_from_text("warning: x\nERROR: bad link\n", "d"), "bad link");
        assert_eq!(err_from_text("no prefix here", "d"), "no prefix here");
        assert_eq!(err_from_text("", "fallback"), "fallback");
        // long errors are truncated to 300 chars
        let long = format!("ERROR: {}", "x".repeat(500));
        assert_eq!(err_from_text(&long, "d").chars().count(), 300);
    }

    #[test]
    fn content_type_by_extension() {
        assert_eq!(content_type("clip.mp4"), "video/mp4");
        assert_eq!(content_type("song.mp3"), "audio/mpeg");
        assert_eq!(content_type("other.bin"), "application/octet-stream");
    }

    #[test]
    fn disposition_is_ascii_safe_with_utf8_fallback() {
        let d = disposition("trailer [1m30s].mp4");
        assert!(d.starts_with("attachment; filename=\"trailer [1m30s].mp4\""));
        assert!(d.contains("filename*=UTF-8''"));
        let d = disposition("naïve 测试.mp4");
        assert!(d.contains("filename=\"na_ve __.mp4\""));
        assert!(!d.contains("naïve"));
    }

    #[test]
    fn free_port_finds_open_port() {
        let p = free_port(30000);
        assert!((30000..30020).contains(&p));
    }
}
