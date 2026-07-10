//! Binaire du node : charge la config, démarre le bus et branche les modules
//! activés (player, HTTP+web UI, OSC, MIDI). Arrêt propre sur Ctrl-C.
//!
//! Usage : `toolbox-node [chemin/vers/node.toml]` — sans argument, lit
//! `./node.toml` s'il existe, sinon démarre sur les défauts (mode portable).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use toolbox_core::{
    Bus, BusHandle, Command, LogBuffer, MappingStore, MediaLibrary, NodeConfig, OutputSettings,
    PresetStore, Source,
};
use toolbox_engine::{MemoryBackend, PlaybackPosition, Player, PlayerBackend};

mod fleet;
mod journal;
mod supervision;

use supervision::spawn_service;

fn main() -> ExitCode {
    // `_file_guard` doit vivre jusqu'à la fin du process : à sa chute, les
    // dernières lignes du journal sur disque sont vidées.
    let (config, config_path, logs, _file_guard) = match bootstrap() {
        Ok(parts) => parts,
        Err(err) => {
            eprintln!("toolbox-node : {err}");
            return ExitCode::FAILURE;
        }
    };

    // Tout panic est journalisé (donc visible dans la page de logs) avant
    // que le process ne tombe — un crash muet est interdit.
    std::panic::set_hook(Box::new(|info| {
        error!("PANIC : {info}");
        eprintln!("PANIC : {info}");
    }));

    info!(
        config = %config_path.display(),
        name = config.name.as_deref().unwrap_or("(hostname)"),
        "toolbox-node v{} démarré",
        env!("CARGO_PKG_VERSION")
    );

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            error!(%err, "impossible de démarrer le runtime tokio");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(config, logs)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "arrêt sur erreur");
            ExitCode::FAILURE
        }
    }
}

/// Charge la config PUIS installe le logging (la taille du ring buffer de la
/// page de logs vient de la config).
type Bootstrap = (
    NodeConfig,
    PathBuf,
    LogBuffer,
    Option<tracing_appender::non_blocking::WorkerGuard>,
);

fn bootstrap() -> Result<Bootstrap, Box<dyn std::error::Error>> {
    let explicit_path = std::env::args().nth(1).map(PathBuf::from);
    let config_path = explicit_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("node.toml"));

    // Une config demandée explicitement mais absente est une erreur (une typo
    // ne doit pas lancer silencieusement le node sur la config par défaut).
    // Le node.toml implicite absent, lui, est le cas nominal du mode portable.
    if explicit_path.is_some() && !config_path.exists() {
        return Err(format!("config introuvable : {}", config_path.display()).into());
    }
    let config = NodeConfig::load(&config_path)?;

    // Journal sur disque (un fichier par jour dans paths.logs). Un disque
    // en lecture seule ou plein n'empêche pas le node de démarrer.
    let (file_layer, file_guard) = match journal::disk_writer(&config.paths.logs) {
        Ok((writer, guard)) => (
            Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false),
            ),
            Some(guard),
        ),
        Err(err) => {
            eprintln!(
                "toolbox-node : journal sur disque indisponible ({}) : {err}",
                config.paths.logs.display()
            );
            (None, None)
        }
    };

    let logs = LogBuffer::new(config.limits.log_buffer);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(file_layer)
        .with(logs.layer())
        .init();

    Ok((config, config_path, logs, file_guard))
}

