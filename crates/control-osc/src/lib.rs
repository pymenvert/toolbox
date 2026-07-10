//! # toolbox-control-osc (P1.6)
//!
//! Écoute UDP/OSC et traduit les messages en [`Command`] sur le bus — le
//! MÊME vocabulaire que le REST/WebSocket (voir la table dans
//! `toolbox_core::command`). Pensé pour Chataigne : tolérant sur les types
//! d'arguments (int/float/double/bool interchangeables quand c'est sans
//! ambiguïté).
//!
//! Adresses reconnues :
//! `/play` `/pause` `/stop` `/seek s` `/load chemin` `/loop off|one|all|0|1|2`
//! `/volume f` `/playlist/set p1 p2 …` `/playlist/go i` `/playlist/next`
//! `/playlist/prev` `/corner/<0-3> x y` `/rotation 0|90|180|270`
//! `/flip h v` `/crop l t r b` `/color/<param> f` `/mapping/reset`
//! `/mapping/enabled 0|1` `/mapping/save nom` `/mapping/load nom`
//! `/pattern grid|checker|corners|off` `/preset/save nom` `/preset/load nom`
//!
//! Un message invalide est tracé (visible dans la page de logs) et ignoré :
//! l'OSC ne plante jamais le node.

use rosc::{OscMessage, OscPacket, OscType};
use thiserror::Error;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use toolbox_core::{BusHandle, ColorParam, Command, LoopMode, Source, TestPattern};

#[derive(Debug, Error)]
pub enum OscError {
    #[error("impossible d'écouter en UDP sur {addr} : {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
}

/// Erreur de traduction d'un message OSC (tracée, jamais fatale).
#[derive(Debug, Error, PartialEq)]
pub enum MapError {
    #[error("adresse OSC inconnue : {0}")]
    UnknownAddress(String),
    #[error("arguments invalides pour {addr} : {detail}")]
    BadArguments { addr: String, detail: String },
}

/// Configuration du serveur OSC.
#[derive(Debug, Clone)]
pub struct OscConfig {
    pub bind: String,
    pub port: u16,
}

/// Boucle du service OSC : reçoit, traduit, envoie sur le bus.
pub async fn serve(
    config: OscConfig,
    bus: BusHandle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), OscError> {
    let addr = format!("{}:{}", config.bind, config.port);
    let socket = tokio::net::UdpSocket::bind(&addr)
        .await
        .map_err(|source| OscError::Bind {
            addr: addr.clone(),
            source,
        })?;
    info!(%addr, "OSC démarré (UDP)");
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            received = socket.recv_from(&mut buf) => {
                match received {
                    Ok((len, from)) => {
                        match rosc::decoder::decode_udp(&buf[..len]) {
                            Ok((_rest, packet)) => {
                                let mut messages = Vec::new();
                                flatten(packet, &mut messages);
                                for message in messages {
                                    dispatch(&bus, message, &from.to_string()).await;
                                }
                            }
                            Err(err) => warn!(%from, ?err, "paquet OSC illisible"),
                        }
                    }
                    Err(err) => {
                        // Erreur socket transitoire (ICMP port unreachable sous
                        // Windows, etc.) : on continue d'écouter.
                        warn!(%err, "erreur de réception OSC");
                    }
                }
            }
        }
    }
    info!("OSC arrêté");
    Ok(())
}

/// Aplati les bundles (récursifs) en liste de messages.
fn flatten(packet: OscPacket, out: &mut Vec<OscMessage>) {
    match packet {
        OscPacket::Message(message) => out.push(message),
        OscPacket::Bundle(bundle) => {
            for inner in bundle.content {
                flatten(inner, out);
            }
        }
    }
}

