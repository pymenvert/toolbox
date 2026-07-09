//! Configuration du node (`node.toml`).
//!
//! Précédence (pattern HPlayer3) : défauts < fichier < surcharges forcées.
//! Tout champ absent du fichier prend sa valeur par défaut → un `node.toml`
//! vide est valide, la version portable démarre sans configuration.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    pub http: u16,
    pub osc: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Self {
            http: 8080,
            osc: 9000,
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// Nom du node sur le réseau (mDNS, page fleet). Défaut : hostname.
    pub name: Option<String>,
    pub resolution: Resolution,
    pub modules: Modules,
    pub ports: Ports,
    pub paths: Paths,
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
        assert!(matches!(
            NodeConfig::load(&path),
            Err(CoreError::Config(_))
        ));
    }

    #[test]
    fn resolution_auto_parses() {
        let cfg: NodeConfig = toml::from_str("[resolution]\nmode = \"auto\"").expect("parse");
        assert_eq!(cfg.resolution, Resolution::Auto);
    }
}
