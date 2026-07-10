# Toolbox — contexte pour Claude

Node multimédia en Rust : player vidéo + projection mapping + contrôle
web/OSC/MIDI. Cibles : Raspberry Pi 4/5 (Linux ARM64), Linux x64, Windows x64.
Propriétaire : Pym.

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
- `crates/render` : fenêtre de sortie native (winit + softbuffer, feature
  `render` du node, activée par défaut, exclue du cross ARM64). Rendu CPU des
  mires warpées + couleur — la chaîne par pixel de `raster.rs` est la
  référence testée de la future passe GLSL. Config `[output]` (écran cible,
  plein écran, F11/Échap). Sans mire : sortie noire. GStreamer remplacera la
  mire par la vidéo dans cette même fenêtre.
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

- Workspace vert : build, 99 tests, clippy `-D warnings`, fmt — local
  (Windows) et CI.
- Corrections post-v1 : dev-dependencies de test manquantes dans control-http
  (`http-body-util`, `tower`), tolérance f32 du test de crop,
  `allow-panic-in-tests`, `Cargo.lock` commité.
- Ajouts demandés par Pym : toggle `mapping.enabled` (bypass du rendu,
  réglages conservés), presets de mapping seul (`presets/mapping/`, commandes
  `mapping_save`/`mapping_load`, API `/api/mapping-presets`, OSC `/mapping/*`,
  UI dans l'onglet Mapping — charger n'interrompt pas la lecture),
  `deploy/install-autostart-windows.bat` (lancement à l'ouverture de session).
- Fenêtre de sortie livrée (crates/render) : mires warpées en direct, choix
  de l'écran via `[output] monitor` (liste tracée au démarrage). La sélection
  d'écran depuis l'UI web reste à faire ; la vidéo réelle attend GStreamer.
- Backend vidéo réel (GStreamer) pas encore commencé — attend le bench sur Pi.

## Prochaines étapes

1. Revue de robustesse : cas limites WebSocket, arrêt propre.
2. Petites améliorations UX de l'UI signalées par des TODO éventuels.
3. Les grosses suites (backend GStreamer, sync multi-device, séquenceur)
   attendent le matériel et les retours de Pym — **ne pas les entamer**.
