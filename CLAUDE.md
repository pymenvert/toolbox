# Toolbox / « Lanterne » — contexte pour Claude

Node multimédia en Rust : player vidéo + projection mapping + contrôle
web/OSC/MIDI. Cibles : Raspberry Pi 4/5 (Linux ARM64), Linux x64, Windows x64.
Propriétaire : Pym. Nom d'application affiché : **Lanterne** (UI, manuel,
README) — binaires, crates et artefacts gardent le préfixe `toolbox-`.

## Règles du projet (à respecter absolument)

- **Tout en français** : code, commentaires, docs, messages de commit.
- **Interdits** : `unwrap` en code de prod (lint deny), `expect` hors tests,
  panics silencieux. `clippy.toml` autorise unwrap/expect/panic dans les tests.
- **Contrats publics figés par des tests** : les formats JSON des commandes et
  événements (`crates/core/src/command.rs` et `state.rs`) ne se modifient
  JAMAIS pour faire passer un test — corriger le code, pas le contrat. Idem
  pour les vecteurs de référence de l'homographie
  (`crates/engine/src/homography.rs`, validés contre
  `tools/mapping/homography_ref.py`).
- **UI web = UN SEUL fichier embarqué** (`crates/control-http/assets/index.html`) :
  ne pas la réécrire ni la scinder. Si un test l'exige, corriger côté serveur.
- **Ne pas toucher à `tools/bench/`** (bench matériel à lancer sur les Pi).
- Chaque correction non triviale mérite un test si elle n'en a pas.
- Les tests comparant des valeurs d'état (stockées en f32) à des attentes f64
  utilisent une tolérance ~1e-6, pas 1e-9.

## Architecture

- `crates/core` : bus de commandes (Command → Event broadcast + watch d'état),
  état `NodeState` entièrement validé, presets (branchés sur le bus),
  médiathèque, ring buffer de logs, config `node.toml` (incluant bindings MIDI).
- `crates/engine` : homographie 4 coins, `RenderParams`
  (rotation/flip/crop/couleur → matrices testées), `Player` générique sur le
  trait `PlayerBackend` + `MemoryBackend` simulé. Le backend GStreamer réel
  viendra plus tard (après bench sur Pi) : ne pas l'ajouter.
- `crates/render` : fenêtre de sortie native (winit, feature `render` du
  node, activée par défaut, exclue du cross ARM64). Rendu GPU par défaut
  (`gpu.rs` + `warp.wgsl`, wgpu 30 SANS backend DX12 — allocateur d3d12
  cassé ; Vulkan/GL) avec repli automatique sur le peintre CPU softbuffer
  (`raster.rs`, la référence testée — le shader WGSL doit lui rester
  IDENTIQUE, il est validé par naga en CI). Source par priorité : mire de
  test > frame vidéo (si transport actif) > noir. Écran cible, plein écran
  (et `[output] gpu`) pilotables ; API `/api/outputs` + `/api/output`,
  carte « Sortie » de l'onglet Mapping ; F11/Échap dans la fenêtre ;
  compteur de frames présentées publié pour le badge img/s de l'UI.
- `crates/gst` : backend vidéo GStreamer (`GstBackend`, playbin3 + appsink
  RGBA → canal `watch<Option<VideoFrame>>` vers la fenêtre). Derrière la
  feature `gstreamer` du node (HORS défaut : exige les libs système à la
  compilation, le runtime sur la machine — voir deploy/README.md §6). Sans
  runtime, repli automatique sur MemoryBackend. Vérifié par le job CI
  `check-gstreamer` (Ubuntu) ; artefact Windows
  `toolbox-node-windows-x64-gstreamer` = pack AUTONOME (DLL + plugins
  livrés à côté de l'exe, détectés via `lib/gstreamer-1.0` et
  `GST_PLUGIN_PATH` posé avant `gst::init`), job en continue-on-error.
- `crates/control-http` : axum 0.8 (REST + WebSocket `/ws` et `/ws/logs` + UI
  embarquée + monitoring `/proc`).
- `crates/control-osc` : rosc/UDP.
- `crates/control-midi` : midir (derrière la feature cargo `midi` du node,
  activée par défaut, désactivable car ALSA absent en cross ARM64).
- `crates/node` : binaire d'assemblage (modules activés par `node.toml`, mode
  kiosque startup preset+autoplay, arrêt propre).

## Build et vérifications

