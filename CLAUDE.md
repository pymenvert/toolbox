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

## Prochaines étapes

1. Au retour de Pym : tests matériels (Pi, capture HDMI, Chataigne réel,
   multi-machine, `systemctl stop`) — liste dans
   `../RAPPORT_V1_2026-07-10.md`.
2. Petites améliorations UX de l'UI signalées par des TODO éventuels.
3. Les grosses suites (chaîne vidéo Pi, sync multi-device, séquenceur)
   attendent le matériel et les retours de Pym — **ne pas les entamer**.
