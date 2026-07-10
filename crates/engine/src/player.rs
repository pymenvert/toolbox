//! Player (P1.2) : machine à états de lecture, découplée du backend réel.
//!
//! Le [`Player`] s'abonne au bus, traduit les [`Event`] en appels sur un
//! [`PlayerBackend`] et applique la politique de fin de média (boucle,
//! playlist). Le backend GStreamer arrivera derrière ce même trait quand le
//! matériel sera disponible ; en attendant, [`MemoryBackend`] simule une
//! lecture réelle (position, durée, fin de média) — toute la logique est
//! donc testable et démontrable sans vidéo.

use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{broadcast, watch};
use tracing::{error, info, warn};

use toolbox_core::state::{Event, LoopMode, NodeState, Transport};
use toolbox_core::{BusHandle, Command, Source};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlayerError {
    #[error("erreur backend : {0}")]
    Backend(String),
    #[error("média illisible : {0}")]
    Media(String),
}

/// Événement remonté par un backend (thread GStreamer, décodeur…).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    /// Fin du média courant.
    EndOfStream,
    /// Erreur pendant la lecture (décodage, fichier…).
    Error(String),
}

/// Abstraction du moteur de lecture réel.
///
/// Contrat : les appels sont idempotents quand c'est possible, aucune méthode
/// ne panique, les erreurs remontent en [`PlayerError`]. `take_events` draine
/// les événements accumulés depuis le dernier appel (jamais bloquant).
pub trait PlayerBackend: Send {
    fn load(&mut self, path: &Path) -> Result<(), PlayerError>;
    fn play(&mut self) -> Result<(), PlayerError>;
    fn pause(&mut self) -> Result<(), PlayerError>;
    fn stop(&mut self) -> Result<(), PlayerError>;
    fn seek(&mut self, seconds: f64) -> Result<(), PlayerError>;
    fn set_volume(&mut self, volume: f32) -> Result<(), PlayerError>;
    /// Position de lecture, si un média est chargé.
    fn position_seconds(&self) -> Option<f64>;
    /// Durée du média, si connue.
    fn duration_seconds(&self) -> Option<f64>;
    fn take_events(&mut self) -> Vec<BackendEvent>;
    /// Indique que le média courant doit reboucler SANS COUPURE quand il se
    /// termine (mode boucle « un »). Un backend qui sait le faire (GStreamer
    /// via `about-to-finish`) n'émet alors plus de fin de média ; les autres
    /// ignorent l'indication et le player reboucle par seek (petit hoquet).
    fn set_gapless_loop(&mut self, _enabled: bool) {}
    /// Vitesse de lecture (1.0 = normale). Les micro-corrections de synchro
    /// (±3 %) passent par ici : un backend qui ne sait pas faire ignore
    /// l'appel — la synchro retombe alors sur les resyncs durs (seek).
    fn set_rate(&mut self, _rate: f64) -> Result<(), PlayerError> {
        Ok(())
    }
}

/// Position de lecture publiée pour l'UI (WebSocket/REST).
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct PlaybackPosition {
    pub position: Option<f64>,
    pub duration: Option<f64>,
}

/// Le service player : branche un backend sur le bus.
pub struct Player<B: PlayerBackend> {
    backend: B,
    bus: BusHandle,
    media_root: PathBuf,
    // Miroirs de l'état (mis à jour par les événements du bus).
    transport: Transport,
    loop_mode: LoopMode,
    playlist_len: usize,
    in_playlist: bool,
    position_tx: watch::Sender<PlaybackPosition>,
    /// Départ synchronisé programmé (`/sync/startAt`) : échéance locale
    /// calculée depuis l'heure Unix demandée.
    start_deadline: Option<tokio::time::Instant>,
    /// Prochaine tentative de reprise d'une source live indisponible
    /// (carte d'acquisition débranchée, flux coupé) — mode passthrough.
    reprise: Option<tokio::time::Instant>,
    intervalle_reprise: Duration,
}

impl<B: PlayerBackend> Player<B> {
    /// Crée le player et le synchronise sur l'état courant du bus.
    pub fn new(backend: B, bus: BusHandle, media_root: impl Into<PathBuf>) -> Self {
        let (position_tx, _) = watch::channel(PlaybackPosition::default());
        let mut player = Self {
            backend,
            bus: bus.clone(),
            media_root: media_root.into(),
            transport: Transport::Stopped,
            loop_mode: LoopMode::Off,
            playlist_len: 0,
            in_playlist: false,
            position_tx,
            start_deadline: None,
            reprise: None,
            intervalle_reprise: Duration::from_secs(3),
        };
        player.resync(&bus.snapshot());
        player
    }