async fn dispatch(bus: &BusHandle, message: OscMessage, from: &str) {
    match map_message(&message.addr, &message.args) {
        Ok(command) => {
            debug!(addr = %message.addr, %from, "OSC → commande");
            if !bus.send(Source::Osc, command).await {
                warn!("bus arrêté : commande OSC perdue");
            }
        }
        Err(err) => warn!(%from, %err, "message OSC ignoré"),
    }
}

// ---------------------------------------------------------------------------
// Traduction pure adresse+arguments → Command (testée exhaustivement)
// ---------------------------------------------------------------------------

/// Traduit un message OSC en commande du bus.
pub fn map_message(addr: &str, args: &[OscType]) -> Result<Command, MapError> {
    let bad = |detail: &str| MapError::BadArguments {
        addr: addr.to_string(),
        detail: detail.to_string(),
    };

    match addr {
        "/play" => Ok(Command::Play),
        "/pause" => Ok(Command::Pause),
        "/stop" => Ok(Command::Stop),
        "/mapping/reset" => Ok(Command::MappingReset),
        "/playlist/next" => Ok(Command::PlaylistNext),
        "/playlist/prev" => Ok(Command::PlaylistPrev),
        "/seek" => float_arg(args, 0)
            .map(|s| Command::Seek {
                seconds: f64::from(s),
            })
            .ok_or_else(|| bad("attendu : secondes (float)")),
        "/load" => string_arg(args, 0)
            .map(|path| Command::Load { path })
            .ok_or_else(|| bad("attendu : chemin (string)")),
        "/volume" => float_arg(args, 0)
            .map(|volume| Command::SetVolume { volume })
            .ok_or_else(|| bad("attendu : volume (float 0..1)")),
        "/loop" => parse_loop_mode(args).ok_or_else(|| bad("attendu : off|one|all ou 0|1|2")),
        "/playlist/set" => {
            let items: Option<Vec<String>> = (0..args.len()).map(|i| string_arg(args, i)).collect();
            items
                .filter(|items| !items.is_empty())
                .map(|items| Command::PlaylistSet { items })
                .ok_or_else(|| bad("attendu : une liste de chemins (strings)"))
        }
        "/playlist/go" => int_arg(args, 0)
            .and_then(|i| usize::try_from(i).ok())
            .map(|index| Command::PlaylistGo { index })
            .ok_or_else(|| bad("attendu : index (int ≥ 0)")),
        "/rotation" => int_arg(args, 0)
            .and_then(|d| u16::try_from(d).ok())
            .map(|degrees| Command::SetRotation { degrees })
            .ok_or_else(|| bad("attendu : 0|90|180|270")),
        "/flip" => match (bool_arg(args, 0), bool_arg(args, 1)) {
            (Some(horizontal), Some(vertical)) => Ok(Command::SetFlip {
                horizontal,
                vertical,
            }),
            _ => Err(bad("attendu : deux booléens (h, v)")),
        },
        "/crop" => match (
            float_arg(args, 0),
            float_arg(args, 1),
            float_arg(args, 2),
            float_arg(args, 3),
        ) {
            (Some(left), Some(top), Some(right), Some(bottom)) => Ok(Command::SetCrop {
                left,
                top,
                right,
                bottom,
            }),
            _ => Err(bad("attendu : quatre floats (gauche, haut, droite, bas)")),
        },
        "/mapping/enabled" => bool_arg(args, 0)
            .map(|enabled| Command::SetMappingEnabled { enabled })
            .ok_or_else(|| bad("attendu : booléen (0|1)")),
        "/mapping/save" => string_arg(args, 0)
            .map(|name| Command::MappingSave { name })
            .ok_or_else(|| bad("attendu : nom (string)")),
        "/mapping/load" => string_arg(args, 0)
            .map(|name| Command::MappingLoad { name })
            .ok_or_else(|| bad("attendu : nom (string)")),
        "/pattern" => parse_pattern(args).ok_or_else(|| bad("attendu : grid|checker|corners|off")),
        "/preset/save" => string_arg(args, 0)
            .map(|name| Command::PresetSave { name })
            .ok_or_else(|| bad("attendu : nom (string)")),
        "/preset/load" => string_arg(args, 0)
            .map(|name| Command::PresetLoad { name })
            .ok_or_else(|| bad("attendu : nom (string)")),
        other => {
            if let Some(rest) = other.strip_prefix("/corner/") {
                let index: u8 = rest
                    .parse()
                    .map_err(|_| bad("index de coin invalide (attendu 0..3)"))?;
                return match (float_arg(args, 0), float_arg(args, 1)) {
                    (Some(x), Some(y)) => Ok(Command::CornerSet { index, x, y }),
                    _ => Err(bad("attendu : deux floats (x, y)")),
                };
            }
            if let Some(rest) = other.strip_prefix("/color/") {
                let param =
                    parse_color_param(rest).ok_or_else(|| bad("paramètre couleur inconnu"))?;
                return float_arg(args, 0)
                    .map(|value| Command::ColorSet { param, value })
                    .ok_or_else(|| bad("attendu : valeur (float)"));
            }
            Err(MapError::UnknownAddress(other.to_string()))
        }
    }
}

