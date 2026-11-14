use serde::Serialize;
use tauri::AppHandle;
use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_shell::ShellExt;

use crate::sidecar::resolve_sidecar;

const MODEL_URL_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
const MODEL_NAME: &str = "ggml-tiny.bin";
const MIN_MODEL_BYTES: u64 = 30 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ModelDownloadProgress {
    pub percent: f64,
    pub status: String,
}

pub struct Transcriber;

impl Transcriber {
    /// Extract 16kHz mono WAV from a video file using ffmpeg.
    pub async fn extract_audio(app: &AppHandle, video_path: &str, output_wav: &str) -> Result<(), String> {
        let ffmpeg = resolve_sidecar(app, "ffmpeg");

        let output = app
            .shell()
            .command(&ffmpeg)
            .args([
                "-i",
                video_path,
                "-ar",
                "16000",
                "-ac",
                "1",
                "-c:a",
                "pcm_s16le",
                "-y",
                output_wav,
            ])
            .output()
            .await
            .map_err(|e| format!("Failed to spawn ffmpeg: {}", e))?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("ffmpeg audio extraction failed: {}", stderr))
        }
    }

    /// Ensure the whisper GGML model exists in the app cache.
    ///
    /// Downloads `ggml-tiny.bin` (~78 MB) from HuggingFace if missing,
    /// emitting `model-download-progress` events.
    /// Returns the path to the model file.
    pub async fn ensure_model(app: &AppHandle) -> Result<String, String> {
        let models_dir = app
            .path()
            .app_cache_dir()
            .map_err(|e| e.to_string())?
            .join("models");

        std::fs::create_dir_all(&models_dir).map_err(|e| e.to_string())?;

        let model_path = models_dir.join(MODEL_NAME);
        let model_path_str = model_path.to_string_lossy().to_string();

        // A truncated download or saved error page would otherwise be treated
        // as a valid model forever. ggml-tiny.bin is ~75 MB.
        let valid = std::fs::metadata(&model_path)
            .map(|m| m.len() > MIN_MODEL_BYTES)
            .unwrap_or(false);
        if model_path.exists() && !valid {
            eprintln!("[better-clipper] whisper model at {model_path_str} is invalid; re-downloading");
            let _ = std::fs::remove_file(&model_path);
        }

        if valid {
            let _ = app.emit(
                "model-download-progress",
                ModelDownloadProgress {
                    percent: 100.0,
                    status: "exists".to_string(),
                },
            );
            return Ok(model_path_str);
        }

        let url = format!("{}/{}", MODEL_URL_BASE, MODEL_NAME);

        let response = reqwest::get(&url)
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("Failed to fetch model: {}", e))?;

        let total_size = response.content_length().unwrap_or(0);
        let mut downloaded: u64 = 0;
        // Download to a temp name and rename when complete.
        let part_path = models_dir.join(format!("{MODEL_NAME}.part"));
        let mut file = tokio::fs::File::create(&part_path)
            .await
            .map_err(|e| format!("Failed to create model file: {}", e))?;

        let mut stream = response.bytes_stream();

        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("Download error: {}", e))?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(|e| format!("Write error: {}", e))?;
            downloaded += chunk.len() as u64;

            if total_size > 0 {
                let pct = (downloaded as f64 / total_size as f64) * 100.0;
                let _ = app.emit(
                    "model-download-progress",
                    ModelDownloadProgress {
                        percent: pct,
                        status: "downloading".to_string(),
                    },
                );
            }
        }

        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|e| format!("Write error: {}", e))?;
        drop(file);
        if downloaded <= MIN_MODEL_BYTES || (total_size > 0 && downloaded != total_size) {
            let _ = std::fs::remove_file(&part_path);
            return Err(format!(
                "Model download incomplete ({downloaded} of {total_size} bytes)"
            ));
        }
        std::fs::rename(&part_path, &model_path)
            .map_err(|e| format!("Failed to move model into place: {}", e))?;

        let _ = app.emit(
            "model-download-progress",
            ModelDownloadProgress {
                percent: 100.0,
                status: "complete".to_string(),
            },
        );

        Ok(model_path_str)
    }

    /// Run whisper-cli on a 16kHz mono WAV to produce a VTT transcript.
    ///
    /// Returns the path to the generated `.vtt` file.
    /// `on_progress` receives 0–100 (whisper reports after each 30s window).
    pub async fn transcribe(
        app: &AppHandle,
        model_path: &str,
        wav_path: &str,
        output_prefix: &str,
        on_progress: crate::downloader::ProgressFn,
    ) -> Result<String, String> {
        let whisper = resolve_sidecar(app, "whisper-cli");

        let mut child = tokio::process::Command::new(&whisper)
            .args([
                "-m", model_path,
                "-f", wav_path,
                "-ovtt",
                "-of", output_prefix,
                "-l", "auto",
                "-pp", // "whisper_print_progress_callback: progress = NN%" on stderr
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to spawn whisper-cli: {}", e))?;

        let stderr = child.stderr.take().ok_or("Failed to capture whisper-cli stderr")?;
        let reader = tokio::spawn(async move {
            let mut tail: Vec<String> = Vec::new();
            crate::downloader::for_each_line(stderr, |line| {
                if let Some(pct) = line
                    .split("progress =")
                    .nth(1)
                    .and_then(|r| r.trim().trim_end_matches('%').trim().parse::<f64>().ok())
                {
                    on_progress(pct.clamp(0.0, 100.0));
                } else if !line.is_empty() {
                    tail.push(line.to_string());
                    if tail.len() > 20 {
                        tail.remove(0);
                    }
                }
            })
            .await;
            tail
        });

        let status = child
            .wait()
            .await
            .map_err(|e| format!("whisper-cli process error: {}", e))?;
        let tail = reader.await.unwrap_or_default();

        let vtt_path = format!("{}.vtt", output_prefix);
        if status.success() && std::path::Path::new(&vtt_path).exists() {
            Ok(vtt_path)
        } else {
            Err(format!(
                "whisper-cli failed (exit {:?}): {}",
                status.code(),
                tail[tail.len().saturating_sub(5)..].join(" | ")
            ))
        }
    }
}
