# Changelog

Évolutions notables du node Toolbox. Format inspiré de
[Keep a Changelog](https://keepachangelog.com/fr/), versionnage SemVer.

## [Non publié]

- **Brique DRM/KMS** : `[output] mode = "kms"` — plein écran SANS bureau
  (Raspberry Pi OS Lite, console) via kmssink, frames composées par la
  référence CPU (compositeur partagé avec la sortie RTSP, tampons
  réutilisés). Suit l'interrupteur « fenêtre de sortie » (coupé = noir).
  Construite et testée en CI ; le run réel attend un Pi.
- **Perf** : plus de clone d'état ni d'allocations par frame sur les
  chemins chauds (fenêtre, flux MJPEG, sorties GStreamer).
- **Sortie RTSP** : `rtsp://node:8554/sortie` (section `[rtsp]`, binaires
  avec la feature `gstreamer`) — la sortie composée en H.264 (MJPEG en
  repli), pipeline PARTAGÉ entre les clients (dix spectateurs = un seul
  rendu + un seul encodage). Testée par la CI avec un vrai DESCRIBE RTSP.
  La sortie NDI reste bloquée par le SDK propriétaire NewTek (licence,
  non redistribuable en CI) — documenté, pas entamé.
- **Flux MJPEG de la sortie** : `http://node:8080/flux.mjpg` — la sortie
  composée (mapping, couleur, LUT, blackout) en continu dans VLC, OBS ou
  un navigateur, sans rien installer. `?w=1280&fps=25` optionnels, coupé
  avec la fonction Aperçu. Pour du multi-spectateur lourd : la sortie
  RTSP (ci-dessous).

## [3.0.0] — 2026-07-11

L'application prend son nom : **Lanterne** (les binaires restent
`toolbox-node`). Image avancée (LUT, mesh warp), boutons de régie, slots
intelligents, installation par profils, télémétrie opt-in.

- **LUT 3D .cube** : étalonnage complet en un fichier (dossier `luts/`,
  dépôt et sélection depuis l'onglet Couleur — les fichiers invalides sont
  refusés avec la raison). Interpolation trilinéaire, MÊME formule CPU et
  GPU (buffer storage, pas de texture filtrée dépendante du matériel),
  appliquée après la correction couleur. Sauvée dans les presets,
  OSC `/lut nom.cube`.
- **Mesh warp** : grille de points de contrôle (2×2 à 9×9) par-dessus le
  mapping 4 coins pour épouser les surfaces irrégulières — éditeur à la
  souris dans l'onglet Mapping, déplacements bornés ±0,25, interpolation
  bilinéaire, parité stricte CPU/GPU. Les vecteurs de référence de
  l'homographie restent intouchés (le mesh est un décalage APRÈS le
  mapping). Sauvé dans les presets de mapping.
- **Boutons de régie** : **BLACKOUT** (voile noir avec rampe animée —
  300 ms par défaut, réglable — l'état complet continue en dessous et
  revient intact) et **FREEZE** (gel de la source vidéo sur la dernière
  frame, transport vivant en dessous). Gros boutons sur le Dashboard,
  OSC `/blackout 1 [fondu_ms]` et `/freeze 1`, exposés en OSCQuery,
  parité stricte CPU/GPU, aperçu web assombri aussi.
- **Installation par profils** : `deploy/installer-windows.ps1` (nouveau)
  et `deploy/install.sh` (refondu) proposent Complet / Lecteur+Mapping /
  Synchro / Lumières / Minimal — chaque profil écrit `node.toml` +
  `fonctions.json`, les fonctions inutiles sont réellement coupées.
  L'installateur Windows sait télécharger la dernière release tout seul.
- **Smoke test CI** : les jobs Linux et Windows lancent désormais le VRAI
  binaire (`deploy/smoke.sh`) et vérifient l'API vivante (health, state,
  features) avant de publier quoi que ce soit.
- **Télémétrie opt-in** : chaque panic est consigné dans `logs/crash.txt`
  (diagnostic local). Si — et seulement si — `[telemetrie] url` est
  configurée, le rapport est envoyé au démarrage suivant puis supprimé.
- **Lanceur Chataigne** : carte Système → Chataigne (détection, lancement,
  lien de téléchargement officiel) + API `/api/chataigne`.
- **Manuel** : sections Tailscale (contrôle à distance), profils
  d'installation, télémétrie ; table node.toml complétée
  (`[sync]`, `[security]`, `[telemetrie]`).
