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
//!
//! Split edits: audio has its own range, snapped to whole frames relative to
//! the video range, so it can start/end before or after the video. The output
//! covers the union of both. Where there is audio but no video, the edge
//! video frame is held (cloned frames, constant frame rate: OBS skips a
//! late-starting video track and freezes an early-ending one anyway); where
//! there is video but no audio, the audio is silent.

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
    /// Requested [start, end] whole seconds of a section download; None for
    /// the full video.
    #[serde(default)]
    pub section: Option<(u64, u64)>,
}

/// Absolute clip ranges in seconds of source video time: video [start, end)
/// and audio [audio_start, audio_end). Audio edges sit a whole number of
/// frames from the video edges (equal when linked).
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct ClipRange {
    pub start: f64,
    pub end: f64,
    pub audio_start: f64,
    pub audio_end: f64,
}

/// How a ClipRange maps onto the output file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// Held copies of the first video frame before it (audio leads video).
    pub head_frames: u64,
    /// Held copies of the last video frame after it (audio trails video).
    pub tail_frames: u64,
    /// Silence before the audio starts (video leads audio), seconds.
    pub audio_delay: f64,
    /// Output length, seconds (whole frames).
    pub duration: f64,
    pub frames: u64,
}

impl ClipRange {
    /// A range whose audio matches its video exactly.
    pub fn linked(start: f64, end: f64) -> Self {
        Self { start, end, audio_start: start, audio_end: end }
    }

    pub fn layout(&self, fd: f64) -> Layout {
        let head = ((self.start - self.audio_start) / fd).round();
        let tail = ((self.audio_end - self.end) / fd).round();
        let body = ((self.end - self.start) / fd).round();
        let frames = (body + head.max(0.0) + tail.max(0.0)) as u64;
        Layout {
            head_frames: head.max(0.0) as u64,
            tail_frames: tail.max(0.0) as u64,
            audio_delay: (-head).max(0.0) * fd,
            duration: frames as f64 * fd,
            frames,
        }
    }

    /// Union of the video and audio ranges.
    pub fn union(&self) -> (f64, f64) {
        (self.start.min(self.audio_start), self.end.max(self.audio_end))
    }
}

