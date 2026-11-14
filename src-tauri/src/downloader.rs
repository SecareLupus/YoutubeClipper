use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tauri::Emitter;

use crate::sidecar::resolve_sidecar;

/// Basic video metadata from `yt-dlp -J`, plus the caption track we picked.
pub struct VideoInfo {
    pub info_json_path: String,
    pub title: String,
    pub duration: f64,
    /// (language key, is_automatic) of the best English caption track, if any.
    pub caption_track: Option<(String, bool)>,
    /// All caption language keys offered (manual + automatic), for messages.
    pub available_langs: Vec<String>,
}

/// Resolution used for the editing copy (section or full video). 720p is the
/// lowest YouTube tier that carries 60fps, so edits happen at the real frame
/// rate for both 30fps and 60fps sources.
pub const EDIT_RESOLUTION: &str = "720p";

pub fn max_height(resolution: &str) -> u32 {
    match resolution {
        "best" => 2160,
        "1080p" => 1080,
        "720p" => 720,
        _ => 480,
    }
}

fn format_selector(resolution: &str) -> String {
    format!(
        "bestvideo[height<={0}][vcodec^=avc1]+bestaudio[acodec^=mp4a]/bestvideo[height<={0}]+bestaudio/best[height<={0}]",
        max_height(resolution)
    )
}

/// Progress callback receiving an overall percentage (0–100).
pub type ProgressFn = std::sync::Arc<dyn Fn(f64) + Send + Sync>;

pub struct Downloader;

impl Downloader {
    /// Fetch video metadata (title, duration, caption tracks) with one
    /// extraction. The info JSON is saved so the caption download can reuse
    /// it via `--load-info-json` instead of hitting YouTube a second time.
    pub async fn fetch_info(
        app: &AppHandle,
        output_dir: &str,
        video_id: &str,
        url: &str,
    ) -> Result<VideoInfo, String> {
        let args: Vec<String> = vec!["-J".into(), "--no-playlist".into(), url.into()];
        let out = run_ytdlp_capture_stdout(app, &args).await?;
        let info: serde_json::Value = serde_json::from_slice(&out)
            .map_err(|e| format!("Couldn't parse video info from yt-dlp: {e}"))?;

        let info_json_path = format!("{}/{}.info.json", output_dir, video_id);
        std::fs::write(&info_json_path, &out).map_err(|e| e.to_string())?;

        let title = info["title"].as_str().unwrap_or_default().to_string();
        let duration = info["duration"].as_f64().unwrap_or(0.0);

        let manual = lang_keys(&info["subtitles"]);
        let auto = lang_keys(&info["automatic_captions"]);
        let caption_track = pick_english_track(&manual, &auto);

        let mut available_langs: Vec<String> =
            manual.iter().chain(auto.iter()).cloned().collect();
        available_langs.sort();
        available_langs.dedup();

        Ok(VideoInfo {
            info_json_path,
            title,
            duration,
            caption_track,
            available_langs,
        })
    }

    /// Download the chosen caption track as VTT, reusing the saved info JSON.
    /// Returns the path of the downloaded `.vtt`.
    pub async fn fetch_captions(
        app: &AppHandle,
        output_dir: &str,
        video_id: &str,
        info: &VideoInfo,
    ) -> Result<String, String> {
        let (lang, is_auto) = info
            .caption_track
            .as_ref()
            .ok_or("No English caption track")?;

        emit_pipeline(app, video_id, "fetching_transcript", "Downloading captions...", 40.0);

        let mut args: Vec<String> = vec![
            "--load-info-json".into(),
            info.info_json_path.clone(),
            "--skip-download".into(),
            if *is_auto { "--write-auto-subs".into() } else { "--write-subs".into() },
            "--sub-langs".into(),
            lang.clone(),
            "--sub-format".into(),
            "vtt/best".into(),
            "--convert-subs".into(),
            "vtt".into(),
            "-o".into(),
            format!("{}/{}.%(ext)s", output_dir, video_id),
        ];
        add_ffmpeg_location(app, &mut args);

        run_ytdlp(app, &args, None, std::sync::Arc::new(|_| {})).await?;

        let expected = format!("{}/{}.{}.vtt", output_dir, video_id, lang);
        if Path::new(&expected).exists() {
            return Ok(expected);
        }
        // Fall back to whatever VTT yt-dlp wrote for this id.
        find_with_prefix(output_dir, &format!("{}.", video_id), ".vtt").ok_or_else(|| {
            format!("yt-dlp finished but no caption file was written for '{lang}'")
        })
    }