- **Slots intelligents** : les cues gagnent les **jours de la semaine**
  (« ven sam à 20:00 »), les actions **lumières** (rappel de scène DMX,
  chaser start/stop) et le déclenchement **OSC** (`/cue/go nom`) et
  **MIDI** (binding `{cmd = "cue_go", name = "…"}`). Les scènes et
  chasers lumières sont aussi pilotables directement en OSC/MIDI
  (`/dmx/scene`, `/dmx/chaser`) — tout le vocabulaire passe par le bus.
## [2.0.0] — 2026-07-11

La V2 : synchronisation à la frame, console lumières Art-Net, séquenceur,
fichiers du parc, interrupteurs de fonctions, edge blending et masques,
santé système, OTA. (LUT .cube et mesh warp sont arrivés en 3.0.0.)

- **Passthrough + état de démarrage** : bouton « Faire de l'état actuel
  l'état de démarrage » (onglet Presets) — mapping, couleur, effets,
  source et lecture retrouvés à CHAQUE lancement (`demarrage.json`, prime
  sur node.toml). Une source live configurée (`capture://0`, `ndi://…`)
  est chargée et jouée automatiquement, et **reprise toute seule après un
  débranchement** (le player réessaie toutes les 3 s, sans jamais
  abandonner une source branchable). `[startup] source` en config aussi.
- **Rendu CPU ×8** : parallélisation par lignes — 1080p chargé passe de
  215 à 25 ms/frame ; l'aperçu web et le repli sans GPU en profitent
  (banc de mesure reproductible `bench_raster`).

- **Edge blending + masques** (onglet Mapping) : bandes de fondu vers le
  noir sur chaque bord de la sortie (largeur par bord + gamma projecteur,
  OSC `/blending g d h b gamma`) pour recouvrir plusieurs projecteurs sans
  sur-brillance ; et jusqu'à 8 masques noirs (quadrilatères en espace de
  sortie) pour cacher fenêtres et reliefs. Même formule au pixel près dans
  la référence CPU et le shader GPU (validé naga) ; le fondu de preset
  fait aussi glisser le blending. Vérifié au pixel sur l'aperçu réel.
