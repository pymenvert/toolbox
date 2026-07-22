//! Bus de commandes/événements — la colonne vertébrale du node.
//!
//! Pattern repris de HPlayer2/3 (bus unique, interfaces découplées), en typé :
//! - les producteurs (OSC, MIDI, HTTP, séquenceur…) envoient des [`Command`]
//!   via un [`BusHandle`] cloné ;
//! - le bus applique la commande à l'état (validation incluse) ; les commandes
//!   de preset passent par le [`PresetStore`] attaché ;
//! - les [`Event`] résultants sont diffusés à tous les abonnés (moteur de
//!   rendu, web UI, feedback OSC, logs). Les erreurs sont tracées, jamais
//!   avalées ;
//! - l'état courant est publié sur un canal `watch` : n'importe quel module
//!   peut obtenir un instantané cohérent sans interroger le bus.

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::command::Command;
use crate::preset::{MappingStore, PresetStore};
use crate::state::{Event, NodeState};

/// Origine d'une commande, pour les logs et le debug terrain
/// ("qui a envoyé ce /stop ?").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Osc,
    Midi,
    Http,
    WebSocket,
    Sequencer,
    Internal,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Source::Osc => "osc",
            Source::Midi => "midi",
            Source::Http => "http",
            Source::WebSocket => "ws",
            Source::Sequencer => "seq",
            Source::Internal => "internal",
        };
        f.write_str(s)
    }
}

/// Poignée clonable : envoyer des commandes, s'abonner aux événements,
/// obtenir un instantané de l'état.
#[derive(Debug, Clone)]
pub struct BusHandle {
    commands: mpsc::Sender<(Source, Command)>,
    events: broadcast::Sender<Event>,
    state: watch::Receiver<NodeState>,
}

impl BusHandle {
    /// Envoie une commande (attend si la file est pleine — backpressure).
    /// Retourne `false` si le bus est arrêté.
    pub async fn send(&self, source: Source, command: Command) -> bool {
        self.commands.send((source, command)).await.is_ok()
    }

    /// Variante non bloquante pour les contextes synchrones (callback MIDI,
    /// thread OSC). Retourne `false` si la file est pleine ou le bus arrêté.
    pub fn try_send(&self, source: Source, command: Command) -> bool {
        self.commands.try_send((source, command)).is_ok()
    }

    /// S'abonne au flux d'événements (chaque abonné reçoit tout).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Instantané cohérent de l'état courant.
    pub fn snapshot(&self) -> NodeState {
        self.state.borrow().clone()
    }

    /// Récepteur `watch` : `changed().await` pour réagir à chaque mutation.
    pub fn state_watch(&self) -> watch::Receiver<NodeState> {
        self.state.clone()
    }
}

/// Le bus lui-même : possède l'état et boucle sur les commandes entrantes.
pub struct Bus {
    state: NodeState,
    rx: mpsc::Receiver<(Source, Command)>,
    events: broadcast::Sender<Event>,
    state_tx: watch::Sender<NodeState>,
    presets: Option<PresetStore>,
    mapping_presets: Option<MappingStore>,
    handle: BusHandle,
}

impl Bus {
    /// Crée le bus. `command_capacity` borne la file d'attente (backpressure) ;
    /// `event_capacity` borne le buffer de diffusion (un abonné trop lent
    /// perd les événements les plus anciens, signalé par `RecvError::Lagged`).
    pub fn new(command_capacity: usize, event_capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(command_capacity.max(1));
        let (events, _) = broadcast::channel(event_capacity.max(1));
        let (state_tx, state_rx) = watch::channel(NodeState::default());
        let handle = BusHandle {
            commands: tx,
            events: events.clone(),
            state: state_rx,
        };
        Self {
            state: NodeState::default(),
            rx,
            events,
            state_tx,
            presets: None,
            mapping_presets: None,
            handle,
        }
    }

    /// Attache un dépôt de presets : `preset_save`/`preset_load` deviennent
    /// fonctionnels. Sans dépôt, ces commandes sont refusées (et tracées).
    #[must_use]
    pub fn with_presets(mut self, store: PresetStore) -> Self {
        self.presets = Some(store);
        self
    }

