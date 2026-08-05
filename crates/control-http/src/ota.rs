//! Mise à jour OTA (V2, expérimental) : télécharger la dernière release
//! GitHub et préparer la bascule — SANS jamais remplacer un binaire à
//! l'aveugle.
//!
//! Déroulé prudent, en trois temps :
//! 1. `GET /api/update/check` : interroge l'API GitHub (via `curl` système,
//!    pas de pile TLS embarquée) et compare à la version courante ;
//! 2. `POST /api/update/download` : télécharge l'archive de la plateforme
//!    dans `update/` À CÔTÉ du binaire, vérifie taille et signature de
//!    format (zip/tar.gz), extrait le binaire en `toolbox-node.nouveau` ;
//! 3. `POST /api/update/apply` : pose le script de bascule et ARRÊTE le
//!    node — le gestionnaire de service (systemd `Restart=always`,
//!    démarrage auto Windows) relance la nouvelle version. Sous Windows, un
//!    `.bat` remplace l'exe verrouillé puis relance ; sous Linux, `rename()`
//!    suffit avant l'arrêt.
//!
//! En cas d'échec à n'importe quelle étape : rien n'a changé, le binaire
//! courant reste en place.

use serde::Serialize;
use tracing::{info, warn};

const DEPOT: &str = "pymenvert/toolbox";

/// Ce que rapporte `check`.
#[derive(Debug, Serialize)]
pub struct EtatMiseAJour {
    pub version_courante: String,
    pub version_disponible: Option<String>,
    pub plus_recente: bool,
    /// Nom de l'archive adaptée à cette plateforme, si trouvée.
    pub asset: Option<String>,
}

/// Ce que le binaire EN COURS d'exécution sait faire. Déclaré une fois au
/// démarrage par le binaire lui-même (lui seul connaît ses features).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capacites {
    /// Décodage vidéo et sorties GStreamer (feature `gstreamer`).
    pub video: bool,
    /// Fenêtre de sortie (feature `render`).
    pub fenetre: bool,
}

static CAPACITES: std::sync::OnceLock<Capacites> = std::sync::OnceLock::new();

/// À appeler au démarrage du node. Sans appel, on suppose le binaire le plus
/// pauvre — jamais l'inverse : mieux vaut refuser une mise à jour possible
/// que d'en proposer une qui retire des fonctions.
pub fn declarer_capacites(capacites: Capacites) {
    let _ = CAPACITES.set(capacites);
}

fn capacites() -> Capacites {
    CAPACITES.get().copied().unwrap_or_default()
}

/// L'archive de release qui correspond à CE binaire — `None` quand aucune
/// archive publiée ne sait faire autant que lui.
///
/// Le choix se faisait sur la seule plateforme : sous Windows il visait
/// toujours `…-windows-x64.zip`, le binaire LÉGER, y compris sur une machine
/// installée avec le pack vidéo. « Mettre à jour » depuis l'onglet Système
/// remplaçait donc un node qui lit des vidéos par un node qui n'en lit plus
/// — les DLL GStreamer toujours là, mais plus aucun code pour s'en servir.
/// Sur un Pi où l'on a compilé sur place (le seul moyen de projeter), il
/// proposait l'archive `--no-default-features` : ni vidéo, ni fenêtre.
fn nom_asset_plateforme() -> Option<&'static str> {
    asset_pour(
        capacites(),
        cfg!(target_os = "windows"),
        cfg!(target_arch = "aarch64"),
    )
}

/// Logique pure (plateforme passée en paramètre) : testable pour Windows
/// comme pour le Pi depuis n'importe quelle machine.
fn asset_pour(capacites: Capacites, windows: bool, aarch64: bool) -> Option<&'static str> {
    if windows {
        // Seule plateforme à publier les deux variantes.
        Some(if capacites.video {
            "toolbox-node-windows-x64-gstreamer.zip"
        } else {
            "toolbox-node-windows-x64.zip"
        })
    } else if capacites.video {
        // Aucune archive Linux ou Pi ne contient GStreamer : la seule façon
        // d'avoir la vidéo y est de compiler sur place. Se mettre à jour
        // depuis la page Releases reviendrait à la perdre.
        None
    } else if aarch64 {
        // L'archive ARM64 officielle est compilée --no-default-features :
        // elle n'a même pas de fenêtre de sortie.
        (!capacites.fenetre).then_some("toolbox-node-raspberrypi-arm64.tar.gz")
    } else {
        Some("toolbox-node-linux-x64.tar.gz")
    }
}

