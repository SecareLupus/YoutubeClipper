#!/bin/sh
# Frame-accuracy regression check against a real YouTube video.
# Downloads a 720p editing section and a higher-res export section with the
# same yt-dlp arguments the app uses, then runs the ignored e2e test in
# src-tauri/src/media.rs (needs yt-dlp and ffmpeg on PATH).
#
# Usage: scripts/e2e-media.sh <youtube-url> <start-seconds> [export-res-height]
set -eu
URL="$1"
START="$2"
EXPORT_H="${3:-1080}"
EXPORT_START=$((START > 2 ? START - 2 : 0))
DIR="${TMPDIR:-/tmp}/bc-e2e"
mkdir -p "$DIR"
rm -f "$DIR"/edit.* "$DIR"/export.*

fmt() {
  echo "bestvideo[height<=$1][vcodec^=avc1]+bestaudio[acodec^=mp4a]/bestvideo[height<=$1]+bestaudio/best[height<=$1]"
}

yt-dlp -q -f "$(fmt 720)" --download-sections "*$START-$((START + 15))" \
  --merge-output-format mp4 --no-playlist -o "$DIR/edit.%(ext)s" "$URL"
yt-dlp -q -f "$(fmt "$EXPORT_H")" --download-sections "*$EXPORT_START-$((START + 15))" \
  --merge-output-format mp4 --no-playlist -o "$DIR/export.%(ext)s" "$URL"

cd "$(dirname "$0")/../src-tauri"
BC_E2E_EDIT="$DIR/edit.mp4" BC_E2E_START="$START" \
BC_E2E_EXPORT="$DIR/export.mp4" BC_E2E_EXPORT_START="$EXPORT_START" \
  cargo test --lib e2e -- --ignored --nocapture
