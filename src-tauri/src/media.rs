//! Frame-accurate probing and cutting.
//!
//! Timeline model: every editing file (a downloaded section or the full
//! video) maps local time to absolute video time via its origins:
//!   absolute = video_origin + local   (video stream)
//!   absolute = audio_origin + local   (audio stream)
//! yt-dlp section downloads start the video at the first source frame at or
//! after the requested start, while audio starts at the requested start, so
//! the two origins can differ by up to one frame.
//!
//! A marker at local time `t` means "the frame being displayed at t", i.e. the
//! frame with the largest pts <= t. A clip [In, Out] covers the In frame
//! through the Out frame inclusive, which is the absolute half-open range
//! [pts(In frame), pts(Out frame) + frame duration).
//!
//! Cuts select video frames with half-frame margins so floating-point and
//! timebase rounding can never add or drop a frame.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditSource {
    pub path: String,
    /// Absolute time (s) of local t=0 for the video stream.
    pub video_origin: f64,
    /// Absolute time (s) of local t=0 for the audio stream.
    pub audio_origin: f64,
    /// Duration of one video frame (s).
    pub frame_dur: f64,
}

/// Absolute clip range: [start, end) in seconds of source video time.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ClipRange {
    pub start: f64,
    pub end: f64,
}

const KNOWN_RATES: &[(f64, f64)] = &[
    (24000.0, 1001.0),
    (24.0, 1.0),
    (25.0, 1.0),
    (30000.0, 1001.0),
    (30.0, 1.0),
    (48.0, 1.0),
    (50.0, 1.0),
    (60000.0, 1001.0),
    (60.0, 1.0),
    (120.0, 1.0),
];

/// Snap a measured frame duration to the exact duration of a standard rate.
pub fn snap_frame_dur(measured: f64) -> f64 {
    // Closest standard rate (30 and 29.97 are only 0.1% apart), if within 0.5%.
    KNOWN_RATES
        .iter()
        .map(|(n, d)| d / n)
        .map(|fd| (fd, ((fd - measured) / measured).abs()))
        .filter(|(_, err)| *err < 0.005)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(fd, _)| fd)
        .unwrap_or(measured)
}

/// Frame rate as an ffmpeg rate string ("30000/1001", "25", ...).
pub fn rate_string(fd: f64) -> String {
    for (n, d) in KNOWN_RATES {
        if (d / n - fd).abs() < 1e-9 {
            return if *d == 1.0 { format!("{n}") } else { format!("{n}/{d}") };
        }
    }
    format!("{:.6}", 1.0 / fd)
}

/// First point on the frame grid (multiples of `fd` from 0) at or after `s`.
pub fn ceil_to_grid(s: f64, fd: f64) -> f64 {
    if s <= 0.0 {
        return 0.0;
    }
    (s / fd - 1e-3).ceil() * fd
}

/// Video frame presentation times (seconds, the file's own timeline) of the
/// frames between `from` and `to`, via ffmpeg's showinfo filter.
pub async fn frame_times(ffmpeg: &str, path: &str, from: f64, to: f64) -> Result<Vec<f64>, String> {
    let from = from.max(0.0);
    let out = tokio::process::Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-nostats",
            "-copyts",
            "-ss",
            &format!("{:.6}", from),
            "-t",
            &format!("{:.6}", (to - from).max(0.05)),
            "-i",
            path,
            "-map",
            "0:v:0",
            "-vf",
            "showinfo",
            "-f",
            "null",
            "-",
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffmpeg frame probe failed: {}",
            tail(&String::from_utf8_lossy(&out.stderr), 3)
        ));
    }
    let mut times: Vec<f64> = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter_map(|l| {
            let rest = &l[l.find(" pts_time:")? + " pts_time:".len()..];
            rest.split_whitespace().next()?.parse::<f64>().ok()
        })
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Ok(times)
}

/// Exact frame duration of a file's video stream, from its first frames.
pub async fn probe_frame_dur(ffmpeg: &str, path: &str) -> Result<f64, String> {
    let times = frame_times(ffmpeg, path, 0.0, 2.0).await?;
    if times.len() < 3 {
        return Err(format!("Couldn't read frames from {path}"));
    }
    // Average over the window: per-frame deltas are quantised by the
    // container timebase (e.g. 29.97fps in 1/12800 alternates 427/428 ticks).
    let mean = (times[times.len() - 1] - times[0]) / (times.len() - 1) as f64;
    Ok(snap_frame_dur(mean))
}

