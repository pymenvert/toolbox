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

fn nom_asset_plateforme() -> &'static str {
    if cfg!(target_os = "windows") {
        "toolbox-node-windows-x64.zip"
    } else if cfg!(target_arch = "aarch64") {
        "toolbox-node-raspberrypi-arm64.tar.gz"
    } else {
        "toolbox-node-linux-x64.tar.gz"
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
            assets.iter().find_map(|a| {
                let nom = a.get("name")?.as_str()?;
                (nom == nom_asset_plateforme()).then(|| nom.to_string())
            })
        });
    Ok(EtatMiseAJour {
        plus_recente: version_dispo != version_courante,
        version_courante: version_courante.to_string(),
        version_disponible: Some(version_dispo),
        asset,
    })
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
    let asset = etat
        .asset
        .as_deref()
        .ok_or("pas d'archive pour cette plateforme dans la release")?;
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

/// Nettoyage au démarrage : l'ancien binaire d'une bascule réussie.
pub fn nettoyer_apres_demarrage() {
    if let Ok(dossier) = dossier_du_binaire() {
        for reste in [
            "toolbox-node.exe.precedent",
            "toolbox-node.precedent",
            "mise-a-jour.bat",
        ] {
            let chemin = dossier.join(reste);
            if chemin.exists() && std::fs::remove_file(&chemin).is_ok() {
                info!(fichier = %chemin.display(), "reste de mise à jour nettoyé");
            }
        }
    } else {
        warn!("dossier du binaire inconnu : pas de nettoyage OTA");
    }
}
