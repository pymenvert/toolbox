//! Erreurs du crate core. Toujours explicites : pas de `unwrap` en prod
//! (interdit par lint workspace), chaque erreur porte son contexte.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("commande invalide : {0}")]
    InvalidCommand(String),

    #[error("index de coin invalide : {0} (attendu 0..=3)")]
    InvalidCorner(u8),

    #[error("rotation invalide : {0}° (attendu 0, 90, 180 ou 270)")]
    InvalidRotation(u16),

    #[error("valeur hors bornes pour {param} : {value} (attendu {min}..={max})")]
    OutOfRange {
        param: &'static str,
        value: f64,
        min: f64,
        max: f64,
    },

    #[error("chemin de média invalide : {0:?} (relatif au dossier media/, sans '..')")]
    InvalidMediaPath(String),

    #[error("média introuvable : {0}")]
    MediaNotFound(String),

    #[error("un média nommé {0:?} existe déjà")]
    MediaAlreadyExists(String),

    #[error("type de fichier refusé : {0:?} (extensions médias uniquement)")]
    UnsupportedMediaType(String),

    #[error("fichier trop volumineux : {name} (maximum {max} octets)")]
    MediaTooLarge { name: String, max: u64 },

    #[error("preset introuvable : {0}")]
    PresetNotFound(String),

    #[error("nom de preset invalide : {0:?} (alphanumérique, '-', '_' uniquement)")]
    InvalidPresetName(String),

    #[error("erreur d'E/S sur {path} : {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("erreur de (dé)sérialisation : {0}")]
    Serde(#[from] serde_json::Error),

    #[error("config invalide : {0}")]
    Config(String),
}

impl CoreError {
    /// Helper pour envelopper une erreur d'E/S avec le chemin concerné.
    pub fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
