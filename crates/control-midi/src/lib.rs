//! # toolbox-control-midi (P1.6)
//!
//! Traduit les notes et contrôleurs continus (CC) MIDI en [`Command`] sur le
//! bus, selon les bindings déclarés dans `node.toml` (voir
//! [`toolbox_core::config::MidiBinding`]).
//!
//! - une **note** déclenche une commande fixe (`{ cmd = "play" }`…) ;
//! - un **CC** pilote un paramètre continu (`scale = "volume"`) — la valeur
//!   0..127 est mise à l'échelle des bornes du paramètre — ou déclenche
//!   aussi une commande fixe.
//!
//! La traduction (`parse_midi` + `resolve`) est pure et testée ; seule la
//! connexion au port passe par `midir` (à valider sur matériel réel).
//! Une erreur MIDI ne fait JAMAIS tomber le node : le module se désactive
//! en le signalant dans les logs.

use midir::{Ignore, MidiInput, MidiInputConnection};
use thiserror::Error;
use tracing::{debug, warn};

use toolbox_core::config::{MidiBinding, MidiSettings, ScaleTarget};
use toolbox_core::state::color_bounds;
use toolbox_core::{BusHandle, ColorParam, Command, Source};

#[derive(Debug, Error)]
pub enum MidiError {
    #[error("initialisation MIDI impossible : {0}")]
    Init(String),
    #[error("aucun port MIDI d'entrée trouvé{0}")]
    NoPort(String),
    #[error("connexion au port MIDI impossible : {0}")]
    Connect(String),
}

/// Événement MIDI décodé (sous-ensemble utile au node).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiEvent {
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
}

impl MidiEvent {
    fn channel(&self) -> u8 {
        match self {
            MidiEvent::NoteOn { channel, .. } | MidiEvent::ControlChange { channel, .. } => {
                *channel
            }
        }
    }
}

/// Décode un message MIDI brut. Retourne `None` pour tout ce qui ne nous
/// concerne pas (note-off, aftertouch, sysex, message tronqué…).
pub fn parse_midi(bytes: &[u8]) -> Option<MidiEvent> {
    let status = *bytes.first()?;
    let channel = status & 0x0F;
    match status & 0xF0 {
        0x90 => {
            let note = *bytes.get(1)?;
            let velocity = *bytes.get(2)?;
            if velocity == 0 {
                // Note-on à vélocité nulle = note-off déguisé : ignoré.
                return None;
            }
            Some(MidiEvent::NoteOn {
                channel,
                note,
                velocity,
            })
        }
        0xB0 => Some(MidiEvent::ControlChange {
            channel,
            controller: *bytes.get(1)?,
            value: *bytes.get(2)?,
        }),
        _ => None,
    }
}

/// Cherche le premier binding qui correspond à l'événement et produit la
/// commande associée.
pub fn resolve(bindings: &[MidiBinding], event: &MidiEvent) -> Option<Command> {
    for binding in bindings {
        // Canal : bindings en 1..=16, événements en 0..=15.
        if let Some(wanted) = binding.channel {
            if u16::from(event.channel()) + 1 != u16::from(wanted) {
                continue;
            }
        }
        match event {
            MidiEvent::NoteOn { note, .. } => {
                if binding.note == Some(*note) {
                    if let Some(command) = &binding.command {
                        return Some(command.clone());
                    }
                }
            }
            MidiEvent::ControlChange {
                controller, value, ..
            } => {
                if binding.cc == Some(*controller) {
                    if let Some(target) = binding.scale {
                        return Some(scaled_command(target, *value));
                    }
                    if let Some(command) = &binding.command {
                        return Some(command.clone());
                    }
                }
            }
        }
    }
    None
}

/// CC 0..127 → commande à l'échelle des bornes du paramètre.
fn scaled_command(target: ScaleTarget, value: u8) -> Command {
    let t = f32::from(value.min(127)) / 127.0;
    let color = |param: ColorParam| {
        let (min, max) = color_bounds(param);
        Command::ColorSet {
            param,
            value: min + t * (max - min),
        }
    };
    match target {
        ScaleTarget::Volume => Command::SetVolume { volume: t },
        ScaleTarget::Brightness => color(ColorParam::Brightness),
        ScaleTarget::Contrast => color(ColorParam::Contrast),
        ScaleTarget::Gamma => color(ColorParam::Gamma),
        ScaleTarget::Saturation => color(ColorParam::Saturation),
        ScaleTarget::Hue => color(ColorParam::Hue),
        ScaleTarget::GainR => color(ColorParam::GainR),
        ScaleTarget::GainG => color(ColorParam::GainG),
        ScaleTarget::GainB => color(ColorParam::GainB),
    }
}