    /// Attache un dépôt de presets de mapping : `mapping_save`/`mapping_load`
    /// deviennent fonctionnels. Sans dépôt, ces commandes sont refusées.
    #[must_use]
    pub fn with_mapping_presets(mut self, store: MappingStore) -> Self {
        self.mapping_presets = Some(store);
        self
    }

    pub fn handle(&self) -> BusHandle {
        self.handle.clone()
    }

    /// Accès en lecture à l'état courant (snapshot pour l'API/UI).
    pub fn state(&self) -> &NodeState {
        &self.state
    }

    /// Traite une commande immédiatement (utilisé par la boucle et les tests).
    /// Retourne les événements émis (vide si la commande a été refusée).
    pub fn dispatch(&mut self, source: Source, command: &Command) -> Vec<Event> {
        Self::process(
            &mut self.state,
            &self.events,
            &self.state_tx,
            self.presets.as_ref(),
            self.mapping_presets.as_ref(),
            source,
            command,
        )
    }

    fn process(
        state: &mut NodeState,
        events: &broadcast::Sender<Event>,
        state_tx: &watch::Sender<NodeState>,
        presets: Option<&PresetStore>,
        mapping_presets: Option<&MappingStore>,
        source: Source,
        command: &Command,
    ) -> Vec<Event> {
        let result = match command {
            Command::PresetSave { name } => match presets {
                Some(store) => store
                    .save(name, state)
                    .map(|()| vec![Event::PresetSaved { name: name.clone() }]),
                None => Err(crate::error::CoreError::InvalidCommand(
                    "aucun dépôt de presets configuré".into(),
                )),
            },
            Command::PresetLoad { name } => match presets {
                Some(store) => store.load(name).map(|loaded| {
                    *state = loaded;
                    vec![
                        Event::PresetLoaded { name: name.clone() },
                        Event::StateReplaced {
                            state: Box::new(state.clone()),
                        },
                    ]
                }),
                None => Err(crate::error::CoreError::InvalidCommand(
                    "aucun dépôt de presets configuré".into(),
                )),
            },
            // Le bus valide (durée, preset existant) et annonce ; l'interpolation
            // est menée par le service fader (voir `crate::fader`), abonné aux
            // événements — le bus ne dort jamais.
            Command::PresetFade { name, seconds } => match presets {
                Some(store) => {
                    if !seconds.is_finite() || *seconds <= 0.0 || *seconds > 60.0 {
                        Err(crate::error::CoreError::OutOfRange {
                            param: "fade.seconds",
                            value: f64::from(*seconds),
                            min: 0.0,
                            max: 60.0,
                        })
                    } else {
                        store.load(name).map(|_| {
                            vec![Event::PresetFadeStarted {
                                name: name.clone(),
                                seconds: *seconds,
                            }]
                        })
                    }
                }
                None => Err(crate::error::CoreError::InvalidCommand(
                    "aucun dépôt de presets configuré".into(),
                )),
            },
            // Même principe que PresetFade, sur le dépôt de mappings.
            Command::MappingFade { name, seconds } => match mapping_presets {
                Some(store) => {
                    if !seconds.is_finite() || *seconds <= 0.0 || *seconds > 60.0 {
                        Err(crate::error::CoreError::OutOfRange {
                            param: "fade.seconds",
                            value: f64::from(*seconds),
                            min: 0.0,
                            max: 60.0,
                        })
                    } else {
                        store.load(name).map(|_| {
                            vec![Event::MappingFadeStarted {
                                name: name.clone(),
                                seconds: *seconds,
                            }]
                        })
                    }
                }
                None => Err(crate::error::CoreError::InvalidCommand(
                    "aucun dépôt de presets de mapping configuré".into(),
                )),
            },
            Command::MappingSave { name } => match mapping_presets {
                Some(store) => store
                    .save(name, &state.mapping)
                    .map(|()| vec![Event::MappingSaved { name: name.clone() }]),
                None => Err(crate::error::CoreError::InvalidCommand(
                    "aucun dépôt de presets de mapping configuré".into(),
                )),
            },
            // Ne remplace QUE le mapping : la lecture en cours continue
            // (pas de StateReplaced, qui resynchroniserait le player).
            Command::MappingLoad { name } => match mapping_presets {
                Some(store) => store.load(name).map(|mapping| {
                    state.mapping = mapping.clone();
                    vec![Event::MappingLoaded {
                        name: name.clone(),
                        mapping,
                    }]
                }),
                None => Err(crate::error::CoreError::InvalidCommand(
                    "aucun dépôt de presets de mapping configuré".into(),
                )),
            },
            other => state.apply(other),
        };

        match result {
            Ok(emitted) => {
                // Publier l'état AVANT les événements : un abonné réveillé par
                // un événement qui fait un snapshot voit déjà l'état à jour.
                state_tx.send_replace(state.clone());
                for event in &emitted {
                    info!(%source, ?command, ?event, "commande appliquée");
                    // send() n'échoue que s'il n'y a aucun abonné : pas une erreur.
                    let _ = events.send(event.clone());
                }
                emitted
            }
            Err(err) => {
                warn!(%source, ?command, %err, "commande refusée");
                Vec::new()
            }
        }
    }