- **Mise à jour OTA (expérimental)** : page Système → « Mise à jour ».
  Trois temps prudents : vérifier la dernière release GitHub, télécharger
  le binaire de la plateforme À CÔTÉ (garde-fous : taille, format ;
  rien n'est remplacé), puis appliquer — bascule avec conservation de
  l'ancien binaire (`.precedent`) et redémarrage par le service
  (systemd `Restart=always` / démarrage auto Windows). Un échec à
  n'importe quelle étape laisse l'installation intacte. À valider en
  conditions réelles lors de la prochaine release.
- **Santé du système d'un coup d'œil** (page Système) : pastille par
  fonction (état des interrupteurs), tuile « Erreurs récentes » (compteur
  ERROR du journal, en rouge s'il y en a) et tuile « Dérive de synchro »
  pour les nodes suiveurs (verte sous 40 ms = verrouillé sous la frame).
- **Page Séquences — séquenceur de cues** : chaque cue = un nom + une
  liste d'actions (charger un média, lecture, fondu vers un preset,
  mire… — tout le vocabulaire du node). Déclenchement : GO manuel,
  enchaînement « N secondes après la précédente », ou **tous les jours à
  HH:MM** (heure de la machine, garde une-fois-par-jour). Réordonnable,
  Stop annule l'enchaînement en attente, persistance `sequences.json`.
  API `GET/POST /api/cues`. Vérifié en réel : GO, enchaînement à 1,5 s et
  déclenchement horaire constatés sur l'état du node.
- **Page Lumières — console DMX Art-Net** : des faders nommés créés à la
  volée (univers + canal, couleur d'étiquette), un master, des **scènes**
  (instantanés rappelables) et des **chasers** (enchaînements de scènes
  avec fondu et tenue par pas, boucle ou one-shot), à la manière des
  consoles (Chataigne/QLC+). Émission ArtDMX continue à 30 trames/s vers
  l'IP configurée (broadcast par défaut), trame complète par univers.
  Persistance `lumieres.json` ; interrupteur dans Fonctions (coupé : plus
  aucune trame, socket fermée, l'édition reste possible). API
  `GET/POST /api/dmx`. Vérifié en réel : trames décodées conformes, fondu
  de chaser mesuré rampe par rampe.
- **Fichiers du parc** : depuis l'onglet Médias, voir les médias de chaque
  node du réseau, envoyer un fichier à un node précis ou **à tout le parc
  d'un coup** — l'envoi passe de node à node en flux direct (jamais le
  fichier entier en RAM), avec rapport par machine. Le node ne relaie que
  vers des machines découvertes en mDNS (pas de proxy ouvert) et transmet
  le mot de passe du parc s'il y en a un.
- **Synchronisation multi-node niveau 2 — verrouillage à la frame** :
  `[sync] role = "maitre"|"suiveur"` dans node.toml. Les suiveurs
  s'annoncent d'eux-mêmes au maître (rien à configurer côté maître), qui
  leur publie son horloge de lecture à 5 Hz. Le suiveur suit le média et
  le transport du maître, lisse la dérive (médiane, robuste aux paquets
  retardés) et corrige : micro-ajustement de vitesse ±3 % (invisible)
  jusqu'au seuil, resync dur au-delà (`tolerance_ms`, 80 par défaut).
  Mesuré en réel avec deux nodes : 2,3 s de décalage résorbées, dérive
  stabilisée sous 2 ms — bien en dessous d'une frame. Horloges des
  machines à synchroniser en NTP ; GStreamer applique la vitesse sans
  coupure (instant rate change, à valider sur matériel).
- **Vitesse de lecture** : commande `set_rate` (0.25×..4×), OSC `/rate` —
  utilisée par la synchro, disponible partout.
- **Fenêtre de sortie dormante** : l'interrupteur « Fenêtre de sortie »
  agit maintenant à chaud — coupée, la fenêtre est masquée, le rendu
  suspendu et la surface (GPU comprise) libérée : 0 % CPU/GPU ; réveillée,
  tout est recréé immédiatement, sans relancer le node.
- **Onglet « Fonctions »** (début de la V2) : un interrupteur par fonction
  du node — lecteur vidéo, OSC, OSCQuery, retour d'état, MIDI, parc mDNS,
  fondus, aperçu. Désactivée = la fonction est **réellement arrêtée** à
  chaud (socket fermée, port MIDI relâché, pipeline vidéo libéré, annonce
  réseau retirée) : zéro ressource consommée. Réactivée = redémarrage
  immédiat, sans relancer le node. Choix mémorisés dans `fonctions.json`
  (priment sur `[modules]` au démarrage). API `GET/POST /api/features`.
  La fenêtre de sortie s'applique au prochain démarrage (mise en sommeil
  à chaud dans un prochain lot) ; l'UI web reste non désactivable depuis
  elle-même.
- **Téléphone : la barre d'onglets réapparaît**. Sur écran étroit, elle
  était écrasée à la hauteur de sa barre de défilement (onglets rognés,
  intapables) — revue mobile complète en 375 px au passage : aucune autre
  page ne déborde, les logs et le mapping défilent dans leurs conteneurs.
- **Aperçu partagé** : les requêtes `/api/preview.png` concurrentes
  partagent un seul rendu CPU (cache 250 ms) — plusieurs dashboards
  ouverts ne surchargent plus un Pi (mesuré : 724 ms à froid → 2 ms en
  cache sur un rendu 1920).

## [1.1.0] — 2026-07-10

Améliorations continues de l'après-midi/soirée : scènes en fondu,
exploitation renforcée, intégration Chataigne complète.

- **Arrêt propre sur SIGTERM** (Linux/Pi) : `systemctl stop` déclenche le
  même arrêt propre que Ctrl-C (services signalés, attente bornée) au lieu
  de laisser systemd tuer le node au timeout.
- **Fondu du mapping seul** : bouton « Fondu » dans la carte Mappings
  enregistrés (onglet Mapping), commande `mapping_fade`, OSC
  `/mapping/fade nom secondes` — coins et recadrage glissent vers le
  calage cible en 2 s, sans toucher couleur, effets, volume ni lecture.
- **Annonce OSCQuery en mDNS** (`_oscjson._tcp`) : Chataigne découvre le
  node dans son module OSCQuery sans qu'on tape la moindre IP (l'annonce
  suit le module OSC ; vérifiée avec un scanner mDNS local).
- **Journal sur disque** : en plus de la page de logs (mémoire), le node
  écrit un fichier par jour dans `paths.logs`
  (`toolbox.log.AAAA-MM-JJ`, 14 jours gardés, purge au démarrage) —
  lisible APRÈS un crash ou une coupure de courant. Écriture non
  bloquante : une carte SD lente ne fige jamais le node.
- **Supervision des services** : un service du node (player, HTTP, OSC,
  fader…) qui panique ou se termine avant l'arrêt demandé est tracé en
  ERROR (visible dans les logs et le diagnostic) au lieu de disparaître en
  silence ; le node continue avec ses autres services.
- **Export diagnostic ZIP** (brief 7.2) : bouton « Exporter le diagnostic »
  (Système) et `GET /api/diagnostic.zip` — état complet, journal, infos
  système, écrans, médias, presets, parc mDNS dans une archive à joindre
  quand on demande de l'aide. Aucun secret dedans (ni node.toml, ni mot de
  passe). Écrivain ZIP maison sans dépendance (entrées « stored », testé).
