#!/usr/bin/env bash
# Installeur interactif du node Toolbox (Linux / Raspberry Pi).
#
# - choix d'un PROFIL (complet / lecteur+mapping / synchro / lumières /
#   minimal / personnalisé) → écrit node.toml ET fonctions.json : les
#   fonctions inutiles sont réellement coupées (0 ressource consommée) ;
# - installe le binaire + les dossiers dans un préfixe (défaut /opt/toolbox) ;
# - optionnel : service systemd avec redémarrage automatique (mode kiosque).
#
# Usage :
#   ./install.sh [--prefix /opt/toolbox] [--binary ./toolbox-node]
#                [--profil complet|lecteur|synchro|lumieres|minimal]
# Le binaire vient soit du dossier courant, soit d'un build local
# (cargo build --release), soit d'un artefact CI GitHub décompressé.

set -euo pipefail

PREFIX="/opt/toolbox"
BINARY=""
PROFIL=""
NEED_SUDO=false

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="$2"; shift 2 ;;
        --binary) BINARY="$2"; shift 2 ;;
        --profil) PROFIL="$2"; shift 2 ;;
        -h|--help)
            grep '^#' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "option inconnue : $1 (voir --help)"; exit 1 ;;
    esac
done

say()  { printf '\033[1;36m>>\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m!!\033[0m %s\n' "$*" >&2; exit 1; }

# Exécute avec sudo seulement si nécessaire.
run() {
    if [ "$NEED_SUDO" = true ]; then
        sudo "$@"
    else
        "$@"
    fi
}

ask_yn() { # ask_yn "question" "défaut o/n"
    local answer
    read -r -p "$1 [$2] " answer
    answer="${answer:-$2}"
    case "$answer" in o|O|y|Y|oui|yes) return 0 ;; *) return 1 ;; esac
}

# --- binaire -----------------------------------------------------------------
if [ -z "$BINARY" ]; then
    for candidate in ./toolbox-node ./target/release/toolbox-node ../target/release/toolbox-node; do
        if [ -x "$candidate" ]; then
            BINARY="$candidate"
            break
        fi
    done
fi
if [ -z "$BINARY" ] || [ ! -f "$BINARY" ]; then
    fail "binaire toolbox-node introuvable — passez --binary /chemin/vers/toolbox-node
(build : cargo build --release -p toolbox-node ; ou artefact CI GitHub Actions)"
fi

say "Binaire : $BINARY"
say "Préfixe d'installation : $PREFIX"

# --- profil d'installation ------------------------------------------------------
# Chaque profil écrit node.toml (modules) + fonctions.json (interrupteurs de
# l'onglet Fonctions) : tout reste modifiable plus tard depuis l'UI.
if [ -z "$PROFIL" ]; then
    say "Profils d'installation :"
    echo "  1. complet        — tout : lecteur, mapping, OSC, MIDI, parc, lumières"
    echo "  2. lecteur        — lecteur + mapping seuls, pas de réseau ni lumières"
    echo "  3. synchro        — lecteur + mapping + parc réseau + synchro multi-machines"
    echo "  4. lumieres       — console Art-Net + OSC/MIDI, pas de vidéo"
    echo "  5. minimal        — lecteur seul (le plus léger)"
    echo "  6. personnalise   — questions module par module"
    read -r -p "Choix [1] " answer
    case "${answer:-1}" in
        1) PROFIL=complet ;;
        2) PROFIL=lecteur ;;
        3) PROFIL=synchro ;;
        4) PROFIL=lumieres ;;
        5) PROFIL=minimal ;;
        6) PROFIL=personnalise ;;
        *) fail "choix invalide : $answer" ;;
    esac
fi
say "Profil : $PROFIL"

MOD_PLAYER=true; MOD_HTTP=true; MOD_OSC=true; MOD_MIDI=false
FONCTIONS=""
case "$PROFIL" in
    complet)
        MOD_MIDI=true
        FONCTIONS='{"player":true,"output":true,"osc":true,"oscquery":true,"osc_feedback":true,"midi":true,"fleet":true,"fader":true,"preview":true,"artnet":true}' ;;
    lecteur)
        MOD_OSC=false
        FONCTIONS='{"player":true,"output":true,"osc":false,"oscquery":false,"osc_feedback":false,"midi":false,"fleet":false,"fader":true,"preview":true,"artnet":false}' ;;
    synchro)
        FONCTIONS='{"player":true,"output":true,"osc":true,"oscquery":false,"osc_feedback":false,"midi":false,"fleet":true,"fader":true,"preview":true,"artnet":false}' ;;
    lumieres)
        MOD_PLAYER=false; MOD_MIDI=true
        FONCTIONS='{"player":false,"output":false,"osc":true,"oscquery":true,"osc_feedback":true,"midi":true,"fleet":false,"fader":false,"preview":false,"artnet":true}' ;;
    minimal)
        MOD_OSC=false
        FONCTIONS='{"player":true,"output":true,"osc":false,"oscquery":false,"osc_feedback":false,"midi":false,"fleet":false,"fader":false,"preview":false,"artnet":false}' ;;
    personnalise)
        say "Choix des modules (tout reste réactivable plus tard dans node.toml)"
        if ! ask_yn "  player vidéo ?" o; then MOD_PLAYER=false; fi
        if ! ask_yn "  web UI + API HTTP ?" o; then MOD_HTTP=false; fi
        if ! ask_yn "  contrôle OSC ?" o; then MOD_OSC=false; fi
        if ask_yn "  contrôle MIDI ?" n; then MOD_MIDI=true; fi ;;
    *) fail "profil inconnu : $PROFIL (voir --help)" ;;