/// `curl` système : présent sur Windows 10+, Linux et Pi OS. Sortie stdout.
fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let sortie = std::process::Command::new("curl")
        .args(args)
        .output()
        .map_err(|e| format!("curl indisponible : {e}"))?;
    if sortie.status.success() {
        Ok(sortie.stdout)
    } else {
        Err(format!(
            "curl a échoué ({}) : {}",
            sortie.status,
            String::from_utf8_lossy(&sortie.stderr)
        ))
    }
}

/// Interroge la dernière release GitHub.
pub fn verifier(version_courante: &str) -> Result<EtatMiseAJour, String> {
    let json = curl(&[
        "-sL",
        "-H",
        "Accept: application/vnd.github+json",
        &format!("https://api.github.com/repos/{DEPOT}/releases/latest"),
    ])?;
    let release: serde_json::Value =
        serde_json::from_slice(&json).map_err(|e| format!("réponse GitHub illisible : {e}"))?;
    let tag = release
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or("pas de tag dans la réponse GitHub")?
        .to_string();
    let version_dispo = tag.trim_start_matches('v').to_string();
    let asset = release
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| {
            let attendu = nom_asset_plateforme()?;
            assets.iter().find_map(|a| {
                let nom = a.get("name")?.as_str()?;
                (nom == attendu).then(|| nom.to_string())
            })
        });
    Ok(EtatMiseAJour {
        // STRICTEMENT supérieure. Une simple inégalité annonçait une « mise
        // à jour » vers une version ANTÉRIEURE dès qu'un node tournait sur
        // un binaire de CI plus récent que la dernière release — et un
        // retour arrière peut rendre illisibles des fichiers d'état.
        plus_recente: est_plus_recente(&version_dispo, version_courante),
        version_courante: version_courante.to_string(),
        version_disponible: Some(version_dispo),
        asset,
    })
}

/// `dispo` est-elle strictement postérieure à `courante` ? Comparaison
/// numérique champ par champ (majeur.mineur.correctif) ; un champ non
/// numérique (pré-version « 3.5.0-rc1 ») compte pour 0, ce qui rend une
/// pré-version NON proposée face à la version finale — prudence voulue.
pub fn est_plus_recente(dispo: &str, courante: &str) -> bool {
    fn triplet(v: &str) -> (u32, u32, u32) {
        let base = v.split(['-', '+']).next().unwrap_or(v);
        let mut it = base.split('.').map(|c| c.parse::<u32>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    }
    triplet(dispo) > triplet(courante)
}

/// Télécharge et prépare le nouveau binaire (`toolbox-node.nouveau[.exe]`)
/// dans le dossier du binaire courant. Ne touche PAS au binaire en place.
pub fn telecharger() -> Result<String, String> {
    let etat = verifier(env!("CARGO_PKG_VERSION"))?;
    let Some(version) = &etat.version_disponible else {
        return Err("aucune release publiée".into());
    };
    if !etat.plus_recente {
        return Err(format!("déjà en {version} : rien à télécharger"));
    }
    let asset = etat.asset.as_deref().ok_or_else(|| {
        if nom_asset_plateforme().is_none() {
            "ce binaire a été compilé sur place (vidéo et/ou fenêtre de sortie) : \
             aucune archive publiée ne fait autant. Une mise à jour automatique \
             vous ferait PERDRE la lecture vidéo. Recompilez plutôt sur la machine."
                .to_string()
        } else {
            "pas d'archive pour cette plateforme dans la release".to_string()
        }
    })?;
    let url = format!("https://github.com/{DEPOT}/releases/latest/download/{asset}");
    let dossier = dossier_du_binaire()?;
    let archive = dossier.join(asset);
    info!(%url, "téléchargement de la mise à jour");
    curl(&["-sL", "--fail", "-o", &archive.to_string_lossy(), &url])?;

    // Garde-fous : taille plausible et signature de format.
    let octets = std::fs::read(&archive).map_err(|e| format!("archive illisible : {e}"))?;
    if octets.len() < 500_000 {
        let _ = std::fs::remove_file(&archive);
        return Err(format!(
            "archive suspecte ({} octets) : mise à jour abandonnée",
            octets.len()
        ));
    }
    let zip = octets.starts_with(b"PK\x03\x04");
    let targz = octets.starts_with(&[0x1f, 0x8b]);
    if !(zip || targz) {
        let _ = std::fs::remove_file(&archive);
        return Err("format d'archive inattendu : mise à jour abandonnée".into());
    }

    // Extraction du binaire seul, sous un nom NEUTRE (pas de remplacement).
    let nouveau = dossier.join(if cfg!(windows) {
        "toolbox-node.nouveau.exe"
    } else {
        "toolbox-node.nouveau"
    });
    extraire_binaire(&archive, &nouveau)?;
    let _ = std::fs::remove_file(&archive);
    info!(chemin = %nouveau.display(), "nouveau binaire prêt (bascule à confirmer)");
    Ok(format!(
        "Binaire {} prêt : confirmer la bascule pour l'appliquer.",
        etat.version_disponible.unwrap_or_default()
    ))
}

fn dossier_du_binaire() -> Result<std::path::PathBuf, String> {
    std::env::current_exe()
        .map_err(|e| format!("chemin du binaire inconnu : {e}"))?
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "binaire sans dossier parent".into())
}

