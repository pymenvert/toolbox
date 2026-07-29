#!/bin/sh
# Harnais d'endurance de Lanterne (Linux / Raspberry Pi).
#
# Meme role, MEME charge et MEME format de CSV que endurance.ps1 : ce script
# ne fait que collecter. Le depouillement se fait avec
#   pwsh tools/endurance/endurance.ps1 -Analyser endurance.csv
# depuis n'importe quelle machine — c'est fait expres, pour comparer un run
# de Pi et un run de PC avec exactement la meme grille de lecture.
#
# Usage :
#   ./endurance.sh                       # 60 min, charge par defaut
#   MINUTES=240 ./endurance.sh           # 4 h
#   CHARGE=0 ./endurance.sh              # observation passive
#   CADENCE=20 ./endurance.sh            # 20 commandes/s au lieu de 10
#
# Depend de curl seul (present sur Raspberry Pi OS).

set -eu

URL="${URL:-http://127.0.0.1:8080}"
MINUTES="${MINUTES:-60}"
INTERVALLE="${INTERVALLE:-15}"
CSV="${CSV:-endurance.csv}"
CHARGE="${CHARGE:-1}"
CADENCE="${CADENCE:-10}"

if ! command -v curl >/dev/null 2>&1; then
  echo "curl est requis." >&2
  exit 1
fi

# Horloge MONOTONE. `date +%s` est l'heure murale : sur un Pi sans pile RTC,
# la synchro NTP au demarrage peut la faire bondir de plusieurs heures — le
# run se terminait alors instantanement, ou la colonne « secondes » sautait,
# et c'est cette colonne qui sert de base de temps a la pente memoire.
maintenant() {
  if [ -r /proc/uptime ]; then
    read -r up _ </proc/uptime
    printf '%s' "${up%%.*}"
  else
    date +%s
  fi
}

# Envoi d'une commande. Le code HTTP est LU : la version precedente avalait
# les refus avec `|| true`, et une mire inexistante partait en 422 sans que
# rien ne le signale — un sixieme de la charge annoncee n'existait pas.
refus=0
cmd() {
  code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' \
    -X POST -H 'Content-Type: application/json' \
    -d "$1" "$URL/api/command" 2>/dev/null || printf '000')
  case "$code" in
    2*) : ;;
    *)
      refus=$((refus + 1))
      if [ "$refus" -le 3 ] || [ $((refus % 100)) -eq 0 ]; then
        echo "  commande refusee ($refus, HTTP $code) : $1" >&2
      fi
      ;;
  esac
}

# Extraction sans jq (absent d'une install Pi minimale) : on lit une cle
# numerique dans le JSON plat de /api/system. Renvoie une chaine vide si la
# cle manque — une plateforme sans la mesure ne doit pas casser le run.
champ() {
  printf '%s' "$1" | sed -n "s/.*\"$2\":\([0-9.]*\).*/\1/p" | head -n 1
}

# Etat du node restaure a la fin, quoi qu'il arrive. Sans ca, la charge
# laissait la MIRE DE TEST allumee — et la mire est prioritaire sur la video
# (crates/engine/src/raster.rs) : le spectacle suivant projetait un damier.
sauvegarde=0
boucle_pid=""
mjpeg_pid=""

nettoyer() {
  [ -n "$boucle_pid" ] && kill "$boucle_pid" 2>/dev/null || true
  [ -n "$mjpeg_pid" ] && kill "$mjpeg_pid" 2>/dev/null || true
  if [ "$CHARGE" = "1" ]; then
    cmd '{"cmd":"set_test_pattern","pattern":null}'
    if [ "$sauvegarde" = "1" ]; then
      cmd '{"cmd":"mapping_load","name":"__endurance_avant"}'
    fi
  fi
}
trap nettoyer EXIT INT TERM

