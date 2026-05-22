# Phase 2: Native Sidecar Integration Bridge — Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Wire the Rust backend to spawn and orchestrate `yt-dlp`, `ffmpeg`, and `whisper-cli` sidecars. Implement download orchestration, audio extraction, transcription, and model auto-fetching.

**Architecture:** Two Rust modules (`downloader.rs`, `transcriber.rs`) expose async Tauri commands. Sidecar binaries are resolved from PATH in dev, or from `src-tauri/binaries/<name>-<target-triple>` in production. `yt-dlp` downloads video at two resolutions (720p preview + master) in parallel. `ffmpeg` extracts 16kHz mono WAV from preview. `whisper-cli` transcribes WAV to VTT using a GGML model auto-fetched from HuggingFace if missing.

**Tech Stack:** Tauri v2 (process::Command), tauri-plugin-shell, reqwest (HTTP for model download), serde, tokio (async runtime).

**Dependencies to add:** `reqwest = { version = "0.12", features = ["stream"] }`, `tokio = { version = "1", features = ["process", "io-util", "fs"] }`, `tempfile = "3"`

---

### Task 0: Add Cargo dependencies

**Objective:** Add `reqwest`, `tokio`, and `tempfile` to `Cargo.toml`

**Files:**
- Modify: `src-tauri/Cargo.toml`

**Step 1: Edit Cargo.toml**

Add under `[dependencies]`:

```toml
reqwest = { version = "0.12", features = ["stream"] }
tokio = { version = "1", features = ["process", "io-util", "fs"] }
tempfile = "3"
```

**Step 2: Verify build**

```bash
cd src-tauri && cargo check
```

Expected: compiles new deps, no errors (4 pre-existing warnings about unused `new()` are fine).

---

### Task 1: Sidecar binary resolution utility

**Objective:** Create a helper function that locates a sidecar binary — dev uses PATH, production uses bundled `binaries/<name>-<target-triple>`.

**Files:**
- Create: `src-tauri/src/sidecar.rs`

**Step 1: Write the module**

```rust
/// Resolve a sidecar binary path.
/// In dev builds: uses the binary name directly (relies on PATH).
/// In production: looks in the bundled `binaries/` directory with target-triple suffix.
pub fn resolve_sidecar(name: &str) -> String {
    #[cfg(debug_assertions)]
    {
        // Dev: assume binary is on PATH
        name.to_string()
    }

    #[cfg(not(debug_assertions))]
    {
        let target_triple = env!("TAURI_ENV_TARGET_TRIPLE");
        let bundled = format!("binaries/{}-{}", name, target_triple);
        // Tauri resolves sidecars relative to the app bundle
        bundled
    }
}
```

**Step 2: Register module in lib.rs**

Add `mod sidecar;` after the existing `mod` declarations.

**Step 3: Verify build**

```bash
cd src-tauri && cargo check
```

Expected: compiles, no new errors.

---

### Task 2: Downloader — yt-dlp invocation with format selection

**Objective:** Implement `Downloader` with a `fetch_video` method that spawns `yt-dlp` to download a video at a specified resolution, emitting progress events.

**Files:**
- Modify: `src-tauri/src/downloader.rs`

**Step 1: Replace placeholder with full implementation**

