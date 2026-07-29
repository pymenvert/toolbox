#!/usr/bin/env bash
# Smoke test (CI et local) : lance le VRAI binaire sur une config minimale,
# vérifie que l'API répond (health, state, features) puis l'arrête.
# Fonctionne sur les runners headless : la fenêtre de sortie est coupée via
# fonctions.json et le node continue sans environnement graphique.
#
# Usage : smoke.sh /chemin/vers/toolbox-node [port-http]

set -euo pipefail

BIN="${1:?usage: smoke.sh /chemin/vers/toolbox-node [port-http]}"
if [ ! -f "$BIN" ]; then
    echo "binaire introuvable : $BIN" >&2
    exit 1
fi
# Chemin absolu : le node est lancé depuis un dossier temporaire.
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
PORT="${2:-8123}"
BASE="http://127.0.0.1:$PORT"

DIR="$(mktemp -d)"
PID=""
cleanup() {
    if [ -n "$PID" ] && kill -0 "$PID" 2> /dev/null; then
        kill "$PID" 2> /dev/null || true
        wait "$PID" 2> /dev/null || true
    fi
    rm -rf "$DIR"
}
trap cleanup EXIT

cat > "$DIR/node.toml" << EOF
name = "smoke"

[ports]
bind = "127.0.0.1"
http = $PORT
osc = 9123
oscquery = 8124
EOF

# Sortie, MIDI et mDNS coupés : un runner CI n'a ni écran, ni périphérique,
# ni multicast utile — et cela vérifie au passage que fonctions.json prime.
cat > "$DIR/fonctions.json" << 'EOF'
{"player":true,"output":false,"osc":true,"oscquery":false,"osc_feedback":false,"midi":false,"fleet":false,"fader":true,"preview":true,"artnet":false}
EOF

(cd "$DIR" && exec "$BIN" node.toml) &
PID=$!

# Jusqu'à 30 s pour répondre (binaire tout juste compilé, runner chargé).
pret=false
for _ in $(seq 1 60); do
    if curl -sf "$BASE/api/health" > /dev/null 2>&1; then
        pret=true
        break
    fi
    if ! kill -0 "$PID" 2> /dev/null; then
        echo "ÉCHEC : le node s'est arrêté avant de répondre" >&2
        exit 1
    fi
    sleep 0.5
done
if [ "$pret" != true ]; then
    echo "ÉCHEC : pas de réponse HTTP après 30 s" >&2
    exit 1
fi

echo "== /api/health"
curl -sf "$BASE/api/health"
echo
echo "== /api/features"
curl -sf "$BASE/api/features"
echo

curl -sf "$BASE/api/state" | grep -q '"player"' \
    || { echo "ÉCHEC : /api/state sans bloc player" >&2; exit 1; }
curl -sf "$BASE/api/features" | grep -q '"output":false' \
    || { echo "ÉCHEC : fonctions.json ignoré (output devrait être coupé)" >&2; exit 1; }

kill "$PID"
wait "$PID" 2> /dev/null || true
PID=""
echo "smoke OK"
