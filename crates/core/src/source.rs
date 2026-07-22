//! Classification des sources média — le boîtier ne lit pas que des
//! fichiers : capture HDMI/USB, flux réseau et NDI sont des sources de
//! plein droit (pitch d'origine du projet).
//!
//! Le contrat JSON ne change pas : `load` transporte toujours une chaîne.
//! C'est sa grammaire qui s'élargit :
//!
//! | Forme                  | Source                                        |
//! |------------------------|-----------------------------------------------|
//! | `clips/a.mp4`          | fichier (relatif à `media/`), vidéo ou image  |
//! | `rtsp://…` `srt://…`   | flux réseau (aussi `http(s)://`, `udp://`)    |
//! | `capture://0`          | capture locale n° 0 (webcam, carte HDMI UVC)  |
//! | `ndi://Nom de source`  | flux NDI (plugin GStreamer optionnel)         |

use crate::error::CoreError;

/// Schémas réseau que playbin sait ouvrir directement.
const NETWORK_SCHEMES: [&str; 5] = ["rtsp", "srt", "http", "https", "udp"];

/// Une source média classifiée depuis la chaîne du `load`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaSource {
    /// Fichier relatif au dossier `media/` (vidéo ou image fixe).
    File(String),
    /// URL réseau jouable telle quelle (rtsp, srt, http, https, udp).
    Network(String),
    /// Capture locale par index (`capture://0`).
    Capture(u32),
    /// Source NDI par nom (`ndi://Machine (sortie)`).
    Ndi(String),
}

impl MediaSource {
    /// Classifie et valide une chaîne de `load`. Toute la validation des
    /// sources du node passe ici : refusé ici = refusé pour TOUTES les
    /// interfaces (UI, REST, OSC, MIDI, presets).
    pub fn parse(raw: &str) -> Result<Self, CoreError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(CoreError::InvalidCommand("load : source vide".into()));
        }
        if let Some(index) = raw.strip_prefix("capture://") {
            let index = index
                .parse::<u32>()
                .map_err(|_| CoreError::InvalidMediaPath(raw.into()))?;
            return Ok(MediaSource::Capture(index));
        }
        if let Some(name) = raw.strip_prefix("ndi://") {
            // Le nom part dans un pipeline GStreamer : pas de guillemets ni
            // d'antislash, et pas de nom vide.
            if name.is_empty() || name.contains(['"', '\\', '\0']) {
                return Err(CoreError::InvalidMediaPath(raw.into()));
            }
            return Ok(MediaSource::Ndi(name.to_string()));
        }
        if let Some((scheme, rest)) = raw.split_once("://") {
            if NETWORK_SCHEMES.contains(&scheme) {
                if rest.is_empty() || raw.contains(['"', '\0']) {
                    return Err(CoreError::InvalidMediaPath(raw.into()));
                }
                return Ok(MediaSource::Network(raw.to_string()));
            }
            // Schéma inconnu (file://, ftp://…) : refusé plutôt qu'interprété
            // comme un chemin — un `file:///etc/passwd` ne doit pas passer.
            return Err(CoreError::InvalidMediaPath(raw.into()));
        }
        validate_relative_file(raw)?;
        Ok(MediaSource::File(raw.to_string()))
    }

    /// La source est-elle « live » (ni position, ni durée, ni seek) ?
    pub fn is_live(&self) -> bool {
        matches!(self, MediaSource::Capture(_) | MediaSource::Ndi(_))
    }

    /// La source peut-elle se débrancher puis revenir, méritant une reprise
    /// automatique ? Capture (webcam rebranchée), NDI (émetteur relancé) ET
    /// flux réseau (rtsp/srt/http/udp : une micro-coupure ne doit pas arrêter
    /// la lecture définitivement). Un fichier local absent, lui, ne revient
    /// pas tout seul : pas de reprise.
    pub fn reconnectable(&self) -> bool {
        matches!(
            self,
            MediaSource::Capture(_) | MediaSource::Ndi(_) | MediaSource::Network(_)
        )
    }
}

