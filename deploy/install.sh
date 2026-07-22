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
# Utilisateur qui fera tourner le node (défini si un service systemd est
# installé) — sert à lui donner la propriété du préfixe en fin d'install.
SERVICE_USER=""

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

# Échappe une valeur pour le REMPLACEMENT d'un sed dont le délimiteur est
# « | » : & (rappel du motif), | (délimiteur) et \ doivent être protégés,
# sinon un préfixe contenant l'un d'eux produirait une unité systemd cassée.
esc_sed() { printf '%s' "$1" | sed -e 's/[&|\\]/\\&/g'; }

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
    # `|| answer=""` : en exécution NON interactive (SSH sans tty,
    # provisioning), `read` renvoie EOF ; sans ça, `set -e` tuerait le script
    # à mi-installation, sans message. On retombe alors sur le défaut.
    read -r -p "$1 [$2] " answer || answer=""
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

# --- détection du matériel -------------------------------------------------------
# Reconnaît le modèle de Raspberry Pi (ou un PC) et adapte les conseils et
# les valeurs par défaut. Surchargable pour les tests : TOOLBOX_MODELE="...".
detecter_materiel() {
    local modele="${TOOLBOX_MODELE:-}"
    if [ -z "$modele" ] && [ -r /proc/device-tree/model ]; then
        modele="$(tr -d '\0' < /proc/device-tree/model 2> /dev/null || true)"
    fi
    case "$modele" in
        *"Raspberry Pi 5"*) echo pi5 ;;
        *"Raspberry Pi 4"* | *"Compute Module 4"*) echo pi4 ;;
        *"Raspberry Pi 3"* | *"Zero 2"*) echo pi3 ;;
        *"Raspberry Pi"*) echo pi_ancien ;;
        "") echo pc ;;
        *) echo autre ;;
    esac
}

MATERIEL="$(detecter_materiel)"
case "$MATERIEL" in
    pi5)
        say "Matériel détecté : Raspberry Pi 5"
        echo "  Conseillé   : profil complet, 1080p, rendu GPU, synchro, lumières."
        echo "  À savoir    : décodage HEVC matériel ; H.264 décodé par le CPU (ça"
        echo "                tient très bien) ; PAS d'encodage matériel → sortie"
        echo "                RTSP en 720p maximum." ;;
    pi4)
        say "Matériel détecté : Raspberry Pi 4"
        echo "  Conseillé   : profil complet, 1080p (décodage H.264 matériel)."
        echo "  Déconseillé : sortie RTSP au-delà de 720p (encodage au CPU)." ;;
    pi3)
        say "Matériel détecté : Raspberry Pi 3 / Zero 2 — VERSION ALLÉGÉE conseillée"
        echo "  Conseillé   : profil « lecteur » (lecture + mapping), rendu CPU en"
        echo "                960×540, aperçu web coupé (onglet Fonctions)."
        echo "  Déconseillé : rendu GPU (puce GLES 2.0 trop ancienne), sortie RTSP,"
        echo "                flux MJPEG au-delà de 480p, effets lourds." ;;
    pi_ancien)
        say "Matériel détecté : Raspberry Pi 1/2/Zero — NON RECOMMANDÉ"
        echo "  Ce modèle est trop léger pour Lanterne. Un Pi 4 est conseillé." ;;
    *)
        say "Matériel : PC / autre — profil complet conseillé." ;;
esac

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
    defaut=1
    if [ "$MATERIEL" = pi3 ]; then defaut=2; fi
    read -r -p "Choix [$defaut] " answer || answer=""
    case "${answer:-$defaut}" in
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
    read -r -p "  port web UI [8080] " answer || answer=""
    HTTP_PORT="${answer:-8080}"
fi
if [ "$MOD_OSC" = true ]; then
    read -r -p "  port OSC [9000] " answer || answer=""
    OSC_PORT="${answer:-9000}"
fi

NODE_NAME="$(hostname 2>/dev/null || echo toolbox-node)"
read -r -p "  nom du node [$NODE_NAME] " answer || answer=""
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

# Réglages de performance selon le matériel détecté (carte Système →
# Réglages de performance pour les changer ensuite).
if [ ! -f "$PREFIX/reglages.json" ]; then
    REGLAGES=""
    case "$MATERIEL" in
        pi3) REGLAGES='{"profil":"pi3","largeur":960,"hauteur":540,"gpu":false,"kms_fps":20}' ;;
        pi4) REGLAGES='{"profil":"pi4","largeur":1920,"hauteur":1080,"gpu":true,"kms_fps":30}' ;;
        pi5) REGLAGES='{"profil":"pi5","largeur":1920,"hauteur":1080,"gpu":true,"kms_fps":30}' ;;
    esac
    if [ -n "$REGLAGES" ]; then
        TMP_REG="$(mktemp)"
        printf '%s\n' "$REGLAGES" > "$TMP_REG"
        run cp "$TMP_REG" "$PREFIX/reglages.json"
        rm -f "$TMP_REG"
        say "Réglages de performance $MATERIEL écrits (modifiables dans l'UI)."
    fi
fi

# --- systemd (mode kiosque P1.9) ------------------------------------------------
if command -v systemctl > /dev/null 2>&1; then
    if ask_yn "Installer le service systemd (démarrage auto + redémarrage en cas de crash) ?" o; then
        RUN_USER="${SUDO_USER:-$(id -un)}"
        read -r -p "  utilisateur du service [$RUN_USER] " answer || answer=""
        RUN_USER="${answer:-$RUN_USER}"
        SERVICE_USER="$RUN_USER"
        UNIT_SRC="$(dirname "$0")/systemd/toolbox-node.service"
        if [ ! -f "$UNIT_SRC" ]; then
            fail "fichier unité introuvable : $UNIT_SRC"
        fi
        sed -e "s|@PREFIX@|$(esc_sed "$PREFIX")|g" -e "s|@USER@|$(esc_sed "$RUN_USER")|g" "$UNIT_SRC" \
            | sudo tee /etc/systemd/system/toolbox-node.service > /dev/null
        sudo systemctl daemon-reload
        sudo systemctl enable toolbox-node.service
        say "Service installé. Démarrage : sudo systemctl start toolbox-node"
        say "Logs : journalctl -u toolbox-node -f  (ou la page Logs de la web UI)"
    fi
fi

# --- propriété du préfixe -------------------------------------------------------
# Créé en root (sudo), le préfixe ne serait PAS inscriptible par le node qui
# tourne en simple utilisateur : plus aucun preset, réglage, LUT, log ni
# bascule de l'UI ne pourrait être enregistré. On en donne la propriété à
# l'utilisateur qui fera tourner le node (service systemd, ou à défaut celui
# qui a lancé sudo). Root pur (pas de sudo) : rien à faire, root écrit partout.
if [ "$NEED_SUDO" = true ]; then
    OWNER="${SERVICE_USER:-${SUDO_USER:-}}"
    if [ -n "$OWNER" ] && [ "$OWNER" != root ]; then
        run chown -R "$OWNER" "$PREFIX"
        say "Propriété de $PREFIX donnée à « $OWNER » (écriture presets/réglages/logs)."
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