    /// Download a time-bounded section of the video at `resolution`.
    /// `start_sec`/`end_sec` are whole seconds of absolute video time; integer
    /// starts land on the frame grid of 24/25/30/50/60fps sources.
    /// `on_progress` receives overall 0–100.
    pub async fn fetch_section(
        app: &AppHandle,
        output_dir: &str,
        video_id: &str,
        url: &str,
        start_sec: u64,
        end_sec: u64,
        resolution: &str,
        on_progress: ProgressFn,
    ) -> Result<String, String> {
        let stem = format!(
            "{}/{}_section_{}_{}_{}",
            output_dir, video_id, resolution, start_sec, end_sec
        );
        let args: Vec<String> = vec![
            "--download-sections".into(),
            format!("*{}-{}", start_sec, end_sec),
        ];
        // Section downloads go through ffmpeg, which reports elapsed output
        // time rather than percentages.
        let secs = end_sec.saturating_sub(start_sec) as f64;
        download_video(app, url, resolution, &stem, args, Some(secs), on_progress).await
    }

    /// Download the full video at `resolution` (used for manual trimming).
    pub async fn fetch_full_video(
        app: &AppHandle,
        output_dir: &str,
        video_id: &str,
        url: &str,
        resolution: &str,
        on_progress: ProgressFn,
    ) -> Result<String, String> {
        let stem = format!("{}/{}_full_{}", output_dir, video_id, resolution);
        download_video(app, url, resolution, &stem, Vec::new(), None, on_progress).await
    }

    /// Download only the best audio stream (for whisper transcription).
    pub async fn fetch_audio(
        app: &AppHandle,
        output_dir: &str,
        video_id: &str,
        url: &str,
    ) -> Result<String, String> {
        let prefix = format!("{}.audio.", video_id);
        let finished = |p: &String| !p.ends_with(".part") && !p.ends_with(".ytdl");
        if let Some(existing) = find_with_prefix(output_dir, &prefix, "").filter(finished) {
            return Ok(existing);
        }
        let mut args: Vec<String> = vec![
            "-f".into(),
            "bestaudio/best".into(),
            "-o".into(),
            format!("{}/{}.audio.%(ext)s", output_dir, video_id),
            "--newline".into(),
            "--no-playlist".into(),
            url.into(),
        ];
        add_ffmpeg_location(app, &mut args);

        let app_c = app.clone();
        let vid = video_id.to_string();
        let progress: ProgressFn = std::sync::Arc::new(move |pct| {
            emit_pipeline(
                &app_c,
                &vid,
                "download",
                &format!("Downloading audio... {:.0}%", pct),
                5.0 + pct * 0.25,
            );
        });
        run_ytdlp(app, &args, None, progress).await?;

        find_with_prefix(output_dir, &prefix, "")
            .filter(finished)
            .ok_or_else(|| "yt-dlp finished but no audio file was written".to_string())
    }
}

/// Shared video download: `<stem>.mp4`, format chosen by `resolution`.
/// Reuses an existing finished file (the stem includes the resolution, so a
/// different quality never collides with a cached one).
async fn download_video(
    app: &AppHandle,
    url: &str,
    resolution: &str,
    stem: &str,
    extra_args: Vec<String>,
    section_secs: Option<f64>,
    on_progress: ProgressFn,
) -> Result<String, String> {
    let output_path = format!("{stem}.mp4");
    if Path::new(&output_path).exists() {
        on_progress(100.0);
        return Ok(output_path);
    }
    on_progress(0.0);

    let mut args: Vec<String> = vec!["-f".into(), format_selector(resolution)];
    args.extend(extra_args);
    args.extend([
        "-o".into(),
        format!("{stem}.%(ext)s"),
        "--merge-output-format".into(),
        "mp4".into(),
        "--newline".into(),
        "--no-playlist".into(),
        url.into(),
    ]);
    add_ffmpeg_location(app, &mut args);

    run_ytdlp(app, &args, section_secs, on_progress.clone()).await?;
    if Path::new(&output_path).exists() {
        on_progress(100.0);
        Ok(output_path)
    } else {
        Err(format!("yt-dlp finished but {output_path} was not created"))
    }
}

// ── caption track selection ────────────────────────────────────────────────

