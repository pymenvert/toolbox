//! État du node : ce que le moteur rend à l'écran, entièrement sérialisable.
//!
//! Règles :
//! - Toute mutation passe par [`NodeState::apply`] qui **valide** puis publie
//!   un [`Event`]. Aucune écriture directe des champs depuis l'extérieur.
//! - L'état complet est un document JSON → presets, export, clonage gratuits.

use serde::{Deserialize, Serialize};

use crate::command::{ColorParam, Command};
use crate::error::CoreError;

/// Un coin du quad de mapping, coordonnées normalisées (0,0 = haut-gauche).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Corner {
    pub x: f32,
    pub y: f32,
}

/// Mapping 4 coins. Ordre : 0=HG, 1=HD, 2=BD, 3=BG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MappingState {
    pub corners: [Corner; 4],
}

impl Default for MappingState {
    fn default() -> Self {
        Self {
            corners: [
                Corner { x: 0.0, y: 0.0 },
                Corner { x: 1.0, y: 0.0 },
                Corner { x: 1.0, y: 1.0 },
                Corner { x: 0.0, y: 1.0 },
            ],
        }
    }
}

/// Correction couleur. Valeurs neutres par défaut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorState {
    pub brightness: f32,
    pub contrast: f32,
    pub gamma: f32,
    pub saturation: f32,
    pub hue: f32,
}

impl Default for ColorState {
    fn default() -> Self {
        Self {
            brightness: 1.0,
            contrast: 1.0,
            gamma: 1.0,
            saturation: 1.0,
            hue: 0.0,
        }
    }
}

/// Bornes autorisées par paramètre couleur : (min, max).
fn color_bounds(param: ColorParam) -> (f32, f32) {
    match param {
        ColorParam::Brightness | ColorParam::Contrast | ColorParam::Saturation => (0.0, 2.0),
        ColorParam::Gamma => (0.2, 4.0),
        ColorParam::Hue => (-180.0, 180.0),
    }
}

/// État de transport du player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    pub transport: Transport,
    /// Média chargé (chemin relatif au dossier `media/`), s'il y en a un.
    pub media: Option<String>,
    /// Lecture en boucle.
    pub looping: bool,
    /// Volume 0.0..=1.0.
    pub volume: f32,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            transport: Transport::default(),
            media: None,
            looping: false,
            // Piège terrain évité : un node fraîchement démarré ne doit pas
            // jouer en silence. Le défaut est plein volume, pas muet.
            volume: 1.0,
        }
    }
}

/// L'état complet du node — LE document que l'on preset/exporte/clone.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NodeState {
    pub player: PlayerState,
    pub mapping: MappingState,
    pub color: ColorState,
}

/// Événement publié après chaque mutation réussie. Les abonnés (web UI, OSC
/// feedback, moteur de rendu, page de logs) reçoivent tous le même flux.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    TransportChanged { transport: Transport },
    MediaLoaded { path: String },
    Seeked { seconds: f64 },
    LoopChanged { enabled: bool },
    VolumeChanged { volume: f32 },
    CornerMoved { index: u8, x: f32, y: f32 },
    ColorChanged { param: ColorParam, value: f32 },
    MappingReset,
    PresetSaved { name: String },
    PresetLoaded { name: String },
}