/// Valide un chemin de fichier média : relatif, canonique, sans traversée.
///
/// Accepter un chemin absolu ou `..` permettrait de lire n'importe quel
/// fichier de la machine via l'API réseau.
fn validate_relative_file(path: &str) -> Result<(), CoreError> {
    if path.contains('\0') {
        return Err(CoreError::InvalidMediaPath(path.into()));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(CoreError::InvalidMediaPath(path.into()));
    }
    // Chemin Windows absolu type `C:\...` ou `C:/...`.
    if path.as_bytes().get(1) == Some(&b':') {
        return Err(CoreError::InvalidMediaPath(path.into()));
    }
    if path.split(['/', '\\']).any(|part| part == "..") {
        return Err(CoreError::InvalidMediaPath(path.into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_parse_and_stay_relative() {
        assert_eq!(
            MediaSource::parse(" clips/a.mp4 ").expect("fichier"),
            MediaSource::File("clips/a.mp4".into())
        );
        for bad in [
            "",
            "/etc/passwd",
            "C:\\evil.mp4",
            "../secret.mp4",
            "sub/../../x.mp4",
            "nul\0.mp4",
        ] {
            assert!(
                MediaSource::parse(bad).is_err(),
                "aurait dû refuser {bad:?}"
            );
        }
    }

    #[test]
    fn network_schemes_are_whitelisted() {
        assert_eq!(
            MediaSource::parse("rtsp://10.0.0.5:8554/cam").expect("rtsp"),
            MediaSource::Network("rtsp://10.0.0.5:8554/cam".into())
        );
        assert!(matches!(
            MediaSource::parse("srt://host:9710?mode=caller"),
            Ok(MediaSource::Network(_))
        ));
        assert!(matches!(
            MediaSource::parse("https://exemple.fr/boucle.mp4"),
            Ok(MediaSource::Network(_))
        ));
        // Schémas non listés : refusés (file:// surtout).
        assert!(MediaSource::parse("file:///etc/passwd").is_err());
        assert!(MediaSource::parse("ftp://host/x.mp4").is_err());
        assert!(MediaSource::parse("rtsp://").is_err());
    }

    #[test]
    fn capture_and_ndi_parse() {
        assert_eq!(
            MediaSource::parse("capture://0").expect("capture"),
            MediaSource::Capture(0)
        );
        assert_eq!(
            MediaSource::parse("capture://2").expect("capture"),
            MediaSource::Capture(2)
        );
        assert!(MediaSource::parse("capture://webcam").is_err());
        assert_eq!(
            MediaSource::parse("ndi://Régie (sortie 1)").expect("ndi"),
            MediaSource::Ndi("Régie (sortie 1)".into())
        );
        assert!(MediaSource::parse("ndi://").is_err());
        assert!(MediaSource::parse("ndi://nom\"pipeline").is_err());

        assert!(MediaSource::Capture(0).is_live());
        assert!(MediaSource::Ndi("x".into()).is_live());
        assert!(!MediaSource::File("a.mp4".into()).is_live());
        assert!(!MediaSource::Network("rtsp://h/s".into()).is_live());
    }

    #[test]
    fn les_flux_reseau_sont_reconnectables() {
        // La reprise automatique couvre capture, NDI ET flux réseau : une
        // micro-coupure d'un rtsp/srt/http ne doit pas arrêter la lecture.
        assert!(MediaSource::Capture(0).reconnectable());
        assert!(MediaSource::Ndi("x".into()).reconnectable());
        assert!(MediaSource::Network("rtsp://cam/live".into()).reconnectable());
        assert!(MediaSource::Network("srt://h:9000".into()).reconnectable());
        // Un fichier local absent ne revient pas seul : pas de reprise.
        assert!(!MediaSource::File("a.mp4".into()).reconnectable());
    }
}