fn lang_keys(v: &serde_json::Value) -> Vec<String> {
    v.as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

fn is_english(key: &str) -> bool {
    key == "en" || key.starts_with("en-") || key.starts_with("en_")
}

/// Pick the best English track. Preference:
/// 1. manual `en`, 2. manual `en-*` (en-US, en-GB, ...),
/// 3. auto `en-orig` (original-language ASR), 4. auto `en`, 5. auto `en-*`.
/// Returns (language key, is_automatic).
pub fn pick_english_track(manual: &[String], auto: &[String]) -> Option<(String, bool)> {
    let best = |keys: &[String], exact: &[&str]| -> Option<String> {
        for e in exact {
            if keys.iter().any(|k| k == e) {
                return Some(e.to_string());
            }
        }
        let mut rest: Vec<&String> = keys.iter().filter(|k| is_english(k)).collect();
        // Shorter keys first: "en-US" before "en-en-nP7-2PuUl7o" (a translation).
        rest.sort_by_key(|k| (k.len(), (*k).clone()));
        rest.first().map(|k| (*k).clone())
    };
    if let Some(k) = best(manual, &["en"]) {
        return Some((k, false));
    }
    best(auto, &["en-orig", "en"]).map(|k| (k, true))
}

// ── yt-dlp process runner ──────────────────────────────────────────────────

fn ytdlp_command(app: &AppHandle) -> tokio::process::Command {
    let ytdlp = resolve_sidecar(app, "yt-dlp");
    let mut cmd = tokio::process::Command::new(&ytdlp);
    // The AppImage sets LD_LIBRARY_PATH, LD_PRELOAD, PYTHONHOME, and other
    // vars that break PyInstaller's _MEIPASS extraction. Clear inherited env
    // and pass only what yt-dlp needs.
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("USER", std::env::var("USER").unwrap_or_default())
        .env(
            "LANG",
            std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into()),
        );
    for var in [
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, v);
        }
    }
    cmd.kill_on_drop(true);
    cmd
}

/// Point yt-dlp at our ffmpeg for merging / section cuts / sub conversion,
/// so it works on hosts without a system ffmpeg.
fn add_ffmpeg_location(app: &AppHandle, args: &mut Vec<String>) {
    let ffmpeg = resolve_sidecar(app, "ffmpeg");
    if Path::new(&ffmpeg).is_absolute() && Path::new(&ffmpeg).exists() {
        args.insert(0, ffmpeg);
        args.insert(0, "--ffmpeg-location".into());
    }
}

/// Summarise a failed run: prefer yt-dlp's ERROR lines, else the last lines.
fn summarize_failure(lines: &[String], code: Option<i32>) -> String {
    let errors: Vec<&String> = lines.iter().filter(|l| l.starts_with("ERROR")).collect();
    let picked: Vec<&String> = if errors.is_empty() {
        let n = lines.len();
        lines[n.saturating_sub(4)..].iter().collect()
    } else {
        errors
    };
    let detail = picked
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "yt-dlp failed (exit {}){}{}",
        code.map_or("signal".to_string(), |c| c.to_string()),
        if detail.is_empty() { "" } else { ": " },
        detail
    )
}

/// Turns yt-dlp / ffmpeg output lines into one monotonic overall percentage.
///
/// - Normal downloads print `[download]  NN.N%` per format; a merged
///   video+audio download runs 0→100 once per format, so each format
///   (announced by `[download] Destination:`) gets a slice of the bar.
/// - `--download-sections` downloads run through ffmpeg, which prints only
///   `time=HH:MM:SS.xx` (relative to the section start, negative while it
///   pre-rolls); progress is that time over the section length.
pub struct ProgressTracker {
    section_secs: Option<f64>,
    formats: usize,
    phase: usize,
    last: f64,
}

impl ProgressTracker {
    pub fn new(section_secs: Option<f64>) -> Self {
        Self { section_secs, formats: 1, phase: 0, last: -1.0 }
    }

    /// Feed one output line; returns a new percentage when it advanced.
    pub fn feed(&mut self, line: &str) -> Option<f64> {
        let pct = if let Some(rest) = line.split("format(s):").nth(1) {
            // "[info] ID: Downloading 1 format(s): 137+140"
            self.formats = rest.trim().split('+').count().max(1);
            return None;
        } else if line.starts_with("[download] Destination:") {
            self.phase += 1;
            return None;
        } else if line.starts_with("[Merger]") {
            99.0
        } else if let Some(secs) = self.section_secs {
            let t = parse_ffmpeg_time(line)?;
            if secs <= 0.0 {
                return None;
            }
            (t.max(0.0) / secs * 100.0).min(99.0)
        } else {
            let p = parse_download_percent(line)?.clamp(0.0, 100.0);
            // Video carries most of the bytes in a video+audio download.
            let weights: Vec<f64> = match self.formats {
                1 => vec![1.0],
                2 => vec![0.85, 0.15],
                n => vec![1.0 / n as f64; n],
            };
            let idx = self.phase.saturating_sub(1).min(weights.len() - 1);
            let done: f64 = weights[..idx].iter().sum();
            ((done + weights[idx] * p / 100.0) * 100.0).min(99.0)
        };
        // Monotonic, and skip sub-half-percent changes to limit event spam.
        if pct >= self.last + 0.5 || (pct >= 99.0 && self.last < 99.0) {
            self.last = pct;
            Some(pct)
        } else {
            None
        }
    }
}

