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
    Bus, Command, FeatureFlags, LogBuffer, MappingStore, MediaLibrary, NodeConfig, OutputSettings,
    PresetStore, Source,
};
use toolbox_engine::PlaybackPosition;

mod bascules;
mod fleet;
mod journal;
mod supervision;
mod sync;
mod telemetrie;

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

    // Tout panic est journalisé (donc visible dans la page de logs) ET
    // consigné dans crash.txt avant que le process ne tombe — un crash muet
    // est interdit. Le fichier sert au diagnostic local ; il n'est envoyé à
    // la télémétrie (au prochain démarrage) que si `[telemetrie] url` est
    // configurée.
    let chemin_crash = telemetrie::chemin_crash(&config.paths.logs);
    let nom_crash = config.name.clone().unwrap_or_else(|| "node".to_string());
    std::panic::set_hook(Box::new(move |info| {
        error!("PANIC : {info}");
        eprintln!("PANIC : {info}");
        telemetrie::ecrire_rapport(
            &chemin_crash,
            &telemetrie::rapport(env!("CARGO_PKG_VERSION"), &nom_crash, &info.to_string()),
        );
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

async fn run(mut config: NodeConfig, logs: LogBuffer) -> Result<(), Box<dyn std::error::Error>> {
    // Réglages de performance (carte « Réglages » de l'UI, reglages.json) :
    // appliqués par-dessus node.toml, comme sortie.json.
    if let Some(reglages) = toolbox_core::Reglages::load(std::path::Path::new("reglages.json")) {
        info!(
            profil = %reglages.profil,
            largeur = reglages.largeur,
            hauteur = reglages.hauteur,
            gpu = reglages.gpu,
            "réglages de performance appliqués (reglages.json)"
        );
        config.resolution = toolbox_core::Resolution::Fixed {
            width: reglages.largeur,
            height: reglages.hauteur,
        };
        config.output.gpu = reglages.gpu;
        config.output.kms_fps = reglages.kms_fps;
    }

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

    // Dossier des LUT .cube (étalonnage) : créé pour que l'utilisateur
    // puisse y déposer ses fichiers (ou via PUT /api/luts/<nom>).
    if let Err(err) = std::fs::create_dir_all("luts") {
        warn!(%err, "dossier luts/ non créé");
    }

    // Reste d'une mise à jour OTA réussie (ancien binaire, script) : nettoyé.
    toolbox_control_http::ota::nettoyer_apres_demarrage();

    // Rapport de crash en attente d'un précédent démarrage : envoyé en tâche
    // de fond, uniquement si la télémétrie est configurée (opt-in strict).
    {
        let url = config.telemetrie.url.clone();
        let chemin = telemetrie::chemin_crash(&config.paths.logs);
        tokio::spawn(async move {
            telemetrie::envoyer_rapport_en_attente(url.as_deref(), &chemin).await;
        });
    }

    // Le bus, cœur du node.
    let bus = Bus::new(256, 1024)
        .with_presets(presets.clone())
        .with_mapping_presets(mapping_presets.clone());
    let handle = bus.handle();
    let bus_task = tokio::spawn(bus.run());

    // Signal d'arrêt partagé par tous les services.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut services: Vec<(&'static str, tokio::task::JoinHandle<()>)> = Vec::new();

    // Canaux de la sortie vidéo : frames décodées (backend → fenêtre),
    // réglages à chaud (API web → fenêtre, initialisés depuis [output]) et
    // écrans détectés (fenêtre → API web).
    let (video_tx, video_rx) = watch::channel::<Option<toolbox_engine::VideoFrame>>(None);
    // Entrée NDI : fait de `ndi://Nom` une vraie source — le service dort
    // tant qu'aucun média ndi:// n'est demandé (la bibliothèque NDI n'est
    // chargée qu'à la première demande).
    let entree_ndi = match toolbox_ndi::entree::demarrer(
        handle.state_watch(),
        video_tx.clone(),
        config.ndi.bibliotheque.clone(),
    ) {
        Ok(h) => Some(h),
        Err(err) => {
            error!(%err, "entrée NDI indisponible — les sources ndi:// resteront noires");
            None
        }
    };
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

    // Parc mDNS : la liste publiée pour /api/fleet (le service lui-même est
    // démarré/arrêté par le contrôleur de fonctions).
    let (fleet_tx, fleet_rx) = watch::channel(serde_json::Value::Array(Vec::new()));

    // Position de lecture : canal STABLE — le player courant y est ponté par
    // le contrôleur, l'UI garde le même récepteur à travers les bascules.
    let (position_tx, position_rx) = watch::channel(PlaybackPosition::default());

    // Dérive de synchro publiée par le suiveur (page Santé). None = pas de
    // rôle suiveur ou pas encore de mesures.
    let (sync_derive_tx, sync_derive_rx) = watch::channel::<Option<f64>>(None);

    // Interrupteurs de fonctions (onglet « Fonctions ») : fonctions.json
    // prime, sinon défauts de la config ([modules]/[output]). Persistés à
    // chaque bascule, comme sortie.json.
    let features_path = std::path::PathBuf::from("fonctions.json");
    let initial_features =
        FeatureFlags::load(&features_path).unwrap_or_else(|| FeatureFlags::from_config(&config));
    let (features_tx, features_rx) = watch::channel(initial_features);
    let features_tx = std::sync::Arc::new(features_tx);
    {
        let mut changes = features_rx.clone();
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let flags = *changes.borrow_and_update();
                if let Err(err) = flags.save(&features_path) {
                    warn!(%err, "interrupteurs de fonctions non persistés");
                }
            }
        });
    }

    // Contrôleur : démarre/arrête à chaud chaque service selon les
    // interrupteurs (OSC, OSCQuery, retour d'état, MIDI, parc, fondus,
    // player). Une fonction coupée est réellement arrêtée.
    services.push((
        "bascules",
        spawn_service(
            "bascules",
            shutdown_rx.clone(),
            bascules::controleur(
                bascules::Contexte {
                    handle: handle.clone(),
                    config: config.clone(),
                    node_name: node_name.clone(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    presets: presets.clone(),
                    mapping_presets: mapping_presets.clone(),
                    video_tx,
                    position_tx: std::sync::Arc::new(position_tx),
                    fleet_tx,
                },
                features_rx.clone(),
                shutdown_rx.clone(),
            ),
        ),
    ));

    // Console lumières Art-Net (page Lumières). Le service vit toujours
    // (l'édition reste possible) ; l'interrupteur « Lumières » coupe
    // l'émission ET la socket via le canal `actif`.
    let lumieres_handle = {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(64);
        let (etat_tx, etat_rx) = watch::channel(toolbox_artnet::EtatLumieres::default());
        let (actif_tx, actif_rx) = watch::channel(initial_features.artnet);
        {
            let mut changes = features_rx.clone();
            tokio::spawn(async move {
                while changes.changed().await.is_ok() {
                    let flags = *changes.borrow_and_update();
                    let _ = actif_tx.send(flags.artnet);
                }
            });
        }
        services.push((
            "lumieres",
            spawn_service(
                "lumieres",
                shutdown_rx.clone(),
                toolbox_artnet::service(
                    std::path::PathBuf::from("lumieres.json"),
                    handle.clone(),
                    cmd_rx,
                    etat_tx,
                    actif_rx,
                    shutdown_rx.clone(),
                ),
            ),
        ));
        toolbox_artnet::LumieresHandle {
            commandes: cmd_tx,
            etat: etat_rx,
        }
    };

    // Séquenceur : cues (GO, enchaînements, heure fixe) — page Séquences.
    let sequenceur_handle = {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(64);
        let (etat_tx, etat_rx) =
            watch::channel(toolbox_core::sequenceur::EtatSequenceur::default());
        services.push((
            "sequenceur",
            spawn_service(
                "sequenceur",
                shutdown_rx.clone(),
                toolbox_core::sequenceur::service(
                    std::path::PathBuf::from("sequences.json"),
                    handle.clone(),
                    cmd_rx,
                    etat_tx,
                    shutdown_rx.clone(),
                ),
            ),
        ));
        toolbox_core::sequenceur::SequenceurHandle {
            commandes: cmd_tx,
            etat: etat_rx,
        }
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
        .with_password(config.security.password.clone())
        .with_fleet_token(config.security.fleet_token.clone())
        .with_features(features_tx.clone(), features_rx.clone())
        .with_lumieres(lumieres_handle.clone())
        .with_sequenceur(sequenceur_handle.clone())
        .with_sync_derive(sync_derive_rx.clone());
        if config.security.password.is_some() {
            info!("interface web protégée par mot de passe ([security])");
            // Honnêteté envers l'opérateur : l'OSCQuery (port 8081, pour
            // Chataigne) reste ouvert même avec un mot de passe — il expose
            // les VALEURS courantes en lecture seule. Isoler par le réseau
            // (Tailscale) si le LAN n'est pas de confiance.
            if config.modules.osc {
                warn!(
                    "note sécurité : l'OSCQuery (port {}) reste en accès libre malgré le mot de passe (lecture seule)",
                    config.ports.oscquery
                );
            }
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

    // Fenêtre de sortie : mires et vidéo warpées en direct.
    // Thread dédié ; si l'environnement graphique manque, le node continue.
    // L'interrupteur « fenêtre de sortie » (onglet Fonctions) est relayé en
    // canal bool dédié : coupée = fenêtre masquée + peintre détruit (zéro
    // rendu), réveillée à chaud. Le fil winit reste dormant entre deux.
    let (output_enabled_tx, output_enabled_rx) = watch::channel(initial_features.output);
    {
        let mut changes = features_rx.clone();
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let flags = *changes.borrow_and_update();
                let _ = output_enabled_tx.send(flags.output);
            }
        });
    }
    // Sortie RTSP (feature gstreamer) : la sortie composée servie en
    // rtsp://node:port/sortie — clone ses canaux AVANT la fenêtre.
    #[cfg(feature = "gstreamer")]
    let rtsp_handle = if config.rtsp.enabled {
        match toolbox_gst::rtsp::demarrer(
            toolbox_gst::rtsp::RtspConfig {
                port: config.rtsp.port,
                largeur: config.rtsp.largeur,
                hauteur: config.rtsp.hauteur,
                fps: config.rtsp.fps,
            },
            handle.state_watch(),
            video_rx.clone(),
        ) {
            Ok(h) => Some(h),
            Err(err) => {
                error!(%err, "sortie RTSP indisponible — le node continue sans");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(feature = "gstreamer"))]
    if config.rtsp.enabled {
        warn!("[rtsp] activé mais ce binaire est compilé sans la feature `gstreamer`");
    }

    // Sortie NDI : la sortie composée annoncée sur le réseau. Compilée
    // partout (la bibliothèque NDI est chargée à l'exécution) ; sans SDK
    // installé, message clair et le node continue.
    let ndi_handle = if config.ndi.sortie {
        match toolbox_ndi::demarrer(
            toolbox_ndi::NdiConfig {
                nom: config
                    .ndi
                    .nom
                    .clone()
                    .unwrap_or_else(|| format!("{node_name} (Lanterne)")),
                largeur: config.ndi.largeur,
                hauteur: config.ndi.hauteur,
                fps: config.ndi.fps,
                bibliotheque: config.ndi.bibliotheque.clone(),
            },
            handle.state_watch(),
            video_rx.clone(),
        ) {
            Ok(h) => Some(h),
            Err(err) => {
                error!(%err, "sortie NDI indisponible — le node continue sans");
                None
            }
        }
    } else {
        None
    };

    // Sortie DRM/KMS (plein écran console, Pi Lite) : remplace la fenêtre
    // quand `[output] mode = "kms"` et que le binaire embarque GStreamer.
    let mode_kms = config.output.mode == toolbox_core::SortieMode::Kms;
    #[cfg(feature = "gstreamer")]
    let kms_handle = if mode_kms {
        let (largeur, hauteur) = match &config.resolution {
            toolbox_core::Resolution::Fixed { width, height } => (*width, *height),
            toolbox_core::Resolution::Auto => (1920, 1080),
        };
        match toolbox_gst::kms::demarrer(
            toolbox_gst::kms::KmsConfig {
                largeur,
                hauteur,
                fps: config.output.kms_fps,
            },
            handle.state_watch(),
            video_rx.clone(),
            output_enabled_rx.clone(),
        ) {
            Ok(h) => Some(h),
            Err(err) => {
                error!(%err, "sortie KMS indisponible — le node continue sans sortie");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(feature = "gstreamer"))]
    if mode_kms {
        warn!("[output] mode = \"kms\" demandé mais ce binaire est compilé sans la feature `gstreamer`");
    }

    #[cfg(feature = "render")]
    let render_thread = if mode_kms {
        // La sortie KMS remplace la fenêtre : canaux fenêtre sans usage.
        drop(video_rx);
        drop(output_settings_rx);
        drop(monitors_tx);
        drop(fps_tx);
        drop(output_enabled_rx);
        None
    } else {
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
                enabled: output_enabled_rx,
                shutdown: shutdown_rx.clone(),
            },
        ))
    };
    #[cfg(not(feature = "render"))]
    {
        // Pas de fenêtre dans ce binaire : canaux de sortie sans consommateur.
        drop(video_rx);
        drop(output_settings_rx);
        drop(monitors_tx);
        drop(fps_tx);
        drop(output_enabled_rx);
        if initial_features.output && !mode_kms {
            warn!("fenêtre de sortie demandée mais ce binaire est compilé sans (feature `render`)");
        }
    }

    // Synchro multi-node niveau 2 : le maître publie son horloge de
    // lecture, les suiveurs se verrouillent dessus (voir module sync).
    match config.sync.role {
        toolbox_core::SyncRole::Maitre => {
            services.push((
                "sync-maitre",
                spawn_service(
                    "sync-maitre",
                    shutdown_rx.clone(),
                    sync::maitre(
                        config.sync.clone(),
                        handle.clone(),
                        position_rx.clone(),
                        shutdown_rx.clone(),
                    ),
                ),
            ));
        }
        toolbox_core::SyncRole::Suiveur => {
            services.push((
                "sync-suiveur",
                spawn_service(
                    "sync-suiveur",
                    shutdown_rx.clone(),
                    sync::suiveur(
                        config.sync.clone(),
                        handle.clone(),
                        position_rx.clone(),
                        sync_derive_tx,
                        shutdown_rx.clone(),
                    ),
                ),
            ));
        }
        toolbox_core::SyncRole::Aucun => {}
    }

    // Mode kiosque (P1.9 + passthrough V2.1) : preset de démarrage, source
    // par défaut (carte d'acquisition, NDI…), lecture automatique.
    // demarrage.json (bouton « état de démarrage » de l'UI) prime sur
    // [startup] de node.toml.
    let demarrage =
        toolbox_core::config::Startup::load_override(std::path::Path::new("demarrage.json"))
            .unwrap_or_else(|| config.startup.clone());
    if demarrage.preset.is_some() || demarrage.source.is_some() {
        info!(
            preset = ?demarrage.preset,
            source = ?demarrage.source,
            autoplay = demarrage.autoplay,
            "état de démarrage appliqué"
        );
    }
    if let Some(preset) = &demarrage.preset {
        handle
            .send(
                Source::Internal,
                Command::PresetLoad {
                    name: preset.clone(),
                },
            )
            .await;
    }
    if let Some(source) = &demarrage.source {
        handle
            .send(
                Source::Internal,
                Command::Load {
                    path: source.clone(),
                },
            )
            .await;
    }
    if demarrage.autoplay {
        handle.send(Source::Internal, Command::Play).await;
    }

    info!(
        http = config.modules.http,
        osc = initial_features.osc,
        midi = initial_features.midi,
        player = initial_features.player,
        "prêt — Ctrl-C pour arrêter (onglet Fonctions pour les bascules)"
    );
    attendre_arret().await?;
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
    #[cfg(feature = "gstreamer")]
    if let Some(rtsp) = rtsp_handle {
        let join = tokio::task::spawn_blocking(move || rtsp.arreter());
        if tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .is_err()
        {
            warn!("la sortie RTSP ne s'est pas arrêtée en 5 s");
        }
    }
    if let Some(ndi) = ndi_handle {
        let join = tokio::task::spawn_blocking(move || ndi.arreter());
        if tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .is_err()
        {
            warn!("la sortie NDI ne s'est pas arrêtée en 5 s");
        }
    }
    if let Some(entree) = entree_ndi {
        let join = tokio::task::spawn_blocking(move || entree.arreter());
        if tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .is_err()
        {
            warn!("l'entrée NDI ne s'est pas arrêtée en 5 s");
        }
    }
    #[cfg(feature = "gstreamer")]
    if let Some(kms) = kms_handle {
        let join = tokio::task::spawn_blocking(move || kms.arreter());
        if tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .is_err()
        {
            warn!("la sortie KMS ne s'est pas arrêtée en 5 s");
        }
    }
    info!("arrêt complet");
    Ok(())
}

/// Attend une demande d'arrêt : Ctrl-C partout, plus SIGTERM sous Unix —
/// c'est le signal qu'envoie systemd à `systemctl stop` ; sans lui, le
/// service serait tué au timeout sans arrêt propre.
async fn attendre_arret() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => {
                info!("SIGTERM reçu (systemctl stop)");
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