/// Given the frame times near `t`, return (pts of the frame displayed at `t`,
/// pts at which that frame ends). Pure; see `snap_marker`.
pub fn displayed_frame(times: &[f64], t: f64, fd: f64) -> Option<(f64, f64)> {
    if times.is_empty() {
        return None;
    }
    // A frame is "displayed" from its pts; allow 0.5ms for float noise in the
    // time the webview reports.
    let idx = match times.iter().rposition(|p| *p <= t + 0.0005) {
        Some(i) => i,
        None => 0, // before the first frame: the first frame is shown
    };
    let pts = times[idx];
    let end = times.get(idx + 1).copied().unwrap_or(pts + fd);
    Some((pts, end))
}

/// Snap a local marker to the displayed frame: returns (frame pts, frame end).
pub async fn snap_marker(ffmpeg: &str, src: &EditSource, t: f64) -> Result<(f64, f64), String> {
    let times = frame_times(ffmpeg, &src.path, t - 1.0, t + 1.0).await?;
    displayed_frame(&times, t, src.frame_dur)
        .ok_or_else(|| format!("No video frames near {t:.3}s"))
}

/// Absolute clip range for local In/Out markers on an editing file.
pub async fn clip_range(ffmpeg: &str, src: &EditSource, in_sec: f64, out_sec: f64) -> Result<ClipRange, String> {
    let (in_pts, _) = snap_marker(ffmpeg, src, in_sec).await?;
    let (_, out_end) = snap_marker(ffmpeg, src, out_sec.max(in_sec)).await?;
    if out_end <= in_pts {
        return Err("Out marker must be after In marker".into());
    }
    Ok(ClipRange {
        start: src.video_origin + in_pts,
        end: src.video_origin + out_end,
    })
}

pub struct EncodeOpts<'a> {
    pub preset: &'a str,
    pub crf: &'a str,
    pub audio_bitrate: &'a str,
}

/// Cut `range` (absolute) out of `src` and re-encode to `out_path`.
/// Writes to a temporary file and renames on success, so a failed or
/// interrupted encode never leaves a half-written file at `out_path`.
pub async fn cut<F>(
    ffmpeg: &str,
    src: &EditSource,
    range: ClipRange,
    out_path: &str,
    opts: EncodeOpts<'_>,
    on_progress: F,
) -> Result<(), String>
where
    F: Fn(f64) + Send + 'static,
{
    use tokio::io::AsyncBufReadExt;

    let half = src.frame_dur / 2.0;
    let v_start = (range.start - src.video_origin - half).max(0.0);
    let v_end = range.end - src.video_origin - half;
    let a_start = (range.start - src.audio_origin).max(0.0);
    let a_end = range.end - src.audio_origin;
    // Fast input seek to a little before the cut, keeping original
    // timestamps (-copyts) so trim/atrim can use file-relative times.
    let seek = (v_start.min(a_start) - 3.0).max(0.0);
    let read_for = v_end.max(a_end) - seek + 1.0;

    let filter = format!(
        "[0:v:0]trim=start={v_start:.6}:end={v_end:.6},setpts=PTS-STARTPTS[v];\
         [0:a:0]atrim=start={a_start:.6}:end={a_end:.6},asetpts=PTS-STARTPTS[a]"
    );
    let tmp_path = format!("{out_path}.part.mp4");
    let rate = rate_string(src.frame_dur);
    eprintln!("[better-clipper] cut {} [{:.4}, {:.4}) -> {}", src.path, range.start, range.end, out_path);

    let mut child = tokio::process::Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-y",
            "-copyts",
            "-ss",
            &format!("{seek:.6}"),
            "-t",
            &format!("{read_for:.6}"),
            "-i",
            &src.path,
            "-filter_complex",
            &filter,
            "-map",
            "[v]",
            "-map",
            "[a]",
            // Explicit output rate: without it the mp4 muxer gives the final
            // frame zero duration and the edit list hides it (drops 1 frame).
            "-r",
            &rate,
            "-c:v",
            "libx264",
            "-preset",
            opts.preset,
            "-crf",
            opts.crf,
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            opts.audio_bitrate,
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            &tmp_path,
        ])
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;

    let stderr = child.stderr.take().ok_or("Failed to capture ffmpeg stderr")?;
    let duration = range.end - range.start;
    let reader_task = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).split(b'\r');
        let mut last: Vec<String> = Vec::new();
        while let Ok(Some(chunk)) = lines.next_segment().await {
            let text = String::from_utf8_lossy(&chunk).to_string();
            for line in text.lines() {
                if let Some(t) = parse_ffmpeg_time(line) {
                    if duration > 0.0 {
                        on_progress((t / duration * 100.0).clamp(0.0, 99.0));
                    }
                } else if !line.trim().is_empty() {
                    last.push(line.trim().to_string());
                    if last.len() > 20 {
                        last.remove(0);
                    }
                }
            }
        }
        last
    });

    let status = child.wait().await.map_err(|e| format!("ffmpeg process error: {e}"))?;
    let last = reader_task.await.unwrap_or_default();
    if !status.success() {
        let _ = std::fs::remove_file(&tmp_path);
        let n = last.len();
        return Err(format!("ffmpeg cut failed: {}", last[n.saturating_sub(3)..].join(" | ")));
    }
    std::fs::rename(&tmp_path, out_path).map_err(|e| format!("Couldn't move output into place: {e}"))?;
    Ok(())
}