/// Connexion MIDI vivante : la lâcher déconnecte le port.
pub struct MidiService {
    _connection: MidiInputConnection<()>,
    pub port_name: String,
}

/// Vrai si le nom désigne un port MIDI VIRTUEL de bouclage (le « Midi
/// Through » d'ALSA, présent sur tout Linux/Pi). À éviter comme choix par
/// défaut : il est énuméré AVANT les contrôleurs USB mais ne reçoit jamais
/// rien d'un périphérique physique — le node semblerait « connecté » et muet.
fn est_port_virtuel(nom: &str) -> bool {
    let n = nom.to_ascii_lowercase();
    n.contains("midi through") || n.contains("through port")
}

/// Choisit l'index du port d'entrée à ouvrir parmi `noms`.
///
/// - avec un `filtre`, le premier port dont le nom le contient ;
/// - sans filtre, le premier port NON virtuel (on saute le « Midi Through »
///   d'ALSA), et seulement s'il n'en existe aucun autre, le premier port.
pub fn choisir_port(noms: &[String], filtre: Option<&str>) -> Option<usize> {
    match filtre {
        Some(f) => noms.iter().position(|n| n.contains(f)),
        None => noms
            .iter()
            .position(|n| !est_port_virtuel(n))
            .or(if noms.is_empty() { None } else { Some(0) }),
    }
}

/// Énumère les noms des ports MIDI d'entrée actuellement présents. Sert au
/// superviseur du node pour détecter un contrôleur débranché à chaud.
pub fn noms_ports() -> Result<Vec<String>, MidiError> {
    let input = MidiInput::new("toolbox-scan").map_err(|e| MidiError::Init(e.to_string()))?;
    Ok(input
        .ports()
        .iter()
        .map(|p| input.port_name(p).unwrap_or_default())
        .collect())
}