esac

HTTP_PORT=8080
OSC_PORT=9000
answer=""
if [ "$MOD_HTTP" = true ]; then
    read -r -p "  port web UI [8080] " answer
    HTTP_PORT="${answer:-8080}"
fi
if [ "$MOD_OSC" = true ]; then
    read -r -p "  port OSC [9000] " answer
    OSC_PORT="${answer:-9000}"
fi

NODE_NAME="$(hostname 2>/dev/null || echo toolbox-node)"
read -r -p "  nom du node [$NODE_NAME] " answer
NODE_NAME="${answer:-$NODE_NAME}"

# --- installation ---------------------------------------------------------------
PARENT_DIR="$(dirname "$PREFIX")"
if { [ -d "$PREFIX" ] && [ ! -w "$PREFIX" ]; } || { [ ! -d "$PREFIX" ] && [ ! -w "$PARENT_DIR" ]; }; then
    NEED_SUDO=true
    say "Le préfixe demande les droits administrateur (sudo)."
fi

run mkdir -p "$PREFIX" "$PREFIX/media" "$PREFIX/presets" "$PREFIX/logs" "$PREFIX/shaders"
run install -m 755 "$BINARY" "$PREFIX/toolbox-node"

TMP_CONF="$(mktemp)"
cat > "$TMP_CONF" << EOF
# Configuration du node Toolbox — générée par install.sh
# Documentation complète : node.toml.example dans le dépôt.
name = "$NODE_NAME"

[modules]
player = $MOD_PLAYER
http = $MOD_HTTP
osc = $MOD_OSC
midi = $MOD_MIDI

[ports]
bind = "0.0.0.0"
http = $HTTP_PORT
osc = $OSC_PORT
EOF
if [ -f "$PREFIX/node.toml" ]; then
    say "node.toml existant conservé — nouvelle config écrite dans node.toml.new"
    run cp "$TMP_CONF" "$PREFIX/node.toml.new"
else
    run cp "$TMP_CONF" "$PREFIX/node.toml"
fi
rm -f "$TMP_CONF"

# Interrupteurs de fonctions du profil (un fonctions.json existant prime :
# ce sont les bascules faites dans l'UI).
if [ -n "$FONCTIONS" ] && [ ! -f "$PREFIX/fonctions.json" ]; then
    TMP_FONC="$(mktemp)"
    printf '%s\n' "$FONCTIONS" > "$TMP_FONC"
    run cp "$TMP_FONC" "$PREFIX/fonctions.json"
    rm -f "$TMP_FONC"
fi

# --- systemd (mode kiosque P1.9) ------------------------------------------------
if command -v systemctl > /dev/null 2>&1; then
    if ask_yn "Installer le service systemd (démarrage auto + redémarrage en cas de crash) ?" o; then
        RUN_USER="${SUDO_USER:-$(id -un)}"
        read -r -p "  utilisateur du service [$RUN_USER] " answer
        RUN_USER="${answer:-$RUN_USER}"
        UNIT_SRC="$(dirname "$0")/systemd/toolbox-node.service"
        if [ ! -f "$UNIT_SRC" ]; then
            fail "fichier unité introuvable : $UNIT_SRC"
        fi
        sed -e "s|@PREFIX@|$PREFIX|g" -e "s|@USER@|$RUN_USER|g" "$UNIT_SRC" \
            | sudo tee /etc/systemd/system/toolbox-node.service > /dev/null
        sudo systemctl daemon-reload
        sudo systemctl enable toolbox-node.service
        say "Service installé. Démarrage : sudo systemctl start toolbox-node"
        say "Logs : journalctl -u toolbox-node -f  (ou la page Logs de la web UI)"
    fi
fi

say "Installation terminée dans $PREFIX"
if [ "$MOD_HTTP" = true ]; then
    IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
    say "Web UI : http://${IP:-<ip-du-node>}:$HTTP_PORT/"
fi
say "Lancement manuel : cd $PREFIX && ./toolbox-node"
if [ "$PROFIL" = "synchro" ]; then
    say "Synchro : ajouter [sync] role = \"maitre\" (ou \"suiveur\" + maitre = \"ip:9010\") dans node.toml"
fi