```rust
use tauri::AppHandle;
use tauri::Emitter;
use serde::Serialize;

use crate::sidecar::resolve_sidecar;

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    pub video_id: String,
    pub resolution: String,
    pub percent: f64,
    pub status: String, // "downloading" | "complete" | "error"
}

pub struct Downloader;

impl Downloader {
    /// Spawn yt-dlp to download a video at the given resolution.
    /// `output_dir` — where to save the file.
    /// `video_id` — unique ID passed through to events so frontend can correlate.
    /// `resolution` — "720p", "1080p", or "4k" (maps to yt-dlp -f).
    /// `url` — the video URL.
    pub async fn fetch_video(
        app: AppHandle,
        output_dir: &str,
        video_id: &str,
        resolution: &str,
        url: &str,
    ) -> Result<String, String> {
        let ytdlp = resolve_sidecar("yt-dlp");

        let format_flag = match resolution {
            "4k" => "bestvideo[height<=2160]+bestaudio/best[height<=2160]",
            "1080p" => "bestvideo[height<=1080]+bestaudio/best[height<=1080]",
            _ => "bestvideo[height<=720]+bestaudio/best[height<=720]",
        };

        let output_template = format!("{}/%(title)s-%(id)s.%(ext)s", output_dir);

        let output = tauri::process::Command::new(&ytdlp)
            .args([
                "-f", format_flag,
                "-o", &output_template,
                "--print", "filename",       // print output path on stdout
                "--no-playlist",
                url,
            ])
            .output()
            .await
            .map_err(|e| format!("Failed to spawn yt-dlp: {}", e))?;

        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();

            let _ = app.emit("download-progress", DownloadProgress {
                video_id: video_id.to_string(),
                resolution: resolution.to_string(),
                percent: 100.0,
                status: "complete".to_string(),
            });

            Ok(path)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = app.emit("download-progress", DownloadProgress {
                video_id: video_id.to_string(),
                resolution: resolution.to_string(),
                percent: 0.0,
                status: "error".to_string(),
            });
            Err(format!("yt-dlp failed: {}", stderr))
        }
    }
}
```

**Step 2: Add Tauri command for frontend**

In `lib.rs`, add a command that invokes the downloader:

```rust
#[tauri::command]
async fn download_video(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
    resolution: String,
) -> Result<String, String> {
    let output_dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("videos")
        .to_string_lossy()
        .to_string();

    std::fs::create_dir_all(&output_dir).map_err(|e| e.to_string())?;

    downloader::Downloader::fetch_video(app, &output_dir, &video_id, &resolution, &url).await
}
```

Register it in `generate_handler![]`.

**Step 3: Verify build**

```bash
cd src-tauri && cargo check
```

Expected: compiles, no errors.

---

### Task 3: Transcriber — ffmpeg audio extraction

**Objective:** Add an `extract_audio` method that uses `ffmpeg` to convert a video to 16kHz mono WAV.

**Files:**
- Modify: `src-tauri/src/transcriber.rs`

**Step 1: Add `extract_audio` method**

```rust
use crate::sidecar::resolve_sidecar;

impl Transcriber {
    /// Extract 16kHz mono WAV from a video file using ffmpeg.
    pub async fn extract_audio(video_path: &str, output_wav: &str) -> Result<(), String> {
        let ffmpeg = resolve_sidecar("ffmpeg");

        let output = tauri::process::Command::new(&ffmpeg)
            .args([
                "-i", video_path,
                "-ar", "16000",
                "-ac", "1",
                "-c:a", "pcm_s16le",
                "-y",              // overwrite without prompting
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
}
```

**Step 2: Verify build**

```bash
cd src-tauri && cargo check
```

Expected: compiles.

---

### Task 4: Transcriber — GGML model auto-fetch from HuggingFace

**Objective:** Add `ensure_model` method that checks for the GGML model file, downloads it from HuggingFace if missing, and reports progress.

**Files:**
- Modify: `src-tauri/src/transcriber.rs`
- Modify: `src-tauri/Cargo.toml` (if not already done in Task 0)

**Step 1: Add model download logic**

```rust
use tauri::AppHandle;
use tauri::Emitter;

#[derive(Debug, Clone, Serialize)]
pub struct ModelDownloadProgress {
    pub percent: f64,
    pub status: String, // "downloading" | "complete" | "exists"
}

const MODEL_URL_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
const MODEL_NAME: &str = "ggml-tiny.bin"; // ~78MB, fast enough for local CPU

impl Transcriber {
    /// Ensure the whisper GGML model exists in the models cache dir.
    /// Downloads from HuggingFace if missing, emitting progress events.
    pub async fn ensure_model(app: AppHandle) -> Result<String, String> {
        let models_dir = app
            .path()
            .app_cache_dir()
            .map_err(|e| e.to_string())?
            .join("models");

        std::fs::create_dir_all(&models_dir).map_err(|e| e.to_string())?;

        let model_path = models_dir.join(MODEL_NAME);
        let model_path_str = model_path.to_string_lossy().to_string();

        if model_path.exists() {
            let _ = app.emit("model-download-progress", ModelDownloadProgress {
                percent: 100.0,
                status: "exists".to_string(),
            });
            return Ok(model_path_str);
        }

        let url = format!("{}/{}", MODEL_URL_BASE, MODEL_NAME);

        // Download with reqwest
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
                let _ = app.emit("model-download-progress", ModelDownloadProgress {
                    percent: pct,
                    status: "downloading".to_string(),
                });
            }
        }

        let _ = app.emit("model-download-progress", ModelDownloadProgress {
            percent: 100.0,
            status: "complete".to_string(),
        });

        Ok(model_path_str)
    }
}
```