async fn run(config: NodeConfig, logs: LogBuffer) -> Result<(), Box<dyn std::error::Error>> {
    let node_name = config
        .name
        .clone()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "toolbox-node".to_string());

    // Stockage : presets (état complet + mapping seul) et médiathèque
    // (dossiers créés si besoin).
    let presets = PresetStore::open(&config.paths.presets)?;
    let mapping_presets = MappingStore::open(config.paths.presets.join("mapping"))?;
    let media = MediaLibrary::open(
        &config.paths.media,
        config.limits.max_upload_mb.saturating_mul(1024 * 1024),
    )?;

    // Le bus, cœur du node.
    let bus = Bus::new(256, 1024)
        .with_presets(presets.clone())
        .with_mapping_presets(mapping_presets.clone());
    let handle = bus.handle();
    let bus_task = tokio::spawn(bus.run());

    // Signal d'arrêt partagé par tous les services.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut services: Vec<(&'static str, tokio::task::JoinHandle<()>)> = Vec::new();

    // Fader : mène les fondus entre presets (`preset_fade`) en commandes
    // interpolées sur le bus.
    services.push((
        "fader",
        spawn_service(
            "fader",
            shutdown_rx.clone(),
            toolbox_core::fader::run(handle.clone(), presets.clone(), shutdown_rx.clone()),
        ),
    ));

    // Canaux de la sortie vidéo : frames décodées (backend → fenêtre),
    // réglages à chaud (API web → fenêtre, initialisés depuis [output]) et
    // écrans détectés (fenêtre → API web).
    let (video_tx, video_rx) = watch::channel::<Option<toolbox_engine::VideoFrame>>(None);
    // Réglages de sortie : les changements faits dans l'UI sont persistés
    // dans sortie.json (à côté de node.toml) et repris au démarrage —
    // sinon défauts de la section [output].
    let output_settings_path = std::path::PathBuf::from("sortie.json");
    let initial_output = OutputSettings::load(&output_settings_path).unwrap_or(OutputSettings {
        monitor: config.output.monitor,
        fullscreen: config.output.fullscreen,
    });
    let (output_settings_tx, output_settings_rx) = watch::channel(initial_output);
    let output_settings_tx = std::sync::Arc::new(output_settings_tx);
    {
        let mut changes = output_settings_rx.clone();
        let path = output_settings_path.clone();
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let settings = *changes.borrow_and_update();
                if let Err(err) = settings.save(&path) {
                    warn!(%err, "réglages de sortie non persistés");
                }
            }
        });
    }
    let (monitors_tx, monitors_rx) = watch::channel(Vec::new());
    let (fps_tx, fps_rx) = watch::channel(0.0f32);

    // Découverte réseau du parc (mDNS) : annonce + écoute, liste publiée
    // pour /api/fleet. Sans réseau multicast, le node fonctionne sans.
    let (fleet_tx, fleet_rx) = watch::channel(serde_json::Value::Array(Vec::new()));
    fleet::spawn(
        node_name.clone(),
        config.ports.http,
        env!("CARGO_PKG_VERSION").to_string(),
        fleet_tx,
    );

    // Player. Backend GStreamer si compilé et disponible (vidéo réelle dans
    // la fenêtre de sortie), sinon backend mémoire : position, durée
    // (simulée à 10 s), fin de média, boucles et playlist fonctionnent
    // réellement — seule l'image manque.
    let position_rx = if config.modules.player {
        #[cfg(feature = "gstreamer")]
        {
            match toolbox_gst::GstBackend::new(video_tx) {
                Ok(backend) => {
                    info!("module player actif (backend GStreamer)");
                    spawn_player(backend, &handle, &config, &shutdown_rx, &mut services)
                }
                Err(err) => {
                    error!(%err, "GStreamer indisponible — repli sur le backend simulé");
                    spawn_player(
                        MemoryBackend::new(10.0, true),
                        &handle,
                        &config,
                        &shutdown_rx,
                        &mut services,
                    )
                }
            }
        }
        #[cfg(not(feature = "gstreamer"))]
        {
            drop(video_tx); // pas de producteur de frames dans ce binaire
            info!("module player actif (backend simulé — compiler avec `gstreamer` pour la vidéo)");
            spawn_player(
                MemoryBackend::new(10.0, true),
                &handle,
                &config,
                &shutdown_rx,
                &mut services,
            )
        }
    } else {
        drop(video_tx);
        watch::channel(PlaybackPosition::default()).1
    };

    // HTTP : REST + WebSocket + web UI + monitoring.
    if config.modules.http {
        let app = toolbox_control_http::AppState::new(
            handle.clone(),
            presets.clone(),
            mapping_presets.clone(),
            media.clone(),
            logs.clone(),
            position_rx.clone(),
            shutdown_rx.clone(),
            toolbox_control_http::OutputControl {
                monitors: monitors_rx.clone(),
                settings: output_settings_tx.clone(),
                fps: fps_rx.clone(),
                video: video_rx.clone(),
            },
            fleet_rx.clone(),
            node_name.clone(),
            env!("CARGO_PKG_VERSION").to_string(),
        )
        .with_password(config.security.password.clone());
        if config.security.password.is_some() {
            info!("interface web protégée par mot de passe ([security])");
        }
        let http_config = toolbox_control_http::HttpConfig {
            bind: config.ports.bind.clone(),
            port: config.ports.http,
            node_name: node_name.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let shutdown = shutdown_rx.clone();
        services.push((
            "http",
            spawn_service("http", shutdown_rx.clone(), async move {
                if let Err(err) = toolbox_control_http::serve(http_config, app, shutdown).await {
                    error!(%err, "le serveur HTTP s'est arrêté en erreur");
                }
            }),
        ));
    }

    // OSC + OSCQuery (auto-découverte des paramètres pour Chataigne).
    if config.modules.osc {
        let osc_config = toolbox_control_osc::OscConfig {
            bind: config.ports.bind.clone(),
            port: config.ports.osc,
        };
        let osc_bus = handle.clone();
        let shutdown = shutdown_rx.clone();
        services.push((
            "osc",
            spawn_service("osc", shutdown_rx.clone(), async move {
                if let Err(err) = toolbox_control_osc::serve(osc_config, osc_bus, shutdown).await {
                    error!(%err, "le serveur OSC s'est arrêté en erreur");
                }
            }),
        ));

        // Retour d'état vers Chataigne (les curseurs suivent le node).
        if let Some(target) = config.osc.feedback.clone() {
            let feedback_bus = handle.clone();
            let shutdown = shutdown_rx.clone();
            services.push((
                "osc-feedback",
                spawn_service("osc-feedback", shutdown_rx.clone(), async move {
                    if let Err(err) =
                        toolbox_control_osc::feedback(target, feedback_bus, shutdown).await
                    {
                        error!(%err, "le retour d'état OSC s'est arrêté en erreur");
                    }
                }),
            ));
        }

        let oscquery_state = toolbox_control_http::oscquery::OscQueryState {
            bus: handle.clone(),
            node_name: node_name.clone(),
            osc_port: config.ports.osc,
        };
        let bind = config.ports.bind.clone();
        let port = config.ports.oscquery;
        let shutdown = shutdown_rx.clone();
        services.push((
            "oscquery",
            spawn_service("oscquery", shutdown_rx.clone(), async move {
                if let Err(err) =
                    toolbox_control_http::oscquery::serve(bind, port, oscquery_state, shutdown)
                        .await
                {
                    error!(%err, "le serveur OSCQuery s'est arrêté en erreur");
                }
            }),
        ));
    }

    // Fenêtre de sortie : mires et vidéo warpées en direct.
    // Thread dédié ; si l'environnement graphique manque, le node continue.
    #[cfg(feature = "render")]
    let render_thread = if config.output.enabled {
        Some(toolbox_render::spawn(
            toolbox_render::WindowConfig {
                title: format!("Toolbox — sortie ({node_name})"),
                gpu: config.output.gpu,
            },
            toolbox_render::OutputChannels {
                state: handle.state_watch(),
                video: video_rx,
                settings: output_settings_rx,
                monitors: monitors_tx,
                fps: fps_tx,
                shutdown: shutdown_rx.clone(),
            },
        ))
    } else {
        info!("fenêtre de sortie désactivée par la config ([output] enabled = false)");
        None
    };
    #[cfg(not(feature = "render"))]
    {
        // Pas de fenêtre dans ce binaire : canaux de sortie sans consommateur.
        drop(video_rx);
        drop(output_settings_rx);
        drop(monitors_tx);
        drop(fps_tx);
        if config.output.enabled {
            warn!("fenêtre de sortie demandée mais ce binaire est compilé sans (feature `render`)");
        }
    }

    // MIDI (optionnel à la compilation : dépend d'ALSA sous Linux).
    #[cfg(feature = "midi")]
    let _midi = if config.modules.midi {
        match toolbox_control_midi::connect(&config.midi, handle.clone()) {
            Ok(service) => {
                info!(port = %service.port_name, "module MIDI actif");
                Some(service)
            }
            Err(err) => {
                error!(%err, "MIDI indisponible — le node continue sans");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(feature = "midi"))]
    if config.modules.midi {
        warn!("MIDI demandé dans la config mais ce binaire est compilé sans (feature `midi`)");
    }

    // Mode kiosque (P1.9) : preset de démarrage + lecture automatique.
    if let Some(preset) = &config.startup.preset {
        info!(preset = %preset, autoplay = config.startup.autoplay, "démarrage kiosque");
        handle
            .send(
                Source::Internal,
                Command::PresetLoad {
                    name: preset.clone(),
                },
            )
            .await;
        if config.startup.autoplay {
            handle.send(Source::Internal, Command::Play).await;
        }
    }

    info!(
        http = config.modules.http,
        osc = config.modules.osc,
        midi = config.modules.midi,
        player = config.modules.player,
        "prêt — Ctrl-C pour arrêter"
    );
    tokio::signal::ctrl_c().await?;
    info!("arrêt demandé");

    // Arrêt propre : signal aux services, fermeture du dernier émetteur du
    // bus, puis attente bornée (un service qui traîne n'empêche pas l'arrêt).
    let _ = shutdown_tx.send(true);
    drop(handle);
    drop(position_rx);

    for (name, task) in services {
        if tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .is_err()
        {
            warn!(service = name, "arrêt forcé après 5 s");
        }
    }
    if tokio::time::timeout(Duration::from_secs(5), bus_task)
        .await
        .is_err()
    {
        warn!("le bus ne s'est pas arrêté en 5 s");
    }
    #[cfg(feature = "render")]
    if let Some(thread) = render_thread {
        // L'event loop sort sur le signal d'arrêt (relais Wake::Quit).
        let join = tokio::task::spawn_blocking(move || {
            let _ = thread.join();
        });
        if tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .is_err()
        {
            warn!("la fenêtre de sortie ne s'est pas fermée en 5 s");
        }
    }
    info!("arrêt complet");
    Ok(())
}

/// Monte un player (backend quelconque) sur le bus dans une tâche dédiée et
/// retourne le canal de position de lecture.
fn spawn_player<B: PlayerBackend + 'static>(
    backend: B,
    handle: &BusHandle,
    config: &NodeConfig,
    shutdown_rx: &watch::Receiver<bool>,
    services: &mut Vec<(&'static str, tokio::task::JoinHandle<()>)>,
) -> watch::Receiver<PlaybackPosition> {
    let player = Player::new(backend, handle.clone(), &config.paths.media);
    let rx = player.position_watch();
    let mut shutdown = shutdown_rx.clone();
    services.push((
        "player",
        spawn_service("player", shutdown_rx.clone(), async move {
            tokio::select! {
                () = player.run() => {},
                _ = shutdown.changed() => {},
            }
        }),
    ));
    rx
}