/// Snap raw audio edges to whole frames from the video edges.
pub fn snap_audio(start: f64, end: f64, a_start: f64, a_end: f64, fd: f64) -> Result<ClipRange, String> {
    let head = ((start - a_start) / fd).round();
    let tail = ((a_end - end) / fd).round();
    let r = ClipRange { start, end, audio_start: start - head * fd, audio_end: end + tail * fd };
    if r.audio_end - r.audio_start < fd * 0.5 {
        return Err("Audio Out must be after audio In".into());
    }
    Ok(r)
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

/// Absolute clip range for local markers on an editing file. Video In/Out
/// and audio In/Out each mean "the frame displayed at that time" (audio Out
/// includes its frame, like video Out).
pub async fn clip_range(
    ffmpeg: &str,
    src: &EditSource,
    in_sec: f64,
    out_sec: f64,
    audio_in_sec: f64,
    audio_out_sec: f64,
) -> Result<ClipRange, String> {
    let (in_pts, _) = snap_marker(ffmpeg, src, in_sec).await?;
    let (_, out_end) = snap_marker(ffmpeg, src, out_sec.max(in_sec)).await?;
    if out_end <= in_pts {
        return Err("Out marker must be after In marker".into());
    }
    let start = src.video_origin + in_pts;
    let end = src.video_origin + out_end;
    let a_start = if audio_in_sec == in_sec {
        start
    } else {
        src.video_origin + snap_marker(ffmpeg, src, audio_in_sec).await?.0
    };
    let a_end = if audio_out_sec == out_sec {
        end
    } else {
        src.video_origin + snap_marker(ffmpeg, src, audio_out_sec).await?.1
    };
    snap_audio(start, end, a_start, a_end, src.frame_dur)
}

/// ffmpeg filtergraph for a cut: `[v]` and `[a]` outputs on the output timeline.
pub fn cut_filter(src: &EditSource, range: &ClipRange, declick: bool) -> String {
    let fd = src.frame_dur;
    let half = fd / 2.0;
    let lay = range.layout(fd);

    let v_start = (range.start - src.video_origin - half).max(0.0);
    let v_end = range.end - src.video_origin - half;
    let mut v = format!("[0:v:0]trim=start={v_start:.6}:end={v_end:.6},setpts=PTS-STARTPTS");
    if lay.head_frames > 0 || lay.tail_frames > 0 {
        // tpad timestamps its clones from the link's frame rate; without an
        // explicit fps (unknown/VFR after trim) some ffmpeg builds give them
        // near-zero durations and the -r output then drops them.
        v += &format!(
            ",fps={},tpad=start={}:stop={}:start_mode=clone:stop_mode=clone",
            rate_string(fd),
            lay.head_frames,
            lay.tail_frames
        );
    }

    // Audio is cut over the whole output span, then samples outside the
    // audio range are zeroed (aeval is per-sample; adelay produced a burst
    // at the start of the output).
    let (u_start, u_end) = range.union();
    let a_from = (u_start - src.audio_origin).max(0.0);
    let a_to = u_end - src.audio_origin;
    let on = range.audio_start - u_start; // audio begins (output time)
    let off = range.audio_end - u_start; // audio ends (output time)
    let mut a = format!("[0:a:0]atrim=start={a_from:.6}:end={a_to:.6},asetpts=PTS-STARTPTS");
    if on > 1e-6 || off < lay.duration - 1e-6 {
        a += &format!(",aeval=val(ch)*between(t\\,{on:.6}\\,{off:.6}):c=same");
    }
    if declick {
        let d = DECLICK_SECS.min((off - on) / 2.0);
        a += &format!(",afade=t=in:st={on:.6}:d={d:.4},afade=t=out:st={:.6}:d={d:.4}", off - d);
    }
    // Exact output length (pads if the source audio runs out early).
    a += &format!(",apad=whole_dur={0:.6},atrim=end={0:.6}", lay.duration);

    format!("{v}[v];{a}[a]")
}

/// Fade length at audio edges when de-click is on.
pub const DECLICK_SECS: f64 = 0.02;

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
    declick: bool,
    on_progress: F,
) -> Result<(), String>
where
    F: Fn(f64) + Send + 'static,
{
    use tokio::io::AsyncBufReadExt;

    // Fast input seek to a little before the cut, keeping original
    // timestamps (-copyts) so trim/atrim can use file-relative times.
    let (u_start, u_end) = range.union();
    let seek = (u_start - src.video_origin.max(src.audio_origin) - 3.0).max(0.0);
    let read_for = u_end - src.video_origin.min(src.audio_origin) - seek + 1.0;

    let filter = cut_filter(src, &range, declick);
    let enc_path = format!("{out_path}.enc.mp4");
    let tmp_path = format!("{out_path}.part.mp4");
    let rate = rate_string(src.frame_dur);
    eprintln!("[better-clipper] cut {} {range:?} -> {out_path}\n  {filter}", src.path);

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
            "-f",
            "mp4",
            &enc_path,
        ])
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;

    let stderr = child.stderr.take().ok_or("Failed to capture ffmpeg stderr")?;
    let duration = range.layout(src.frame_dur).duration;
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
        let _ = std::fs::remove_file(&enc_path);
        let n = last.len();
        return Err(format!("ffmpeg cut failed: {}", last[n.saturating_sub(3)..].join(" | ")));
    }
    // Re-mux (stream copy). A CRF encode writes the video's average bitrate
    // as 0 in the mp4 `btrt` box (unknown until the end); WebKitGTK's HTTP
    // media player then stalls ~0.14s into playback. The remux fills in the
    // real value and moves the index to the front (faststart).
    let remux = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-v", "error", "-y", "-i", &enc_path, "-map", "0", "-c", "copy",
               "-movflags", "+faststart", "-f", "mp4", &tmp_path])
        .output()
        .await
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;
    let _ = std::fs::remove_file(&enc_path);
    if !remux.status.success() {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("ffmpeg remux failed: {}", tail(&String::from_utf8_lossy(&remux.stderr), 3)));
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

    /// Max volume (dB) of the audio in [from, from+dur) of `path`.
    async fn max_volume(path: &str, from: f64, dur: f64) -> f64 {
        let out = tokio::process::Command::new(FF)
            .args(["-hide_banner", "-ss", &format!("{from:.4}"), "-t", &format!("{dur:.4}"), "-i", path,
                   "-map", "0:a:0", "-af", "volumedetect", "-f", "null", "-"])
            .output().await.unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        err.lines()
            .find_map(|l| l.split("max_volume:").nth(1))
            .and_then(|v| v.trim().trim_end_matches("dB").trim().parse().ok())
            .unwrap_or(-200.0)
    }

    /// Mean volume (dB) of the audio in [from, to) seconds of `path`.
    async fn mean_volume(path: &str, from: f64, to: f64) -> f64 {
        let out = tokio::process::Command::new(FF)
            .args(["-hide_banner", "-i", path, "-map", "0:a:0",
                   "-af", &format!("atrim={from:.4}:{to:.4},volumedetect"), "-f", "null", "-"])
            .output().await.unwrap();
        String::from_utf8_lossy(&out.stderr).lines()
            .find_map(|l| l.split("mean_volume:").nth(1))
            .and_then(|v| v.trim().trim_end_matches("dB").trim().parse().ok())
            .unwrap_or(-200.0)
    }

    /// avgBitrate of the first `btrt` box (the video track's).
    fn video_avg_bitrate(path: &str) -> u32 {
        let d = std::fs::read(path).unwrap();
        let i = d.windows(4).position(|w| w == b"btrt").expect("no btrt box");
        u32::from_be_bytes(d[i + 12..i + 16].try_into().unwrap())
    }

    async fn stream_duration(path: &str, stream: &str) -> f64 {
        let out = tokio::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", stream, "-show_entries", "stream=duration",
                   "-of", "csv=p=0", path])
            .output().await.unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }

    /// L-cut, J-cut and silent (audio-shorter) edges on a real download.
    #[tokio::test]
    #[ignore]
    async fn split_edits() {
        let edit_path = std::env::var("BC_E2E_EDIT").expect("BC_E2E_EDIT");
        let start: f64 = std::env::var("BC_E2E_START").unwrap().parse().unwrap();
        let tmp = std::env::temp_dir();
        let fd = probe_frame_dur(FF, &edit_path).await.unwrap();
        let edit = EditSource {
            path: edit_path.clone(),
            video_origin: ceil_to_grid(start, fd),
            audio_origin: start,
            frame_dur: fd,
            section: None,
        };
        let (k_in, k_out) = (120usize, 180usize);
        let at = |k: usize, frac: f64| (k as f64 + frac) * fd;
        let opts = || EncodeOpts { preset: "ultrafast", crf: "10", audio_bitrate: "128k" };
        let body = k_out - k_in + 1;

        // name, audio In frame, audio Out frame, want head held, tail held, lead silence, trail silence
        let cases = [
            ("lcut", k_in, k_out + 40, 0usize, 40usize, 0usize, 0usize),
            ("jcut", k_in - 30, k_out, 30, 0, 0, 0),
            ("both", k_in - 15, k_out + 25, 15, 25, 0, 0),
            ("silent", k_in + 20, k_out - 20, 0, 0, 20, 20),
        ];
        for (name, a_in, a_out, head, tail, lead, trail) in cases {
            let range = clip_range(FF, &edit, at(k_in, 0.37), at(k_out, 0.61), at(a_in, 0.4), at(a_out, 0.55))
                .await.unwrap();
            let out = tmp.join(format!("bc_e2e_split_{name}.mp4")).to_string_lossy().to_string();
            cut(FF, &edit, range, &out, opts(), false, |_| {}).await.unwrap();
            let want = body + head + tail;
            eprintln!("{name}: {range:?} want {want} frames");

            assert_eq!(frame_count(&out).await, want, "{name}: frame count");
            assert!(video_avg_bitrate(&out) > 0, "{name}: btrt avg bitrate is 0 (WebKit HTTP stall)");
            // First body frame and last body frame are the In/Out frames.
            assert_eq!(best_match(&edit_path, k_in, &gray(&out, head).await).await, 0, "{name}: In frame");
            assert_eq!(best_match(&edit_path, k_out, &gray(&out, head + body - 1).await).await, 0, "{name}: Out frame");
            // Held frames are copies of the edge frames.
            if head > 0 {
                assert!(diff(&gray(&out, 0).await, &gray(&out, head).await) < 1.0, "{name}: head hold");
            }
            if tail > 0 {
                assert!(diff(&gray(&out, want - 1).await, &gray(&out, head + body - 1).await) < 1.0, "{name}: tail hold");
            }
            // Audio covers the whole output (aac priming/padding < 1 frame + 1 aac frame).
            let vd = want as f64 * fd;
            let ad = stream_duration(&out, "a:0").await;
            assert!((ad - vd).abs() < fd + 0.025, "{name}: audio {ad} vs video {vd}");
            // Audio in held regions is real; silent regions are silent.
            if tail > 0 {
                assert!(max_volume(&out, vd - tail as f64 * fd + 0.05, tail as f64 * fd - 0.1).await > -50.0, "{name}: tail audio");
            }
            if lead > 0 {
                assert!(max_volume(&out, 0.0, lead as f64 * fd - 0.05).await < -50.0, "{name}: lead silence");
                assert!(max_volume(&out, lead as f64 * fd + 0.05, 0.5).await > -50.0, "{name}: audio after lead");
            }
            if trail > 0 {
                assert!(max_volume(&out, vd - trail as f64 * fd + 0.05, trail as f64 * fd - 0.1).await < -50.0, "{name}: trail silence");
            }
        }

        // De-click: the first 20ms (the fade) are quieter with it on, and
        // the audio after the fade is unchanged.
        let range = clip_range(FF, &edit, at(k_in, 0.37), at(k_out, 0.61), at(k_in, 0.37), at(k_out, 0.61)).await.unwrap();
        let hard = tmp.join("bc_e2e_hard.mp4").to_string_lossy().to_string();
        let soft = tmp.join("bc_e2e_soft.mp4").to_string_lossy().to_string();
        cut(FF, &edit, range, &hard, opts(), false, |_| {}).await.unwrap();
        cut(FF, &edit, range, &soft, opts(), true, |_| {}).await.unwrap();
        let (h, s) = (mean_volume(&hard, 0.0, 0.02).await, mean_volume(&soft, 0.0, 0.02).await);
        let (h2, s2) = (mean_volume(&hard, 0.03, 0.2).await, mean_volume(&soft, 0.03, 0.2).await);
        eprintln!("declick: first 20ms mean {h} dB hard, {s} dB faded; after: {h2} / {s2}");
        assert!(s < h - 2.0, "fade-in not applied");
        assert!((h2 - s2).abs() < 1.0, "audio after the fade changed");
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
            section: None,
        };
        // Markers dragged between frames (the old bug's trigger).
        let k_in = 88usize;
        let k_out = 126usize;
        let in_sec = k_in as f64 * fd + fd * 0.37;
        let out_sec = k_out as f64 * fd + fd * 0.61;
        let range = clip_range(FF, &edit, in_sec, out_sec, in_sec, out_sec).await.unwrap();
        let want = k_out - k_in + 1;
        eprintln!("fd={fd} range={range:?} want {want} frames");

        // Preview path: cut from the edit file.
        let a = tmp.join("bc_e2e_a.mp4").to_string_lossy().to_string();
        cut(FF, &edit, range, &a, EncodeOpts { preset: "ultrafast", crf: "10", audio_bitrate: "128k" }, false, |_| {}).await.unwrap();
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
            section: None,
        };
        let b = tmp.join("bc_e2e_b.mp4").to_string_lossy().to_string();
        cut(FF, &exp, range, &b, EncodeOpts { preset: "ultrafast", crf: "10", audio_bitrate: "128k" }, false, |_| {}).await.unwrap();
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
    fn linked_layout_is_plain_cut() {
        let r = ClipRange::linked(10.0, 12.0);
        let l = r.layout(0.04);
        assert_eq!((l.head_frames, l.tail_frames, l.frames), (0, 0, 50));
        assert_eq!(l.audio_delay, 0.0);
        assert!((l.duration - 2.0).abs() < 1e-9);
    }

    #[test]
    fn audio_snaps_to_whole_frames_from_video_edges() {
        // L-cut: audio Out 1.013s past video Out -> 25 frames at 25fps (1.0s)
        let r = snap_audio(10.0, 12.0, 10.0, 13.013, 0.04).unwrap();
        assert!((r.audio_end - 13.0).abs() < 1e-9);
        let l = r.layout(0.04);
        assert_eq!((l.head_frames, l.tail_frames, l.frames), (0, 25, 75));
        // J-cut: audio In 0.5s early -> 12.5 frames rounds to 13 (0.52s)
        let r = snap_audio(10.0, 12.0, 9.5, 12.0, 0.04).unwrap();
        assert!((r.audio_start - 9.48).abs() < 1e-9);
        assert_eq!(r.layout(0.04).head_frames, 13);
        // Audio inside video: silence, no held frames
        let r = snap_audio(10.0, 12.0, 10.4, 11.6, 0.04).unwrap();
        let l = r.layout(0.04);
        assert_eq!((l.head_frames, l.tail_frames, l.frames), (0, 0, 50));
        assert!((l.audio_delay - 0.4).abs() < 1e-9);
        // Empty audio range rejected
        assert!(snap_audio(10.0, 12.0, 11.0, 11.0, 0.04).is_err());
    }

    #[test]
    fn filter_pads_and_fades() {
        let src = EditSource { path: "x".into(), video_origin: 100.0, audio_origin: 100.0, frame_dur: 0.04, section: None };
        let linked = cut_filter(&src, &ClipRange::linked(101.0, 103.0), false);
        assert!(!linked.contains("tpad") && !linked.contains("adelay") && !linked.contains("afade"));
        let l = cut_filter(&src, &snap_audio(101.0, 103.0, 100.5, 104.0, 0.04).unwrap(), true);
        assert!(l.contains("fps=25,tpad=start=13:stop=25:start_mode=clone:stop_mode=clone"), "{l}");
        assert!(l.contains("afade=t=in:st=0.000000:d=0.0200"), "{l}");
        assert!(l.contains("apad=whole_dur=3.520000,atrim=end=3.520000"), "{l}");
        let silent = cut_filter(&src, &snap_audio(101.0, 103.0, 101.4, 103.0, 0.04).unwrap(), false);
        assert!(silent.contains("aeval=val(ch)*between(t\\,0.400000\\,2.000000):c=same"), "{silent}");
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
