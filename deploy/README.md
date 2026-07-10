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

## 4. Démarrage automatique Windows

Copiez `install-autostart-windows.bat` à côté de `toolbox-node.exe` puis
double-cliquez : Toolbox se lancera (fenêtre réduite) à chaque ouverture de
session. Pour retirer : relancez le script avec `--remove`, ou supprimez
`toolbox-node-autostart.bat` du dossier Démarrage.

Combiné au mode kiosque ci-dessous, le show — mapping compris — reprend seul
au démarrage de l'ordinateur.

## 5. Mode kiosque (P1.9)

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

## 6. Vidéo réelle (backend GStreamer)

Le binaire standard joue les médias « en silence » (backend simulé) et la
fenêtre de sortie n'affiche que les mires. Pour la vraie vidéo, il faut le
binaire compilé avec la feature `gstreamer` **et** le runtime GStreamer :

- **Windows** : artefact CI `toolbox-node-windows-x64-gstreamer` (job
  expérimental) ou compilation locale. Installez le runtime officiel MSVC
  64 bits depuis https://gstreamer.freedesktop.org/download/ (installeur
  `gstreamer-1.0-msvc-x86_64-*.msi`, mode « Complete »), puis ajoutez
  `C:\gstreamer\1.0\msvc_x86_64\bin` au PATH (l'installeur propose de le
  faire). Pour compiler : installez aussi le paquet *development*.
- **Ubuntu / Raspberry Pi OS** :

  ```bash
  # exécution
  sudo apt install gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-libav
  # compilation
  sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev
  cargo build --release -p toolbox-node --features gstreamer
  ```

  Sur Pi, GStreamer choisit tout seul le décodage matériel (V4L2) quand il
  est disponible.

Sans GStreamer sur la machine, ce binaire se replie automatiquement sur le
backend simulé (visible dans les logs) : rien ne casse.

## 7. Ce qui arrive ensuite

- Image carte SD prête à flasher (pi-gen) — phase 4.
- Mise à jour OTA, mot de passe UI, token API — phase 4.
