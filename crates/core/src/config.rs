//! Configuration du node (`node.toml`).
//!
//! Précédence (pattern HPlayer3) : défauts < fichier < surcharges forcées.
//! Tout champ absent du fichier prend sa valeur par défaut → un `node.toml`
//! vide est valide, la version portable démarre sans configuration.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::error::CoreError;

/// Résolution de sortie. `auto` = résolution native de l'écran/VP détectée.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum Resolution {
    Auto,
    Fixed { width: u32, height: u32 },
}

impl Default for Resolution {
    fn default() -> Self {
        // Décision Pym 2026-07-09 : 1080p par défaut, configurable.
        Resolution::Fixed {
            width: 1920,
            height: 1080,
        }
    }
}

/// Modules activables — c'est ce qui rend l'installeur "à la carte" possible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Modules {
    pub player: bool,
    pub osc: bool,
    pub midi: bool,
    pub http: bool,
    pub sequencer: bool,
    pub sync: bool,
    pub ndi: bool,
}

impl Default for Modules {
    fn default() -> Self {
        Self {
            player: true,
            osc: true,
            midi: false,
            http: true,
            sequencer: false,
            sync: false,
            ndi: false,
        }
    }
}

/// Ports réseau des interfaces de contrôle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ports {
    /// Adresse d'écoute ("0.0.0.0" = accessible depuis le téléphone/réseau).
    pub bind: String,
    pub http: u16,
    pub osc: u16,
    /// OSCQuery : auto-découverte des paramètres OSC (Chataigne…).
    pub oscquery: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            http: 8080,
            osc: 9000,
            oscquery: 8081,
        }
    }
}

/// Comportement au démarrage (mode kiosque P1.9) : charger un preset et,
/// si demandé, lancer la lecture — le node redémarre seul en plein show.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Startup {
    /// Preset chargé au démarrage, s'il existe.
    pub preset: Option<String>,
    /// Lance la lecture après chargement du preset.
    pub autoplay: bool,
}

/// Cible d'un contrôleur continu MIDI (CC) : la valeur 0..127 est mise à
/// l'échelle des bornes du paramètre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleTarget {
    Volume,
    Brightness,
    Contrast,
    Gamma,
    Saturation,
    Hue,
    GainR,
    GainG,
    GainB,
}

/// Un binding MIDI : note ou CC → commande fixe ou paramètre continu.
///
/// ```toml
/// [[midi.bindings]]
/// note = 60                      # note-on 60 (C4)
/// command = { cmd = "play" }
///
/// [[midi.bindings]]
/// cc = 7
/// scale = "volume"               # CC7 0..127 → volume 0..1
/// ```
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiBinding {
    /// Numéro de note (note-on) déclencheuse.
    pub note: Option<u8>,
    /// Numéro de contrôleur continu (CC).
    pub cc: Option<u8>,
    /// Canal MIDI 1..=16 (absent = tous les canaux).
    pub channel: Option<u8>,
    /// Commande envoyée telle quelle (pour `note`, ou `cc` en tout-ou-rien).
    pub command: Option<Command>,
    /// Paramètre continu piloté par la valeur du CC.
    pub scale: Option<ScaleTarget>,
}

/// Réglages MIDI.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiSettings {
    /// Sous-chaîne du nom du port à ouvrir (absent = premier port trouvé).
    pub port: Option<String>,
    pub bindings: Vec<MidiBinding>,
}