    /// Boucle principale : consomme les commandes jusqu'à fermeture de tous
    /// les émetteurs. À lancer dans une tâche tokio dédiée.
    ///
    /// NB : le handle interne est droppé en entrant, sinon le bus garderait
    /// vivant son propre émetteur et `recv()` ne rendrait jamais `None`.
    pub async fn run(self) {
        let Bus {
            mut state,
            mut rx,
            events,
            state_tx,
            presets,
            mapping_presets,
            handle,
        } = self;
        drop(handle);
        info!("bus démarré");
        while let Some((source, command)) = rx.recv().await {
            // NB : preset_save/mapping_save écrivent (fsync) DANS cette boucle,
            // ce qui fige brièvement les commandes le temps de l'écriture. Les
            // sortir en tâche asynchrone casserait l'ordre « sauver puis
            // charger » dont dépendent le fader et le séquenceur (ils relisent
            // le preset sur DISQUE). Le vrai remède serait un cache mémoire des
            // presets partagé avec le fader — changement d'architecture laissé
            // pour plus tard. Impact réel limité : une sauvegarde est une
            // action manuelle ponctuelle, pas un flux continu.
            Self::process(
                &mut state,
                &events,
                &state_tx,
                presets.as_ref(),
                mapping_presets.as_ref(),
                source,
                &command,
            );
        }
        info!("bus arrêté (tous les émetteurs fermés)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Transport;

    #[tokio::test]
    async fn command_produces_event_for_subscribers() {
        let bus = Bus::new(16, 16);
        let handle = bus.handle();
        let mut events = handle.subscribe();

        tokio::spawn(bus.run());

        assert!(
            handle
                .send(
                    Source::Http,
                    Command::Load {
                        path: "media/a.mp4".into()
                    }
                )
                .await
        );
        assert!(handle.send(Source::Osc, Command::Play).await);

        let e1 = events.recv().await.expect("event 1");
        assert_eq!(
            e1,
            Event::MediaLoaded {
                path: "media/a.mp4".into()
            }
        );
        let e2 = events.recv().await.expect("event 2");
        assert!(matches!(e2, Event::TransportChanged { .. }));
    }

    #[tokio::test]
    async fn invalid_command_emits_no_event() {
        let mut bus = Bus::new(16, 16);
        let handle = bus.handle();
        let mut events = handle.subscribe();

        // play sans média : refusé, aucun événement.
        assert!(bus.dispatch(Source::Osc, &Command::Play).is_empty());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn bus_stops_when_all_handles_dropped() {
        let bus = Bus::new(4, 4);
        let handle = bus.handle();
        let task = tokio::spawn(bus.run());
        drop(handle);
        // Sans le drop du handle interne dans run(), ceci bloquerait à jamais.
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("le bus doit s'arrêter quand tous les handles sont droppés")
            .expect("join");
    }

    #[tokio::test]
    async fn every_subscriber_receives_all_events() {
        let mut bus = Bus::new(16, 16);
        let handle = bus.handle();
        let mut a = handle.subscribe();
        let mut b = handle.subscribe();

        bus.dispatch(Source::Internal, &Command::SetVolume { volume: 0.5 });

        let expected = Event::VolumeChanged { volume: 0.5 };
        assert_eq!(a.try_recv().expect("a"), expected);
        assert_eq!(b.try_recv().expect("b"), expected);
    }

    #[tokio::test]
    async fn snapshot_follows_mutations() {
        let mut bus = Bus::new(16, 16);
        let handle = bus.handle();
        assert_eq!(handle.snapshot(), NodeState::default());

        bus.dispatch(Source::Http, &Command::SetVolume { volume: 0.25 });
        assert_eq!(handle.snapshot().player.volume, 0.25);

        // Une commande refusée ne change pas l'instantané.
        bus.dispatch(Source::Http, &Command::SetVolume { volume: 9.0 });
        assert_eq!(handle.snapshot().player.volume, 0.25);
    }

    #[tokio::test]
    async fn try_send_works_from_sync_context() {
        let bus = Bus::new(4, 4);
        let handle = bus.handle();
        let mut events = handle.subscribe();
        tokio::spawn(bus.run());

        assert!(handle.try_send(
            Source::Midi,
            Command::Load {
                path: "a.mp4".into()
            }
        ));
        let ev = events.recv().await.expect("event");
        assert_eq!(
            ev,
            Event::MediaLoaded {
                path: "a.mp4".into()
            }
        );
    }

    #[tokio::test]
    async fn preset_save_and_load_via_bus() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PresetStore::open(dir.path().join("presets")).expect("open");
        let mut bus = Bus::new(16, 16).with_presets(store);
        let handle = bus.handle();

        // Construire un état, le sauvegarder.
        bus.dispatch(
            Source::Http,
            &Command::Load {
                path: "clips/a.mp4".into(),
            },
        );
        bus.dispatch(Source::Http, &Command::SetVolume { volume: 0.4 });
        let saved = bus.dispatch(
            Source::Http,
            &Command::PresetSave {
                name: "scene".into(),
            },
        );
        assert_eq!(
            saved,
            vec![Event::PresetSaved {
                name: "scene".into()
            }]
        );

        // Modifier l'état, puis recharger le preset : état restauré + événements.
        bus.dispatch(Source::Http, &Command::SetVolume { volume: 1.0 });
        let mut events = handle.subscribe();
        let emitted = bus.dispatch(
            Source::Http,
            &Command::PresetLoad {
                name: "scene".into(),
            },
        );
        assert_eq!(emitted.len(), 2);
        assert_eq!(
            emitted[0],
            Event::PresetLoaded {
                name: "scene".into()
            }
        );
        let Event::StateReplaced { state } = &emitted[1] else {
            panic!("attendu StateReplaced, reçu {:?}", emitted[1]);
        };
        assert_eq!(state.player.volume, 0.4);
        assert_eq!(state.player.media.as_deref(), Some("clips/a.mp4"));
        assert_eq!(handle.snapshot().player.volume, 0.4);
        // Les abonnés reçoivent les deux événements.
        assert!(matches!(
            events.try_recv().expect("ev1"),
            Event::PresetLoaded { .. }
        ));
        assert!(matches!(
            events.try_recv().expect("ev2"),
            Event::StateReplaced { .. }
        ));
    }

    #[tokio::test]
    async fn preset_fade_is_validated_by_the_bus() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PresetStore::open(dir.path().join("presets")).expect("open");
        let mut bus = Bus::new(16, 16).with_presets(store);
        bus.dispatch(Source::Http, &Command::PresetSave { name: "a".into() });

        // Fondu valide : annoncé, sans toucher l'état (le fader s'en charge).
        let emitted = bus.dispatch(
            Source::Http,
            &Command::PresetFade {
                name: "a".into(),
                seconds: 2.0,
            },
        );
        assert_eq!(
            emitted,
            vec![Event::PresetFadeStarted {
                name: "a".into(),
                seconds: 2.0,
            }]
        );

        // Durée invalide ou preset absent : refusé.
        for (name, seconds) in [("a", 0.0), ("a", -1.0), ("a", f32::NAN), ("fantome", 1.0)] {
            assert!(
                bus.dispatch(
                    Source::Http,
                    &Command::PresetFade {
                        name: name.into(),
                        seconds,
                    },
                )
                .is_empty(),
                "aurait dû refuser {name}/{seconds}"
            );
        }
    }

    #[tokio::test]
    async fn preset_commands_without_store_are_rejected() {
        let mut bus = Bus::new(4, 4);
        assert!(bus
            .dispatch(Source::Osc, &Command::PresetSave { name: "x".into() })
            .is_empty());
        assert!(bus
            .dispatch(Source::Osc, &Command::PresetLoad { name: "x".into() })
            .is_empty());
    }

    #[tokio::test]
    async fn missing_preset_leaves_state_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PresetStore::open(dir.path().join("presets")).expect("open");
        let mut bus = Bus::new(4, 4).with_presets(store);
        bus.dispatch(Source::Http, &Command::SetVolume { volume: 0.7 });

        let emitted = bus.dispatch(
            Source::Http,
            &Command::PresetLoad {
                name: "fantome".into(),
            },
        );
        assert!(emitted.is_empty());
        assert_eq!(bus.state().player.volume, 0.7);
        assert_eq!(bus.state().player.transport, Transport::Stopped);
    }

