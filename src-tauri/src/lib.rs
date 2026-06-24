use serde::Serialize;
use std::io::{Read, Write, Seek, SeekFrom};
use std::net::TcpListener;
use tauri::Emitter;
use tauri::Manager;

mod downloader;
mod sidecar;
mod transcriber;

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

// ── process_video: fetch transcript only (no video download) ──────────────

#[tauri::command]
async fn process_video(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
) -> Result<serde_json::Value, String> {
    eprintln!(
        "[better-clipper] process_video id={video_id} url={url}",
    );
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    let videos_dir = cache.join("videos");
    let transcripts_dir = cache.join("transcripts");
    std::fs::create_dir_all(&videos_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&transcripts_dir).map_err(|e| e.to_string())?;

    // Stage 1: Fetch captions (fast, no video)
    let _ = app.emit(
        "pipeline-progress",
        PipelineProgress {
            video_id: video_id.clone(),
            stage: "fetching_transcript".to_string(),
            message: "Checking for captions...".to_string(),
            percent: 10.0,
        },
    );

    let yt_vtt = downloader::Downloader::fetch_transcript(
        &app,
        &videos_dir.to_string_lossy(),
        &video_id,
        &url,
    )
    .await
    .map_err(|e| {
        eprintln!("[better-clipper] process_video: fetch_transcript error: {e}");
        format!("Transcript fetch failed: {}", e)
    })?;

    // Fetch video title for export filename suggestions
    let title = fetch_video_title(&app, &url).await;

    if let Some(vtt_path) = yt_vtt {
        let dest_vtt = transcripts_dir.join(format!("{}.vtt", video_id));
        std::fs::copy(&vtt_path, &dest_vtt).map_err(|e| e.to_string())?;

        let _ = app.emit(
            "pipeline-progress",
            PipelineProgress {
                video_id: video_id.clone(),
                stage: "done".to_string(),
                message: "Transcript ready — select a segment to preview.".to_string(),
                percent: 100.0,
            },
        );

        return Ok(serde_json::json!({
            "video_id": video_id,
            "transcript_path": dest_vtt.to_string_lossy(),
            "transcript_source": "youtube",
            "title": title,
            "url": url,
        }));
    }

    // No captions — download full video and transcribe with whisper
    let _ = app.emit(
        "pipeline-progress",
        PipelineProgress {
            video_id: video_id.clone(),
            stage: "download".to_string(),
            message: "No captions — downloading video for transcription...".to_string(),
            percent: 15.0,
        },
    );

    let preview_path = downloader::Downloader::fetch_video(
        &app,
        &videos_dir.to_string_lossy(),
        &video_id,
        "720p",
        &url,
    )
    .await
    .map_err(|e| format!("Download failed: {}", e))?;

    let _ = app.emit(
        "pipeline-progress",
        PipelineProgress {
            video_id: video_id.clone(),
            stage: "extract_audio".to_string(),
            message: "Extracting audio...".to_string(),
            percent: 35.0,
        },
    );

    let wav_path = cache.join("audio").join(format!("{}.wav", &video_id));
    if let Some(parent) = wav_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    transcriber::Transcriber::extract_audio(&app, &preview_path, &wav_path.to_string_lossy())
        .await
        .map_err(|e| format!("Audio extraction failed: {}", e))?;

    let _ = app.emit(
        "pipeline-progress",
        PipelineProgress {
            video_id: video_id.clone(),
            stage: "ensure_model".to_string(),
            message: "Checking whisper model...".to_string(),
            percent: 50.0,
        },
    );

    let model_path = transcriber::Transcriber::ensure_model(&app)
        .await
        .map_err(|e| format!("Model setup failed: {}", e))?;

    let _ = app.emit(
        "pipeline-progress",
        PipelineProgress {
            video_id: video_id.clone(),
            stage: "transcribe".to_string(),
            message: "Transcribing with whisper...".to_string(),
            percent: 60.0,
        },
    );

    let output_prefix = transcripts_dir.join(&video_id).to_string_lossy().to_string();

    let vtt_path = transcriber::Transcriber::transcribe(
        &app,
        &model_path,
        &wav_path.to_string_lossy(),
        &output_prefix,
    )
    .await
    .map_err(|e| format!("Transcription failed: {}", e))?;

    let _ = app.emit(
        "pipeline-progress",
        PipelineProgress {
            video_id: video_id.clone(),
            stage: "done".to_string(),
            message: "Transcription complete — select a segment to preview.".to_string(),
            percent: 100.0,
        },
    );

    Ok(serde_json::json!({
        "video_id": video_id,
        "preview_path": preview_path,
        "transcript_path": vtt_path,
        "transcript_source": "whisper",
        "title": title,
        "url": url,
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
    let mut segments: Vec<TranscriptSegment> = Vec::new();
    let mut current_start: u64 = 0;
    let mut current_end: u64 = 0;
    let mut current_text: Vec<String> = Vec::new();
    let mut in_cue = false;

    for line in content.lines() {
        let line = line.trim();

        // Empty line ends the current cue
        if line.is_empty() {
            if in_cue && !current_text.is_empty() {
                push_segment(&mut segments, current_start, current_end, current_text.join(" "));
                current_text.clear();
            }
            in_cue = false;
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
                if in_cue && !current_text.is_empty() {
                    push_segment(&mut segments, current_start, current_end, current_text.join(" "));
                    current_text.clear();
                }
                current_start = start;
                current_end = end;
                in_cue = true;
            }
        } else if in_cue {
            let cleaned = strip_tags(line);
            if !cleaned.is_empty() {
                current_text.push(cleaned);
            }
        }
    }

    if in_cue && !current_text.is_empty() {
        push_segment(&mut segments, current_start, current_end, current_text.join(" "));
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

// ── download_section: time-bounded video download ─────────────────────────
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

// ── download_section: time-bounded video download ─────────────────────────

#[tauri::command]
async fn download_section(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
    start_ms: u64,
    end_ms: u64,
) -> Result<String, String> {
    let output_dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("videos")
        .to_string_lossy()
        .to_string();

    downloader::Downloader::fetch_section(&app, &output_dir, &video_id, &url, start_ms, end_ms, 10, "480p").await
}

// ── export_slice: ffmpeg re-encode with in/out markers ──────────────────

#[tauri::command]
async fn export_slice(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
    start_ms: u64,
    end_ms: u64,
    start_sec: f64,
    end_sec: f64,
    resolution: String,
    output_path: String,
) -> Result<String, String> {
    // Step 1: download section at requested quality
    let videos_dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("videos");
    std::fs::create_dir_all(&videos_dir).map_err(|e| e.to_string())?;

    let section_path = downloader::Downloader::fetch_section(
        &app,
        &videos_dir.to_string_lossy(),
        &video_id,
        &url,
        start_ms,
        end_ms,
        10,
        &resolution,
    )
    .await
    .map_err(|e| format!("Download failed: {}", e))?;

    // Step 2: re-encode with ffmpeg
    let duration = end_sec - start_sec + 1.0 / 30.0;
    let final_path = if output_path.is_empty() {
        let exports_dir = app
            .path()
            .app_cache_dir()
            .map_err(|e| e.to_string())?
            .join("exports");
        std::fs::create_dir_all(&exports_dir).map_err(|e| e.to_string())?;
        exports_dir
            .join(format!(
                "clip_{}.mp4",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            ))
            .to_string_lossy()
            .to_string()
    } else {
        output_path
    };

    let ffmpeg = sidecar::resolve_sidecar(&app, "ffmpeg");

    let _ = app.emit(
        "export-progress",
        ExportProgress {
            percent: 0.0,
            status: "encoding".to_string(),
        },
    );

    let mut child = tokio::process::Command::new(&ffmpeg)
        .args([
            "-i",
            &section_path,
            "-ss",
            &format!("{:.3}", start_sec),
            "-to",
            &format!("{:.3}", end_sec + 1.0 / 30.0),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "18",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-y",
            &final_path,
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {}", e))?;

    let stderr = child
        .stderr
        .take()
        .ok_or("Failed to capture ffmpeg stderr")?;

    let app_clone = app.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line).await {
            if n == 0 {
                break;
            }
            if let Some(time_sec) = parse_ffmpeg_time(&line) {
                if duration > 0.0 {
                    let pct = (time_sec / duration * 100.0).min(99.0);
                    let _ = app_clone.emit(
                        "export-progress",
                        ExportProgress {
                            percent: pct,
                            status: "encoding".to_string(),
                        },
                    );
                }
            }
            line.clear();
        }
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("ffmpeg process error: {}", e))?;

    if status.success() {
        let _ = app.emit(
            "export-progress",
            ExportProgress {
                percent: 100.0,
                status: "complete".to_string(),
            },
        );
        Ok(final_path)
    } else {
        Err("ffmpeg export failed".to_string())
    }
}

/// Parse ffmpeg progress line: "frame= 123 ... time=00:00:05.12 bitrate=..."
fn parse_ffmpeg_time(line: &str) -> Option<f64> {
    let idx = line.find("time=")?;
    let rest = &line[idx + "time=".len()..];
    let time_str = rest.split_whitespace().next()?;
    parse_hhmmss(time_str)
}

/// Parse "HH:MM:SS.mm" → seconds as f64
fn parse_hhmmss(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let s: f64 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

// ── preview_clip: re-encode the section to exact In/Out, serve as preview ──

#[tauri::command]
async fn preview_clip(
    app: tauri::AppHandle,
    section_path: String,
    start_sec: f64,
    end_sec: f64,
) -> Result<String, String> {
    let duration = end_sec - start_sec;

    // Deterministic filename from section path + markers — cached across runs
    let stem = section_path
        .strip_suffix(".mp4")
        .unwrap_or(&section_path);
    let preview_path = format!(
        "{}_preview_{:.2}_{:.2}.mp4",
        stem, start_sec, end_sec
    );

    // If already rendered with these exact markers, skip ffmpeg
    if std::path::Path::new(&preview_path).exists() {
        return serve_video(preview_path);
    }

    let ffmpeg = sidecar::resolve_sidecar(&app, "ffmpeg");

    // -ss after -i: frame-accurate seek (decodes from start, discards before -ss).
    // Re-encode (not -c copy) because the downloaded section has sparse keyframes
    // and can't be trimmed at arbitrary positions without decoding.
    let status = tokio::process::Command::new(&ffmpeg)
        .args([
            "-i",
            &section_path,
            "-ss",
            &format!("{:.3}", start_sec),
            "-t",
            &format!("{:.3}", duration),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "23",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-y",
            &preview_path,
        ])
        .status()
        .await
        .map_err(|e| format!("ffmpeg preview failed: {}", e))?;

    if !status.success() {
        return Err("ffmpeg preview cut failed".to_string());
    }

    serve_video(preview_path)
}

// ── serve_video: local HTTP server so the browser can load the file ────────

#[tauri::command]
fn serve_video(path: String) -> Result<String, String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("addr: {}", e))?
        .port();
    let file_size = std::fs::metadata(&path)
        .map_err(|e| format!("stat: {}", e))?
        .len();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let req = String::from_utf8_lossy(&buf);

            let range_start = req
                .lines()
                .find(|l| l.to_lowercase().starts_with("range:"))
                .and_then(|l| l.split("bytes=").nth(1))
                .and_then(|r| r.split('-').next())
                .and_then(|s| s.trim().parse::<u64>().ok());

            match range_start {
                Some(start) => {
                    let end = file_size - 1;
                    let clen = file_size - start;
                    let resp = format!(
                        "HTTP/1.1 206 Partial Content\r\n\
                         Content-Type: video/mp4\r\n\
                         Content-Range: bytes {}-{}/{}\r\n\
                         Content-Length: {}\r\n\
                         Accept-Ranges: bytes\r\n\r\n",
                        start, end, file_size, clen
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    if let Ok(mut f) = std::fs::File::open(&path) {
                        let _ = f.seek(SeekFrom::Start(start));
                        let mut remaining = clen as usize;
                        let mut buf = [0u8; 8192];
                        while remaining > 0 {
                            let cap = remaining.min(8192);
                            let n = f.read(&mut buf[..cap]).unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            let _ = stream.write_all(&buf[..n]);
                            remaining -= n;
                        }
                    }
                }
                None => {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: video/mp4\r\n\
                         Content-Length: {}\r\n\
                         Accept-Ranges: bytes\r\n\r\n",
                        file_size
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    if let Ok(mut f) = std::fs::File::open(&path) {
                        let _ = std::io::copy(&mut f, &mut stream);
                    }
                }
            }
        }
    });

    Ok(format!("http://127.0.0.1:{}", port))
}

// ── helpers ────────────────────────────────────────────────────────────────

async fn fetch_video_title(app: &tauri::AppHandle, url: &str) -> String {
    let ytdlp = sidecar::resolve_sidecar(app, "yt-dlp");
    match tokio::process::Command::new(&ytdlp)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("USER", std::env::var("USER").unwrap_or_default())
        .env("LANG", std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into()))
        .args(["--print", "title", "--no-playlist", url])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => String::new(),
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
            // GStreamer plugins are bundled as resources in gst-plugins/.
            // Force GStreamer to scan that directory since there's no registry.
            let bundled = app.path().resource_dir()
                .unwrap_or_default()
                .join("binaries/gst-plugins");
            if bundled.exists() {
                eprintln!("[better-clipper] GST_PLUGIN_PATH = {:?}", bundled);
                std::env::set_var("GST_PLUGIN_PATH", &bundled);
                std::env::set_var("GST_PLUGIN_SYSTEM_PATH", &bundled);
                std::env::set_var("GST_REGISTRY_UPDATE", "no");
                std::env::set_var("GST_REGISTRY_FORK", "no");
            } else {
                eprintln!("[better-clipper] gst-plugins dir not found, trying system paths");
                for dir in &["/usr/lib/x86_64-linux-gnu/gstreamer-1.0"] {
                    if std::path::Path::new(dir).exists() {
                        std::env::set_var("GST_PLUGIN_SYSTEM_PATH", dir);
                        break;
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            process_video,
            read_transcript,
            download_section,
            export_slice,
            preview_clip,
            serve_video
        ])
        .run(tauri::generate_context!())
        .expect("error while running BetterClipper");
}