/// Parse ffmpeg progress: "... time=00:00:05.12 bitrate=..."
fn parse_ffmpeg_time(line: &str) -> Option<f64> {
    let idx = line.find("time=")?;
    let s = line[idx + 5..].split_whitespace().next()?;
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join(" | ")
}

/// End-to-end check on real downloads (run `scripts/e2e-media.sh`, which
/// downloads sections with the app's yt-dlp arguments and runs this).
/// Env: BC_E2E_EDIT (720p section from BC_E2E_START), BC_E2E_EXPORT
/// (higher-res section from BC_E2E_EXPORT_START).
#[cfg(test)]
mod e2e {
    use super::*;

    const FF: &str = "ffmpeg";

    async fn gray(path: &str, n: usize) -> Vec<u8> {
        let out = tokio::process::Command::new(FF)
            .args(["-v", "error", "-i", path, "-vf", &format!("select=eq(n\\,{n}),scale=64:36"),
                   "-frames:v", "1", "-pix_fmt", "gray", "-f", "rawvideo", "-"])
            .output().await.unwrap();
        assert_eq!(out.stdout.len(), 64 * 36, "no frame {n} in {path}");
        out.stdout
    }

    fn diff(a: &[u8], b: &[u8]) -> f64 {
        a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).abs()).sum::<f64>() / a.len() as f64
    }

    async fn frame_count(path: &str) -> usize {
        let out = tokio::process::Command::new(FF)
            .args(["-v", "error", "-i", path, "-map", "0:v:0", "-f", "framemd5", "-"])
            .output().await.unwrap();
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#')).count()
    }

    /// Which of edit-file frames k-1, k, k+1 does `img` match best?
    async fn best_match(edit: &str, k: usize, img: &[u8]) -> isize {
        let mut best = (f64::MAX, 0isize);
        for d in [-1isize, 0, 1] {
            let f = gray(edit, (k as isize + d) as usize).await;
            let e = diff(&f, img);
            if e < best.0 { best = (e, d); }
        }
        best.1
    }

    #[tokio::test]
    #[ignore]
    async fn frame_exact_preview_and_export() {
        let edit_path = std::env::var("BC_E2E_EDIT").expect("BC_E2E_EDIT");
        let export_path = std::env::var("BC_E2E_EXPORT").expect("BC_E2E_EXPORT");
        let start: f64 = std::env::var("BC_E2E_START").unwrap().parse().unwrap();
        let tmp = std::env::temp_dir();

        let fd = probe_frame_dur(FF, &edit_path).await.unwrap();
        let edit = EditSource {
            path: edit_path.clone(),
            video_origin: ceil_to_grid(start, fd),
            audio_origin: start,
            frame_dur: fd,
        };
        // Markers dragged between frames (the old bug's trigger).
        let k_in = 88usize;
        let k_out = 126usize;
        let in_sec = k_in as f64 * fd + fd * 0.37;
        let out_sec = k_out as f64 * fd + fd * 0.61;
        let range = clip_range(FF, &edit, in_sec, out_sec).await.unwrap();
        let want = k_out - k_in + 1;
        eprintln!("fd={fd} range={range:?} want {want} frames");

        // Preview path: cut from the edit file.
        let a = tmp.join("bc_e2e_a.mp4").to_string_lossy().to_string();
        cut(FF, &edit, range, &a, EncodeOpts { preset: "ultrafast", crf: "10", audio_bitrate: "128k" }, |_| {}).await.unwrap();
        assert_eq!(frame_count(&a).await, want, "preview frame count");
        assert_eq!(best_match(&edit_path, k_in, &gray(&a, 0).await).await, 0, "preview first frame");
        assert_eq!(best_match(&edit_path, k_out, &gray(&a, want - 1).await).await, 0, "preview last frame");

        // Export path: cut the same absolute range from a different-resolution download.
        let s2: f64 = std::env::var("BC_E2E_EXPORT_START").unwrap().parse().unwrap();
        let fd2 = probe_frame_dur(FF, &export_path).await.unwrap();
        let exp = EditSource {
            path: export_path.clone(),
            video_origin: ceil_to_grid(s2, fd2),
            audio_origin: s2,
            frame_dur: fd2,
        };
        let b = tmp.join("bc_e2e_b.mp4").to_string_lossy().to_string();
        cut(FF, &exp, range, &b, EncodeOpts { preset: "ultrafast", crf: "10", audio_bitrate: "128k" }, |_| {}).await.unwrap();
        let want_b = ((range.end - range.start) / fd2).round() as usize;
        assert_eq!(frame_count(&b).await, want_b, "export frame count");
        if (fd2 - fd).abs() < 1e-9 {
            assert_eq!(best_match(&edit_path, k_in, &gray(&b, 0).await).await, 0, "export first frame");
            assert_eq!(best_match(&edit_path, k_out, &gray(&b, want_b - 1).await).await, 0, "export last frame");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_standard_rates() {
        assert!((snap_frame_dur(0.0333) - 1.0 / 30.0).abs() < 1e-9);
        assert!((snap_frame_dur(0.03337) - 1001.0 / 30000.0).abs() < 1e-9);
        assert!((snap_frame_dur(427.0 / 12800.0) - 1001.0 / 30000.0).abs() < 1e-9);
        assert!((snap_frame_dur(1.0 / 30.0) - 1.0 / 30.0).abs() < 1e-9);
        assert!((snap_frame_dur(0.04) - 0.04).abs() < 1e-9);
        assert!((snap_frame_dur(0.01668) - 1001.0 / 60000.0).abs() < 1e-9);
    }

    #[test]
    fn grid_ceil() {
        // Matches observed yt-dlp behaviour at 25fps: 33.517 -> 33.52, 33.531 -> 33.56.
        assert!((ceil_to_grid(33.517, 0.04) - 33.52).abs() < 1e-9);
        assert!((ceil_to_grid(33.531, 0.04) - 33.56).abs() < 1e-9);
        assert!((ceil_to_grid(33.0, 0.04) - 33.0).abs() < 1e-9);
        assert_eq!(ceil_to_grid(0.0, 0.04), 0.0);
    }

    #[test]
    fn displayed_frame_rules() {
        let t: Vec<f64> = (0..10).map(|i| i as f64 * 0.04).collect();
        // Exactly on a frame
        assert_eq!(displayed_frame(&t, 0.12, 0.04), Some((0.12, 0.16)));
        // Between frames: the earlier frame is displayed
        assert_eq!(displayed_frame(&t, 0.13, 0.04), Some((0.12, 0.16)));
        // Float noise just below a frame boundary still counts as that frame
        assert_eq!(displayed_frame(&t, 0.1599999, 0.04), Some((0.16, 0.2)));
        // Past the last frame
        assert_eq!(displayed_frame(&t, 5.0, 0.04).map(|x| x.0), Some(0.36));
    }
}