fn parse_color_param(name: &str) -> Option<ColorParam> {
    match name {
        "brightness" => Some(ColorParam::Brightness),
        "contrast" => Some(ColorParam::Contrast),
        "gamma" => Some(ColorParam::Gamma),
        "saturation" => Some(ColorParam::Saturation),
        "hue" => Some(ColorParam::Hue),
        "gain_r" => Some(ColorParam::GainR),
        "gain_g" => Some(ColorParam::GainG),
        "gain_b" => Some(ColorParam::GainB),
        _ => None,
    }
}

fn parse_loop_mode(args: &[OscType]) -> Option<Command> {
    let mode = if let Some(text) = string_arg(args, 0) {
        match text.as_str() {
            "off" => LoopMode::Off,
            "one" => LoopMode::One,
            "all" => LoopMode::All,
            _ => return None,
        }
    } else {
        match int_arg(args, 0)? {
            0 => LoopMode::Off,
            1 => LoopMode::One,
            2 => LoopMode::All,
            _ => return None,
        }
    };
    Some(Command::SetLoop { mode })
}

fn parse_pattern(args: &[OscType]) -> Option<Command> {
    let pattern = if let Some(text) = string_arg(args, 0) {
        match text.as_str() {
            "off" | "none" => None,
            "grid" => Some(TestPattern::Grid),
            "checker" => Some(TestPattern::Checker),
            "corners" => Some(TestPattern::Corners),
            _ => return None,
        }
    } else {
        match int_arg(args, 0)? {
            0 => None,
            1 => Some(TestPattern::Grid),
            2 => Some(TestPattern::Checker),
            3 => Some(TestPattern::Corners),
            _ => return None,
        }
    };
    Some(Command::SetTestPattern { pattern })
}