**Step 2: Add `futures-util` dependency**

In `Cargo.toml`, add:
```toml
futures-util = "0.3"
```

**Step 3: Verify build**

```bash
cd src-tauri && cargo check
```

Expected: compiles.

---

### Task 5: Transcriber — whisper-cli invocation

**Objective:** Add `transcribe` method that runs `whisper-cli` on the extracted WAV to produce a VTT transcript.

**Files:**
- Modify: `src-tauri/src/transcriber.rs`

**Step 1: Add `transcribe` method**

```rust
impl Transcriber {
    /// Run whisper-cli on a 16kHz mono WAV to produce a VTT transcript.
    pub async fn transcribe(model_path: &str, wav_path: &str, output_prefix: &str) -> Result<String, String> {
        let whisper = resolve_sidecar("whisper-cli");

        let output = tauri::process::Command::new(&whisper)
            .args([
                "-m", model_path,
                "-f", wav_path,
                "-ovtt",
                "-of", output_prefix,
                "-l", "auto",          // auto-detect language
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
```

**Step 2: Verify build**

```bash
cd src-tauri && cargo check
```

Expected: compiles.

---

### Task 6: Full pipeline Tauri command

**Objective:** Wire a single `process_video` command that orchestrates the full pipeline: download preview → extract audio → ensure model → transcribe, all while emitting progress events.

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Step 1: Add the pipeline command**

```rust
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct PipelineProgress {
    pub video_id: String,
    pub stage: String,     // "download_preview" | "extract_audio" | "transcribe" | "done" | "error"
    pub message: String,
}

#[tauri::command]
async fn process_video(
    app: tauri::AppHandle,
    video_id: String,
    url: String,
) -> Result<serde_json::Value, String> {
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;

    // Stage 1: Download 720p preview
    let _ = app.emit("pipeline-progress", PipelineProgress {
        video_id: video_id.clone(),
        stage: "download_preview".to_string(),
        message: "Downloading 720p preview...".to_string(),
    });

    let videos_dir = cache.join("videos");
    std::fs::create_dir_all(&videos_dir).map_err(|e| e.to_string())?;

    let preview_path = downloader::Downloader::fetch_video(
        app.clone(),
        &videos_dir.to_string_lossy(),
        &video_id,
        "720p",
        &url,
    )
    .await
    .map_err(|e| format!("Download failed: {}", e))?;

    // Stage 2: Extract audio
    let _ = app.emit("pipeline-progress", PipelineProgress {
        video_id: video_id.clone(),
        stage: "extract_audio".to_string(),
        message: "Extracting 16kHz mono audio...".to_string(),
    });

    let wav_path = cache
        .join("audio")
        .join(format!("{}.wav", &video_id));
    std::fs::create_dir_all(wav_path.parent().unwrap()).map_err(|e| e.to_string())?;

    transcriber::Transcriber::extract_audio(
        &preview_path,
        &wav_path.to_string_lossy(),
    )
    .await
    .map_err(|e| format!("Audio extraction failed: {}", e))?;

    // Stage 3: Ensure model + transcribe
    let _ = app.emit("pipeline-progress", PipelineProgress {
        video_id: video_id.clone(),
        stage: "transcribe".to_string(),
        message: "Transcribing...".to_string(),
    });

    let model_path = transcriber::Transcriber::ensure_model(app.clone()).await
        .map_err(|e| format!("Model setup failed: {}", e))?;

    let output_prefix = cache
        .join("transcripts")
        .join(&video_id)
        .to_string_lossy()
        .to_string();
    std::fs::create_dir_all(
        std::path::Path::new(&output_prefix).parent().unwrap()
    ).map_err(|e| e.to_string())?;

    let vtt_path = transcriber::Transcriber::transcribe(
        &model_path,
        &wav_path.to_string_lossy(),
        &output_prefix,
    )
    .await
    .map_err(|e| format!("Transcription failed: {}", e))?;

    let _ = app.emit("pipeline-progress", PipelineProgress {
        video_id: video_id.clone(),
        stage: "done".to_string(),
        message: format!("Transcript ready: {}", vtt_path),
    });

    Ok(serde_json::json!({
        "video_id": video_id,
        "preview_path": preview_path,
        "transcript_path": vtt_path,
    }))
}
```

