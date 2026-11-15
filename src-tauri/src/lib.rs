use serde::Serialize;
use std::sync::Arc;
use tauri::Emitter;
use tauri::Manager;

mod downloader;
mod media;
mod server;
mod sidecar;
mod transcriber;

use downloader::{emit_pipeline, Downloader, EDIT_RESOLUTION};
use media::EditSource;

#[derive(Debug, Clone, Serialize)]
pub struct PipelineProgress {
    pub video_id: String,
    pub stage: String,
    pub message: String,
    pub percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Timepoint {
    pub char_idx: usize,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptData {
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    pub timepoints: Vec<Timepoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportProgress {
    pub percent: f64,
    pub status: String,
}

/// Seconds of context downloaded on each side of a transcript segment.
const SECTION_PAD_SECS: f64 = 10.0;
/// Seconds of slack around the clip when downloading at export quality.
const EXPORT_PAD_SECS: f64 = 3.0;
/// How much "Extend section" adds to one side of the editing section.
const EXTEND_SECS: u64 = 10;

fn cache_subdir(app: &tauri::AppHandle, name: &str) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_cache_dir().map_err(|e| e.to_string())?.join(name);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

// ── process_video: metadata + captions (no video download) ────────────────

/// Fetch title/duration and, if the video has English captions, the
/// transcript. When there is no usable transcript, returns
/// `transcript_path: null` and a `caption_note` explaining why; the UI then
/// offers whisper transcription or manual trimming.
#[tauri::command]
async fn process_video(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
) -> Result<serde_json::Value, String> {
    eprintln!("[better-clipper] process_video id={video_id} url={url}");
    let videos_dir = cache_subdir(&app, "videos")?;
    let transcripts_dir = cache_subdir(&app, "transcripts")?;
    let videos_dir_s = videos_dir.to_string_lossy().to_string();

    emit_pipeline(&app, &video_id, "fetching_transcript", "Reading video info...", 10.0);

    let info = Downloader::fetch_info(&app, &videos_dir_s, &video_id, &url)
        .await
        .map_err(|e| format!("Couldn't load this video. {e}"))?;

    let mut transcript_path: Option<String> = None;
    let caption_note: Option<String> = match &info.caption_track {
        Some((lang, _)) => {
            match Downloader::fetch_captions(&app, &videos_dir_s, &video_id, &info).await {
                Ok(vtt) => {
                    let dest = transcripts_dir.join(format!("{}.vtt", video_id));
                    std::fs::copy(&vtt, &dest).map_err(|e| e.to_string())?;
                    transcript_path = Some(dest.to_string_lossy().to_string());
                    None
                }
                Err(e) => Some(format!(
                    "This video has captions ({lang}) but they couldn't be downloaded: {e}"
                )),
            }
        }
        None if info.available_langs.is_empty() => Some("This video has no captions.".into()),
        None => Some(format!(
            "This video has no English captions (available: {}).",
            info.available_langs.join(", ")
        )),
    };

    let done_msg = if transcript_path.is_some() {
        "Transcript ready — select a segment to preview."
    } else {
        "No usable captions."
    };
    emit_pipeline(&app, &video_id, "done", done_msg, 100.0);

    Ok(serde_json::json!({
        "video_id": video_id,
        "url": url,
        "title": info.title,
        "duration": info.duration,
        "transcript_path": transcript_path,
        "transcript_source": transcript_path.as_ref().map(|_| "youtube"),
        "caption_note": caption_note,
    }))
}

// ── transcribe_video: whisper fallback (audio only) ───────────────────────

#[tauri::command]
async fn transcribe_video(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
) -> Result<serde_json::Value, String> {
    let videos_dir = cache_subdir(&app, "videos")?;
    let audio_dir = cache_subdir(&app, "audio")?;
    let transcripts_dir = cache_subdir(&app, "transcripts")?;

    emit_pipeline(&app, &video_id, "download", "Downloading audio...", 5.0);
    let audio = Downloader::fetch_audio(&app, &videos_dir.to_string_lossy(), &video_id, &url)
        .await
        .map_err(|e| format!("Audio download failed: {e}"))?;

    emit_pipeline(&app, &video_id, "extract_audio", "Converting audio...", 32.0);
    let wav_path = audio_dir.join(format!("{}.wav", &video_id));
    transcriber::Transcriber::extract_audio(&app, &audio, &wav_path.to_string_lossy())
        .await
        .map_err(|e| format!("Audio extraction failed: {e}"))?;

    emit_pipeline(&app, &video_id, "ensure_model", "Checking whisper model...", 40.0);
    let model_path = transcriber::Transcriber::ensure_model(&app)
        .await
        .map_err(|e| format!("Whisper model setup failed: {e}"))?;

    emit_pipeline(&app, &video_id, "transcribe", "Transcribing with whisper...", 55.0);
    let output_prefix = transcripts_dir.join(&video_id).to_string_lossy().to_string();
    let (app_c, vid) = (app.clone(), video_id.clone());
    let vtt_path = transcriber::Transcriber::transcribe(
        &app,
        &model_path,
        &wav_path.to_string_lossy(),
        &output_prefix,
        Arc::new(move |pct| {
            let msg = format!("Transcribing with whisper... {pct:.0}%");
            emit_pipeline(&app_c, &vid, "transcribe", &msg, 55.0 + pct * 0.44);
        }),
    )
    .await
    .map_err(|e| format!("Transcription failed: {e}"))?;
    let _ = std::fs::remove_file(&wav_path);

    emit_pipeline(&app, &video_id, "done", "Transcription complete.", 100.0);

    Ok(serde_json::json!({
        "transcript_path": vtt_path,
        "transcript_source": "whisper",
    }))
}

// ── read_transcript: parse VTT into searchable text + timepoints ──────────

#[tauri::command]
fn read_transcript(path: String) -> Result<TranscriptData, String> {
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read transcript: {}", e))?;

    let mut segments = parse_vtt(&content)?;
    merge_segments(&mut segments);

    // Build continuous text and char_idx → timestamp map
    let mut text = String::new();
    let mut timepoints: Vec<Timepoint> = Vec::new();

    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        let start_idx = text.len();
        timepoints.push(Timepoint {
            char_idx: start_idx,
            timestamp_ms: seg.start_ms,
        });
        text.push_str(&seg.text);
        let end_idx = text.len();
        timepoints.push(Timepoint {
            char_idx: end_idx,
            timestamp_ms: seg.end_ms,
        });
    }

    Ok(TranscriptData {
        text,
        segments,
        timepoints,
    })
}

fn push_segment(segments: &mut Vec<TranscriptSegment>, start: u64, end: u64, text: String) {
    // Skip segments shorter than 300ms (YouTube auto-caption artifacts)
    if end.saturating_sub(start) < 300 {
        return;
    }
    if text.is_empty() {
        return;
    }
    segments.push(TranscriptSegment {
        start_ms: start,
        end_ms: end,
        text,
    });
}

fn parse_vtt(content: &str) -> Result<Vec<TranscriptSegment>, String> {
    // YouTube's rolling auto-captions show two lines per cue: the previous
    // cue's last line, then the new one. Drop that repeated top line. A
    // single-line cue matching the previous one is real speech said twice.
    fn flush(segs: &mut Vec<TranscriptSegment>, start: u64, end: u64, text: &mut Vec<String>, cue_prev: &str) {
        if text.len() > 1 && text[0] == cue_prev {
            text.remove(0);
        }
        if !text.is_empty() {
            push_segment(segs, start, end, text.join(" "));
        }
        text.clear();
    }

    let mut segments: Vec<TranscriptSegment> = Vec::new();
    let mut current_start: u64 = 0;
    let mut current_end: u64 = 0;
    let mut current_text: Vec<String> = Vec::new();
    let mut in_cue = false;
    // Last text line seen (pushed or not), and its value when this cue began.
    let mut prev_line = String::new();
    let mut cue_prev = String::new();

    for raw in content.lines() {
        // Only a truly empty line ends a cue. YouTube auto-captions use a " "
        // line for a blank caption row; treating it as the end of the cue
        // dropped the cue's real text (e.g. every cue of b-U1Av2vsu8).
        if raw.is_empty() {
            if in_cue {
                flush(&mut segments, current_start, current_end, &mut current_text, &cue_prev);
            }
            in_cue = false;
            continue;
        }
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        // Skip header/metadata
        if line == "WEBVTT" || line.starts_with("Kind:") || line.starts_with("Language:") {
            continue;
        }

        if line.starts_with("NOTE") {
            continue;
        }

        // Timestamp line
        if line.contains("-->") {
            if let Some((start, end)) = parse_timestamp_line(line) {
                if in_cue {
                    flush(&mut segments, current_start, current_end, &mut current_text, &cue_prev);
                }
                current_start = start;
                current_end = end;
                in_cue = true;
                cue_prev = prev_line.clone();
            }
        } else if in_cue {
            let cleaned = strip_tags(line);
            if !cleaned.is_empty() {
                prev_line = cleaned.clone();
                current_text.push(cleaned);
            }
        }
    }

    if in_cue {
        flush(&mut segments, current_start, current_end, &mut current_text, &cue_prev);
    }

    Ok(segments)
}

/// Merge overlapping YouTube auto-caption fragments into clean, non-overlapping
/// segments suitable for search — same format whisper.cpp produces natively.
fn merge_segments(segments: &mut Vec<TranscriptSegment>) {
    if segments.is_empty() {
        return;
    }

    segments.sort_by_key(|s| s.start_ms);

    let mut merged: Vec<TranscriptSegment> = Vec::with_capacity(segments.len());
    let mut current = segments[0].clone();

    for next in segments.iter().skip(1) {
        let gap = next.start_ms.saturating_sub(current.end_ms);

        // Same-start ghost: longer version of same segment replaces shorter
        if next.start_ms <= current.start_ms + 200 && next.end_ms > current.end_ms {
            current = next.clone();
            continue;
        }

        // Fully contained ghost: skip
        if next.start_ms >= current.start_ms && next.end_ms <= current.end_ms {
            continue;
        }

        // Continuous or overlapping: merge text
        if gap <= 500 && current.end_ms - current.start_ms < 15_000 {
            let next_text = next.text.trim();
            let curr_text = current.text.trim();

            if next_text.is_empty() {
                continue;
            }

            // Duplicate: current already contains next
            if curr_text.contains(next_text) {
                continue;
            }

            // Find overlap suffix and append only new part
            if let Some(suffix_start) = find_overlap_suffix(curr_text, next_text) {
                let new_part = &next_text[suffix_start..];
                if !new_part.trim().is_empty() {
                    current.text = format!("{} {}", curr_text, new_part.trim());
                }
            } else {
                current.text = format!("{} {}", curr_text, next_text);
            }

            current.end_ms = current.end_ms.max(next.end_ms);
        } else {
            merged.push(current);
            current = next.clone();
        }
    }

    merged.push(current);
    *segments = merged;
}

/// Find character overlap: how many chars at end of `a` match start of `b`.
/// Returns byte offset into `b` (original, not lowercased) where new content begins.
fn find_overlap_suffix(a: &str, b: &str) -> Option<usize> {
    let a_chars: Vec<char> = a.to_lowercase().chars().collect();
    let b_chars: Vec<char> = b.to_lowercase().chars().collect();

    for len in (1..=a_chars.len().min(b_chars.len())).rev() {
        let suffix = &a_chars[a_chars.len() - len..];
        if b_chars.starts_with(suffix) {
            // Return byte offset in original b, computed from char count
            return Some(b.chars().take(len).map(|c| c.len_utf8()).sum());
        }
    }
    None
}

// ── VTT timestamp parsing ─────────────────────────────────────────────────
fn parse_timestamp_line(line: &str) -> Option<(u64, u64)> {
    let mut parts = line.split("-->");
    let start_str = parts.next()?.trim();
    let end_str = parts.next()?.trim();
    Some((parse_vtt_time(start_str)?, parse_vtt_time(end_str)?))
}

/// Parse "00:00:00.000" or "00:00:00,000" → milliseconds
fn parse_vtt_time(s: &str) -> Option<u64> {
    let s = s.trim();
    // Handle optional hours
    let (hours, rest) = if let Some(idx) = s.find(':') {
        let after_first = &s[idx + 1..];
        if after_first.contains(':') {
            // Has hours: HH:MM:SS.mmm
            let h: u64 = s[..idx].parse().ok()?;
            let rest = &s[idx + 1..];
            (h, rest)
        } else {
            // No hours: MM:SS.mmm
            (0, s)
        }
    } else {
        return None;
    };

    let (minutes, rest) = if let Some(idx) = rest.find(':') {
        let m: u64 = rest[..idx].parse().ok()?;
        (m, &rest[idx + 1..])
    } else {
        return None;
    };

    let (seconds, millis) = if let Some(idx) = rest.find('.') {
        let sec: u64 = rest[..idx].parse().ok()?;
        let ms_str = &rest[idx + 1..];
        let ms: u64 = take_digits(ms_str).parse().ok()?;
        (sec, ms)
    } else if let Some(idx) = rest.find(',') {
        let sec: u64 = rest[..idx].parse().ok()?;
        let ms_str = &rest[idx + 1..];
        let ms: u64 = take_digits(ms_str).parse().ok()?;
        (sec, ms)
    } else {
        return None;
    };

    Some(hours * 3_600_000 + minutes * 60_000 + seconds * 1000 + millis)
}

/// Extract leading ASCII digits from a string. "360 align:start" → "360"
fn take_digits(s: &str) -> &str {
    let end = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    &s[..end]
}

/// Strip HTML/XML tags and inline timestamps like <00:00:01.040> from caption text.
fn strip_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    decode_html_entities(result.trim())
}

/// Decode common HTML character entities: &amp; &lt; &gt; &nbsp; &quot; &#39;
fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            // Look forward for the matching ';'
            if let Some(end) = chars[i..].iter().position(|&c| c == ';') {
                let end = i + end;
                let entity: String = chars[i..=end].iter().collect();
                let replacement = match entity.as_str() {
                    "&amp;" => Some('&'),
                    "&lt;" => Some('<'),
                    "&gt;" => Some('>'),
                    "&nbsp;" => Some(' '),
                    "&quot;" => Some('"'),
                    "&#39;" | "&apos;" => Some('\''),
                    _ => None,
                };
                if let Some(ch) = replacement {
                    out.push(ch);
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ── editing sources: transcript section or full video ─────────────────────

/// Download the editing copy (720p) of a transcript segment ± padding and
/// describe how its local timeline maps to absolute video time.
#[tauri::command]
async fn download_section(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
    start_ms: u64,
    end_ms: u64,
) -> Result<EditSource, String> {
    // Whole-second section bounds (yt-dlp starts the video at the first frame
    // at or after this point; see media.rs).
    let start_sec = (start_ms as f64 / 1000.0 - SECTION_PAD_SECS).max(0.0).floor() as u64;
    let end_sec = (end_ms as f64 / 1000.0 + SECTION_PAD_SECS).ceil() as u64;
    edit_section(&app, &video_id, &url, start_sec, end_sec).await
}

/// Widen the current section by EXTEND_SECS at its start or end.
#[tauri::command]
async fn extend_section(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
    edit: EditSource,
    side: String,
) -> Result<EditSource, String> {
    let (start, end) = edit.section.ok_or("The full video is already loaded")?;
    let (start, end) = match side.as_str() {
        "start" if start == 0 => return Err("Already at the start of the video".into()),
        "start" => (start.saturating_sub(EXTEND_SECS), end),
        "end" => (start, end + EXTEND_SECS),
        _ => return Err(format!("Unknown side: {side}")),
    };
    edit_section(&app, &video_id, &url, start, end).await
}

async fn edit_section(
    app: &tauri::AppHandle,
    video_id: &str,
    url: &str,
    start_sec: u64,
    end_sec: u64,
) -> Result<EditSource, String> {
    let videos_dir = cache_subdir(app, "videos")?;
    let path = Downloader::fetch_section(
        app,
        &videos_dir.to_string_lossy(),
        video_id,
        url,
        start_sec,
        end_sec,
        EDIT_RESOLUTION,
        download_progress(app),
    )
    .await
    .map_err(|e| format!("Section download failed: {e}"))?;

    let mut src = section_source(app, path, start_sec as f64).await?;
    src.section = Some((start_sec, end_sec));
    Ok(src)
}

/// Download the whole video (720p) for manual trimming.
#[tauri::command]
async fn load_full_video(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
) -> Result<EditSource, String> {
    let videos_dir = cache_subdir(&app, "videos")?;
    let path = Downloader::fetch_full_video(
        &app,
        &videos_dir.to_string_lossy(),
        &video_id,
        &url,
        EDIT_RESOLUTION,
        download_progress(&app),
    )
    .await
    .map_err(|e| format!("Video download failed: {e}"))?;
    section_source(&app, path, 0.0).await
}

/// Progress for the editing-copy downloads (drives the top progress bar).
fn download_progress(app: &tauri::AppHandle) -> downloader::ProgressFn {
    let app = app.clone();
    Arc::new(move |pct| {
        let _ = app.emit("download-progress", serde_json::json!({ "percent": pct }));
    })
}

async fn section_source(app: &tauri::AppHandle, path: String, requested_start: f64) -> Result<EditSource, String> {
    let ffmpeg = sidecar::resolve_sidecar(app, "ffmpeg");
    let frame_dur = media::probe_frame_dur(&ffmpeg, &path).await?;
    Ok(EditSource {
        video_origin: media::ceil_to_grid(requested_start, frame_dur),
        audio_origin: requested_start,
        frame_dur,
        path,
        section: None,
    })
}

// ── preview_clip: exact In/Out render from the editing copy ───────────────

#[tauri::command]
async fn preview_clip(
    app: tauri::AppHandle,
    edit: EditSource,
    in_sec: f64,
    out_sec: f64,
    audio_in_sec: f64,
    audio_out_sec: f64,
    declick: bool,
) -> Result<serde_json::Value, String> {
    let ffmpeg = sidecar::resolve_sidecar(&app, "ffmpeg");
    let range = media::clip_range(&ffmpeg, &edit, in_sec, out_sec, audio_in_sec, audio_out_sec).await?;

    // Cache by exact absolute frame ranges and options.
    let ms = |t: f64| (t * 1000.0).round() as u64;
    let stem = edit.path.strip_suffix(".mp4").unwrap_or(&edit.path);
    let preview_path = format!(
        // "prev2": earlier previews lack the remux (stall in WebKit); don't reuse them.
        "{}_prev2_{}_{}_a{}_{}{}.mp4",
        stem,
        ms(range.start),
        ms(range.end),
        ms(range.audio_start),
        ms(range.audio_end),
        if declick { "_dc" } else { "" }
    );

    if !std::path::Path::new(&preview_path).exists() {
        media::cut(
            &ffmpeg,
            &edit,
            range,
            &preview_path,
            media::EncodeOpts { preset: "ultrafast", crf: "23", audio_bitrate: "128k" },
            declick,
            {
                let app = app.clone();
                move |pct| {
                    let _ = app.emit("preview-progress", serde_json::json!({ "percent": pct }));
                }
            },
        )
        .await?;
    }

    Ok(serde_json::json!({
        "url": server::register(&preview_path)?,
        "start": range.start,
        "end": range.end,
        "frames": range.layout(edit.frame_dur).frames,
    }))
}

// ── export_clip: exact In/Out at the chosen quality ───────────────────────

#[tauri::command]
async fn export_clip(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
    edit: EditSource,
    in_sec: f64,
    out_sec: f64,
    audio_in_sec: f64,
    audio_out_sec: f64,
    declick: bool,
    resolution: String,
    output_path: String,
) -> Result<String, String> {
    let ffmpeg = sidecar::resolve_sidecar(&app, "ffmpeg");
    let range = media::clip_range(&ffmpeg, &edit, in_sec, out_sec, audio_in_sec, audio_out_sec).await?;

    let emit = {
        let app = app.clone();
        move |percent: f64, status: &str| {
            let _ = app.emit("export-progress", ExportProgress { percent, status: status.into() });
        }
    };

    // Same quality as the editing copy: cut straight from it, and encoding is
    // the whole bar. Otherwise download first (0–60%), then encode (60–100%).
    let needs_download = resolution != EDIT_RESOLUTION;
    let (enc_base, enc_span) = if needs_download { (60.0, 40.0) } else { (0.0, 100.0) };
    let source = if !needs_download {
        edit.clone()
    } else {
        emit(0.0, "downloading");
        let videos_dir = cache_subdir(&app, "videos")?;
        // Cover audio as well as video (split edits extend past the video).
        let (u_start, u_end) = range.union();
        let start_sec = (u_start - EXPORT_PAD_SECS).max(0.0).floor() as u64;
        let end_sec = (u_end + EXPORT_PAD_SECS).ceil() as u64;
        let path = Downloader::fetch_section(
            &app,
            &videos_dir.to_string_lossy(),
            &video_id,
            &url,
            start_sec,
            end_sec,
            &resolution,
            {
                let emit = emit.clone();
                Arc::new(move |pct| emit(pct * 0.6, "downloading"))
            },
        )
        .await
        .map_err(|e| {
            emit(0.0, "error");
            format!("Download failed: {e}")
        })?;
        section_source(&app, path, start_sec as f64).await?
    };

    emit(enc_base, "encoding");

    let result = media::cut(
        &ffmpeg,
        &source,
        range,
        &output_path,
        media::EncodeOpts { preset: "medium", crf: "18", audio_bitrate: "192k" },
        declick,
        {
            let emit = emit.clone();
            move |pct| emit(enc_base + pct * enc_span / 100.0, "encoding")
        },
    )
    .await;

    match result {
        Ok(()) => {
            emit(100.0, "complete");
            Ok(output_path)
        }
        Err(e) => {
            emit(0.0, "error");
            Err(e)
        }
    }
}

// ── serve_video: stream a local file to the webview ───────────────────────

#[tauri::command]
fn serve_video(path: String) -> Result<String, String> {
    server::register(&path)
}

// ── cache housekeeping ────────────────────────────────────────────────────

/// Delete cached downloads/renders older than `max_age`. Models are kept.
fn prune_cache(cache: &std::path::Path, max_age: std::time::Duration) {
    let now = std::time::SystemTime::now();
    for dir in ["videos", "audio", "transcripts", "exports"] {
        let Ok(entries) = std::fs::read_dir(cache.join(dir)) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .map(|age| age > max_age)
                .unwrap_or(false);
            if old && path.is_file() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

// ── app entry ─────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            eprintln!(
                "[better-clipper] resource_dir = {:?}",
                app.path().resource_dir().unwrap_or_default()
            );
            if let Ok(cache) = app.path().app_cache_dir() {
                std::thread::spawn(move || {
                    prune_cache(&cache, std::time::Duration::from_secs(3 * 24 * 3600))
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            process_video,
            transcribe_video,
            extend_section,
            read_transcript,
            download_section,
            load_full_video,
            export_clip,
            preview_clip,
            serve_video
        ])
        .run(tauri::generate_context!())
        .expect("error while running BetterClipper");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// YouTube auto-captions where every cue opens with a " " line
    /// (b-U1Av2vsu8). Used to parse to 0 segments.
    #[test]
    fn parse_vtt_keeps_text_after_blank_space_line() {
        let vtt = "WEBVTT\nKind: captions\nLanguage: en\n\n\
00:00:05.759 --> 00:00:08.709 align:start position:0%\n \nlook<00:00:05.920><c> at</c><00:00:06.080><c> that</c><00:00:06.319><c> wet</c>\n\n\
00:00:08.709 --> 00:00:08.719 align:start position:0%\n \n \n\n\
00:00:08.719 --> 00:00:18.810 align:start position:0%\n \nnose<00:00:09.800><c> a</c><00:00:10.800><c> look</c><00:00:11.040><c> at</c><00:00:11.360><c> you</c>\n\n\
00:00:18.810 --> 00:00:18.820 align:start position:0%\n \n \n\n\
00:00:18.820 --> 00:00:22.100 align:start position:0%\n \n[Music]\n";
        let segs = parse_vtt(vtt).unwrap();
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["look at that wet", "nose a look at you", "[Music]"]);
        assert_eq!((segs[0].start_ms, segs[0].end_ms), (5759, 8709));
    }

    /// Rolling auto-captions: each cue repeats the previous line on top.
    #[test]
    fn parse_vtt_drops_rolling_repeat_line() {
        let vtt = "WEBVTT\n\n\
00:00:01.000 --> 00:00:03.000\n \nlook<00:00:01.500><c> down.</c>\n\n\
00:00:03.000 --> 00:00:03.010\nlook down.\n \n\n\
00:00:03.010 --> 00:00:06.000\nlook down.\nDirectly<00:00:04.000><c> beneath</c>\n";
        let texts: Vec<String> = parse_vtt(vtt).unwrap().into_iter().map(|s| s.text).collect();
        assert_eq!(texts, ["look down.", "Directly beneath"]);
    }

    /// Manual captions where the same line is spoken twice, in separate cues.
    #[test]
    fn parse_vtt_keeps_line_spoken_twice() {
        let vtt = "WEBVTT\n\n00:11:12.801 --> 00:11:13.636\nI'm losing it.\n\n\
00:11:14.837 --> 00:11:15.604\nI'm losing it.\n";
        let texts: Vec<String> = parse_vtt(vtt).unwrap().into_iter().map(|s| s.text).collect();
        assert_eq!(texts, ["I'm losing it.", "I'm losing it."]);
    }
}
