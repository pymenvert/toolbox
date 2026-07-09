#!/usr/bin/env bash
# Bench décodage + rendu GStreamer. Écrit un rapport markdown dans results/.
#
# Deux étages par clip :
#   1. décodage pur   → fakesink        (plafond de décodage de la machine)
#   2. décodage+rendu → glimagesink/kmssink (le vrai chemin vers l'écran)
#
# Le décodeur est choisi par decodebin3 ; le rapport note lequel a été utilisé
# (v4l2* = hardware, avdec_* = software). Un écart important entre les deux
# étages signale un problème de copie CPU→GPU (voir README, pièges Pi 5).
set -euo pipefail

cd "$(dirname "$0")"
mkdir -p results

command -v gst-launch-1.0 >/dev/null || { echo "ERREUR: gstreamer manquant (voir README)"; exit 1; }
[[ -d media ]] || { echo "ERREUR: lancer d'abord ./01_prepare_media.sh"; exit 1; }

HOST="$(hostname)"
DATE="$(date +%Y-%m-%d_%H%M)"
REPORT="results/${HOST}_${DATE}.md"
MODEL="unknown"
[[ -f /proc/device-tree/model ]] && MODEL="$(tr -d '\0' </proc/device-tree/model)"

# Sink de rendu : kmssink si console pure (mode kiosque), sinon glimagesink.
RENDER_SINK="glimagesink sync=true"
if [[ -z "${DISPLAY:-}" && -z "${WAYLAND_DISPLAY:-}" ]]; then
  RENDER_SINK="kmssink sync=true"
fi

{
  echo "# Bench ${HOST} — ${DATE}"
  echo
  echo "- Machine : ${MODEL}"
  # shellcheck disable=SC1091
  echo "- OS : $(. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-?}")"
  echo "- Kernel : $(uname -r)"
  echo "- GStreamer : $(gst-launch-1.0 --version | head -n1)"
  echo "- Sink de rendu : ${RENDER_SINK%% *}"
  echo
  echo "| Clip | Étage | Décodeur | FPS moy | Drop | CPU moy |"
  echo "|---|---|---|---|---|---|"
} >"$REPORT"

# run_case <clip> <parser> <étage> <sink>
# Joue le clip, échantillonne le CPU, extrait décodeur/FPS/drops des logs.
run_case() {
  local clip="$1" parser="$2" stage="$3" sink="$4"
  local log statlog pid
  log="$(mktemp)"
  statlog="$(mktemp)"

  # timeout : un sink qui bloque (droits DRM, VT occupé) ne doit pas geler le bench.
  GST_DEBUG="decodebin3:4" timeout 90 gst-launch-1.0 -v -q \
    filesrc location="media/${clip}.mp4" ! qtdemux ! "$parser" ! \
    decodebin3 ! queue ! videoconvert ! \
    fpsdisplaysink text-overlay=false silent=false video-sink="$sink" \
    >"$log" 2>&1 &
  pid=$!

  while kill -0 "$pid" 2>/dev/null; do
    ps -p "$pid" -o %cpu= >>"$statlog" 2>/dev/null || true
    sleep 1
  done
  wait "$pid" || true

  local decoder fps drop cpu
  # grep -m1 (pas de "| head") : sous pipefail, head qui ferme le pipe ferait
  # sortir grep en erreur et dupliquerait la valeur avec le "|| echo '?'".
  decoder="$(grep -m1 -oE 'v4l2[a-z0-9]*dec|avdec_[a-z0-9]+' "$log" || echo '?')"
  fps="$(grep -oE 'average: [0-9.]+' "$log" | tail -n1 | grep -oE '[0-9.]+' || echo '?')"
  drop="$(grep -oE 'dropped: [0-9]+' "$log" | tail -n1 | grep -oE '[0-9]+' || echo '?')"
  cpu="$(awk '{s+=$1; n++} END {if (n>0) printf "%.0f%%", s/n; else print "?"}' "$statlog")"

  echo "| ${clip} | ${stage} | ${decoder} | ${fps} | ${drop} | ${cpu} |" >>"$REPORT"
  echo "  ${stage} : décodeur=${decoder} fps=${fps} drop=${drop} cpu=${cpu}"
  rm -f "$log" "$statlog"
}

for clip in h264_1080p30 h264_1080p60 hevc_1080p60 hevc_2160p30; do
  [[ -f "media/${clip}.mp4" ]] || { echo "skip ${clip} (absent)"; continue; }
  case "$clip" in
    h264*) parser="h264parse" ;;
    *)     parser="h265parse" ;;
  esac
  echo "=== ${clip} ==="
  run_case "$clip" "$parser" "décodage seul"  "fakesink sync=true"
  run_case "$clip" "$parser" "décodage+rendu" "$RENDER_SINK"
done

echo
echo "Rapport écrit : ${REPORT}"
cat "$REPORT"
