#!/usr/bin/env bash
# Lancement portable (P1.10) : tout vit dans le dossier du script.
# Décompressez le binaire à côté de ce script et double-cliquez / lancez-le.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p media presets logs shaders
exec ./toolbox-node "$@"