# Une commande par tour, au rythme demande — comme une console OSC qui
# pilote le spectacle en continu. La version precedente n'envoyait que
# 6 commandes par INTERVALLE (0,4/s a 15 s) et n'avait aucun client MJPEG :
# elle sollicitait le node 25 fois moins que son homologue Windows, et les
# deux runs n'etaient donc pas comparables.
charge_continue() {
  pause_ms=$((1000 / CADENCE))
  [ "$pause_ms" -lt 1 ] && pause_ms=1
  pause="$((pause_ms / 1000)).$(printf '%03d' $((pause_ms % 1000)))"
  i=0
  while :; do
    case $((i % 6)) in
      0) cmd '{"cmd":"corner_set","index":0,"x":0.01,"y":0.01}' ;;
      1) cmd '{"cmd":"corner_set","index":0,"x":0.0,"y":0.0}' ;;
      2) cmd '{"cmd":"color_set","param":"gamma","value":1.1}' ;;
      3) cmd '{"cmd":"color_set","param":"gamma","value":1.0}' ;;
      4) cmd '{"cmd":"set_test_pattern","pattern":"grid"}' ;;
      # « bars » n'existe pas dans TestPattern (grid/checker/corners) :
      # cette commande partait en 422 a chaque tour.
      *) cmd '{"cmd":"set_test_pattern","pattern":"checker"}' ;;
    esac
    i=$((i + 1))
    sleep "$pause"
  done
}

echo "secondes;rss_mb;p50_us;p95_us;max_us;sautees;fps;erreurs;uptime_s" >"$CSV"

debut=$(maintenant)
fin=$((debut + MINUTES * 60))
points=0
echecs=0
echo "Endurance : $MINUTES min sur $URL, un point toutes les $INTERVALLE s."
echo "CSV : $CSV"

if [ "$CHARGE" = "1" ]; then
  # Le mapping de l'operateur est ecrase par la charge (coin 0) : on le
  # sauve pour le rendre a la fin.
  cmd '{"cmd":"mapping_save","name":"__endurance_avant"}'
  sauvegarde=1
  charge_continue &
  boucle_pid=$!
  curl -s -o /dev/null "$URL/flux.mjpg?fps=15" &
  mjpeg_pid=$!
  echo "Charge : ~$CADENCE commandes/s + client MJPEG."
fi

while [ "$(maintenant)" -lt "$fin" ]; do
  json=$(curl -s -m 15 "$URL/api/system" 2>/dev/null || true)
  if [ -n "$json" ]; then
    secondes=$(( $(maintenant) - debut ))
    # Le bloc "rendu" est un objet imbrique : on l'isole avant d'y lire les
    # cles, sinon "max_us" pourrait etre pioche ailleurs dans le JSON.
    rendu=$(printf '%s' "$json" | sed -n 's/.*"rendu":{\([^}]*\)}.*/\1/p')
    printf '%s;%s;%s;%s;%s;%s;%s;%s;%s\n' \
      "$secondes" \
      "$(champ "$json" rss_mb)" \
      "$(champ "$rendu" p50_us)" \
      "$(champ "$rendu" p95_us)" \
      "$(champ "$rendu" max_us)" \
      "$(champ "$rendu" sautees)" \
      "$(champ "$json" fps)" \
      "$(champ "$json" erreurs_recentes)" \
      "$(champ "$json" uptime_s)" >>"$CSV"
    points=$((points + 1))
    if [ $((points % 20)) -eq 0 ]; then
      echo "  $points points | RSS $(champ "$json" rss_mb) Mo | p95 $(champ "$rendu" p95_us) us"
    fi
  else
    echecs=$((echecs + 1))
    echo "  node injoignable ($echecs)"
  fi
  sleep "$INTERVALLE"
done

echo "Collecte terminee : $points points, $echecs echec(s) de lecture, $refus refus."
echo "Depouiller avec : pwsh tools/endurance/endurance.ps1 -Analyser $CSV"