- **Fondu entre presets** (brief 7.4) : bouton « Fondu » sur chaque preset
  (durée réglable), commande `preset_fade`, OSC `/preset/fade nom secondes`.
  Coins, recadrage, couleur, effets et volume glissent en douceur (~30 pas/s,
  adoucis) ; rotation, miroirs, mire et bypass basculent à la fin ; le média
  et la lecture en cours ne sont jamais touchés. Un nouveau fondu repart de
  l'état courant.
- **Retour d'état OSC** (`[osc] feedback = "hôte:port"`) : chaque changement
  du node (transport, volume, coins, couleur, effets, presets…) est renvoyé
  en OSC à l'adresse configurée, avec la même grammaire que les commandes
  (`/volume`, `/corner/2`, `/pattern`…). Les curseurs de Chataigne suivent
  le node quelle que soit l'interface qui a fait le changement.
- **Mot de passe optionnel de l'UI/API** (`[security] password`) : HTTP
  Basic, tout identifiant + ce mot de passe ; absent = ouvert comme avant.
  L'OSC (UDP) reste ouvert — réseau local ou Tailscale conseillés.
- La liste des Médias se rafraîchit toute seule (un fichier copié dans
  `media/` apparaît sans recharger la page).
- **Aperçu de la sortie dans le Dashboard** : ce que projette le node
  (warp, mires, vidéo, effets), en PNG basse résolution rafraîchi toutes
  les 1,5 s — contrôlable depuis un téléphone sans voir le projecteur
  (`GET /api/preview.png?w=480`). La référence CPU du rendu vit désormais
  dans l'engine (`toolbox_engine::raster`).

## [1.0.0] — 2026-07-10

Première version complète du brief : lecture vidéo réelle, mapping GPU,
calibrage projecteur de bout en bout, packs autonomes sur la page Releases,
manuel utilisateur (`docs/manuel.html`).

### Fonctions v1 du brief (après-midi)
- **Sources externes** : capture HDMI/USB (`capture://N`), flux réseau
  (`rtsp/srt/http/udp`), image fixe, NDI optionnel (`ndi://Nom`) — champ
  « Source externe » de l'onglet Médias.
- **Effets** : pixellisation, postérisation, bruit animé, netteté, miroir —
  intensités 0..1, OSC `/effect/*`, carte Effets (onglet Couleur), mêmes
  formules CPU (référence testée) et GPU (validé naga).
- **Synchro multi-node niveau 1** : `/sync/arm` + `/sync/startAt`
  (timestamp Unix double, départ à l'échéance sur timer, annulé par stop).
- **OSCQuery** (port 8081) : auto-découverte de tous les paramètres dans
  Chataigne, bornes et valeurs live.
- **Fleet** : annonce/découverte mDNS `_toolbox._tcp`, page Réseau
  (Système), bouton « Identifier » (mire coins 4 s sur le node visé).
- **Exploitation** : espace disque, statut Tailscale, boutons
  redémarrer/éteindre la machine ; réglages de sortie persistés
  (`sortie.json`) ; page Releases publique (workflow de release).

### Ajouté
- **Fenêtre de sortie** (`crates/render`) : fenêtre native qui affiche les
  mires puis la vidéo, déformées EN DIRECT par le mapping avec la correction
  couleur — le calibrage projecteur est opérationnel. F11 plein écran,
  Échap le quitte ; fermer la fenêtre n'arrête pas le node.
- **Sélection d'écrans depuis l'UI** : carte « Sortie » de l'onglet Mapping
  (liste des écrans détectés, plein écran), appliquée à chaud ;
  `[output]` dans node.toml pour l'état au démarrage ;
  API `GET /api/outputs` + `POST /api/output`.
- **Backend vidéo GStreamer** (`crates/gst`, feature `gstreamer`) : décodage
  réel multiplateforme (Windows/Ubuntu/Pi, accélération matérielle auto),
  audio système, frames RGBA vers la fenêtre de sortie. Repli automatique
  sur le backend simulé si le runtime manque. Artefact Windows
  **pack autonome** (DLL et plugins inclus, rien à installer).
- **Toggle mapping** : `set_mapping_enabled` (UI, OSC `/mapping/enabled`) —
  bypass du warp sans perdre les réglages.
- **Presets de mapping seul** : `mapping_save`/`mapping_load` (UI, OSC,
  `/api/mapping-presets`), stockés dans `presets/mapping/` ; charger un
  mapping n'interrompt pas la lecture.
- **Démarrage automatique Windows** :
  `deploy/install-autostart-windows.bat` (retrait via `--remove`).

- **Rendu GPU** (wgpu/Vulkan, repli CPU automatique) : le warp, les mires et
  la correction couleur calculés par la carte graphique, vidéo lissée et
  synchronisée à l'écran. `[output] gpu = false` force le CPU.
- **Boucle sans coupure** (mode « un ») via GStreamer `about-to-finish`.
- **Compteur img/s** à côté de la barre de lecture (frames réellement
  présentées par la fenêtre de sortie).

### Robustesse
- Les WebSockets observent l'arrêt du node (extinction immédiate même UI
  ouverte) ; ping serveur 20 s pour détecter les clients disparus.
