//! Télémétrie d'incidents — STRICTEMENT opt-in (`[telemetrie] url`).
//!
//! Deux moitiés indépendantes :
//! - à chaque panic, un rapport est écrit dans `crash.txt` (dossier de logs),
//!   que la télémétrie soit configurée ou non — c'est d'abord un outil de
//!   diagnostic local ;
//! - au démarrage suivant, SI une URL est configurée ET qu'un rapport
//!   attend, il est envoyé en POST (curl système, comme l'OTA) puis
//!   supprimé. Sans URL, le fichier reste sur place et rien ne sort.

use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

/// Emplacement du rapport de crash, à côté des journaux.
pub fn chemin_crash(dossier_logs: &Path) -> PathBuf {
    dossier_logs.join("crash.txt")
}

/// Formate un rapport : horodatage epoch, version, texte du panic. Pur pour
/// être testable — l'écriture disque est séparée.
pub fn rapport(version: &str, nom: &str, panic: &str) -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch={epoch}\nversion={version}\nnode={nom}\n---\n{panic}\n")
}

/// Plafond de `crash.txt` : un node kiosque qui panique en boucle (relancé
/// par systemd) ne doit pas remplir la carte SD. Au-delà, on repart du
/// rapport le plus récent — c'est le plus utile au diagnostic.
const CRASH_MAX_OCTETS: u64 = 256 * 1024;

/// Écrit (en ajout, plafonné) un rapport de crash. Appelée depuis le hook
/// de panic : ne doit JAMAIS paniquer elle-même, toute erreur est
/// silencieusement ignorée (le panic d'origine est déjà journalisé).
pub fn ecrire_rapport(chemin: &Path, contenu: &str) {
    if let Some(parent) = chemin.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let deborde = std::fs::metadata(chemin).is_ok_and(|m| m.len() > CRASH_MAX_OCTETS);
    let mut options = std::fs::OpenOptions::new();
    options.create(true);
    if deborde {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    if let Ok(mut fichier) = options.open(chemin) {
        if deborde {
            let _ = fichier
                .write_all(b"(rapports precedents purges : fichier au-dela du plafond)\n---\n");
        }
        let _ = fichier.write_all(contenu.as_bytes());
    }
}

/// La règle d'envoi, isolée pour le test : on n'envoie QUE si une URL est
/// configurée ET qu'un rapport attend.
pub fn doit_envoyer(url: Option<&str>, rapport_present: bool) -> bool {
    url.is_some_and(|u| !u.trim().is_empty()) && rapport_present
}

/// Au démarrage : envoie le rapport en attente s'il y en a un et que la
/// télémétrie est configurée. POST via le curl du système (même choix que
/// l'OTA : pas de pile TLS embarquée). Supprimé uniquement après un envoi
/// réussi — sinon on retentera au prochain démarrage.
pub async fn envoyer_rapport_en_attente(url: Option<&str>, chemin: &Path) {
    if !doit_envoyer(url, chemin.exists()) {
        return;
    }
    // doit_envoyer garantit Some ici ; le repli est inatteignable.
    let Some(url) = url else { return };
    // curl bloquant → tâche dédiée (même approche que l'OTA : le curl du
    // système évite d'embarquer une pile TLS).
    let url_curl = url.to_string();
    let fichier = format!("@{}", chemin.display());
    let statut = tokio::task::spawn_blocking(move || {
        std::process::Command::new("curl")
            .args([
                "-sf",
                "--max-time",
                "20",
                "-X",
                "POST",
                "-H",
                "Content-Type: text/plain; charset=utf-8",
                "--data-binary",
            ])
            .arg(fichier)
            .arg(url_curl)
            .status()
    })
    .await;
    let statut = match statut {
        Ok(s) => s,
        Err(err) => {
            warn!(%err, "tâche d'envoi du rapport interrompue");
            return;
        }
    };
    match statut {
        Ok(s) if s.success() => {
            if let Err(err) = std::fs::remove_file(chemin) {
                warn!(%err, "rapport de crash envoyé mais impossible à supprimer");
            } else {
                info!(url, "rapport de crash envoyé à la télémétrie");
            }
        }
        Ok(s) => warn!(
            code = s.code().unwrap_or(-1),
            "envoi du rapport de crash refusé — nouvel essai au prochain démarrage"
        ),
        Err(err) => warn!(%err, "curl indisponible : rapport de crash conservé sur place"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_rapport_contient_version_node_et_panic() {
        let r = rapport("2.0.0", "vp-01", "PANIC : boom");
        assert!(r.contains("version=2.0.0"));
        assert!(r.contains("node=vp-01"));
        assert!(r.contains("PANIC : boom"));
        assert!(r.starts_with("epoch="));
    }

    #[test]
    fn ecrire_rapport_cree_le_dossier_et_ajoute() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chemin = chemin_crash(&dir.path().join("logs"));
        ecrire_rapport(&chemin, "premier\n");
        ecrire_rapport(&chemin, "second\n");
        let contenu = std::fs::read_to_string(&chemin).expect("read");
        assert_eq!(contenu, "premier\nsecond\n");
    }

    #[test]
    fn sans_url_on_n_envoie_jamais() {
        // La règle opt-in : URL absente ou vide → pas d'envoi, même si un
        // rapport attend.
        assert!(!doit_envoyer(None, true));
        assert!(!doit_envoyer(Some(""), true));
        assert!(!doit_envoyer(Some("   "), true));
        // URL présente mais rien à envoyer → pas d'envoi non plus.
        assert!(!doit_envoyer(Some("https://exemple.fr"), false));
        assert!(doit_envoyer(Some("https://exemple.fr"), true));
    }

    #[tokio::test]
    async fn sans_url_le_fichier_reste_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chemin = chemin_crash(dir.path());
        ecrire_rapport(&chemin, "rapport\n");
        envoyer_rapport_en_attente(None, &chemin).await;
        assert!(chemin.exists(), "sans URL, rien ne bouge");
    }
}
