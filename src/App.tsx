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

interface DownloadProgress {
  video_id: string;
  resolution: string;
  percent: number;
  status: string;
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
  transcript_path: string;
  transcript_source: "youtube" | "whisper";
  title: string;
  url: string;
}

const PAD_SECONDS = 10;

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
  const [sectionPath, setSectionPath] = useState<string | null>(null);
  const [videoSrc, setVideoSrc] = useState<string | null>(null);
  const [downloadingSection, setDownloadingSection] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [exportProgress, setExportProgress] = useState(0);
  const [quality, setQuality] = useState("1080p");
  const [originalStartMs, setOriginalStartMs] = useState(0);
  const [originalEndMs, setOriginalEndMs] = useState(0);
  const [previewing, setPreviewing] = useState(false);

  // ── video / timeline state ──────────────────────────────────────────────
  const videoRef = useRef<HTMLVideoElement>(null);
  const timelineRef = useRef<HTMLDivElement>(null);
  const draggingRef = useRef<string | null>(null);

  const [duration, setDuration] = useState(0); // seconds
  const [currentTime, setCurrentTime] = useState(0); // seconds
  const [inMarker, setInMarker] = useState(0); // seconds (video-local)
  const [outMarker, setOutMarker] = useState(0); // seconds (video-local)
  const [zoomMin, setZoomMin] = useState(0); // seconds
  const [zoomMax, setZoomMax] = useState(30); // seconds

  // Refs for keyboard handler (avoids stale closures)
  const inMarkerRef = useRef(inMarker);
  const outMarkerRef = useRef(outMarker);
  const durationRef = useRef(duration);
  useEffect(() => { inMarkerRef.current = inMarker; }, [inMarker]);
  useEffect(() => { outMarkerRef.current = outMarker; }, [outMarker]);
  useEffect(() => { durationRef.current = duration; }, [duration]);

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

    const unlistenDownload = listen<DownloadProgress>(
      "download-progress",
      (event) => {
        setProgress(event.payload.percent);
      },
    );

    const unlistenExport = listen<ExportProgress>(
      "export-progress",
      (event) => {
        setExportProgress(event.payload.percent);
        if (event.payload.status === "complete") {
          setExporting(false);
        }
      },
    );

    return () => {
      unlistenPipeline.then((fn) => fn());
      unlistenDownload.then((fn) => fn());
      unlistenExport.then((fn) => fn());
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

  // ── keyboard shortcuts ───────────────────────────────────────────────────
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      const video = videoRef.current;
      if (!video || !hasVideo) return;
      // Don't capture when typing in inputs
      if (
        e.target instanceof HTMLInputElement ||
        e.target instanceof HTMLTextAreaElement
      )
        return;

      const FRAME = 1 / 30;
      const ARROW_SKIP = 5; // seconds for left/right arrow

      const getKeypoints = (): number[] => {
        const pts = [
          0,
          inMarkerRef.current,
          outMarkerRef.current,
          durationRef.current,
        ].filter((k) => Number.isFinite(k));
        return [...new Set(pts)].sort((a, b) => a - b);
      };

      switch (e.key) {
        case " ":
          e.preventDefault();
          setPreviewing(false);
          if (video.paused) {
            video.play();
          } else {
            video.pause();
          }
          break;
        case ",":
          e.preventDefault();
          setPreviewing(false);
          video.currentTime = Math.max(0, video.currentTime - FRAME);
          break;
        case ".":
          e.preventDefault();
          setPreviewing(false);
          video.currentTime = Math.min(
            video.duration || Infinity,
            video.currentTime + FRAME,
          );
          break;
        case "ArrowLeft":
          e.preventDefault();
          setPreviewing(false);
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
          setPreviewing(false);
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
          setInMarker(video.currentTime);
          break;
        case "o":
        case "O":
          e.preventDefault();
          setOutMarker(video.currentTime);
          break;
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [hasVideo]);

  // ── preview clip: seek-to-Out, wait for buffer flush, pause ──────────────
  useEffect(() => {
    if (!previewing || !videoRef.current) return;
    const video = videoRef.current;

    const onEnded = () => {
      video.pause();
      setPreviewing(false);
    };
    video.addEventListener("ended", onEnded);

    let id: number;
    const checkFrame = () => {
      if (video.currentTime >= outMarker) {
        // Seek to Out marker — flushes audio decode buffer
        video.currentTime = outMarker;
        const onSeeked = () => {
          video.pause();
          video.removeEventListener("seeked", onSeeked);
          setPreviewing(false);
        };
        video.addEventListener("seeked", onSeeked);
        return;
      }
      id = requestAnimationFrame(checkFrame);
    };
    id = requestAnimationFrame(checkFrame);

    return () => {
      cancelAnimationFrame(id);
      video.removeEventListener("ended", onEnded);
    };
  }, [previewing, outMarker]);

  // ── process video ───────────────────────────────────────────────────────
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
    setSelectedIdx(null);
    setSectionPath(null);
    setVideoSrc(null);
    setDuration(0);
    setCurrentTime(0);

    try {
      const r = (await invoke("process_video", {
        videoId: crypto.randomUUID(),
        url: url.trim(),
      })) as ProcessResult;

      setResult(r);

      const td = (await invoke("read_transcript", {
        path: r.transcript_path,
      })) as TranscriptData;

      setSegments(td.segments);
      setTranscriptData(td);
      setStatusMsg(
        r.transcript_source === "youtube"
          ? `${td.segments.length} segments — search or click one to preview`
          : `${td.segments.length} segments (whisper) — search or click one to preview`,
      );
      setProgress(100);
    } catch (e) {
      setStatusMsg(`Error: ${e}`);
    } finally {
      setProcessing(false);
    }
  };

  // ── segment click → download section ────────────────────────────────────
  const handleSegmentClick = async (idx: number) => {
    if (!result || downloadingSection) return;
    setSelectedIdx(idx);
    setDownloadingSection(true);
    setSectionPath(null);
    setVideoSrc(null);
    setStatusMsg("Downloading section preview...");

    const seg = segments[idx];
    try {
      const path = (await invoke("download_section", {
        videoId: result.video_id,
        url: result.url,
        startMs: seg.start_ms,
        endMs: seg.end_ms,
      })) as string;

      const segStartSec = seg.start_ms / 1000;
      const segEndSec = seg.end_ms / 1000;
      const padBefore = Math.min(PAD_SECONDS, segStartSec);
      setInMarker(padBefore);
      setOutMarker(padBefore + (segEndSec - segStartSec));
      setOriginalStartMs(seg.start_ms);
      setOriginalEndMs(seg.end_ms);

      setSectionPath(path);
      const videoUrl = await invoke("serve_video", { path }) as string;
      console.log("[video] serve_video URL", videoUrl);
      setVideoSrc(videoUrl);
      setStatusMsg(
        `Section ready: ${formatTime(seg.start_ms)} – ${formatTime(seg.end_ms)}`,
      );
    } catch (e) {
      setStatusMsg(`Section download failed: ${e}`);
    } finally {
      setDownloadingSection(false);
    }
  };

  // ── preview clip ──────────────────────────────────────────────────────────
  const handlePreviewClip = () => {
    const video = videoRef.current;
    if (!video) return;
    video.currentTime = inMarker;
    video.play();
    setPreviewing(true);
  };

  // ── export clip ──────────────────────────────────────────────────────────
  const handleExport = async () => {
    if (!sectionPath || exporting) return;

    // Suggest filename from video title + timestamp range
    const title = (result?.title || "clip")
      .replace(/[<>:"/\\|?*]/g, "") // strip invalid filename chars
      .trim()
      .slice(0, 80);
    const suggestedName = `${title} ${formatTime(originalStartMs)}-${formatTime(originalEndMs)}`.replace(/\s+/g, " ");

    const outPath = await save({
      defaultPath: `${suggestedName}.mp4`,
      filters: [{ name: "MP4 Video", extensions: ["mp4"] }],
    });

    if (!outPath) return; // user cancelled

    setExporting(true);
    setExportProgress(0);
    setStatusMsg("Exporting clip...");

    try {
      const path = (await invoke("export_slice", {
        videoId: result!.video_id,
        url: result!.url,
        startMs: originalStartMs,
        endMs: originalEndMs,
        startSec: inMarker,
        endSec: outMarker,
        resolution: quality,
        outputPath: outPath,
      })) as string;
      setStatusMsg(`Exported: ${path}`);
    } catch (e) {
      setExporting(false);
      setStatusMsg(`Export failed: ${e}`);
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
    (px: number): number => {
      const { width } = timelineDimensions();
      if (width === 0) return 0;
      const range = zoomMax - zoomMin;
      if (range <= 0) return 0;
      return zoomMin + (px / width) * range;
    },
    [zoomMin, zoomMax, timelineDimensions],
  );

  const timeToPx = useCallback(
    (t: number): number => {
      const { width } = timelineDimensions();
      if (width === 0) return 0;
      const range = zoomMax - zoomMin;
      if (range <= 0) return 0;
      return ((t - zoomMin) / range) * width;
    },
    [zoomMin, zoomMax, timelineDimensions],
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

  const matchCount = searchMatches ? searchMatches.total : null;

  // ── handle search match click ────────────────────────────────────────────
  const handleSearchMatchClick = async (match: SearchMatch) => {
    if (!result || downloadingSection) return;
    setSelectedIdx(null);
    setDownloadingSection(true);
    setSectionPath(null);
    setVideoSrc(null);
    setStatusMsg("Downloading section preview...");

    try {
      const path = (await invoke("download_section", {
        videoId: result.video_id,
        url: result.url,
        startMs: match.start_ms,
        endMs: match.end_ms,
      })) as string;

      const segStartSec = match.start_ms / 1000;
      const segEndSec = match.end_ms / 1000;
      const padBefore = Math.min(PAD_SECONDS, segStartSec);
      setInMarker(padBefore);
      setOutMarker(padBefore + (segEndSec - segStartSec));
      setOriginalStartMs(match.start_ms);
      setOriginalEndMs(match.end_ms);

      setSectionPath(path);
      const videoUrl = await invoke("serve_video", { path }) as string;
      console.log("[video] serve_video URL (search match)", videoUrl);
      setVideoSrc(videoUrl);
      setStatusMsg(
        `Section ready: ${formatTime(match.start_ms)} – ${formatTime(match.end_ms)}`,
      );
    } catch (e) {
      setStatusMsg(`Section download failed: ${e}`);
    } finally {
      setDownloadingSection(false);
    }
  };

  // ── timeline rendering ──────────────────────────────────────────────────

  const renderTimeline = () => {
    if (!showTimeline) return null;

    const w = timelineDimensions().width;
    const range = zoomMax - zoomMin;

    // Bar segment highlight
    const segLeft = timeToPx(inMarker);
    const segRight = timeToPx(outMarker);
    const playheadPx = timeToPx(currentTime);

    // Overview bar: zoom window position relative to full duration
    const overviewZoomLeft = duration > 0 ? (zoomMin / duration) * 100 : 0;
    const overviewZoomWidth = duration > 0 ? (range / duration) * 100 : 100;

    return (
      <div className="timeline-container">
        {/* Time readout */}
        <div className="timeline-readout">
          <span>In: {formatTimeSec(inMarker)}</span>
          <span>Out: {formatTimeSec(outMarker)}</span>
          <span>Clip: {formatTimeSec(outMarker - inMarker)}</span>
          <span className="timeline-shortcuts">space , . ← → ctrl+←→  I/O</span>
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
          {renderTicks(range, zoomMin)}
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
  const renderTicks = (range: number, start: number) => {
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
      const px = timeToPx(t);
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
          <span className="progress-text">{statusMsg}</span>
        </div>
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
                      <span>Encoding...</span>
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
          ) : result && !processing ? (
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
          ) : !processing ? (
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
                    {matchCount} / {segments.length}
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
                          +{searchMatches.total - searchMatches.matches.length} more — narrow your search
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
                {result ? "No segments found" : "No transcript loaded"}
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
    </div>
  );
}

export default App;
