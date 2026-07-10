# Toolbox

Node multimédia open source (MIT) : player vidéo, projection mapping 4 coins,
correction couleur, playlists, presets, mires de test, contrôle **web UI /
OSC / MIDI / REST / WebSocket**, page de logs et monitoring intégrés.
Cibles : Raspberry Pi 4/5, Linux, Windows.

> Cadrage complet (décisions, plan, architecture, recherches) : dossier
> `Toolbox/docs/` du projet — ce repo ne contient que le code.

## État — v2.0.0

La chaîne complète est fonctionnelle et testée (160+ tests, CI Linux +
Windows + check ARM64) : **lecture vidéo réelle** (GStreamer, boucle sans
coupure), **fenêtre de sortie** avec warp/mires/couleur/effets calculés par
le **GPU** (wgpu/Vulkan, repli CPU automatique), sources externes (capture,
RTSP/SRT/HTTP, NDI optionnel, images), sélection d'écran et plein écran
depuis l'UI, mappings et presets enregistrés — **rechargeables en fondu**
(coins, couleur, effets et volume glissent sans couper la lecture),
démarrage automatique. Sans GStreamer sur la machine, un backend simulé
prend le relais : l'UI, l'OSC et le MIDI restent démontrables partout.

Côté exploitation : **retour d'état OSC** (les curseurs de Chataigne
suivent le node), auto-découverte **OSCQuery + mDNS** (aucune IP à taper),
**export diagnostic ZIP**, journal quotidien sur disque, supervision des
services, arrêt propre `systemctl stop`, mot de passe optionnel de l'UI.

La V2 ajoute : **synchronisation multi-node à la frame** (maître/suiveurs,
dérive mesurée < 2 ms), **console lumières Art-Net** (faders, scènes,
chasers), **séquenceur** (cues, enchaînements, programmation quotidienne),
**fichiers du parc** (voir/pousser les médias de toutes les machines,
1 → N), **interrupteurs de fonctions** (chaque service réellement arrêté
à chaud, zéro ressource), **edge blending + masques**, page **santé**,
mise à jour **OTA** expérimentale.

📖 **[Manuel utilisateur](docs/manuel.html)** — démarrage rapide,
calibrage pas à pas, référence OSC/MIDI/config, dépannage.

## Démarrage rapide

**Binaires prêts** : page **[Releases](https://github.com/pymenvert/toolbox/releases)**
(sans compte) — `toolbox-node-windows-x64-gstreamer` (pack complet avec
vidéo, rien à installer), `toolbox-node-windows-x64` (léger),
`toolbox-node-linux-x64`, `toolbox-node-raspberrypi-arm64`.

```bash
# ou compilation locale (Linux : sudo apt install libasound2-dev)
cargo run -p toolbox-node             # config : ./node.toml optionnel
```

Puis ouvrez **http://localhost:8080/** — dashboard, mapping, couleur,
médias (upload), presets, logs en direct, monitoring.

- Version portable : `deploy/run-portable.sh` / `run-portable.bat` à côté du binaire.
- Installation Pi/Linux + service systemd (kiosque) : `deploy/install.sh`.
- Configuration complète documentée : [`node.toml.example`](node.toml.example).

## Contrôle à distance

Une commande = un JSON, identique partout (REST, WebSocket, MIDI) avec un
équivalent OSC :

| Action | JSON (`POST /api/command` ou WS) | OSC (UDP :9000) |
|---|---|---|
| Lecture / pause / stop | `{"cmd":"play"}` … | `/play` `/pause` `/stop` |
| Charger un média | `{"cmd":"load","path":"clip.mp4"}` | `/load clip.mp4` |
| Seek / volume | `{"cmd":"seek","seconds":12.5}` | `/seek 12.5` `/volume 0.8` |
| Boucle | `{"cmd":"set_loop","mode":"one"}` | `/loop one` |
| Playlist | `playlist_set/go/next/prev` | `/playlist/…` |
| Coin de mapping | `{"cmd":"corner_set","index":2,"x":0.9,"y":1.0}` | `/corner/2 0.9 1.0` |
| Rotation / flip / crop | `set_rotation`, `set_flip`, `set_crop` | `/rotation 90` `/flip 1 0` `/crop …` |
| Couleur (8 paramètres) | `{"cmd":"color_set","param":"gamma","value":1.2}` | `/color/gamma 1.2` |
| Mires de test | `{"cmd":"set_test_pattern","pattern":"grid"}` | `/pattern grid` |
| Effets (5 shaders) | `{"cmd":"effect_set","param":"pixelate","value":0.5}` | `/effect/pixelate 0.5` |
| Presets | `preset_save` / `preset_load` | `/preset/save nom` |
| Fondu vers un preset | `{"cmd":"preset_fade","name":"scene","seconds":2}` | `/preset/fade scene 2` |
| Fondu de mapping | `{"cmd":"mapping_fade","name":"salon","seconds":2}` | `/mapping/fade salon 2` |
| Synchro multi-node | `sync_arm` puis `sync_start_at` | `/sync/arm` `/sync/startAt` |

Détails : table complète dans `crates/core/src/command.rs` (contrat figé par
tests) ; événements temps réel sur `GET /ws`.

## Structure

```
crates/core/          bus de commandes, état validé, presets, médiathèque,
                      ring buffer de logs, config           [fait, testé]
crates/engine/        homographie (validée vs référence Python), paramètres
                      de rendu (rotation/flip/crop/couleur), player + backend
                      simulé ; GStreamer à venir            [fait, testé]
crates/control-http/  REST + WebSocket + web UI embarquée + monitoring
                                                            [fait, testé]
crates/control-osc/   OSC UDP (Chataigne)                   [fait, testé]
crates/control-midi/  notes/CC → commandes (bindings TOML)  [fait, testé*]
crates/node/          binaire : assemble les modules        [fait]
deploy/               installeur, systemd, portable         [fait]
tools/bench/          bench décodage à lancer sur les Pi    [fait]
webui/                (réservé : UI Svelte phase suivante — l'UI V1 vanilla
                      est embarquée dans control-http)
```
\* la traduction MIDI est testée ; l'ouverture du port reste à valider sur matériel.

## Développement

```bash
cargo test --workspace        # tous les tests
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p toolbox-node
```

La CI (GitHub Actions) vérifie format + clippy + tests sur Linux et Windows,
compile pour ARM64 et publie les binaires en artefacts. En cas d'échec, les
logs sont poussés sur les branches `ci-logs-*`.

## Bench phase 0 (sur Pi 4 / Pi 5 / desktop)

```bash
cd tools/bench
./01_prepare_media.sh
./02_decode_bench.sh    # → results/<host>_<date>.md
```

Critères de sortie : voir `tools/bench/README.md`.