    /// Intervalle entre deux tentatives de reprise d'une source live
    /// (3 s par défaut ; raccourci par les tests).
    #[must_use]
    pub fn avec_intervalle_reprise(mut self, intervalle: Duration) -> Self {
        self.intervalle_reprise = intervalle;
        self
    }

    /// Récepteur de la position de lecture (pour l'UI).
    pub fn position_watch(&self) -> watch::Receiver<PlaybackPosition> {
        self.position_tx.subscribe()
    }

    /// Résout une source média : un fichier (déjà validé : relatif, sans
    /// `..`) est ancré sous `media/` ; les autres sources (`rtsp://`,
    /// `capture://`, `ndi://`…) passent telles quelles au backend.
    fn resolve(&self, rel: &str) -> PathBuf {
        use toolbox_core::MediaSource;
        match MediaSource::parse(rel) {
            Ok(MediaSource::File(_)) | Err(_) => self.media_root.join(rel),
            Ok(_) => PathBuf::from(rel),
        }
    }

    /// Recale entièrement le backend sur un état complet (démarrage,
    /// chargement de preset, rattrapage après `Lagged`).
    pub fn resync(&mut self, state: &NodeState) {
        self.loop_mode = state.player.loop_mode;
        self.playlist_len = state.player.playlist.len();
        self.in_playlist = state.player.playlist_index.is_some();
        self.backend
            .set_gapless_loop(self.loop_mode == LoopMode::One);

        if let Err(err) = self.backend.set_volume(state.player.volume) {
            warn!(%err, "volume non appliqué");
        }
        if let Err(err) = self.backend.set_rate(f64::from(state.player.rate)) {
            warn!(%err, "vitesse non appliquée");
        }
        match &state.player.media {
            Some(rel) => {
                let path = self.resolve(rel);
                if let Err(err) = self.backend.load(&path) {
                    error!(media = %rel, %err, "média illisible au resync");
                    self.transport = Transport::Stopped;
                    let _ = self.bus.try_send(Source::Internal, Command::Stop);
                    return;
                }
                self.transport = state.player.transport;
                let result = match state.player.transport {
                    Transport::Playing => self.backend.play(),
                    Transport::Paused => self.backend.pause(),
                    Transport::Stopped => self.backend.stop(),
                };
                if let Err(err) = result {
                    error!(%err, "transport non appliqué au resync");
                }
            }
            None => {
                self.transport = Transport::Stopped;
                if let Err(err) = self.backend.stop() {
                    warn!(%err, "stop non appliqué");
                }
            }
        }
    }

