use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::collections::{HashMap, HashSet};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tauri::{Emitter, Manager};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{watch, Mutex},
};
use uuid::Uuid;
use tract_onnx::prelude::*;

const DEFAULT_DB: &str = "postgres://shortforge:shortforge_dev_password@127.0.0.1:55434/shortforge";

#[derive(Clone)]
struct AppState {
    db: PgPool,
    media_dir: PathBuf,
    thumbnail_dir: PathBuf,
    export_dir: PathBuf,
    render_cancel: Arc<Mutex<Option<watch::Sender<bool>>>>,
}

#[derive(Serialize, sqlx::FromRow)]
struct Project {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct Probe {
    path: String,
    file_name: String,
    duration_seconds: f64,
    width: u32,
    height: u32,
    frame_rate: String,
    size_bytes: u64,
    has_audio: bool,
    category: String,
    missing: bool,
    thumbnail_path: Option<String>,
}

#[derive(Clone, Serialize, sqlx::FromRow)]
struct TimelineClip {
    id: Uuid,
    media_path: String,
    file_name: String,
    source_in_frame: i64,
    source_out_frame: i64,
    frame_rate: String,
    duration_seconds: f64,
    has_audio: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimelineClipInput {
    id: Uuid,
    media_path: String,
    source_in_frame: i64,
    source_out_frame: i64,
}

#[derive(Serialize, sqlx::FromRow)]
struct AudioAsset {
    id: Uuid,
    path: String,
    file_name: String,
    duration_seconds: f64,
    file_size_bytes: i64,
}

#[derive(Clone, Serialize, sqlx::FromRow)]
struct TimelineAudioClip {
    id: Uuid,
    audio_asset_id: Uuid,
    path: String,
    file_name: String,
    track_kind: String,
    timeline_start_seconds: f64,
    source_in_seconds: f64,
    duration_seconds: f64,
    gain: f64,
    fade_in_seconds: f64,
    fade_out_seconds: f64,
    source_duration_seconds: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimelineAudioClipInput {
    id: Uuid,
    audio_asset_id: Uuid,
    track_kind: String,
    timeline_start_seconds: f64,
    source_in_seconds: f64,
    duration_seconds: f64,
    gain: f64,
    fade_in_seconds: f64,
    fade_out_seconds: f64,
}

#[derive(Clone, Serialize, sqlx::FromRow)]
struct SceneCut {
    id: Uuid,
    time_seconds: f64,
    score: f64,
    threshold: f64,
}

#[derive(Clone, Serialize, sqlx::FromRow)]
struct AutoHighlight {
    id: Uuid,
    start_seconds: f64,
    end_seconds: f64,
    title: String,
    reason: String,
    score: f64,
    output_path: Option<String>,
    language: String,
    description: String,
    hashtags: String,
}

#[derive(Clone, Serialize, sqlx::FromRow)]
struct SilenceRange {
    id: Uuid,
    start_seconds: f64,
    end_seconds: f64,
    noise_threshold_db: f64,
    minimum_duration_seconds: f64,
}

#[derive(Clone)]
struct AudioRenderClip {
    input_index: usize,
    timeline_start_seconds: f64,
    source_in_seconds: f64,
    duration_seconds: f64,
    gain: f64,
    fade_in_seconds: f64,
    fade_out_seconds: f64,
}

#[derive(Clone)]
struct TimelineRenderClip {
    media_path: PathBuf,
    source_in_frame: i64,
    source_out_frame: i64,
    fps_numerator: u32,
    fps_denominator: u32,
    has_audio: bool,
}

fn parse_frame_rate(frame_rate: &str) -> Result<(u32, u32), String> {
    let mut parts = frame_rate.split('/');
    let numerator = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or("Media frame rate is not usable for frame-accurate editing.")?;
    let denominator = match parts.next() {
        Some(value) => value
            .parse::<u32>()
            .map_err(|_| "Media frame rate is not usable for frame-accurate editing.")?,
        None => 1,
    };
    if parts.next().is_some() || numerator == 0 || denominator == 0 {
        return Err("Media frame rate is not usable for frame-accurate editing.".into());
    }
    Ok((numerator, denominator))
}

fn timeline_frame_count(duration_seconds: f64, numerator: u32, denominator: u32) -> Option<i64> {
    let count = (duration_seconds * f64::from(numerator) / f64::from(denominator)).ceil();
    (count.is_finite() && count > 0.0 && count <= i64::MAX as f64).then_some(count as i64)
}

fn timeline_clip_duration(clip: &TimelineClip) -> Result<f64, String> {
    if clip.source_in_frame < 0 || clip.source_out_frame <= clip.source_in_frame {
        return Err("Timeline clip frame range is invalid.".into());
    }
    let (numerator, denominator) = parse_frame_rate(&clip.frame_rate)?;
    Ok(
        (clip.source_out_frame - clip.source_in_frame) as f64 * f64::from(denominator)
            / f64::from(numerator),
    )
}

fn output_dimensions(aspect_ratio: &str) -> Result<(u32, u32), String> {
    match aspect_ratio {
        "9:16" => Ok((1080, 1920)),
        "1:1" => Ok((1080, 1080)),
        "16:9" => Ok((1920, 1080)),
        _ => Err("Unsupported aspect ratio.".into()),
    }
}

fn escape_filtergraph_path(path: &str) -> String {
    let option_escaped = path.chars().fold(String::new(), |mut output, character| {
        if matches!(character, '\\' | ':' | '\'') {
            output.push('\\');
        }
        output.push(character);
        output
    });
    option_escaped
        .chars()
        .fold(String::new(), |mut output, character| {
            if matches!(character, '\\' | '\'' | ',' | '[' | ']' | ';') {
                output.push('\\');
            }
            output.push(character);
            output
        })
}

fn clip_fits_source(start: f64, duration: f64, source_duration: f64) -> bool {
    start.is_finite()
        && duration.is_finite()
        && source_duration.is_finite()
        && start >= 0.0
        && duration > 0.0
        && start + duration <= source_duration
}

fn parse_scene_cut_output(output: &str) -> Vec<(f64, f64)> {
    let mut candidates = Vec::new();
    let mut time = None;
    for line in output.lines() {
        if let Some(value) = line
            .split_once("pts_time:")
            .and_then(|(_, value)| value.split_whitespace().next())
        {
            time = value.parse::<f64>().ok().filter(|value| value.is_finite());
        }
        if let Some(value) = line
            .split_once("lavfi.scene_score=")
            .and_then(|(_, value)| value.split_whitespace().next())
        {
            if let (Some(time), Ok(score)) = (time.take(), value.parse::<f64>()) {
                if time > 0.0 && score.is_finite() && score > 0.0 && score <= 1.0 {
                    candidates.push((time, score));
                }
            }
        }
    }

    let mut cuts: Vec<(f64, f64)> = Vec::new();
    let mut cluster_start = None;
    for (time, score) in candidates {
        if cluster_start.is_some_and(|start| time - start < 0.15) {
            if let Some(last) = cuts.last_mut() {
                if score > last.1 {
                    *last = (time, score);
                }
            }
        } else {
            cuts.push((time, score));
            cluster_start = Some(time);
        }
        if cuts.len() == 2000 {
            break;
        }
    }
    cuts
}

fn parse_silence_output(
    output: &str,
    media_duration: f64,
    minimum_duration: f64,
) -> Vec<(f64, f64)> {
    let mut ranges = Vec::new();
    let mut silence_start = None;
    for line in output.lines() {
        if let Some(value) = line
            .split_once("silence_start:")
            .and_then(|(_, value)| value.split_whitespace().next())
        {
            silence_start = value
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0);
        }
        if let Some(value) = line
            .split_once("silence_end:")
            .and_then(|(_, value)| value.split_whitespace().next())
        {
            let end = value.parse::<f64>().ok().filter(|value| value.is_finite());
            if let (Some(start), Some(end)) = (silence_start.take(), end) {
                let start = start.min(media_duration);
                let end = end.min(media_duration);
                if end > start && end - start >= minimum_duration * 0.95 {
                    ranges.push((start, end));
                }
            }
        }
    }
    if let Some(start) = silence_start {
        let start = start.min(media_duration);
        if media_duration > start && media_duration - start >= minimum_duration * 0.95 {
            ranges.push((start, media_duration));
        }
    }
    ranges
}

fn audio_filter_graph(
    source: bool,
    voice_index: Option<usize>,
    music_index: Option<usize>,
    duration: f64,
) -> Option<String> {
    let mut filters = Vec::new();
    if source {
        filters.push(format!("[0:a:0]volume=1.0,atrim=duration={duration}[src]"));
    }
    if let Some(index) = voice_index {
        filters.push(format!(
            "[{index}:a:0]volume=1.0,atrim=duration={duration}[voice]"
        ));
    }
    if let Some(index) = music_index {
        filters.push(format!(
            "[{index}:a:0]volume=0.18,atrim=duration={duration}[music]"
        ));
    }
    let mut labels = Vec::new();
    if source {
        labels.push("[src]");
    }
    if voice_index.is_some() {
        labels.push("[voice]");
    }
    if music_index.is_some() {
        labels.push("[music]");
    }
    match labels.len() {
        0 => None,
        1 => Some(format!(
            "{};{}anull,loudnorm=I=-16:LRA=11:TP=-1.5[aout]",
            filters.join(";"),
            labels[0]
        )),
        count => Some(format!("{};{}amix=inputs={count}:duration=longest:dropout_transition=2,loudnorm=I=-16:LRA=11:TP=-1.5[aout]", filters.join(";"), labels.join(""))),
    }
}

fn app_data_dir() -> Result<PathBuf, String> {
    let base = dirs::data_local_dir().ok_or("Could not resolve Windows LocalAppData")?;
    Ok(base.join("ShortForge"))
}
fn ensure_safe_file(path: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(path);
    if !p.is_file() {
        return Err(format!("File does not exist: {path}"));
    }
    Ok(p)
}
async fn run_tool(program: &str, args: &[String]) -> Result<String, String> {
    let executable = resolve_tool_path(program);
    let output = tool_command(&executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| {
            format!("Could not start {}: {e}. Ensure the tool is on PATH, or set SHORTFORGE_FFMPEG_DIR / SHORTFORGE_WHISPER_DIR.", executable.display())
        })?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Starts a helper program (FFmpeg, whisper, yt-dlp, PowerShell) without opening a console window:
/// the release app runs without a console, so Windows would otherwise give each tool its own.
fn tool_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    command
}

fn resolve_tool_path(program: &str) -> PathBuf {
    let executable = format!("{program}.exe");
    let (env_dir, dev_dir) = if program.starts_with("whisper") {
        ("SHORTFORGE_WHISPER_DIR", whisper_dev_dir())
    } else if program == "yt-dlp" {
        ("SHORTFORGE_YTDLP_DIR", tools_dir().join("yt-dlp"))
    } else {
        ("SHORTFORGE_FFMPEG_DIR", tools_dir().join("ffmpeg").join("bin"))
    };
    if let Some(directory) = std::env::var_os(env_dir) {
        let candidate = PathBuf::from(directory).join(&executable);
        if candidate.is_file() {
            return candidate;
        }
    }
    let development_binary = dev_dir.join(&executable);
    if development_binary.is_file() {
        return development_binary;
    }
    PathBuf::from(program)
}

const STOPPED: &str = "Stopped.";

/// Runs a long tool step (download, transcription) that the Stop button can end.
/// It shares the single cancel slot with renders, so Stop always ends whatever is running.
async fn run_cancellable(program: &str, args: &[String], state: &AppState) -> Result<String, String> {
    let (sender, mut receiver) = watch::channel(false);
    {
        let mut active = state.render_cancel.lock().await;
        if active.is_some() {
            return Err("Another job is already running.".into());
        }
        *active = Some(sender);
    }
    let result = async {
        let executable = resolve_tool_path(program);
        let mut child = tool_command(&executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Could not start {}: {error}. Ensure the tool is on PATH, or set SHORTFORGE_FFMPEG_DIR / SHORTFORGE_WHISPER_DIR / SHORTFORGE_YTDLP_DIR.", executable.display()))?;
        let mut stdout = child.stdout.take().ok_or("Tool output is unavailable.")?;
        let mut stderr = child.stderr.take().ok_or("Tool error output is unavailable.")?;
        let out_task = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text).await;
            text
        });
        let err_task = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            text
        });
        let stop_requested = async {
            while receiver.changed().await.is_ok() {
                if *receiver.borrow() {
                    return;
                }
            }
            std::future::pending::<()>().await
        };
        let status = tokio::select! {
            status = child.wait() => status.map_err(|error| error.to_string())?,
            _ = stop_requested => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(STOPPED.to_string());
            }
        };
        let output = out_task.await.unwrap_or_default();
        let errors = err_task.await.unwrap_or_default();
        if !status.success() {
            return Err(format!("{program} failed: {errors}"));
        }
        Ok(output)
    }
    .await;
    *state.render_cancel.lock().await = None;
    result
}

fn tools_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("tools")
}

fn whisper_dev_dir() -> PathBuf {
    tools_dir().join("whisper")
}

const WEB_API_ADDRESS: &str = "127.0.0.1:1421";

#[derive(Deserialize)]
struct WebCommandRequest {
    command: String,
    #[serde(default)]
    args: serde_json::Value,
}

fn web_argument<T: DeserializeOwned>(args: &serde_json::Value, key: &str) -> Result<T, String> {
    let value = args
        .get(key)
        .cloned()
        .ok_or_else(|| format!("Missing request field: {key}"))?;
    serde_json::from_value(value).map_err(|error| format!("Invalid {key}: {error}"))
}

fn web_json<T: Serialize>(value: T) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

async fn dispatch_web_command(
    app: &tauri::AppHandle,
    request: WebCommandRequest,
) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    let args = request.args;
    match request.command.as_str() {
        "list_projects" => web_json(list_projects(app.state()).await?),
        "create_project" => {
            web_json(create_project(web_argument(&args, "name")?, app.state()).await?)
        }
        "list_project_media" => {
            web_json(list_project_media(web_argument(&args, "projectId")?, app.state()).await?)
        }
        "get_media_thumbnail" => web_json(
            get_media_thumbnail(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                app.state(),
            )
            .await?,
        ),
        "list_scene_cuts" => web_json(
            list_scene_cuts(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                app.state(),
            )
            .await?,
        ),
        "detect_scene_cuts" => web_json(
            detect_scene_cuts(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                web_argument(&args, "threshold")?,
                app.state(),
            )
            .await?,
        ),
        "list_auto_highlights" => web_json(
            list_auto_highlights(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                app.state(),
            )
            .await?,
        ),
        "delete_project" => web_json(delete_project(web_argument(&args, "projectId")?, app.state()).await?),
        "update_auto_highlight_text" => web_json(
            update_auto_highlight_text(
                web_argument(&args, "projectId")?,
                web_argument(&args, "highlightId")?,
                web_argument(&args, "title")?,
                web_argument(&args, "description")?,
                web_argument(&args, "hashtags")?,
                app.state(),
            )
            .await?,
        ),
        "is_job_running" => web_json(is_job_running(app.state()).await?),
        "delete_auto_highlight" => web_json(
            delete_auto_highlight(
                web_argument(&args, "projectId")?,
                web_argument(&args, "highlightId")?,
                app.state(),
            )
            .await?,
        ),
        "render_auto_highlight" => web_json(
            render_auto_highlight(
                web_argument(&args, "projectId")?,
                web_argument(&args, "highlightId")?,
                app.clone(),
                app.state(),
            )
            .await?,
        ),
        "find_auto_highlights" => web_json(
            find_auto_highlights(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                web_argument(&args, "maxClips")?,
                args.get("language").and_then(|value| value.as_str()).map(String::from),
                app.state(),
            )
            .await?,
        ),
        "list_silence_ranges" => web_json(
            list_silence_ranges(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                app.state(),
            )
            .await?,
        ),
        "detect_silence_ranges" => web_json(
            detect_silence_ranges(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                web_argument(&args, "noiseThresholdDb")?,
                web_argument(&args, "minimumDurationSeconds")?,
                app.state(),
            )
            .await?,
        ),
        "list_timeline" => {
            web_json(list_timeline(web_argument(&args, "projectId")?, app.state()).await?)
        }
        "save_timeline" => web_json(
            save_timeline(
                web_argument(&args, "projectId")?,
                web_argument(&args, "clips")?,
                app.state(),
            )
            .await?,
        ),
        "import_audio" => web_json(
            import_audio(
                web_argument(&args, "projectId")?,
                web_argument(&args, "path")?,
                app.state(),
            )
            .await?,
        ),
        "list_project_audio" => {
            web_json(list_project_audio(web_argument(&args, "projectId")?, app.state()).await?)
        }
        "list_timeline_audio" => {
            web_json(list_timeline_audio(web_argument(&args, "projectId")?, app.state()).await?)
        }
        "save_timeline_audio" => web_json(
            save_timeline_audio(
                web_argument(&args, "projectId")?,
                web_argument(&args, "clips")?,
                app.state(),
            )
            .await?,
        ),
        "probe_media" => web_json(probe_media(web_argument(&args, "path")?).await?),
        "import_url" => web_json(
            import_url(
                web_argument(&args, "projectId")?,
                web_argument(&args, "url")?,
                app.state(),
            )
            .await?,
        ),
        "import_media" => web_json(
            import_media(
                web_argument(&args, "projectId")?,
                web_argument(&args, "path")?,
                app.state(),
            )
            .await?,
        ),
        "set_media_category" => web_json(
            set_media_category(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                web_argument(&args, "category")?,
                app.state(),
            )
            .await?,
        ),
        "delete_media_asset" => web_json(
            delete_media_asset(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                app.state(),
            )
            .await?,
        ),
        "relink_media" => web_json(
            relink_media(
                web_argument(&args, "projectId")?,
                web_argument(&args, "mediaPath")?,
                web_argument(&args, "replacementPath")?,
                app.state(),
            )
            .await?,
        ),
        "render_timeline" => web_json(
            render_timeline(
                web_argument(&args, "projectId")?,
                web_argument(&args, "aspectRatio")?,
                web_argument(&args, "voiceoverPath")?,
                web_argument(&args, "musicPath")?,
                web_argument(&args, "subtitlePath")?,
                app.clone(),
                app.state(),
            )
            .await?,
        ),
        "render_short" => web_json(
            render_short(
                web_argument(&args, "projectId")?,
                web_argument(&args, "inputPath")?,
                web_argument(&args, "startSeconds")?,
                web_argument(&args, "durationSeconds")?,
                web_argument(&args, "aspectRatio")?,
                web_argument(&args, "voiceoverPath")?,
                web_argument(&args, "musicPath")?,
                web_argument(&args, "subtitlePath")?,
                args.get("framing").and_then(|value| value.as_str()).map(String::from),
                app.clone(),
                app.state(),
            )
            .await?,
        ),
        "cancel_render" => web_json(cancel_render(app.state()).await?),
        "generate_voiceover" => web_json(
            generate_voiceover(
                web_argument(&args, "text")?,
                web_argument(&args, "voiceName")?,
                app.state(),
            )
            .await?,
        ),
        _ => Err(format!("Unknown local command: {}", request.command)),
    }
    .map(|value| if value.is_null() { Value::Null } else { value })
}

fn decode_query_component(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .map_err(|error| error.to_string())?;
                decoded.push(u8::from_str_radix(hex, 16).map_err(|error| error.to_string())?);
                index += 2;
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8(decoded).map_err(|error| error.to_string())
}

fn query_parameter<'a>(query: &'a str, key: &str) -> Result<Option<String>, String> {
    for part in query.split('&') {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        if decode_query_component(name)? == key {
            return decode_query_component(value).map(Some);
        }
    }
    Ok(None)
}

fn cors_origin(origin: Option<&str>) -> Result<Option<&'static str>, String> {
    match origin {
        None => Ok(None),
        Some("http://127.0.0.1:1420") => Ok(Some("http://127.0.0.1:1420")),
        Some("http://localhost:1420") => Ok(Some("http://localhost:1420")),
        Some(_) => {
            Err("This local API only accepts requests from the ShortForge browser UI.".into())
        }
    }
}

