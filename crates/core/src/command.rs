//! Le vocabulaire de commandes du node.
//!
//! Chaque commande a une représentation JSON canonique (utilisée par le
//! WebSocket/REST) et une adresse OSC équivalente documentée ci-dessous.
//! Le mapping OSC↔Command vit dans `control-osc` ; ici on ne définit que le
//! vocabulaire, unique pour toutes les interfaces.

use serde::{Deserialize, Serialize};

/// Paramètre de correction couleur (tous normalisés, voir bornes dans `state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorParam {
    /// 0.0..=2.0, neutre 1.0
    Brightness,
    /// 0.0..=2.0, neutre 1.0
    Contrast,
    /// 0.2..=4.0, neutre 1.0
    Gamma,
    /// 0.0..=2.0, neutre 1.0
    Saturation,
    /// -180.0..=180.0 degrés, neutre 0.0
    Hue,
}

/// Une commande adressée au node.
///
/// | JSON (`cmd`)    | OSC équivalent            |
/// |-----------------|---------------------------|
/// | `play`          | `/play`                   |
/// | `pause`         | `/pause`                  |
/// | `stop`          | `/stop`                   |
/// | `seek`          | `/seek <f64 s>`           |
/// | `load`          | `/load <path>`            |
/// | `set_loop`      | `/loop <0|1>`             |
/// | `set_volume`    | `/volume <f32 0..1>`      |
/// | `corner_set`    | `/corner/<i> <x> <y>`     |
/// | `color_set`     | `/color/<param> <f32>`    |
/// | `mapping_reset` | `/mapping/reset`          |
/// | `preset_save`   | `/preset/save <name>`     |
/// | `preset_load`   | `/preset/load <name>`     |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Play,
    Pause,
    Stop,
    Seek { seconds: f64 },
    Load { path: String },
    SetLoop { enabled: bool },
    SetVolume { volume: f32 },
    /// Déplace un coin du mapping. `index` ∈ 0..=3 (0=HG, 1=HD, 2=BD, 3=BG),
    /// coordonnées normalisées 0.0..=1.0 dans l'espace de sortie.
    CornerSet { index: u8, x: f32, y: f32 },
    ColorSet { param: ColorParam, value: f32 },
    MappingReset,
    PresetSave { name: String },
    PresetLoad { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le format JSON est un contrat public (web UI, REST) : on le fige par test.
    #[test]
    fn json_format_is_stable() {
        let cmd = Command::CornerSet {
            index: 2,
            x: 0.95,
            y: 1.0,
        };
        let json = serde_json::to_string(&cmd).expect("serialize");
        assert_eq!(json, r#"{"cmd":"corner_set","index":2,"x":0.95,"y":1.0}"#);

        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cmd);
    }

    #[test]
    fn simple_commands_roundtrip() {
        for cmd in [
            Command::Play,
            Command::Pause,
            Command::Stop,
            Command::MappingReset,
            Command::Seek { seconds: 12.5 },
            Command::SetLoop { enabled: true },
            Command::SetVolume { volume: 0.8 },
            Command::Load {
                path: "media/foo.mp4".into(),
            },
            Command::ColorSet {
                param: ColorParam::Gamma,
                value: 1.2,
            },
            Command::PresetLoad {
                name: "scene_01".into(),
            },
        ] {
            let json = serde_json::to_string(&cmd).expect("serialize");
            let back: Command = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, cmd, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn unknown_command_is_rejected() {
        let res: Result<Command, _> = serde_json::from_str(r#"{"cmd":"self_destruct"}"#);
        assert!(res.is_err());
    }
}