- Tolérance f32/f64 des tests de rendu ; `Cargo.lock` versionné ;
  première compilation complète du workspace (CI verte sur les 3 cibles).

## [0.1.0-nuit] — nuit du 2026-07-09 au 2026-07-10

### Ajouté
- **P1.0 logs** : ring buffer borné branché sur `tracing`, flux en direct,
  page Logs de la web UI, API `/api/logs` + `/ws/logs`.
- **Player (P1.2)** : trait `PlayerBackend` (GStreamer s'y branchera),
  backend simulé complet (position/durée/fin de média), politique de fin de
  média (boucle un/tout, enchaînement de playlist), position publiée à l'UI.
- **Playlists** : `playlist_set/go/next/prev`, modes de boucle off/one/all.
- **Mapping (P1.3)** : rotation 0/90/180/270°, flip H/V, crop par bord —
  en plus des 4 coins ; matrices de rendu calculées et testées
  (`engine::render`), shaders GLSL mis à jour (uv_transform, gains, mires).
- **Couleur (P1.4)** : gains RGB en plus de
  luminosité/contraste/gamma/saturation/teinte.
- **Mires de test (P1.8)** : grille, damier, identification des coins.
- **Web UI (P1.5)** : embarquée dans le binaire — dashboard, éditeur de
  mapping (préviz homographie temps réel, tactile), couleur, médiathèque
  avec upload streaming, presets, logs en direct, monitoring. Français.
- **Médiathèque (P1.7)** : liste récursive, upload atomique borné,
  renommage, suppression, extensions whitelistées.
- **OSC (P1.6)** : toutes les commandes, tolérant sur les types (Chataigne),
  bundles supportés.
- **MIDI (P1.6)** : bindings note/CC → commandes déclarés dans `node.toml`,
  CC mis à l'échelle des bornes des paramètres.
- **Monitoring (P2.5 partiel)** : `/api/system` — charge, mémoire,
  température (Pi), uptime.
- **Kiosque (P1.9)** : `[startup]` preset + autoplay, service systemd
  (`deploy/`), redémarrage automatique.
- **Installeur (P4.2 partiel)** : `deploy/install.sh` interactif (choix des
  modules), scripts portables Linux/Windows (P1.10).
- **Presets** : branchés sur le bus (`preset_save`/`preset_load` partout),
  validation complète au chargement (fichier trafiqué refusé).
- **CI** : tests Windows, artefacts binaires Linux x64 / Windows x64 /
  Raspberry Pi ARM64 téléchargeables depuis l'onglet Actions.

### Sécurité / robustesse
- Chemins médias : validation anti-traversée unique pour toutes les
  interfaces (UI/REST/OSC/MIDI/presets).
- Écritures atomiques + fsync partout (presets, uploads) ; un crash ne
  corrompt jamais un fichier existant.
- Aucun `unwrap` en production (lint bloquant) ; panics journalisés ;
  services isolés : une erreur OSC/MIDI/HTTP ne fait pas tomber le node.

## [0.1.0] — 2026-07-09

- Squelette du workspace : bus de commandes, état, presets, config,
  homographie validée contre la référence Python, bench phase 0, CI verte
  (27 tests).