/// Parse ffmpeg's "... time=HH:MM:SS.xx ..." (may be negative) into seconds.
pub fn parse_ffmpeg_time(line: &str) -> Option<f64> {
    let idx = line.find("time=")?;
    let s = line[idx + 5..].split_whitespace().next()?;
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    let t = h * 3600.0 + m * 60.0 + sec;
    Some(if neg { -t } else { t })
}

/// Read `reader`, calling `f` for every line terminated by `\n` OR `\r`
/// (ffmpeg rewrites its stats line with `\r`, so `lines()` would only see
/// it once the process exits).
pub async fn for_each_line<R, F>(reader: R, mut f: F)
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(&str),
{
    use tokio::io::AsyncReadExt;
    let mut reader = reader;
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                if !pending.is_empty() {
                    f(String::from_utf8_lossy(&pending).trim());
                    pending.clear();
                }
            } else {
                pending.push(b);
            }
        }
    }
    if !pending.is_empty() {
        f(String::from_utf8_lossy(&pending).trim());
    }
}

/// Run yt-dlp, streaming both stdout and stderr through a ProgressTracker
/// (`section_secs` = Some(len) for `--download-sections` runs). On failure,
/// returns yt-dlp's ERROR lines.
async fn run_ytdlp(
    app: &AppHandle,
    args: &[String],
    section_secs: Option<f64>,
    on_progress: ProgressFn,
) -> Result<(), String> {
    eprintln!("[better-clipper] yt-dlp {}", args.join(" "));
    let mut child = ytdlp_command(app)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn yt-dlp: {}", e))?;

    let stdout = child.stdout.take().ok_or("Failed to capture yt-dlp stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to capture yt-dlp stderr")?;

    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tracker = std::sync::Arc::new(std::sync::Mutex::new(ProgressTracker::new(section_secs)));

    let readers: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> =
        vec![Box::new(stdout), Box::new(stderr)];
    let mut tasks = Vec::new();
    for reader in readers {
        let lines = lines.clone();
        let tracker = tracker.clone();
        let on_progress = on_progress.clone();
        tasks.push(tokio::spawn(async move {
            for_each_line(reader, |line| {
                if line.is_empty() {
                    return;
                }
                let advanced = tracker.lock().unwrap().feed(line);
                if let Some(pct) = advanced {
                    on_progress(pct);
                }
                // Keep diagnostics, but not the per-tick progress lines.
                if line.starts_with("frame=") || parse_download_percent(line).is_some() {
                    return;
                }
                let mut l = lines.lock().unwrap();
                l.push(line.to_string());
                if l.len() > 200 {
                    l.remove(0);
                }
            })
            .await;
        }));
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("yt-dlp process error: {}", e))?;
    for t in tasks {
        let _ = t.await;
    }

    if status.success() {
        Ok(())
    } else {
        let l = lines.lock().unwrap();
        eprintln!("[better-clipper] yt-dlp failed; last output:");
        for line in l.iter().rev().take(20).rev() {
            eprintln!("[better-clipper]   {line}");
        }
        Err(summarize_failure(&l, status.code()))
    }
}

/// Run yt-dlp and return its stdout bytes (for `-J`).
async fn run_ytdlp_capture_stdout(app: &AppHandle, args: &[String]) -> Result<Vec<u8>, String> {
    eprintln!("[better-clipper] yt-dlp {}", args.join(" "));
    let out = ytdlp_command(app)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("Failed to spawn yt-dlp: {}", e))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let lines: Vec<String> = String::from_utf8_lossy(&out.stderr)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        eprintln!("[better-clipper] yt-dlp -J failed: {:?}", lines);
        Err(summarize_failure(&lines, out.status.code()))
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

pub fn emit_pipeline(app: &AppHandle, video_id: &str, stage: &str, message: &str, percent: f64) {
    let _ = app.emit(
        "pipeline-progress",
        crate::PipelineProgress {
            video_id: video_id.to_string(),
            stage: stage.to_string(),
            message: message.to_string(),
            percent,
        },
    );
}

