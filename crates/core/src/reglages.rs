//! Réglages de performance (`reglages.json`, à côté de node.toml) :
//! résolution de rendu interne, GPU, cadence KMS — choisis dans la carte
//! « Réglages de performance » de l'UI (profils Pi 3 / Pi 4 / Pi 5 / PC)
//! et appliqués AU DÉMARRAGE, par-dessus node.toml (même logique que
//! sortie.json). Le bouton Enregistrer l'écrit ; l'UI explique que le
//! changement prend effet au prochain lancement.

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// Bornes des réglages : un fichier bricolé à la main ne doit pas pouvoir
/// demander un rendu 16K ou 1000 fps.
const LARGEUR_MAX: u32 = 3840;
const HAUTEUR_MAX: u32 = 2160;
const FPS_MAX: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reglages {
    /// Nom du profil choisi (`pi3`, `pi4`, `pi5`, `pc`, `perso`) — purement
    /// informatif, ce sont les champs qui font foi.
    pub profil: String,
    /// Résolution de rendu interne (fenêtre, KMS, référence CPU).
    pub largeur: u32,
    pub hauteur: u32,
    /// Rendu par la carte graphique (repli CPU automatique). À désactiver
    /// sur Pi 3 (GLES 2.0, pas de wgpu).
    pub gpu: bool,
    /// Cadence de la sortie KMS (frames poussées par seconde).
    pub kms_fps: u32,
}

impl Default for Reglages {
    fn default() -> Self {
        Self {
            profil: "pc".to_string(),
            largeur: 1920,
            hauteur: 1080,
            gpu: true,
            kms_fps: 30,
        }
    }
}

impl Reglages {
    /// Relit les réglages persistés ; `None` si absents ou illisibles
    /// (l'appelant garde alors node.toml tel quel).
    pub fn load(path: &std::path::Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let reglages: Self = serde_json::from_slice(&bytes).ok()?;
        reglages.validate().ok()?;
        Some(reglages)
    }

    /// Persiste (écriture atomique, comme sortie.json).
    pub fn save(&self, path: &std::path::Path) -> Result<(), CoreError> {
        self.validate()?;
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
        std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path.display().to_string(), e))
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        let ok = (64..=LARGEUR_MAX).contains(&self.largeur)
            && (64..=HAUTEUR_MAX).contains(&self.hauteur)
            && (1..=FPS_MAX).contains(&self.kms_fps)
            && self.profil.len() <= 32;
        if ok {
            Ok(())
        } else {
            Err(CoreError::InvalidCommand(format!(
                "réglages hors bornes : {}×{} à {} fps (max {LARGEUR_MAX}×{HAUTEUR_MAX} à {FPS_MAX} fps)",
                self.largeur, self.hauteur, self.kms_fps
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_reglages_persistent_et_les_bornes_tiennent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chemin = dir.path().join("reglages.json");

        assert_eq!(Reglages::load(&chemin), None, "absent = None");

        let r = Reglages {
            profil: "pi3".into(),
            largeur: 960,
            hauteur: 540,
            gpu: false,
            kms_fps: 20,
        };
        r.save(&chemin).expect("save");
        assert_eq!(Reglages::load(&chemin), Some(r));

        // Hors bornes : refusé à l'écriture ET ignoré à la lecture.
        let cassee = Reglages {
            largeur: 100_000,
            ..Reglages::default()
        };
        assert!(cassee.save(&chemin).is_err());
        std::fs::write(&chemin, br#"{"largeur":100000}"#).expect("write");
        assert_eq!(Reglages::load(&chemin), None, "fichier bricolé ignoré");
    }
}