/// Ouvre le port d'entrée (filtré par `settings.port` si présent) et branche
/// les bindings sur le bus. Le callback tourne sur le thread MIDI : il
/// utilise `try_send` (jamais bloquant).
pub fn connect(settings: &MidiSettings, bus: BusHandle) -> Result<MidiService, MidiError> {
    let mut input = MidiInput::new("toolbox-node").map_err(|e| MidiError::Init(e.to_string()))?;
    input.ignore(Ignore::None);

    let ports = input.ports();
    let noms: Vec<String> = ports
        .iter()
        .map(|p| input.port_name(p).unwrap_or_default())
        .collect();
    // Journalise ce qui existe : sur site, ça montre d'un coup d'œil quel
    // contrôleur brancher dans [midi] port.
    tracing::info!(ports = ?noms, "ports MIDI d'entrée détectés");
    let index =
        choisir_port(&noms, settings.port.as_deref()).ok_or_else(|| match &settings.port {
            Some(filter) => MidiError::NoPort(format!(" correspondant à {filter:?}")),
            None => MidiError::NoPort(String::new()),
        })?;
    let port = ports[index].clone();

    let port_name = noms
        .get(index)
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "port inconnu".to_string());
    let bindings = settings.bindings.clone();

    let connection = input
        .connect(
            &port,
            "toolbox-in",
            move |_timestamp, bytes, _data| {
                let Some(event) = parse_midi(bytes) else {
                    return;
                };
                match resolve(&bindings, &event) {
                    Some(command) => {
                        if !bus.try_send(Source::Midi, command) {
                            warn!("bus saturé ou arrêté : commande MIDI perdue");
                        }
                    }
                    None => debug!(?event, "événement MIDI sans binding"),
                }
            },
            (),
        )
        .map_err(|e| MidiError::Connect(e.to_string()))?;

    Ok(MidiService {
        _connection: connection,
        port_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolbox_core::LoopMode;

    fn note_binding(note: u8, command: Command) -> MidiBinding {
        MidiBinding {
            note: Some(note),
            command: Some(command),
            ..MidiBinding::default()
        }
    }

    #[test]
    fn choisir_port_saute_le_midi_through() {
        // Cas Linux/Pi typique : le Through est énuméré en premier.
        let noms = vec![
            "Midi Through:Midi Through Port-0 14:0".to_string(),
            "APC mini mk2:APC mini mk2 MIDI 1 20:0".to_string(),
        ];
        assert_eq!(choisir_port(&noms, None), Some(1), "on saute le Through");
        // Un filtre explicite prime, même s'il vise le Through.
        assert_eq!(choisir_port(&noms, Some("APC")), Some(1));
        assert_eq!(choisir_port(&noms, Some("introuvable")), None);
        // Si le Through est le SEUL port, on le prend faute de mieux.
        let seul = vec!["Midi Through Port-0".to_string()];
        assert_eq!(choisir_port(&seul, None), Some(0));
        // Aucun port du tout.
        assert_eq!(choisir_port(&[], None), None);
    }

    #[test]
    fn parse_note_on_and_cc() {
        assert_eq!(
            parse_midi(&[0x90, 60, 100]),
            Some(MidiEvent::NoteOn {
                channel: 0,
                note: 60,
                velocity: 100
            })
        );
        assert_eq!(
            parse_midi(&[0x9A, 61, 1]),
            Some(MidiEvent::NoteOn {
                channel: 10,
                note: 61,
                velocity: 1
            })
        );
        assert_eq!(
            parse_midi(&[0xB3, 7, 127]),
            Some(MidiEvent::ControlChange {
                channel: 3,
                controller: 7,
                value: 127
            })
        );
        // Note-off (0x80), note-on vélocité 0, message tronqué : ignorés.
        assert_eq!(parse_midi(&[0x80, 60, 0]), None);
        assert_eq!(parse_midi(&[0x90, 60, 0]), None);
        assert_eq!(parse_midi(&[0x90, 60]), None);
        assert_eq!(parse_midi(&[]), None);
    }

    #[test]
    fn note_binding_fires_fixed_command() {
        let bindings = vec![
            note_binding(60, Command::Play),
            note_binding(62, Command::Stop),
        ];
        assert_eq!(
            resolve(
                &bindings,
                &MidiEvent::NoteOn {
                    channel: 0,
                    note: 62,
                    velocity: 80
                }
            ),
            Some(Command::Stop)
        );
        assert_eq!(
            resolve(
                &bindings,
                &MidiEvent::NoteOn {
                    channel: 0,
                    note: 61,
                    velocity: 80
                }
            ),
            None
        );
    }

    #[test]
    fn channel_filter_applies() {
        let binding = MidiBinding {
            note: Some(60),
            channel: Some(10),
            command: Some(Command::Play),
            ..MidiBinding::default()
        };
        // Canal 10 (1-indexé) = canal brut 9.
        assert_eq!(
            resolve(
                std::slice::from_ref(&binding),
                &MidiEvent::NoteOn {
                    channel: 9,
                    note: 60,
                    velocity: 1
                }
            ),
            Some(Command::Play)
        );
        assert_eq!(
            resolve(
                &[binding],
                &MidiEvent::NoteOn {
                    channel: 0,
                    note: 60,
                    velocity: 1
                }
            ),
            None
        );
    }

    #[test]
    fn cc_scale_maps_to_bounds() {
        let bindings = vec![MidiBinding {
            cc: Some(7),
            scale: Some(ScaleTarget::Volume),
            ..MidiBinding::default()
        }];
        assert_eq!(
            resolve(
                &bindings,
                &MidiEvent::ControlChange {
                    channel: 0,
                    controller: 7,
                    value: 127
                }
            ),
            Some(Command::SetVolume { volume: 1.0 })
        );
        assert_eq!(
            resolve(
                &bindings,
                &MidiEvent::ControlChange {
                    channel: 0,
                    controller: 7,
                    value: 0
                }
            ),
            Some(Command::SetVolume { volume: 0.0 })
        );

        // Gamma : 0..127 → 0.2..4.0.
        let Some(Command::ColorSet { param, value }) = resolve(
            &[MidiBinding {
                cc: Some(1),
                scale: Some(ScaleTarget::Gamma),
                ..MidiBinding::default()
            }],
            &MidiEvent::ControlChange {
                channel: 0,
                controller: 1,
                value: 127,
            },
        ) else {
            panic!("attendu ColorSet");
        };
        assert_eq!(param, ColorParam::Gamma);
        assert!((value - 4.0).abs() < 1e-6);
    }

    #[test]
    fn cc_can_fire_fixed_command_too() {
        let bindings = vec![MidiBinding {
            cc: Some(64),
            command: Some(Command::SetLoop {
                mode: LoopMode::All,
            }),
            ..MidiBinding::default()
        }];
        assert_eq!(
            resolve(
                &bindings,
                &MidiEvent::ControlChange {
                    channel: 0,
                    controller: 64,
                    value: 127
                }
            ),
            Some(Command::SetLoop {
                mode: LoopMode::All
            })
        );
    }
}