impl NodeState {
    /// Applique une commande : valide, mute l'état, retourne l'événement.
    ///
    /// Les commandes de preset ne sont PAS traitées ici (elles touchent le
    /// disque) : c'est le rôle du service `preset` branché sur le bus. Les
    /// recevoir ici est une erreur de câblage.
    pub fn apply(&mut self, command: &Command) -> Result<Event, CoreError> {
        match command {
            Command::Play => {
                if self.player.media.is_none() {
                    return Err(CoreError::InvalidCommand("play sans média chargé".into()));
                }
                self.player.transport = Transport::Playing;
                Ok(Event::TransportChanged {
                    transport: Transport::Playing,
                })
            }
            Command::Pause => {
                self.player.transport = Transport::Paused;
                Ok(Event::TransportChanged {
                    transport: Transport::Paused,
                })
            }
            Command::Stop => {
                self.player.transport = Transport::Stopped;
                Ok(Event::TransportChanged {
                    transport: Transport::Stopped,
                })
            }
            Command::Seek { seconds } => {
                if self.player.media.is_none() {
                    return Err(CoreError::InvalidCommand("seek sans média chargé".into()));
                }
                if !seconds.is_finite() || *seconds < 0.0 {
                    return Err(CoreError::OutOfRange {
                        param: "seek.seconds",
                        value: *seconds,
                        min: 0.0,
                        max: f64::MAX,
                    });
                }
                Ok(Event::Seeked { seconds: *seconds })
            }
            Command::Load { path } => {
                if path.trim().is_empty() {
                    return Err(CoreError::InvalidCommand("load: chemin vide".into()));
                }
                self.player.media = Some(path.clone());
                Ok(Event::MediaLoaded { path: path.clone() })
            }
            Command::SetLoop { enabled } => {
                self.player.looping = *enabled;
                Ok(Event::LoopChanged { enabled: *enabled })
            }
            Command::SetVolume { volume } => {
                check_range("volume", *volume, 0.0, 1.0)?;
                self.player.volume = *volume;
                Ok(Event::VolumeChanged { volume: *volume })
            }
            Command::CornerSet { index, x, y } => {
                let i = usize::from(*index);
                if i >= 4 {
                    return Err(CoreError::InvalidCorner(*index));
                }
                // Marge de 0.5 hors cadre autorisée : utile pour "tirer" un coin
                // au-delà du bord physique du projecteur.
                check_range("corner.x", *x, -0.5, 1.5)?;
                check_range("corner.y", *y, -0.5, 1.5)?;
                self.mapping.corners[i] = Corner { x: *x, y: *y };
                Ok(Event::CornerMoved {
                    index: *index,
                    x: *x,
                    y: *y,
                })
            }
            Command::ColorSet { param, value } => {
                let (min, max) = color_bounds(*param);
                check_range("color", *value, min, max)?;
                match param {
                    ColorParam::Brightness => self.color.brightness = *value,
                    ColorParam::Contrast => self.color.contrast = *value,
                    ColorParam::Gamma => self.color.gamma = *value,
                    ColorParam::Saturation => self.color.saturation = *value,
                    ColorParam::Hue => self.color.hue = *value,
                }
                Ok(Event::ColorChanged {
                    param: *param,
                    value: *value,
                })
            }
            Command::MappingReset => {
                self.mapping = MappingState::default();
                Ok(Event::MappingReset)
            }
            Command::PresetSave { .. } | Command::PresetLoad { .. } => {
                Err(CoreError::InvalidCommand(
                    "les presets sont gérés par le service preset, pas par l'état".into(),
                ))
            }
        }
    }
}

fn check_range(param: &'static str, value: f32, min: f32, max: f32) -> Result<(), CoreError> {
    if !value.is_finite() || value < min || value > max {
        return Err(CoreError::OutOfRange {
            param,
            value: f64::from(value),
            min: f64::from(min),
            max: f64::from(max),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_without_media_is_rejected() {
        let mut s = NodeState::default();
        assert!(s.apply(&Command::Play).is_err());
        assert_eq!(s.player.transport, Transport::Stopped);
    }

    #[test]
    fn load_then_play() {
        let mut s = NodeState::default();
        s.apply(&Command::Load {
            path: "media/a.mp4".into(),
        })
        .expect("load");
        let ev = s.apply(&Command::Play).expect("play");
        assert_eq!(
            ev,
            Event::TransportChanged {
                transport: Transport::Playing
            }
        );
    }

    #[test]
    fn corner_validation() {
        let mut s = NodeState::default();
        // index hors bornes
        assert!(matches!(
            s.apply(&Command::CornerSet {
                index: 4,
                x: 0.5,
                y: 0.5
            }),
            Err(CoreError::InvalidCorner(4))
        ));
        // NaN refusé
        assert!(s
            .apply(&Command::CornerSet {
                index: 0,
                x: f32::NAN,
                y: 0.5
            })
            .is_err());
        // marge hors cadre OK
        s.apply(&Command::CornerSet {
            index: 0,
            x: -0.2,
            y: 1.3,
        })
        .expect("corner in margin");
        assert_eq!(s.mapping.corners[0], Corner { x: -0.2, y: 1.3 });
        // état inchangé sur erreur
        let before = s.clone();
        assert!(s
            .apply(&Command::CornerSet {
                index: 1,
                x: 9.0,
                y: 0.0
            })
            .is_err());
        assert_eq!(s, before);
    }

    #[test]
    fn color_bounds_enforced() {
        let mut s = NodeState::default();
        assert!(s
            .apply(&Command::ColorSet {
                param: ColorParam::Gamma,
                value: 0.0
            })
            .is_err());
        s.apply(&Command::ColorSet {
            param: ColorParam::Hue,
            value: -90.0,
        })
        .expect("hue");
        assert_eq!(s.color.hue, -90.0);
    }

    #[test]
    fn state_json_roundtrip() {
        let mut s = NodeState::default();
        s.apply(&Command::Load {
            path: "media/a.mp4".into(),
        })
        .expect("load");
        s.apply(&Command::CornerSet {
            index: 2,
            x: 0.9,
            y: 0.95,
        })
        .expect("corner");
        let json = serde_json::to_string_pretty(&s).expect("serialize");
        let back: NodeState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }

    #[test]
    fn presets_rejected_by_state() {
        let mut s = NodeState::default();
        assert!(s.apply(&Command::PresetSave { name: "x".into() }).is_err());
    }
}
