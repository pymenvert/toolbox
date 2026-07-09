#!/usr/bin/env bash
# Génère les clips de test du bench (mire animée + timecode incrusté).
# Idempotent : ne régénère pas un clip déjà présent.
set -euo pipefail

cd "$(dirname "$0")"
mkdir -p media
DUR=30 # secondes

command -v ffmpeg >/dev/null || { echo "ERREUR: ffmpeg manquant"; exit 1; }

# Source synthétique : mire SMPTE + compteur de frames (drawtext) pour pouvoir
# vérifier visuellement les saccades et, plus tard, mesurer la sync entre nodes.
# gen <nom> <taille> <fps> <codec> [options codec...]
gen() {
  local name="$1" size="$2" rate="$3" codec="$4"
  shift 4
  local out="media/${name}.mp4"
  if [[ -f "$out" ]]; then
    echo "OK (déjà présent) $out"
    return
  fi
  echo "Génération $out ..."
  ffmpeg -hide_banner -loglevel error \
    -f lavfi -i "testsrc2=size=${size}:rate=${rate},format=yuv420p" \
    -t "$DUR" \
    -vf "drawtext=text='%{n}':fontsize=h/8:fontcolor=white:box=1:boxcolor=black@0.6:x=(w-tw)/2:y=h-th-20" \
    -c:v "$codec" "$@" \
    -movflags +faststart \
    "$out"
  echo "OK $out"
}

# H.264 : décodé HW sur Pi 4, soft sur Pi 5 (le Pi 5 n'a plus de bloc H.264).
gen "h264_1080p60" "1920x1080" 60 libx264 -preset medium -crf 18 -g 60
gen "h264_1080p30" "1920x1080" 30 libx264 -preset medium -crf 18 -g 30

# HEVC : décodé HW sur Pi 5 (V4L2 stateless). GOP court = seek réactif.
gen "hevc_1080p60" "1920x1080" 60 libx265 -preset medium -crf 22 -g 60 -tag:v hvc1
gen "hevc_2160p30" "3840x2160" 30 libx265 -preset medium -crf 24 -g 30 -tag:v hvc1

echo
echo "Clips prêts dans $(pwd)/media/"