/// Fenêtre de sortie (rendu). Tant que le backend vidéo n'est pas branché,
/// elle affiche les mires de test warpées : le calibrage projecteur est déjà
/// possible. La vidéo remplacera la mire dans la même fenêtre.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Output {
    /// Ouvre la fenêtre de sortie au démarrage du node.
    pub enabled: bool,
    /// Écran cible, par index (0 = premier). La liste des écrans détectés est
    /// tracée au démarrage (visible dans la page Logs).
    pub monitor: usize,
    /// Plein écran sans bordure sur l'écran cible. F11 bascule à chaud,
    /// Échap quitte le plein écran.
    pub fullscreen: bool,
    /// Rendu par la carte graphique (Vulkan/GL). En cas d'échec (pilote
    /// absent, VM…), repli automatique sur le rendu CPU — `false` force le
    /// CPU d'emblée.
    pub gpu: bool,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            enabled: true,
            monitor: 0,
            fullscreen: false,
            gpu: true,
        }
    }
}

/// Réglages OSC au-delà du port d'écoute.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OscSettings {
    /// Destination du retour d'état (`"10.0.0.5:9001"`) : chaque changement
    /// (volume, coins, couleur…) est renvoyé en OSC à cette adresse — les
    /// curseurs de Chataigne suivent le node. Absent = pas de feedback.
    pub feedback: Option<String>,
}

/// Rôle du node dans la synchronisation multi-machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRole {
    /// Pas de synchro (défaut).
    #[default]
    Aucun,
    /// Ce node publie son horloge de lecture aux suiveurs.
    Maitre,
    /// Ce node se cale sur le maître (`[sync] maitre = "ip:port"`).
    Suiveur,
}

/// Synchronisation multi-node niveau 2 : les suiveurs se verrouillent sur
/// la position du maître (micro-ajustements de vitesse, resync dur au-delà
/// du seuil). Les suiveurs s'annoncent d'eux-mêmes au maître : rien à
/// configurer côté maître à part le rôle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSettings {
    pub role: SyncRole,
    /// Adresse du maître (`"10.0.0.2:9010"`) — suiveurs uniquement.
    pub maitre: Option<String>,
    /// Port UDP de l'horloge (écoute côté maître ET côté suiveur).
    pub port: u16,
    /// Dérive tolérée avant resync dur (au-delà : seek immédiat). En deçà,
    /// la vitesse est micro-ajustée (±3 % max) — invisible à l'œil.
    pub tolerance_ms: u64,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            role: SyncRole::Aucun,
            maitre: None,
            port: 9010,
            tolerance_ms: 80,
        }
    }
}

/// Sécurité de l'interface web (P4.4). Sans mot de passe : réseau local de
/// confiance, comportement historique.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Security {
    /// Mot de passe de l'UI web et de l'API (HTTP Basic, tout identifiant
    /// accepté). Absent = pas d'authentification. L'OSC (UDP) et l'OSCQuery
    /// restent ouverts : à réserver au réseau local ou à Tailscale.
    pub password: Option<String>,
}

/// Bornes de ressources — un node de spectacle ne doit jamais saturer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Taille maximale d'un upload de média, en Mo.
    pub max_upload_mb: u64,
    /// Nombre d'entrées gardées par la page de logs.
    pub log_buffer: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_upload_mb: 2048,
            log_buffer: 1000,
        }
    }
}

/// Chemins des données. Relatifs au dossier de travail → portable par défaut.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Paths {
    pub media: PathBuf,
    pub presets: PathBuf,
    pub shaders: PathBuf,
    pub logs: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            media: PathBuf::from("media"),
            presets: PathBuf::from("presets"),
            shaders: PathBuf::from("shaders"),
            logs: PathBuf::from("logs"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// Nom du node sur le réseau (mDNS, page fleet). Défaut : hostname.
    pub name: Option<String>,
    pub resolution: Resolution,
    pub modules: Modules,
    pub ports: Ports,
    pub paths: Paths,
    pub startup: Startup,
    pub output: Output,
    pub security: Security,
    pub osc: OscSettings,
    pub sync: SyncSettings,
    pub limits: Limits,
    pub midi: MidiSettings,
}

