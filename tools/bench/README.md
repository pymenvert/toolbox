# Bench phase 0 — validation du pipeline vidéo

But : valider AVANT d'écrire l'application que la machine tient le décodage +
rendu GPU. À lancer sur chaque machine cible (Pi 4, Pi 5, desktop Linux).
À relancer après chaque upgrade kernel/GStreamer (régressions DMABUF connues sur Pi 5).

## Prérequis

```bash
sudo apt install -y gstreamer1.0-tools gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad ffmpeg
```

## Utilisation

```bash
./01_prepare_media.sh   # génère les clips de test (une fois)
./02_decode_bench.sh    # lance les mesures, écrit results/<host>_<date>.md
```

Le rapport indique pour chaque combinaison codec/résolution : décodeur utilisé
(HW ou soft), FPS moyen, frames perdues, charge CPU. 

## Critère de sortie phase 0 (PLAN.md)

- Pi 5 : HEVC 1080p60 **HW** fluide (≥ 59 fps, < 1% drop), H.264 1080p60 soft OK.
- Pi 4 : H.264 1080p60 **HW** fluide.
- Le rendu `glimagesink`/`kmssink` ne doit pas s'effondrer vs `fakesink`
  (sinon = problème de copie CPU→GPU, à investiguer avant de continuer).