    /// Applique un événement du bus au backend.
    pub fn handle_event(&mut self, event: &Event) {
        match event {
            Event::MediaLoaded { path } => {
                self.in_playlist = false;
                self.reprise = None;
                let abs = self.resolve(path);
                if let Err(err) = self.backend.load(&abs) {
                    // Source live absente (carte débranchée, flux coupé) :
                    // on N'ABANDONNE PAS — reprise automatique (passthrough).
                    if source_branchable(path) {
                        warn!(media = %path, %err, "source live indisponible — reprise automatique planifiée");
                        self.reprise = Some(tokio::time::Instant::now() + self.intervalle_reprise);
                        return;
                    }
                    error!(media = %path, %err, "chargement refusé par le backend");
                    let _ = self.bus.try_send(Source::Internal, Command::Stop);
                    return;
                }
                info!(media = %path, "média chargé");
                // Enchaînement de playlist : la lecture continue toute seule.
                if self.transport == Transport::Playing {
                    if let Err(err) = self.backend.play() {
                        error!(%err, "lecture non relancée après chargement");
                    }
                }
            }
            Event::PlaylistPositionChanged { .. } => {
                self.in_playlist = true;
            }
            Event::PlaylistChanged { items, index } => {
                self.playlist_len = items.len();
                self.in_playlist = index.is_some();
            }
            Event::TransportChanged { transport } => {
                self.transport = *transport;
                if *transport == Transport::Stopped {
                    // Un stop annule un départ synchronisé en attente.
                    self.start_deadline = None;
                }
                let result = match transport {
                    Transport::Playing => self.backend.play(),
                    Transport::Paused => self.backend.pause(),
                    Transport::Stopped => self.backend.stop(),
                };
                if let Err(err) = result {
                    error!(%err, ?transport, "changement de transport refusé");
                }
            }
            Event::SyncScheduled { at } => {
                // Heure Unix → échéance sur l'horloge tokio locale. Passée ⇒
                // départ immédiat ; bornée à 24 h par sécurité.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let delay = (at - now).clamp(0.0, 86_400.0);
                self.start_deadline =
                    Some(tokio::time::Instant::now() + Duration::from_secs_f64(delay));
                info!(dans_secondes = delay, "départ synchronisé programmé");
            }
            Event::Seeked { seconds } => {
                if let Err(err) = self.backend.seek(*seconds) {
                    error!(%err, seconds, "seek refusé");
                }
            }
            Event::VolumeChanged { volume } => {
                if let Err(err) = self.backend.set_volume(*volume) {
                    error!(%err, volume, "volume refusé");
                }
            }
            Event::RateChanged { rate } => {
                if let Err(err) = self.backend.set_rate(f64::from(*rate)) {
                    error!(%err, rate, "vitesse refusée");
                }
            }
            Event::LoopChanged { mode } => {
                self.loop_mode = *mode;
                self.backend.set_gapless_loop(*mode == LoopMode::One);
            }
            Event::StateReplaced { state } => {
                self.resync(state);
            }
            // Mapping/couleur/mire : affaire du moteur de rendu, pas du player.
            _ => {}
        }
    }

    /// Draine les événements du backend et publie la position.
    /// À appeler périodiquement (tick).
    pub fn pump(&mut self) {
        for event in self.backend.take_events() {
            match event {
                BackendEvent::EndOfStream => self.on_end_of_stream(),
                BackendEvent::Error(message) => {
                    let media = self.bus.snapshot().player.media;
                    match media {
                        Some(rel) if source_branchable(&rel) => {
                            // Passthrough : une source live qui tombe se
                            // reconnecte toute seule quand elle revient.
                            warn!(%message, media = %rel, "source live tombée — reprise automatique planifiée");
                            self.reprise =
                                Some(tokio::time::Instant::now() + self.intervalle_reprise);
                        }
                        _ => {
                            error!(%message, "erreur backend");
                            let _ = self.bus.try_send(Source::Internal, Command::Stop);
                        }
                    }
                }
            }
        }
        let _ = self.position_tx.send_replace(PlaybackPosition {
            position: self.backend.position_seconds(),
            duration: self.backend.duration_seconds(),
        });
    }

    /// Politique de fin de média — LA logique métier du player.
    fn on_end_of_stream(&mut self) {
        match self.loop_mode {
            LoopMode::One => self.replay(),
            mode => {
                if self.playlist_len > 0 && self.in_playlist {
                    // La machine à états du core décide : suivant, boucle
                    // complète ou stop en fin de playlist.
                    let _ = self.bus.try_send(Source::Internal, Command::PlaylistNext);
                } else if mode == LoopMode::All {
                    // Média seul + boucle globale : on reboucle le média.
                    self.replay();
                } else {
                    let _ = self.bus.try_send(Source::Internal, Command::Stop);
                }
            }
        }
    }

    fn replay(&mut self) {
        info!("fin de média : rebouclage");
        if let Err(err) = self.backend.seek(0.0).and_then(|()| self.backend.play()) {
            error!(%err, "rebouclage impossible");
            let _ = self.bus.try_send(Source::Internal, Command::Stop);
        }
    }

    /// Tente de reprendre une source live indisponible (mode passthrough).
    fn retenter_source(&mut self) {
        self.reprise = None;
        let etat = self.bus.snapshot();
        let Some(rel) = etat.player.media.clone() else {
            return;
        };
        if !source_branchable(&rel) {
            return;
        }
        let abs = self.resolve(&rel);
        match self.backend.load(&abs) {
            Ok(()) => {
                info!(media = %rel, "source revenue — reprise");
                if etat.player.transport == Transport::Playing {
                    if let Err(err) = self.backend.play() {
                        error!(%err, "lecture non reprise sur la source revenue");
                    }
                }
            }
            Err(_) => {
                // Toujours absente : on retentera, sans bruit dans les logs.
                self.reprise = Some(tokio::time::Instant::now() + self.intervalle_reprise);
            }
        }
    }

