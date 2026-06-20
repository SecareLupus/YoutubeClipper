# BetterClipper

Desktop video clipping tool. Paste a YouTube URL, search the transcript, scrub the timeline, and export a frame-perfect clip — at your chosen resolution.

Built with [Tauri v2](https://tauri.app/) (Rust) + [React](https://react.dev/) (TypeScript).

It works by:

1. Fetching automatic or creator-provided subtitles via `yt-dlp` (no video download yet).
2. Merging YouTube's word-level caption fragments into clean, searchable segments.
3. Searching the transcript in real-time or clicking segments to download a ±10s preview.
4. Scrubbing the preview with In/Out markers on a zoomable timeline.
5. Exporting a frame-perfect clip re-encoded via `ffmpeg` at your chosen resolution.
6. Falling back to local [whisper.cpp](https://github.com/ggerganov/whisper.cpp) transcription when no subtitles are available.

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
| `ffmpeg` | Audio demuxing + frame-perfect re-encoding | `sudo apt install ffmpeg` (Linux), `brew install ffmpeg` (macOS), or [ffmpeg.org](https://ffmpeg.org) |
| `whisper-cli` | Local transcription fallback | Build from [ggerganov/whisper.cpp](https://github.com/ggerganov/whisper.cpp) |

**Build dependencies** (for compiling from source):

- [Rust](https://rustup.rs) (stable toolchain)
- [pnpm](https://pnpm.io) (Node.js package manager)
- Linux: `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libappindicator3-dev`, and related Tauri system libraries

See the [Tauri v2 prerequisites](https://tauri.app/start/prerequisites/) for platform-specific setup.

## Usage

1. Launch with `pnpm tauri dev`
2. Paste a YouTube URL (or any site `yt-dlp` supports) and click **Load**
3. The transcript loads in the right panel — search it or scroll through segments
4. Click a segment (or search match) to download a ±10 second preview
5. The video player loads with In/Out markers set to your selection
6. Drag the markers or use keyboard shortcuts to fine-tune the cut
7. Select a quality preset and click **Export Clip** to save the final `.mp4`

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| `Space` | Play / pause |
| `,` / `.` | Frame back / forward (1/30s) |
| `←` / `→` | Skip ±5 seconds |
| `Ctrl`+`←` / `→` | Jump to nearest keypoint (In, Out, start, end) |
| `I` | Set In marker at playhead |
| `O` | Set Out marker at playhead |

### Timeline

- **Zoom bar** — shows the selected clip region with draggable In/Out markers and a moving playhead
- **Zoom handles** — drag the left/right edges of the bar to expand or narrow the view
- **Overview bar** — shows the zoom window position relative to the full clip duration
- **Time readout** — In, Out, and Clip duration displayed above the bar

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
│  Video · Timeline · Search · Export  │
└──────────────┬───────────────────────┘
               │ Tauri IPC (invoke / events)
┌──────────────▼───────────────────────┐
│           Rust Backend               │  Tauri v2 commands
│  Downloader · Transcriber · VTT ·    │
│  Export · HTTP server (video serve)  │
└──────────────┬───────────────────────┘
               │ tokio::process + std::net
┌──────────────▼───────────────────────┐
│  yt-dlp    ffmpeg    whisper-cli     │  Sidecar binaries
└──────────────────────────────────────┘
```

## Project status

Builds and runs. Core workflow complete:

- [x] YouTube captions → transcript fetch (no video download until needed)
- [x] Transcript search with debounced query, match highlighting, and interpolated timestamps
- [x] Segment merging (collapses YouTube's word-level fragments)
- [x] Time-bounded section download via `yt-dlp --download-sections`
- [x] Video player with play/pause, frame-stepping, and skip controls
- [x] Zoomable timeline with draggable In/Out markers and playhead tracking
- [x] Preview clip playback (plays from In to Out, stops at Out)
- [x] Frame-perfect export via `ffmpeg` (`libx264`, ultrafast preset, CRF 18, AAC 192k)
- [x] Multi-resolution export (Best, 1080p, 720p, 480p)
- [x] Save dialog with suggested filename (video title + timestamp range)
- [x] Live progress bars (download, model fetch, transcription, export encoding)
- [x] Whisper.cpp fallback with auto-downloaded `ggml-tiny.bin`
- [x] Local HTTP server with Range support for `<video>` element playback
- [x] Video title detection for export filename suggestions

## License

MIT