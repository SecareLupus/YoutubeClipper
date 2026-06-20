# YouTube Transcript Clipper

Desktop app for clipping YouTube videos by searching the transcript. Paste a URL, find the line you want, and download only that section.

Built with [Tauri v2](https://tauri.app/) (Rust) + [React](https://react.dev/) (TypeScript).

It works by:

1. Fetching automatic or creator-provided subtitles via `yt-dlp` (no video download yet).
2. Merging YouTube's word-level caption fragments into clean, searchable segments.
3. Letting you search the transcript in real-time and click a segment to download only ±10 seconds around it.
4. Falling back to local [whisper.cpp](https://github.com/ggerganov/whisper.cpp) transcription when no subtitles are available.

## Quick start

```bash
git clone https://github.com/SecareLupus/YoutubeClipper.git
cd YoutubeClipper
pnpm install
pnpm tauri dev
```

`pnpm tauri dev` compiles the Rust backend, starts the Vite dev server, and opens the desktop window — all in one command.

## Requirements

**Runtime sidecars** (must be on your `PATH` in dev mode):

| Binary | Purpose | Install |
|--------|---------|---------|
| `yt-dlp` | Video download + subtitle extraction | `pip install yt-dlp` or [github.com/yt-dlp/yt-dlp](https://github.com/yt-dlp/yt-dlp) |
| `ffmpeg` | Audio demuxing + re-encoding | `sudo apt install ffmpeg` (Linux), `brew install ffmpeg` (macOS), or [ffmpeg.org](https://ffmpeg.org) |
| `whisper-cli` | Local transcription fallback | Build from [ggerganov/whisper.cpp](https://github.com/ggerganov/whisper.cpp) |

**Build dependencies** (for compiling from source):

- [Rust](https://rustup.rs) (stable toolchain)
- [pnpm](https://pnpm.io) (Node.js package manager)
- Linux: `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libappindicator3-dev`, and related Tauri system libraries

See the [Tauri v2 prerequisites](https://tauri.app/start/prerequisites/) for platform-specific setup.

## Usage

1. Launch with `pnpm tauri dev`
2. Paste a YouTube URL (or any site `yt-dlp` supports) and click **Load**
3. The transcript loads in the right panel — search it, then click any segment
4. A time-bounded preview clip downloads (±10 seconds around your selection)

## Speech-to-text fallback

When `yt-dlp` cannot find subtitles for a video, the app automatically falls back to local transcription via whisper.cpp:

1. Downloads the 720p video
2. Extracts a 16kHz mono WAV with `ffmpeg`
3. Transcribes with `whisper-cli` using the `ggml-tiny.bin` model

The model (~78 MB) is auto-downloaded from HuggingFace on first use and cached in `~/.cache/better-clipper/models/`.

## Architecture

```
┌──────────────────────────────────────┐
│              React UI                │  TypeScript + Vite
│   Search · Transcript · Progress     │
└──────────────┬───────────────────────┘
               │ Tauri IPC (invoke / events)
┌──────────────▼───────────────────────┐
│           Rust Backend               │  Tauri v2 commands
│   Downloader · Transcriber · VTT     │
└──────────────┬───────────────────────┘
               │ tokio::process
┌──────────────▼───────────────────────┐
│  yt-dlp    ffmpeg    whisper-cli     │  Sidecar binaries
└──────────────────────────────────────┘
```

## Project status

Phase 1-2 complete. Builds and runs.

- [x] YouTube captions → transcript fetch (no video download)
- [x] Transcript search with match highlighting
- [x] Segment merging (collapses YouTube's word-level fragments)
- [x] Time-bounded section download via `yt-dlp --download-sections`
- [x] Live progress bars (download, model fetch, transcription)
- [x] Whisper.cpp fallback with auto-downloaded `ggml-tiny.bin`
- [ ] Video player with timeline scrub (Phase 4)
- [ ] Frame-perfect export pipeline (Phase 5)
- [ ] Multi-resolution support (1080p, 4K)

## License

MIT
