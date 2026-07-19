//! Contrôleur des interrupteurs de fonctions (V2, onglet « Fonctions »).
//!
//! Observe les [`FeatureFlags`] sur un canal `watch` et démarre/arrête
//! chaque service À CHAUD. Désactivé = réellement arrêté : socket fermée,
//! port MIDI relâché, pipeline vidéo libéré, annonce mDNS retirée — zéro
//! ressource consommée. Réactivé = le service redémarre sans relancer le
//! node.
//!
//! Hors périmètre : le serveur HTTP (on ne coupe pas la main qui tient
//! l'interrupteur) et la fenêtre de sortie (gérée par les canaux de rendu,
//! voir `[output]` — sa mise en sommeil arrive dans un lot dédié).

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use toolbox_core::{BusHandle, Command, FeatureFlags, MappingStore, NodeConfig, PresetStore};
use toolbox_engine::{MemoryBackend, PlaybackPosition, Player, PlayerBackend};

use crate::supervision::spawn_service;

/// Tout ce qu'il faut pour (re)démarrer chaque service.
pub struct Contexte {
    pub handle: BusHandle,
    pub config: NodeConfig,
    pub node_name: String,
    pub version: String,
    pub presets: PresetStore,
    pub mapping_presets: MappingStore,
    /// Producteur de frames vidéo (cloné pour chaque backend démarré) ;
    /// les récepteurs (fenêtre, aperçu) gardent le même canal à vie.
    pub video_tx: watch::Sender<Option<toolbox_engine::VideoFrame>>,
    /// Canal stable de position de lecture (l'UI garde le même récepteur,
    /// le player courant y est ponté).
    pub position_tx: std::sync::Arc<watch::Sender<PlaybackPosition>>,
    /// Liste du parc pour /api/fleet (clonée pour chaque démarrage mDNS).
    pub fleet_tx: watch::Sender<serde_json::Value>,
}

/// Un service en cours, avec de quoi l'arrêter réellement.
enum Actif {
    /// Tâche tokio avec son canal d'arrêt dédié.
    Service {
        arret: watch::Sender<bool>,
        tache: JoinHandle<()>,
    },
    /// Daemon mDNS : l'arrêt retire les annonces du réseau.
    Fleet(crate::fleet::FleetHandle),
}

async fn arreter(nom: &'static str, actif: Actif) {
    match actif {
        Actif::Service { arret, tache } => {
            let _ = arret.send(true);
            if tokio::time::timeout(Duration::from_secs(5), tache)
                .await
                .is_err()
            {
                warn!(service = nom, "arrêt forcé après 5 s");
            }
        }
        Actif::Fleet(handle) => handle.arreter(),
    }
    info!(service = nom, "fonction désactivée (service arrêté)");
}