    #[tokio::test]
    async fn mapping_save_and_load_via_bus_keeps_playback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = MappingStore::open(dir.path().join("presets").join("mapping")).expect("open");
        let mut bus = Bus::new(16, 16).with_mapping_presets(store);

        // Calage : un coin déplacé, puis sauvegarde du mapping seul.
        bus.dispatch(
            Source::Http,
            &Command::CornerSet {
                index: 2,
                x: 0.8,
                y: 0.9,
            },
        );
        let saved = bus.dispatch(
            Source::Http,
            &Command::MappingSave {
                name: "salon".into(),
            },
        );
        assert_eq!(
            saved,
            vec![Event::MappingSaved {
                name: "salon".into()
            }]
        );

        // Lecture en cours + mapping remis à zéro entre-temps.
        bus.dispatch(
            Source::Http,
            &Command::Load {
                path: "clips/a.mp4".into(),
            },
        );
        bus.dispatch(Source::Http, &Command::Play);
        bus.dispatch(Source::Http, &Command::MappingReset);

        // Recharger le mapping : restauré, et la lecture n'est PAS interrompue
        // (un seul événement MappingLoaded, pas de StateReplaced).
        let emitted = bus.dispatch(
            Source::Http,
            &Command::MappingLoad {
                name: "salon".into(),
            },
        );
        assert_eq!(emitted.len(), 1);
        let Event::MappingLoaded { name, mapping } = &emitted[0] else {
            panic!("attendu MappingLoaded, reçu {:?}", emitted[0]);
        };
        assert_eq!(name, "salon");
        assert_eq!(mapping.corners[2], crate::state::Corner { x: 0.8, y: 0.9 });
        assert_eq!(&bus.state().mapping, mapping);
        assert_eq!(bus.state().player.transport, Transport::Playing);
        assert_eq!(bus.state().player.media.as_deref(), Some("clips/a.mp4"));
    }

    #[tokio::test]
    async fn mapping_preset_commands_without_store_are_rejected() {
        let mut bus = Bus::new(4, 4);
        assert!(bus
            .dispatch(Source::Osc, &Command::MappingSave { name: "x".into() })
            .is_empty());
        assert!(bus
            .dispatch(Source::Osc, &Command::MappingLoad { name: "x".into() })
            .is_empty());
    }
}
