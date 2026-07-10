# Déploiement du node Toolbox

## 1. Récupérer un binaire

- **Sans compiler** : GitHub → onglet *Actions* → dernier run vert →
  *Artifacts* : `toolbox-node-linux-x64`, `toolbox-node-windows-x64`,
  `toolbox-node-raspberrypi-arm64` (login GitHub requis).
- **En compilant** : `cargo build --release -p toolbox-node`
  (Linux : `sudo apt install libasound2-dev` pour le MIDI).

## 2. Version portable (P1.10)

Un dossier suffit : le binaire + `run-portable.sh` (Linux/Pi) ou
`run-portable.bat` (Windows). Les dossiers `media/ presets/ logs/` sont créés
à côté au premier lancement. Web UI sur http://localhost:8080/.

## 3. Installation Pi / Linux (P4.2)

```bash
./install.sh                         # interactif : modules, ports, systemd
./install.sh --prefix /opt/toolbox --binary ./toolbox-node
```

## 4. Mode kiosque (P1.9)

1. Réglez votre scène dans la web UI puis sauvegardez un preset (ex. `show`).
2. Dans `node.toml` :

   ```toml
   [startup]
   preset = "show"
   autoplay = true
   ```

3. Service systemd installé par `install.sh` : démarre au boot, redémarre en
   cas de crash (`Restart=always`). Un Pi branché à un VP reprend son show
   seul après une coupure de courant.

Commandes utiles :

```bash
sudo systemctl start|stop|status toolbox-node
journalctl -u toolbox-node -f      # ou la page Logs de la web UI
```

## 5. Ce qui arrive ensuite

- Image carte SD prête à flasher (pi-gen) — phase 4.
- Mise à jour OTA, mot de passe UI, token API — phase 4.
