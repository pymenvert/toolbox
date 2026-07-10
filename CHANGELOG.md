# Changelog

Évolutions notables du node Toolbox. Format inspiré de
[Keep a Changelog](https://keepachangelog.com/fr/), versionnage SemVer.

## [Non publié] — nuit du 2026-07-09 au 2026-07-10

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
