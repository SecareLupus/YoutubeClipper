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

On Windows PowerShell:

```powershell
py -3 -m venv .venv
.venv\Scripts\Activate.ps1
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
- `--auto-transcribe` falls back to a speech-to-text workflow when no subtitles are available.
- `--stt-provider` selects the speech-to-text provider plugin to use alongside `--auto-transcribe`.

Example:

```bash
python clipper.py "https://www.youtube.com/watch?v=dQw4w9WgXcQ" "never gonna give you up" --before 3 --after 7 --output rickroll.mp4
```

The script uses `yt-dlp`'s `--download-sections "*start-end"` support to grab just the requested time span when possible, and saves an `.srt` subtitle file alongside the clip so you can burn captions later if you prefer. If the line occurs multiple times, it picks the subtitle segment with the highest fuzzy match score, which usually corresponds to the closest textual match. When `yt-dlp` cannot download just the requested range, it falls back to fetching the full video and trims it locally with `ffmpeg`.

If no good match is found the script exits with a nonzero status. Try adjusting your query (shorter phrases often match better) or confirm that the video has subtitles in the selected language.

## Speech-to-text fallback

**==EXPERIMENTAL, HERE BE DRAGONS!==**

When `yt-dlp` cannot fetch subtitles, pass `--auto-transcribe` to download the audio track, send it through a speech-to-text (STT) provider, and continue matching locally. Providers are pluggable: define a subclass of `STTProvider` in `stt_providers.py`, decorate it with `@register_provider`, and select it at runtime via `--stt-provider <name>`. The repository currently ships with a `stub` provider that simply documents the interface; replace it with an implementation that uploads audio to your service of choice (for example, OpenAI Whisper API or Google Cloud Speech-to-Text) and returns timestamped segments.