async fn write_web_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    origin: Option<&str>,
    body: &[u8],
) -> Result<(), String> {
    let cors = origin
        .map(|origin| format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"))
        .unwrap_or_default();
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{cors}\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(body)
        .await
        .map_err(|error| error.to_string())
}

async fn write_web_json(
    stream: &mut TcpStream,
    status: &str,
    origin: Option<&str>,
    value: serde_json::Value,
) -> Result<(), String> {
    let body = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    write_web_response(
        stream,
        status,
        "application/json; charset=utf-8",
        origin,
        &body,
    )
    .await
}

fn parse_byte_range(header: &str, size: u64) -> Option<Option<(u64, u64)>> {
    let value = header.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(size);
        return (suffix > 0).then_some(Some((size - suffix, size - 1)));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= size {
        return Some(None);
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (end >= start).then_some(Some((start, end)))
}

async fn serve_web_media(
    reader: &mut BufReader<TcpStream>,
    app: &tauri::AppHandle,
    path: &str,
    range: Option<&str>,
    origin: Option<&str>,
    download: bool,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let media_root = fs::canonicalize(&state.media_dir)
        .await
        .map_err(|error| error.to_string())?;
    let media_path = fs::canonicalize(path)
        .await
        .map_err(|_| "Media file was not found.".to_string())?;
    let export_root = fs::canonicalize(&state.export_dir).await.ok();
    let allowed = media_path.starts_with(&media_root)
        || export_root.as_ref().is_some_and(|root| media_path.starts_with(root));
    if !allowed || !media_path.is_file() {
        return Err("Web playback is limited to the local ShortForge media and export folders.".into());
    }
    let mut file = fs::File::open(&media_path)
        .await
        .map_err(|error| error.to_string())?;
    let size = file
        .metadata()
        .await
        .map_err(|error| error.to_string())?
        .len();
    let range = match range {
        Some(value) => match parse_byte_range(value, size) {
            Some(Some(range)) => Some(range),
            _ => {
                let body = b"Invalid byte range";
                let headers = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                reader
                    .get_mut()
                    .write_all(headers.as_bytes())
                    .await
                    .map_err(|error| error.to_string())?;
                reader
                    .get_mut()
                    .write_all(body)
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
        },
        None => None,
    };
    let (start, end, status) = range
        .map(|(start, end)| (start, end, "206 Partial Content"))
        .unwrap_or((0, size.saturating_sub(1), "200 OK"));
    let content_length = if size == 0 { 0 } else { end - start + 1 };
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|error| error.to_string())?;
    let mime = match media_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    };
    let mut headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {content_length}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n"
    );
    if let Some((start, end)) = range {
        headers.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
    }
    if download {
        // Header-safe file name: ASCII letters, digits, dot, dash and underscore only.
        let name: String = media_path
            .file_name()
            .map(|name| {
                name.to_string_lossy()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() || ".-_".contains(c) { c } else { '_' })
                    .collect()
            })
            .unwrap_or_else(|| "short.mp4".into());
        headers.push_str(&format!("Content-Disposition: attachment; filename=\"{name}\"\r\n"));
    }
    if let Some(origin) = origin {
        headers.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"
        ));
    }
    headers.push_str("\r\n");
    let stream = reader.get_mut();
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    if content_length > 0 {
        let mut limited = file.take(content_length);
        tokio::io::copy(&mut limited, stream)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn remove_browser_upload(upload_root: &PathBuf, path: &PathBuf) {
    let (Ok(root), Ok(path)) = (
        fs::canonicalize(upload_root).await,
        fs::canonicalize(path).await,
    ) else {
        return;
    };
    if !path.starts_with(&root) || !path.is_file() {
        return;
    }
    if fs::remove_file(&path).await.is_ok() {
        if let Some(folder) = path.parent() {
            if folder.parent() == Some(root.as_path()) {
                let _ = fs::remove_dir(folder).await;
            }
        }
    }
}

async fn handle_web_connection(stream: TcpStream, app: tauri::AppHandle) -> Result<(), String> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .map_err(|error| error.to_string())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or("");
    let target = request_parts.next().unwrap_or("/");
    let (route, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = HashMap::new();
    let mut total_header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        let count = reader
            .read_line(&mut line)
            .await
            .map_err(|error| error.to_string())?;
        total_header_bytes += count;
        if count == 0 || total_header_bytes > 32_768 {
            return Err("HTTP headers are invalid or too large.".into());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let origin = match cors_origin(headers.get("origin").map(String::as_str)) {
        Ok(origin) => origin,
        Err(error) => {
            return write_web_json(
                reader.get_mut(),
                "403 Forbidden",
                None,
                serde_json::json!({"ok":false,"error":error}),
            )
            .await;
        }
    };
    if method == "OPTIONS" {
        let cors = origin
            .map(|origin| format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"))
            .unwrap_or_default();
        let response = format!("HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, X-File-Extension, X-File-Name\r\nAccess-Control-Max-Age: 600\r\n{cors}Content-Length: 0\r\nConnection: close\r\n\r\n");
        return reader
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .map_err(|error| error.to_string());
    }
    if method == "GET" && route == "/api/health" {
        return write_web_json(
            reader.get_mut(),
            "200 OK",
            origin,
            serde_json::json!({"ok":true,"data":{"service":"ShortForge local browser bridge"}}),
        )
        .await;
    }
    if method == "GET" && route == "/api/media" {
        let Some(path) = query_parameter(query, "path")? else {
            return write_web_json(
                reader.get_mut(),
                "400 Bad Request",
                origin,
                serde_json::json!({"ok":false,"error":"Missing media path."}),
            )
            .await;
        };
        return match serve_web_media(
            &mut reader,
            &app,
            &path,
            headers.get("range").map(String::as_str),
            origin,
            query_parameter(query, "download")?.is_some(),
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                write_web_json(
                    reader.get_mut(),
                    "403 Forbidden",
                    origin,
                    serde_json::json!({"ok":false,"error":error}),
                )
                .await
            }
        };
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if method == "POST" && route == "/api/upload" {
        if content_length == 0 || content_length > 8 * 1024 * 1024 * 1024 {
            return write_web_json(
                reader.get_mut(),
                "413 Content Too Large",
                origin,
                serde_json::json!({"ok":false,"error":"Upload must be between 1 byte and 8 GiB."}),
            )
            .await;
        }
        let kind = query_parameter(query, "kind")?.unwrap_or_default();
        let extension = headers
            .get("x-file-extension")
            .map(String::as_str)
            .unwrap_or("bin")
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if extension.is_empty()
            || extension.len() > 12
            || !extension.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return write_web_json(
                reader.get_mut(),
                "400 Bad Request",
                origin,
                serde_json::json!({"ok":false,"error":"Invalid file extension."}),
            )
            .await;
        }
        let upload_dir = app_data_dir()?.join("uploads");
        fs::create_dir_all(&upload_dir)
            .await
            .map_err(|error| error.to_string())?;
        let original_name = headers
            .get("x-file-name")
            .map(|name| decode_query_component(name))
            .transpose()?
            .unwrap_or_else(|| format!("upload.{extension}"));
        let base_name = PathBuf::from(original_name)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| "upload".into());
        let upload_folder = upload_dir.join(format!("upload-{}", Uuid::new_v4()));
        fs::create_dir_all(&upload_folder)
            .await
            .map_err(|error| error.to_string())?;
        let upload_path = upload_folder.join(base_name);
        let mut file = fs::File::create(&upload_path)
            .await
            .map_err(|error| error.to_string())?;
        let mut limited = (&mut reader).take(content_length);
        if let Err(error) = tokio::io::copy(&mut limited, &mut file).await {
            remove_browser_upload(&upload_dir, &upload_path).await;
            return Err(error.to_string());
        }
        drop(file);
        let result = match kind.as_str() {
            "video" => {
                let project_id: Uuid = query_parameter(query, "projectId")?
                    .ok_or("Missing project id.")?
                    .parse()
                    .map_err(|error| format!("Invalid project id: {error}"))?;
                let state = app.state::<AppState>();
                import_media(project_id, upload_path.to_string_lossy().to_string(), state)
                    .await
                    .and_then(web_json)
            }
            "audio" => {
                let project_id: Uuid = query_parameter(query, "projectId")?
                    .ok_or("Missing project id.")?
                    .parse()
                    .map_err(|error| format!("Invalid project id: {error}"))?;
                let state = app.state::<AppState>();
                import_audio(project_id, upload_path.to_string_lossy().to_string(), state)
                    .await
                    .and_then(web_json)
            }
            "relink" => {
                let project_id: Uuid = query_parameter(query, "projectId")?
                    .ok_or("Missing project id.")?
                    .parse()
                    .map_err(|error| format!("Invalid project id: {error}"))?;
                let media_path =
                    query_parameter(query, "mediaPath")?.ok_or("Missing media path.")?;
                let state = app.state::<AppState>();
                relink_media(
                    project_id,
                    media_path,
                    upload_path.to_string_lossy().to_string(),
                    state,
                )
                .await
                .and_then(web_json)
            }
            "aux" => Ok(serde_json::json!({"path":upload_path.to_string_lossy()})),
            _ => Err("Unsupported upload type.".into()),
        };
        if kind != "aux" || result.is_err() {
            remove_browser_upload(&upload_dir, &upload_path).await;
        }
        return match result {
            Ok(data) => {
                write_web_json(
                    reader.get_mut(),
                    "200 OK",
                    origin,
                    serde_json::json!({"ok":true,"data":data}),
                )
                .await
            }
            Err(error) => {
                write_web_json(
                    reader.get_mut(),
                    "400 Bad Request",
                    origin,
                    serde_json::json!({"ok":false,"error":error}),
                )
                .await
            }
        };
    }
    if method == "POST" && route == "/api/invoke" {
        if content_length > 16 * 1024 * 1024 {
            return write_web_json(
                reader.get_mut(),
                "413 Content Too Large",
                origin,
                serde_json::json!({"ok":false,"error":"Command payload is too large."}),
            )
            .await;
        }
        let mut body = vec![0; content_length as usize];
        reader
            .read_exact(&mut body)
            .await
            .map_err(|error| error.to_string())?;
        let request = match serde_json::from_slice::<WebCommandRequest>(&body) {
            Ok(request) => request,
            Err(error) => {
                return write_web_json(
                    reader.get_mut(),
                    "400 Bad Request",
                    origin,
                    serde_json::json!({"ok":false,"error":error.to_string()}),
                )
                .await
            }
        };
        let temporary_dir = app_data_dir()?.join("uploads");
        let temporary_paths = ["voiceoverPath", "musicPath", "subtitlePath"]
            .into_iter()
            .filter_map(|key| request.args.get(key).and_then(serde_json::Value::as_str))
            .map(PathBuf::from)
            .filter(|path| path.starts_with(&temporary_dir))
            .collect::<Vec<_>>();
        let is_render = matches!(request.command.as_str(), "render_short" | "render_timeline");
        let result = dispatch_web_command(&app, request).await;
        if is_render {
            for path in temporary_paths {
                remove_browser_upload(&temporary_dir, &path).await;
            }
        }
        return match result {
            Ok(data) => {
                write_web_json(
                    reader.get_mut(),
                    "200 OK",
                    origin,
                    serde_json::json!({"ok":true,"data":data}),
                )
                .await
            }
            Err(error) => {
                write_web_json(
                    reader.get_mut(),
                    "400 Bad Request",
                    origin,
                    serde_json::json!({"ok":false,"error":error}),
                )
                .await
            }
        };
    }
    write_web_json(
        reader.get_mut(),
        "404 Not Found",
        origin,
        serde_json::json!({"ok":false,"error":"Not found."}),
    )
    .await
}

async fn run_local_web_api(app: tauri::AppHandle) -> Result<(), String> {
    let listener = TcpListener::bind(WEB_API_ADDRESS).await.map_err(|error| {
        format!("Could not start local browser API on {WEB_API_ADDRESS}: {error}")
    })?;
    loop {
        let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(error) = handle_web_connection(stream, app).await {
                eprintln!("ShortForge local browser request failed: {error}");
            }
        });
    }
}

async fn sha256_file(path: &PathBuf) -> Result<String, String> {
    let mut file = fs::File::open(path).await.map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn generate_thumbnail(
    input: &PathBuf,
    duration_seconds: f64,
    thumbnail_dir: &PathBuf,
) -> Result<PathBuf, String> {
    fs::create_dir_all(thumbnail_dir)
        .await
        .map_err(|error| format!("Could not create the thumbnail directory: {error}"))?;
    let output = thumbnail_dir.join(format!("{}.jpg", Uuid::new_v4()));
    let seek_seconds = (duration_seconds * 0.25).clamp(0.0, (duration_seconds - 0.04).max(0.0));
    let args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-n".into(),
        "-ss".into(),
        seek_seconds.to_string(),
        "-i".into(),
        input.to_string_lossy().to_string(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        "scale=480:270:force_original_aspect_ratio=decrease,pad=480:270:(ow-iw)/2:(oh-ih)/2".into(),
        "-q:v".into(),
        "4".into(),
        output.to_string_lossy().to_string(),
    ];
    if let Err(error) = run_tool("ffmpeg", &args).await {
        let _ = fs::remove_file(&output).await;
        return Err(format!("Could not generate a media thumbnail: {error}"));
    }
    if !output.is_file() {
        return Err("FFmpeg completed without creating a thumbnail.".into());
    }
    Ok(output)
}

async fn render_ffmpeg(
    args: &[String],
    duration_seconds: f64,
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<(), String> {
    let (cancel_sender, mut cancel_receiver) = watch::channel(false);
    {
        let mut active = state.render_cancel.lock().await;
        if active.is_some() {
            return Err("A render is already running.".into());
        }
        *active = Some(cancel_sender);
    }

    let executable = resolve_tool_path("ffmpeg");
    let mut child = match tool_command(&executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            *state.render_cancel.lock().await = None;
            return Err(format!("Could not start {}: {error}", executable.display()));
        }
    };
    let Some(stdout) = child.stdout.take() else {
        *state.render_cancel.lock().await = None;
        let _ = child.start_kill();
        return Err("FFmpeg progress stream is unavailable.".into());
    };
    let Some(stderr) = child.stderr.take() else {
        *state.render_cancel.lock().await = None;
        let _ = child.start_kill();
        return Err("FFmpeg error stream is unavailable.".into());
    };
    let mut progress_lines = BufReader::new(stdout).lines();
    let stderr_reader = tokio::spawn(async move {
        let mut text = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut text).await;
        text
    });

    let mut exit_status = None;
    let mut cancelled = false;
    let mut read_error = None;
    let mut last_progress = 0.0f64;
    loop {
        tokio::select! {
            line = progress_lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(value) = line.strip_prefix("out_time_us=").and_then(|x| x.parse::<u64>().ok()) {
                            let progress = ((value as f64 / 1_000_000.0) / duration_seconds).clamp(0.0, 0.99);
                            if progress > last_progress {
                                last_progress = progress;
                                let _ = app.emit("render-progress", progress);
                            }
                        } else if line == "progress=end" {
                            let _ = app.emit("render-progress", 1.0f64);
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        read_error = Some(error.to_string());
                        let _ = child.start_kill();
                        break;
                    }
                }
            }
            status = child.wait() => {
                match status {
                    Ok(status) => exit_status = Some(status),
                    Err(error) => read_error = Some(format!("Could not wait for FFmpeg: {error}")),
                }
                break;
            }
            changed = cancel_receiver.changed() => {
                if changed.is_ok() && *cancel_receiver.borrow() {
                    cancelled = true;
                    let _ = child.start_kill();
                    exit_status = child.wait().await.ok();
                    break;
                }
            }
        }
    }
    if exit_status.is_none() {
        exit_status = child.wait().await.ok();
    }
    *state.render_cancel.lock().await = None;
    let stderr_text = stderr_reader.await.unwrap_or_default();
    if cancelled {
        return Err("Rendering cancelled.".into());
    }
    if let Some(error) = read_error {
        return Err(format!("Could not read FFmpeg progress: {error}"));
    }
    if !exit_status.is_some_and(|status| status.success()) {
        return Err(format!("FFmpeg failed: {stderr_text}"));
    }
    Ok(())
}
#[derive(Serialize, sqlx::FromRow)]
struct ProjectSummary {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    source_count: i64,
    shorts_ready: i64,
    cover_media_path: Option<String>,
}

