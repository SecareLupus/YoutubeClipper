# YouTube Transcript Clipper

`clipper.py` is a small helper that lets you point `yt-dlp` at a YouTube video, search the subtitles for a specific line, and download a short clip around that subtitle.

It works by:

1. Fetching automatic or creator-provided subtitles via `yt-dlp` (JSON format).
2. Fuzzy matching your query against the transcript to find the best subtitle segment.
3. Asking `yt-dlp` to download only the portion of the video around that timestamp.

## Requirements

- Python 3.9+
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) installed as a Python package (`pip install yt-dlp`)
- `ffmpeg` available on your `PATH` (required by `yt-dlp` for cutting clips)

Create a local environment and install dependencies:

```bash
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

## Usage

```bash
python clipper.py "<youtube-url>" "<query text>" --before 5 --after 10 --lang en --output myclip.mp4
```

- `--before` / `--after` control how many seconds to keep before/after the matching subtitle (defaults: 5s each).
- `--lang` selects the subtitle language to search (default: `en`). The tool will try both manual and automatic subtitles.
- `--output` sets the destination file. If omitted, a name is generated from the query.
- `--format` lets you forward a custom `yt-dlp` format selector (for example: `bestvideo+bestaudio/best`).
- `--verbose` surfaces the underlying `yt-dlp` and `ffmpeg` logs.

Example:

```bash
python clipper.py "https://www.youtube.com/watch?v=dQw4w9WgXcQ" "never gonna give you up" --before 3 --after 7 --output rickroll.mp4
```

The script uses `yt-dlp`'s `--download-sections "*start-end"` support to grab just the requested time span when possible, and saves an `.srt` subtitle file alongside the clip so you can burn captions later if you prefer. If the line occurs multiple times, it picks the subtitle segment with the highest fuzzy match score, which usually corresponds to the closest textual match. When `yt-dlp` cannot download just the requested range, it falls back to fetching the full video and trims it locally with `ffmpeg`.

If no good match is found the script exits with a nonzero status. Try adjusting your query (shorter phrases often match better) or confirm that the video has subtitles in the selected language.