/// Extrait `toolbox-node(.exe)` de l'archive vers `destination` en
/// s'appuyant sur `tar` (Windows 10+ et Linux l'ont, y compris pour
/// les .zip côté Windows via `tar -xf`).
fn extraire_binaire(
    archive: &std::path::Path,
    destination: &std::path::Path,
) -> Result<(), String> {
    let dossier = tempfile_dir(archive)?;
    let statut = std::process::Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(&dossier)
        .status()
        .map_err(|e| format!("tar indisponible : {e}"))?;
    if !statut.success() {
        return Err(format!("extraction échouée ({statut})"));
    }
    let nom = if cfg!(windows) {
        "toolbox-node.exe"
    } else {
        "toolbox-node"
    };
    let binaire =
        chercher(&dossier, nom).ok_or_else(|| format!("{nom} introuvable dans l'archive"))?;
    std::fs::copy(&binaire, destination).map_err(|e| format!("copie : {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755));
    }
    let _ = std::fs::remove_dir_all(&dossier);
    Ok(())
}

fn tempfile_dir(a_cote_de: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let dossier = a_cote_de
        .parent()
        .ok_or("archive sans dossier")?
        .join(".update-extraction");
    let _ = std::fs::remove_dir_all(&dossier);
    std::fs::create_dir_all(&dossier).map_err(|e| format!("dossier d'extraction : {e}"))?;
    Ok(dossier)
}

fn chercher(dossier: &std::path::Path, nom: &str) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dossier).ok()?;
    for entry in entries.flatten() {
        let chemin = entry.path();
        if chemin.is_dir() {
            if let Some(trouve) = chercher(&chemin, nom) {
                return Some(trouve);
            }
        } else if chemin.file_name().and_then(|n| n.to_str()) == Some(nom) {
            return Some(chemin);
        }
    }
    None
}

/// Applique la bascule préparée par [`telecharger`], puis demande l'arrêt
/// du node (le service/démarrage auto relance la nouvelle version).
/// Retourne une description de ce qui va se passer.
pub fn appliquer() -> Result<String, String> {
    let dossier = dossier_du_binaire()?;
    let courant = std::env::current_exe().map_err(|e| format!("binaire courant : {e}"))?;
    let nouveau = dossier.join(if cfg!(windows) {
        "toolbox-node.nouveau.exe"
    } else {
        "toolbox-node.nouveau"
    });
    if !nouveau.is_file() {
        return Err("aucun binaire préparé : lancer d'abord le téléchargement".into());
    }

    #[cfg(windows)]
    {
        // L'exe courant est verrouillé tant que le process vit : un .bat
        // attend la fin du process, garde l'ancien en .precedent, remplace,
        // puis relance.
        let script = dossier.join("mise-a-jour.bat");
        let contenu = format!(
            "@echo off\r\n\
             timeout /t 2 /nobreak > NUL\r\n\
             move /y \"{courant}\" \"{courant}.precedent\" > NUL\r\n\
             move /y \"{nouveau}\" \"{courant}\" > NUL\r\n\
             start \"\" \"{courant}\"\r\n",
            courant = courant.display(),
            nouveau = nouveau.display(),
        );
        std::fs::write(&script, contenu).map_err(|e| format!("script de bascule : {e}"))?;
        std::process::Command::new("cmd")
            .args(["/C", "start", "/min", "", &script.to_string_lossy()])
            .spawn()
            .map_err(|e| format!("lancement du script : {e}"))?;
        info!("bascule Windows programmée : le node redémarre dans quelques secondes");
    }
    #[cfg(unix)]
    {
        // rename() sur soi-même est sûr sous Unix (l'inode courant survit
        // au process) ; systemd Restart=always relance la nouvelle version.
        let precedent = dossier.join("toolbox-node.precedent");
        std::fs::rename(&courant, &precedent).map_err(|e| format!("sauvegarde : {e}"))?;
        if let Err(err) = std::fs::rename(&nouveau, &courant) {
            // Échec : on remet l'ancien en place, rien n'a changé.
            let _ = std::fs::rename(&precedent, &courant);
            return Err(format!("bascule échouée (annulée) : {err}"));
        }
        info!("bascule Unix effectuée : redémarrage du node (Restart=always attendu)");
    }

    // L'arrêt effectif est déclenché par l'appelant (handler HTTP) après
    // la réponse — voir /api/update/apply.
    Ok("Mise à jour appliquée : le node redémarre.".into())
}

