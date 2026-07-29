# Endurance — faire tourner Lanterne longtemps et regarder ce qu'il devient

Certaines pannes ne se voient **que** dans la durée : une mémoire qui grimpe
doucement, un rendu qui se dégrade au fil des heures, un compteur d'images
perdues qui s'emballe. Ce sont exactement celles qui gâchent une
installation permanente — et un test de trente secondes ne les verra jamais.

Ces deux scripts font tourner le node sous charge réelle, échantillonnent son
état à cadence régulière et dépouillent le résultat.

## Ce qui est mesuré

Tout vient de `GET /api/system` :

| Colonne du CSV | Ce que ça dit |
|---|---|
| `rss_mb` | mémoire de **Lanterne lui-même** (pas de la machine) — c'est elle qui révèle une fuite |
| `p50_us` / `p95_us` / `max_us` | temps de production d'une image sur la dernière seconde |
| `sautees` | images perdues depuis le démarrage (cumul) |
| `fps` | images réellement présentées |
| `erreurs` | erreurs dans le journal |

Le `p95` est le chiffre qui compte : c'est l'à-coup qui fait saccader une
projection, pas la moyenne. Au-delà de **16 ms**, une sortie 60 Hz commence à
sauter des images.

## La charge

Un node au repos **ne redessine pas** (la fenêtre ne repeint que sur
changement d'état) : l'observer passivement reviendrait à mesurer un logiciel
qui ne fait rien. Les scripts maintiennent donc en continu :

- un flot de commandes au rythme demandé (défaut : 10/s), comme une console
  OSC qui pilote le spectacle — chacune traverse le bus, republie l'état et
  déclenche un redessin ;
- un client MJPEG permanent, qui fait tourner le compositeur partagé.

`-SansCharge` (ou `CHARGE=0`) permet l'observation passive si besoin.

## Windows

```powershell
# 4 heures, un point toutes les 30 s
.\tools\endurance\endurance.ps1 -Url http://127.0.0.1:8080 -Minutes 240 -IntervalleS 30
```

Le dépouillement s'affiche à la fin. On peut le rejouer sur un CSV existant :

```powershell
.\tools\endurance\endurance.ps1 -Analyser endurance.csv
```

## Linux / Raspberry Pi

```sh
MINUTES=240 INTERVALLE=30 URL=http://127.0.0.1:8080 ./tools/endurance/endurance.sh
```

Le script `.sh` ne fait que **collecter** (curl seul, pas de dépendance).
Le CSV a exactement le même format : on le dépouille avec le script
PowerShell depuis n'importe quelle machine — c'est voulu, pour comparer un
run de Pi et un run de PC avec la même grille de lecture.

## Ce que ce test ne couvre PAS — à lire avant d'en tirer une conclusion

- **La lecture vidéo n'est pas sollicitée.** La charge ne fait que du
  mapping, de la couleur et de la mire : aucun `load`, aucun `play`. Or une
  installation permanente tourne avec une vidéo en boucle 24 h/24, et c'est
  *cette* configuration qui peut fuir. Pour un test représentatif : lancer un
  binaire compilé avec `--features gstreamer`, charger un média et le mettre
  en lecture bouclée **avant** de démarrer le harnais.
- **Le Raspberry Pi en mode KMS ne mesure pas le rendu.** Le chronomètre vit
  dans la fenêtre de sortie ; l'artefact ARM64 officiel est compilé sans
  elle. `/api/system` renvoie alors `"rendu": null` et l'UI affiche « n/d ».
  La mémoire, elle, reste mesurée — c'est déjà la question principale.
- **Le p95 n'a de sens que sur assez d'images.** Le node ne repeint que sur
  changement d'état : si la charge est faible, une seconde ne contient que
  deux ou trois images et le p95 vaut alors le maximum. Le dépouillement le
  signale, et le champ `echantillons` de `/api/system` donne le compte exact.

## Lire le résultat

- **Mémoire** : aucun verdict en dessous de 30 min de run. Les premières
  minutes sont de la montée en régime (caches, allocateur qui ne rend pas
  tout de suite) ; les extrapoler donnerait un faux cri à la fuite.
- **Rendu** : le script compare le premier quart du run au dernier. Une
  hausse de plus de 30 % signale une dégradation dans le temps.
- **Images perdues** : quelques-unes au lancement sont normales. Une
  croissance continue ne l'est pas.
- **Redémarrages** : si `uptime_s` retombe, le node est reparti (panique +
  `Restart=always`). Le dépouillement le signale en tête et invalide les
  tendances — un run traversé par un redémarrage ne dit rien sur une fuite,
  et le redémarrage lui-même est le vrai résultat du test.

Le CSV utilise le point-virgule : Excel français l'ouvre directement.
