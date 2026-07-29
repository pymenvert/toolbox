#!/bin/sh
# Harnais d'endurance de Lanterne (Linux / Raspberry Pi).
#
# Meme role et MEME format de CSV que endurance.ps1 : ce script ne fait que
# collecter. Le depouillement se fait avec
#   pwsh tools/endurance/endurance.ps1 -Analyser endurance.csv
# depuis n'importe quelle machine — c'est fait expres, pour comparer un run
# de Pi et un run de PC avec exactement la meme grille de lecture.
#
# Usage :
#   ./endurance.sh                       # 60 min, charge par defaut
#   MINUTES=240 ./endurance.sh           # 4 h
#   CHARGE=0 ./endurance.sh              # observation passive
#
# Depend de curl seul (present sur Raspberry Pi OS).

set -eu

URL="${URL:-http://127.0.0.1:8080}"
MINUTES="${MINUTES:-60}"
INTERVALLE="${INTERVALLE:-15}"
CSV="${CSV:-endurance.csv}"
CHARGE="${CHARGE:-1}"

if ! command -v curl >/dev/null 2>&1; then
  echo "curl est requis." >&2
  exit 1
fi

# Extraction sans jq (absent d'une install Pi minimale) : on lit une cle
# numerique dans le JSON plat de /api/system. Renvoie une chaine vide si la
# cle manque — une plateforme sans la mesure ne doit pas casser le run.
champ() {
  printf '%s' "$1" | sed -n "s/.*\"$2\":\([0-9.]*\).*/\1/p" | head -n 1
}

charge() {
  for c in \
    '{"cmd":"corner_set","index":0,"x":0.01,"y":0.01}' \
    '{"cmd":"corner_set","index":0,"x":0.0,"y":0.0}' \
    '{"cmd":"color_set","param":"gamma","value":1.1}' \
    '{"cmd":"color_set","param":"gamma","value":1.0}' \
    '{"cmd":"set_test_pattern","pattern":"grid"}' \
    '{"cmd":"set_test_pattern","pattern":"bars"}'
  do
    curl -s -m 10 -X POST -H 'Content-Type: application/json' \
      -d "$c" "$URL/api/command" >/dev/null 2>&1 || true
  done
}

echo "secondes;rss_mb;p50_us;p95_us;max_us;sautees;fps;erreurs;uptime_s" >"$CSV"

debut=$(date +%s)
fin=$((debut + MINUTES * 60))
points=0
echecs=0
echo "Endurance : $MINUTES min sur $URL, un point toutes les $INTERVALLE s."
echo "CSV : $CSV"

while [ "$(date +%s)" -lt "$fin" ]; do
  json=$(curl -s -m 15 "$URL/api/system" 2>/dev/null || true)
  if [ -n "$json" ]; then
    secondes=$(( $(date +%s) - debut ))
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
      echo "  $points points · RSS $(champ "$json" rss_mb) Mo · p95 $(champ "$rendu" p95_us) us"
    fi
  else
    echecs=$((echecs + 1))
    echo "  node injoignable ($echecs)"
  fi
  # `if` explicite, PAS `[ ... ] && charge` : sous `set -e`, une liste ET
  # dont la condition est fausse renvoie 1, et le comportement qui s'ensuit
  # depend du shell. Sur le Pi, /bin/sh est dash, pas bash -- CHARGE=0
  # risquait d'arreter la collecte au premier point.
  if [ "$CHARGE" = "1" ]; then
    charge
  fi
  sleep "$INTERVALLE"
done

echo "Collecte terminee : $points points, $echecs echec(s) de lecture."
echo "Depouiller avec : pwsh tools/endurance/endurance.ps1 -Analyser $CSV"