/// Nettoyage au démarrage — mais SEULEMENT le script de bascule, jamais le
/// binaire précédent.
///
/// L'ancien binaire est le SEUL filet de sécurité d'un opérateur non
/// développeur dont le node ne redémarre plus après une mise à jour : le
/// supprimer au tout premier démarrage de la nouvelle version (donc avant
/// d'avoir la moindre preuve qu'elle fonctionne) revenait à couper la corde
/// en même temps qu'on la tend. Il est conservé jusqu'au prochain
/// `telecharger()`, et reste utilisable via [`revenir_en_arriere`].
pub fn nettoyer_apres_demarrage() {
    let Ok(dossier) = dossier_du_binaire() else {
        warn!("dossier du binaire inconnu : pas de nettoyage OTA");
        return;
    };
    // `relance.bat` autant que `mise-a-jour.bat` : le retour arrière en pose
    // un, il ne doit pas rester dans le dossier d'installation.
    for nom in ["mise-a-jour.bat", "relance.bat"] {
        let script = dossier.join(nom);
        if script.exists() && std::fs::remove_file(&script).is_ok() {
            info!(fichier = %script.display(), "script de bascule nettoyé");
        }
    }
    if binaire_precedent().is_some() {
        info!("version précédente conservée : retour arrière possible depuis l'onglet Système");
    }
}

/// Le binaire de la version précédente, s'il existe encore.
pub fn binaire_precedent() -> Option<std::path::PathBuf> {
    let dossier = dossier_du_binaire().ok()?;
    ["toolbox-node.exe.precedent", "toolbox-node.precedent"]
        .iter()
        .map(|n| dossier.join(n))
        .find(|p| p.exists())
}

