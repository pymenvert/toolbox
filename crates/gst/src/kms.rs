//! Sortie DRM/KMS : plein écran SANS environnement graphique (Raspberry
//! Pi OS Lite, console) via `kmssink`. C'est la brique « chaîne vidéo Pi »
//! préparée sans le matériel : compilée et construite par la CI, le run
//! réel reste À VALIDER SUR UN PI (voir le rapport).
//!
//! Pipeline : appsrc (frames composées par la référence CPU, tampons
//! réutilisés) → videoconvert → kmssink. `[output] mode = "kms"` l'active
//! à la place de la fenêtre ; le node continue sans sortie si /dev/dri
//! est absent (erreur claire dans les logs).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gstreamer::prelude::*;
use tokio::sync::watch;
use tracing::{error, info, warn};

use toolbox_core::NodeState;
use toolbox_engine::composite::Compositeur;
use toolbox_engine::VideoFrame;

/// Réglages de la sortie KMS.
#[derive(Debug, Clone)]
pub struct KmsConfig {
    pub largeur: u32,
    pub hauteur: u32,
    pub fps: u32,
}

/// Poignée : arrêt propre du pipeline et du pousseur.
pub struct KmsHandle {
    pipeline: gstreamer::Pipeline,
    arret: Arc<AtomicBool>,
    pousseur: Option<std::thread::JoinHandle<()>>,
}

impl KmsHandle {
    pub fn arreter(mut self) {
        self.arret.store(true, Ordering::Relaxed);
        if let Some(pousseur) = self.pousseur.take() {
            if pousseur.join().is_err() {
                warn!("pousseur KMS terminé en panique");
            }
        }
        if let Err(err) = self.pipeline.set_state(gstreamer::State::Null) {
            warn!(%err, "pipeline KMS non remis à zéro");
        }
    }
}

/// Construit le pipeline KMS (sans le démarrer) — séparé pour être
/// testable en CI, où kmssink existe mais /dev/dri est absent.
pub fn construire(config: &KmsConfig) -> Result<gstreamer::Pipeline, String> {
    gstreamer::init().map_err(|e| format!("init GStreamer : {e}"))?;
    if gstreamer::ElementFactory::find("kmssink").is_none() {
        return Err("kmssink absent (installer gstreamer1.0-plugins-bad)".into());
    }
    let (largeur, hauteur) = (
        config.largeur.clamp(64, 3840),
        config.hauteur.clamp(64, 2160),
    );
    let fps = config.fps.clamp(1, 60);
    let description = format!(
        "appsrc name=source is-live=true do-timestamp=true format=time \
         caps=video/x-raw,format=RGB,width={largeur},height={hauteur},framerate={fps}/1 \
         ! videoconvert ! queue max-size-buffers=2 leaky=downstream ! kmssink"
    );
    let pipeline = gstreamer::parse::launch(&description)
        .map_err(|e| format!("pipeline KMS : {e}"))?
        .downcast::<gstreamer::Pipeline>()
        .map_err(|_| "le pipeline KMS n'est pas un Pipeline".to_string())?;
    Ok(pipeline)
}

/// Démarre la sortie KMS : pipeline en lecture + thread pousseur de frames.
/// `enabled` suit l'interrupteur « fenêtre de sortie » de l'onglet
/// Fonctions : coupé, la sortie affiche du noir (rendu sauté).
pub fn demarrer(
    config: KmsConfig,
    state: watch::Receiver<NodeState>,
    video: watch::Receiver<Option<VideoFrame>>,
    enabled: watch::Receiver<bool>,
) -> Result<KmsHandle, String> {
    let pipeline = construire(&config)?;
    let (largeur, hauteur) = (
        config.largeur.clamp(64, 3840),
        config.hauteur.clamp(64, 2160),
    );
    let fps = config.fps.clamp(1, 60);

    let Some(source) = pipeline
        .by_name("source")
        .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
    else {
        return Err("appsrc « source » introuvable dans le pipeline KMS".into());
    };

    // Playing AVANT de pousser : si /dev/dri manque, on échoue ici avec une
    // erreur claire plutôt qu'en silence.
    pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|e| format!("sortie KMS impossible (pas de /dev/dri ? console requise) : {e}"))?;

    let arret = Arc::new(AtomicBool::new(false));
    let arret_pousseur = arret.clone();
    let pousseur = std::thread::Builder::new()
        .name("toolbox-kms".into())
        .spawn(move || {
            let periode = std::time::Duration::from_millis(u64::from(1000 / fps.max(1)));
            let mut compositeur = Compositeur::new(state, video, largeur, hauteur);
            let noir = vec![0u8; (largeur * hauteur * 3) as usize];
            info!(
                largeur,
                hauteur, fps, "sortie KMS active (plein écran console)"
            );
            loop {
                if arret_pousseur.load(Ordering::Relaxed) {
                    break;
                }
                let tic = std::time::Instant::now();
                let rgb = if *enabled.borrow() {
                    compositeur.frame()
                } else {
                    // Fonction « sortie » coupée : écran noir, rendu sauté.
                    &noir
                };
                let tampon = gstreamer::Buffer::from_slice(rgb.to_vec());
                if source.push_buffer(tampon).is_err() {
                    warn!("pipeline KMS en erreur — pousseur arrêté");
                    break;
                }
                if let Some(reste) = periode.checked_sub(tic.elapsed()) {
                    std::thread::sleep(reste);
                }
            }
        })
        .map_err(|e| format!("thread KMS : {e}"))?;

    // Les erreurs du pipeline (câble débranché, permission DRM…) sont
    // tracées : la page Logs de l'UI les montre.
    if let Some(bus) = pipeline.bus() {
        std::thread::Builder::new()
            .name("toolbox-kms-bus".into())
            .spawn(move || {
                for message in bus.iter_timed(gstreamer::ClockTime::NONE) {
                    if let gstreamer::MessageView::Error(err) = message.view() {
                        error!(erreur = %err.error(), "erreur de la sortie KMS");
                        break;
                    }
                }
            })
            .ok();
    }

    Ok(KmsHandle {
        pipeline,
        arret,
        pousseur: Some(pousseur),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le pipeline KMS se construit (parse + appsrc nommé) — le run réel
    /// exige /dev/dri, absent des runners : il attend le Pi.
    #[test]
    fn le_pipeline_kms_se_construit() {
        let config = KmsConfig {
            largeur: 1280,
            hauteur: 720,
            fps: 30,
        };
        match construire(&config) {
            Ok(pipeline) => {
                assert!(pipeline.by_name("source").is_some(), "appsrc nommé");
            }
            Err(err) => {
                // kmssink peut manquer selon l'environnement : le message
                // doit alors être actionnable, pas cryptique.
                assert!(err.contains("kmssink"), "erreur inattendue : {err}");
            }
        }
    }
}