/// Float tolérant : Float, Double, Int, Long, Bool (0/1).
fn float_arg(args: &[OscType], index: usize) -> Option<f32> {
    match args.get(index)? {
        OscType::Float(f) => Some(*f),
        OscType::Double(d) => Some(*d as f32),
        OscType::Int(i) => Some(*i as f32),
        OscType::Long(l) => Some(*l as f32),
        OscType::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Entier tolérant : Int, Long, ou Float/Double à valeur entière
/// (Chataigne envoie volontiers 90.0 pour 90).
fn int_arg(args: &[OscType], index: usize) -> Option<i64> {
    match args.get(index)? {
        OscType::Int(i) => Some(i64::from(*i)),
        OscType::Long(l) => Some(*l),
        OscType::Float(f) if f.fract() == 0.0 => Some(*f as i64),
        OscType::Double(d) if d.fract() == 0.0 => Some(*d as i64),
        _ => None,
    }
}

/// Booléen tolérant : Bool, ou entier/float 0/1.
fn bool_arg(args: &[OscType], index: usize) -> Option<bool> {
    match args.get(index)? {
        OscType::Bool(b) => Some(*b),
        _ => match int_arg(args, index)? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
    }
}

fn string_arg(args: &[OscType], index: usize) -> Option<String> {
    match args.get(index)? {
        OscType::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_addresses_map() {
        assert_eq!(map_message("/play", &[]), Ok(Command::Play));
        assert_eq!(map_message("/pause", &[]), Ok(Command::Pause));
        assert_eq!(map_message("/stop", &[]), Ok(Command::Stop));
        assert_eq!(
            map_message("/mapping/reset", &[]),
            Ok(Command::MappingReset)
        );
    }

    #[test]
    fn numeric_tolerance_matches_chataigne_habits() {
        // Chataigne envoie souvent des floats pour tout.
        assert_eq!(
            map_message("/volume", &[OscType::Float(0.5)]),
            Ok(Command::SetVolume { volume: 0.5 })
        );
        assert_eq!(
            map_message("/volume", &[OscType::Int(1)]),
            Ok(Command::SetVolume { volume: 1.0 })
        );
        assert_eq!(
            map_message("/rotation", &[OscType::Float(90.0)]),
            Ok(Command::SetRotation { degrees: 90 })
        );
        assert_eq!(
            map_message("/seek", &[OscType::Double(12.5)]),
            Ok(Command::Seek { seconds: 12.5 })
        );
        // Mais 90.5 n'est pas un entier.
        assert!(map_message("/rotation", &[OscType::Float(90.5)]).is_err());
    }

    #[test]
    fn corner_addresses_carry_index() {
        assert_eq!(
            map_message("/corner/2", &[OscType::Float(0.9), OscType::Float(1.0)]),
            Ok(Command::CornerSet {
                index: 2,
                x: 0.9,
                y: 1.0
            })
        );
        assert!(map_message(
            "/corner/quatre",
            &[OscType::Float(0.0), OscType::Float(0.0)]
        )
        .is_err());
        assert!(map_message("/corner/1", &[OscType::Float(0.0)]).is_err());
    }

    #[test]
    fn color_addresses_map_all_params() {
        for (name, param) in [
            ("brightness", ColorParam::Brightness),
            ("contrast", ColorParam::Contrast),
            ("gamma", ColorParam::Gamma),
            ("saturation", ColorParam::Saturation),
            ("hue", ColorParam::Hue),
            ("gain_r", ColorParam::GainR),
            ("gain_g", ColorParam::GainG),
            ("gain_b", ColorParam::GainB),
        ] {
            assert_eq!(
                map_message(&format!("/color/{name}"), &[OscType::Float(1.2)]),
                Ok(Command::ColorSet { param, value: 1.2 })
            );
        }
        assert!(map_message("/color/sepia", &[OscType::Float(1.0)]).is_err());
    }

    #[test]
    fn loop_mode_accepts_strings_and_ints() {
        assert_eq!(
            map_message("/loop", &[OscType::String("all".into())]),
            Ok(Command::SetLoop {
                mode: LoopMode::All
            })
        );
        assert_eq!(
            map_message("/loop", &[OscType::Int(1)]),
            Ok(Command::SetLoop {
                mode: LoopMode::One
            })
        );
        assert!(map_message("/loop", &[OscType::Int(9)]).is_err());
    }

    #[test]
    fn playlist_addresses_map() {
        assert_eq!(
            map_message(
                "/playlist/set",
                &[
                    OscType::String("a.mp4".into()),
                    OscType::String("b.mp4".into())
                ]
            ),
            Ok(Command::PlaylistSet {
                items: vec!["a.mp4".into(), "b.mp4".into()]
            })
        );
        assert!(map_message("/playlist/set", &[]).is_err());
        assert_eq!(
            map_message("/playlist/go", &[OscType::Int(2)]),
            Ok(Command::PlaylistGo { index: 2 })
        );
        assert!(map_message("/playlist/go", &[OscType::Int(-1)]).is_err());
        assert_eq!(
            map_message("/playlist/next", &[]),
            Ok(Command::PlaylistNext)
        );
    }

    #[test]
    fn flip_crop_pattern_map() {
        assert_eq!(
            map_message("/flip", &[OscType::Int(1), OscType::Bool(false)]),
            Ok(Command::SetFlip {
                horizontal: true,
                vertical: false
            })
        );
        assert_eq!(
            map_message(
                "/crop",
                &[
                    OscType::Float(0.1),
                    OscType::Float(0.0),
                    OscType::Float(0.2),
                    OscType::Float(0.0)
                ]
            ),
            Ok(Command::SetCrop {
                left: 0.1,
                top: 0.0,
                right: 0.2,
                bottom: 0.0
            })
        );
        assert_eq!(
            map_message("/pattern", &[OscType::String("grid".into())]),
            Ok(Command::SetTestPattern {
                pattern: Some(TestPattern::Grid)
            })
        );
        assert_eq!(
            map_message("/pattern", &[OscType::String("off".into())]),
            Ok(Command::SetTestPattern { pattern: None })
        );
        assert_eq!(
            map_message("/pattern", &[OscType::Int(3)]),
            Ok(Command::SetTestPattern {
                pattern: Some(TestPattern::Corners)
            })
        );
    }

    #[test]
    fn presets_map() {
        assert_eq!(
            map_message("/preset/save", &[OscType::String("scene_01".into())]),
            Ok(Command::PresetSave {
                name: "scene_01".into()
            })
        );
        assert_eq!(
            map_message("/preset/load", &[OscType::String("scene_01".into())]),
            Ok(Command::PresetLoad {
                name: "scene_01".into()
            })
        );
    }

    #[test]
    fn mapping_toggle_and_presets_map() {
        // Toggle tolérant sur les types (int, bool), comme /flip.
        assert_eq!(
            map_message("/mapping/enabled", &[OscType::Int(0)]),
            Ok(Command::SetMappingEnabled { enabled: false })
        );
        assert_eq!(
            map_message("/mapping/enabled", &[OscType::Bool(true)]),
            Ok(Command::SetMappingEnabled { enabled: true })
        );
        assert_eq!(
            map_message("/mapping/save", &[OscType::String("salon".into())]),
            Ok(Command::MappingSave {
                name: "salon".into()
            })
        );
        assert_eq!(
            map_message("/mapping/load", &[OscType::String("salon".into())]),
            Ok(Command::MappingLoad {
                name: "salon".into()
            })
        );
        // Argument manquant : erreur propre, pas de panique.
        assert!(matches!(
            map_message("/mapping/save", &[]),
            Err(MapError::BadArguments { .. })
        ));
    }

    #[test]
    fn unknown_addresses_are_reported() {
        assert_eq!(
            map_message("/self/destruct", &[]),
            Err(MapError::UnknownAddress("/self/destruct".into()))
        );
    }

    #[test]
    fn bundles_are_flattened_recursively() {
        use rosc::{OscBundle, OscTime};
        let time = OscTime {
            seconds: 0,
            fractional: 1,
        };
        let inner = OscPacket::Bundle(OscBundle {
            timetag: time,
            content: vec![OscPacket::Message(OscMessage {
                addr: "/play".into(),
                args: vec![],
            })],
        });
        let outer = OscPacket::Bundle(OscBundle {
            timetag: time,
            content: vec![
                inner,
                OscPacket::Message(OscMessage {
                    addr: "/stop".into(),
                    args: vec![],
                }),
            ],
        });
        let mut messages = Vec::new();
        flatten(outer, &mut messages);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].addr, "/play");
        assert_eq!(messages[1].addr, "/stop");
    }
}
