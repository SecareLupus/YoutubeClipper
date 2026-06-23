use serde::Serialize;
use tauri::AppHandle;
use tauri::Emitter;

use crate::sidecar::resolve_sidecar;

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    pub video_id: String,
    pub resolution: String,
    pub percent: f64,
    pub status: String,
}

pub struct Downloader;

impl Downloader {
    /// Fetch YouTube auto-generated captions (VTT) without downloading the video.
    ///
    /// Emits pipeline-progress events during fetch.
    /// Returns `Some(vtt_path)` if captions were available, `None` otherwise.
    pub async fn fetch_transcript(
        app: &AppHandle,
        output_dir: &str,
        video_id: &str,
        url: &str,
    ) -> Result<Option<String>, String> {
        let ytdlp = resolve_sidecar(app, "yt-dlp");
        eprintln!("[better-clipper] fetch_transcript: yt-dlp path = {ytdlp}");
        let output_template = format!("{}/{}.%(ext)s", output_dir, video_id);

        let _ = app.emit(
            "pipeline-progress",
            crate::PipelineProgress {
                video_id: video_id.to_string(),
                stage: "fetching_transcript".to_string(),
                message: "Connecting to YouTube...".to_string(),
                percent: 5.0,
            },
        );

        let mut child = tokio::process::Command::new(&ytdlp)
            .args([
                "--skip-download",
                "--write-auto-subs",
                "--write-subs",
                "--sub-langs",
                "en",
                "--convert-subs",
                "vtt",
                "-o",
                &output_template,
                "--newline",
                "--no-playlist",
                url,
            ])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn yt-dlp: {}", e))?;

        let stderr = child
            .stderr
            .take()
            .ok_or("Failed to capture yt-dlp stderr")?;

        // Parse stderr for progress (same pattern as fetch_video)
        let app_clone = app.clone();
        let vid = video_id.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.contains("Downloading subtitles") || trimmed.contains("Downloading video subtitles") {
                    let _ = app_clone.emit("pipeline-progress", crate::PipelineProgress {
                        video_id: vid.clone(),
                        stage: "fetching_transcript".to_string(),
                        message: "Downloading captions...".to_string(),
                        percent: 25.0,
                    });
                } else if trimmed.contains("Converting") || trimmed.contains("SubtitlesConvertor") {
                    let _ = app_clone.emit("pipeline-progress", crate::PipelineProgress {
                        video_id: vid.clone(),
                        stage: "fetching_transcript".to_string(),
                        message: "Converting captions to VTT...".to_string(),
                        percent: 60.0,
                    });
                } else if let Some(pct) = parse_download_percent(&line) {
                    let _ = app_clone.emit("pipeline-progress", crate::PipelineProgress {
                        video_id: vid.clone(),
                        stage: "fetching_transcript".to_string(),
                        message: format!("Downloading captions... {:.0}%", pct),
                        percent: 10.0 + pct * 0.4,
                    });
                }
                line.clear();
            }
        });

        let status = child
            .wait()
            .await
            .map_err(|e| {
                eprintln!("[better-clipper] fetch_transcript: yt-dlp spawn error: {e}");
                format!("yt-dlp transcript fetch error: {}", e)
            })?;

        eprintln!(
            "[better-clipper] fetch_transcript: yt-dlp exit code = {}",
            status.code().map_or(-1, |c| c)
        );

        if !status.success() {
            eprintln!("[better-clipper] fetch_transcript: yt-dlp failed (no captions?)");
            return Ok(None);
        }

        let vtt_path = format!("{}/{}.en.vtt", output_dir, video_id);
        if std::path::Path::new(&vtt_path).exists() {
            Ok(Some(vtt_path))
        } else {
            Ok(None)
        }
    }

    /// Download a time-bounded section of the video (±pad_seconds around the range).
    ///
    /// Uses yt-dlp's `--download-sections` to fetch only the needed segment.
    pub async fn fetch_section(
        app: &AppHandle,
        output_dir: &str,
        video_id: &str,
        url: &str,
        start_ms: u64,
        end_ms: u64,
        pad_seconds: u64,
        resolution: &str,
    ) -> Result<String, String> {
        let ytdlp = resolve_sidecar(app, "yt-dlp");
        let pad_ms = pad_seconds * 1000;
        let section_start = ms_to_ytdlp_time(start_ms.saturating_sub(pad_ms));
        let section_end = ms_to_ytdlp_time(end_ms + pad_ms);

        let max_height = match resolution {
            "best" => 2160,
            "1080p" => 1080,
            "720p" => 720,
            _ => 480,
        };

        let format_flag = format!(
            "bestvideo[height<={0}][vcodec^=avc1]+bestaudio[acodec^=mp4a]/bestvideo[height<={0}]+bestaudio/best[height<={0}]",
            max_height
        );

        let output_path = format!(
            "{}/{}_section_{}_{}.mp4",
            output_dir, video_id, start_ms, end_ms
        );
        let output_template = format!(
            "{}/{}_section_{}_{}.%(ext)s",
            output_dir, video_id, start_ms, end_ms
        );

        let _ = app.emit(
            "download-progress",
            DownloadProgress {
                video_id: video_id.to_string(),
                resolution: resolution.to_string(),
                percent: 0.0,
                status: "downloading".to_string(),
            },
        );

        let mut child = tokio::process::Command::new(&ytdlp)
            .args([
                "-f",
                &format_flag,
                "--download-sections",
                &format!("*{}-{}", section_start, section_end),
                "-o",
                &output_template,
                "--merge-output-format",
                "mp4",
                "--newline",
                "--no-playlist",
                url,
            ])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn yt-dlp: {}", e))?;

        let stderr = child
            .stderr
            .take()
            .ok_or("Failed to capture yt-dlp stderr")?;

        let app_clone = app.clone();
        let vid = video_id.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                if let Some(pct) = parse_download_percent(&line) {
                    let _ = app_clone.emit(
                        "download-progress",
                        DownloadProgress {
                            video_id: vid.clone(),
                            resolution: "section".to_string(),
                            percent: pct,
                            status: "downloading".to_string(),
                        },
                    );
                }
                line.clear();
            }
        });

        let status = child
            .wait()
            .await
            .map_err(|e| format!("yt-dlp section download error: {}", e))?;

        if status.success() {
            let _ = app.emit(
                "download-progress",
                DownloadProgress {
                    video_id: video_id.to_string(),
                    resolution: "section".to_string(),
                    percent: 100.0,
                    status: "complete".to_string(),
                },
            );
            Ok(output_path)
        } else {
            Err("yt-dlp section download failed".to_string())
        }
    }

    /// Download full video at the given resolution, emitting real-time progress.
    pub async fn fetch_video(
        app: &AppHandle,
        output_dir: &str,
        video_id: &str,
        resolution: &str,
        url: &str,
    ) -> Result<String, String> {
        let ytdlp = resolve_sidecar(app, "yt-dlp");

        let format_flag = match resolution {
            "4k" => "bestvideo[height<=2160][vcodec^=avc1]+bestaudio[acodec^=mp4a]/bestvideo[height<=2160]+bestaudio/best[height<=2160]",
            "1080p" => "bestvideo[height<=1080][vcodec^=avc1]+bestaudio[acodec^=mp4a]/bestvideo[height<=1080]+bestaudio/best[height<=1080]",
            _ => "bestvideo[height<=720][vcodec^=avc1]+bestaudio[acodec^=mp4a]/best[height<=720]",
        };

        let output_path = format!("{}/{}.mp4", output_dir, video_id);
        let output_template = format!("{}/{}.%(ext)s", output_dir, video_id);

        let _ = app.emit(
            "download-progress",
            DownloadProgress {
                video_id: video_id.to_string(),
                resolution: resolution.to_string(),
                percent: 0.0,
                status: "downloading".to_string(),
            },
        );

        let mut child = tokio::process::Command::new(&ytdlp)
            .args([
                "-f",
                format_flag,
                "-o",
                &output_template,
                "--merge-output-format",
                "mp4",
                "--newline",
                "--no-playlist",
                url,
            ])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn yt-dlp: {}", e))?;

        let stderr = child
            .stderr
            .take()
            .ok_or("Failed to capture yt-dlp stderr")?;

        let app_clone = app.clone();
        let vid = video_id.to_string();
        let res = resolution.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                if let Some(pct) = parse_download_percent(&line) {
                    let _ = app_clone.emit(
                        "download-progress",
                        DownloadProgress {
                            video_id: vid.clone(),
                            resolution: res.clone(),
                            percent: pct,
                            status: "downloading".to_string(),
                        },
                    );
                }
                line.clear();
            }
        });

        let status = child
            .wait()
            .await
            .map_err(|e| format!("yt-dlp process error: {}", e))?;

        if status.success() {
            let _ = app.emit(
                "download-progress",
                DownloadProgress {
                    video_id: video_id.to_string(),
                    resolution: resolution.to_string(),
                    percent: 100.0,
                    status: "complete".to_string(),
                },
            );

            Ok(output_path)
        } else {
            let _ = app.emit(
                "download-progress",
                DownloadProgress {
                    video_id: video_id.to_string(),
                    resolution: resolution.to_string(),
                    percent: 0.0,
                    status: "error".to_string(),
                },
            );

            Err("yt-dlp exited with error".to_string())
        }
    }
}

/// Parse yt-dlp progress output like "[download]  42.3% of ~50.00MiB at ..."
fn parse_download_percent(line: &str) -> Option<f64> {
    if !line.starts_with("[download]") {
        return None;
    }
    let after_prefix = line.strip_prefix("[download]")?.trim_start();
    let pct_str = after_prefix.split('%').next()?;
    pct_str.trim().parse::<f64>().ok()
}

/// Convert milliseconds to HH:MM:SS for yt-dlp --download-sections
fn ms_to_ytdlp_time(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}
