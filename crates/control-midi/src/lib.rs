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
    // Les effets sont tous bornés 0..1, comme `t` : pas de mise à l'échelle.
    let effet = |param: toolbox_core::state::EffectParam| Command::EffectSet { param, value: t };
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
        ScaleTarget::Pixelate => effet(toolbox_core::state::EffectParam::Pixelate),
        ScaleTarget::Posterize => effet(toolbox_core::state::EffectParam::Posterize),
        ScaleTarget::Noise => effet(toolbox_core::state::EffectParam::Noise),
        ScaleTarget::Sharpen => effet(toolbox_core::state::EffectParam::Sharpen),
        ScaleTarget::Mirror => effet(toolbox_core::state::EffectParam::Mirror),
        // Échelle GÉOMÉTRIQUE, pas linéaire : 0,25× à t=0, 1× PILE à
        // mi-course, 4× à fond. En linéaire sur [0,25 ; 4], la vitesse
        // normale tombait à t = 0,2, soit CC 25,4 — une position qu'aucun
        // contrôleur ne peut émettre : les deux crans encadrants donnaient
        // 0,988× et 1,018×, et le régisseur ne pouvait plus revenir à la
        // vitesse nominale depuis sa surface. Un fader de vitesse se pense
        // d'ailleurs en octaves (moitié / normal / double), pas en pas
        // constants.
        ScaleTarget::Rate => Command::SetRate {
            rate: 4.0f32.powf(2.0 * t - 1.0),
        },
        // 0..127 → 0..255 : la course entière du fader couvre celle du
        // master, et 127 donne bien 255 (pas 254).
        ScaleTarget::DmxMaster => Command::DmxMaster {
            valeur: (t * 255.0).round() as u8,
        },
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
/// - avec un `filtre`, le premier port dont le nom le contient (l'opérateur
///   décide, y compris s'il vise un port virtuel) ;
/// - sans filtre, le premier port NON virtuel, et RIEN si tous le sont.
///
/// Ce dernier point est essentiel : se rabattre sur le « Midi Through »
/// d'ALSA (toujours présent sur Linux/Pi) donnait une connexion qui
/// réussit, ne reçoit jamais rien, et surtout ne disparaît jamais — le
/// superviseur s'y verrouillait et n'ouvrait plus JAMAIS le contrôleur
/// branché ensuite. Ne rien ouvrir laisse le superviseur retenter.
pub fn choisir_port(noms: &[String], filtre: Option<&str>) -> Option<usize> {
    match filtre {
        Some(f) => noms.iter().position(|n| n.contains(f)),
        None => noms.iter().position(|n| !est_port_virtuel(n)),
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
    // En `debug` seulement : cette fonction est rappelée toutes les 3 s par
    // le superviseur tant qu'aucun contrôleur n'est branché — journaliser à
    // chaque tour noierait le tampon de logs de l'opérateur.
    tracing::debug!(ports = ?noms, "ports MIDI d'entrée détectés");
    let index =
        choisir_port(&noms, settings.port.as_deref()).ok_or_else(|| match &settings.port {
            Some(filter) => MidiError::NoPort(format!(" correspondant à {filter:?}")),
            // Distinguer « aucun port » de « que des ports de bouclage » :
            // le second cas se règle en branchant le contrôleur.
            None if noms.is_empty() => MidiError::NoPort(String::new()),
            None => MidiError::NoPort(format!(
                " (seuls des ports virtuels détectés : {}) — branchez le contrôleur, ou visez-en un avec [midi] port",
                noms.join(", ")
            )),
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

    /// Le manuel promet depuis la v1 qu'« un fader MIDI ou OSC suffit à
    /// activer et doser » un effet. Aucune cible n'existait : un binding CC
    /// sans `scale` ne peut envoyer qu'une valeur CONSTANTE, donc le fader
    /// était inutilisable pour ça.
    #[test]
    fn un_fader_pilote_un_effet_et_la_vitesse() {
        use toolbox_core::state::EffectParam;

        // Effets : bornés 0..1, donc la valeur du CC passe telle quelle.
        assert_eq!(
            scaled_command(ScaleTarget::Pixelate, 127),
            Command::EffectSet {
                param: EffectParam::Pixelate,
                value: 1.0
            }
        );
        assert_eq!(
            scaled_command(ScaleTarget::Noise, 0),
            Command::EffectSet {
                param: EffectParam::Noise,
                value: 0.0
            }
        );

        // Vitesse : à l'échelle des bornes de SetRate, pour que le bus
        // n'ait aucune raison de refuser la commande à fond de course.
        let Command::SetRate { rate } = scaled_command(ScaleTarget::Rate, 0) else {
            panic!("attendu SetRate");
        };
        assert!((rate - 0.25).abs() < 1e-6);
        let Command::SetRate { rate } = scaled_command(ScaleTarget::Rate, 127) else {
            panic!("attendu SetRate");
        };
        assert!((rate - 4.0).abs() < 1e-6);

        // La vitesse NORMALE doit être atteignable depuis la surface : en
        // échelle linéaire elle tombait à CC 25,4, une position qu'aucun
        // contrôleur ne peut émettre — le régisseur ne pouvait plus revenir
        // à 1× après avoir ralenti. Échelle géométrique : 1× pile à
        // mi-course (CC 64 sur 127 n'est pas exactement le milieu, mais
        // 63,5 l'est, donc on vise la valeur exacte à t = 0,5).
        let Command::SetRate { rate } = scaled_command(ScaleTarget::Rate, 127 / 2) else {
            panic!("attendu SetRate");
        };
        assert!(
            (rate - 1.0).abs() < 0.02,
            "à mi-course le fader doit rendre la vitesse normale, pas {rate}"
        );
        // Et les octaves tombent juste : moitié au quart, double aux trois quarts.
        let Command::SetRate { rate } = scaled_command(ScaleTarget::Rate, 127 / 4) else {
            panic!("attendu SetRate");
        };
        assert!(
            (rate - 0.5).abs() < 0.02,
            "au quart : 0,5× attendu, eu {rate}"
        );

        // Contre-épreuve : les bornes doivent être acceptées par l'état.
        let mut etat = toolbox_core::NodeState::default();
        etat.apply(&scaled_command(ScaleTarget::Rate, 127))
            .expect("la vitesse maximale doit être acceptée");
        etat.apply(&scaled_command(ScaleTarget::Rate, 0))
            .expect("la vitesse minimale doit être acceptée");
    }

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
        assert_eq!(choisir_port(&noms, Some("Through")), Some(0));
        assert_eq!(choisir_port(&noms, Some("introuvable")), None);
        // Le Through SEUL : on n'ouvre RIEN. S'y accrocher donnait une
        // connexion muette et définitive (le port ne disparaît jamais, donc
        // le superviseur ne se reconnectait plus au vrai contrôleur).
        let seul = vec!["Midi Through Port-0".to_string()];
        assert_eq!(choisir_port(&seul, None), None);
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
