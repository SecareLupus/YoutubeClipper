use serde::Serialize;
use tauri::AppHandle;
use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_shell::ShellExt;

use crate::sidecar::resolve_sidecar;

const MODEL_URL_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
const MODEL_NAME: &str = "ggml-tiny.bin";

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

        if model_path.exists() {
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
            .map_err(|e| format!("Failed to fetch model: {}", e))?;

        let total_size = response.content_length().unwrap_or(0);
        let mut downloaded: u64 = 0;
        let mut file = tokio::fs::File::create(&model_path)
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
    pub async fn transcribe(
        app: &AppHandle,
        model_path: &str,
        wav_path: &str,
        output_prefix: &str,
    ) -> Result<String, String> {
        let whisper = resolve_sidecar(app, "whisper-cli");

        let output = app
            .shell()
            .command(&whisper)
            .args([
                "-m", model_path,
                "-f", wav_path,
                "-ovtt",
                "-of", output_prefix,
                "-l", "auto",
            ])
            .output()
            .await
            .map_err(|e| format!("Failed to spawn whisper-cli: {}", e))?;

        if output.status.success() {
            let vtt_path = format!("{}.vtt", output_prefix);
            Ok(vtt_path)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("whisper-cli transcription failed: {}", stderr))
        }
    }
}
