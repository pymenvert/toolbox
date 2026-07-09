# Toolbox

Node multimédia open source (MIT) : player vidéo, projection mapping,
contrôle OSC/MIDI/HTTP, web UI, séquenceur. Cibles : Raspberry Pi 4/5,
Linux, Windows.

> Cadrage complet (décisions, plan, architecture, recherches) : dossier
> `Toolbox/docs/` du projet — ce repo ne contient que le code.

## État

Phase 0 — dérisquage : squelette du workspace + bench vidéo.

## Structure

```
crates/core/   bus de commandes, état, presets, config   [fait, testé]
crates/node/   binaire (assemble les modules)            [squelette]
tools/bench/   bench décodage/rendu à lancer sur les Pi  [fait]
webui/         web UI Svelte                              [phase 1]
deploy/        installeur, image Pi, portable             [phase 4]
```

## Développement

```bash
cargo test          # tests
cargo clippy        # lints (unwrap interdit)
cargo run -p toolbox-node   # lance le node (config: ./node.toml optionnel)
```

## Bench phase 0 (sur Pi 4 / Pi 5 / desktop)

```bash
cd tools/bench
./01_prepare_media.sh
./02_decode_bench.sh    # → results/<host>_<date>.md
```

Critères de sortie : voir `tools/bench/README.md`.
