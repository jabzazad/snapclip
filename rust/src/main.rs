//! SnapClip — Rust backend (axum). Drop-in replacement for `app.py`; the
//! frontend (`static/index.html`) is unchanged.
//!
//! Download engine: yt-dlp (subprocess). Trim / MP3 conversion: FFmpeg (subprocess).
//! Run:  cargo run --release   ->  http://127.0.0.1:<port>
//!
//! Env vars: HOST, PORT, SNAPCLIP_DOWNLOADS, SNAPCLIP_YTDLP, SNAPCLIP_FFMPEG,
//!           SNAPCLIP_NO_OPEN=1 (don't auto-open the browser)

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, TcpListener},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::{Path as AxPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{fs, io::AsyncBufReadExt, io::BufReader, process::Command};

const FILE_TTL: u64 = 60 * 60; // keep finished files for 1 hour, then clean up
const YTDLP_TIMEOUT: Duration = Duration::from_secs(90);

const PROG_TEMPLATE: &str = "download:PROG|%(progress.downloaded_bytes)s|%(progress.total_bytes)s\
|%(progress.total_bytes_estimate)s|%(progress._speed_str)s|%(progress._eta_str)s";

// ---------------------------------------------------------------- context ---

type Jobs = Arc<Mutex<HashMap<String, Job>>>;

#[derive(Clone)]
struct Ctx {
    jobs: Jobs,
    static_html: PathBuf,
    downloads: PathBuf,
    ytdlp: String,
    ffmpeg: Option<PathBuf>,
}

#[derive(Clone)]
struct Job {
    status: String, // running | done | error
    stage: String,
    progress: f64,
    speed: String,
    eta: String,
    error: Option<String>,
    filename: Option<String>,
    size: u64,
    created: u64,
}

// ------------------------------------------------------------------- main ---

#[tokio::main]
async fn main() {
    let host = env_or("HOST", "127.0.0.1");
    let port: u16 = match std::env::var("PORT") {
        Ok(p) => p.parse().unwrap_or(8765),
        Err(_) => free_port(8765),
    };

    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let static_html = base.join("static").join("index.html");
    let downloads = std::env::var("SNAPCLIP_DOWNLOADS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| base.join("downloads"));
    let _ = fs::create_dir_all(&downloads).await;

    let ffmpeg = find_ffmpeg();
    if ffmpeg.is_none() {
        eprintln!("WARNING: FFmpeg not found — install it or set SNAPCLIP_FFMPEG");
    }

    let ctx = Ctx {
        jobs: Arc::new(Mutex::new(HashMap::new())),
        static_html,
        downloads,
        ytdlp: std::env::var("SNAPCLIP_YTDLP").unwrap_or_else(|_| "yt-dlp".into()),
        ffmpeg,
    };

    tokio::spawn(cleanup_loop(ctx.clone()));

    let app = Router::new()
        .route("/", get(home))
        .route("/api/config", get(api_config))
        .route("/api/info", post(api_info))
        .route("/api/stream", post(api_stream))
        .route("/api/download", post(api_download))
        .route("/api/progress/{job_id}", get(api_progress))
        .route("/api/file/{job_id}", get(api_file))
        .with_state(ctx.clone());

    let addr = SocketAddr::new(host.parse().unwrap_or(IpAddr::from([127, 0, 0, 1])), port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));

    let shown = if host == "0.0.0.0" { "127.0.0.1" } else { &host };
    println!("  SnapClip running at  http://{shown}:{port}");
    maybe_open_browser(&host, port);

    axum::serve(listener, app).await.unwrap();
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

fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SNAPCLIP_FFMPEG") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
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
    c.args(extra);
    c.stdin(Stdio::null());
    c
}

// ---------------------------------------------------------------- helpers ---

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

fn clean(s: &str) -> String {
    let s = strip_ansi(s).trim().to_string();
    if matches!(s.to_lowercase().as_str(), "" | "n/a" | "unknown" | "--") {
        String::new()
    } else {
        s
    }
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

async fn err_from_file(path: &std::path::Path, default: &str) -> String {
    match fs::read_to_string(path).await {
        Ok(text) => err_from_text(&text, default),
        Err(_) => default.to_string(),
    }
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

fn update_job(ctx: &Ctx, job_id: &str, f: impl FnOnce(&mut Job)) {
    if let Some(job) = ctx.jobs.lock().unwrap().get_mut(job_id) {
        f(job);
    }
}

fn fail(ctx: &Ctx, job_id: &str, msg: String) {
    update_job(ctx, job_id, |j| {
        j.status = "error".into();
        j.stage = "Failed".into();
        j.error = Some(msg);
    });
}

// ------------------------------------------------------------------- api ---

#[derive(Deserialize)]
struct UrlIn {
    url: String,
}

async fn home(State(ctx): State<Ctx>) -> Response {
    match fs::read(&ctx.static_html).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, "index.html not found"),
    }
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
struct DownloadIn {
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

async fn api_download(State(ctx): State<Ctx>, mut req: Json<DownloadIn>) -> Response {
    if ctx.ffmpeg.is_none() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FFmpeg not found — install FFmpeg or set SNAPCLIP_FFMPEG",
        );
    }
    if req.fmt != "mp4" && req.fmt != "mp3" {
        return err(StatusCode::BAD_REQUEST, "Format must be mp4 or mp3");
    }
    if req.fmt == "mp4" && !matches!(req.quality.as_str(), "best" | "1080" | "720" | "480" | "360") {
        return err(StatusCode::BAD_REQUEST, "Invalid video quality");
    }
    if req.fmt == "mp3" && !matches!(req.quality.as_str(), "320" | "192" | "128") {
        return err(StatusCode::BAD_REQUEST, "Invalid audio bitrate");
    }
    if req.start < 0.0 {
        req.start = 0.0;
    }
    if req.duration > 0.0 {
        if req.start > req.duration {
            return err(StatusCode::BAD_REQUEST, "Start time is past the end of the video");
        }
        req.end = req.end.min(req.duration);
    }
    if req.end >= 0.0 && req.end - req.start <= 0.2 {
        return err(StatusCode::BAD_REQUEST, "Trim range is too short (minimum 0.2s)");
    }

    let job_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    ctx.jobs.lock().unwrap().insert(
        job_id.clone(),
        Job {
            status: "running".into(),
            stage: "Starting".into(),
            progress: 2.0,
            speed: String::new(),
            eta: String::new(),
            error: None,
            filename: None,
            size: 0,
            created: now_secs(),
        },
    );
    tokio::spawn(run_job(ctx.clone(), job_id.clone(), req.0));
    Json(json!({ "job_id": job_id })).into_response()
}

async fn api_progress(State(ctx): State<Ctx>, AxPath(job_id): AxPath<String>) -> Response {
    let jobs = ctx.jobs.lock().unwrap();
    match jobs.get(&job_id) {
        Some(j) => Json(json!({
            "status": j.status,
            "stage": j.stage,
            "progress": j.progress,
            "speed": j.speed,
            "eta": j.eta,
            "error": j.error,
            "filename": j.filename,
            "size": j.size,
        }))
        .into_response(),
        None => err(StatusCode::NOT_FOUND, "Unknown job"),
    }
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

async fn api_file(State(ctx): State<Ctx>, AxPath(job_id): AxPath<String>) -> Response {
    let filename = {
        let jobs = ctx.jobs.lock().unwrap();
        match jobs.get(&job_id) {
            Some(j) if j.status == "done" => match &j.filename {
                Some(f) => f.clone(),
                None => return err(StatusCode::NOT_FOUND, "File not ready"),
            },
            _ => return err(StatusCode::NOT_FOUND, "File not ready"),
        }
    };
    let path = ctx.downloads.join(&job_id).join(&filename);
    let meta = match fs::metadata(&path).await {
        Ok(m) if m.is_file() => m,
        _ => return err(StatusCode::NOT_FOUND, "File expired"),
    };
    let file = match fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return err(StatusCode::NOT_FOUND, "File expired"),
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_LENGTH, meta.len().to_string().parse().unwrap());
    headers.insert(
        header::CONTENT_TYPE,
        content_type(&filename).parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        disposition(&filename).parse().unwrap(),
    );
    (
        StatusCode::OK,
        headers,
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response()
}

// ------------------------------------------------------------- job runner ---

async fn run_job(ctx: Ctx, job_id: String, req: DownloadIn) {
    let work = ctx.downloads.join(&job_id);
    if let Err(e) = fs::create_dir_all(&work).await {
        fail(&ctx, &job_id, format!("Could not create work dir: {e}"));
        return;
    }
    if let Err(e) = download_stage(&ctx, &job_id, &work, &req).await {
        fail(&ctx, &job_id, e);
        return;
    }
    if let Err(e) = ffmpeg_stage(&ctx, &job_id, &work, &req).await {
        fail(&ctx, &job_id, e);
    }
}

async fn download_stage(ctx: &Ctx, job_id: &str, work: &std::path::Path, req: &DownloadIn) -> Result<(), String> {
    let is_mp3 = req.fmt == "mp3";
    let mut extra: Vec<String> = if is_mp3 {
        // yt-dlp converts to mp3 itself (needed for thumbnail embedding)
        [
            "-x".into(),
            "--audio-format".into(),
            "mp3".into(),
            "--audio-quality".into(),
            format!("{}K", req.quality),
            "--embed-metadata".into(),
            "--embed-thumbnail".into(),
        ]
        .to_vec()
    } else {
        let h = req.quality.as_str();
        let sel = if h == "best" {
            "bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b".to_string()
        } else {
            format!("bv*[ext=mp4][height<={h}]+ba[ext=m4a]/bv*[height<={h}]+ba/b[height<={h}]")
        };
        ["-f".into(), sel, "--merge-output-format".into(), "mp4".into()].to_vec()
    };
    extra.extend([
        "--newline".into(),
        "--progress-template".into(),
        PROG_TEMPLATE.into(),
        "-o".into(),
        work.join("src.%(ext)s").to_string_lossy().into_owned(),
        req.url.trim().to_string(),
    ]);

    let what = if is_mp3 { "audio" } else { "video" };
    update_job(ctx, job_id, |j| j.stage = format!("Downloading {what}"));

    let errlog = std::fs::File::create(work.join("ytdlp_err.log"))
        .map_err(|e| format!("Could not open error log: {e}"))?;
    let mut cmd = ytdlp(ctx, &extra);
    cmd.stdout(Stdio::piped()).stderr(Stdio::from(errlog));
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start yt-dlp: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .expect("yt-dlp stdout was piped");
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(rest) = line.strip_prefix("PROG|") else {
            continue;
        };
        let parts: Vec<&str> = rest.split('|').collect();
        let f = |i: usize| parts.get(i).and_then(|s| s.trim().parse::<f64>().ok());
        let (done, total, est) = (f(0), f(1), f(2));
        let Some(size) = total.or(est) else {
            continue;
        };
        let Some(done) = done else {
            continue;
        };
        if size <= 0.0 {
            continue;
        }
        let progress = (5.0 + 83.0 * (done / size)).min(88.0);
        let (speed, eta) = if parts.len() > 4 {
            (clean(parts[3]), clean(parts[4]))
        } else {
            (String::new(), String::new())
        };
        update_job(ctx, job_id, |j| {
            j.progress = progress;
            if !speed.is_empty() {
                j.speed = speed;
            }
            if !eta.is_empty() {
                j.eta = eta;
            }
        });
    }
    let status = child.wait().await.map_err(|e| format!("yt-dlp failed: {e}"))?;
    if !status.success() {
        return Err(err_from_file(&work.join("ytdlp_err.log"), "Download failed").await);
    }
    Ok(())
}

async fn find_src(work: &std::path::Path) -> Option<PathBuf> {
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

async fn ffmpeg_stage(ctx: &Ctx, job_id: &str, work: &std::path::Path, req: &DownloadIn) -> Result<(), String> {
    let src = find_src(work)
        .await
        .ok_or_else(|| "Downloaded file not found".to_string())?;
    let ffmpeg = ctx
        .ffmpeg
        .clone()
        .ok_or_else(|| "FFmpeg not found".to_string())?;

    let is_mp3 = req.fmt == "mp3";
    let trim = req.end >= 0.0 && req.end - req.start > 0.2;
    let span: Option<f64> = if trim {
        Some(req.end - req.start)
    } else if req.duration > 0.0 {
        Some(req.duration)
    } else {
        None
    };

    update_job(ctx, job_id, |j| {
        j.stage = if is_mp3 { "Converting to MP3".into() } else { "Trimming video".into() }
    });

    let out_name = final_name(&req.title, req.start, req.end, is_mp3);
    let out_path = work.join(&out_name);

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1"]);
    if trim {
        cmd.arg("-ss").arg(format!("{:.3}", req.start));
    }
    cmd.arg("-i").arg(&src);
    if trim {
        cmd.arg("-t").arg(format!("{:.3}", span.unwrap_or(0.0)));
    }
    if is_mp3 {
        cmd.args(["-c:a", "copy"]); // already mp3 (yt-dlp converted), lossless trim
    } else if trim {
        // re-encode for frame-accurate start/end — stream copy would snap
        // the cut to the nearest keyframe, so the clip wouldn't match the
        // selected times exactly
        cmd.args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart",
        ]);
    } else {
        cmd.args(["-c", "copy", "-movflags", "+faststart"]);
    }
    cmd.arg(&out_path);

    let errlog = std::fs::File::create(work.join("ffmpeg_err.log"))
        .map_err(|e| format!("Could not open error log: {e}"))?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::from(errlog));
    let mut child = cmd.spawn().map_err(|e| format!("Could not start ffmpeg: {e}"))?;
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Some(v) = line.strip_prefix("out_time_ms=") else {
                continue;
            };
            let (Some(t), Some(s)) = (v.trim().parse::<f64>().ok(), span) else {
                continue;
            };
            let t = t / 1e6; // microseconds
            let progress = (88.0 + 11.0 * (t / s)).min(99.0);
            update_job(ctx, job_id, |j| j.progress = progress);
        }
    }
    let status = child.wait().await.map_err(|e| format!("ffmpeg failed: {e}"))?;
    if !status.success() {
        return Err(err_from_file(&work.join("ffmpeg_err.log"), "Processing failed").await);
    }

    let size = tokio::fs::metadata(&out_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    update_job(ctx, job_id, |j| {
        j.status = "done".into();
        j.progress = 100.0;
        j.stage = "Done".into();
        j.filename = Some(out_name);
        j.size = size;
    });
    Ok(())
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
    fn clean_filters_placeholders_and_ansi() {
        assert_eq!(clean(" N/A "), "");
        assert_eq!(clean("--"), "");
        assert_eq!(clean("Unknown"), "");
        assert_eq!(clean(" 3.2MiB/s "), "3.2MiB/s");
        assert_eq!(clean("\x1b[1;32m5.0x\x1b[0m"), "5.0x");
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
    fn download_defaults_match_api_contract() {
        let r: DownloadIn = serde_json::from_str(r#"{"url":"https://x"}"#).unwrap();
        assert_eq!(r.fmt, "mp4");
        assert_eq!(r.quality, "best");
        assert_eq!(r.start, 0.0);
        assert_eq!(r.end, -1.0);
        assert_eq!(r.duration, 0.0);
        assert_eq!(r.title, "");
    }

    #[test]
    fn free_port_finds_open_port() {
        let p = free_port(30000);
        assert!((30000..30020).contains(&p));
    }
}

// --------------------------------------------------------------- cleanup ---

async fn cleanup_loop(ctx: Ctx) {
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        let now = now_secs();

        let expired: Vec<String> = {
            let mut jobs = ctx.jobs.lock().unwrap();
            let expired: Vec<String> = jobs
                .iter()
                .filter(|(_, j)| now.saturating_sub(j.created) > FILE_TTL)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &expired {
                jobs.remove(id);
            }
            expired
        };
        for id in &expired {
            let _ = fs::remove_dir_all(ctx.downloads.join(id)).await;
        }

        let Ok(mut rd) = fs::read_dir(&ctx.downloads).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let tracked = p
                .file_name()
                .map(|n| ctx.jobs.lock().unwrap().contains_key(&*n.to_string_lossy()))
                .unwrap_or(false);
            let mtime = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(now);
            if !tracked && now.saturating_sub(mtime) > FILE_TTL {
                let _ = fs::remove_dir_all(p).await;
            }
        }
    }
}