impl NodeConfig {
    /// Charge la config depuis un fichier TOML. Fichier absent = défauts
    /// (cas nominal de la version portable au premier lancement).
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| CoreError::io(path.display().to_string(), e))?;
        toml::from_str(&text).map_err(|e| CoreError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = NodeConfig::load(Path::new("/nonexistent/node.toml")).expect("load");
        assert_eq!(cfg, NodeConfig::default());
        assert_eq!(cfg.ports.http, 8080);
        assert!(cfg.modules.player);
        assert!(!cfg.modules.ndi);
    }

    #[test]
    fn partial_file_fills_defaults() {
        let toml = r#"
            name = "vp-01"

            [resolution]
            mode = "fixed"
            width = 1280
            height = 720

            [modules]
            midi = true
        "#;
        let cfg: NodeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.name.as_deref(), Some("vp-01"));
        assert_eq!(
            cfg.resolution,
            Resolution::Fixed {
                width: 1280,
                height: 720
            }
        );
        // midi surchargé, le reste par défaut
        assert!(cfg.modules.midi);
        assert!(cfg.modules.player);
        assert_eq!(cfg.ports.osc, 9000);
    }

    #[test]
    fn invalid_toml_is_a_config_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node.toml");
        std::fs::write(&path, "ceci n'est pas du toml [[[").expect("write");
        assert!(matches!(NodeConfig::load(&path), Err(CoreError::Config(_))));
    }

    #[test]
    fn resolution_auto_parses() {
        let cfg: NodeConfig = toml::from_str("[resolution]\nmode = \"auto\"").expect("parse");
        assert_eq!(cfg.resolution, Resolution::Auto);
    }

    #[test]
    fn midi_bindings_parse_from_toml() {
        let cfg: NodeConfig = toml::from_str(
            r#"
            [midi]
            port = "APC"

            [[midi.bindings]]
            note = 60
            command = { cmd = "play" }

            [[midi.bindings]]
            cc = 7
            scale = "volume"

            [[midi.bindings]]
            note = 61
            channel = 10
            command = { cmd = "set_loop", mode = "all" }
            "#,
        )
        .expect("parse");
        assert_eq!(cfg.midi.port.as_deref(), Some("APC"));
        assert_eq!(cfg.midi.bindings.len(), 3);
        assert_eq!(cfg.midi.bindings[0].note, Some(60));
        assert_eq!(cfg.midi.bindings[0].command, Some(Command::Play));
        assert_eq!(cfg.midi.bindings[1].cc, Some(7));
        assert_eq!(cfg.midi.bindings[1].scale, Some(ScaleTarget::Volume));
        assert_eq!(cfg.midi.bindings[2].channel, Some(10));
    }

    #[test]
    fn output_parses_with_defaults() {
        // Absent : fenêtre activée, premier écran, fenêtré.
        let cfg: NodeConfig = toml::from_str("").expect("parse");
        assert!(cfg.output.enabled);
        assert_eq!(cfg.output.monitor, 0);
        assert!(!cfg.output.fullscreen);
        assert!(cfg.output.gpu, "GPU par défaut");

        let cfg: NodeConfig =
            toml::from_str("[output]\nmonitor = 1\nfullscreen = true\ngpu = false").expect("parse");
        assert!(cfg.output.enabled);
        assert_eq!(cfg.output.monitor, 1);
        assert!(cfg.output.fullscreen);
        assert!(!cfg.output.gpu);
    }

    #[test]
    fn startup_and_limits_parse_with_defaults() {
        let cfg: NodeConfig = toml::from_str(
            "[startup]\npreset = \"show\"\nautoplay = true\n\n[limits]\nmax_upload_mb = 100",
        )
        .expect("parse");
        assert_eq!(cfg.startup.preset.as_deref(), Some("show"));
        assert!(cfg.startup.autoplay);
        assert_eq!(cfg.limits.max_upload_mb, 100);
        assert_eq!(cfg.limits.log_buffer, 1000);
        assert_eq!(cfg.ports.bind, "0.0.0.0");
    }
}