    /// Boucle du service : événements du bus + tick périodique.
    pub async fn run(mut self) {
        let mut events = self.bus.subscribe();
        // Comble le trou entre la création du player et cet abonnement :
        // tout événement émis entre-temps est déjà reflété dans l'état.
        let snapshot = self.bus.snapshot();
        self.resync(&snapshot);
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        info!(media_root = %self.media_root.display(), "player démarré");
        loop {
            // Copies locales : le garde du select ne peut pas emprunter self
            // pendant que les autres branches le mutent.
            let deadline = self.start_deadline;
            let reprise = self.reprise;
            tokio::select! {
                received = events.recv() => match received {
                    Ok(event) => self.handle_event(&event),
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(missed, "événements manqués : resynchronisation complète");
                        let snapshot = self.bus.snapshot();
                        self.resync(&snapshot);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = tick.tick() => self.pump(),
                // Départ synchronisé : à l'échéance, Play via le bus (tous
                // les abonnés voient le même TransportChanged).
                () = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)),
                    if deadline.is_some() =>
                {
                    self.start_deadline = None;
                    info!("départ synchronisé : lecture");
                    let _ = self.bus.try_send(Source::Internal, Command::Play);
                }
                // Reprise d'une source live (passthrough) : à l'échéance,
                // nouvelle tentative de chargement.
                () = tokio::time::sleep_until(reprise.unwrap_or_else(tokio::time::Instant::now)),
                    if reprise.is_some() =>
                {
                    self.retenter_source();
                }
            }
        }
        info!("player arrêté");
    }
}

/// Une source qui se branche/débranche (capture, NDI, flux réseau) mérite
/// une reprise automatique ; un fichier absent, non.
fn source_branchable(rel: &str) -> bool {
    toolbox_core::MediaSource::parse(rel)
        .map(|source| source.is_live())
        .unwrap_or(false)
}

/// Backend en mémoire : simule une lecture réelle (position qui avance,
/// fin de média) sans rien afficher. Sert aux tests ET au mode sans vidéo
/// (contrôle OSC/UI démontrable sur n'importe quelle machine).
pub struct MemoryBackend {
    media: Option<PathBuf>,
    playing: bool,
    /// Position au dernier (re)démarrage de lecture.
    base_position: f64,
    started_at: Option<std::time::Instant>,
    duration: f64,
    volume: f32,
    /// Vitesse de lecture simulée (1.0 = normale) — la position avance à
    /// cette allure, comme un vrai backend : la synchro est testable.
    rate: f64,
    pending: Vec<BackendEvent>,
    /// Les fichiers doivent-ils exister sur disque ? (true dans le node,
    /// false dans les tests purs)
    check_files: bool,
    eos_emitted: bool,
    /// Indication gapless reçue (enregistrée pour les tests ; le backend
    /// mémoire boucle via la politique EOS du player).
    gapless: bool,
}

impl MemoryBackend {
    pub fn new(duration_seconds: f64, check_files: bool) -> Self {
        Self {
            media: None,
            playing: false,
            base_position: 0.0,
            started_at: None,
            duration: duration_seconds.max(0.1),
            volume: 1.0,
            rate: 1.0,
            pending: Vec::new(),
            check_files,
            eos_emitted: false,
            gapless: false,
        }
    }

    fn current_position(&self) -> f64 {
        let elapsed = self
            .started_at
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        (self.base_position + elapsed * self.rate).min(self.duration)
    }

    /// Force la fin de média (tests).
    pub fn force_end_of_stream(&mut self) {
        self.base_position = self.duration;
        self.started_at = None;
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn loaded_path(&self) -> Option<&Path> {
        self.media.as_deref()
    }

    /// L'indication gapless reçue en dernier (tests).
    pub fn gapless_loop(&self) -> bool {
        self.gapless
    }
}

impl PlayerBackend for MemoryBackend {
    fn load(&mut self, path: &Path) -> Result<(), PlayerError> {
        // Seules les sources fichier sont vérifiées sur disque : une URL
        // réseau ou une capture n'a pas de fichier à trouver.
        let is_file_source = matches!(
            toolbox_core::MediaSource::parse(&path.to_string_lossy()),
            Ok(toolbox_core::MediaSource::File(_)) | Err(_)
        );
        if self.check_files && is_file_source && !path.is_file() {
            return Err(PlayerError::Media(format!(
                "fichier introuvable : {}",
                path.display()
            )));
        }
        self.media = Some(path.to_path_buf());
        self.base_position = 0.0;
        self.started_at = None;
        self.playing = false;
        self.eos_emitted = false;
        Ok(())
    }

