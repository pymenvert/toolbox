//! Binaire du node. Pour l'instant : charge la config, démarre le bus,
//! attend Ctrl-C. Les modules (engine, control-http, control-osc…) viendront
//! se brancher ici au fil de la phase 1, chacun derrière son flag de config.

use std::path::PathBuf;
use std::process::ExitCode;

use tracing::{error, info};

use toolbox_core::{Bus, NodeConfig};

fn main() -> ExitCode {
    // Logs structurés dès la première ligne (exigence : page de logs — le
    // ring buffer + export WebSocket arrivent en P1.0, même socle tracing).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "arrêt sur erreur");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
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
    info!(
        config = %config_path.display(),
        name = config.name.as_deref().unwrap_or("(hostname)"),
        "toolbox-node v{} démarré",
        env!("CARGO_PKG_VERSION")
    );

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let bus = Bus::new(256, 1024);
        let handle = bus.handle();
        let bus_task = tokio::spawn(bus.run());

        info!("prêt — Ctrl-C pour arrêter");
        tokio::signal::ctrl_c().await?;
        info!("arrêt demandé");

        // Arrêt propre : on ferme le dernier émetteur, le bus draine sa file
        // et sort de sa boucle tout seul (testé dans core::bus).
        drop(handle);
        if bus_task.await.is_err() {
            error!("le bus s'est terminé anormalement");
        }
        Ok::<(), std::io::Error>(())
    })?;

    Ok(())
}
