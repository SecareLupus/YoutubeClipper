import { useState } from "react";

interface AppState {
  currentVideo: {
    id: string;
    previewPath: string;
    masterPath: string;
    resolution: "720p" | "1080p" | "4k";
    status: "idle" | "downloading" | "transcribing" | "ready" | "error";
    progress: number;
  };
  search: {
    query: string;
    results: SearchResultInstance[];
    selectedInstanceIdx: number | null;
  };
  timeline: {
    currentTime: number;
    inMarker: number;
    outMarker: number;
    zoomWindow: {
      min: number;
      max: number;
    };
  };
}

interface SearchResultInstance {
  startTime: number;
  endTime: number;
  text: string;
}

const DEFAULT_STATE: AppState = {
  currentVideo: {
    id: "",
    previewPath: "",
    masterPath: "",
    resolution: "720p",
    status: "idle",
    progress: 0,
  },
  search: {
    query: "",
    results: [],
    selectedInstanceIdx: null,
  },
  timeline: {
    currentTime: 0,
    inMarker: 0,
    outMarker: 0,
    zoomWindow: {
      min: 0,
      max: 0,
    },
  },
};

function App() {
  const [_state, _setState] = useState<AppState>(DEFAULT_STATE);

  return (
    <div className="app-container">
      <main className="main-layout">
        <section className="video-panel">
          <div className="video-placeholder">Video Player</div>
        </section>
        <section className="search-panel">
          <div className="search-placeholder">
            <input
              type="text"
              placeholder="Search transcript..."
              disabled
            />
            <div className="results-placeholder">No results</div>
          </div>
        </section>
      </main>
      <footer className="timeline-panel">
        <div className="timeline-placeholder">Timeline</div>
      </footer>
    </div>
  );
}

export default App;
