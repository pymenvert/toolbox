//! Lanceur Chataigne — le logiciel reste un programme séparé (l'embarquer
//! dans un onglet est techniquement impossible : application native JUCE),
//! mais le node sait le détecter, le lancer et pointer vers son
//! téléchargement officiel. Combiné à OSCQuery (port 8081), Chataigne
//! découvre tout seul les paramètres du node.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Page officielle de téléchargement (lien affiché dans l'UI — le node ne
/// télécharge rien lui-même).
pub const URL_TELECHARGEMENT: &str = "https://benjamin.kuperberg.fr/chataigne/en/#download";

/// Réponse de `/api/chataigne`.
#[derive(Debug, Serialize)]
pub struct EtatChataigne {
    pub installe: bool,
    pub chemin: Option<String>,
    pub telechargement: &'static str,
}

/// Emplacements habituels de l'exécutable, par plateforme.
pub fn candidats() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let mut liste = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Ok(base) = std::env::var(var) {
                liste.push(PathBuf::from(&base).join("Chataigne").join("Chataigne.exe"));
            }
        }
        liste
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![
            PathBuf::from("/usr/bin/chataigne"),
            PathBuf::from("/usr/local/bin/chataigne"),
            PathBuf::from("/opt/chataigne/Chataigne"),
        ]
    }
}

/// Premier candidat réellement présent sur le disque.
pub fn detecter(candidats: &[PathBuf]) -> Option<&Path> {
    candidats.iter().find(|c| c.is_file()).map(PathBuf::as_path)
}

/// État courant : installé ou non, où, et où le télécharger sinon.
pub fn etat() -> EtatChataigne {
    let liste = candidats();
    let trouve = detecter(&liste);
    EtatChataigne {
        installe: trouve.is_some(),
        chemin: trouve.map(|p| p.display().to_string()),
        telechargement: URL_TELECHARGEMENT,
    }
}

/// Lance Chataigne en processus détaché (il survit à l'arrêt du node).
pub fn lancer() -> Result<PathBuf, String> {
    let liste = candidats();
    let Some(exe) = detecter(&liste) else {
        return Err("Chataigne introuvable sur cette machine".to_string());
    };
    match std::process::Command::new(exe).spawn() {
        Ok(_) => Ok(exe.to_path_buf()),
        Err(err) => Err(format!("lancement impossible : {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detecte_le_premier_candidat_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absent = dir.path().join("nulle-part/Chataigne.exe");
        let present = dir.path().join("Chataigne.exe");
        std::fs::write(&present, b"exe").expect("write");

        let liste = vec![absent.clone(), present.clone()];
        assert_eq!(detecter(&liste), Some(present.as_path()));
        // Un dossier du même nom ne compte pas comme installé.
        let liste = vec![absent, dir.path().to_path_buf()];
        assert_eq!(detecter(&liste), None);
    }

    #[test]
    fn l_etat_serialise_le_lien_officiel() {
        let etat = etat();
        let json = serde_json::to_value(&etat).expect("json");
        assert_eq!(
            json["telechargement"].as_str(),
            Some(URL_TELECHARGEMENT),
            "l'UI doit toujours pouvoir proposer le téléchargement"
        );
    }
}
