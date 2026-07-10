//! Interrupteurs de fonctions (V2) : activer/désactiver les services du
//! node À CHAUD depuis l'onglet « Fonctions » de l'UI web.
//!
//! Une fonction désactivée est réellement ARRÊTÉE — socket fermée, port
//! MIDI relâché, pipeline vidéo libéré, annonce mDNS retirée — pas juste
//! ignorée. Le contrôleur de services du binaire (crates/node) observe ces
//! drapeaux sur un canal `watch` et arrête/relance chaque service.
//!
//! Les choix sont persistés dans `fonctions.json` (à côté de `node.toml`)
//! et priment au démarrage sur `[modules]`/`[output]` de la config — même
//! logique que `sortie.json`. L'UI web (HTTP) n'est PAS désactivable
//! depuis elle-même : on ne se coupe pas la main qui tient l'interrupteur
//! (`[modules] http = false` reste possible dans node.toml).

use serde::{Deserialize, Serialize};

fn vrai() -> bool {
    true
}

/// L'état des interrupteurs. Tout est actif par défaut (et les champs
/// absents d'un vieux `fonctions.json` retombent sur « actif » — défauts
/// serde) : désactiver est toujours un choix explicite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureFlags {
    /// Lecteur vidéo (pipeline GStreamer ou backend simulé).
    #[serde(default = "vrai")]
    pub player: bool,
    /// Fenêtre de sortie (projection). Désactivée : fenêtre fermée à
    /// l'écran, rendu suspendu (0 % CPU/GPU), fil winit dormant.
    #[serde(default = "vrai")]
    pub output: bool,
    /// Contrôle OSC entrant (UDP).
    #[serde(default = "vrai")]
    pub osc: bool,
    /// Auto-découverte OSCQuery (serveur HTTP dédié + annonce mDNS).
    #[serde(default = "vrai")]
    pub oscquery: bool,
    /// Retour d'état OSC vers l'hôte `[osc] feedback` de la config.
    #[serde(default = "vrai")]
    pub osc_feedback: bool,
    /// Contrôle MIDI (port ouvert via les bindings de node.toml).
    #[serde(default = "vrai")]
    pub midi: bool,
    /// Parc réseau : annonce + découverte mDNS (page Système → Réseau).
    #[serde(default = "vrai")]
    pub fleet: bool,
    /// Fondus entre presets/mappings (service fader).
    #[serde(default = "vrai")]
    pub fader: bool,
    /// Aperçu de la sortie dans le Dashboard (`/api/preview.png`).
    #[serde(default = "vrai")]
    pub preview: bool,
    /// Lumières Art-Net (page Lumières). Coupé : plus aucune trame émise,
    /// socket fermée — l'édition de la console reste possible.
    #[serde(default = "vrai")]
    pub artnet: bool,
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self {
            player: true,
            output: true,
            osc: true,
            oscquery: true,
            osc_feedback: true,
            midi: true,
            fleet: true,
            fader: true,
            preview: true,
            artnet: true,
        }
    }
}

impl FeatureFlags {
    /// Relit les interrupteurs persistés (`fonctions.json`). `None` si le
    /// fichier est absent ou illisible — l'appelant retombe sur les défauts
    /// issus de la config.
    pub fn load(path: &std::path::Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persiste les interrupteurs (écriture atomique, comme `sortie.json`) :
    /// les bascules faites dans l'UI survivent au redémarrage du node.
    pub fn save(&self, path: &std::path::Path) -> Result<(), crate::CoreError> {
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .map_err(|e| crate::CoreError::io(tmp.display().to_string(), e))?;
        std::fs::rename(&tmp, path).map_err(|e| crate::CoreError::io(path.display().to_string(), e))
    }

    /// Les défauts au premier démarrage : ce que dit la config (`[modules]`,
    /// `[output]`). Un module coupé dans node.toml démarre coupé — l'UI
    /// peut ensuite le rallumer (sauf http, hors périmètre des drapeaux).
    pub fn from_config(config: &crate::NodeConfig) -> Self {
        Self {
            player: config.modules.player,
            output: config.output.enabled,
            osc: config.modules.osc,
            oscquery: config.modules.osc,
            osc_feedback: config.modules.osc && config.osc.feedback.is_some(),
            midi: config.modules.midi,
            fleet: true,
            fader: true,
            preview: true,
            artnet: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_persist_and_survive_corruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fonctions.json");

        assert_eq!(FeatureFlags::load(&path), None);

        let flags = FeatureFlags {
            osc: false,
            midi: false,
            ..FeatureFlags::default()
        };
        flags.save(&path).expect("save");
        assert_eq!(FeatureFlags::load(&path), Some(flags));
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::write(&path, b"{pas du json").expect("corrupt");
        assert_eq!(FeatureFlags::load(&path), None);
    }

    /// Un vieux fichier partiel reste chargeable : les fonctions absentes
    /// retombent sur « actif » (on n'éteint jamais par accident).
    #[test]
    fn missing_fields_default_to_enabled() {
        let flags: FeatureFlags = serde_json::from_str(r#"{"osc":false}"#).expect("de");
        assert!(!flags.osc);
        assert!(flags.player && flags.output && flags.midi && flags.fader && flags.preview);
    }

    /// Format JSON exposé par l'API `/api/features` : figé par test.
    #[test]
    fn json_format_is_stable() {
        let json = serde_json::to_string(&FeatureFlags::default()).expect("ser");
        assert_eq!(
            json,
            r#"{"player":true,"output":true,"osc":true,"oscquery":true,"osc_feedback":true,"midi":true,"fleet":true,"fader":true,"preview":true,"artnet":true}"#
        );
    }

    /// Les défauts suivent la config : un module coupé démarre coupé.
    #[test]
    fn defaults_follow_the_config() {
        let mut config = crate::NodeConfig::default();
        config.modules.osc = false;
        config.modules.midi = false;
        config.output.enabled = false;
        let flags = FeatureFlags::from_config(&config);
        assert!(!flags.osc && !flags.oscquery && !flags.osc_feedback);
        assert!(!flags.midi && !flags.output);
        assert!(flags.player && flags.fleet && flags.fader && flags.preview);
    }
}