/// Remet la version précédente en place (retour arrière après une mise à
/// jour ratée). Le node doit redémarrer ensuite, comme pour `appliquer`.
pub fn revenir_en_arriere() -> Result<String, String> {
    let precedent = binaire_precedent().ok_or(
        "aucune version précédente conservée — rien à restaurer (le retour arrière n'est possible qu'après une mise à jour)",
    )?;
    let dossier = dossier_du_binaire()?;
    let courant = dossier.join(if cfg!(windows) {
        "toolbox-node.exe"
    } else {
        "toolbox-node"
    });
    // On garde la version défaillante sous la main plutôt que de l'effacer :
    // elle peut servir au diagnostic.
    let ecarte = courant.with_extension("defaillant");
    let _ = std::fs::remove_file(&ecarte);
    std::fs::rename(&courant, &ecarte).map_err(|e| format!("mise à l'écart : {e}"))?;
    if let Err(err) = std::fs::rename(&precedent, &courant) {
        let _ = std::fs::rename(&ecarte, &courant); // rien n'a changé
        return Err(format!("retour arrière échoué (annulé) : {err}"));
    }
    #[cfg(windows)]
    {
        // Sous Windows il n'y a NI service NI Restart=always : le démarrage
        // automatique n'est qu'un .bat du dossier Démarrage, exécuté à
        // l'ouverture de session. Sans ce script, le retour arrière arrêtait
        // le node et personne ne le relançait — alors que le message promet
        // « le node redémarre, reconnexion automatique ». Même mécanique que
        // `appliquer()`, à ceci près que les renommages sont déjà faits :
        // il ne reste qu'à attendre la fin du process et à relancer.
        let script = dossier.join("relance.bat");
        let contenu = format!(
            "@echo off\r\n\
             timeout /t 2 /nobreak > NUL\r\n\
             start \"\" \"{courant}\"\r\n",
            courant = courant.display(),
        );
        match std::fs::write(&script, contenu) {
            Ok(()) => {
                if let Err(err) = std::process::Command::new("cmd")
                    .args(["/C", "start", "/min", "", &script.to_string_lossy()])
                    .spawn()
                {
                    warn!(%err, "relance Windows non programmée : relancer le node à la main");
                }
            }
            Err(err) => warn!(%err, "script de relance non écrit : relancer le node à la main"),
        }
    }
    info!("retour à la version précédente effectué : le node redémarre");
    Ok("Version précédente restaurée : le node redémarre.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une mise à jour ne doit JAMAIS retirer de fonctions. Le choix se
    /// faisait sur la seule plateforme : sous Windows il visait toujours le
    /// binaire léger, y compris sur une machine installée avec le pack
    /// vidéo — « Mettre à jour » retirait alors la lecture vidéo.
    #[test]
    fn l_archive_visee_ne_retire_jamais_de_fonctions() {
        let leger = Capacites {
            video: false,
            fenetre: true,
        };
        let pack_video = Capacites {
            video: true,
            fenetre: true,
        };
        let arm64_officiel = Capacites {
            video: false,
            fenetre: false,
        };

        // Windows : la seule plateforme qui publie les deux variantes.
        assert_eq!(
            asset_pour(pack_video, true, false),
            Some("toolbox-node-windows-x64-gstreamer.zip")
        );
        assert_eq!(
            asset_pour(leger, true, false),
            Some("toolbox-node-windows-x64.zip")
        );

        // Linux/Pi : aucune archive ne contient GStreamer. Un binaire
        // compilé sur place pour avoir la vidéo ne doit rien se voir
        // proposer, plutôt que de la perdre.
        assert_eq!(asset_pour(pack_video, false, false), None);
        assert_eq!(asset_pour(pack_video, false, true), None);

        // L'archive ARM64 officielle n'a même pas de fenêtre de sortie :
        // elle ne convient qu'à un binaire qui n'en a pas non plus.
        assert_eq!(asset_pour(leger, false, true), None);
        assert_eq!(
            asset_pour(arm64_officiel, false, true),
            Some("toolbox-node-raspberrypi-arm64.tar.gz")
        );

        // Linux x64 : l'archive officielle a bien la fenêtre.
        assert_eq!(
            asset_pour(leger, false, false),
            Some("toolbox-node-linux-x64.tar.gz")
        );
    }

    /// Sans déclaration explicite, on suppose le binaire le plus pauvre —
    /// jamais l'inverse.
    #[test]
    fn capacites_par_defaut_prudentes() {
        assert_eq!(
            Capacites::default(),
            Capacites {
                video: false,
                fenetre: false
            }
        );
    }

    /// Le coeur du garde-fou : ne JAMAIS proposer une version antérieure.
    /// Avant, une simple inégalité proposait « 3.3.0 » à un node en 3.4.0
    /// (binaire de CI plus récent que la dernière release) — et accepter
    /// revenait à installer un binaire qui peut ne plus relire les fichiers
    /// d'état écrits depuis.
    #[test]
    fn est_plus_recente_compare_bien_les_versions() {
        assert!(est_plus_recente("3.5.0", "3.4.0"));
        assert!(est_plus_recente("3.4.1", "3.4.0"));
        assert!(est_plus_recente("4.0.0", "3.9.9"));
        // Le piège corrigé : plus ancienne ou identique = pas de proposition.
        assert!(!est_plus_recente("3.3.0", "3.4.0"));
        assert!(!est_plus_recente("3.4.0", "3.4.0"));
        // Comparaison NUMÉRIQUE, pas lexicographique ("10" > "9").
        assert!(est_plus_recente("3.10.0", "3.9.0"));
        // Une pré-version n'est pas proposée face à la version finale.
        assert!(!est_plus_recente("3.4.0-rc1", "3.4.0"));
        // Champs manquants ou illisibles : traités comme 0, jamais de panique.
        assert!(est_plus_recente("3.5", "3.4.9"));
        assert!(!est_plus_recente("", "3.4.0"));
        assert!(!est_plus_recente("bidon", "3.4.0"));
    }

    /// Sans mise à jour préalable, le retour arrière refuse proprement au
    /// lieu de toucher au binaire en place.
    #[test]
    fn revenir_en_arriere_sans_precedent_refuse() {
        if binaire_precedent().is_none() {
            let err = revenir_en_arriere().expect_err("doit refuser");
            assert!(err.contains("aucune version précédente"), "message : {err}");
        }
    }
}