#[tauri::command]
async fn list_projects(state: tauri::State<'_, AppState>) -> Result<Vec<ProjectSummary>, String> {
    sqlx::query_as::<_, ProjectSummary>(
        "SELECT p.id, p.name, p.created_at,
           (SELECT COUNT(*) FROM media_assets m WHERE m.project_id=p.id) AS source_count,
           (SELECT COUNT(*) FROM auto_highlights h WHERE h.project_id=p.id AND h.output_path IS NOT NULL) AS shorts_ready,
           (SELECT m.original_path FROM media_assets m WHERE m.project_id=p.id AND m.thumbnail_path IS NOT NULL ORDER BY m.original_path LIMIT 1) AS cover_media_path
         FROM projects p ORDER BY p.created_at DESC",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_project_media(
    project_id: Uuid,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<Probe>, String> {
    #[derive(sqlx::FromRow)]
    struct MediaRow {
        path: String,
        file_name: String,
        duration_seconds: f64,
        width: i32,
        height: i32,
        frame_rate: String,
        size_bytes: i64,
        has_audio: bool,
        category: String,
        thumbnail_path: Option<String>,
    }
    let rows = sqlx::query_as::<_, MediaRow>("SELECT original_path AS path,file_name,duration_seconds,width,height,frame_rate,file_size_bytes AS size_bytes,has_audio,category,thumbnail_path FROM media_assets WHERE project_id=$1 ORDER BY imported_at DESC")
        .bind(project_id).fetch_all(&state.db).await.map_err(|e| e.to_string())?;
    let media_root = fs::canonicalize(&state.media_dir).await.ok();
    let thumbnail_root = fs::canonicalize(&state.thumbnail_dir).await.ok();
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let source_path = PathBuf::from(&row.path);
        let missing = !source_path.is_file();
        let mut thumbnail_path =
            if let (Some(path), Some(root)) = (row.thumbnail_path, thumbnail_root.as_ref()) {
                match fs::canonicalize(path).await {
                    Ok(path) if path.starts_with(root) && path.is_file() => {
                        Some(path.to_string_lossy().to_string())
                    }
                    _ => None,
                }
            } else {
                None
            };
        if thumbnail_path.is_none() && !missing {
            if let (Some(root), Ok(source)) =
                (media_root.as_ref(), fs::canonicalize(&source_path).await)
            {
                if source.starts_with(root) {
                    if let Ok(thumbnail) =
                        generate_thumbnail(&source, row.duration_seconds, &state.thumbnail_dir)
                            .await
                    {
                        let thumbnail_text = thumbnail.to_string_lossy().to_string();
                        let saved = sqlx::query("UPDATE media_assets SET thumbnail_path=$1 WHERE project_id=$2 AND original_path=$3 AND (thumbnail_path IS NULL OR thumbnail_path='')")
                            .bind(&thumbnail_text).bind(project_id).bind(&row.path)
                            .execute(&state.db).await;
                        if saved.is_ok_and(|result| result.rows_affected() == 1) {
                            thumbnail_path = Some(thumbnail_text);
                        } else {
                            let _ = fs::remove_file(thumbnail).await;
                        }
                    }
                }
            }
        }
        items.push(Probe {
            path: row.path,
            file_name: row.file_name,
            duration_seconds: row.duration_seconds,
            width: row.width.max(0) as u32,
            height: row.height.max(0) as u32,
            frame_rate: row.frame_rate,
            size_bytes: row.size_bytes.max(0) as u64,
            has_audio: row.has_audio,
            category: row.category,
            missing,
            thumbnail_path,
        });
    }
    Ok(items)
}

#[tauri::command]
async fn list_scene_cuts(
    project_id: Uuid,
    media_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SceneCut>, String> {
    sqlx::query_as::<_, SceneCut>(
        "SELECT id,time_seconds,score,threshold FROM scene_cuts WHERE project_id=$1 AND media_path=$2 ORDER BY time_seconds",
    )
    .bind(project_id)
    .bind(media_path)
    .fetch_all(&state.db)
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn detect_scene_cuts(
    project_id: Uuid,
    media_path: String,
    threshold: f64,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SceneCut>, String> {
    if !threshold.is_finite() || !(0.01..=0.9).contains(&threshold) {
        return Err("Scene threshold must be between 0.01 and 0.9.".into());
    }
    let duration: Option<f64> = sqlx::query_scalar(
        "SELECT duration_seconds FROM media_assets WHERE project_id=$1 AND original_path=$2",
    )
    .bind(project_id)
    .bind(&media_path)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| error.to_string())?;
    let duration = duration.ok_or("The selected source is not in this project's media library.")?;
    let media_root = fs::canonicalize(&state.media_dir)
        .await
        .map_err(|error| error.to_string())?;
    let source = fs::canonicalize(&media_path)
        .await
        .map_err(|error| format!("Source media is unavailable: {error}"))?;
    if !source.starts_with(&media_root) || !source.is_file() {
        return Err("Scene detection only runs on media in the local project library.".into());
    }
    if !duration.is_finite() || duration <= 0.0 || duration > 3600.0 {
        return Err("Scene detection supports media up to 60 minutes long.".into());
    }
    let filter = format!("select='gt(scene,{threshold})',metadata=print:file=-");
    let args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        source.to_string_lossy().to_string(),
        "-vf".into(),
        filter,
        "-an".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ];
    let output = run_tool("ffmpeg", &args).await?;
    let cuts = parse_scene_cut_output(&output)
        .into_iter()
        .filter(|(time, score)| *time < duration && *score > threshold)
        .collect::<Vec<_>>();

    let mut transaction = state.db.begin().await.map_err(|error| error.to_string())?;
    sqlx::query("DELETE FROM scene_cuts WHERE project_id=$1 AND media_path=$2")
        .bind(project_id)
        .bind(&media_path)
        .execute(&mut *transaction)
        .await
        .map_err(|error| error.to_string())?;
    let mut results = Vec::with_capacity(cuts.len());
    for (time_seconds, score) in cuts {
        let cut = SceneCut {
            id: Uuid::new_v4(),
            time_seconds,
            score,
            threshold,
        };
        sqlx::query("INSERT INTO scene_cuts (id,project_id,media_path,time_seconds,score,threshold) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(cut.id)
            .bind(project_id)
            .bind(&media_path)
            .bind(cut.time_seconds)
            .bind(cut.score)
            .bind(cut.threshold)
            .execute(&mut *transaction)
            .await
            .map_err(|error| error.to_string())?;
        results.push(cut);
    }
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(results)
}

#[derive(Deserialize)]
struct WhisperOutput {
    transcription: Vec<WhisperSegment>,
    #[serde(default)]
    result: Option<WhisperResult>,
}
#[derive(Deserialize)]
struct WhisperResult {
    #[serde(default)]
    language: String,
}
#[derive(Deserialize)]
struct WhisperSegment {
    offsets: WhisperOffsets,
    text: String,
}
#[derive(Deserialize)]
struct WhisperOffsets {
    from: u64,
    to: u64,
}

#[derive(Debug, PartialEq)]
struct TranscriptSegment {
    start: f64,
    end: f64,
    text: String,
}

/// Parses whisper.cpp `-oj` JSON into non-empty segments with times in seconds.
fn whisper_segments(json: &str) -> Result<Vec<TranscriptSegment>, String> {
    let output: WhisperOutput =
        serde_json::from_str(json).map_err(|error| format!("Unreadable whisper output: {error}"))?;
    Ok(output
        .transcription
        .into_iter()
        .filter(|segment| segment.offsets.to > segment.offsets.from && !is_non_speech(&segment.text))
        .map(|segment| TranscriptSegment {
            start: segment.offsets.from as f64 / 1000.0,
            end: segment.offsets.to as f64 / 1000.0,
            text: segment.text.trim().to_string(),
        })
        .collect())
}

#[derive(Clone, Copy, Debug)]
struct FaceBox {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    score: f32,
}

#[derive(Debug, PartialEq)]
enum Framing {
    /// Fill the frame; the crop window sits at this fraction of its possible travel (0 = left edge, 1 = right edge).
    Crop(f64),
    /// People are too far apart for one crop: show the whole frame over a blurred fill.
    Fit,
}

type FaceModel = TypedRunnableModel<TypedModel>;
static FACE_MODEL: std::sync::OnceLock<Option<FaceModel>> = std::sync::OnceLock::new();
const FACE_W: usize = 320;
const FACE_H: usize = 240;

/// UltraFace RFB-320 (MIT), loaded once from tools/models or SHORTFORGE_FACE_MODEL.
fn face_model() -> Option<&'static FaceModel> {
    FACE_MODEL
        .get_or_init(|| {
            let path = std::env::var_os("SHORTFORGE_FACE_MODEL")
                .map(PathBuf::from)
                .unwrap_or_else(|| tools_dir().join("models").join("version-RFB-320.onnx"));
            tract_onnx::onnx()
                .model_for_path(path)
                .ok()?
                .with_input_fact(0, f32::fact([1, 3, FACE_H, FACE_W]).into())
                .ok()?
                .into_optimized()
                .ok()?
                .into_runnable()
                .ok()
        })
        .as_ref()
}

/// Faces in one 320x240 RGB frame, as boxes in 0..1 coordinates after non-maximum suppression.
fn detect_faces(model: &FaceModel, rgb: &[u8]) -> Vec<FaceBox> {
    if rgb.len() != FACE_W * FACE_H * 3 {
        return Vec::new();
    }
    let input: Tensor = tract_ndarray::Array4::from_shape_fn((1, 3, FACE_H, FACE_W), |(_, c, y, x)| {
        (rgb[(y * FACE_W + x) * 3 + c] as f32 - 127.0) / 128.0
    })
    .into();
    let Ok(outputs) = model.run(tvec!(input.into())) else {
        return Vec::new();
    };
    let views: Vec<_> = outputs.iter().filter_map(|output| output.to_array_view::<f32>().ok()).collect();
    let scores = views.iter().find(|view| view.shape().last() == Some(&2));
    let boxes = views.iter().find(|view| view.shape().last() == Some(&4));
    let (Some(scores), Some(boxes)) = (scores, boxes) else {
        return Vec::new();
    };
    let count = scores.shape()[1].min(boxes.shape()[1]);
    let mut candidates: Vec<FaceBox> = (0..count)
        .filter_map(|i| {
            let score = scores[[0, i, 1]];
            (score > 0.7).then(|| FaceBox { x1: boxes[[0, i, 0]], y1: boxes[[0, i, 1]], x2: boxes[[0, i, 2]], y2: boxes[[0, i, 3]], score })
        })
        .collect();
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<FaceBox> = Vec::new();
    for face in candidates {
        if kept.iter().all(|other| iou(&face, other) < 0.3) {
            kept.push(face);
        }
    }
    kept
}

fn iou(a: &FaceBox, b: &FaceBox) -> f32 {
    let w = (a.x2.min(b.x2) - a.x1.max(b.x1)).max(0.0);
    let h = (a.y2.min(b.y2) - a.y1.max(b.y1)).max(0.0);
    let inter = w * h;
    let union = (a.x2 - a.x1) * (a.y2 - a.y1) + (b.x2 - b.x1) * (b.y2 - b.y1) - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// Picks one steady crop for the whole clip. `window` is the crop width as a fraction of the source width.
/// Small background faces (under a third of the largest face's area) are ignored.
fn choose_framing(frames: &[Vec<FaceBox>], window: f64) -> Framing {
    if window >= 1.0 {
        return Framing::Crop(0.5);
    }
    let mut centers = Vec::new();
    let mut too_wide = 0;
    for faces in frames {
        let largest = faces.iter().map(|f| (f.x2 - f.x1) * (f.y2 - f.y1)).fold(0.0_f32, f32::max);
        let main: Vec<&FaceBox> = faces.iter().filter(|f| (f.x2 - f.x1) * (f.y2 - f.y1) >= largest / 3.0).collect();
        if main.is_empty() {
            continue;
        }
        let left = main.iter().map(|f| f.x1).fold(f32::MAX, f32::min).max(0.0) as f64;
        let right = main.iter().map(|f| f.x2).fold(f32::MIN, f32::max).min(1.0) as f64;
        if right - left > window * 0.9 {
            too_wide += 1;
        }
        centers.push((left + right) / 2.0);
    }
    if centers.is_empty() {
        return Framing::Crop(0.5);
    }
    if too_wide * 3 > centers.len() {
        return Framing::Fit;
    }
    centers.sort_by(f64::total_cmp);
    let center = centers[centers.len() / 2];
    let left = (center - window / 2.0).clamp(0.0, 1.0 - window);
    Framing::Crop(left / (1.0 - window))
}

/// One stretch of the clip between camera cuts and how to frame it.
#[derive(Debug, PartialEq)]
struct ShotPlan {
    start: f64,
    end: f64,
    framing: Framing,
    /// (seconds from clip start, crop position 0..1) for crop shots; empty for Fit.
    track: Vec<(f64, f64)>,
}

/// Horizontal extent of the faces that matter in one frame (small background faces ignored).
fn main_face_span(faces: &[FaceBox]) -> Option<(f64, f64)> {
    let largest = faces.iter().map(|f| (f.x2 - f.x1) * (f.y2 - f.y1)).fold(0.0_f32, f32::max);
    let main: Vec<&FaceBox> = faces.iter().filter(|f| (f.x2 - f.x1) * (f.y2 - f.y1) >= largest / 3.0).collect();
    (!main.is_empty()).then(|| {
        (
            main.iter().map(|f| f.x1).fold(f32::MAX, f32::min).max(0.0) as f64,
            main.iter().map(|f| f.x2).fold(f32::MIN, f32::max).min(1.0) as f64,
        )
    })
}

/// Follows the faces sample by sample: holds through frames with no face, averages over about a
/// second either side, and ignores moves under 2% of the travel so the frame does not jitter.
fn follow_track(samples: &[(f64, Vec<FaceBox>)], window: f64, base: f64) -> Vec<(f64, f64)> {
    if window >= 1.0 || samples.is_empty() {
        return vec![(samples.first().map_or(0.0, |s| s.0), base)];
    }
    let mut last = base;
    let raw: Vec<(f64, f64)> = samples
        .iter()
        .map(|(time, faces)| {
            if let Some((left, right)) = main_face_span(faces) {
                if right - left <= window * 0.9 {
                    let start = ((left + right) / 2.0 - window / 2.0).clamp(0.0, 1.0 - window);
                    last = start / (1.0 - window);
                }
            }
            (*time, last)
        })
        .collect();
    let mut track: Vec<(f64, f64)> = Vec::with_capacity(raw.len());
    for index in 0..raw.len() {
        let from = index.saturating_sub(2);
        let to = (index + 2).min(raw.len() - 1);
        let average = raw[from..=to].iter().map(|point| point.1).sum::<f64>() / (to - from + 1) as f64;
        let position = match track.last() {
            Some(&(_, previous)) if (average - previous).abs() < 0.02 => previous,
            _ => average,
        };
        track.push((raw[index].0, position));
    }
    track
}

/// Crop position at time `t`, linearly between track points.
fn track_position(track: &[(f64, f64)], t: f64) -> f64 {
    match track.iter().position(|point| point.0 > t) {
        None => track.last().map_or(0.5, |point| point.1),
        Some(0) => track[0].1,
        Some(next) => {
            let (t0, p0) = track[next - 1];
            let (t1, p1) = track[next];
            p0 + (p1 - p0) * ((t - t0) / (t1 - t0).max(1e-6))
        }
    }
}

/// Splits the clip at its camera cuts and frames every shot on its own faces.
/// `samples` are (seconds from clip start, faces). Neighbouring shots with the same framing are merged.
fn plan_shots(cuts: &[f64], duration: f64, samples: &[(f64, Vec<FaceBox>)], window: f64) -> Vec<ShotPlan> {
    // A jump in where the main faces are also marks a new shot (soft cuts and split screens fool scene detection).
    let layout_cuts = samples.windows(2).filter_map(|pair| {
        let (before, after) = (main_face_span(&pair[0].1)?, main_face_span(&pair[1].1)?);
        let moved = ((before.0 + before.1) - (after.0 + after.1)).abs() / 2.0 > 0.15;
        let resized = ((before.1 - before.0) - (after.1 - after.0)).abs() > 0.15;
        (moved || resized).then(|| (pair[0].0 + pair[1].0) / 2.0)
    });
    // Real camera cuts are exact; a face-layout change only adds a cut where no camera cut is near.
    let layout_cuts: Vec<f64> = layout_cuts.filter(|t| cuts.iter().all(|cut| (cut - t).abs() > 0.6)).collect();
    let mut bounds: Vec<f64> = cuts.iter().copied().chain(layout_cuts).filter(|t| *t > 0.3 && *t < duration - 0.3).collect();
    bounds.sort_by(f64::total_cmp);
    bounds.dedup_by(|a, b| (*a - *b).abs() < 0.5);
    let mut edges = vec![0.0];
    edges.extend(bounds);
    edges.push(duration);
    let mut plans: Vec<ShotPlan> = Vec::new();
    for pair in edges.windows(2) {
        let (shot_start, shot_end) = (pair[0], pair[1]);
        let shot_samples: Vec<(f64, Vec<FaceBox>)> = samples
            .iter()
            .filter(|(time, _)| *time >= shot_start && *time < shot_end)
            .cloned()
            .collect();
        let frames: Vec<Vec<FaceBox>> = shot_samples.iter().map(|(_, faces)| faces.clone()).collect();
        let framing = if frames.is_empty() {
            // Too short to sample: keep the previous shot's framing so the picture does not jump.
            plans.last().map_or(Framing::Crop(0.5), |last| match last.framing {
                Framing::Crop(p) => Framing::Crop(p),
                Framing::Fit => Framing::Fit,
            })
        } else {
            choose_framing(&frames, window)
        };
        let same_as_last = plans.last().is_some_and(|last| match (&last.framing, &framing) {
            (Framing::Fit, Framing::Fit) => true,
            (Framing::Crop(a), Framing::Crop(b)) => (a - b).abs() < 0.04,
            _ => false,
        });
        let track = match framing {
            Framing::Crop(base) if shot_samples.is_empty() => vec![(shot_start, base)],
            Framing::Crop(base) => follow_track(&shot_samples, window, base),
            Framing::Fit => Vec::new(),
        };
        if same_as_last {
            if let Some(last) = plans.last_mut() {
                last.end = shot_end;
                last.track.extend(track);
            }
        } else {
            plans.push(ShotPlan { start: shot_start, end: shot_end, framing, track });
        }
    }
    plans
}

/// FFmpeg filter for a shot plan, plus the sendcmd script that moves the crop.
/// The crop follows each shot's face track 10 times a second; Fit shots show the whole frame
/// over a blurred fill. `source` is the picture size after black bars are removed.
fn shot_filter(plans: &[ShotPlan], bars: &str, w: u32, h: u32, source: (f64, f64), commands_path: &str) -> (String, String) {
    let ratio = w as f64 / h as f64;
    let (source_w, source_h) = source;
    let crop_w = ((source_h * ratio).min(source_w) / 2.0).floor() * 2.0;
    let crop_h = ((source_w / ratio).min(source_h) / 2.0).floor() * 2.0;
    let travel = (source_w - crop_w).max(0.0);
    let top = ((source_h - crop_h) / 2.0).floor();
    let mut commands = String::new();
    for plan in plans.iter().filter(|plan| !plan.track.is_empty()) {
        let mut t = plan.start;
        while t < plan.end {
            let x = (track_position(&plan.track, t) * travel).round();
            commands.push_str(&format!("{t:.2} crop@track x {x};\n"));
            t += 0.1;
        }
    }
    let first_x = plans
        .iter()
        .find(|plan| !plan.track.is_empty())
        .map_or(travel / 2.0, |plan| (track_position(&plan.track, plan.start) * travel).round());
    let script = escape_filtergraph_path(commands_path);
    let follow = if commands.is_empty() { String::new() } else { format!("sendcmd=f={script},") };
    let crop = format!("{follow}crop@track=w={crop_w}:h={crop_h}:x={first_x}:y={top},scale={w}:{h},setsar=1");
    let fit_spans: Vec<String> = plans
        .iter()
        .filter(|plan| plan.framing == Framing::Fit)
        .map(|plan| format!("between(t\\,{:.3}\\,{:.3})", plan.start, plan.end))
        .collect();
    let fit_chain = format!("[bgsrc]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},boxblur=24:2,eq=brightness=-0.12[bg];[fgsrc]scale={w}:{h}:force_original_aspect_ratio=decrease[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1");
    let graph = if fit_spans.is_empty() {
        format!("{bars}{crop},fps=30")
    } else if fit_spans.len() == plans.len() {
        format!("{bars}split=2[bgsrc][fgsrc];{fit_chain},fps=30")
    } else {
        format!("{bars}split=3[cropsrc][bgsrc][fgsrc];[cropsrc]{crop}[filled];{fit_chain}[fit];[filled][fit]overlay=0:0:enable='{}',fps=30", fit_spans.join("+"))
    };
    (graph, commands)
}

/// Mean brightness change between two frames inside a box given in 0..1 coordinates.
fn region_change(previous: &[u8], current: &[u8], x1: f32, y1: f32, x2: f32, y2: f32) -> f64 {
    let to_px = |v: f32, max: usize| ((v.clamp(0.0, 1.0) * max as f32) as usize).min(max - 1);
    let (left, right) = (to_px(x1, FACE_W), to_px(x2, FACE_W));
    let (top, bottom) = (to_px(y1, FACE_H), to_px(y2, FACE_H));
    if right <= left || bottom <= top {
        return 0.0;
    }
    let mut total = 0.0;
    for y in top..bottom {
        for x in left..right {
            let index = (y * FACE_W + x) * 3 + 1; // green channel carries most of the brightness
            total += (current[index] as f64 - previous[index] as f64).abs();
        }
    }
    total / ((right - left) * (bottom - top)) as f64
}

/// How much a face is talking between two frames: mouth movement minus eye-area movement,
/// so a head that turns or nods (both regions move) does not count as speech.
fn mouth_activity(previous: &[u8], current: &[u8], face: &FaceBox) -> f64 {
    let (w, h) = (face.x2 - face.x1, face.y2 - face.y1);
    let mouth = region_change(previous, current, face.x1 + 0.2 * w, face.y1 + 0.62 * h, face.x2 - 0.2 * w, face.y2 + 0.05 * h);
    let eyes = region_change(previous, current, face.x1 + 0.15 * w, face.y1 + 0.2 * h, face.x2 - 0.15 * w, face.y1 + 0.5 * h);
    (mouth - eyes).max(0.0)
}

/// Follows whoever is talking in a shot where people are too far apart for one crop.
/// `frames` are (seconds, 320x240 RGB, faces) a few times a second. People are told apart by where
/// they stand; each second the face with clearly the most mouth movement is the speaker, and a new
/// speaker must win two seconds in a row before the camera moves to them. None if no one is ever clear.
fn speaker_track(frames: &[(f64, Vec<u8>, Vec<FaceBox>)], window: f64) -> Option<Vec<(f64, f64)>> {
    let mut people: Vec<f64> = Vec::new(); // horizontal centre of each person
    let mut activity: Vec<(f64, usize, f64)> = Vec::new(); // (time, person, activity)
    for pair in frames.windows(2) {
        let (_, previous, _) = &pair[0];
        let (time, current, faces) = &pair[1];
        for face in faces {
            let centre = ((face.x1 + face.x2) / 2.0) as f64;
            let person = match people.iter().position(|p| (p - centre).abs() < 0.12) {
                Some(index) => index,
                None => {
                    people.push(centre);
                    people.len() - 1
                }
            };
            activity.push((*time, person, mouth_activity(previous, current, face)));
        }
    }
    if people.len() < 2 || activity.is_empty() {
        return None;
    }
    let start = frames.first()?.0;
    let end = frames.last()?.0;
    let position_of = |person: usize| ((people[person] - window / 2.0).clamp(0.0, 1.0 - window)) / (1.0 - window);
    let mut track: Vec<(f64, f64)> = Vec::new();
    let mut speaker: Option<usize> = None;
    let mut challenger: Option<usize> = None;
    let mut second = start;
    while second < end {
        let mut totals = vec![0.0; people.len()];
        for (time, person, amount) in &activity {
            if *time >= second && *time < second + 1.0 {
                totals[*person] += amount;
            }
        }
        let mut order: Vec<usize> = (0..people.len()).collect();
        order.sort_by(|a, b| totals[*b].total_cmp(&totals[*a]));
        let (best, runner_up) = (order[0], order[1]);
        let clear = totals[best] > 1.5 && totals[best] > 1.4 * totals[runner_up];
        if clear {
            match speaker {
                None => speaker = Some(best),
                Some(current) if current != best => {
                    if challenger == Some(best) {
                        speaker = Some(best);
                        challenger = None;
                        // Cut, do not pan: the previous framing holds until the switch.
                        if let Some(&(_, held)) = track.last() {
                            track.push((second - 0.05, held));
                        }
                    } else {
                        challenger = Some(best);
                    }
                }
                _ => challenger = None,
            }
        }
        if let Some(current) = speaker {
            track.push((second, position_of(current)));
        }
        second += 1.0;
    }
    if track.is_empty() {
        return None;
    }
    // Before the first clear speaker, frame them from the start of the shot.
    if track[0].0 > start {
        let first = track[0].1;
        track.insert(0, (start, first));
    }
    Some(track)
}

/// Samples the clip twice a second, finds the camera cuts, and plans the framing shot by shot.
async fn smart_framing(input: &std::path::Path, start: f64, duration: f64, bars: &str, window: f64) -> Vec<ShotPlan> {
    let whole_clip_fit = || vec![ShotPlan { start: 0.0, end: duration, framing: Framing::Fit, track: Vec::new() }];
    let Some(model) = face_model() else {
        return whole_clip_fit();
    };
    let fps = (60.0 / duration.max(1.0)).clamp(0.5, 2.0);
    let source = input.to_string_lossy().to_string();
    let frames = tool_command(resolve_tool_path("ffmpeg"))
        .args([
            "-hide_banner".to_string(), "-loglevel".into(), "error".into(),
            "-ss".into(), format!("{start:.3}"), "-t".into(), format!("{duration:.3}"),
            "-i".into(), source.clone(),
            "-vf".into(), format!("{bars}fps={fps:.3},scale={FACE_W}:{FACE_H}"),
            "-f".into(), "rawvideo".into(), "-pix_fmt".into(), "rgb24".into(), "-".into(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    let Ok(frames) = frames else {
        return whole_clip_fit();
    };
    let samples: Vec<(f64, Vec<FaceBox>)> = frames
        .stdout
        .chunks_exact(FACE_W * FACE_H * 3)
        .enumerate()
        .map(|(index, frame)| (index as f64 / fps, detect_faces(model, frame)))
        .collect();
    let cuts = tool_command(resolve_tool_path("ffmpeg"))
        .args([
            "-hide_banner".to_string(), "-loglevel".into(), "error".into(),
            "-ss".into(), format!("{start:.3}"), "-t".into(), format!("{duration:.3}"),
            "-i".into(), source,
            "-vf".into(), format!("{bars}scale=320:-2,select='gt(scene,0.3)',metadata=print:file=-"),
            "-an".into(), "-f".into(), "null".into(), "-".into(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map(|output| parse_scene_cut_output(&String::from_utf8_lossy(&output.stdout)).into_iter().map(|(time, _)| time).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut plans = plan_shots(&cuts, duration, &samples, window);
    for plan in plans.iter_mut().filter(|plan| plan.framing == Framing::Fit && plan.end - plan.start >= 2.0) {
        let length = plan.end - plan.start;
        let fps = (90.0 / length).clamp(3.0, 6.0);
        let Ok(output) = tool_command(resolve_tool_path("ffmpeg"))
            .args([
                "-hide_banner".to_string(), "-loglevel".into(), "error".into(),
                "-ss".into(), format!("{:.3}", start + plan.start), "-t".into(), format!("{length:.3}"),
                "-i".into(), input.to_string_lossy().to_string(),
                "-vf".into(), format!("{bars}fps={fps:.3},scale={FACE_W}:{FACE_H}"),
                "-f".into(), "rawvideo".into(), "-pix_fmt".into(), "rgb24".into(), "-".into(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
        else {
            continue;
        };
        let frames: Vec<(f64, Vec<u8>, Vec<FaceBox>)> = output
            .stdout
            .chunks_exact(FACE_W * FACE_H * 3)
            .enumerate()
            .map(|(index, frame)| (plan.start + index as f64 / fps, frame.to_vec(), detect_faces(model, frame)))
            .collect();
        if let Some(track) = speaker_track(&frames, window) {
            plan.framing = Framing::Crop(track[0].1);
            plan.track = track;
        }
    }
    plans
}

/// Reads the last `crop=W:H:X:Y` that FFmpeg's cropdetect printed; ignores tiny or malformed values.
fn parse_cropdetect(stderr: &str) -> Option<String> {
    let value = stderr.rsplit("crop=").next().filter(|_| stderr.contains("crop="))?;
    let numbers: Vec<u32> = value
        .split_whitespace()
        .next()?
        .split(':')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<_>>>()?;
    match numbers[..] {
        [w, h, x, y] if w >= 64 && h >= 64 => Some(format!("crop={w}:{h}:{x}:{y}")),
        _ => None,
    }
}

/// Samples four seconds from the middle of the clip and returns a crop that removes black bars, if any.
async fn detect_black_bars(input: &std::path::Path, start: f64, duration: f64) -> Option<String> {
    let sample_start = start + (duration / 2.0 - 2.0).max(0.0);
    let output = tool_command(resolve_tool_path("ffmpeg"))
        .args([
            "-hide_banner".to_string(), "-ss".into(), format!("{sample_start:.3}"), "-t".into(), "4".into(),
            "-i".into(), input.to_string_lossy().to_string(),
            "-vf".into(), "cropdetect=limit=24:round=2:reset=0".into(),
            "-an".into(), "-f".into(), "null".into(), "-".into(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;
    parse_cropdetect(&String::from_utf8_lossy(&output.stderr))
}

fn whisper_language(json: &str) -> String {
    serde_json::from_str::<WhisperOutput>(json)
        .ok()
        .and_then(|output| output.result)
        .map(|result| result.language)
        .unwrap_or_default()
}

/// Cuts text at a word boundary so it fits `limit` characters, adding an ellipsis when shortened.
fn clip_text(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let cut: String = text.chars().take(limit).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end_matches([',', ';', ':', ' ']))
}

/// The clip's opening sentence is its hook, so it becomes the title.
fn hook_title(opener: &str) -> String {
    let opener = opener.trim().trim_start_matches(['-', ' ']);
    let end = opener.find(['.', '!', '?', '।']).map_or(opener.len(), |index| index + opener[index..].chars().next().map_or(1, char::len_utf8));
    clip_text(&opener[..end], 70)
}

/// The first lines of the clip, ready to paste as a post caption.
fn social_description(text: &str) -> String {
    clip_text(text, 220)
}

const STOPWORDS: &[&str] = &[
    "the", "and", "that", "this", "with", "your", "for", "are", "was", "have", "not", "but", "they",
    "what", "when", "just", "from", "like", "about", "there", "their", "will", "would", "could",
    "them", "then", "than", "been", "were", "into", "more", "some", "here", "very", "really", "know",
    "think", "going", "dont", "it's", "i'm", "that's", "yeah", "okay", "because", "which", "also",
    "only", "even", "much", "make", "made", "want", "need", "said", "says", "thing", "things",
    "people", "you're", "we're", "they're", "can't", "don't", "didn't", "doesn't", "this", "where",
    "while", "these", "those", "should", "every", "other", "again", "still", "right", "well",
    "everything", "something", "nothing", "anything", "everyone", "someone", "today", "actually",
];

fn language_hashtag(code: &str) -> Option<&'static str> {
    Some(match code {
        "en" => "english", "ta" => "tamil", "hi" => "hindi", "te" => "telugu", "ml" => "malayalam",
        "kn" => "kannada", "bn" => "bengali", "mr" => "marathi", "ur" => "urdu", "es" => "spanish",
        "fr" => "french", "de" => "german", "pt" => "portuguese", "it" => "italian", "ar" => "arabic",
        "ja" => "japanese", "ko" => "korean", "zh" => "chinese", "ru" => "russian", "id" => "indonesian",
        _ => return None,
    })
}

/// Hashtags from the clip's own most-repeated words, its language and the short-video platform tags.
/// shortcut: frequency only, no live trend data (the app is offline); swap in a local language model to rewrite them.
fn social_hashtags(text: &str, language: &str) -> String {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for word in text.split_whitespace() {
        let word = word
            .trim_matches(|c: char| c.is_ascii_punctuation() || "“”‘’…।!?¿¡".contains(c))
            .to_lowercase();
        if word.chars().count() < 4
            || word.chars().any(|c| c.is_ascii_punctuation())
            || word.chars().all(|c| c.is_ascii_digit())
            || STOPWORDS.contains(&word.as_str())
        {
            continue;
        }
        *counts.entry(word).or_default() += 1;
    }
    let mut words: Vec<(String, usize)> = counts.into_iter().collect();
    words.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.chars().count().cmp(&a.0.chars().count())).then(a.0.cmp(&b.0)));
    let mut tags: Vec<String> = words.into_iter().take(4).map(|(word, _)| format!("#{word}")).collect();
    if let Some(tag) = language_hashtag(language) {
        tags.push(format!("#{tag}"));
    }
    for tag in ["#shorts", "#reels", "#fyp"] {
        if !tags.iter().any(|existing| existing == tag) {
            tags.push(tag.to_string());
        }
    }
    tags.join(" ")
}

fn is_non_speech(text: &str) -> bool {
    let text = text.trim().trim_start_matches(['-', ' ']).trim();
    text.is_empty()
        || (text.starts_with('[') && text.ends_with(']'))
        || (text.starts_with('(') && text.ends_with(')'))
        || (text.starts_with('*') && text.ends_with('*'))
        || text.chars().all(|c| !c.is_alphanumeric())
        || is_repetition_loop(text)
}

/// Whisper sometimes "hallucinates" a loop of one character or word; such lines are not speech.
fn is_repetition_loop(text: &str) -> bool {
    let letters: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if letters.len() < 12 {
        return false;
    }
    let distinct: std::collections::HashSet<char> = letters.iter().copied().collect();
    let words: Vec<&str> = text.split_whitespace().collect();
    let distinct_words: std::collections::HashSet<&str> = words.iter().copied().collect();
    distinct.len() * 5 < letters.len().min(60) || (words.len() >= 6 && distinct_words.len() * 4 < words.len())
}

const HOOK_WORDS: &[&str] = &[
    "how", "why", "what", "secret", "never", "always", "mistake", "truth", "best", "worst",
    "stop", "nobody", "everyone", "actually", "crazy", "insane", "money", "free", "wrong",
];

fn ends_sentence(text: &str) -> bool {
    text.trim_end().ends_with(['.', '!', '?'])
}

/// Free, local clip picker: scores every transcript window of 15-60 s by speaking pace,
/// a hook in the opening line, energy (! and ?) and a clean sentence ending, then keeps
/// the best non-overlapping windows.
/// shortcut: keyword/pace heuristic, not semantic understanding; swap in a local LLM if picks feel off.
/// YouTube's "Most replayed" graph as (start s, end s, value 0..1), from yt-dlp's heatmap JSON.
/// Videos without enough views have none ("NA").
fn parse_heatmap(json: &str) -> Vec<(f64, f64, f64)> {
    #[derive(Deserialize)]
    struct Point {
        start_time: f64,
        end_time: f64,
        value: f64,
    }
    serde_json::from_str::<Vec<Point>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.start_time.is_finite() && p.end_time > p.start_time && p.value.is_finite())
        .map(|p| (p.start_time, p.end_time, p.value.clamp(0.0, 1.0)))
        .collect()
}

/// How much more a stretch is replayed than the video's average: 0 (average or less) to 1 (the peak).
fn replay_lift(replay: &[(f64, f64, f64)], start: f64, end: f64) -> f64 {
    if replay.is_empty() {
        return 0.0;
    }
    let mean = replay.iter().map(|p| p.2).sum::<f64>() / replay.len() as f64;
    let peak = replay.iter().map(|p| p.2).fold(0.0, f64::max);
    let (mut weighted, mut covered) = (0.0, 0.0);
    for (a, b, value) in replay {
        let overlap = (b.min(end) - a.max(start)).max(0.0);
        weighted += overlap * value;
        covered += overlap;
    }
    if covered <= 0.0 || peak <= mean {
        return 0.0;
    }
    ((weighted / covered - mean) / (peak - mean)).clamp(0.0, 1.0)
}

/// Momentary loudness (EBU R128, LUFS) over time from FFmpeg's ebur128 metadata output.
fn parse_loudness(output: &str) -> Vec<(f64, f64)> {
    let mut points = Vec::new();
    let mut time = None;
    for line in output.lines() {
        if let Some(value) = line.split_once("pts_time:").and_then(|(_, v)| v.split_whitespace().next()) {
            time = value.parse::<f64>().ok();
        }
        if let Some(value) = line.split_once("lavfi.r128.M=").map(|(_, v)| v.trim()) {
            if let (Some(t), Ok(lufs)) = (time.take(), value.parse::<f64>()) {
                if lufs.is_finite() && lufs > -70.0 {
                    points.push((t, lufs));
                }
            }
        }
    }
    points
}

/// How much louder a stretch is than the video's usual level: 0 (typical or quieter) to 1 (6 LU or more above).
fn loudness_lift(loudness: &[(f64, f64)], median: f64, start: f64, end: f64) -> f64 {
    let inside: Vec<f64> = loudness.iter().filter(|(t, _)| *t >= start && *t < end).map(|(_, l)| *l).collect();
    if inside.is_empty() {
        return 0.0;
    }
    let mean = inside.iter().sum::<f64>() / inside.len() as f64;
    ((mean - median) / 6.0).clamp(0.0, 1.0)
}

fn same_line(a: &str, b: &str) -> bool {
    let normal = |text: &str| text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let (a, b) = (normal(a), normal(b));
    !a.is_empty() && a == b
}

/// Keeps the best-scoring windows that do not overlap or repeat an opening line.
fn select_top(mut candidates: Vec<AutoHighlight>, count: usize) -> Vec<AutoHighlight> {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.start_seconds.total_cmp(&b.start_seconds)));
    let mut picked: Vec<AutoHighlight> = Vec::new();
    for candidate in candidates {
        if picked.len() >= count {
            break;
        }
        let overlaps = picked.iter().any(|clip| candidate.end_seconds > clip.start_seconds && candidate.start_seconds < clip.end_seconds);
        // Two clips that open on the same line are the same moment told twice: keep the better one.
        let repeats = picked.iter().any(|clip| same_line(&clip.title, &candidate.title));
        if !overlaps && !repeats {
            picked.push(candidate);
        }
    }
    picked
}

fn pick_highlights(segments: &[TranscriptSegment], duration: f64, max_clips: usize, language: &str, loudness: &[(f64, f64)], replay: &[(f64, f64, f64)]) -> Vec<AutoHighlight> {
    let median_loudness = {
        let mut levels: Vec<f64> = loudness.iter().map(|(_, l)| *l).collect();
        levels.sort_by(f64::total_cmp);
        levels.get(levels.len() / 2).copied().unwrap_or(-23.0)
    };
    const MIN_SECONDS: f64 = 15.0;
    const TARGET_SECONDS: f64 = 35.0;
    const MAX_SECONDS: f64 = 60.0;
    let mut candidates = Vec::new();
    for first in 0..segments.len() {
        let start = segments[first].start;
        let mut last = None;
        for (index, segment) in segments.iter().enumerate().skip(first) {
            let length = segment.end - start;
            if length > MAX_SECONDS {
                break;
            }
            if length >= MIN_SECONDS {
                last = Some(index);
                if length >= TARGET_SECONDS && ends_sentence(&segment.text) {
                    break;
                }
            }
        }
        let Some(last) = last else { continue };
        let window = &segments[first..=last];
        let end = window[window.len() - 1].end.min(duration);
        if end - start < MIN_SECONDS {
            continue;
        }
        let text = window.iter().map(|segment| segment.text.as_str()).collect::<Vec<_>>().join(" ");
        let words_per_second = text.split_whitespace().count() as f64 / (end - start);
        let opener = window[0].text.to_lowercase();
        let hook = opener.contains('?')
            || opener
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| HOOK_WORDS.contains(&word));
        let energy = (text.matches(['!', '?']).count() as f64 / 3.0).min(1.0);
        let complete = ends_sentence(&window[window.len() - 1].text);
        let loud = loudness_lift(loudness, median_loudness, start, end);
        // What viewers actually rewatch is the strongest signal when YouTube has it.
        let replayed = replay_lift(replay, start, end);
        let score = 0.35 * (words_per_second / 3.0).min(1.0)
            + if hook { 0.25 } else { 0.0 }
            + 0.1 * energy
            + if complete { 0.1 } else { 0.0 }
            + 0.2 * loud
            + 0.35 * replayed;
        let mut reasons = vec![format!("{words_per_second:.1} words/s")];
        if replayed >= 0.4 {
            reasons.insert(0, "most replayed part on YouTube".into());
        }
        if loud >= 0.3 {
            reasons.push("louder than the rest of the video".into());
        }
        if hook {
            reasons.push("hook in the opening line".into());
        }
        if energy > 0.0 {
            reasons.push("energetic delivery".into());
        }
        if complete {
            reasons.push("ends on a full sentence".into());
        }
        candidates.push(AutoHighlight {
            id: Uuid::new_v4(),
            start_seconds: start,
            end_seconds: end,
            title: hook_title(&window[0].text),
            reason: reasons.join(", "),
            score: score.clamp(0.0, 1.0),
            output_path: None,
            language: language.to_string(),
            description: social_description(&text),
            hashtags: social_hashtags(&text, language),
        });
    }
    select_top(candidates, max_clips)
}

/// Looks at three frames of a candidate: how often a face is on screen and whether it is a close-up.
async fn face_presence(input: &std::path::Path, start: f64, length: f64) -> Option<(f64, bool)> {
    let model = face_model()?;
    let output = tool_command(resolve_tool_path("ffmpeg"))
        .args([
            "-hide_banner".to_string(), "-loglevel".into(), "error".into(),
            "-ss".into(), format!("{:.3}", start + length * 0.15), "-t".into(), format!("{:.3}", length * 0.7),
            "-i".into(), input.to_string_lossy().to_string(),
            "-vf".into(), format!("fps={:.4},scale={FACE_W}:{FACE_H}", 3.0 / (length * 0.7).max(1.0)),
            "-frames:v".into(), "3".into(), "-f".into(), "rawvideo".into(), "-pix_fmt".into(), "rgb24".into(), "-".into(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    let frames: Vec<Vec<FaceBox>> = output.stdout.chunks_exact(FACE_W * FACE_H * 3).map(|frame| detect_faces(model, frame)).collect();
    if frames.is_empty() {
        return None;
    }
    let with_face = frames.iter().filter(|faces| !faces.is_empty()).count() as f64 / frames.len() as f64;
    let close_up = frames.iter().flatten().any(|f| (f.x2 - f.x1) * (f.y2 - f.y1) > 0.02);
    Some((with_face, close_up))
}

/// Re-scores the strongest candidates by what is on screen, then keeps the best non-overlapping ones.
async fn rank_by_picture(input: &std::path::Path, mut candidates: Vec<AutoHighlight>, count: usize) -> Vec<AutoHighlight> {
    for clip in candidates.iter_mut() {
        if let Some((with_face, close_up)) = face_presence(input, clip.start_seconds, clip.end_seconds - clip.start_seconds).await {
            clip.score = (clip.score + 0.12 * with_face + if close_up { 0.06 } else { 0.0 }).clamp(0.0, 1.0);
            if with_face >= 0.6 {
                clip.reason.push_str(if close_up { ", close-up faces on screen" } else { ", faces on screen" });
            }
        }
    }
    select_top(candidates, count)
}

/// Runs whisper-cli once on 16 kHz audio and returns its JSON (with broken multi-byte characters repaired).
/// `duration_ms` limits the run to the start of the audio, for quick language checks.
async fn run_whisper(model: &std::path::Path, wav: &std::path::Path, language: &str, duration_ms: Option<u32>, state: &AppState) -> Result<String, String> {
    let out = std::env::temp_dir().join(format!("shortforge-transcript-{}", Uuid::new_v4()));
    let json_path = out.with_extension("json");
    let mut args: Vec<String> = vec![
        "-m".into(), model.to_string_lossy().to_string(),
        "-f".into(), wav.to_string_lossy().to_string(),
        "-oj".into(), "-np".into(),
        "-t".into(), std::thread::available_parallelism().map_or(4, |n| n.get().clamp(1, 8)).to_string(),
        "-l".into(), language.to_string(),
        "-of".into(), out.to_string_lossy().to_string(),
    ];
    if let Some(ms) = duration_ms {
        args.extend(["-d".into(), ms.to_string()]);
    }
    // Voice activity detection skips music and silence: faster, and no words invented over music.
    let vad = whisper_dev_dir().join("ggml-silero-v5.1.2.bin");
    let with_vad = vad.is_file();
    if with_vad {
        args.extend(["--vad".into(), "-vm".into(), vad.to_string_lossy().to_string()]);
    }
    let result = async {
        match run_cancellable("whisper-cli", &args, state).await {
            Ok(_) => {}
            Err(error) if error == STOPPED || !with_vad => return Err(error),
            Err(_) => {
                // An older whisper-cli without VAD support: run once more without it.
                args.truncate(args.len() - 3);
                run_cancellable("whisper-cli", &args, state).await?;
            }
        }
        // whisper.cpp can split one multi-byte character across tokens (common in Indic scripts),
        // leaving invalid UTF-8 in its JSON; repair those bytes instead of losing the transcript.
        fs::read(&json_path)
            .await
            .map(|bytes| String::from_utf8_lossy(&bytes).replace(char::REPLACEMENT_CHARACTER, ""))
            .map_err(|error| format!("Whisper did not write a transcript: {error}"))
    }
    .await;
    let _ = fs::remove_file(&json_path).await;
    result
}

/// Which model transcribes what: English goes to the fast model, other languages to the
/// accurate one (large-v3-turbo) when it is installed, since base garbles Indian languages.
fn choose_whisper_model(language: Option<&str>, fast: &std::path::Path, accurate: Option<&std::path::Path>) -> PathBuf {
    match (language, accurate) {
        (Some("en"), _) | (_, None) => fast.to_path_buf(),
        (_, Some(accurate)) => accurate.to_path_buf(),
    }
}

async fn transcribe_with_whisper(source: &std::path::Path, state: &AppState, forced_language: Option<&str>) -> Result<(Vec<TranscriptSegment>, String, Vec<(f64, f64)>), String> {
    let fast = std::env::var_os("SHORTFORGE_WHISPER_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            ["ggml-base.bin", "ggml-base.en.bin"]
                .iter()
                .map(|name| whisper_dev_dir().join(name))
                .find(|path| path.is_file())
                .unwrap_or_else(|| whisper_dev_dir().join("ggml-base.bin"))
        });
    if !fast.is_file() {
        return Err(format!(
            "Whisper model not found at {}. Download a free ggml model (for example ggml-base.bin) into tools/whisper or set SHORTFORGE_WHISPER_MODEL.",
            fast.display()
        ));
    }
    let accurate = Some(whisper_dev_dir().join("ggml-large-v3-turbo-q5_0.bin")).filter(|path| path.is_file());
    let english_only = fast.to_string_lossy().ends_with(".en.bin");
    let wav = std::env::temp_dir().join(format!("shortforge-audio-{}.wav", Uuid::new_v4()));
    let result = async {
        run_tool(
            "ffmpeg",
            &[
                "-hide_banner".into(), "-loglevel".into(), "error".into(), "-y".into(),
                "-i".into(), source.to_string_lossy().to_string(),
                "-vn".into(), "-ac".into(), "1".into(), "-ar".into(), "16000".into(),
                "-c:a".into(), "pcm_s16le".into(), wav.to_string_lossy().to_string(),
            ],
        )
        .await?;
        // Loudness over time from the same 16 kHz audio, used to spot the moments that stand out.
        let loudness = run_tool(
            "ffmpeg",
            &[
                "-hide_banner".into(), "-nostats".into(), "-i".into(), wav.to_string_lossy().to_string(),
                "-af".into(), "ebur128=metadata=1,ametadata=print:key=lavfi.r128.M:file=-".into(),
                "-f".into(), "null".into(), "-".into(),
            ],
        )
        .await
        .map(|output| parse_loudness(&output))
        .unwrap_or_default();

        // Know the language before choosing a model: a 30 s check with the fast model takes seconds.
        let language = match forced_language {
            Some(code) => Some(code.to_string()),
            None if english_only => Some("en".to_string()),
            None if accurate.is_none() => None,
            None => Some(whisper_language(&run_whisper(&fast, &wav, "auto", Some(30_000), state).await?)).filter(|code| !code.is_empty()),
        };
        let model = choose_whisper_model(language.as_deref(), &fast, accurate.as_deref());
        let mut whisper_language_arg = match forced_language {
            Some(code) => code.to_string(),
            None if english_only => "en".to_string(),
            None => "auto".to_string(),
        };
        // A chosen language is tried on the first minute; if it yields nothing usable, auto-detect instead
        // of spending a whole run on it.
        if forced_language.is_some_and(|code| code != "en") {
            let probe = run_whisper(&model, &wav, &whisper_language_arg, Some(60_000), state).await?;
            if whisper_segments(&probe).map(|segments| segments.is_empty()).unwrap_or(true) {
                whisper_language_arg = "auto".into();
            }
        }
        let json = run_whisper(&model, &wav, &whisper_language_arg, None, state).await?;
        let language = Some(whisper_language(&json)).filter(|code| !code.is_empty()).or(language).unwrap_or_default();
        whisper_segments(&json).map(|segments| (segments, language, loudness))
    }
    .await;
    let _ = fs::remove_file(&wav).await;
    result
}


#[tauri::command]
async fn list_auto_highlights(
    project_id: Uuid,
    media_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<AutoHighlight>, String> {
    sqlx::query_as::<_, AutoHighlight>(
        "SELECT id,start_seconds,end_seconds,title,reason,score,output_path,language,description,hashtags FROM auto_highlights WHERE project_id=$1 AND media_path=$2 ORDER BY score DESC, start_seconds",
    )
    .bind(project_id)
    .bind(media_path)
    .fetch_all(&state.db)
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn find_auto_highlights(
    project_id: Uuid,
    media_path: String,
    max_clips: u32,
    language: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<AutoHighlight>, String> {
    // A two-letter Whisper language code the user picked, or None to auto-detect.
    let language = language.filter(|code| code.len() == 2 && code.chars().all(|c| c.is_ascii_lowercase()));
    if !(1..=20).contains(&max_clips) {
        return Err("Clip count must be between 1 and 20.".into());
    }
    let row: Option<(f64, bool)> = sqlx::query_as(
        "SELECT duration_seconds,has_audio FROM media_assets WHERE project_id=$1 AND original_path=$2",
    )
    .bind(project_id)
    .bind(&media_path)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| error.to_string())?;
    let (duration, has_audio) = row.ok_or("The selected source is not in this project's media library.")?;
    if !has_audio {
        return Err("The auto clipper needs a source with an audio track.".into());
    }
    let media_root = fs::canonicalize(&state.media_dir)
        .await
        .map_err(|error| error.to_string())?;
    let source = fs::canonicalize(&media_path)
        .await
        .map_err(|error| format!("Source media is unavailable: {error}"))?;
    if !source.starts_with(&media_root) || !source.is_file() {
        return Err("The auto clipper only runs on media in the local project library.".into());
    }
    if !duration.is_finite() || duration <= 0.0 || duration > 4.0 * 3600.0 {
        return Err("The auto clipper supports media up to 4 hours long.".into());
    }
    let (segments, language, loudness) = transcribe_with_whisper(&source, state.inner(), language.as_deref()).await?;
    if segments.is_empty() {
        return Err("No speech was found in this source.".into());
    }
    let kept: Vec<(f64, f64)> = sqlx::query_as(
        "SELECT start_seconds,end_seconds FROM auto_highlights WHERE project_id=$1 AND media_path=$2 AND output_path IS NOT NULL",
    )
    .bind(project_id)
    .bind(&media_path)
    .fetch_all(&state.db)
    .await
    .map_err(|error| error.to_string())?;
    // Speech and sound narrow the field to three times the shorts asked for; the picture decides the final order.
    let replay: Vec<(f64, f64, f64)> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT heatmap FROM media_assets WHERE project_id=$1 AND original_path=$2",
    )
    .bind(project_id)
    .bind(&media_path)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| error.to_string())?
    .flatten()
    .and_then(|json| serde_json::from_str(&json).ok())
    .unwrap_or_default();
    let shortlist = pick_highlights(&segments, duration, (max_clips as usize + kept.len()) * 3, &language, &loudness, &replay);
    let clips: Vec<AutoHighlight> = rank_by_picture(&source, shortlist, max_clips as usize * 3 + kept.len()).await
        .into_iter()
        .filter(|clip| kept.iter().all(|(start, end)| clip.end_seconds <= *start || clip.start_seconds >= *end))
        .take(max_clips as usize)
        .collect();

    let mut transaction = state.db.begin().await.map_err(|error| error.to_string())?;
    sqlx::query("DELETE FROM auto_highlights WHERE project_id=$1 AND media_path=$2 AND output_path IS NULL")
        .bind(project_id)
        .bind(&media_path)
        .execute(&mut *transaction)
        .await
        .map_err(|error| error.to_string())?;
    for clip in &clips {
        sqlx::query("INSERT INTO auto_highlights (id,project_id,media_path,start_seconds,end_seconds,title,reason,score,language,description,hashtags) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(clip.id)
            .bind(project_id)
            .bind(&media_path)
            .bind(clip.start_seconds)
            .bind(clip.end_seconds)
            .bind(&clip.title)
            .bind(&clip.reason)
            .bind(clip.score)
            .bind(&clip.language)
            .bind(&clip.description)
            .bind(&clip.hashtags)
            .execute(&mut *transaction)
            .await
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().await.map_err(|error| error.to_string())?;
    // Finished shorts that were kept come back too, best first.
    list_auto_highlights(project_id, media_path, state).await
}

/// Renders one suggested clip as a 9:16 short and remembers the output next to the suggestion.
#[tauri::command]
async fn render_auto_highlight(
    project_id: Uuid,
    highlight_id: Uuid,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<AutoHighlight, String> {
    let db = state.db.clone();
    let export_dir = state.export_dir.clone();
    let previous_output: Option<String> = sqlx::query_scalar("SELECT output_path FROM auto_highlights WHERE id=$1 AND project_id=$2")
        .bind(highlight_id)
        .bind(project_id)
        .fetch_optional(&db)
        .await
        .map_err(|error| error.to_string())?
        .flatten();
    let (media_path, start_seconds, end_seconds): (String, f64, f64) = sqlx::query_as(
        "SELECT media_path,start_seconds,end_seconds FROM auto_highlights WHERE id=$1 AND project_id=$2",
    )
    .bind(highlight_id)
    .bind(project_id)
    .fetch_optional(&db)
    .await
    .map_err(|error| error.to_string())?
    .ok_or("That clip suggestion no longer exists. Run the clipper again.")?;
    let output = render_short(
        project_id,
        media_path,
        start_seconds,
        end_seconds - start_seconds,
        "9:16".into(),
        None,
        None,
        None,
        Some("smart".into()),
        app,
        state,
    )
    .await?;
    let updated = sqlx::query_as::<_, AutoHighlight>(
        "UPDATE auto_highlights SET output_path=$1 WHERE id=$2 AND project_id=$3 RETURNING id,start_seconds,end_seconds,title,reason,score,output_path,language,description,hashtags",
    )
    .bind(&output)
    .bind(highlight_id)
    .bind(project_id)
    .fetch_one(&db)
    .await
    .map_err(|error| error.to_string());
    // A re-render replaces the earlier file of the same short.
    if updated.is_ok() {
        if let Some(previous) = previous_output.filter(|previous| *previous != output) {
            let _ = remove_managed_file(Some(previous), &export_dir).await;
        }
    }
    updated
}

#[tauri::command]
async fn list_silence_ranges(
    project_id: Uuid,
    media_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SilenceRange>, String> {
    sqlx::query_as::<_, SilenceRange>(
        "SELECT id,start_seconds,end_seconds,noise_threshold_db,minimum_duration_seconds FROM silence_ranges WHERE project_id=$1 AND media_path=$2 ORDER BY start_seconds",
    )
    .bind(project_id)
    .bind(media_path)
    .fetch_all(&state.db)
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn detect_silence_ranges(
    project_id: Uuid,
    media_path: String,
    noise_threshold_db: f64,
    minimum_duration_seconds: f64,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SilenceRange>, String> {
    if !noise_threshold_db.is_finite() || !(-60.0..=-10.0).contains(&noise_threshold_db) {
        return Err("Silence threshold must be between -60 and -10 dB.".into());
    }
    if !minimum_duration_seconds.is_finite() || !(0.1..=30.0).contains(&minimum_duration_seconds) {
        return Err("Minimum silence duration must be between 0.1 and 30 seconds.".into());
    }
    let source_info: Option<(f64, bool)> = sqlx::query_as(
        "SELECT duration_seconds,has_audio FROM media_assets WHERE project_id=$1 AND original_path=$2",
    )
    .bind(project_id)
    .bind(&media_path)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| error.to_string())?;
    let (duration, has_audio) =
        source_info.ok_or("The selected source is not in this project's media library.")?;
    if !has_audio {
        return Err("This video has no audio track to analyze.".into());
    }
    if !duration.is_finite() || duration <= 0.0 || duration > 3600.0 {
        return Err("Silence detection supports media up to 60 minutes long.".into());
    }
    let media_root = fs::canonicalize(&state.media_dir)
        .await
        .map_err(|error| error.to_string())?;
    let source = fs::canonicalize(&media_path)
        .await
        .map_err(|error| format!("Source media is unavailable: {error}"))?;
    if !source.starts_with(&media_root) || !source.is_file() {
        return Err("Silence detection only runs on media in the local project library.".into());
    }
    let filter =
        format!("silencedetect=noise={noise_threshold_db:.1}dB:d={minimum_duration_seconds:.3}");
    let args = vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-loglevel".into(),
        "info".into(),
        "-i".into(),
        source.to_string_lossy().to_string(),
        "-vn".into(),
        "-af".into(),
        filter,
        "-f".into(),
        "null".into(),
        "-".into(),
    ];
    let output = tool_command(resolve_tool_path("ffmpeg"))
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("Could not start ffmpeg: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg silence detection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let ranges = parse_silence_output(
        &String::from_utf8_lossy(&output.stderr),
        duration,
        minimum_duration_seconds,
    );

    let mut transaction = state.db.begin().await.map_err(|error| error.to_string())?;
    sqlx::query("DELETE FROM silence_ranges WHERE project_id=$1 AND media_path=$2")
        .bind(project_id)
        .bind(&media_path)
        .execute(&mut *transaction)
        .await
        .map_err(|error| error.to_string())?;
    let mut results = Vec::with_capacity(ranges.len());
    for (start_seconds, end_seconds) in ranges.into_iter().take(5000) {
        let range = SilenceRange {
            id: Uuid::new_v4(),
            start_seconds,
            end_seconds,
            noise_threshold_db,
            minimum_duration_seconds,
        };
        sqlx::query("INSERT INTO silence_ranges (id,project_id,media_path,start_seconds,end_seconds,noise_threshold_db,minimum_duration_seconds) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(range.id)
            .bind(project_id)
            .bind(&media_path)
            .bind(range.start_seconds)
            .bind(range.end_seconds)
            .bind(range.noise_threshold_db)
            .bind(range.minimum_duration_seconds)
            .execute(&mut *transaction)
            .await
            .map_err(|error| error.to_string())?;
        results.push(range);
    }
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(results)
}

#[tauri::command]
async fn list_timeline(
    project_id: Uuid,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<TimelineClip>, String> {
    fetch_timeline(&state.db, project_id).await
}

async fn fetch_timeline(db: &PgPool, project_id: Uuid) -> Result<Vec<TimelineClip>, String> {
    sqlx::query_as::<_, TimelineClip>(
        "SELECT t.id,t.media_path,m.file_name,t.source_in_frame,t.source_out_frame,m.frame_rate,m.duration_seconds,m.has_audio FROM timeline_clips t JOIN media_assets m ON m.project_id=t.project_id AND m.original_path=t.media_path WHERE t.project_id=$1 ORDER BY t.position_index",
    )
    .bind(project_id)
    .fetch_all(db)
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn save_timeline(
    project_id: Uuid,
    clips: Vec<TimelineClipInput>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if clips.len() > 300 {
        return Err("A timeline can contain at most 300 clips.".into());
    }
    let media_root = fs::canonicalize(&state.media_dir)
        .await
        .map_err(|error| error.to_string())?;
    let mut ids = HashSet::with_capacity(clips.len());
    for clip in &clips {
        if !ids.insert(clip.id) {
            return Err("Timeline clip identifiers must be unique.".into());
        }
        let source: Option<(String, f64)> = sqlx::query_as(
            "SELECT frame_rate,duration_seconds FROM media_assets WHERE project_id=$1 AND original_path=$2",
        )
        .bind(project_id)
        .bind(&clip.media_path)
        .fetch_optional(&state.db)
        .await
        .map_err(|error| error.to_string())?;
        let (frame_rate, duration) = source.ok_or("Timeline source is not in this project.")?;
        let path = fs::canonicalize(&clip.media_path)
            .await
            .map_err(|error| format!("Timeline source is unavailable: {error}"))?;
        if !path.starts_with(&media_root) || !path.is_file() {
            return Err(
                "Timeline source must be an available file in the local project library.".into(),
            );
        }
        let (numerator, denominator) = parse_frame_rate(&frame_rate)?;
        let frame_count = timeline_frame_count(duration, numerator, denominator)
            .ok_or("Could not determine the source frame count.")?;
        if clip.source_in_frame < 0
            || clip.source_out_frame <= clip.source_in_frame
            || clip.source_out_frame > frame_count
        {
            return Err("Timeline clip frames must be a non-empty range within the source.".into());
        }
    }

    let mut transaction = state.db.begin().await.map_err(|error| error.to_string())?;
    sqlx::query("DELETE FROM timeline_clips WHERE project_id=$1")
        .bind(project_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| error.to_string())?;
    for (position_index, clip) in clips.iter().enumerate() {
        sqlx::query("INSERT INTO timeline_clips (id,project_id,media_path,position_index,source_in_frame,source_out_frame) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(clip.id)
            .bind(project_id)
            .bind(&clip.media_path)
            .bind(position_index as i32)
            .bind(clip.source_in_frame)
            .bind(clip.source_out_frame)
            .execute(&mut *transaction)
            .await
            .map_err(|error| error.to_string())?;
    }
    sqlx::query("UPDATE projects SET updated_at=NOW() WHERE id=$1")
        .bind(project_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| error.to_string())?;
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn import_audio(
    project_id: Uuid,
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<AudioAsset, String> {
    let source = ensure_safe_file(&path)?;
    let args = vec![
        "-v".into(),
        "error".into(),
        "-show_entries".into(),
        "format=duration:stream=codec_type".into(),
        "-of".into(),
        "json".into(),
        path.clone(),
    ];
    let raw = run_tool("ffprobe", &args).await?;
    let probe: serde_json::Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
    if !probe["streams"]
        .as_array()
        .is_some_and(|streams| streams.iter().any(|stream| stream["codec_type"] == "audio"))
    {
        return Err("The selected file does not contain an audio stream.".into());
    }
    let duration_seconds = probe["format"]["duration"]
        .as_str()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or("Could not determine the audio duration.")?;
    let metadata = fs::metadata(&source)
        .await
        .map_err(|error| error.to_string())?;
    let file_size_bytes = i64::try_from(metadata.len())
        .ok()
        .filter(|size| *size > 0)
        .ok_or("Audio file is empty or too large to index.")?;
    let content_sha256 = sha256_file(&source).await?;
    let duplicate: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM audio_assets WHERE project_id=$1 AND content_sha256=$2)",
    )
    .bind(project_id)
    .bind(&content_sha256)
    .fetch_one(&state.db)
    .await
    .map_err(|error| error.to_string())?;
    if duplicate {
        return Err("This audio file is already in the selected project library.".into());
    }
    let id = Uuid::new_v4();
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "audio".into());
    let stored = state.media_dir.join(format!("audio-{id}.{extension}"));
    fs::copy(&source, &stored)
        .await
        .map_err(|error| format!("Could not copy audio into the local project library: {error}"))?;
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("audio")
        .to_string();
    let stored_path = stored.to_string_lossy().to_string();
    let result = sqlx::query("INSERT INTO audio_assets (id,project_id,original_path,file_name,duration_seconds,file_size_bytes,content_sha256) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(id)
        .bind(project_id)
        .bind(&stored_path)
        .bind(&file_name)
        .bind(duration_seconds)
        .bind(file_size_bytes)
        .bind(&content_sha256)
        .execute(&state.db)
        .await;
    if let Err(error) = result {
        let _ = fs::remove_file(&stored).await;
        return Err(format!("Could not save audio metadata: {error}"));
    }
    Ok(AudioAsset {
        id,
        path: stored_path,
        file_name,
        duration_seconds,
        file_size_bytes,
    })
}

#[tauri::command]
async fn list_project_audio(
    project_id: Uuid,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<AudioAsset>, String> {
    sqlx::query_as::<_, AudioAsset>(
        "SELECT id,original_path AS path,file_name,duration_seconds,file_size_bytes FROM audio_assets WHERE project_id=$1 ORDER BY imported_at DESC",
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn list_timeline_audio(
    project_id: Uuid,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<TimelineAudioClip>, String> {
    fetch_timeline_audio(&state.db, project_id).await
}

async fn fetch_timeline_audio(
    db: &PgPool,
    project_id: Uuid,
) -> Result<Vec<TimelineAudioClip>, String> {
    sqlx::query_as::<_, TimelineAudioClip>(
        "SELECT t.id,t.audio_asset_id,a.original_path AS path,a.file_name,t.track_kind,t.timeline_start_seconds,t.source_in_seconds,t.duration_seconds,t.gain,t.fade_in_seconds,t.fade_out_seconds,a.duration_seconds AS source_duration_seconds FROM timeline_audio_clips t JOIN audio_assets a ON a.project_id=t.project_id AND a.id=t.audio_asset_id WHERE t.project_id=$1 ORDER BY t.position_index",
    )
    .bind(project_id)
    .fetch_all(db)
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn save_timeline_audio(
    project_id: Uuid,
    clips: Vec<TimelineAudioClipInput>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if clips.len() > 300 {
        return Err("A project can contain at most 300 audio clips.".into());
    }
    let media_root = fs::canonicalize(&state.media_dir)
        .await
        .map_err(|error| error.to_string())?;
    let mut ids = HashSet::with_capacity(clips.len());
    for clip in &clips {
        if !ids.insert(clip.id) {
            return Err("Audio clip identifiers must be unique.".into());
        }
        if !matches!(clip.track_kind.as_str(), "voice" | "music" | "effects")
            || !clip.timeline_start_seconds.is_finite()
            || clip.timeline_start_seconds < 0.0
            || !clip.source_in_seconds.is_finite()
            || clip.source_in_seconds < 0.0
            || !clip.duration_seconds.is_finite()
            || clip.duration_seconds <= 0.0
            || clip.timeline_start_seconds + clip.duration_seconds > 900.0
            || !clip.gain.is_finite()
            || !(0.0..=2.0).contains(&clip.gain)
            || !clip.fade_in_seconds.is_finite()
            || clip.fade_in_seconds < 0.0
            || !clip.fade_out_seconds.is_finite()
            || clip.fade_out_seconds < 0.0
            || clip.fade_in_seconds + clip.fade_out_seconds > clip.duration_seconds
        {
            return Err("Audio clip timing, gain, track, or fade values are invalid.".into());
        }
        let source: Option<(String, f64)> = sqlx::query_as(
            "SELECT original_path,duration_seconds FROM audio_assets WHERE project_id=$1 AND id=$2",
        )
        .bind(project_id)
        .bind(clip.audio_asset_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|error| error.to_string())?;
        let (path, source_duration) = source.ok_or("Audio clip source is not in this project.")?;
        if clip.source_in_seconds + clip.duration_seconds > source_duration + 0.000001 {
            return Err("Audio clip source range extends beyond its audio file.".into());
        }
        let canonical_path = fs::canonicalize(path)
            .await
            .map_err(|error| format!("Audio source is unavailable: {error}"))?;
        if !canonical_path.starts_with(&media_root) || !canonical_path.is_file() {
            return Err("Audio source must remain inside the local project library.".into());
        }
    }

    let mut transaction = state.db.begin().await.map_err(|error| error.to_string())?;
    sqlx::query("DELETE FROM timeline_audio_clips WHERE project_id=$1")
        .bind(project_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| error.to_string())?;
    for (position_index, clip) in clips.iter().enumerate() {
        sqlx::query("INSERT INTO timeline_audio_clips (id,project_id,audio_asset_id,position_index,track_kind,timeline_start_seconds,source_in_seconds,duration_seconds,gain,fade_in_seconds,fade_out_seconds) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(clip.id)
            .bind(project_id)
            .bind(clip.audio_asset_id)
            .bind(position_index as i32)
            .bind(&clip.track_kind)
            .bind(clip.timeline_start_seconds)
            .bind(clip.source_in_seconds)
            .bind(clip.duration_seconds)
            .bind(clip.gain)
            .bind(clip.fade_in_seconds)
            .bind(clip.fade_out_seconds)
            .execute(&mut *transaction)
            .await
            .map_err(|error| error.to_string())?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn create_project(
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<Project, String> {
    if name.trim().is_empty() || name.len() > 120 {
        return Err("Project name must be 1–120 characters.".into());
    }
    let id = Uuid::new_v4();
    sqlx::query_as::<_, Project>(
        "INSERT INTO projects (id,name) VALUES ($1,$2) RETURNING id,name,created_at",
    )
    .bind(id)
    .bind(name.trim())
    .fetch_one(&state.db)
    .await
    .map_err(|e| e.to_string())
}
#[tauri::command]
async fn probe_media(path: String) -> Result<Probe, String> {
    let p = ensure_safe_file(&path)?;
    let args = vec![
        "-v".into(),
        "error".into(),
        "-show_entries".into(),
        "format=duration,size:stream=codec_type,width,height,r_frame_rate".into(),
        "-of".into(),
        "json".into(),
        path.clone(),
    ];
    let raw = run_tool("ffprobe", &args).await?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let stream = v["streams"]
        .as_array()
        .and_then(|streams| streams.iter().find(|s| s["codec_type"] == "video"))
        .ok_or("No video stream found")?;
    let duration_seconds = v["format"]["duration"]
        .as_str()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0.0);
    let size_bytes = v["format"]["size"]
        .as_str()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    let file_name = p
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("video")
        .to_string();
    let has_audio = v["streams"]
        .as_array()
        .map(|streams| streams.iter().any(|s| s["codec_type"] == "audio"))
        .unwrap_or(false);
    Ok(Probe {
        path,
        file_name,
        duration_seconds,
        width: stream["width"].as_u64().unwrap_or(0) as u32,
        height: stream["height"].as_u64().unwrap_or(0) as u32,
        frame_rate: stream["r_frame_rate"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        size_bytes,
        has_audio,
        category: "Uncategorized".into(),
        missing: false,
        thumbnail_path: None,
    })
}
/// Accepts only absolute http(s) links without whitespace; yt-dlp receives it after `--` so it can never be read as an option.
fn validate_video_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if url.len() > 2048
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
        || !(lower.starts_with("https://") || lower.starts_with("http://"))
        || url.split("://").nth(1).map_or(true, |rest| rest.is_empty() || rest.starts_with('/'))
    {
        return Err("Paste a full video link that starts with https://".into());
    }
    Ok(url.to_string())
}

/// Downloads one video from a link (YouTube and other sites yt-dlp supports) and imports it like a local file.
#[tauri::command]
async fn import_url(
    project_id: Uuid,
    url: String,
    state: tauri::State<'_, AppState>,
) -> Result<Probe, String> {
    let url = validate_video_url(&url)?;
    let download_dir = std::env::temp_dir().join(format!("shortforge-download-{}", Uuid::new_v4()));
    fs::create_dir_all(&download_dir)
        .await
        .map_err(|error| error.to_string())?;
    let ffmpeg = resolve_tool_path("ffmpeg");
    let mut args: Vec<String> = vec![
        "--no-playlist".into(), "--no-progress".into(), "--no-warnings".into(),
        "--restrict-filenames".into(),
        "-f".into(), "bv*[ext=mp4][height<=1080]+ba[ext=m4a]/b[ext=mp4][height<=1080]/bv*[height<=1080]+ba/b".into(),
        "--merge-output-format".into(), "mp4".into(),
        "-o".into(), download_dir.join("%(title).80B-%(id)s.%(ext)s").to_string_lossy().to_string(),
        "--print".into(), "video:REPLAY %(heatmap)j".into(),
        "--print".into(), "after_move:filepath".into(),
    ];
    if let Some(directory) = ffmpeg.parent().filter(|directory| directory.is_dir()) {
        args.extend(["--ffmpeg-location".into(), directory.to_string_lossy().to_string()]);
    }
    args.extend(["--".into(), url]);
    let result = async {
        let output = run_cancellable("yt-dlp", &args, state.inner()).await.map_err(|error| {
            if error == STOPPED {
                return error;
            }
            format!("Could not download that link. Put yt-dlp.exe in tools/yt-dlp (or set SHORTFORGE_YTDLP_DIR). Details: {error}")
        })?;
        let replay = output.lines().find_map(|line| line.trim().strip_prefix("REPLAY ")).map(parse_heatmap).unwrap_or_default();
        let downloaded = output
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("REPLAY "))
            .map(PathBuf::from)
            .filter(|path| path.is_file() && path.starts_with(&download_dir))
            .ok_or("The download finished but no video file was produced.")?;
        let db = state.db.clone();
        let probe = import_media(project_id, downloaded.to_string_lossy().to_string(), state).await?;
        if !replay.is_empty() {
            sqlx::query("UPDATE media_assets SET heatmap=$1 WHERE project_id=$2 AND original_path=$3")
                .bind(serde_json::to_string(&replay).map_err(|error| error.to_string())?)
                .bind(project_id)
                .bind(&probe.path)
                .execute(&db)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(probe)
    }
    .await;
    let _ = fs::remove_dir_all(&download_dir).await;
    result
}

#[tauri::command]
async fn import_media(
    project_id: Uuid,
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Probe, String> {
    let source = ensure_safe_file(&path)?;
    let mut metadata = probe_media(path.clone()).await?;
    let content_sha256 = sha256_file(&source).await?;
    let is_duplicate: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM media_assets WHERE project_id=$1 AND content_sha256=$2)",
    )
    .bind(project_id)
    .bind(&content_sha256)
    .fetch_one(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    if is_duplicate {
        return Err("This exact video is already in the selected project.".into());
    }
    let extension = source.extension().and_then(|x| x.to_str()).unwrap_or("mp4");
    let stored = state
        .media_dir
        .join(format!("{}.{}", Uuid::new_v4(), extension));
    fs::copy(&source, &stored)
        .await
        .map_err(|e| format!("Could not copy media into the project library: {e}"))?;
    let thumbnail =
        match generate_thumbnail(&stored, metadata.duration_seconds, &state.thumbnail_dir).await {
            Ok(path) => path,
            Err(error) => {
                let _ = fs::remove_file(&stored).await;
                return Err(error);
            }
        };
    let stored_text = stored.to_string_lossy().to_string();
    let thumbnail_text = thumbnail.to_string_lossy().to_string();
    let result = sqlx::query("INSERT INTO media_assets (id,project_id,original_path,file_name,duration_seconds,width,height,file_size_bytes,frame_rate,has_audio,content_sha256,thumbnail_path) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
        .bind(Uuid::new_v4()).bind(project_id).bind(&stored_text).bind(&metadata.file_name)
        .bind(metadata.duration_seconds).bind(metadata.width as i32).bind(metadata.height as i32).bind(metadata.size_bytes as i64)
        .bind(&metadata.frame_rate).bind(metadata.has_audio).bind(&content_sha256).bind(&thumbnail_text)
        .execute(&state.db).await;
    if let Err(error) = result {
        let _ = fs::remove_file(&stored).await;
        let _ = fs::remove_file(&thumbnail).await;
        return Err(format!("Could not save media metadata: {error}"));
    }
    metadata.path = stored_text;
    metadata.thumbnail_path = Some(thumbnail_text);
    Ok(metadata)
}

#[tauri::command]
async fn get_media_thumbnail(
    project_id: Uuid,
    media_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<u8>, String> {
    let thumbnail_path: Option<String> = sqlx::query_scalar(
        "SELECT thumbnail_path FROM media_assets WHERE project_id=$1 AND original_path=$2",
    )
    .bind(project_id)
    .bind(media_path)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| error.to_string())?
    .flatten();
    let thumbnail_path = thumbnail_path.ok_or("Thumbnail is not available for this media item.")?;
    let path = PathBuf::from(thumbnail_path);
    let thumbnail_root = fs::canonicalize(&state.thumbnail_dir)
        .await
        .map_err(|error| error.to_string())?;
    let canonical_path = fs::canonicalize(&path)
        .await
        .map_err(|error| format!("Thumbnail file is missing: {error}"))?;
    if !canonical_path.starts_with(thumbnail_root) || !canonical_path.is_file() {
        return Err("Thumbnail path is outside the local thumbnail directory.".into());
    }
    fs::read(canonical_path)
        .await
        .map_err(|error| format!("Could not read thumbnail: {error}"))
}

#[tauri::command]
async fn set_media_category(
    project_id: Uuid,
    media_path: String,
    category: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let category = category.trim();
    if category.is_empty() || category.len() > 40 {
        return Err("Category must contain 1–40 characters.".into());
    }
    let result =
        sqlx::query("UPDATE media_assets SET category=$1 WHERE project_id=$2 AND original_path=$3")
            .bind(category)
            .bind(project_id)
            .bind(media_path)
            .execute(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    if result.rows_affected() == 0 {
        return Err("Media item was not found in this project.".into());
    }
    Ok(())
}

async fn remove_managed_file(path: Option<String>, root: &PathBuf) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let canonical_root = fs::canonicalize(root)
        .await
        .map_err(|error| error.to_string())?;
    let canonical_file = match fs::canonicalize(&path).await {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Could not resolve managed file: {error}")),
    };
    if !canonical_file.starts_with(&canonical_root) || !canonical_file.is_file() {
        return Err("Refusing to remove a file outside the ShortForge library.".into());
    }
    fs::remove_file(canonical_file)
        .await
        .map_err(|error| format!("Could not remove managed file: {error}"))
}

#[tauri::command]
async fn delete_project(project_id: Uuid, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let media: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT original_path,thumbnail_path FROM media_assets WHERE project_id=$1",
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await
    .map_err(|error| error.to_string())?;
    let outputs: Vec<String> = sqlx::query_scalar(
        "SELECT output_path FROM exports WHERE project_id=$1 UNION SELECT output_path FROM auto_highlights WHERE project_id=$1 AND output_path IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await
    .map_err(|error| error.to_string())?;
    let deleted = sqlx::query("DELETE FROM projects WHERE id=$1")
        .bind(project_id)
        .execute(&state.db)
        .await
        .map_err(|error| error.to_string())?;
    if deleted.rows_affected() == 0 {
        return Err("Project was not found.".into());
    }
    // The project is gone from the database; a file that cannot be removed must not undo that.
    for (media_path, thumbnail) in media {
        let _ = remove_managed_file(Some(media_path), &state.media_dir).await;
        let _ = remove_managed_file(thumbnail, &state.thumbnail_dir).await;
    }
    for output in outputs {
        let _ = remove_managed_file(Some(output), &state.export_dir).await;
    }
    Ok(())
}

/// Saves the post text a person edited for one short (title, description, hashtags).
#[tauri::command]
async fn update_auto_highlight_text(
    project_id: Uuid,
    highlight_id: Uuid,
    title: String,
    description: String,
    hashtags: String,
    state: tauri::State<'_, AppState>,
) -> Result<AutoHighlight, String> {
    let (title, description, hashtags) = (title.trim(), description.trim(), hashtags.trim());
    if title.is_empty() || title.chars().count() > 150 {
        return Err("The title must be 1 to 150 characters.".into());
    }
    if description.chars().count() > 2200 || hashtags.chars().count() > 500 {
        return Err("The description is limited to 2,200 characters and hashtags to 500.".into());
    }
    sqlx::query_as::<_, AutoHighlight>(
        "UPDATE auto_highlights SET title=$1, description=$2, hashtags=$3 WHERE id=$4 AND project_id=$5 RETURNING id,start_seconds,end_seconds,title,reason,score,output_path,language,description,hashtags",
    )
    .bind(title)
    .bind(description)
    .bind(hashtags)
    .bind(highlight_id)
    .bind(project_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "That short no longer exists.".to_string())
}

#[tauri::command]
async fn delete_auto_highlight(
    project_id: Uuid,
    highlight_id: Uuid,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let output: Option<Option<String>> = sqlx::query_scalar(
        "DELETE FROM auto_highlights WHERE id=$1 AND project_id=$2 RETURNING output_path",
    )
    .bind(highlight_id)
    .bind(project_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| error.to_string())?;
    let output = output.ok_or("That short was already removed.")?;
    remove_managed_file(output, &state.export_dir).await
}

#[tauri::command]
async fn delete_media_asset(
    project_id: Uuid,
    media_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut transaction = state.db.begin().await.map_err(|error| error.to_string())?;
    let paths = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT original_path,thumbnail_path FROM media_assets WHERE project_id=$1 AND original_path=$2 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&media_path)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| error.to_string())?
    .ok_or("Media item was not found in this project.")?;
    let short_files: Vec<String> = sqlx::query_scalar(
        "SELECT output_path FROM auto_highlights WHERE project_id=$1 AND media_path=$2 AND output_path IS NOT NULL",
    )
    .bind(project_id)
    .bind(&media_path)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| error.to_string())?;
    sqlx::query("DELETE FROM media_assets WHERE project_id=$1 AND original_path=$2")
        .bind(project_id)
        .bind(&media_path)
        .execute(&mut *transaction)
        .await
        .map_err(|error| error.to_string())?;
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;

    remove_managed_file(Some(paths.0), &state.media_dir).await?;
    remove_managed_file(paths.1, &state.thumbnail_dir).await?;
    for file in short_files {
        let _ = remove_managed_file(Some(file), &state.export_dir).await;
    }
    Ok(())
}

#[tauri::command]
async fn relink_media(
    project_id: Uuid,
    media_path: String,
    replacement_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Probe, String> {
    let source = ensure_safe_file(&replacement_path)?;
    let belongs_to_project: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM media_assets WHERE project_id=$1 AND original_path=$2)",
    )
    .bind(project_id)
    .bind(&media_path)
    .fetch_one(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    if !belongs_to_project {
        return Err("Media item was not found in this project.".into());
    }
    let mut metadata = probe_media(replacement_path).await?;
    let content_sha256 = sha256_file(&source).await?;
    let is_duplicate: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM media_assets WHERE project_id=$1 AND content_sha256=$2 AND original_path<>$3)",
    )
    .bind(project_id)
    .bind(&content_sha256)
    .bind(&media_path)
    .fetch_one(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    if is_duplicate {
        return Err("That video is already linked to another item in this project.".into());
    }
    let extension = source.extension().and_then(|x| x.to_str()).unwrap_or("mp4");
    let stored = state
        .media_dir
        .join(format!("{}.{}", Uuid::new_v4(), extension));
    fs::copy(&source, &stored)
        .await
        .map_err(|e| format!("Could not copy replacement media into the local library: {e}"))?;
    let thumbnail =
        match generate_thumbnail(&stored, metadata.duration_seconds, &state.thumbnail_dir).await {
            Ok(path) => path,
            Err(error) => {
                let _ = fs::remove_file(&stored).await;
                return Err(error);
            }
        };
    let stored_text = stored.to_string_lossy().to_string();
    let thumbnail_text = thumbnail.to_string_lossy().to_string();
    let result = sqlx::query("UPDATE media_assets SET original_path=$1,file_name=$2,duration_seconds=$3,width=$4,height=$5,file_size_bytes=$6,frame_rate=$7,has_audio=$8,content_sha256=$9,thumbnail_path=$10 WHERE project_id=$11 AND original_path=$12")
        .bind(&stored_text).bind(&metadata.file_name).bind(metadata.duration_seconds)
        .bind(metadata.width as i32).bind(metadata.height as i32).bind(metadata.size_bytes as i64)
        .bind(&metadata.frame_rate).bind(metadata.has_audio).bind(&content_sha256)
        .bind(&thumbnail_text).bind(project_id).bind(&media_path).execute(&state.db).await;
    if let Err(error) = result {
        let _ = fs::remove_file(&stored).await;
        let _ = fs::remove_file(&thumbnail).await;
        return Err(format!("Could not update the media link: {error}"));
    }
    metadata.thumbnail_path = Some(thumbnail_text);
    metadata.path = stored_text;
    Ok(metadata)
}

fn build_timeline_filter_graph(
    clips: &[TimelineRenderClip],
    width: u32,
    height: u32,
    total_duration: f64,
    subtitle_path: Option<&str>,
    audio_overlays: &[AudioRenderClip],
) -> String {
    let mut filters = Vec::new();
    let has_audio = clips.iter().any(|clip| clip.has_audio) || !audio_overlays.is_empty();
    for (index, clip) in clips.iter().enumerate() {
        let duration = (clip.source_out_frame - clip.source_in_frame) as f64
            * f64::from(clip.fps_denominator)
            / f64::from(clip.fps_numerator);
        let start_seconds = clip.source_in_frame as f64 * f64::from(clip.fps_denominator)
            / f64::from(clip.fps_numerator);
        filters.push(format!(
            "[{index}:v:0]trim=start_frame={}:end_frame={},setpts=PTS-STARTPTS,scale={width}:{height}:force_original_aspect_ratio=increase,crop={width}:{height},setsar=1,fps=30,format=yuv420p[v{index}]",
            clip.source_in_frame, clip.source_out_frame
        ));
        if has_audio {
            if clip.has_audio {
                filters.push(format!(
                    "[{index}:a:0]atrim=start={start_seconds}:duration={duration},asetpts=PTS-STARTPTS,aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo[a{index}]"
                ));
            } else {
                filters.push(format!(
                    "anullsrc=r=48000:cl=stereo,atrim=duration={duration}[a{index}]"
                ));
            }
        }
    }
    let mut concat_inputs = Vec::with_capacity(clips.len() * if has_audio { 2 } else { 1 });
    for index in 0..clips.len() {
        concat_inputs.push(format!("[v{index}]"));
        if has_audio {
            concat_inputs.push(format!("[a{index}]"));
        }
    }
    if has_audio {
        filters.push(format!(
            "{}concat=n={}:v=1:a=1[vcat][acat]",
            concat_inputs.join(""),
            clips.len()
        ));
    } else {
        filters.push(format!(
            "{}concat=n={}:v=1:a=0[vcat]",
            concat_inputs.join(""),
            clips.len()
        ));
    }
    if let Some(path) = subtitle_path {
        filters.push(format!(
            "[vcat]subtitles=filename={}[vout]",
            escape_filtergraph_path(path)
        ));
    } else {
        filters.push("[vcat]null[vout]".into());
    }

    if has_audio {
        let mut audio_labels = vec!["[src]".to_string()];
        filters.push(format!(
            "[acat]volume=1.0,atrim=duration={total_duration}[src]"
        ));
        for (index, overlay) in audio_overlays.iter().enumerate() {
            let delay_samples = (overlay.timeline_start_seconds * 48_000.0).round() as u64;
            let fade_in = if overlay.fade_in_seconds > 0.0 {
                format!(",afade=t=in:st=0:d={}", overlay.fade_in_seconds)
            } else {
                String::new()
            };
            let fade_out = if overlay.fade_out_seconds > 0.0 {
                format!(
                    ",afade=t=out:st={}:d={}",
                    (overlay.duration_seconds - overlay.fade_out_seconds).max(0.0),
                    overlay.fade_out_seconds
                )
            } else {
                String::new()
            };
            filters.push(format!(
                "[{}:a:0]atrim=start={}:duration={},asetpts=PTS-STARTPTS,aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,volume={}{}{},adelay={delay_samples}S|{delay_samples}S[aud{index}]",
                overlay.input_index,
                overlay.source_in_seconds,
                overlay.duration_seconds,
                overlay.gain,
                fade_in,
                fade_out
            ));
            audio_labels.push(format!("[aud{index}]"));
        }
        if audio_labels.len() == 1 {
            filters.push("[src]alimiter=limit=0.95[aout]".into());
        } else {
            filters.push(format!(
                "{}amix=inputs={}:duration=longest:dropout_transition=0:normalize=0,alimiter=limit=0.95[aout]",
                audio_labels.join(""),
                audio_labels.len()
            ));
        }
    } else {
        filters.push(format!(
            "anullsrc=r=48000:cl=stereo,atrim=duration={total_duration}[aout]"
        ));
    }
    filters.join(";")
}

#[tauri::command]
async fn render_timeline(
    project_id: Uuid,
    aspect_ratio: String,
    voiceover_path: Option<String>,
    music_path: Option<String>,
    subtitle_path: Option<String>,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let clips = fetch_timeline(&state.db, project_id).await?;
    if clips.is_empty() {
        return Err("Add at least one clip to the timeline before rendering.".into());
    }
    if clips.len() > 300 {
        return Err("A timeline can contain at most 300 clips.".into());
    }
    let (width, height) = output_dimensions(&aspect_ratio)?;
    let voice = voiceover_path
        .map(|path| ensure_safe_file(&path))
        .transpose()?;
    let music = music_path.map(|path| ensure_safe_file(&path)).transpose()?;
    let subtitles = subtitle_path
        .map(|path| ensure_safe_file(&path))
        .transpose()?;
    let timeline_audio = fetch_timeline_audio(&state.db, project_id).await?;
    let media_root = fs::canonicalize(&state.media_dir)
        .await
        .map_err(|error| error.to_string())?;
    let mut render_clips = Vec::with_capacity(clips.len());
    let mut total_duration = 0.0;
    for clip in &clips {
        let input = fs::canonicalize(&clip.media_path)
            .await
            .map_err(|error| format!("Timeline source is unavailable: {error}"))?;
        if !input.starts_with(&media_root) || !input.is_file() {
            return Err("Timeline sources must remain inside the local media library.".into());
        }
        let (fps_numerator, fps_denominator) = parse_frame_rate(&clip.frame_rate)?;
        let frame_count =
            timeline_frame_count(clip.duration_seconds, fps_numerator, fps_denominator)
                .ok_or("Could not determine the source frame count.")?;
        if clip.source_in_frame < 0
            || clip.source_out_frame <= clip.source_in_frame
            || clip.source_out_frame > frame_count
        {
            return Err("Timeline clip frames are outside the source media.".into());
        }
        let duration = timeline_clip_duration(clip)?;
        total_duration += duration;
        render_clips.push(TimelineRenderClip {
            media_path: input,
            source_in_frame: clip.source_in_frame,
            source_out_frame: clip.source_out_frame,
            fps_numerator,
            fps_denominator,
            has_audio: clip.has_audio,
        });
    }
    if !total_duration.is_finite() || total_duration <= 0.0 || total_duration > 900.0 {
        return Err("Timeline duration must be greater than zero and at most 900 seconds.".into());
    }

    let mut args = vec!["-hide_banner".into(), "-n".into()];
    for clip in &render_clips {
        args.extend(["-i".into(), clip.media_path.to_string_lossy().to_string()]);
    }
    let mut next_input = render_clips.len();
    let mut audio_overlays = Vec::new();
    for clip in &timeline_audio {
        if clip.timeline_start_seconds >= total_duration {
            continue;
        }
        if !clip.timeline_start_seconds.is_finite()
            || !clip.source_in_seconds.is_finite()
            || !clip.duration_seconds.is_finite()
            || !clip.gain.is_finite()
            || !clip.fade_in_seconds.is_finite()
            || !clip.fade_out_seconds.is_finite()
            || clip.timeline_start_seconds < 0.0
            || clip.source_in_seconds < 0.0
            || clip.duration_seconds <= 0.0
            || clip.source_in_seconds + clip.duration_seconds
                > clip.source_duration_seconds + 0.000001
            || !(0.0..=2.0).contains(&clip.gain)
            || clip.fade_in_seconds < 0.0
            || clip.fade_out_seconds < 0.0
            || clip.fade_in_seconds + clip.fade_out_seconds > clip.duration_seconds
        {
            return Err("A saved audio timeline clip contains invalid values.".into());
        }
        let input = fs::canonicalize(&clip.path)
            .await
            .map_err(|error| format!("Timeline audio is unavailable: {error}"))?;
        if !input.starts_with(&media_root) || !input.is_file() {
            return Err("Timeline audio must remain inside the local media library.".into());
        }
        let duration = clip
            .duration_seconds
            .min(total_duration - clip.timeline_start_seconds);
        args.extend(["-i".into(), input.to_string_lossy().to_string()]);
        audio_overlays.push(AudioRenderClip {
            input_index: next_input,
            timeline_start_seconds: clip.timeline_start_seconds,
            source_in_seconds: clip.source_in_seconds,
            duration_seconds: duration,
            gain: clip.gain,
            fade_in_seconds: clip.fade_in_seconds.min(duration),
            fade_out_seconds: clip.fade_out_seconds.min(duration),
        });
        next_input += 1;
    }
    for (path, gain) in [(voice.as_ref(), 1.0), (music.as_ref(), 0.18)] {
        if let Some(path) = path {
            args.extend(["-i".into(), path.to_string_lossy().to_string()]);
            audio_overlays.push(AudioRenderClip {
                input_index: next_input,
                timeline_start_seconds: 0.0,
                source_in_seconds: 0.0,
                duration_seconds: total_duration,
                gain,
                fade_in_seconds: 0.0,
                fade_out_seconds: 0.0,
            });
            next_input += 1;
        }
    }
    let subtitle_path = subtitles
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let graph = build_timeline_filter_graph(
        &render_clips,
        width,
        height,
        total_duration,
        subtitle_path.as_deref(),
        &audio_overlays,
    );
    let output = state
        .export_dir
        .join(format!("short-{}.mp4", Uuid::new_v4()));
    args.extend([
        "-filter_complex".into(),
        graph,
        "-map".into(),
        "[vout]".into(),
        "-map".into(),
        "[aout]".into(),
        "-t".into(),
        total_duration.to_string(),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "faster".into(),
        "-crf".into(),
        "18".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-movflags".into(),
        "+faststart".into(),
        output.to_string_lossy().to_string(),
    ]);
    if let Err(error) = render_ffmpeg(&args, total_duration, &app, &state).await {
        let _ = fs::remove_file(&output).await;
        return Err(error);
    }
    if !output.is_file()
        || fs::metadata(&output)
            .await
            .map_err(|error| error.to_string())?
            .len()
            == 0
    {
        return Err("FFmpeg returned without a valid timeline output file.".into());
    }
    let first_source = clips.first().expect("non-empty clips").media_path.clone();
    if let Err(error) = sqlx::query("INSERT INTO exports (id,project_id,source_path,output_path,aspect_ratio,duration_seconds) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(Uuid::new_v4())
        .bind(project_id)
        .bind(first_source)
        .bind(output.to_string_lossy().to_string())
        .bind(aspect_ratio)
        .bind(total_duration)
        .execute(&state.db)
        .await
    {
        let _ = fs::remove_file(&output).await;
        return Err(format!("Could not save export history: {error}"));
    }
    Ok(output.to_string_lossy().to_string())
}

#[tauri::command]
async fn render_short(
    project_id: Uuid,
    input_path: String,
    start_seconds: f64,
    duration_seconds: f64,
    aspect_ratio: String,
    voiceover_path: Option<String>,
    music_path: Option<String>,
    subtitle_path: Option<String>,
    framing: Option<String>,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let input = ensure_safe_file(&input_path)?;
    if !start_seconds.is_finite()
        || start_seconds < 0.0
        || !duration_seconds.is_finite()
        || duration_seconds <= 0.0
        || duration_seconds > 900.0
    {
        return Err("Invalid start/duration. Duration must be between 0 and 900 seconds.".into());
    }
    if !matches!(aspect_ratio.as_str(), "9:16" | "1:1" | "16:9") {
        return Err("Unsupported aspect ratio.".into());
    }
    let voice = voiceover_path.map(|p| ensure_safe_file(&p)).transpose()?;
    let music = music_path.map(|p| ensure_safe_file(&p)).transpose()?;
    let srt = subtitle_path.map(|p| ensure_safe_file(&p)).transpose()?;
    let (w, h) = output_dimensions(&aspect_ratio)?;
    let source_probe = probe_media(input_path.clone()).await?;
    if !clip_fits_source(
        start_seconds,
        duration_seconds,
        source_probe.duration_seconds,
    ) {
        return Err("Selected clip extends beyond the source media duration.".into());
    }
    let belongs_to_project: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM media_assets WHERE project_id=$1 AND original_path=$2)",
    )
    .bind(project_id)
    .bind(&input_path)
    .fetch_one(&state.db)
    .await
    .map_err(|e| e.to_string())?;
    if !belongs_to_project {
        return Err("The selected source is not in this project’s media library.".into());
    }
    let output = state
        .export_dir
        .join(format!("short-{}.mp4", Uuid::new_v4()));
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-n".into(),
        "-ss".into(),
        start_seconds.to_string(),
        "-i".into(),
        input.to_string_lossy().to_string(),
    ];
    let mut next_input = 1usize;
    let voice_index = if let Some(ref p) = voice {
        args.extend(["-i".into(), p.to_string_lossy().to_string()]);
        let index = next_input;
        next_input += 1;
        Some(index)
    } else {
        None
    };
    let music_index = if let Some(ref p) = music {
        args.extend(["-i".into(), p.to_string_lossy().to_string()]);
        let index = next_input;
        Some(index)
    } else {
        None
    };
    // Cut baked-in black bars (cinema letterbox) first, so the 9:16 crop is all picture.
    let bars = detect_black_bars(&input, start_seconds, duration_seconds)
        .await
        .map(|crop| format!("{crop},"))
        .unwrap_or_default();
    let mut follow_script: Option<PathBuf> = None;
    let framing = if framing.as_deref() == Some("smart") {
        // Picture size after black bars are removed decides how wide a 9:16 window is.
        let (source_w, source_h) = match bars.trim_end_matches(',').strip_prefix("crop=").map(|v| v.split(':').filter_map(|n| n.parse::<f64>().ok()).collect::<Vec<_>>()) {
            Some(numbers) if numbers.len() == 4 => (numbers[0], numbers[1]),
            _ => probe_media(input.to_string_lossy().to_string()).await.map(|p| (p.width as f64, p.height as f64)).unwrap_or((16.0, 9.0)),
        };
        let window = (source_h * w as f64 / h as f64) / source_w.max(1.0);
        let plans = smart_framing(&input, start_seconds, duration_seconds, &bars, window).await;
        let script = std::env::temp_dir().join(format!("shortforge-follow-{}.txt", Uuid::new_v4()));
        let (graph, commands) = shot_filter(&plans, &bars, w, h, (source_w, source_h), &script.to_string_lossy());
        if !commands.is_empty() {
            fs::write(&script, commands).await.map_err(|error| format!("Could not prepare face tracking: {error}"))?;
            follow_script = Some(script);
        }
        Some(graph)
    } else {
        None
    }
    .map(|graph| ("smart".to_string(), graph))
    .or(framing.map(|f| (f, String::new())));
    let vf_base = if let Some((_, graph)) = framing.as_ref().filter(|(kind, _)| kind == "smart") {
        graph.clone()
    } else if framing.as_ref().map(|(kind, _)| kind.as_str()) == Some("fit") {
        // Whole picture, centred, over a blurred zoom of itself: nobody in the shot is cut off.
        format!(
            "{bars}split=2[bgsrc][fgsrc];[bgsrc]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},boxblur=24:2,eq=brightness=-0.12[bg];[fgsrc]scale={w}:{h}:force_original_aspect_ratio=decrease[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1,fps=30"
        )
    } else {
        format!("{bars}scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},setsar=1,fps=30")
    };
    let vf = if let Some(ref sub) = srt {
        let escaped = escape_filtergraph_path(&sub.to_string_lossy());
        format!("{vf_base},subtitles=filename={escaped}")
    } else {
        vf_base
    };
    args.extend(["-t".into(), duration_seconds.to_string(), "-vf".into(), vf]);
    args.extend(["-map".into(), "0:v:0".into()]);
    if let Some(graph) = audio_filter_graph(
        source_probe.has_audio,
        voice_index,
        music_index,
        duration_seconds,
    ) {
        args.extend([
            "-filter_complex".into(),
            graph,
            "-map".into(),
            "[aout]".into(),
        ]);
    } else if source_probe.has_audio {
        args.extend([
            "-map".into(),
            "0:a:0".into(),
            "-af".into(),
            "loudnorm=I=-16:LRA=11:TP=-1.5".into(),
        ]);
    }
    args.extend([
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "faster".into(),
        "-crf".into(),
        "18".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-movflags".into(),
        "+faststart".into(),
        output.to_string_lossy().to_string(),
    ]);
    let rendered = render_ffmpeg(&args, duration_seconds, &app, &state).await;
    if let Some(script) = follow_script {
        let _ = fs::remove_file(script).await;
    }
    if let Err(error) = rendered {
        let _ = fs::remove_file(&output).await;
        return Err(error);
    }
    if !output.is_file()
        || fs::metadata(&output)
            .await
            .map_err(|e| e.to_string())?
            .len()
            == 0
    {
        return Err("FFmpeg returned without a valid output file.".into());
    }
    if let Err(error) = sqlx::query("INSERT INTO exports (id,project_id,source_path,output_path,aspect_ratio,duration_seconds) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(Uuid::new_v4()).bind(project_id).bind(input_path).bind(output.to_string_lossy().to_string()).bind(aspect_ratio).bind(duration_seconds)
        .execute(&state.db).await {
            let _ = fs::remove_file(&output).await;
            return Err(format!("Could not save export history: {error}"));
        }
    Ok(output.to_string_lossy().to_string())
}

#[tauri::command]
async fn is_job_running(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.render_cancel.lock().await.is_some())
}

#[tauri::command]
async fn cancel_render(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let active = state.render_cancel.lock().await;
    let sender = active.as_ref().ok_or("No render is currently running.")?;
    sender
        .send(true)
        .map_err(|_| "The render process is no longer available.".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        audio_filter_graph, build_timeline_filter_graph, clip_fits_source, escape_filtergraph_path,
        output_dimensions, parse_frame_rate, parse_scene_cut_output, parse_silence_output,
        timeline_frame_count, speaker_track, parse_heatmap, replay_lift, loudness_lift, parse_loudness, choose_framing, follow_track, plan_shots, shot_filter, track_position, ShotPlan, hook_title, parse_cropdetect, FaceBox, Framing, social_hashtags, whisper_language, pick_highlights, validate_video_url, whisper_segments, AudioRenderClip,
        TimelineRenderClip, TranscriptSegment,
    };

    #[test]
    fn face_model_loads_and_runs_when_present() {
        let path = super::tools_dir().join("models").join("version-RFB-320.onnx");
        if !path.is_file() {
            return; // model is a local download; nothing to check without it
        }
        let model = tract_onnx::onnx()
            .model_for_path(&path)
            .and_then(|m| m.with_input_fact(0, f32::fact([1, 3, 240, 320]).into()))
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable());
        let model = model.unwrap_or_else(|error| panic!("face model failed to load: {error:?}"));
        let blank = vec![128u8; 320 * 240 * 3];
        assert!(super::detect_faces(&model, &blank).is_empty());
    }

    #[test]
    fn framing_follows_faces_and_falls_back_to_fit_only_when_people_spread_out() {
        let face = |x1: f32, x2: f32| FaceBox { x1, y1: 0.3, x2, y2: 0.5, score: 0.9 };
        let window = 0.3164; // 9:16 window inside a 16:9 frame
        // One person on the right: crop moves right and keeps them in.
        let Framing::Crop(position) = choose_framing(&vec![vec![face(0.70, 0.80)]; 10], window) else { panic!("expected crop") };
        let left = position * (1.0 - window);
        assert!(left <= 0.70 && left + window >= 0.80, "window {left}..{} misses the face", left + window);
        // Two people at opposite edges in most frames: show the whole frame.
        assert_eq!(choose_framing(&vec![vec![face(0.05, 0.15), face(0.85, 0.95)]; 10], window), Framing::Fit);
        // A tiny background face does not pull the crop away from the main speaker.
        let Framing::Crop(position) = choose_framing(&vec![vec![face(0.40, 0.55), FaceBox { x1: 0.95, y1: 0.1, x2: 0.97, y2: 0.12, score: 0.9 }]; 10], window) else { panic!("expected crop") };
        let left = position * (1.0 - window);
        assert!(left <= 0.40 && left + window >= 0.55);
        // No faces: centre crop.
        assert_eq!(choose_framing(&vec![vec![]; 5], window), Framing::Crop(0.5));
    }

    #[test]
    fn each_shot_gets_its_own_framing_and_fit_only_where_needed() {
        let face = |x1: f32, x2: f32| FaceBox { x1, y1: 0.3, x2, y2: 0.5, score: 0.9 };
        let window = 0.3164;
        let mut samples = Vec::new();
        for i in 0..8 { samples.push((i as f64 * 0.5, vec![face(0.50, 0.61)])); }          // one person, 0-4 s
        for i in 8..16 { samples.push((i as f64 * 0.5, vec![face(0.20, 0.31), face(0.66, 0.80)])); } // two apart, 4-8 s
        let plans = plan_shots(&[4.0], 8.0, &samples, window);
        // The same change is found from the faces alone when scene detection misses the cut.
        let from_faces = plan_shots(&[], 8.0, &samples, window);
        assert_eq!(from_faces.len(), 2);
        assert_eq!(from_faces[1].framing, Framing::Fit);
        assert!((from_faces[1].start - 3.75).abs() < 0.01);
        assert_eq!(plans.len(), 2);
        assert!(matches!(plans[0].framing, Framing::Crop(_)));
        assert_eq!(plans[1], ShotPlan { start: 4.0, end: 8.0, framing: Framing::Fit, track: vec![] });
        let (graph, commands) = shot_filter(&plans, "", 1080, 1920, (1920.0, 1080.0), "C:/tmp/f.txt");
        assert!(graph.contains("enable='between(t\\,4.000\\,8.000)'"), "{graph}");
        assert!(graph.contains("crop@track=w=606:h=1080") && commands.starts_with("0.00 crop@track x "), "{graph}\n{commands}");
        // Same framing on both sides of a cut merges into one shot and a plain crop.
        let steady = plan_shots(&[3.0], 6.0, &vec![(0.0, vec![face(0.5, 0.6)]), (4.0, vec![face(0.5, 0.6)])], window);
        assert_eq!(steady.len(), 1);
        assert!(!shot_filter(&steady, "", 1080, 1920, (1920.0, 1080.0), "f.txt").0.contains("split"));
    }

    #[test]
    fn tracking_follows_a_moving_person_smoothly() {
        let face = |x1: f32| FaceBox { x1, y1: 0.3, x2: x1 + 0.08, y2: 0.5, score: 0.9 };
        let window = 0.3164;
        // Walks from the left third to the right third over 10 s, face lost for two samples.
        let samples: Vec<(f64, Vec<FaceBox>)> = (0..21)
            .map(|i| (i as f64 * 0.5, if i == 9 || i == 10 { vec![] } else { vec![face(0.15 + i as f32 * 0.03)] }))
            .collect();
        let track = follow_track(&samples, window, 0.5);
        assert!(track.first().unwrap().1 < 0.2 && track.last().unwrap().1 > 0.75, "{track:?}");
        assert!(track.windows(2).all(|pair| pair[1].1 >= pair[0].1 - 1e-9), "never jumps back: {track:?}");
        assert!(track.windows(2).all(|pair| (pair[1].1 - pair[0].1).abs() < 0.12), "no big jumps: {track:?}");
        assert!((track_position(&[(0.0, 0.2), (1.0, 0.4)], 0.5) - 0.3).abs() < 1e-9);
        // A person standing still gives one steady position.
        let still = follow_track(&vec![(0.0, vec![face(0.5)]); 6], window, 0.5);
        assert!(still.windows(2).all(|pair| pair[0].1 == pair[1].1));
    }

    #[test]
    fn loud_moments_score_higher_than_the_video_usual_level() {
        let log = "frame:0    pts:0       pts_time:0.1\nlavfi.r128.M=-24.5\nframe:1 pts:1 pts_time:0.2\nlavfi.r128.M=-inf\nframe:2 pts:2 pts_time:0.3\nlavfi.r128.M=-15.0\n";
        assert_eq!(parse_loudness(log), vec![(0.1, -24.5), (0.3, -15.0)]);
        let level: Vec<(f64, f64)> = (0..100).map(|i| (i as f64, if (40..60).contains(&i) { -14.0 } else { -23.0 })).collect();
        assert!(loudness_lift(&level, -23.0, 40.0, 60.0) > 0.99);
        assert_eq!(loudness_lift(&level, -23.0, 0.0, 20.0), 0.0);
        // The same words win when they are also the loud part of the video.
        let segment = |start: f64, text: &str| TranscriptSegment { start, end: start + 10.0, text: text.into() };
        let speech = vec![segment(0.0, "This is a normal sentence here."), segment(10.0, "And another normal one follows."),
                          segment(40.0, "This is a normal sentence here."), segment(50.0, "And another normal one follows.")];
        let picks = pick_highlights(&speech, 100.0, 1, "en", &level, &[]);
        assert_eq!(picks[0].start_seconds, 40.0);
        assert!(picks[0].reason.contains("louder"));
        // The quieter copy of the same lines is not picked as a second short.
        assert_eq!(pick_highlights(&speech, 100.0, 2, "en", &level, &[]).len(), 1);
    }

    #[test]
    fn the_crop_follows_whoever_is_talking() {
        let (w, h) = (320usize, 240usize);
        let left = FaceBox { x1: 0.10, y1: 0.30, x2: 0.25, y2: 0.60, score: 0.9 };
        let right = FaceBox { x1: 0.75, y1: 0.30, x2: 0.90, y2: 0.60, score: 0.9 };
        // Paint a face's mouth area brighter on odd frames: that face is talking.
        let frame = |index: usize, talking: &FaceBox| {
            let mut pixels = vec![100u8; w * h * 3];
            if index % 2 == 1 {
                for y in ((talking.y1 + 0.65 * 0.30) * h as f32) as usize..((talking.y2) * h as f32) as usize {
                    for x in ((talking.x1 + 0.04) * w as f32) as usize..((talking.x2 - 0.04) * w as f32) as usize {
                        pixels[(y * w + x) * 3 + 1] = 180;
                    }
                }
            }
            pixels
        };
        // Left person talks for 4 s, then the right person for 4 s, 5 frames a second.
        let frames: Vec<(f64, Vec<u8>, Vec<FaceBox>)> = (0..40)
            .map(|i| (i as f64 * 0.2, frame(i, if i < 20 { &left } else { &right }), vec![left, right]))
            .collect();
        let window = 0.3164;
        let track = speaker_track(&frames, window).expect("a speaker is found");
        let left_position = track.iter().find(|(t, _)| *t >= 1.0).unwrap().1;
        let right_position = track.iter().rev().find(|(t, _)| *t <= 7.5).unwrap().1;
        assert!(left_position < 0.15, "starts on the left speaker: {track:?}");
        assert!(right_position > 0.85, "ends on the right speaker: {track:?}");
        // Nobody moving their mouth: no speaker, the shot stays as it was.
        let silent: Vec<(f64, Vec<u8>, Vec<FaceBox>)> = (0..10).map(|i| (i as f64 * 0.2, vec![100u8; w * h * 3], vec![left, right])).collect();
        assert!(speaker_track(&silent, window).is_none());
    }

    #[test]
    fn english_uses_the_fast_model_other_languages_the_accurate_one() {
        use std::path::Path;
        let (fast, accurate) = (Path::new("base.bin"), Path::new("turbo.bin"));
        assert_eq!(super::choose_whisper_model(Some("en"), fast, Some(accurate)), fast);
        assert_eq!(super::choose_whisper_model(Some("ml"), fast, Some(accurate)), accurate);
        assert_eq!(super::choose_whisper_model(None, fast, Some(accurate)), accurate);
        assert_eq!(super::choose_whisper_model(Some("ml"), fast, None), fast);
    }

    #[test]
    fn most_replayed_part_wins() {
        let json = r#"[{"start_time":0.0,"end_time":10.0,"value":0.1},{"start_time":10.0,"end_time":20.0,"value":1.0},{"start_time":20.0,"end_time":30.0,"value":0.2}]"#;
        let replay = parse_heatmap(json);
        assert_eq!(replay.len(), 3);
        assert!(parse_heatmap("NA").is_empty());
        assert!(replay_lift(&replay, 10.0, 20.0) > 0.99);
        assert_eq!(replay_lift(&replay, 0.0, 10.0), 0.0);
        let segment = |start: f64, text: &str| TranscriptSegment { start, end: start + 10.0, text: text.into() };
        let speech = vec![segment(0.0, "First plain sentence."), segment(10.0, "Second plain sentence."), segment(20.0, "Third plain sentence."),
                          segment(30.0, "Fourth plain sentence."), segment(40.0, "Fifth plain sentence."), segment(50.0, "Sixth plain sentence.")];
        let heat = vec![(0.0, 30.0, 0.1), (30.0, 60.0, 0.9)];
        let picks = pick_highlights(&speech, 60.0, 1, "en", &[], &heat);
        assert!(picks[0].start_seconds >= 30.0, "picked {}", picks[0].start_seconds);
        assert!(picks[0].reason.starts_with("most replayed"));
    }

    #[test]
    fn social_copy_uses_the_clip_words_and_language() {
        let tags = social_hashtags("Saving money is easy. Money habits beat money hacks. Saving daily!", "en");
        assert!(tags.starts_with("#money #saving"), "{tags}");
        assert!(tags.contains("#english"));
        let tamil = social_hashtags("பணம் சேமிப்பு பணம் எளிது", "ta");
        assert!(tamil.starts_with("#பணம்"), "{tamil}");
        assert!(tamil.contains("#tamil"));
        assert_eq!(hook_title("- Why does this work? Because it does."), "Why does this work?");
        assert_eq!(whisper_language(r#"{"result":{"language":"ta"},"transcription":[]}"#), "ta");
        assert_eq!(whisper_language(r#"{"transcription":[]}"#), "");
    }

    #[test]
    fn cropdetect_takes_the_last_reported_crop() {
        let log = "[Parsed_cropdetect_0] x1:0 crop=1920:816:0:132
[Parsed_cropdetect_0] x1:0 crop=1920:800:0:140
";
        assert_eq!(parse_cropdetect(log).as_deref(), Some("crop=1920:800:0:140"));
        assert_eq!(parse_cropdetect("no crop here"), None);
        assert_eq!(parse_cropdetect("crop=-16:0:8:8"), None);
        assert_eq!(parse_cropdetect("crop=32:32:0:0"), None);
    }

    #[test]
    fn video_links_must_be_full_http_urls() {
        assert_eq!(validate_video_url("  https://www.youtube.com/watch?v=abc123 ").unwrap(), "https://www.youtube.com/watch?v=abc123");
        assert!(validate_video_url("https://youtu.be/abc123").is_ok());
        for bad in ["", "youtube.com/watch?v=1", "--exec calc", "file:///C:/x.mp4", "https://", "https:///x", "https://a b.com", "ftp://x.com/v"] {
            assert!(validate_video_url(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn whisper_json_becomes_segments_in_seconds() {
        let json = r#"{"transcription":[{"offsets":{"from":0,"to":2500},"text":" Hello there."},{"offsets":{"from":2500,"to":3000},"text":"  "},{"offsets":{"from":3000,"to":6100},"text":" Big news."}]}"#;
        let segments = whisper_segments(json).unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1], TranscriptSegment { start: 3.0, end: 6.1, text: "Big news.".into() });
        assert!(whisper_segments("not json").is_err());
        let tags = r#"{"transcription":[{"offsets":{"from":0,"to":900},"text":" [Music]"},{"offsets":{"from":900,"to":1800},"text":" (speaking in foreign language)"},{"offsets":{"from":1800,"to":2400},"text":" - Fits?"},{"offsets":{"from":2400,"to":3000},"text":" ♪ ♪"}]}"#;
        let kept = whisper_segments(tags).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "- Fits?");
        assert!(super::is_repetition_loop("ༀༀༀༀༀༀༀༀༀༀༀༀༀༀༀༀༀༀ"));
        assert!(super::is_repetition_loop("the the the the the the the the"));
        assert!(!super::is_repetition_loop("Why does nobody talk about this secret?"));
        assert!(!super::is_repetition_loop("நான் பிருத்தையும் ஒரு சம்சியில்லை."));
    }

    #[test]
    fn highlight_picker_prefers_hooks_and_never_overlaps() {
        let segment = |start: f64, text: &str| TranscriptSegment { start, end: start + 10.0, text: text.into() };
        let segments = vec![
            segment(0.0, "so um yeah"),
            segment(10.0, "and then"),
            segment(20.0, "ok."),
            segment(30.0, "Why does nobody talk about this secret?"),
            segment(40.0, "It changes everything you know about saving money!"),
            segment(50.0, "Here is the proof, step by step, right now."),
            segment(60.0, "That is the whole trick."),
        ];
        let clips = pick_highlights(&segments, 70.0, 3, "en", &[], &[]);
        assert!(!clips.is_empty());
        assert_eq!(clips[0].start_seconds, 30.0);
        assert!(clips[0].reason.contains("hook"));
        for (i, a) in clips.iter().enumerate() {
            assert!((15.0..=60.0).contains(&(a.end_seconds - a.start_seconds)));
            for b in &clips[i + 1..] {
                assert!(a.end_seconds <= b.start_seconds || a.start_seconds >= b.end_seconds);
            }
        }
        assert!(pick_highlights(&segments[..1], 10.0, 3, "en", &[], &[]).is_empty());
        assert_eq!(clips[0].title, "Why does nobody talk about this secret?");
        assert_eq!(clips[0].language, "en");
        assert!(clips[0].hashtags.contains("#english") && clips[0].hashtags.ends_with("#shorts #reels #fyp"));
    }
    use std::path::PathBuf;
    use tract_onnx::prelude::*;

    #[test]
    fn output_aspects_have_expected_dimensions() {
        assert_eq!(output_dimensions("9:16").unwrap(), (1080, 1920));
        assert_eq!(output_dimensions("1:1").unwrap(), (1080, 1080));
        assert_eq!(output_dimensions("16:9").unwrap(), (1920, 1080));
        assert!(output_dimensions("4:3").is_err());
    }

    #[test]
    fn clip_range_must_be_finite_and_within_source() {
        assert!(clip_fits_source(2.0, 4.0, 6.0));
        assert!(!clip_fits_source(2.0, 4.1, 6.0));
        assert!(!clip_fits_source(-1.0, 1.0, 6.0));
        assert!(!clip_fits_source(0.0, f64::INFINITY, 6.0));
    }

    #[test]
    fn external_audio_streams_use_their_actual_input_indexes() {
        let graph = audio_filter_graph(true, Some(1), Some(2), 8.0).unwrap();
        assert!(graph.contains("[0:a:0]"));
        assert!(graph.contains("[1:a:0]"));
        assert!(graph.contains("[2:a:0]"));
        assert!(graph.contains("amix=inputs=3"));
        assert!(graph.contains("[music];[src][voice][music]amix"));
    }

    #[test]
    fn single_audio_stream_reuses_its_output_label_in_a_new_chain() {
        let graph = audio_filter_graph(true, None, None, 2.0).unwrap();
        assert!(graph.contains("[src];[src]anull"));
        assert!(!graph.contains("[src][src]"));
    }

    #[test]
    fn scene_cut_parser_collapses_nearby_candidates_to_the_strongest() {
        let output = "frame:0 pts:512 pts_time:0.0333333\nlavfi.scene_score=0.025029\nframe:1 pts:1024 pts_time:0.0666667\nlavfi.scene_score=0.023001\nframe:2 pts:30720 pts_time:2\nlavfi.scene_score=0.026842\n";
        let cuts = parse_scene_cut_output(output);
        assert_eq!(cuts.len(), 2);
        assert!((cuts[0].0 - 0.0333333).abs() < 0.000001);
        assert!((cuts[0].1 - 0.025029).abs() < 0.000001);
        assert_eq!(cuts[1], (2.0, 0.026842));
    }

    #[test]
    fn scene_cut_grouping_does_not_chain_across_long_sequences() {
        let output = "frame:0 pts:1 pts_time:0.1\nlavfi.scene_score=0.3\nframe:1 pts:2 pts_time:0.2\nlavfi.scene_score=0.4\nframe:2 pts:3 pts_time:0.3\nlavfi.scene_score=0.2\n";
        assert_eq!(parse_scene_cut_output(output), vec![(0.2, 0.4), (0.3, 0.2)]);
    }

    #[test]
    fn silence_parser_clips_ranges_and_closes_trailing_silence() {
        let output = "[silencedetect @ 0x1] silence_start: 0.000\n[silencedetect @ 0x1] silence_end: 1.200 | silence_duration: 1.200\n[silencedetect @ 0x1] silence_start: 4.000\n";
        assert_eq!(
            parse_silence_output(output, 5.0, 0.5),
            vec![(0.0, 1.2), (4.0, 5.0)]
        );
    }

    #[test]
    fn silence_parser_discards_short_or_invalid_ranges() {
        let output = "silence_start: 1.0\nsilence_end: 1.2 | silence_duration: 0.2\nsilence_start: NaN\nsilence_end: 3.0 | silence_duration: 0.5\n";
        assert!(parse_silence_output(output, 4.0, 0.5).is_empty());
    }

    #[test]
    fn subtitle_paths_escape_filtergraph_syntax() {
        let escaped = escape_filtergraph_path("E:\\Media\\sub,cap's[1].srt");
        assert!(escaped.contains("\\\\:"));
        assert!(escaped.contains("\\,"));
        assert!(escaped.contains("\\\\\\'"));
        assert!(escaped.contains("\\["));
    }

    #[test]
    fn frame_rates_and_source_frame_counts_are_exact() {
        assert_eq!(parse_frame_rate("30000/1001").unwrap(), (30000, 1001));
        assert_eq!(parse_frame_rate("30").unwrap(), (30, 1));
        assert!(parse_frame_rate("0/0").is_err());
        assert_eq!(timeline_frame_count(10.0, 30000, 1001), Some(300));
        assert_eq!(timeline_frame_count(f64::NAN, 30, 1), None);
    }

    #[test]
    fn timeline_graph_trims_concatenates_and_fills_silent_clips() {
        let clips = vec![
            TimelineRenderClip {
                media_path: PathBuf::from("source.mp4"),
                source_in_frame: 30,
                source_out_frame: 90,
                fps_numerator: 30,
                fps_denominator: 1,
                has_audio: true,
            },
            TimelineRenderClip {
                media_path: PathBuf::from("silent.mp4"),
                source_in_frame: 0,
                source_out_frame: 60,
                fps_numerator: 30,
                fps_denominator: 1,
                has_audio: false,
            },
        ];
        let graph = build_timeline_filter_graph(&clips, 1080, 1920, 4.0, None, &[]);
        assert!(graph.contains("trim=start_frame=30:end_frame=90"));
        assert!(graph.contains("anullsrc=r=48000:cl=stereo"));
        assert!(graph.contains("concat=n=2:v=1:a=1"));
        assert!(graph.contains("[vcat]null[vout]"));
        assert!(graph.contains("[src]alimiter=limit=0.95[aout]"));
    }

    #[test]
    fn silent_only_timeline_does_not_create_nan_audio() {
        let clips = [TimelineRenderClip {
            media_path: PathBuf::from("silent.mp4"),
            source_in_frame: 0,
            source_out_frame: 30,
            fps_numerator: 30,
            fps_denominator: 1,
            has_audio: false,
        }];
        let graph = build_timeline_filter_graph(&clips, 1080, 1920, 1.0, None, &[]);
        assert!(graph.contains("concat=n=1:v=1:a=0[vcat]"));
        assert!(graph.contains("anullsrc=r=48000:cl=stereo,atrim=duration=1[aout]"));
        assert!(!graph.contains("loudnorm"));
    }

    #[test]
    fn positioned_audio_clips_apply_trim_gain_fades_and_delay() {
        let clips = [TimelineRenderClip {
            media_path: PathBuf::from("source.mp4"),
            source_in_frame: 0,
            source_out_frame: 90,
            fps_numerator: 30,
            fps_denominator: 1,
            has_audio: false,
        }];
        let audio = [AudioRenderClip {
            input_index: 1,
            timeline_start_seconds: 0.5,
            source_in_seconds: 1.0,
            duration_seconds: 2.0,
            gain: 0.7,
            fade_in_seconds: 0.2,
            fade_out_seconds: 0.3,
        }];
        let graph = build_timeline_filter_graph(&clips, 1080, 1920, 3.0, None, &audio);
        assert!(graph.contains("[1:a:0]atrim=start=1:duration=2"));
        assert!(graph.contains("volume=0.7"));
        assert!(graph.contains("afade=t=in:st=0:d=0.2"));
        assert!(graph.contains("afade=t=out:st=1.7:d=0.3"));
        assert!(graph.contains("adelay=24000S|24000S"));
        assert!(graph.contains("amix=inputs=2"));
    }
}
#[tauri::command]
async fn generate_voiceover(
    text: String,
    voice_name: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    if text.trim().is_empty() || text.len() > 12000 {
        return Err("Narration must contain 1–12,000 characters.".into());
    }
    let out = state
        .media_dir
        .join(format!("voiceover-{}.wav", Uuid::new_v4()));
    let script = state.media_dir.join(format!("tts-{}.ps1", Uuid::new_v4()));
    let voice_literal = voice_name.unwrap_or_default().replace("'", "''");
    let text_literal = text.replace("'", "''");
    let ps = format!(
        r#"
Add-Type -AssemblyName System.Speech
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
if ('{voice_literal}' -ne '') {{ $s.SelectVoice('{voice_literal}') }}
$s.SetOutputToWaveFile('{}')
$s.Speak('{}')
$s.Dispose()
"#,
        out.to_string_lossy().replace("'", "''"),
        text_literal
    );
    fs::write(&script, ps).await.map_err(|e| e.to_string())?;
    let result = tool_command("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&script).await;
    if !result.status.success() {
        return Err(String::from_utf8_lossy(&result.stderr).to_string());
    }
    if !out.is_file() {
        return Err("Windows speech synthesis did not create a WAV file.".into());
    }
    Ok(out.to_string_lossy().to_string())
}
pub fn run() {
    tauri::Builder::default().plugin(tauri_plugin_dialog::init()).setup(|app| {
        let base=app_data_dir().map_err(std::io::Error::other)?;
        let media_dir=base.join("media"); let thumbnail_dir=base.join("thumbnails"); let export_dir=base.join("exports");
        std::fs::create_dir_all(&media_dir)?; std::fs::create_dir_all(&thumbnail_dir)?; std::fs::create_dir_all(&export_dir)?;
        let db_url=std::env::var("DATABASE_URL").unwrap_or_else(|_|DEFAULT_DB.into());
        let app_handle=app.handle().clone();
        tauri::async_runtime::block_on(async move {
            let mut connected=false;
            for _ in 0..30 {
                if let Ok(pool)=PgPoolOptions::new().max_connections(8).connect(&db_url).await {
                    sqlx::migrate!("./migrations").run(&pool).await.map_err(std::io::Error::other)?;
                    app_handle.manage(AppState{db:pool,media_dir,thumbnail_dir,export_dir,render_cancel:Arc::new(Mutex::new(None))});
                    connected=true; break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            if !connected { return Err(std::io::Error::other("PostgreSQL is unavailable. Start Docker Desktop and run docker compose up -d postgres.").into()); }
            Ok::<(),Box<dyn std::error::Error>>(())
        })?;
        let web_api_app = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            if let Err(error) = run_local_web_api(web_api_app).await {
                eprintln!("ShortForge browser API unavailable: {error}");
            }
        });
        if std::env::var_os("SHORTFORGE_BROWSER_MODE").is_some() {
            if let Some(window) = app.get_webview_window("main") {
                window.hide()?;
            }
        }
        Ok(())
    }).invoke_handler(tauri::generate_handler![list_projects,list_project_media,get_media_thumbnail,list_scene_cuts,detect_scene_cuts,list_auto_highlights,find_auto_highlights,render_auto_highlight,update_auto_highlight_text,is_job_running,delete_auto_highlight,delete_project,list_silence_ranges,detect_silence_ranges,list_timeline,save_timeline,import_audio,list_project_audio,list_timeline_audio,save_timeline_audio,create_project,probe_media,import_url,import_media,set_media_category,delete_media_asset,relink_media,render_timeline,render_short,cancel_render,generate_voiceover])
    .run(tauri::generate_context!()).expect("error while running ShortForge");
}