    fn play(&mut self) -> Result<(), PlayerError> {
        if self.media.is_none() {
            return Err(PlayerError::Backend("play sans média".into()));
        }
        if !self.playing {
            self.playing = true;
            self.started_at = Some(std::time::Instant::now());
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<(), PlayerError> {
        if self.playing {
            self.base_position = self.current_position();
            self.started_at = None;
            self.playing = false;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlayerError> {
        self.playing = false;
        self.base_position = 0.0;
        self.started_at = None;
        self.eos_emitted = false;
        Ok(())
    }

    fn seek(&mut self, seconds: f64) -> Result<(), PlayerError> {
        if self.media.is_none() {
            return Err(PlayerError::Backend("seek sans média".into()));
        }
        self.base_position = seconds.clamp(0.0, self.duration);
        if self.playing {
            self.started_at = Some(std::time::Instant::now());
        }
        self.eos_emitted = false;
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) -> Result<(), PlayerError> {
        self.volume = volume.clamp(0.0, 1.0);
        Ok(())
    }

    fn position_seconds(&self) -> Option<f64> {
        self.media.as_ref().map(|_| self.current_position())
    }

    fn duration_seconds(&self) -> Option<f64> {
        self.media.as_ref().map(|_| self.duration)
    }

    fn take_events(&mut self) -> Vec<BackendEvent> {
        if self.playing && !self.eos_emitted && self.current_position() >= self.duration {
            self.eos_emitted = true;
            self.playing = false;
            self.pending.push(BackendEvent::EndOfStream);
        }
        std::mem::take(&mut self.pending)
    }

    fn set_gapless_loop(&mut self, enabled: bool) {
        self.gapless = enabled;
    }

    fn set_rate(&mut self, rate: f64) -> Result<(), PlayerError> {
        // Rebase : la position déjà parcourue reste acquise, la nouvelle
        // vitesse s'applique à partir de maintenant.
        self.base_position = self.current_position();
        if self.playing {
            self.started_at = Some(std::time::Instant::now());
        }
        self.rate = rate.clamp(0.25, 4.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolbox_core::Bus;

    /// Backend qui échoue au chargement N fois avant de fonctionner —
    /// simule une carte d'acquisition branchée en retard.
    struct BackendCapricieux {
        echecs_restants: std::sync::Arc<std::sync::atomic::AtomicU32>,
        charge: bool,
        joue: bool,
    }

    impl PlayerBackend for BackendCapricieux {
        fn load(&mut self, _path: &Path) -> Result<(), PlayerError> {
            let restants = &self.echecs_restants;
            if restants.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                restants.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return Err(PlayerError::Backend("périphérique absent".into()));
            }
            self.charge = true;
            Ok(())
        }
        fn play(&mut self) -> Result<(), PlayerError> {
            self.joue = true;
            Ok(())
        }
        fn pause(&mut self) -> Result<(), PlayerError> {
            self.joue = false;
            Ok(())
        }
        fn stop(&mut self) -> Result<(), PlayerError> {
            self.joue = false;
            Ok(())
        }
        fn seek(&mut self, _seconds: f64) -> Result<(), PlayerError> {
            Ok(())
        }
        fn set_volume(&mut self, _volume: f32) -> Result<(), PlayerError> {
            Ok(())
        }
        fn position_seconds(&self) -> Option<f64> {
            (self.charge && self.joue).then_some(0.5)
        }
        fn duration_seconds(&self) -> Option<f64> {
            None
        }
        fn take_events(&mut self) -> Vec<BackendEvent> {
            Vec::new()
        }
    }

    /// Passthrough : une source live absente au chargement est reprise
    /// automatiquement quand elle revient, et la lecture repart seule.
    #[tokio::test]
    async fn la_source_live_est_reprise_quand_elle_revient() {
        let bus = Bus::new(32, 128);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let echecs = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(2));
        let backend = BackendCapricieux {
            echecs_restants: echecs.clone(),
            charge: false,
            joue: false,
        };
        let player = Player::new(backend, handle.clone(), "media")
            .avec_intervalle_reprise(Duration::from_millis(60));
        let position = player.position_watch();
        tokio::spawn(player.run());
        tokio::time::sleep(Duration::from_millis(50)).await;

        // La « carte » est absente : deux chargements vont échouer.
        handle
            .send(
                Source::Http,
                Command::Load {
                    path: "capture://0".into(),
                },
            )
            .await;
        handle.send(Source::Http, Command::Play).await;

        // La reprise automatique doit finir par charger ET relancer la
        // lecture (position publiée = chargé + en lecture).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if position.borrow().position.is_some() {
                assert_eq!(
                    echecs.load(std::sync::atomic::Ordering::SeqCst),
                    0,
                    "les échecs simulés doivent avoir été consommés"
                );
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "la source n'a jamais été reprise"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Monte un bus + player avec backend mémoire (fichiers non vérifiés).
    fn setup() -> (Bus, Player<MemoryBackend>) {
        let bus = Bus::new(64, 64);
        let handle = bus.handle();
        let backend = MemoryBackend::new(10.0, false);
        let player = Player::new(backend, handle, "/tmp/media");
        (bus, player)
    }

    /// Fait suivre au player tous les événements produits par une commande.
    fn drive(bus: &mut Bus, player: &mut Player<MemoryBackend>, source: Source, cmd: &Command) {
        for event in bus.dispatch(source, cmd) {
            player.handle_event(&event);
        }
    }

    #[test]
    fn load_and_play_reach_backend() {
        let (mut bus, mut player) = setup();
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::Load {
                path: "a.mp4".into(),
            },
        );
        drive(&mut bus, &mut player, Source::Http, &Command::Play);
        assert!(player.backend.is_playing());
        player.pump();
        let pos = *player.position_watch().borrow();
        assert_eq!(pos.duration, Some(10.0));
        assert!(pos.position.is_some());
    }

    #[test]
    fn end_of_stream_loop_one_replays() {
        let (mut bus, mut player) = setup();
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::SetLoop {
                mode: LoopMode::One,
            },
        );
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::Load {
                path: "a.mp4".into(),
            },
        );
        drive(&mut bus, &mut player, Source::Http, &Command::Play);
        // On force la fin du média côté backend…
        player.backend.force_end_of_stream();
        player.pump();
        // …et la lecture doit être repartie de zéro, sans passer par le bus.
        assert!(player.backend.is_playing());
        assert!(player.backend.position_seconds().expect("pos") < 1.0);
    }

    /// Synchro niveau 1 : arm fige en pause à 0, startAt lance la lecture
    /// à l'échéance — les tolérances sont larges (runners CI lents).
    #[tokio::test]
    async fn sync_arm_then_start_at_launches_on_time() {
        let bus = Bus::new(32, 32);
        let handle = bus.handle();
        let backend = MemoryBackend::new(10.0, false);
        let player = Player::new(backend, handle.clone(), "/tmp/media");
        tokio::spawn(bus.run());
        tokio::spawn(player.run());
        // Laisse les deux tâches démarrer : l'abonnement du player doit
        // précéder les commandes (les send mpsc ne cèdent pas la main).
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            handle
                .send(
                    Source::Http,
                    Command::Load {
                        path: "a.mp4".into()
                    }
                )
                .await
        );
        assert!(handle.send(Source::Http, Command::SyncArm).await);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("horloge")
            .as_secs_f64();
        assert!(
            handle
                .send(Source::Http, Command::SyncStartAt { at: now + 0.6 })
                .await
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            handle.snapshot().player.transport,
            Transport::Paused,
            "armé : toujours en pause avant l'échéance"
        );
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            handle.snapshot().player.transport,
            Transport::Playing,
            "parti à l'échéance"
        );
    }

