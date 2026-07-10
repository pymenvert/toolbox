//! Vocabulaire de la sortie vidéo (fenêtre de rendu) partagé entre la
//! fenêtre (qui détecte les écrans et applique les réglages) et l'API HTTP
//! (qui les expose à l'UI web).
//!
//! Ces réglages sont propres à la MACHINE (quel écran physique, plein écran)
//! et ne font donc PAS partie de [`crate::NodeState`] : un preset chargé sur
//! un autre node ne doit pas déplacer sa fenêtre. Ils transitent par des
//! canaux `watch` dédiés, câblés dans le binaire du node. Les changements à
//! chaud ne sont pas persistés : `node.toml` reste la source au démarrage.

use serde::{Deserialize, Serialize};

/// Un écran physique détecté par la fenêtre de sortie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Index stable pendant la session (0 = premier détecté).
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// Réglages appliqués par la fenêtre de sortie (modifiables à chaud).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OutputSettings {
    /// Écran cible, par index dans la liste détectée.
    pub monitor: usize,
    /// Plein écran sans bordure sur l'écran cible.
    pub fullscreen: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Formats JSON exposés par l'API `/api/outputs` : figés par test.
    #[test]
    fn json_formats_are_stable() {
        let monitor = MonitorInfo {
            index: 1,
            name: "HDMI-1".into(),
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            serde_json::to_string(&monitor).expect("ser"),
            r#"{"index":1,"name":"HDMI-1","width":1920,"height":1080}"#
        );
        let settings = OutputSettings {
            monitor: 1,
            fullscreen: true,
        };
        let json = serde_json::to_string(&settings).expect("ser");
        assert_eq!(json, r#"{"monitor":1,"fullscreen":true}"#);
        let back: OutputSettings = serde_json::from_str(&json).expect("de");
        assert_eq!(back, settings);
    }
}
