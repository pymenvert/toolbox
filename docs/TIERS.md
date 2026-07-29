# Composants tiers et mentions légales

Lanterne est un logiciel propriétaire de Pym, distribué sous les termes du
fichier `LICENSE`. Il s'appuie sur des composants tiers dont les licences
imposent leurs propres mentions. Ce document doit accompagner **toute**
distribution du logiciel (il est inclus dans chaque archive de release).

Dernière revue : 2026-07-29 (version 3.4.1).

---

## 1. Bibliothèques Rust

Toutes les dépendances Rust du binaire sont sous licence permissive
(MIT, Apache-2.0, BSD, ISC, Zlib, Unicode, MPL-2.0, IJG). Aucune dépendance
sous GPL/AGPL n'est intégrée au binaire — c'est vérifié **automatiquement à
chaque commit** par le job `licences` de la CI (voir `deny.toml`).

### Mention obligatoire — Independent JPEG Group

L'encodeur JPEG du flux MJPEG (`jpeg-encoder`) est sous
`(MIT OR Apache-2.0) AND IJG`. La licence IJG est permissive et compatible
avec une distribution commerciale, mais elle **exige** que la mention
suivante accompagne le logiciel :

> This software is based in part on the work of the Independent JPEG Group.
>
> (Ce logiciel est basé en partie sur le travail de l'Independent JPEG Group.)

Cette mention doit rester présente dans ce fichier, livré avec chaque
archive, et elle est reprise dans le manuel utilisateur.

Pour produire l'inventaire nominatif complet (nom, version, licence, texte) :

```bash
cargo install cargo-about
cargo about generate about.hbs > licences-rust.html
```

## 2. GStreamer — lecture et sorties vidéo

Le cœur de GStreamer et ses plugins `base` / `good` sont sous
**LGPL v2.1**. Lanterne les utilise en **liaison dynamique** (DLL/`.so`
séparés, jamais liés statiquement) : c'est la condition qui permet de
distribuer une application propriétaire avec eux. Deux obligations en
découlent, remplies par ce document et par la structure du pack :

1. mentionner l'usage de GStreamer et sa licence (fait ici) ;
2. permettre le remplacement des bibliothèques par l'utilisateur — les
   fichiers sont livrés séparément de l'exécutable, dans `lib/` et à côté,
   et donc remplaçables.

Le texte de la LGPL v2.1 est disponible sur
<https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html> et accompagne la
distribution GStreamer utilisée.

### ⚠️ Plugins à licence GPL — point de décision avant commercialisation

Certains plugins de la distribution GStreamer sont sous **GPL** et non LGPL,
notamment :

| Plugin | Paquet | Licence | Usage dans Lanterne |
|---|---|---|---|
| `x264enc` | gst-plugins-ugly | GPL v2 | Sortie **RTSP en H.264** |
| divers décodeurs | gst-libav | GPL/LGPL selon la compilation | Décodage de formats exotiques |

**Conséquence** : livrer ces plugins DANS le même paquet qu'un logiciel
vendu sous licence propriétaire est juridiquement contestable, et un service
juridique côté client peut bloquer la livraison.

**Options** (décision de Pym, non tranchée à ce jour) :

- **A.** Retirer `gst-plugins-ugly` et `gst-libav` du pack. La sortie RTSP
  bascule alors sur **MJPEG**, déjà implémenté et fonctionnel (voir
  `crates/gst/src/rtsp.rs`, branche `else`). Coût : plus de H.264 en RTSP,
  donc davantage de bande passante. Aucune autre fonction n'est touchée.
- **B.** Garder H.264 et faire installer GStreamer **par l'utilisateur**
  (le pack ne l'embarque plus) : le client assemble lui-même, ce qui sort
  Lanterne de la chaîne de distribution des composants GPL.
- **C.** Remplacer `x264enc` par un encodeur matériel non-GPL selon la
  plateforme (`v4l2h264enc` sur Pi, Media Foundation sur Windows).

Tant que la décision n'est pas prise, **le pack Windows « gstreamer » ne
doit pas être vendu tel quel**. Les autres artefacts (Linux, Pi, Windows
sans vidéo) ne sont pas concernés : ils n'embarquent aucun composant GPL.

## 3. SDK NDI (NewTek / Vizrt)

L'entrée et la sortie NDI utilisent le SDK NDI, chargé **dynamiquement à
l'exécution** (`libloading`) : aucun code du SDK n'est intégré au binaire, et
le SDK n'est **jamais** redistribué avec Lanterne (dossier `NDI sdk/` exclu
du dépôt). L'utilisateur installe lui-même les NDI Tools ou le runtime NDI.

Si une redistribution du runtime NDI devait être envisagée, elle exige un
**accord de redistribution** auprès de Vizrt et l'affichage des mentions
imposées par le SDK. À vérifier avant toute vente incluant NDI.

NDI® est une marque déposée de Vizrt NDI AB.

## 4. Polices, icônes, ressources de l'interface

L'interface web n'utilise aucune police ni bibliothèque externe : tout est
embarqué dans un unique fichier HTML, et les pictogrammes sont des
caractères Unicode standard. Aucune obligation d'attribution.
