#!/usr/bin/env python3
"""Clip portions of YouTube videos by searching their transcripts."""

from __future__ import annotations

import argparse
import dataclasses
import difflib
import json
import math
import re
import sys
import subprocess
from pathlib import Path
from typing import Iterable, List, Optional, Sequence

try:
    from yt_dlp import YoutubeDL
except ModuleNotFoundError as exc:  # pragma: no cover - dependency guard
    raise SystemExit(
        "This tool requires the 'yt-dlp' Python package. "
        "Install it with 'pip install yt-dlp'."
    ) from exc

@dataclasses.dataclass
class Segment:
    text: str
    start: float  # seconds
    end: float  # seconds


@dataclasses.dataclass
class Match:
    text: str
    start: float
    end: float
    score: float
    start_segment_index: int
    segment_count: int


@dataclasses.dataclass
class DownloadResult:
    path: Path
    partial: bool


def normalize(text: str) -> str:
    cleaned = re.sub(r"\s+", " ", text).strip().lower()
    cleaned = re.sub(r"[^\w\s]", "", cleaned, flags=re.UNICODE)
    return cleaned


def format_timestamp(seconds: float) -> str:
    seconds = max(0.0, seconds)
    hours = int(seconds // 3600)
    minutes = int((seconds % 3600) // 60)
    secs = seconds - (hours * 3600 + minutes * 60)
    return f"{hours:02d}:{minutes:02d}:{secs:06.3f}"


def format_srt_timestamp(seconds: float) -> str:
    seconds = max(0.0, seconds)
    hours = int(seconds // 3600)
    minutes = int((seconds % 3600) // 60)
    secs = int(seconds % 60)
    millis = int(round((seconds - int(seconds)) * 1000))
    return f"{hours:02d}:{minutes:02d}:{secs:02d},{millis:03d}"


def status(message: str) -> None:
    print(f"[clipper] {message}", flush=True)


def read_segments_from_json3(data: dict) -> List[Segment]:
    segments: List[Segment] = []
    events: Iterable[dict] = data.get("events") or []
    for event in events:
        if event.get("tStartMs") is None:
            continue
        start_ms = event["tStartMs"]
        end_ms = event.get("tStartMs") + event.get("dDurationMs", 0)
        if not end_ms and event.get("tEndMs") is not None:
            end_ms = event["tEndMs"]
        if not end_ms:
            # Fallback to one second duration if nothing else is provided.
            end_ms = start_ms + 1000
        text_fragments = []
        for seg in event.get("segs") or []:
            piece = seg.get("utf8")
            if piece:
                text_fragments.append(piece)
        text = re.sub(r"\s+", " ", "".join(text_fragments)).strip()
        if not text:
            continue
        segments.append(Segment(text=text, start=start_ms / 1000.0, end=end_ms / 1000.0))

    # Ensure strictly increasing timelines for downstream calculations.
    for idx in range(1, len(segments)):
        prev = segments[idx - 1]
        current = segments[idx]
        if current.start < prev.end:
            current.start = prev.end
        if current.end <= current.start:
            current.end = current.start + 0.5
    return segments


def find_best_match(segments: Sequence[Segment], query: str, max_window: int = 4) -> Optional[Match]:
    if not segments:
        return None
    target = normalize(query)
    if not target:
        return None

    best: Optional[Match] = None
    for window in range(1, max_window + 1):
        for index in range(0, len(segments) - window + 1):
            window_segments = segments[index : index + window]
            combined = " ".join(seg.text for seg in window_segments)
            normalized = normalize(combined)
            if not normalized:
                continue
            ratio = difflib.SequenceMatcher(None, target, normalized).ratio()
            if target in normalized:
                ratio += 0.5
            if best is None or ratio > best.score:
                best = Match(
                    text=combined,
                    start=window_segments[0].start,
                    end=window_segments[-1].end,
                    score=ratio,
                    start_segment_index=index,
                    segment_count=window,
                )
    return best


def fetch_transcript_segments(url: str, lang: str) -> List[Segment]:
    tmpdir = Path.cwd() / ".clipper_tmp"
    tmpdir.mkdir(exist_ok=True)

    outtmpl = str(tmpdir / "%(id)s.%(ext)s")
    ydl_opts = {
        "skip_download": True,
        "writesubtitles": True,
        "writeautomaticsub": True,
        "subtitlesformat": "json3",
        "subtitleslangs": [lang, f"{lang}.orig", f"{lang}-orig"],
        "outtmpl": outtmpl,
        "quiet": True,
        "no_warnings": True,
    }
    with YoutubeDL(ydl_opts) as ydl:
        ydl.download([url])

    json_files = sorted(tmpdir.glob("*.json3"), key=lambda path: path.stat().st_mtime, reverse=True)
    if not json_files:
        raise RuntimeError("No subtitles were downloaded. Try a different language code or video.")

    latest = json_files[0]
    try:
        data = json.loads(latest.read_text(encoding="utf-8"))
    finally:
        # Clean up temporary subtitle files to avoid clutter.
        for file_path in json_files:
            try:
                file_path.unlink()
            except OSError:
                pass

    return read_segments_from_json3(data)


def download_clip(
    url: str,
    start: float,
    end: float,
    output_path: Path,
    video_format: Optional[str],
    verbose: bool = False,
) -> DownloadResult:
    if end <= start:
        raise ValueError("Clip end must be after clip start.")

    output_path.parent.mkdir(parents=True, exist_ok=True)
    if output_path.exists():
        try:
            output_path.unlink()
        except OSError:
            pass

    target_ext = output_path.suffix.lstrip(".") or "mp4"
    base_template = str(output_path.with_suffix(""))
    outtmpl = f"{base_template}.%(ext)s"

    base_opts: dict = {
        "force_keyframes_at_cuts": True,
        "merge_output_format": target_ext,
        "outtmpl": outtmpl,
        "quiet": not verbose,
        "no_warnings": not verbose,
    }
    if video_format:
        base_opts["format"] = video_format

    requested_duration = max(0.0, end - start)
    duration_tolerance = max(1.0, requested_duration * 0.1)

    def finalize_partial() -> Path:
        produced_path = Path(f"{base_template}.{target_ext}")
        if produced_path != output_path:
            try:
                produced_path.rename(output_path)
            except OSError as exc:
                raise RuntimeError(f"Failed to rename output to {output_path}: {exc}") from exc
        return output_path

    def attempt(extra_opts: dict, outtmpl_override: Optional[str] = None) -> None:
        opts = dict(base_opts)
        if outtmpl_override is not None:
            opts["outtmpl"] = outtmpl_override
        opts.update(extra_opts)
        with YoutubeDL(opts) as ydl:
            ydl.download([url])

    def assess_download(path: Path, label: str) -> DownloadResult:
        duration = probe_duration(path)
        if duration is None:
            status(f"{label} completed but duration is unknown; will trim clip locally.")
            return DownloadResult(path=path, partial=False)
        if requested_duration == 0.0:
            return DownloadResult(path=path, partial=True)
        if abs(duration - requested_duration) <= duration_tolerance:
            status(
                f"{label} produced ≈{duration:.2f}s clip (target {requested_duration:.2f}s)."
            )
            return DownloadResult(path=path, partial=True)
        status(
            f"{label} produced {duration:.2f}s file (target {requested_duration:.2f}s); will trim locally."
        )
        return DownloadResult(path=path, partial=False)

    # Attempt precise range download via yt-dlp callback API.
    try:
        attempt(
            {
                "download_ranges": lambda info_dict, _ydl: [
                    {"start_time": float(start), "end_time": float(end)}
                ]
            }
        )
    except Exception as exc:
        status(
            f"yt-dlp download_ranges failed ({exc}); trying download_sections fallback…"
        )
    else:
        return assess_download(finalize_partial(), "download_ranges")

    # Try older download_sections syntax as a fallback.
    range_expr = f"*{format_timestamp(start)}-{format_timestamp(end)}"
    try:
        attempt({"download_sections": [range_expr]})
    except Exception as exc:
        status(
            f"download_sections failed ({exc}); falling back to full video download."
        )
    else:
        return assess_download(finalize_partial(), "download_sections")

    full_base_template = f"{base_template}_full"
    full_outtmpl = f"{full_base_template}.%(ext)s"
    status("Downloading full video (this may take longer)…")
    attempt({}, outtmpl_override=full_outtmpl)
    full_path = Path(f"{full_base_template}.{target_ext}")
    return DownloadResult(path=full_path, partial=False)


def write_subtitle_file(
    segments: Sequence[Segment],
    clip_start: float,
    clip_end: float,
    destination: Path,
) -> bool:
    relevant: List[Segment] = []
    for segment in segments:
        if segment.end <= clip_start or segment.start >= clip_end:
            continue
        relevant.append(segment)
    if not relevant:
        return False

    lines: List[str] = []
    counter = 1
    for segment in relevant:
        seg_start = max(segment.start, clip_start) - clip_start
        seg_end = min(segment.end, clip_end) - clip_start
        if seg_end <= seg_start:
            continue
        text = re.sub(r"\s+", " ", segment.text).strip()
        if not text:
            continue
        lines.append(str(counter))
        lines.append(f"{format_srt_timestamp(seg_start)} --> {format_srt_timestamp(seg_end)}")
        lines.append(text)
        lines.append("")
        counter += 1

    if counter == 1:
        return False

    destination.write_text("\n".join(lines), encoding="utf-8")
    return True


def trim_with_ffmpeg(
    source: Path,
    start: float,
    end: float,
    output: Path,
    verbose: bool = False,
) -> Path:
    duration = end - start
    if duration <= 0:
        raise ValueError("Clip end must be after clip start.")

    temp_output = output.with_suffix(output.suffix + ".tmp")
    if temp_output.exists():
        temp_output.unlink()

    ffmpeg_cmd = [
        "ffmpeg",
        "-y",
        "-ss",
        f"{start:.3f}",
        "-i",
        str(source),
        "-t",
        f"{duration:.3f}",
        "-c",
        "copy",
        str(temp_output),
    ]
    stdout = None if verbose else subprocess.DEVNULL
    stderr = None if verbose else subprocess.DEVNULL
    try:
        subprocess.run(ffmpeg_cmd, check=True, stdout=stdout, stderr=stderr)
    except FileNotFoundError as exc:
        raise RuntimeError("ffmpeg is required to trim the clip but was not found on PATH.") from exc
    except subprocess.CalledProcessError as exc:
        raise RuntimeError(f"ffmpeg failed to trim the clip: exit code {exc.returncode}") from exc

    try:
        if output.exists():
            output.unlink()
        temp_output.rename(output)
    except OSError as exc:
        raise RuntimeError(f"Failed to finalize trimmed clip: {exc}") from exc

    return output


def probe_duration(path: Path) -> Optional[float]:
    command = [
        "ffprobe",
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        str(path),
    ]
    try:
        result = subprocess.run(
            command,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None
    output = (result.stdout or "").strip().splitlines()
    if not output:
        return None
    try:
        return float(output[0])
    except ValueError:
        return None


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Search a YouTube transcript for a line and clip the matching portion of the video."
        )
    )
    parser.add_argument("url", help="YouTube video URL or ID")
    parser.add_argument("query", help="Line or phrase to match within the transcript")
    parser.add_argument(
        "--before",
        type=float,
        default=5.0,
        help="Seconds to include before the matched transcript (default: 5)",
    )
    parser.add_argument(
        "--after",
        type=float,
        default=5.0,
        help="Seconds to include after the matched transcript (default: 5)",
    )
    parser.add_argument(
        "--lang",
        default="en",
        help="Subtitle language code to search (default: en)",
    )
    parser.add_argument(
        "--max-window",
        type=int,
        default=4,
        help="Maximum number of subtitle segments to join when searching (default: 4)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Destination file for the clip (default: generated from the query)",
    )
    parser.add_argument(
        "--format",
        dest="video_format",
        default=None,
        help="yt-dlp format selector for the clip (e.g. 'bestvideo+bestaudio/best')",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="Show yt-dlp output while downloading",
    )
    return parser.parse_args(argv)


def sanitize_for_filename(text: str) -> str:
    cleaned = re.sub(r"[^\w\s-]", "", text).strip().lower()
    cleaned = re.sub(r"[\s]+", "_", cleaned)
    return cleaned or "clip"


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    try:
        status("Fetching transcript…")
        segments = fetch_transcript_segments(args.url, args.lang)
    except Exception as exc:
        print(f"Failed to download transcript: {exc}", file=sys.stderr)
        return 1

    status(f"Loaded {len(segments)} transcript segments. Searching for best match…")
    match = find_best_match(segments, args.query, max_window=args.max_window)
    if not match:
        print("Could not find a matching subtitle segment for the provided query.", file=sys.stderr)
        return 2

    clip_start = max(0.0, match.start - args.before)
    clip_end = match.end + args.after

    if args.output is None:
        snippet = sanitize_for_filename(match.text)[:60]
        default_name = f"{sanitize_for_filename(args.query)[:40]}_{int(math.floor(clip_start))}"
        args.output = Path(f"{default_name or snippet}.mp4")

    subtitle_output: Optional[Path] = None
    try:
        status("Downloading clipped video segment with yt-dlp…")
        download_result = download_clip(
            url=args.url,
            start=clip_start,
            end=clip_end,
            output_path=args.output,
            video_format=args.video_format,
            verbose=args.verbose,
        )
        if download_result.partial:
            final_path = download_result.path
        else:
            status("Trimming downloaded file with ffmpeg…")
            final_path = trim_with_ffmpeg(
                source=download_result.path,
                start=clip_start,
                end=clip_end,
                output=args.output,
                verbose=args.verbose,
            )
            try:
                if (
                    download_result.path != final_path
                    and download_result.path.exists()
                ):
                    download_result.path.unlink()
            except OSError:
                pass
        status("Writing subtitle file…")
        subtitle_path = final_path.with_suffix(".srt")
        if write_subtitle_file(segments, clip_start, clip_end, subtitle_path):
            status(f"Subtitle file saved to {subtitle_path.resolve()}")
            subtitle_output = subtitle_path
        else:
            status("No matching subtitle lines to write for this clip.")
    except Exception as exc:
        print(f"Failed to download clip: {exc}", file=sys.stderr)
        return 3

    status("Done.")
    print("Matched transcript snippet:")
    print(f"  {match.text}")
    print(f"Match score: {match.score:.3f}")
    print(f"Transcript time span: {format_timestamp(match.start)} – {format_timestamp(match.end)}")
    print(f"Clip time span      : {format_timestamp(clip_start)} – {format_timestamp(clip_end)}")
    print(f"Clip saved to       : {final_path.resolve()}")
    if subtitle_output is not None:
        print(f"Subtitles saved to   : {subtitle_output.resolve()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