    #[test]
    fn gapless_hint_follows_loop_mode() {
        let (mut bus, mut player) = setup();
        assert!(!player.backend.gapless_loop());
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::SetLoop {
                mode: LoopMode::One,
            },
        );
        assert!(player.backend.gapless_loop(), "boucle un = gapless demandé");
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::SetLoop {
                mode: LoopMode::All,
            },
        );
        assert!(
            !player.backend.gapless_loop(),
            "boucle playlist : le player garde la main (enchaînement)"
        );
        // Le resync (chargement de preset) réapplique l'indication.
        let mut state = NodeState::default();
        state
            .apply(&Command::SetLoop {
                mode: LoopMode::One,
            })
            .expect("loop");
        player.resync(&state);
        assert!(player.backend.gapless_loop());
    }

    #[test]
    fn end_of_stream_off_stops() {
        let (mut bus, mut player) = setup();
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::Load {
                path: "a.mp4".into(),
            },
        );
        drive(&mut bus, &mut player, Source::Http, &Command::Play);
        player.backend.force_end_of_stream();
        player.pump();
        // Fin de média sans boucle ni playlist : le player a demandé Stop au
        // bus ; on rejoue la commande comme le ferait Bus::run.
        drive(&mut bus, &mut player, Source::Internal, &Command::Stop);
        assert!(!player.backend.is_playing());
        assert_eq!(bus.state().player.transport, Transport::Stopped);
    }

    #[test]
    fn end_of_stream_advances_playlist() {
        let (mut bus, mut player) = setup();
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::PlaylistSet {
                items: vec!["a.mp4".into(), "b.mp4".into()],
            },
        );
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::PlaylistGo { index: 0 },
        );
        drive(&mut bus, &mut player, Source::Http, &Command::Play);
        player.backend.force_end_of_stream();
        player.pump();
        // Le player demande PlaylistNext : on le traite comme le ferait le bus.
        drive(
            &mut bus,
            &mut player,
            Source::Internal,
            &Command::PlaylistNext,
        );
        assert_eq!(
            player.backend.loaded_path(),
            Some(Path::new("/tmp/media/b.mp4"))
        );
        // Transport toujours "playing" → le backend a relancé la lecture.
        assert!(player.backend.is_playing());
    }

    #[test]
    fn preset_state_replaced_resyncs_backend() {
        let (mut bus, mut player) = setup();
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::Load {
                path: "a.mp4".into(),
            },
        );
        drive(
            &mut bus,
            &mut player,
            Source::Http,
            &Command::SetVolume { volume: 0.3 },
        );
        // Simule un chargement de preset : état complet différent.
        let mut state = NodeState::default();
        state
            .apply(&Command::Load {
                path: "autre.mp4".into(),
            })
            .expect("load");
        state
            .apply(&Command::SetVolume { volume: 0.8 })
            .expect("volume");
        player.handle_event(&Event::StateReplaced {
            state: Box::new(state),
        });
        assert_eq!(
            player.backend.loaded_path(),
            Some(Path::new("/tmp/media/autre.mp4"))
        );
        assert!((player.backend.volume() - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn live_sources_bypass_file_check() {
        let bus = Bus::new(8, 8);
        let handle = bus.handle();
        // check_files = true : un fichier absent serait refusé…
        let backend = MemoryBackend::new(5.0, true);
        let mut player = Player::new(backend, handle, "/nulle/part");
        // …mais une capture ou un flux réseau n'est pas un fichier.
        for src in ["capture://0", "rtsp://10.0.0.5/cam", "ndi://Régie"] {
            player.handle_event(&Event::MediaLoaded { path: src.into() });
            assert_eq!(
                player.backend.loaded_path(),
                Some(Path::new(src)),
                "source {src} transmise telle quelle au backend"
            );
        }
    }

    #[test]
    fn backend_load_error_requests_stop() {
        let bus = Bus::new(8, 8);
        let handle = bus.handle();
        // check_files = true : le fichier n'existe pas → erreur de chargement.
        let backend = MemoryBackend::new(5.0, true);
        let mut player = Player::new(backend, handle, "/nulle/part");
        player.handle_event(&Event::MediaLoaded {
            path: "fantome.mp4".into(),
        });
        assert_eq!(player.backend.loaded_path(), None);
    }

    #[test]
    fn memory_backend_pause_freezes_position() {
        let mut backend = MemoryBackend::new(5.0, false);
        backend.load(Path::new("x.mp4")).expect("load");
        backend.play().expect("play");
        backend.seek(2.0).expect("seek");
        backend.pause().expect("pause");
        let p1 = backend.position_seconds().expect("pos");
        std::thread::sleep(std::time::Duration::from_millis(30));
        let p2 = backend.position_seconds().expect("pos");
        assert!((p1 - p2).abs() < 1e-9, "la pause doit geler la position");
        assert!((2.0..2.5).contains(&p1));
    }
}
