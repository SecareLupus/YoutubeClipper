This specification document is tailored for an **AI Agent (like Roo Code, Claude, or a custom team of coding agents)** to ingest and build out the system.

It uses **Tauri (v2)** for the cross-platform shell because it handles multi-threading beautifully, maintains a minimal memory footprint, and allows us to ship native sidecar binaries (`yt-dlp`, `ffmpeg`, `whisper.cpp`) directly for Windows, macOS, and Linux without forcing the end user to install them manually.

---

# Product Specification: "Schnitt" (Video Snippet Clipper)

**Target Execution Environment:** Cross-Platform Desktop (Windows x64, macOS Apple Silicon/Intel, Linux x64)

**Development Philosophy:** Local-first, native performance, agentic-friendly modular design.

## 1. System Architecture & Dependencies

To ensure painless cross-platform compilation and zero external system prerequisites, the application architecture relies on a **Tauri (Rust) Core** wrapping a **React/TypeScript UI**, communicating natively with bundled sidecar utilities.

### Sidecar Binaries Matrix

The build process must compile or bundle the platform-specific binaries into the `src-tauri/binaries/` directory matching Tauri's naming convention (`[binary]-[target-triple]`).

* **`yt-dlp`:** Handles link extraction and multi-resolution fetching.
* **`ffmpeg` / `ffprobe`:** Handles rapid audio demuxing and frame-perfect video re-encoding.
* **`whisper-cli` (`whisper.cpp`):** Handles CPU-bound local transcription via GGML models. Compiled with cross-platform baseline CPU extensions (AVX2 for x86, Neon for ARM).

---

## 2. Core Operational Workflows

### Data Extraction Pipeline

```
[User Paste URL]
   │
   ├──► Task 1: Spawns `yt-dlp` -> Fetches 720p Preview MP4
   │       │
   │       └──► Spawns `ffmpeg` -> Extracts 16kHz Mono WAV 
   │               │
   │               └──► Spawns `whisper-cli` -> Generates `transcript.vtt`
   │
   └──► Task 2 (Parallel): Spawns `yt-dlp` -> Fetches Target Master Res MP4 (Cached in background)

```

### Frame-Perfect Export Logic

When the user triggers a clip export, the frontend passes millisecond time boundaries to the Rust backend. The backend executes an explicit re-encoding pass against the high-resolution master asset:

```bash
ffmpeg -ss [In_Timestamp_ms] -to [Out_Timestamp_ms] -i [path_to_master_video] -c:v libx264 -preset ultrafast -crf 18 -c:a aac -b:a 192k [output_destination.mp4]

```

---

## 3. Frontend Architecture & State Management

### Unified State Machine Schema

```typescript
interface AppState {
  currentVideo: {
    id: string;
    previewPath: string;
    masterPath: string;
    resolution: '720p' | '1080p' | '4k';
    status: 'idle' | 'downloading' | 'transcribing' | 'ready' | 'error';
    progress: number; // Combined extraction percentage
  };
  search: {
    query: string;
    results: SearchResultInstance[];
    selectedInstanceIdx: number | null;
  };
  timeline: {
    currentTime: number; // in milliseconds
    inMarker: number;    // in milliseconds
    outMarker: number;   // in milliseconds
    zoomWindow: {
      min: number;       // Expandable lower bound view boundary
      max: number;       // Expandable upper bound view boundary
    };
  };
}

interface SearchResultInstance {
  startTime: number;     // Milliseconds from start
  endTime: number;       // Milliseconds from start
  text: string;          // Matching phrase slice
}

```

### The Interactive Timeline Component Layout

```
┌────────────────────────────────────────────────────────┐
│                   [ Video Player ]                     │
└────────────────────────────────────────────────────────┘
┌────────────────────────────────────────────────────────┐
│ Search Transcript: [ solar flare             ] (3 hits)│
│  ► 02:14 "...caused a major solar flare that disrupted"│
│  ► 14:05 "...monitoring the solar flare activity..."  │
└────────────────────────────────────────────────────────┘
◀── [Extensible Lower Bound]                             [Extensible Upper Bound] ──▶
┌────────────────────────────────────────────────────────┐
│ Timeline Zoom (Anchor: 02:14)                          │
│ ───[======█===========================█===========]─── │
│          In-Marker                  Out-Marker         │
└────────────────────────────────────────────────────────┘

```

---

## 4. Phase-1 Step-by-Step Implementation Backlog

*For the AI Agent: Execute these blocks sequentially. Validate file outputs at each stage.*

### Phase 1: Environment Setup & Cross-Platform Scaffolding

* Initialize a Tauri v2 project with a React + TS frontend (`pnpm`).
* Create a clean directory layout for system assets: `src-tauri/binaries/` and `src-tauri/models/`.
* Configure the Tauri permissions manifest (`capabilities/main.json`) to allow child-process execution for the sidecars and write access to the user's temporary/cache directories.

### Phase 2: Native Sidecar Integration Bridge

* Implement the Rust backend module `src-tauri/src/downloader.rs` utilizing `tauri::process::Command` to invoke `yt-dlp`.
* Implement parsing logic to grab the `-f` flag programmatically based on user resolution configurations.
* Implement the `src-tauri/src/transcriber.rs` module. It must verify the existence of the `ggml-tiny.bin` or `ggml-base.bin` model in the app cache folder. If missing, fetch it from Hugging Face's mirror via native HTTP request and provide a progress bar hook.
* Write the execution hook to point `whisper-cli` to a 16kHz audio stream extracted via `ffmpeg`.

### Phase 3: Transcript Indexing & Search Engine

* Build a Web Worker utility in the frontend that takes the `.vtt` / `.srt` outputs from the whisper background run and maps them into a structural token object array.
* Integrate a text lookup client (`Fuse.js` or standard substring array matching) capable of extracting matching context strings with their respective microsecond timestamps.

### Phase 4: Syncing Player State & Expanded Timeline UI

* Create the primary application window splitting Search Results on the left and Media controls on the right.
* Write the timeline engine. When an instance is clicked, update `zoomWindow.min = Math.max(0, matchStart - 5000)` and `zoomWindow.max = Math.min(duration, matchEnd + 5000)`.
* Bind the In/Out scrub handles to track positions inside the absolute master timeline using a clean millisecond tracker to prevent frame-rate shifting bugs entirely.

### Phase 5: Fast Re-Encoding Export Pipeline

* Expose a backend Tauri command `export_slice(master_path, out_path, start_ms, end_ms)`.
* Build the process watcher that monitors `ffmpeg` progress indicators out of `stderr` and maps it back to an export loading state on the screen.

---

## 5. Agent Instructions for Verification

1. **Platform Abstraction:** Always ensure path constructions use `std::path::PathBuf` in Rust to maintain native code compliance across `\` (Windows) and `/` (Unix) environments.
2. **No Frame-Locking:** Under no circumstances should the cut ranges map directly to frame indexes. The UI must strictly pipe string timestamps to `ffmpeg` formatted exactly as `HH:MM:SS.mmm`.