/// First file in `dir` whose name starts with `prefix` and ends with `suffix`.
fn find_with_prefix(dir: &str, prefix: &str, suffix: &str) -> Option<String> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(prefix) && n.ends_with(suffix))
                .unwrap_or(false)
        })
        .collect();
    found.sort();
    found.first().map(|p| p.to_string_lossy().to_string())
}

/// Parse yt-dlp progress output like "[download]  42.3% of ~50.00MiB at ..."
fn parse_download_percent(line: &str) -> Option<f64> {
    let after_prefix = line.strip_prefix("[download]")?.trim_start();
    let pct_str = after_prefix.split('%').next()?;
    pct_str.trim().parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn prefers_manual_en() {
        assert_eq!(
            pick_english_track(&s(&["en", "de"]), &s(&["en-orig", "en"])),
            Some(("en".into(), false))
        );
    }

    #[test]
    fn accepts_regional_manual() {
        assert_eq!(
            pick_english_track(&s(&["en-US", "en-en-xyz"]), &s(&[])),
            Some(("en-US".into(), false))
        );
    }

    #[test]
    fn falls_back_to_auto_orig_then_en() {
        assert_eq!(
            pick_english_track(&s(&["de"]), &s(&["en", "en-orig", "fr"])),
            Some(("en-orig".into(), true))
        );
        assert_eq!(
            pick_english_track(&s(&[]), &s(&["ko-orig", "en"])),
            Some(("en".into(), true))
        );
    }

    #[test]
    fn none_when_no_english() {
        assert_eq!(
            pick_english_track(&s(&["ko"]), &s(&["ko-orig", "es"])),
            None
        );
    }

    /// Live check: real yt-dlp process -> for_each_line -> ProgressTracker.
    /// BC_PROG_ARGS = yt-dlp args (space-separated), BC_PROG_SECS = section len.
    #[tokio::test]
    #[ignore]
    async fn live_progress() {
        let args: Vec<String> = std::env::var("BC_PROG_ARGS").unwrap()
            .split(' ').map(String::from).collect();
        let secs = std::env::var("BC_PROG_SECS").ok().and_then(|s| s.parse().ok());
        let mut child = tokio::process::Command::new("yt-dlp").args(&args)
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        let t = std::sync::Arc::new(std::sync::Mutex::new(ProgressTracker::new(secs)));
        let start = std::time::Instant::now();
        let mut tasks = Vec::new();
        let readers: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> =
            vec![Box::new(child.stdout.take().unwrap()), Box::new(child.stderr.take().unwrap())];
        for r in readers {
            let t = t.clone();
            tasks.push(tokio::spawn(async move {
                for_each_line(r, |l| {
                    if let Some(p) = t.lock().unwrap().feed(l) {
                        println!("{:6.2}s {p:5.1}%", start.elapsed().as_secs_f64());
                    }
                }).await;
            }));
        }
        assert!(child.wait().await.unwrap().success());
        for x in tasks { x.await.unwrap(); }
    }

    #[test]
    fn tracks_section_progress_from_ffmpeg_time() {
        let mut t = ProgressTracker::new(Some(20.0));
        assert_eq!(t.feed("frame=   32 fps=0.0 q=-1.0 size=0KiB time=-00:00:04.16 bitrate=N/A"), Some(0.0));
        assert_eq!(t.feed("frame=  158 fps= 53 q=-1.0 size=768KiB time=00:00:05.00 bitrate=1"), Some(25.0));
        assert_eq!(t.feed("frame=  158 fps= 53 q=-1.0 size=768KiB time=00:00:05.02 bitrate=1"), None);
        assert_eq!(t.feed("frame=  900 time=00:00:30.00"), Some(99.0));
    }

    #[test]
    fn tracks_merged_download_phases() {
        let mut t = ProgressTracker::new(None);
        t.feed("[info] abc: Downloading 1 format(s): 137+140");
        t.feed("[download] Destination: x.f137.mp4");
        assert_eq!(t.feed("[download]  50.0% of 10MiB"), Some(42.5));
        assert_eq!(t.feed("[download] 100.0% of 10MiB"), Some(85.0));
        t.feed("[download] Destination: x.f140.m4a");
        assert_eq!(t.feed("[download]   0.0% of 1MiB"), None); // no going backwards
        assert_eq!(t.feed("[download] 100.0% of 1MiB"), Some(99.0));
    }

    #[test]
    fn parses_progress() {
        assert_eq!(
            parse_download_percent("[download]  42.3% of ~50.00MiB"),
            Some(42.3)
        );
        assert_eq!(parse_download_percent("[info] foo"), None);
    }
}
