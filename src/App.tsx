import { useState, useEffect, useMemo, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";

interface PipelineProgress {
  video_id: string;
  stage: string;
  message: string;
  percent: number;
}

interface ExportProgress {
  percent: number;
  status: string;
}

interface TranscriptSegment {
  start_ms: number;
  end_ms: number;
  text: string;
}

interface Timepoint {
  char_idx: number;
  timestamp_ms: number;
}

interface TranscriptData {
  text: string;
  segments: TranscriptSegment[];
  timepoints: Timepoint[];
}

interface SearchMatch {
  start_idx: number;
  end_idx: number;
  start_ms: number;
  end_ms: number;
  text: string;
}

interface ProcessResult {
  video_id: string;
  url: string;
  title: string;
  duration: number;
  transcript_path: string | null;
  transcript_source: "youtube" | "whisper" | null;
  /** Why there is no transcript (null when transcript_path is set). */
  caption_note: string | null;
}

/** An editing copy (transcript section or full video) and how its local
 *  timeline maps to absolute video time. Mirrors media::EditSource. */
interface EditSource {
  path: string;
  video_origin: number;
  audio_origin: number;
  frame_dur: number;
  /** Requested [start, end] seconds of a section download; null for the full video. */
  section: [number, number] | null;
}

interface PreviewResult {
  url: string;
  start: number;
  end: number;
  frames: number;
}

/** Binary-search timepoints for the tightest enclosing timestamps of a char range.
 *  Returns interpolated ms — see plan doc for algorithm. */
function findEnclosingTimepoints(
  timepoints: Timepoint[],
  startIdx: number,
  endIdx: number,
): { start_ms: number; end_ms: number } {
  const n = timepoints.length;
  if (n === 0) return { start_ms: 0, end_ms: 0 };

  // Largest char_idx <= startIdx
  let lo = 0, hi = n - 1;
  while (lo < hi) {
    const mid = Math.ceil((lo + hi) / 2);
    if (timepoints[mid].char_idx <= startIdx) lo = mid;
    else hi = mid - 1;
  }
  const before = timepoints[lo];

  // Smallest char_idx >= startIdx (for interpolation anchor)
  lo = 0; hi = n - 1;
  while (lo < hi) {
    const mid = Math.floor((lo + hi) / 2);
    if (timepoints[mid].char_idx >= startIdx) hi = mid;
    else lo = mid + 1;
  }
  const after = timepoints[lo];

  // Interpolate start_ms between before and after
  let start_ms: number;
  if (before.char_idx === after.char_idx) {
    start_ms = before.timestamp_ms;
  } else {
    const t = (startIdx - before.char_idx) / (after.char_idx - before.char_idx);
    start_ms = before.timestamp_ms + (after.timestamp_ms - before.timestamp_ms) * t;
  }

  // Largest char_idx <= endIdx
  lo = 0; hi = n - 1;
  while (lo < hi) {
    const mid = Math.ceil((lo + hi) / 2);
    if (timepoints[mid].char_idx <= endIdx) lo = mid;
    else hi = mid - 1;
  }
  const endBefore = timepoints[lo];

  // Smallest char_idx >= endIdx
  lo = 0; hi = n - 1;
  while (lo < hi) {
    const mid = Math.floor((lo + hi) / 2);
    if (timepoints[mid].char_idx >= endIdx) hi = mid;
    else lo = mid + 1;
  }
  const endAfter = timepoints[lo];

  let end_ms: number;
  if (endBefore.char_idx === endAfter.char_idx) {
    end_ms = endBefore.timestamp_ms;
  } else {
    const t = (endIdx - endBefore.char_idx) / (endAfter.char_idx - endBefore.char_idx);
    end_ms = endBefore.timestamp_ms + (endAfter.timestamp_ms - endBefore.timestamp_ms) * t;
  }

  return { start_ms: Math.round(start_ms), end_ms: Math.round(end_ms) };
}

function formatTime(ms: number): string {
  const totalSecs = Math.floor(ms / 1000);
  const mins = Math.floor(totalSecs / 60);
  const secs = totalSecs % 60;
  return `${mins}:${secs.toString().padStart(2, "0")}`;
}

function formatTimeSec(s: number): string {
  const mins = Math.floor(s / 60);
  const secs = Math.floor(s % 60);
  return `${mins}:${secs.toString().padStart(2, "0")}`;
}

function highlightMatch(text: string, query: string): React.ReactNode {
  if (!query.trim()) return text;
  const escaped = query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const parts = text.split(new RegExp(`(${escaped})`, "gi"));
  return parts.map((part, i) =>
    part.toLowerCase() === query.toLowerCase() ? (
      <mark key={i}>{part}</mark>
    ) : (
      part
    ),
  );
}

function App() {
  const [url, setUrl] = useState("");
  const [statusMsg, setStatusMsg] = useState("");
  const [processing, setProcessing] = useState(false);
  const [progress, setProgress] = useState(0);
  const [stage, setStage] = useState("");
  const [result, setResult] = useState<ProcessResult | null>(null);
  const [segments, setSegments] = useState<TranscriptSegment[]>([]);
  const [transcriptData, setTranscriptData] = useState<TranscriptData | null>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [debouncedQuery, setDebouncedQuery] = useState("");
  const [selectedIdx, setSelectedIdx] = useState<number | null>(null);
  const [edit, setEdit] = useState<EditSource | null>(null);
  const [videoSrc, setVideoSrc] = useState<string | null>(null);
  const [downloadingSection, setDownloadingSection] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [exportProgress, setExportProgress] = useState(0);
  const [exportStatus, setExportStatus] = useState("encoding");
  const [previewProgress, setPreviewProgress] = useState(0);
  // 720p matches the editing copy, so export cuts locally with no re-download.
  const [quality, setQuality] = useState("720p");
  const [previewVideoSrc, setPreviewVideoSrc] = useState<string | null>(null);
  /** Full-video mode: set Out to the end once the duration is known. */
  const pendingFullOutRef = useRef(false);
  const sectionPath = edit?.path ?? null;
  const [previewOpen, setPreviewOpen] = useState(false);
  const [generatingPreview, setGeneratingPreview] = useState(false);
  const previewVideoRef = useRef<HTMLVideoElement>(null);
  const previewOpenRef = useRef(false);
  previewOpenRef.current = previewOpen;

  // ── video / timeline state ──────────────────────────────────────────────
  const videoRef = useRef<HTMLVideoElement>(null);
  const timelineRef = useRef<HTMLDivElement>(null);
  const draggingRef = useRef<string | null>(null);

  const [duration, setDuration] = useState(0); // seconds
  const [currentTime, setCurrentTime] = useState(0); // seconds
  const [inMarker, setInMarker] = useState(0); // seconds (video-local)
  const [outMarker, setOutMarker] = useState(0); // seconds (video-local)
  // Split edits: audio In/Out follow the video markers while linked.
  const [audioInRaw, setAudioInRaw] = useState(0);
  const [audioOutRaw, setAudioOutRaw] = useState(0);
  const [linkIn, setLinkIn] = useState(true);
  const [linkOut, setLinkOut] = useState(true);
  const audioIn = linkIn ? inMarker : audioInRaw;
  const audioOut = linkOut ? outMarker : audioOutRaw;
  const [declick, setDeclick] = useState(
    () => localStorage.getItem("declick") !== "0",
  );
  useEffect(() => localStorage.setItem("declick", declick ? "1" : "0"), [declick]);
  const audioDragRef = useRef<"ain" | "aout" | null>(null);
  const pendingSeekRef = useRef<number | null>(null);
  const [zoomMin, setZoomMin] = useState(0); // seconds
  const [zoomMax, setZoomMax] = useState(30); // seconds
  const [timelineWidth, setTimelineWidth] = useState(0); // px — measured after mount

  // Refs for keyboard handler (avoids stale closures)
  const inMarkerRef = useRef(inMarker);
  const outMarkerRef = useRef(outMarker);
  const audioInRef = useRef(audioIn);
  const audioOutRef = useRef(audioOut);
  audioInRef.current = audioIn;
  audioOutRef.current = audioOut;
  const durationRef = useRef(duration);
  const frameDurRef = useRef(1 / 30);
  useEffect(() => { inMarkerRef.current = inMarker; }, [inMarker]);
  useEffect(() => { outMarkerRef.current = outMarker; }, [outMarker]);
  useEffect(() => { durationRef.current = duration; }, [duration]);
  useEffect(() => { frameDurRef.current = edit?.frame_dur ?? 1 / 30; }, [edit]);

  // ── progress events ─────────────────────────────────────────────────────
  useEffect(() => {
    const unlistenPipeline = listen<PipelineProgress>(
      "pipeline-progress",
      (event) => {
        setStage(event.payload.stage);
        setStatusMsg(event.payload.message);
        setProgress(event.payload.percent);
      },
    );

    const unlistenDownload = listen<{ percent: number }>(
      "download-progress",
      (event) => {
        setProgress(event.payload.percent);
      },
    );

    const unlistenExport = listen<ExportProgress>(
      "export-progress",
      (event) => {
        setExportProgress(event.payload.percent);
        setExportStatus(event.payload.status);
        if (event.payload.status === "complete") {
          setExporting(false);
        }
      },
    );

    const unlistenPreview = listen<{ percent: number }>(
      "preview-progress",
      (event) => setPreviewProgress(event.payload.percent),
    );

    // First-time whisper model download happens inside the "ensure_model" stage.
    const unlistenModel = listen<{ percent: number; status: string }>(
      "model-download-progress",
      (event) => {
        if (event.payload.status !== "downloading") return;
        const pct = event.payload.percent;
        setStatusMsg(`Downloading whisper model... ${Math.round(pct)}%`);
        setProgress(40 + pct * 0.15);
      },
    );

    return () => {
      unlistenPipeline.then((fn) => fn());
      unlistenDownload.then((fn) => fn());
      unlistenExport.then((fn) => fn());
      unlistenPreview.then((fn) => fn());
      unlistenModel.then((fn) => fn());
    };
  }, []);

  // ── search debounce ──────────────────────────────────────────────────────
  useEffect(() => {
    if (!searchQuery.trim()) {
      setDebouncedQuery("");
      return;
    }
    const timer = setTimeout(() => setDebouncedQuery(searchQuery), 250);
    return () => clearTimeout(timer);
  }, [searchQuery]);

  // ── video metadata / timeupdate ─────────────────────────────────────────
  useEffect(() => {
    const video = videoRef.current;
    console.log("[video] useEffect fired", { hasVideo: !!video, videoSrc });
    if (!video || !videoSrc) return;

    const onMeta = () => {
      const d = video.duration;
      console.log("[video] loadedmetadata", { duration: d, readyState: video.readyState, networkState: video.networkState, error: video.error });
      if (Number.isFinite(d) && d > 0) {
        setDuration(d);
        setZoomMin(0);
        setZoomMax(d);
        if (pendingFullOutRef.current) {
          pendingFullOutRef.current = false;
          setOutMarker(d);
        }
        if (pendingSeekRef.current !== null) {
          video.currentTime = Math.min(d, Math.max(0, pendingSeekRef.current));
          pendingSeekRef.current = null;
        }
      }
    };

    const onTime = () => setCurrentTime(video.currentTime);
    const onError = () => console.error("[video] error event", video.error);
    const onStalled = () => console.warn("[video] stalled", { readyState: video.readyState, networkState: video.networkState });
    const onSuspend = () => console.log("[video] suspend (likely load complete)", { readyState: video.readyState });
    const onCanPlay = () => console.log("[video] canplay", { duration: video.duration, readyState: video.readyState });

    video.addEventListener("loadedmetadata", onMeta);
    video.addEventListener("timeupdate", onTime);
    video.addEventListener("error", onError);
    video.addEventListener("stalled", onStalled);
    video.addEventListener("suspend", onSuspend);
    video.addEventListener("canplay", onCanPlay);
    return () => {
      video.removeEventListener("loadedmetadata", onMeta);
      video.removeEventListener("timeupdate", onTime);
      video.removeEventListener("error", onError);
      video.removeEventListener("stalled", onStalled);
      video.removeEventListener("suspend", onSuspend);
      video.removeEventListener("canplay", onCanPlay);
    };
  }, [videoSrc]);

  const showTimeline = sectionPath && duration > 0;
  const hasVideo = sectionPath && duration > 0;

  // ── measure timeline width after mount / resize ────────────────────────
  useEffect(() => {
    const el = timelineRef.current;
    if (!el) {
      setTimelineWidth(0);
      return;
    }
    const measure = () => setTimelineWidth(el.getBoundingClientRect().width);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [showTimeline]);

  // ── keyboard shortcuts ───────────────────────────────────────────────────
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // While the preview modal is open, keys belong to it: space toggles the
      // preview, Escape closes it, editor shortcuts are disabled.
      if (previewOpenRef.current) {
        const pv = previewVideoRef.current;
        if (e.key === " ") {
          e.preventDefault();
          if (pv) {
            if (pv.paused) pv.play().catch(() => {});
            else pv.pause();
          }
        } else if (e.key === "Escape") {
          e.preventDefault();
          closePreviewRef.current();
        }
        return;
      }
      const video = videoRef.current;
      if (!video || !hasVideo) return;
      // Don't capture when typing in inputs
      if (
        e.target instanceof HTMLInputElement ||
        e.target instanceof HTMLTextAreaElement
      )
        return;

      const FRAME = frameDurRef.current;
      const ARROW_SKIP = 5; // seconds for left/right arrow
      // Step exactly one frame on the file's real frame grid. Land 1ms inside
      // the target frame so float rounding can't show its neighbour.
      const stepFrames = (n: number) => {
        const k = Math.floor(video.currentTime / FRAME + 1e-3) + n;
        const max = (video.duration || Infinity) - FRAME / 2;
        video.currentTime = Math.min(max, Math.max(0, k * FRAME + 0.001));
      };

      const getKeypoints = (): number[] => {
        const pts = [
          0,
          inMarkerRef.current,
          outMarkerRef.current,
          audioInRef.current,
          audioOutRef.current,
          durationRef.current,
        ].filter((k) => Number.isFinite(k));
        return [...new Set(pts)].sort((a, b) => a - b);
      };

      switch (e.key) {
        case " ":
          e.preventDefault();
          if (video.paused) {
            video.play();
          } else {
            video.pause();
          }
          break;
        case ",":
          e.preventDefault();
          video.pause();
          stepFrames(-1);
          break;
        case ".":
          e.preventDefault();
          video.pause();
          stepFrames(1);
          break;
        case "ArrowLeft":
          e.preventDefault();
          if (e.ctrlKey) {
            const kps = getKeypoints();
            const prev = kps.filter((k) => k < video.currentTime - 0.01).pop();
            if (prev !== undefined) video.currentTime = prev;
          } else {
            video.currentTime = Math.max(0, video.currentTime - ARROW_SKIP);
          }
          break;
        case "ArrowRight":
          e.preventDefault();
          if (e.ctrlKey) {
            const kps = getKeypoints();
            const next = kps.find((k) => k > video.currentTime + 0.01);
            if (next !== undefined) video.currentTime = next;
          } else {
            video.currentTime = Math.min(
              video.duration || Infinity,
              video.currentTime + ARROW_SKIP,
            );
          }
          break;
        case "i":
        case "I":
          e.preventDefault();
          if (e.shiftKey) {
            setAudioInRaw(video.currentTime);
            setLinkIn(false);
          } else {
            setInMarker(video.currentTime);
          }
          break;
        case "o":
        case "O":
          e.preventDefault();
          if (e.shiftKey) {
            setAudioOutRaw(video.currentTime);
            setLinkOut(false);
          } else {
            setOutMarker(video.currentTime);
          }
          break;
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [hasVideo]);

  // ── preview clip: cut section with ffmpeg, play in modal ───────────────
  // Played over the local HTTP server like the editor. (Blob URLs feed
  // WebKitGTK's pipeline corrupted data; the old HTTP stall was the encode's
  // zero average bitrate, fixed in media::cut.)
  const handlePreviewClip = async () => {
    if (!edit || generatingPreview) return;
    setGeneratingPreview(true);
    setPreviewProgress(0);
    setPreviewOpen(true);
    setStatusMsg("Generating preview...");

    try {
      const p = (await invoke("preview_clip", {
        edit,
        inSec: inMarker,
        outSec: outMarker,
        audioInSec: audioIn,
        audioOutSec: audioOut,
        declick,
      })) as PreviewResult;
      console.log("[preview] preview_clip", p);
      setPreviewVideoSrc(p.url);
      setStatusMsg(
        `Preview ready: ${p.frames} frames (${formatTimeSec(p.start)}–${formatTimeSec(p.end)})`,
      );
    } catch (e) {
      setStatusMsg(`Preview failed: ${e}`);
      setPreviewOpen(false);
    } finally {
      setGeneratingPreview(false);
    }
  };

  const closePreviewRef = useRef<() => void>(() => {});
  const closePreview = useCallback(() => {
    setPreviewOpen(false);
    setPreviewVideoSrc(null);
  }, []);
  closePreviewRef.current = closePreview;

  // ── autoplay preview once it can play through ──────────────────────────
  useEffect(() => {
    const vid = previewVideoRef.current;
    if (!vid || !previewVideoSrc) return;
    const play = () => {
      vid.play().catch(() => {
        /* autoplay refused: stay paused with controls for the user */
      });
    };
    vid.addEventListener("canplaythrough", play, { once: true });
    return () => vid.removeEventListener("canplaythrough", play);
  }, [previewVideoSrc]);

  // ── process video ───────────────────────────────────────────────────────
  const resetEditor = () => {
    setSelectedIdx(null);
    setEdit(null);
    setVideoSrc(null);
    setDuration(0);
    setCurrentTime(0);
    pendingFullOutRef.current = false;
  };

  const loadTranscript = async (path: string, source: "youtube" | "whisper") => {
    const td = (await invoke("read_transcript", { path })) as TranscriptData;
    setSegments(td.segments);
    setTranscriptData(td);
    setStatusMsg(
      `${td.segments.length} segments${source === "whisper" ? " (whisper)" : ""} — search or click one to preview`,
    );
  };

  const handleProcess = async () => {
    if (!url.trim()) return;
    setProcessing(true);
    setProgress(0);
    setStage("starting");
    setStatusMsg("Fetching transcript...");
    setResult(null);
    setSegments([]);
    setTranscriptData(null);
    setSearchQuery("");
    resetEditor();

    try {
      const r = (await invoke("process_video", {
        videoId: crypto.randomUUID(),
        url: url.trim(),
      })) as ProcessResult;

      setResult(r);
      if (r.transcript_path) {
        await loadTranscript(r.transcript_path, "youtube");
      } else {
        setStatusMsg(r.caption_note ?? "No captions found.");
      }
      setProgress(100);
    } catch (e) {
      setStatusMsg(`Error: ${e}`);
    } finally {
      setProcessing(false);
    }
  };

  // ── no captions: whisper transcription ─────────────────────────────────
  const handleTranscribe = async () => {
    if (!result || processing) return;
    setProcessing(true);
    setProgress(0);
    setStage("download");
    setStatusMsg("Downloading audio for transcription...");
    try {
      const t = (await invoke("transcribe_video", {
        videoId: result.video_id,
        url: result.url,
      })) as { transcript_path: string; transcript_source: "whisper" };
      setResult({ ...result, transcript_path: t.transcript_path, transcript_source: "whisper", caption_note: null });
      await loadTranscript(t.transcript_path, "whisper");
    } catch (e) {
      setStatusMsg(`Transcription failed: ${e}`);
    } finally {
      setProcessing(false);
    }
  };

  // ── open an editing copy in the player ─────────────────────────────────
  const openEdit = async (src: EditSource, inSec: number, outSec: number) => {
    setInMarker(Math.max(0, inSec));
    setOutMarker(Math.max(0, outSec));
    setLinkIn(true);
    setLinkOut(true);
    setEdit(src);
    const videoUrl = (await invoke("serve_video", { path: src.path })) as string;
    console.log("[video] serve_video URL", videoUrl);
    setVideoSrc(videoUrl);
  };

  // ── no captions: full video for manual trimming ────────────────────────
  const handleLoadFull = async () => {
    if (!result || downloadingSection) return;
    resetEditor();
    setDownloadingSection(true);
    setProgress(0);
    setStage("download");
    setStatusMsg("Downloading full video (720p)...");
    try {
      const src = (await invoke("load_full_video", {
        videoId: result.video_id,
        url: result.url,
      })) as EditSource;
      pendingFullOutRef.current = true; // Out = end once duration is known
      await openEdit(src, 0, 0);
      setStatusMsg("Full video loaded — set In/Out to trim.");
    } catch (e) {
      setStatusMsg(`${e}`);
    } finally {
      setDownloadingSection(false);
    }
  };

  // ── segment / search match → download 720p section around it ───────────
  const openSection = async (startMs: number, endMs: number) => {
    if (!result || downloadingSection) return;
    resetEditor();
    setDownloadingSection(true);
    setProgress(0);
    setStage("download");
    setStatusMsg("Downloading section (720p)...");
    try {
      const src = (await invoke("download_section", {
        videoId: result.video_id,
        url: result.url,
        startMs,
        endMs,
      })) as EditSource;
      await openEdit(src, startMs / 1000 - src.video_origin, endMs / 1000 - src.video_origin);
      setStatusMsg(`Section ready: ${formatTime(startMs)} – ${formatTime(endMs)}`);
    } catch (e) {
      setStatusMsg(`${e}`);
    } finally {
      setDownloadingSection(false);
    }
  };

  // ── widen the editing section, keeping markers on the same absolute time ──
  const handleExtend = async (side: "start" | "end") => {
    if (!result || !edit || downloadingSection) return;
    setDownloadingSection(true);
    setProgress(0);
    setStage("download");
    setStatusMsg(`Extending section ${side === "start" ? "start" : "end"} by 10s...`);
    try {
      const src = (await invoke("extend_section", {
        videoId: result.video_id,
        url: result.url,
        edit,
        side,
      })) as EditSource;
      // Local times are relative to the file's first frame; rebase them.
      const shift = edit.video_origin - src.video_origin;
      setInMarker((t) => t + shift);
      setOutMarker((t) => t + shift);
      setAudioInRaw((t) => t + shift);
      setAudioOutRaw((t) => t + shift);
      pendingSeekRef.current = (videoRef.current?.currentTime ?? 0) + shift;
      setEdit(src);
      setDuration(0);
      setVideoSrc((await invoke("serve_video", { path: src.path })) as string);
      setStatusMsg(`Section now ${formatTimeSec(src.section![0])} – ${formatTimeSec(src.section![1])}`);
    } catch (e) {
      setStatusMsg(`${e}`);
    } finally {
      setDownloadingSection(false);
    }
  };

  const handleSegmentClick = async (idx: number) => {
    const seg = segments[idx];
    await openSection(seg.start_ms, seg.end_ms);
    setSelectedIdx(idx);
  };

  // ── export clip ──────────────────────────────────────────────────────────
  const handleExport = async () => {
    if (!edit || !result || exporting) return;

    // Suggest filename from video title + absolute timestamp range
    const title = (result.title || "clip")
      .replace(/[<>:"/\\|?*]/g, "") // strip invalid filename chars
      .trim()
      .slice(0, 80);
    const absIn = (edit.video_origin + Math.min(inMarker, audioIn)) * 1000;
    const absOut = (edit.video_origin + Math.max(outMarker, audioOut)) * 1000;
    const suggestedName = `${title} ${formatTime(absIn)}-${formatTime(absOut)}`
      .replace(/:/g, ".")
      .replace(/\s+/g, " ");

    const outPath = await save({
      defaultPath: `${suggestedName}.mp4`,
      filters: [{ name: "MP4 Video", extensions: ["mp4"] }],
    });

    if (!outPath) return; // user cancelled

    setExporting(true);
    setExportProgress(0);
    setExportStatus(quality === "720p" ? "encoding" : "downloading");
    setStatusMsg("Exporting clip...");

    try {
      const path = (await invoke("export_clip", {
        videoId: result.video_id,
        url: result.url,
        edit,
        inSec: inMarker,
        outSec: outMarker,
        audioInSec: audioIn,
        audioOutSec: audioOut,
        declick,
        resolution: quality,
        outputPath: outPath,
      })) as string;
      setStatusMsg(`Exported: ${path}`);
    } catch (e) {
      setStatusMsg(`Export failed: ${e}`);
    } finally {
      setExporting(false);
    }
  };

  // ── timeline: pixel ↔ time helpers ──────────────────────────────────────
  const timelineDimensions = useCallback(() => {
    const el = timelineRef.current;
    if (!el) return { width: 0, left: 0 };
    const r = el.getBoundingClientRect();
    return { width: r.width, left: r.left };
  }, []);

  const pxToTime = useCallback(
    (px: number, width?: number): number => {
      const w = width ?? timelineWidth;
      if (w === 0) return 0;
      const range = zoomMax - zoomMin;
      if (range <= 0) return 0;
      return zoomMin + (px / w) * range;
    },
    [zoomMin, zoomMax, timelineWidth],
  );

  const timeToPx = useCallback(
    (t: number, width?: number): number => {
      const w = width ?? timelineWidth;
      if (w === 0) return 0;
      const range = zoomMax - zoomMin;
      if (range <= 0) return 0;
      return ((t - zoomMin) / range) * w;
    },
    [zoomMin, zoomMax, timelineWidth],
  );

  // ── timeline pointer handlers ───────────────────────────────────────────
  const handleTimelinePointerDown = useCallback(
    (e: React.PointerEvent) => {
      const { left, width } = timelineDimensions();
      if (width === 0) return;

      const px = e.clientX - left;

      // Check if clicking on a handle
      const handleSize = 10; // px radius around handle center
      const inPx = timeToPx(inMarker);
      const outPx = timeToPx(outMarker);
      const leftEdgePx = 6;
      const rightEdgePx = width - 6;

      if (Math.abs(px - leftEdgePx) < handleSize) {
        draggingRef.current = "zoom-left";
      } else if (Math.abs(px - rightEdgePx) < handleSize) {
        draggingRef.current = "zoom-right";
      } else if (Math.abs(px - inPx) < handleSize) {
        draggingRef.current = "in";
      } else if (Math.abs(px - outPx) < handleSize) {
        draggingRef.current = "out";
      } else {
        // Click on bar → seek
        const t = pxToTime(px);
        const clamped = Math.max(0, Math.min(duration, t));
        if (videoRef.current) {
          videoRef.current.currentTime = clamped;
        }
        return;
      }

      e.currentTarget.setPointerCapture(e.pointerId);
    },
    [inMarker, outMarker, timeToPx, pxToTime, duration, timelineDimensions],
  );

  const handleTimelinePointerMove = useCallback(
    (e: React.PointerEvent) => {
      const drag = draggingRef.current;
      if (!drag) return;

      const { left, width } = timelineDimensions();
      if (width === 0) return;

      const px = e.clientX - left;
      const t = pxToTime(px);

      switch (drag) {
        case "in":
          setInMarker(Math.max(0, Math.min(outMarker - 0.1, t)));
          break;
        case "out":
          setOutMarker(Math.max(inMarker + 0.1, Math.min(duration, t)));
          break;
        case "zoom-left":
          if (t < zoomMax - 1) setZoomMin(Math.max(0, t));
          break;
        case "zoom-right":
          if (t > zoomMin + 1) setZoomMax(Math.min(duration, t));
          break;
      }
    },
    [inMarker, outMarker, zoomMin, zoomMax, duration, pxToTime, timelineDimensions],
  );

  const handleTimelinePointerUp = useCallback(() => {
    draggingRef.current = null;
  }, []);

  // ── audio lane: A-In / A-Out handles (dragging unlinks that side) ───────
  const handleAudioPointerDown = useCallback(
    (e: React.PointerEvent) => {
      const r = e.currentTarget.getBoundingClientRect();
      const px = e.clientX - r.left;
      const handleSize = 10;
      const inDist = Math.abs(px - timeToPx(audioIn));
      const outDist = Math.abs(px - timeToPx(audioOut));
      if (Math.min(inDist, outDist) < handleSize) {
        audioDragRef.current = inDist <= outDist ? "ain" : "aout";
        e.currentTarget.setPointerCapture(e.pointerId);
      } else if (videoRef.current) {
        videoRef.current.currentTime = Math.max(0, Math.min(duration, pxToTime(px)));
      }
    },
    [audioIn, audioOut, timeToPx, pxToTime, duration],
  );

  const handleAudioPointerMove = useCallback(
    (e: React.PointerEvent) => {
      const drag = audioDragRef.current;
      if (!drag) return;
      const r = e.currentTarget.getBoundingClientRect();
      const t = pxToTime(e.clientX - r.left);
      if (drag === "ain") {
        setAudioInRaw(Math.max(0, Math.min(audioOut - 0.1, t)));
        setLinkIn(false);
      } else {
        setAudioOutRaw(Math.max(audioIn + 0.1, Math.min(duration, t)));
        setLinkOut(false);
      }
    },
    [audioIn, audioOut, duration, pxToTime],
  );

  const handleAudioPointerUp = useCallback(() => {
    audioDragRef.current = null;
  }, []);

  // ── search matches (debounced, min 3 chars, limited) ────────────────────
  const MAX_VISIBLE = 50;

  const textLower = useMemo(
    () => transcriptData?.text.toLowerCase() ?? "",
    [transcriptData],
  );

  const searchMatches = useMemo(() => {
    if (!transcriptData || debouncedQuery.length < 3) return null;
    const { text, timepoints } = transcriptData;
    const q = debouncedQuery.toLowerCase();

    // Fast pass: find all match positions (no binary search yet)
    let totalHits = 0;
    let idx = 0;
    while ((idx = textLower.indexOf(q, idx)) !== -1) {
      totalHits++;
      idx += q.length;
      if (totalHits > MAX_VISIBLE * 3) break; // bail early on massive transcripts
    }

    if (totalHits === 0) return { matches: [] as SearchMatch[], total: 0 };

    // Only interpolate timestamps for the first MAX_VISIBLE
    const matches: SearchMatch[] = [];
    idx = 0;
    while (matches.length < MAX_VISIBLE && (idx = textLower.indexOf(q, idx)) !== -1) {
      const endIdx = idx + q.length;
      const { start_ms, end_ms } = findEnclosingTimepoints(timepoints, idx, endIdx);
      const ctxStart = Math.max(0, idx - 30);
      const ctxEnd = Math.min(text.length, endIdx + 30);
      matches.push({
        start_idx: idx,
        end_idx: endIdx,
        start_ms,
        end_ms,
        text: text.slice(ctxStart, ctxEnd),
      });
      idx = endIdx;
    }

    return { matches, total: totalHits };
  }, [transcriptData, debouncedQuery, textLower]);

  // The scan stops counting past MAX_VISIBLE * 3 hits.
  const matchCount = searchMatches
    ? searchMatches.total > MAX_VISIBLE * 3
      ? `${MAX_VISIBLE * 3}+`
      : `${searchMatches.total}`
    : null;

  // ── handle search match click ────────────────────────────────────────────
  const handleSearchMatchClick = (match: SearchMatch) =>
    openSection(match.start_ms, match.end_ms);

  // ── timeline rendering ──────────────────────────────────────────────────

  const renderTimeline = () => {
    if (!showTimeline) return null;

    const w = timelineWidth;
    const range = zoomMax - zoomMin;

    // Bar segment highlight
    const segLeft = timeToPx(inMarker, w);
    const segRight = timeToPx(outMarker, w);
    const playheadPx = timeToPx(currentTime, w);
    const aLeft = timeToPx(audioIn, w);
    const aRight = timeToPx(audioOut, w);

    // Overview bar: zoom window position relative to full duration
    const overviewZoomLeft = duration > 0 ? (zoomMin / duration) * 100 : 0;
    const overviewZoomWidth = duration > 0 ? (range / duration) * 100 : 100;

    return (
      <div className="timeline-container">
        {/* Time readout */}
        <div className="timeline-readout">
          {edit?.section && (
            <button
              className="timeline-extend"
              onClick={() => handleExtend("start")}
              disabled={downloadingSection || edit.section[0] === 0}
              title="Download 10s more before this section"
            >
              ⟵ +10s
            </button>
          )}
          <span>In: {formatTimeSec(inMarker)}</span>
          <span>Out: {formatTimeSec(outMarker)}</span>
          <span>
            Video {(outMarker - inMarker).toFixed(2)}s · Audio {(audioOut - audioIn).toFixed(2)}s
          </span>
          <label className="timeline-declick" title="20ms fade at the audio edges to avoid clicks">
            <input type="checkbox" checked={declick} onChange={(e) => setDeclick(e.target.checked)} />
            Fade audio edges
          </label>
          <span className="timeline-shortcuts">space , . ← → ctrl+←→  I/O  shift+I/O audio</span>
          <span className="timeline-clip">Clip: {(Math.max(outMarker, audioOut) - Math.min(inMarker, audioIn)).toFixed(2)}s</span>
          {edit?.section && (
            <button
              className="timeline-extend"
              onClick={() => handleExtend("end")}
              disabled={
                downloadingSection ||
                (!!result?.duration && edit.section[1] >= result.duration)
              }
              title="Download 10s more after this section"
            >
              +10s ⟶
            </button>
          )}
        </div>

        {/* Main zoom bar */}
        <div
          className="timeline-zoom-bar"
          ref={timelineRef}
          onPointerDown={handleTimelinePointerDown}
          onPointerMove={handleTimelinePointerMove}
          onPointerUp={handleTimelinePointerUp}
          onPointerCancel={handleTimelinePointerUp}
        >
          {/* Zoom edge handles */}
          <div
            className="timeline-zoom-handle left"
            style={{ left: 0 }}
            title="Drag to adjust zoom start"
          />
          <div
            className="timeline-zoom-handle right"
            style={{ right: 0 }}
            title="Drag to adjust zoom end"
          />

          {/* Segment highlight */}
          <div
            className="timeline-segment-highlight"
            style={{ left: segLeft, width: segRight - segLeft }}
          />

          {/* In marker */}
          <div
            className="timeline-marker in"
            style={{ left: segLeft }}
            title="In point — drag to adjust"
          >
            <div className="timeline-marker-line" />
            <div className="timeline-marker-label">{formatTimeSec(inMarker)}</div>
          </div>

          {/* Out marker */}
          <div
            className="timeline-marker out"
            style={{ left: segRight }}
            title="Out point — drag to adjust"
          >
            <div className="timeline-marker-line" />
            <div className="timeline-marker-label">{formatTimeSec(outMarker)}</div>
          </div>

          {/* Playhead */}
          {playheadPx >= 0 && playheadPx <= w && (
            <div
              className="timeline-playhead"
              style={{ left: playheadPx }}
            />
          )}

          {/* Tick marks */}
          {renderTicks(range, zoomMin, w)}
        </div>

        {/* Audio lane */}
        <div
          className="timeline-audio-lane"
          onPointerDown={handleAudioPointerDown}
          onPointerMove={handleAudioPointerMove}
          onPointerUp={handleAudioPointerUp}
          onPointerCancel={handleAudioPointerUp}
          title="Audio — drag the green handles (or shift+I / shift+O) to split from the video"
        >
          <div className="audio-range" style={{ left: aLeft, width: aRight - aLeft }} />
          {/* Audio without video: the edge frame is held */}
          {audioIn < inMarker && (
            <div className="audio-region hold" style={{ left: aLeft, width: segLeft - aLeft }}>hold</div>
          )}
          {audioOut > outMarker && (
            <div className="audio-region hold" style={{ left: segRight, width: aRight - segRight }}>hold</div>
          )}
          {/* Video without audio: silent */}
          {audioIn > inMarker && (
            <div className="audio-region silent" style={{ left: segLeft, width: aLeft - segLeft }}>silent</div>
          )}
          {audioOut < outMarker && (
            <div className="audio-region silent" style={{ left: aRight, width: segRight - aRight }}>silent</div>
          )}
          <div className="audio-handle" style={{ left: aLeft }} />
          <div className="audio-handle" style={{ left: aRight }} />
          {!linkIn && (
            <button
              className="audio-relink"
              style={{ left: aLeft - 22 }}
              onPointerDown={(e) => e.stopPropagation()}
              onClick={() => setLinkIn(true)}
              title="Relink audio In to video In"
            >
              ⟲
            </button>
          )}
          {!linkOut && (
            <button
              className="audio-relink"
              style={{ left: aRight + 6 }}
              onPointerDown={(e) => e.stopPropagation()}
              onClick={() => setLinkOut(true)}
              title="Relink audio Out to video Out"
            >
              ⟲
            </button>
          )}
          {playheadPx >= 0 && playheadPx <= w && (
            <div className="timeline-playhead" style={{ left: playheadPx }} />
          )}
        </div>

        {/* Overview bar */}
        <div className="timeline-overview">
          <div
            className="timeline-overview-zoom"
            style={{ left: `${overviewZoomLeft}%`, width: `${overviewZoomWidth}%` }}
          />
        </div>
      </div>
    );
  };

  // ── tick marks for zoom bar ─────────────────────────────────────────────
  const renderTicks = (range: number, start: number, barWidth: number) => {
    // Choose tick interval: aim for ~4-8 ticks
    const roughTick = range / 6;
    let interval: number;
    if (roughTick >= 60) interval = 60;
    else if (roughTick >= 30) interval = 30;
    else if (roughTick >= 10) interval = 10;
    else if (roughTick >= 5) interval = 5;
    else if (roughTick >= 2) interval = 2;
    else interval = 1;

    const ticks: React.ReactNode[] = [];
    let t = Math.ceil(start / interval) * interval;
    while (t <= zoomMax) {
      const px = timeToPx(t, barWidth);
      ticks.push(
        <div
          key={t}
          className="timeline-tick"
          style={{ left: px }}
        >
          <div className="timeline-tick-mark" />
          <span className="timeline-tick-label">{formatTimeSec(t)}</span>
        </div>,
      );
      t += interval;
    }
    return ticks;
  };

  // ── render ──────────────────────────────────────────────────────────────
  return (
    <div className="app-container">
      <header className="toolbar">
        <input
          type="text"
          placeholder="Paste YouTube URL..."
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleProcess()}
        />
        <button onClick={handleProcess} disabled={!url.trim() || processing}>
          {processing ? "Loading..." : "Load"}
        </button>
      </header>

      {(processing || downloadingSection) && (
        <div className="progress-bar-container">
          <div className="progress-bar-track">
            <div
              className="progress-bar-fill"
              style={{ width: `${Math.max(progress, 2)}%` }}
            />
          </div>
          <span className="progress-stage">{stage.replace(/_/g, " ")}</span>
          <span className="progress-text">
            {statusMsg} {progress > 0 && progress < 100 ? `(${Math.round(progress)}%)` : ""}
          </span>
        </div>
      )}
      {!(processing || downloadingSection) && statusMsg && (
        <div className="status-line" title={statusMsg}>{statusMsg}</div>
      )}

      <main className="main-layout">
        <section className="video-panel">
          {sectionPath ? (
            <>
              <video
                key={videoSrc}
                ref={videoRef}
                src={videoSrc!}
                controls
                className="video-player"
                style={{ display: hasVideo ? "block" : "none" }}
              />
              {!hasVideo && (
                <div className="video-placeholder">Loading video...</div>
              )}
              {hasVideo && (
                <div className="video-controls">
                  {exporting ? (
                    <div className="export-progress-inline">
                      <span>
                        {exportStatus === "downloading" ? `Downloading ${quality}...` : "Encoding..."}
                      </span>
                      <div className="progress-bar-track small">
                        <div
                          className="progress-bar-fill"
                          style={{ width: `${Math.max(exportProgress, 2)}%` }}
                        />
                      </div>
                      <span className="progress-text">{Math.round(exportProgress)}%</span>
                    </div>
                  ) : (
                    <>
                      <button
                        className="preview-button"
                        onClick={handlePreviewClip}
                        disabled={exporting}
                      >
                        Preview Clip
                      </button>
                      <select
                        className="quality-select"
                        value={quality}
                        onChange={(e) => setQuality(e.target.value)}
                        title="Export quality"
                      >
                        <option value="best">Best</option>
                        <option value="1080p">1080p</option>
                        <option value="720p">720p</option>
                        <option value="480p">480p</option>
                      </select>
                      <button
                        className="export-button"
                        onClick={handleExport}
                        disabled={exporting}
                      >
                        Export Clip
                      </button>
                    </>
                  )}
                </div>
              )}
            </>
          ) : result && !processing && !downloadingSection && result.transcript_path ? (
            <div className="video-ready">
              <div className="result-label">Transcript loaded</div>
              <code className="path">{result.transcript_path}</code>
              <div className="result-source">
                Source:{" "}
                {result.transcript_source === "youtube"
                  ? "YouTube captions"
                  : "Whisper.cpp"}
              </div>
              <div className="hint">Search or click a segment to preview</div>
            </div>
          ) : result && !processing && !downloadingSection ? (
            <div className="video-ready">
              <div className="result-label">No transcript</div>
              <div className="hint">{result.caption_note}</div>
              <div className="no-caption-actions">
                <button className="preview-button" onClick={handleTranscribe}>
                  Transcribe with Whisper
                </button>
                <button className="export-button" onClick={handleLoadFull}>
                  Load full video (manual trim)
                </button>
              </div>
            </div>
          ) : !processing && !downloadingSection ? (
            <div className="video-placeholder">
              Enter a YouTube URL and click Load
            </div>
          ) : null}
        </section>

        <section className="search-panel">
          {segments.length > 0 ? (
            <>
              <div className="search-box">
                <input
                  type="text"
                  placeholder="Search transcript..."
                  value={searchQuery}
                  onChange={(e) => setSearchQuery(e.target.value)}
                  autoFocus
                />
                {matchCount !== null && (
                  <span className="match-count">
                    {matchCount} {matchCount === "1" ? "match" : "matches"}
                  </span>
                )}
              </div>
              <div className="transcript-list">
                {searchQuery.trim() ? (
                  searchMatches && searchMatches.matches.length > 0 ? (
                    <>
                      {searchMatches.matches.map((match, i) => (
                        <button
                          key={`${match.start_idx}-${i}`}
                          className="transcript-segment"
                          onClick={() => handleSearchMatchClick(match)}
                          disabled={downloadingSection}
                        >
                          <span className="seg-time">
                            {formatTime(match.start_ms)}
                          </span>
                          <span className="seg-text">
                            {highlightMatch(match.text, searchQuery)}
                          </span>
                        </button>
                      ))}
                      {searchMatches.total > searchMatches.matches.length && (
                        <div className="results-placeholder">
                          More matches not shown — narrow your search
                        </div>
                      )}
                    </>
                  ) : (
                    <div className="results-placeholder">No matches</div>
                  )
                ) : segments.length > 0 ? (
                  segments.map((seg) => {
                    const realIdx = segments.indexOf(seg);
                    return (
                      <button
                        key={`${seg.start_ms}-${realIdx}`}
                        className={`transcript-segment ${realIdx === selectedIdx ? "selected" : ""}`}
                        onClick={() => handleSegmentClick(realIdx)}
                        disabled={downloadingSection}
                      >
                        <span className="seg-time">
                          {formatTime(seg.start_ms)}
                        </span>
                        <span className="seg-text">{seg.text}</span>
                      </button>
                    );
                  })
                ) : (
                  <div className="results-placeholder">No segments found</div>
                )}
              </div>
            </>
          ) : (
            <div className="search-placeholder">
              <div className="results-placeholder">
                {!result
                  ? "No transcript loaded"
                  : result.transcript_path
                    ? "No segments found"
                    : edit
                      ? "Manual trim — no transcript"
                      : "No transcript"}
              </div>
            </div>
          )}
        </section>
      </main>

      <footer className="timeline-panel">
        {showTimeline ? (
          renderTimeline()
        ) : (
          <div className="timeline-placeholder">
            {sectionPath ? "Loading metadata..." : "Timeline"}
          </div>
        )}
      </footer>
      {/* ── Preview modal ──────────────────────────────────────────────── */}
      {previewOpen && (
        <div className="preview-modal-overlay" onClick={closePreview}>
          <div className="preview-modal" onClick={(e) => e.stopPropagation()}>
            <div className="preview-modal-header">
              <span>Preview Clip</span>
              <button className="preview-modal-close" onClick={closePreview}>
                ✕
              </button>
            </div>
            <div className="preview-modal-body">
              {generatingPreview ? (
                <div className="preview-modal-loading">
                  <div className="spinner" />
                  <span>
                    Generating preview… {previewProgress > 0 ? `${Math.round(previewProgress)}%` : ""}
                  </span>
                  <div className="progress-bar-track small">
                    <div
                      className="progress-bar-fill"
                      style={{ width: `${Math.max(previewProgress, 2)}%` }}
                    />
                  </div>
                </div>
              ) : previewVideoSrc ? (
                <video
                  key={previewVideoSrc}
                  ref={previewVideoRef}
                  src={previewVideoSrc}
                  controls
                  className="preview-modal-video"
                />
              ) : (
                <div className="preview-modal-error">Failed to generate preview</div>
              )}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default App;