/// Boucle du contrôleur : applique chaque changement de drapeaux, arrête
/// tout à l'arrêt du node.
pub async fn controleur(
    ctx: Contexte,
    mut flags_rx: watch::Receiver<FeatureFlags>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut actifs: HashMap<&'static str, Actif> = HashMap::new();
    // Rien n'est encore démarré : le premier passage démarre ce qui doit l'être.
    let mut courant = FeatureFlags {
        player: false,
        output: false,
        osc: false,
        oscquery: false,
        osc_feedback: false,
        midi: false,
        fleet: false,
        fader: false,
        preview: false,
        // L'émission Art-Net est gérée par son service (canal `actif`
        // dérivé des drapeaux), pas par le contrôleur.
        artnet: false,
    };
    info!("contrôleur de fonctions prêt");
    loop {
        let voulu = *flags_rx.borrow_and_update();
        appliquer(&ctx, voulu, &mut courant, &mut actifs).await;
        tokio::select! {
            _ = shutdown.changed() => break,
            changed = flags_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    for (nom, actif) in actifs.drain() {
        arreter(nom, actif).await;
    }
    info!("contrôleur de fonctions arrêté");
}

async fn appliquer(
    ctx: &Contexte,
    voulu: FeatureFlags,
    courant: &mut FeatureFlags,
    actifs: &mut HashMap<&'static str, Actif>,
) {
    // OSC entrant.
    if voulu.osc != courant.osc {
        if voulu.osc {
            actifs.insert("osc", demarrer_osc(ctx));
        } else if let Some(actif) = actifs.remove("osc") {
            arreter("osc", actif).await;
        }
    }

    // OSCQuery (le serveur ; l'annonce mDNS suit via le redémarrage du fleet).
    if voulu.oscquery != courant.oscquery {
        if voulu.oscquery {
            actifs.insert("oscquery", demarrer_oscquery(ctx));
        } else if let Some(actif) = actifs.remove("oscquery") {
            arreter("oscquery", actif).await;
        }
    }

    // Retour d'état OSC (exige une cible dans la config).
    if voulu.osc_feedback != courant.osc_feedback {
        if voulu.osc_feedback {
            match demarrer_feedback(ctx) {
                Some(actif) => {
                    actifs.insert("osc-feedback", actif);
                }
                None => info!("retour d'état OSC sans cible ([osc] feedback absent) — inactif"),
            }
        } else if let Some(actif) = actifs.remove("osc-feedback") {
            arreter("osc-feedback", actif).await;
        }
    }

    // Fondus.
    if voulu.fader != courant.fader {
        if voulu.fader {
            actifs.insert("fader", demarrer_fader(ctx));
        } else if let Some(actif) = actifs.remove("fader") {
            arreter("fader", actif).await;
        }
    }

    // Player (pipeline vidéo).
    if voulu.player != courant.player {
        if voulu.player {
            actifs.insert("player", demarrer_player(ctx));
        } else if let Some(actif) = actifs.remove("player") {
            // Lecture coupée proprement : état cohérent + écran noir.
            ctx.handle
                .send(toolbox_core::Source::Internal, Command::Stop)
                .await;
            arreter("player", actif).await;
            let _ = ctx.video_tx.send(None);
            let _ = ctx.position_tx.send(PlaybackPosition::default());
        }
    }

    // MIDI.
    if voulu.midi != courant.midi {
        if voulu.midi {
            if let Some(actif) = demarrer_midi(ctx) {
                actifs.insert("midi", actif);
            }
        } else if let Some(actif) = actifs.remove("midi") {
            arreter("midi", actif).await;
        }
    }

    // Parc mDNS — redémarré aussi quand l'annonce OSCQuery change d'état.
    let fleet_a_refaire =
        voulu.fleet != courant.fleet || (voulu.fleet && voulu.oscquery != courant.oscquery);
    if fleet_a_refaire {
        if let Some(actif) = actifs.remove("fleet") {
            arreter("fleet", actif).await;
        }
        if voulu.fleet {
            if let Some(actif) = demarrer_fleet(ctx, voulu.oscquery) {
                actifs.insert("fleet", actif);
            }
        }
    }

    *courant = voulu;
}

fn canal_arret() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

fn demarrer_osc(ctx: &Contexte) -> Actif {
    let (arret, rx) = canal_arret();
    let osc_config = toolbox_control_osc::OscConfig {
        bind: ctx.config.ports.bind.clone(),
        port: ctx.config.ports.osc,
    };
    let bus = ctx.handle.clone();
    let stop = rx.clone();
    let tache = spawn_service("osc", rx, async move {
        if let Err(err) = toolbox_control_osc::serve(osc_config, bus, stop).await {
            error!(%err, "le serveur OSC s'est arrêté en erreur");
        }
    });
    Actif::Service { arret, tache }
}

fn demarrer_oscquery(ctx: &Contexte) -> Actif {
    let (arret, rx) = canal_arret();
    let state = toolbox_control_http::oscquery::OscQueryState {
        bus: ctx.handle.clone(),
        node_name: ctx.node_name.clone(),
        osc_port: ctx.config.ports.osc,
    };
    let bind = ctx.config.ports.bind.clone();
    let port = ctx.config.ports.oscquery;
    let stop = rx.clone();
    let tache = spawn_service("oscquery", rx, async move {
        if let Err(err) = toolbox_control_http::oscquery::serve(bind, port, state, stop).await {
            error!(%err, "le serveur OSCQuery s'est arrêté en erreur");
        }
    });
    Actif::Service { arret, tache }
}

fn demarrer_feedback(ctx: &Contexte) -> Option<Actif> {
    let target = ctx.config.osc.feedback.clone()?;
    let (arret, rx) = canal_arret();
    let bus = ctx.handle.clone();
    let stop = rx.clone();
    let tache = spawn_service("osc-feedback", rx, async move {
        if let Err(err) = toolbox_control_osc::feedback(target, bus, stop).await {
            error!(%err, "le retour d'état OSC s'est arrêté en erreur");
        }
    });
    Some(Actif::Service { arret, tache })
}

fn demarrer_fader(ctx: &Contexte) -> Actif {
    let (arret, rx) = canal_arret();
    let tache = spawn_service(
        "fader",
        rx.clone(),
        toolbox_core::fader::run(
            ctx.handle.clone(),
            ctx.presets.clone(),
            ctx.mapping_presets.clone(),
            rx,
        ),
    );
    Actif::Service { arret, tache }
}

fn demarrer_fleet(ctx: &Contexte, oscquery_actif: bool) -> Option<Actif> {
    crate::fleet::spawn(
        ctx.node_name.clone(),
        ctx.config.ports.http,
        oscquery_actif.then_some(ctx.config.ports.oscquery),
        ctx.version.clone(),
        ctx.fleet_tx.clone(),
    )
    .map(Actif::Fleet)
}

#[cfg(feature = "midi")]
fn demarrer_midi(ctx: &Contexte) -> Option<Actif> {
    let (arret, rx) = canal_arret();
    let config = ctx.config.midi.clone();
    let bus = ctx.handle.clone();
    let stop = rx.clone();
    let tache = spawn_service("midi", rx, superviser_midi(config, bus, stop));
    Some(Actif::Service { arret, tache })
}

/// Superviseur MIDI : (re)connecte le port en boucle. Résout deux pièges de
/// terrain d'un seul mécanisme :
/// - au boot, si le contrôleur n'est pas encore énuméré (Pi qui démarre,
///   périphérique branché après), on retente au lieu de rester « actif mais
///   muet » sans reprise possible ;
/// - en spectacle, un débranchement USB rend le callback midir silencieux
///   SANS erreur : on le détecte en ré-énumérant les ports, on relâche la
///   connexion morte et on se reconnecte dès le retour du périphérique.
#[cfg(feature = "midi")]
async fn superviser_midi(
    config: toolbox_core::config::MidiSettings,
    bus: BusHandle,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        match toolbox_control_midi::connect(&config, bus.clone()) {
            Ok(service) => {
                let nom = service.port_name.clone();
                info!(port = %nom, "contrôleur MIDI connecté");
                loop {
                    tokio::select! {
                        _ = stop.changed() => return,
                        () = tokio::time::sleep(Duration::from_secs(3)) => {
                            match toolbox_control_midi::noms_ports() {
                                Ok(noms) if !noms.iter().any(|n| n == &nom) => {
                                    error!(port = %nom, "contrôleur MIDI débranché — reconnexion en cours");
                                    break;
                                }
                                // Présent, ou énumération transitoirement KO :
                                // on garde la connexion en place.
                                _ => {}
                            }
                        }
                    }
                }
                // La connexion morte est relâchée ici avant de retenter.
                drop(service);
            }
            Err(err) => {
                warn!(%err, "MIDI : port introuvable, nouvelle tentative dans 3 s");
            }
        }
        tokio::select! {
            _ = stop.changed() => return,
            () = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
    }
}

#[cfg(not(feature = "midi"))]
fn demarrer_midi(_ctx: &Contexte) -> Option<Actif> {
    warn!("MIDI demandé mais ce binaire est compilé sans (feature `midi`)");
    None
}

/// Monte un player (backend selon la compilation) et ponte sa position vers
/// le canal stable de l'UI.
fn demarrer_player(ctx: &Contexte) -> Actif {
    #[cfg(feature = "gstreamer")]
    {
        match toolbox_gst::GstBackend::new(ctx.video_tx.clone()) {
            Ok(backend) => {
                info!("module player actif (backend GStreamer)");
                return monter_player(ctx, backend);
            }
            Err(err) => {
                error!(%err, "GStreamer indisponible — repli sur le backend simulé");
            }
        }
    }
    info!("module player actif (backend simulé — compiler avec `gstreamer` pour la vidéo)");
    monter_player(ctx, MemoryBackend::new(10.0, true))
}

fn monter_player<B: PlayerBackend + 'static>(ctx: &Contexte, backend: B) -> Actif {
    let (arret, rx) = canal_arret();
    let player = Player::new(backend, ctx.handle.clone(), &ctx.config.paths.media);
    let mut position_interne = player.position_watch();
    let position_stable = ctx.position_tx.clone();
    let mut stop = rx.clone();
    let tache = spawn_service("player", rx, async move {
        let pont = async {
            while position_interne.changed().await.is_ok() {
                let position = *position_interne.borrow();
                let _ = position_stable.send(position);
            }
        };
        tokio::select! {
            () = player.run() => {},
            () = pont => {},
            _ = stop.changed() => {},
        }
    });
    Actif::Service { arret, tache }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolbox_core::Bus;

    fn contexte(dir: &std::path::Path, config: NodeConfig, handle: BusHandle) -> Contexte {
        let presets = PresetStore::open(dir.join("presets")).expect("presets");
        let mapping_presets =
            MappingStore::open(dir.join("presets").join("mapping")).expect("mappings");
        let (video_tx, _video_rx) = watch::channel(None);
        let (position_tx, _position_rx) = watch::channel(PlaybackPosition::default());
        let (fleet_tx, _fleet_rx) = watch::channel(serde_json::Value::Array(Vec::new()));
        Contexte {
            handle,
            config,
            node_name: "test".into(),
            version: "test".into(),
            presets,
            mapping_presets,
            video_tx,
            position_tx: std::sync::Arc::new(position_tx),
            fleet_tx,
        }
    }

    /// Le contrôleur démarre et arrête réellement un service : le fader
    /// répond quand il est actif, plus du tout une fois coupé.
    #[tokio::test]
    async fn le_fader_suit_son_interrupteur() {
        let dir = tempfile::tempdir().expect("tempdir");
        let presets = PresetStore::open(dir.path().join("presets")).expect("presets");
        let bus = Bus::new(64, 256).with_presets(presets.clone());
        let handle = bus.handle();
        tokio::spawn(bus.run());

        let ctx = contexte(dir.path(), NodeConfig::default(), handle.clone());
        // Seul le fader est actif : pas de socket, pas de mDNS dans le test.
        let flags = FeatureFlags {
            player: false,
            output: false,
            osc: false,
            oscquery: false,
            osc_feedback: false,
            midi: false,
            fleet: false,
            fader: true,
            preview: false,
            artnet: false,
        };
        let (flags_tx, flags_rx) = watch::channel(flags);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(controleur(ctx, flags_rx, shutdown_rx));
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Fader actif : un fondu court aboutit.
        handle
            .send(
                toolbox_core::Source::Http,
                Command::SetVolume { volume: 0.2 },
            )
            .await;
        handle
            .send(
                toolbox_core::Source::Http,
                Command::PresetSave { name: "a".into() },
            )
            .await;
        handle
            .send(
                toolbox_core::Source::Http,
                Command::SetVolume { volume: 1.0 },
            )
            .await;
        handle
            .send(
                toolbox_core::Source::Http,
                Command::PresetFade {
                    name: "a".into(),
                    seconds: 0.1,
                },
            )
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if (handle.snapshot().player.volume - 0.2).abs() < 1e-5 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "fondu jamais fini");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Interrupteur coupé : le même fondu ne bouge plus rien.
        flags_tx
            .send(FeatureFlags {
                fader: false,
                ..flags
            })
            .expect("flags");
        tokio::time::sleep(Duration::from_millis(120)).await;
        handle
            .send(
                toolbox_core::Source::Http,
                Command::SetVolume { volume: 1.0 },
            )
            .await;
        handle
            .send(
                toolbox_core::Source::Http,
                Command::PresetFade {
                    name: "a".into(),
                    seconds: 0.1,
                },
            )
            .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            (handle.snapshot().player.volume - 1.0).abs() < 1e-5,
            "fader coupé : le volume ne doit plus bouger"
        );
    }
}
