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

/// Ouvre le port d'entrée (filtré par `settings.port` si présent) et branche
/// les bindings sur le bus. Le callback tourne sur le thread MIDI : il
/// utilise `try_send` (jamais bloquant).
pub fn connect(settings: &MidiSettings, bus: BusHandle) -> Result<MidiService, MidiError> {
    let mut input = MidiInput::new("toolbox-node").map_err(|e| MidiError::Init(e.to_string()))?;
    input.ignore(Ignore::None);

    let ports = input.ports();
    let port = match &settings.port {
        Some(filter) => ports
            .iter()
            .find(|p| {
                input
                    .port_name(p)
                    .map(|name| name.contains(filter.as_str()))
                    .unwrap_or(false)
            })
            .ok_or_else(|| MidiError::NoPort(format!(" correspondant à {filter:?}")))?,
        None => ports
            .first()
            .ok_or_else(|| MidiError::NoPort(String::new()))?,
    }
    .clone();

    let port_name = input
        .port_name(&port)
        .unwrap_or_else(|_| "port inconnu".to_string());
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
                &[binding.clone()],
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
