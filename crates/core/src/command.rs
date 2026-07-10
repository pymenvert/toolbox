//! Le vocabulaire de commandes du node.
//!
//! Chaque commande a une représentation JSON canonique (utilisée par le
//! WebSocket/REST) et une adresse OSC équivalente documentée ci-dessous.
//! Le mapping OSC↔Command vit dans `control-osc` ; ici on ne définit que le
//! vocabulaire, unique pour toutes les interfaces.

use serde::{Deserialize, Serialize};

use crate::state::{EffectParam, LoopMode};

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
    /// Gain du canal rouge, 0.0..=2.0, neutre 1.0
    GainR,
    /// Gain du canal vert, 0.0..=2.0, neutre 1.0
    GainG,
    /// Gain du canal bleu, 0.0..=2.0, neutre 1.0
    GainB,
}

/// Mire de test intégrée (réglage projecteur sans média).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestPattern {
    /// Grille de convergence.
    Grid,
    /// Damier.
    Checker,
    /// Numéros de coins (0=HG, 1=HD, 2=BD, 3=BG) pour identifier le mapping.
    Corners,
}

/// Une commande adressée au node.
///
/// | JSON (`cmd`)       | OSC équivalent               |
/// |--------------------|------------------------------|
/// | `play`             | `/play`                      |
/// | `pause`            | `/pause`                     |
/// | `stop`             | `/stop`                      |
/// | `seek`             | `/seek <f64 s>`              |
/// | `load`             | `/load <path>`               |
/// | `set_loop`         | `/loop <off|one|all>`        |
/// | `set_volume`       | `/volume <f32 0..1>`         |
/// | `set_rate`         | `/rate <f32 0.25..4>`        |
/// | `playlist_set`     | `/playlist/set <p1> <p2> …`  |
/// | `playlist_go`      | `/playlist/go <index>`       |
/// | `playlist_next`    | `/playlist/next`             |
/// | `playlist_prev`    | `/playlist/prev`             |
/// | `corner_set`       | `/corner/<i> <x> <y>`        |
/// | `set_rotation`     | `/rotation <0|90|180|270>`   |
/// | `set_flip`         | `/flip <h 0|1> <v 0|1>`      |
/// | `set_crop`         | `/crop <l> <t> <r> <b>`      |
/// | `color_set`        | `/color/<param> <f32>`       |
/// | `effect_set`       | `/effect/<param> <f32 0..1>` |
/// | `mapping_reset`    | `/mapping/reset`             |
/// | `set_mapping_enabled` | `/mapping/enabled <0|1>`  |
/// | `mapping_save`     | `/mapping/save <name>`       |
/// | `mapping_load`     | `/mapping/load <name>`       |
/// | `mapping_fade`     | `/mapping/fade <name> <s>`   |
/// | `set_test_pattern` | `/pattern <name|off>`        |
/// | `preset_save`      | `/preset/save <name>`        |
/// | `preset_load`      | `/preset/load <name>`        |
/// | `preset_fade`      | `/preset/fade <name> <s>`    |
/// | `sync_arm`         | `/sync/arm`                  |
/// | `sync_start_at`    | `/sync/startAt <unix f64>`   |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Play,
    Pause,
    Stop,
    Seek {
        seconds: f64,
    },
    Load {
        path: String,
    },
    SetLoop {
        mode: LoopMode,
    },
    SetVolume {
        volume: f32,
    },
    /// Vitesse de lecture (1.0 = normale, 0.25..=4.0). Utilisée aussi par la
    /// synchro multi-node pour rattraper la dérive en douceur.
    SetRate {
        rate: f32,
    },
    /// Remplace la playlist entière (liste vide = effacer). Ne charge rien :
    /// enchaîner avec `playlist_go` pour démarrer.
    PlaylistSet {
        items: Vec<String>,
    },
    /// Charge l'élément `index` de la playlist.
    PlaylistGo {
        index: usize,
    },
    PlaylistNext,
    PlaylistPrev,
    /// Déplace un coin du mapping. `index` ∈ 0..=3 (0=HG, 1=HD, 2=BD, 3=BG),
    /// coordonnées normalisées 0.0..=1.0 dans l'espace de sortie.
    CornerSet {
        index: u8,
        x: f32,
        y: f32,
    },
    /// Rotation de la source avant warp. `degrees` ∈ {0, 90, 180, 270}.
    SetRotation {
        degrees: u16,
    },
    /// Miroir horizontal/vertical de la source avant warp.
    SetFlip {
        horizontal: bool,
        vertical: bool,
    },
    /// Recadre la source : fraction rognée sur chaque bord (0.0..=0.45).
    SetCrop {
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    },
    ColorSet {
        param: ColorParam,
        value: f32,
    },
    /// Intensité d'un effet (0..1, 0 = inactif). OSC : `/effect/<param>`.
    EffectSet {
        param: EffectParam,
        value: f32,
    },
    MappingReset,
    /// Active/désactive le mapping sans perdre les réglages (bypass).
    SetMappingEnabled {
        enabled: bool,
    },
    /// Sauvegarde le mapping seul (coins, rotation, miroirs, recadrage) sous
    /// `name` — indépendant des presets d'état complet.
    MappingSave {
        name: String,
    },
    /// Remplace le mapping courant par le preset de mapping `name`.
    /// La lecture en cours n'est pas interrompue.
    MappingLoad {
        name: String,
    },
    /// Fondu vers le preset de mapping `name` en `seconds` secondes : coins
    /// et recadrage glissent, rotation/miroirs/bypass basculent à la fin.
    /// Couleur, effets, volume et lecture ne bougent pas.
    MappingFade {
        name: String,
        seconds: f32,
    },
    /// Affiche une mire de test (`None` = retour au média).
    SetTestPattern {
        pattern: Option<TestPattern>,
    },
    PresetSave {
        name: String,
    },
    PresetLoad {
        name: String,
    },
    /// Fondu vers le preset `name` en `seconds` secondes : coins, recadrage,
    /// couleur, effets et volume glissent en continu ; rotation, miroirs,
    /// mire et bypass basculent à la fin. Le média et la lecture en cours ne
    /// sont PAS touchés (un fondu n'interrompt jamais le show).
    PresetFade {
        name: String,
        seconds: f32,
    },
    /// Arme la synchro multi-node : média prêt, position 0, en pause.
    /// (`/sync/arm` — envoyé à tous les nodes avant un départ commun.)
    SyncArm,
    /// Départ synchronisé : lecture à l'heure Unix `at` (secondes, horloge
    /// système — les nodes doivent partager NTP). Passée → départ immédiat.
    SyncStartAt {
        at: f64,
    },
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
            Command::SetLoop {
                mode: LoopMode::All,
            },
            Command::SetVolume { volume: 0.8 },
            Command::Load {
                path: "media/foo.mp4".into(),
            },
            Command::PlaylistSet {
                items: vec!["a.mp4".into(), "b.mp4".into()],
            },
            Command::PlaylistGo { index: 1 },
            Command::PlaylistNext,
            Command::PlaylistPrev,
            Command::SetCrop {
                left: 0.1,
                top: 0.0,
                right: 0.05,
                bottom: 0.0,
            },
            Command::ColorSet {
                param: ColorParam::Gamma,
                value: 1.2,
            },
            Command::SetRotation { degrees: 270 },
            Command::SetFlip {
                horizontal: true,
                vertical: false,
            },
            Command::SetTestPattern {
                pattern: Some(TestPattern::Corners),
            },
            Command::SetTestPattern { pattern: None },
            Command::PresetLoad {
                name: "scene_01".into(),
            },
            Command::SetMappingEnabled { enabled: false },
            Command::MappingSave {
                name: "salon".into(),
            },
            Command::MappingLoad {
                name: "salon".into(),
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

    /// Formats JSON des nouvelles commandes V1 : contrat public, figé par test.
    #[test]
    fn new_commands_json_format_is_stable() {
        let cases = [
            (
                serde_json::to_string(&Command::SetRotation { degrees: 90 }).expect("ser"),
                r#"{"cmd":"set_rotation","degrees":90}"#,
            ),
            (
                serde_json::to_string(&Command::SetFlip {
                    horizontal: true,
                    vertical: false,
                })
                .expect("ser"),
                r#"{"cmd":"set_flip","horizontal":true,"vertical":false}"#,
            ),
            (
                serde_json::to_string(&Command::SetTestPattern {
                    pattern: Some(TestPattern::Grid),
                })
                .expect("ser"),
                r#"{"cmd":"set_test_pattern","pattern":"grid"}"#,
            ),
            (
                serde_json::to_string(&Command::SetTestPattern { pattern: None }).expect("ser"),
                r#"{"cmd":"set_test_pattern","pattern":null}"#,
            ),
            (
                serde_json::to_string(&Command::SetLoop {
                    mode: LoopMode::One,
                })
                .expect("ser"),
                r#"{"cmd":"set_loop","mode":"one"}"#,
            ),
            (
                serde_json::to_string(&Command::PlaylistSet {
                    items: vec!["a.mp4".into()],
                })
                .expect("ser"),
                r#"{"cmd":"playlist_set","items":["a.mp4"]}"#,
            ),
            (
                serde_json::to_string(&Command::PlaylistNext).expect("ser"),
                r#"{"cmd":"playlist_next"}"#,
            ),
            (
                serde_json::to_string(&Command::ColorSet {
                    param: ColorParam::GainR,
                    value: 1.5,
                })
                .expect("ser"),
                r#"{"cmd":"color_set","param":"gain_r","value":1.5}"#,
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }

    /// Formats JSON des commandes de mapping (toggle + presets dédiés) :
    /// contrat public, figé par test.
    #[test]
    fn mapping_commands_json_format_is_stable() {
        let cases = [
            (
                serde_json::to_string(&Command::SetMappingEnabled { enabled: false }).expect("ser"),
                r#"{"cmd":"set_mapping_enabled","enabled":false}"#,
            ),
            (
                serde_json::to_string(&Command::MappingSave {
                    name: "salon".into(),
                })
                .expect("ser"),
                r#"{"cmd":"mapping_save","name":"salon"}"#,
            ),
            (
                serde_json::to_string(&Command::MappingLoad {
                    name: "salon".into(),
                })
                .expect("ser"),
                r#"{"cmd":"mapping_load","name":"salon"}"#,
            ),
            (
                serde_json::to_string(&Command::SyncArm).expect("ser"),
                r#"{"cmd":"sync_arm"}"#,
            ),
            (
                serde_json::to_string(&Command::SyncStartAt { at: 1234.5 }).expect("ser"),
                r#"{"cmd":"sync_start_at","at":1234.5}"#,
            ),
            (
                serde_json::to_string(&Command::EffectSet {
                    param: EffectParam::Pixelate,
                    value: 0.5,
                })
                .expect("ser"),
                r#"{"cmd":"effect_set","param":"pixelate","value":0.5}"#,
            ),
            (
                serde_json::to_string(&Command::PresetFade {
                    name: "scene_02".into(),
                    seconds: 2.5,
                })
                .expect("ser"),
                r#"{"cmd":"preset_fade","name":"scene_02","seconds":2.5}"#,
            ),
            (
                serde_json::to_string(&Command::MappingFade {
                    name: "salon".into(),
                    seconds: 1.5,
                })
                .expect("ser"),
                r#"{"cmd":"mapping_fade","name":"salon","seconds":1.5}"#,
            ),
            (
                serde_json::to_string(&Command::SetRate { rate: 1.25 }).expect("ser"),
                r#"{"cmd":"set_rate","rate":1.25}"#,
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }
}