Register `process_video` in `generate_handler![]` alongside `download_video`.

**Step 2: Verify build**

```bash
cd src-tauri && cargo check
```

Expected: compiles, no errors.

---

### Task 7: Frontend — Tauri API call for process_video

**Objective:** Add a button in the React UI that accepts a URL and calls `process_video`, displaying progress in console (placeholder for later UI).

**Files:**
- Modify: `src/App.tsx`

**Step 1: Wire up basic invoke call**

Add a test harness to the placeholder UI:

```tsx
import { invoke } from "@tauri-apps/api/core";

function App() {
  const [url, setUrl] = useState("");
  const [status, setStatus] = useState("");

  const handleProcess = async () => {
    setStatus("Starting...");
    try {
      const result = await invoke("process_video", {
        videoId: crypto.randomUUID(),
        url: url,
      });
      setStatus(`Done: ${JSON.stringify(result)}`);
    } catch (e) {
      setStatus(`Error: ${e}`);
    }
  };

  return (
    <div className="app-container">
      <header className="toolbar">
        <input
          type="text"
          placeholder="Paste YouTube URL..."
          value={url}
          onChange={(e) => setUrl(e.target.value)}
        />
        <button onClick={handleProcess} disabled={!url}>
          Process
        </button>
        <span className="status">{status}</span>
      </header>
      {/* ... existing layout ... */}
    </div>
  );
}
```

**Step 2: Verify frontend builds**

```bash
pnpm build
```

Expected: builds, no TS errors.

---

### Task 8: Integration smoke test (manual, with real binaries)

**Objective:** Verify the pipeline works end-to-end once `yt-dlp`, `ffmpeg`, and `whisper-cli` binaries are placed on PATH.

**Step 1: Ensure binaries are installed**

```bash
which yt-dlp ffmpeg whisper-cli || echo "Missing — install: pip install yt-dlp && sudo apt install ffmpeg"
```

Note: `whisper-cli` must be compiled from `whisper.cpp` — see [ggerganov/whisper.cpp](https://github.com/ggerganov/whisper.cpp). This is a Phase 2 prerequisite, not something we build in this phase.

**Step 2: Run Tauri dev**

```bash
cd src-tauri && cargo tauri dev
```

**Step 3: Paste a short YouTube URL and click Process**

Expected: downloads video, extracts audio, downloads model (~78MB), transcribes, returns VTT path.

---

## Verification Checklist

- [ ] `cargo check` passes with no new errors
- [ ] `pnpm build` passes with no TS errors
- [ ] `download_video` command invokes yt-dlp with correct `-f` for 720p/1080p/4k
- [ ] `extract_audio` runs ffmpeg with `-ar 16000 -ac 1 -c:a pcm_s16le`
- [ ] `ensure_model` downloads `ggml-tiny.bin` from HuggingFace, skips if cached
- [ ] `transcribe` runs whisper-cli with `-ovtt` and produces a `.vtt` file
- [ ] `process_video` orchestrates full pipeline and emits progress events
- [ ] Sidecar resolution works in dev (PATH) and production (bundled with target-triple)
