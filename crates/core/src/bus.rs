//! Bus de commandes/événements — la colonne vertébrale du node.
//!
//! Pattern repris de HPlayer2/3 (bus unique, interfaces découplées), en typé :
//! - les producteurs (OSC, MIDI, HTTP, séquenceur…) envoient des [`Command`]
//!   via un [`BusHandle`] cloné ;
//! - le bus applique la commande à l'état (validation incluse) ;
//! - l'[`Event`] résultant est diffusé à tous les abonnés (moteur de rendu,
//!   web UI, feedback OSC, logs). Les erreurs sont tracées, jamais avalées.

use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use crate::command::Command;
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

/// Poignée clonable pour envoyer des commandes et s'abonner aux événements.
#[derive(Debug, Clone)]
pub struct BusHandle {
    commands: mpsc::Sender<(Source, Command)>,
    events: broadcast::Sender<Event>,
}

impl BusHandle {
    /// Envoie une commande. Retourne `false` si le bus est arrêté.
    pub async fn send(&self, source: Source, command: Command) -> bool {
        self.commands.send((source, command)).await.is_ok()
    }

    /// S'abonne au flux d'événements (chaque abonné reçoit tout).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}

/// Le bus lui-même : possède l'état et boucle sur les commandes entrantes.
pub struct Bus {
    state: NodeState,
    rx: mpsc::Receiver<(Source, Command)>,
    events: broadcast::Sender<Event>,
    handle: BusHandle,
}

impl Bus {
    /// Crée le bus. `command_capacity` borne la file d'attente (backpressure) ;
    /// `event_capacity` borne le buffer de diffusion (un abonné trop lent
    /// perd les événements les plus anciens, signalé par `RecvError::Lagged`).
    pub fn new(command_capacity: usize, event_capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(command_capacity);
        let (events, _) = broadcast::channel(event_capacity);
        let handle = BusHandle {
            commands: tx,
            events: events.clone(),
        };
        Self {
            state: NodeState::default(),
            rx,
            events,
            handle,
        }
    }

    pub fn handle(&self) -> BusHandle {
        self.handle.clone()
    }

    /// Accès en lecture à l'état courant (snapshot pour l'API/UI).
    pub fn state(&self) -> &NodeState {
        &self.state
    }

    /// Traite une commande immédiatement (utilisé par la boucle et les tests).
    pub fn dispatch(&mut self, source: Source, command: &Command) -> Option<Event> {
        match self.state.apply(command) {
            Ok(event) => {
                info!(%source, ?command, ?event, "commande appliquée");
                // send() n'échoue que s'il n'y a aucun abonné : pas une erreur.
                let _ = self.events.send(event.clone());
                Some(event)
            }
            Err(err) => {
                warn!(%source, ?command, %err, "commande refusée");
                None
            }
        }
    }

    /// Boucle principale : consomme les commandes jusqu'à fermeture de tous
    /// les émetteurs. À lancer dans une tâche tokio dédiée.
    pub async fn run(mut self) {
        info!("bus démarré");
        while let Some((source, command)) = self.rx.recv().await {
            self.dispatch(source, &command);
        }
        info!("bus arrêté (tous les émetteurs fermés)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(bus.dispatch(Source::Osc, &Command::Play).is_none());
        assert!(events.try_recv().is_err());
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
}