```sh
# Linux : ALSA requis par midir
sudo apt-get install -y libasound2-dev

cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Sous Windows, aucune dépendance système (midir utilise WinMM).

## CI (GitHub Actions, `.github/workflows/ci.yml`)

- Jobs : check (fmt+clippy+tests Linux), check-windows (tests), check-arm64
  (cargo check croisé, sans MIDI), shellcheck, puis artefacts binaires :
  `toolbox-node-linux-x64`, `toolbox-node-windows-x64`,
  `toolbox-node-raspberrypi-arm64` (sans MIDI).
- En cas d'échec, la CI pousse ses logs sur des branches `ci-logs-*`
  (diagnostic à distance sans accès à l'onglet Actions).

## État (juillet 2026)

- Workspace vert : build, 160 tests, clippy `-D warnings`, fmt — local
  (Windows) et CI.
- Corrections post-v1 : dev-dependencies de test manquantes dans control-http
  (`http-body-util`, `tower`), tolérance f32 du test de crop,
  `allow-panic-in-tests`, `Cargo.lock` commité.
- Ajouts demandés par Pym : toggle `mapping.enabled` (bypass du rendu,
  réglages conservés), presets de mapping seul (`presets/mapping/`, commandes
  `mapping_save`/`mapping_load`, API `/api/mapping-presets`, OSC `/mapping/*`,
  UI dans l'onglet Mapping — charger n'interrompt pas la lecture),
  `deploy/install-autostart-windows.bat` (lancement à l'ouverture de session).
- v1.0.0 complète du brief (2026-07-10) : sources externes
  (`core/source.rs` — capture://, rtsp/srt/http, ndi://, images fixes),
  effets (EffectsState, `/effect/*`), synchro niveau 1 (`sync_arm`/
  `sync_start_at`, départ sur timer dans Player::run), OSCQuery
  (`control-http/oscquery.rs`, port 8081), fleet mDNS (`node/fleet.rs`,
  `/api/fleet` + `/api/identify`), exploitation (disque, Tailscale,
  reboot/shutdown machine), réglages de sortie persistés (sortie.json),
  page Releases (workflow release.yml, tag v*). PAS testé sur du vrai
  matériel Pi (décision Pym 2026-07-10 : bench matériel plus tard) ni en
  vrai multi-machine. Reste V2 : mesh warp/edge blending, LUT, RTSP out,
  NDI out, upload multi-nodes, OTA, mot de passe UI, QR code, watch
  folder shaders.
- v1.1.0 (2026-07-10, session autonome du soir) : retour d'état OSC
  (`[osc] feedback`, `control-osc::feedback` + `event_to_osc` miroir),
  fondus `preset_fade`/`mapping_fade` (service `core::fader` — `plan()`
  pur testé, ~30 pas/s smoothstep, ne touche JAMAIS média/transport),
  export diagnostic ZIP (`/api/diagnostic.zip`, écrivain ZIP maison
  `control-http::zipper`, zéro dépendance), supervision des services
  (`node::supervision` — fin/panique tracée en ERROR), journal sur disque
  (`node::journal`, tracing-appender quotidien, 14 jours gardés), annonce
  mDNS `_oscjson._tcp` (Chataigne auto-découvre l'OSCQuery), arrêt propre
  sur SIGTERM (systemd), adresses de fondu dans le namespace OSCQuery.
  Aperçu Dashboard, mot de passe optionnel et médias auto-rafraîchis y
  sont aussi (post-v1.0.0). La revue WebSocket est faite : Lagged→resync,
  ping, arrêt propre — rien à corriger.
- v2.0.0 (2026-07-11, nuit autonome sur demande de Pym) : onglet
  Fonctions (core::features + node::bascules, services réellement
  arrêtés/relancés à chaud, fonctions.json), fenêtre dormante (canal
  `enabled`, peintre détruit), **sync à la frame** (node::sync,
  maître/suiveurs UDP auto-config, set_rate + INSTANT_RATE_CHANGE gst,
  dérive mesurée < 2 ms en réel, test de convergence CI en médiane),
  fichiers du parc (proxy + push serveur-side anti-SSRF, reqwest sans
  TLS), **console Art-Net** (crate toolbox-artnet : trames ArtDMX figées
  par test, faders/scènes/chasers, 30 Hz, lumieres.json), **séquenceur**
  (core::sequenceur : cues = commandes du bus, GO/après/quotidien,
  sequences.json), santé (pastilles fonctions + erreurs récentes +
  dérive dans /api/system), OTA expérimental (control-http::ota, curl +
  tar système, bascule à 3 temps avec .precedent), edge blending +
  masques (BlendingState + 8 Masque, parité raster/warp.wgsl 29 vec4,
  vérifiés au pixel, le fader fait glisser le blending). Restent pour
  2.1 : LUT .cube, mesh warp (tâche #48), sorties NDI/RTSP.
- La chaîne vidéo Pi (DRM/KMS) et le bench GStreamer sur matériel réel
  attendent le retour de Pym.
- v3.0.0 (2026-07-11, nuit) : LUT .cube (engine/lut.rs, parité CPU/GPU par
  buffer storage + trilinéaire WGSL, dossier `luts/`, API /api/luts, OSC
  /lut), mesh warp (MappingState.mesh, champ de déplacements ±0,25, grille
  ≤ 9×9, éditeur canvas), régie BLACKOUT (rampe animée dans la fenêtre) /
  FREEZE (gel de la source), slots (cues jours de semaine + actions
  lumières + /cue/go), installateurs à profils (installer-windows.ps1 —
  fichiers écrits SANS BOM, install.sh), smoke test CI (deploy/smoke.sh,
  jobs check et check-windows), télémétrie opt-in (crash.txt toujours,
  envoi curl uniquement si [telemetrie] url), lanceur Chataigne
  (/api/chataigne), identité Lanterne (logo SVG nav + favicon). Chaque
  fonction vérifiée en réel sur node local (pixels d'aperçu, OSC, API).
- v3.1.0 (2026-07-11, matin) : sorties réseau complètes — flux MJPEG
  (/flux.mjpg, thread par client, tampons réutilisés), RTSP
  (gst-rtsp-server, pipeline partagé, test CI DESCRIBE réel), **NDI**
  (crate toolbox-ndi SANS feature cargo : libloading charge la lib à
  l'exécution — FFI recopié des en-têtes du SDK v6 fourni par Pym ;
  dossier local « NDI sdk/ » gitignoré, JAMAIS versionné ; vérifié en
  réel via le runtime des NDI Tools de Pym, libs Pi aarch64/armhf dans
  le SDK). Brique DRM/KMS ([output] mode = "kms", kmssink, run réel
  attend le Pi), compositeur partagé déplacé dans engine
  (frame/frame_rgba), perf (zéro clone d'état/allocation par frame sur
  les chemins chauds), réglages de performance (reglages.json + carte
  Système, profils Pi 3/4/5/PC appliqués au boot), installateur
  intelligent (détection /proc/device-tree/model, mock TOOLBOX_MODELE),
  archives portables avec install.sh embarqué, infobulles néophyte.

- v3.3.0 (2026-07-19) : passe de fiabilisation « commercialisation »
  issue d'un audit multi-agents (7 dimensions × vérification
  adversariale). 8 blocs corrigés, chacun testé : A séquenceur
  (valider_commande refuse délais/heure/cue_go invalides — 400 HTTP,
  enchaînement par NOM, anti-réentrance 250 ms), B Art-Net (canaux
  bornés partout, lumieres.json corrompu → .corrompu), C synchro
  (filtrage IP du maître, chien de garde 5 s → rate 1.0 + dérive None),
  D HTTP (WS 64 Ko max, MJPEG 4 clients max/503, monitor+preview en
  spawn_blocking, identify anti-double, upload tmp unique),
  E persistance (core::ecrire_atomique avec sync_all PARTOUT, crash.txt
  256 Ko max, résolution bornée au load, seq de log sous verrou),
  F FFI/threads (ndi _recv_nom retenu, kms Null-on-fail + bus thread
  borné, rtsp démarrage bloqué géré, fenêtre dormante si peintre KO au
  boot), G rendu (Compositeur : rampe blackout + gel = parité fenêtre
  TESTÉE au pixel, temps modulo 3600, LUT nan/inf refusés, player pause
  après load), H UI (bandeau déconnexion, accueil 1er lancement,
  confirmations deux temps « Confirmer ? », verrous d'actions longues,
  fin des doubles toasts, master drag, masque focus, dblclick volume,
  cue heure vide refusée, polls suspendus onglet caché). 213 tests.

- v3.4.0 (2026-07-22) : 2e passe de fiabilisation « commercialisation »,
  audit multi-agents à 13 dimensions (zones non couvertes en 3.3.0 +
  relecture adversariale des correctifs 3.3.0). 51 problèmes réels
  confirmés, corrigés en 11 blocs testés (223 tests). Faits notables :
  install.sh chown (une install sudo repart fonctionnelle) ; RTSP appsrc
  borné/bloquant (anti-OOM Pi) ; anti-CSRF sur routes mutatrices ; le mot
  de passe UI n'est PLUS relayé au parc mDNS — nouveau `[security]
  fleet_token` (en-tête `x-lanterne-parc`) ; OSC bundles bornés (anti-stack
  overflow) + anti-boucle feedback ; MIDI (saut du Midi Through,
  reconnexion à chaud, binding tolérant aux typos via `commande_tolerante`,
  superviseur dans node::bascules) ; fader applique enfin mesh/masques/LUT
  à t=1 + annulation sur chargement direct ; média : sous-dossier illisible
  toléré ; fenêtre : repli CPU sur perte GPU (`ResultatRendu`), Alt+F4 ne
  tue plus la boucle, LUT invalidée par mtime, badge img/s→0, écrans à
  chaud ; gst : reprise flux réseau (`MediaSource::reconnectable`), pipeline
  Null avant drop ; fleet : nom mDNS unique par machine (`suffixe_unique` +
  propriété `nom`) ; CI en moindre privilège (read par défaut,
  persist-credentials:false, push tokené) ; manuel EMBARQUÉ servi à
  `/manuel`. Rapport détaillé : `AUDIT_V3.3_2026-07-20.md`.
  LIMITE ASSUMÉE (documentée dans bus.rs) : preset_save/mapping_save font
  toujours leur fsync DANS la boucle du bus (bref gel sur sauvegarde
  manuelle) — le sortir casserait l'ordre « sauver puis charger » dont
  dépendent fader et séquenceur ; vrai remède = cache mémoire des presets
  partagé avec le fader (refonte, non faite).

- v3.4.1 (2026-07-29) : relecture ADVERSARIALE DES CORRECTIFS eux-mêmes
  (les deux étapes jamais exécutées de l'audit v3.4.0 : vérifications
  tombées faute de crédits, critique de complétude). 23 défauts confirmés
  DANS les correctifs 3.3.0/3.4.0 + 4 angles morts. Faits notables :
  install.sh chown conditionné au mauvais drapeau (la panne « critique »
  corrigée en 3.4.0 restait possible avec `sudo ./install.sh`) ; le
  rechargement LUT par mtime ne marchait QUE sur le peintre CPU (les 4
  caches portent désormais une clé `nom@mtime`) ; anti-CSRF aveugle au
  WebSocket ; jeton de parc = clé maîtresse (restreint à GET /api/media et
  PUT /api/media/{nom} via `route_de_parc`) ; MIDI : `choisir_port` sans
  filtre n'ouvre plus AUCUN port virtuel (le repli sur Midi Through
  piégeait le superviseur, la reconnexion promise ne marchait jamais sur
  Pi) + journalisation bornée ; diagnostics de config accumulés dans
  `config.avertissements` et rejoués par main.rs APRÈS l'init du
  subscriber ; feedback OSC : `est_impulsion()` exclut les déclencheurs de
  la déduplication ; fader applique blackout/freeze, et `mapping_load` ne
  fige que le mapping ; `toolbox_core::charger_ou_mettre_de_cote` (filet
  `.corrompu`) appliqué aux 6 fichiers d'état + actions de cue tolérantes ;
  journal borné (200 Mo + purge 6 h) ; OTA : `est_plus_recente` (semver),
  `.precedent` conservé, route + bouton « Revenir à la version
  précédente » ; `docs/TIERS.md` + `deny.toml` + job CI `licences` (a
  révélé l'obligation de mention IJG de jpeg-encoder). 235 tests.
  Rapport : `../AUDIT_CORRECTIFS_2026-07-29.md`.
  DÉCISION DE PYM (2026-07-29) : le pack Windows GARDE gst-plugins-ugly
  (x264enc, GPL) pour le RTSP H.264. Pym ne vend pas Lanterne, et le GPL
  n'oblige qu'à la distribution — ici des binaires amont non modifiés,
  publiés gratuitement. Le H.264 vaut mieux que le MJPEG en bande
  passante : la performance prime. **À rouvrir UNIQUEMENT si le pack est
  livré contre paiement** (options + procédure dans `docs/TIERS.md` §2 ;
  le basculement ne demande aucun code, `rtsp.rs` retombe seul sur MJPEG
  si x264enc est absent). Ne pas re-litiger la question autrement.

- v3.5.0 (2026-08-04/05) : publication de la mesure de performance
  (section déjà écrite) + AUDIT DES ZONES JAMAIS COUVERTES — la cohérence
  entre ce que Lanterne DIT et ce qu'il FAIT. 6 dimensions inédites
  (dérive doc/réalité, complétude des contrats de contrôle, UI ↔ serveur,
  démarrage à froid, 3e passe sur la mesure, mise à jour d'une
  installation existante), chaque lot repassé en relecture adversariale,
  et vérifications EMPIRIQUES sur un vrai node à chaque fois que possible.
  267 tests. Faits notables :
  **l'archive Pi officielle ne projette rien** (`--no-default-features` :
  ni fenêtre, ni MIDI, ni GStreamer) alors que le manuel invitait à
  installer des paquets `gstreamer1.0-*` — seul le pack Windows
  `…-gstreamer` lit des vidéos, c'est écrit partout maintenant ;
  **aucune archive publique ne portait `docs/TIERS.md` ni `LICENSE`** (le
  correctif 3.4.1 n'avait été posé que sur ci.yml, jamais sur release.yml
  — donc partout sauf sur ce que les gens téléchargent) ; **l'OTA
  remplaçait le binaire par une variante plus pauvre** (le node déclare
  désormais ses capacités et REFUSE la mise à jour quand aucune archive ne
  fait autant que lui) ; `fps` restait un zéro dur là où `rendu` devient
  null, donc un Pi sain ressortait « sortie morte » ; l'UI enregistrait
  `load "undefined"` dans la conduite ; F11 n'était jamais republié ;
  OSCQuery passe de 45 à 55 adresses (`/rate`, `/blending`, la régie, et
  `/transport`+`/media` en lecture seule) ; faders MIDI sur les effets, la
  vitesse et le master lumières ; `dmx_master`/`dmx_fader` (le master
  n'était joignable que depuis la web UI) ; l'export diagnostic contient
  enfin le CONTENU des presets et les fichiers d'état — c'est la
  sauvegarde d'avant mise à jour ; CI : `apt-get update` cassé par un
  dépôt tiers du runner (touchait main aussi), et les `.ps1` n'étaient
  lintés NULLE PART (job check-windows).
  LIMITES ASSUMÉES, écrites dans le CHANGELOG : l'unité systemd ne
  projette toujours pas en mode fenêtre (ni DISPLAY ni ordonnancement
  graphique — bloc à décommenter fourni, l'activer par défaut casserait
  les installs sans bureau, et cela demande un Pi réel) ; la résolution de
  `reglages.json` ne s'applique QU'À la sortie KMS (le journal et l'UI le
  disent au lieu de faire semblant).
  POINT LAISSÉ À PYM : `deny.toml` justifie sa politique par « Lanterne
  est destiné à être VENDU » et parle de « distribution propriétaire »,
  alors que LICENSE et Cargo.toml sont MIT. La politique (refuser le GPL
  dans le binaire) reste juste et n'a PAS été touchée — la formulation
  touche à la décision sur gst-plugins-ugly, qu'on ne rouvre pas.
  `docs/TIERS.md`, lui, disait « logiciel propriétaire » en citant le
  fichier LICENSE qui dit MIT : corrigé.

## Prochaines étapes

1. Au retour de Pym : tests matériels (Pi, capture HDMI, Chataigne réel,
   multi-machine, `systemctl stop`) — liste dans
   `../RAPPORT_V1_2026-07-10.md`. **La v3.5.0 y ajoute deux points
   précis** : (a) le kiosque en mode fenêtre sur Pi OS Desktop demande de
   décommenter le bloc `DISPLAY`/`graphical.target` de
   `deploy/systemd/toolbox-node.service` et de le valider ; (b) décider si
   l'on publie une archive Linux/Pi AVEC GStreamer (aujourd'hui il faut
   compiler sur place pour projeter depuis un Pi).
2. Petites améliorations UX de l'UI signalées par des TODO éventuels.
3. Les grosses suites (chaîne vidéo Pi, sync multi-device, séquenceur)
   attendent le matériel et les retours de Pym — **ne pas les entamer**.
