//! Sortie RTSP : la sortie composée du node (mapping, couleur, LUT,
//! blackout — la même image que la fenêtre, rendue par la référence CPU)
//! servie en `rtsp://node:port/sortie` via gst-rtsp-server.
//!
//! Encodage H.264 (`x264enc`) si le plugin est présent, MJPEG sinon —
//! les deux se lisent dans VLC, OBS et les régies. Pipeline PARTAGÉ :
//! dix clients ne coûtent qu'un seul rendu + un seul encodage.
//!
//! Cette machine de dev n'a pas GStreamer : ce module est compilé et
//! testé par le job CI `check-gstreamer` (test DESCRIBE en TCP brut).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_rtsp_server::prelude::*;
use tokio::sync::watch;
use tracing::{error, info, warn};

use toolbox_core::NodeState;
use toolbox_engine::VideoFrame;

/// Réglages du serveur (copie de `[rtsp]` de node.toml).
#[derive(Debug, Clone)]
pub struct RtspConfig {
    /// Port TCP d'écoute (`0` = éphémère, pour les tests).
    pub port: u16,
    pub largeur: u32,
    pub hauteur: u32,
    pub fps: u32,
}

/// Poignée du serveur : arrêt propre + port réellement lié.
pub struct RtspHandle {
    main_loop: glib::MainLoop,
    thread: Option<std::thread::JoinHandle<()>>,
    port: u16,
    arret: Arc<AtomicBool>,
}

impl RtspHandle {
    /// Port TCP réellement lié (utile quand `port = 0`).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Arrête le serveur et attend la fin de son thread.
    pub fn arreter(mut self) {
        self.arret.store(true, Ordering::Relaxed);
        self.main_loop.quit();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                warn!("thread RTSP terminé en panique");
            }
        }
    }
}

/// Démarre le serveur RTSP dans son propre thread (GLib MainLoop) et
/// retourne quand il écoute — ou une erreur s'il n'a pas pu se lier.
pub fn demarrer(
    config: RtspConfig,
    state: watch::Receiver<NodeState>,
    video: watch::Receiver<Option<VideoFrame>>,
) -> Result<RtspHandle, String> {
    gstreamer::init().map_err(|e| format!("init GStreamer : {e}"))?;

    let (largeur, hauteur) = (
        config.largeur.clamp(64, 3840),
        config.hauteur.clamp(64, 2160),
    );
    let fps = config.fps.clamp(1, 60);

    // H.264 si l'encodeur est là, sinon MJPEG (plugins good uniquement).
    let h264 = gstreamer::ElementFactory::find("x264enc").is_some();
    let caps = format!("video/x-raw,format=RGB,width={largeur},height={hauteur},framerate={fps}/1");
    let lancement = if h264 {
        format!(
            "( appsrc name=source is-live=true do-timestamp=true format=time caps={caps} \
             ! videoconvert ! x264enc tune=zerolatency bitrate=4096 key-int-max={gop} \
             ! rtph264pay name=pay0 pt=96 )",
            gop = fps * 2
        )
    } else {
        format!(
            "( appsrc name=source is-live=true do-timestamp=true format=time caps={caps} \
             ! videoconvert ! jpegenc quality=80 ! rtpjpegpay name=pay0 pt=26 )"
        )
    };

    let main_loop = glib::MainLoop::new(None, false);
    let arret = Arc::new(AtomicBool::new(false));
    let (port_tx, port_rx) = std::sync::mpsc::channel::<Result<u16, String>>();

    let boucle = main_loop.clone();
    let arret_thread = arret.clone();
    let thread = std::thread::Builder::new()
        .name("toolbox-rtsp".into())
        .spawn(move || {
            let serveur = gstreamer_rtsp_server::RTSPServer::new();
            serveur.set_service(&config.port.to_string());

            let fabrique = gstreamer_rtsp_server::RTSPMediaFactory::new();
            fabrique.set_launch(&lancement);
            // Partagé : tous les clients regardent le même pipeline.
            fabrique.set_shared(true);

            // À la préparation du média : brancher le pousseur de frames
            // sur l'appsrc (un par pipeline, donc un pour tous en partagé).
            let etat_pousseur = state.clone();
            let video_pousseur = video.clone();
            let arret_pousseur = arret_thread.clone();
            fabrique.connect_media_configure(move |_, media| {
                let Ok(bin) = media.element().downcast::<gstreamer::Bin>() else {
                    error!("média RTSP sans bin — pousseur non branché");
                    return;
                };
                let Some(source) = bin.by_name_recurse_up("source") else {
                    error!("appsrc « source » introuvable dans le média RTSP");
                    return;
                };
                let Ok(appsrc) = source.downcast::<gstreamer_app::AppSrc>() else {
                    error!("élément « source » n'est pas un appsrc");
                    return;
                };
                // Contre-pression : sans borne, si l'encodeur (x264 sur un Pi
                // un peu juste) ne tient pas la cadence, chaque frame poussée
                // s'empile dans l'appsrc et la mémoire monte jusqu'à l'OOM
                // (crash du node). On borne la file interne à quelques frames
                // et on met l'appsrc en mode bloquant : le pousseur ralentit
                // alors à la vitesse réelle de l'encodeur, mémoire bornée.
                let max_octets = u64::from(largeur) * u64::from(hauteur) * 4 * 4;
                appsrc.set_property("max-bytes", max_octets);
                appsrc.set_property("block", true);
                pousser_frames(
                    appsrc,
                    etat_pousseur.clone(),
                    video_pousseur.clone(),
                    arret_pousseur.clone(),
                    largeur,
                    hauteur,
                    fps,
                );
            });

            let Some(montages) = serveur.mount_points() else {
                let _ = port_tx.send(Err("points de montage RTSP indisponibles".into()));
                return;
            };
            montages.add_factory("/sortie", fabrique);

            // attach = écoute effective ; le port réel n'est connu qu'après.
            let contexte = glib::MainContext::default();
            let _garde = contexte.acquire();
            match serveur.attach(Some(&contexte)) {
                Ok(_id) => {
                    let port = u16::try_from(serveur.bound_port().max(0)).unwrap_or(0);
                    info!(port, h264, "sortie RTSP prête : rtsp://<ip>:{port}/sortie");
                    let _ = port_tx.send(Ok(port));
                }
                Err(err) => {
                    let _ = port_tx.send(Err(format!("écoute RTSP impossible : {err}")));
                    return;
                }
            }
            boucle.run();
            info!("sortie RTSP arrêtée");
        })
        .map_err(|e| format!("thread RTSP : {e}"))?;

    // Le démarrage est court : 5 s couvrent largement un attach.
    let port = match port_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(port)) => port,
        Ok(Err(err)) => {
            // Le thread s'est terminé de lui-même juste après l'envoi :
            // le joindre est immédiat et ne laisse rien derrière.
            let _ = thread.join();
            return Err(err);
        }
        Err(_) => {
            // Thread bloqué avant l'écoute (GLib souffrant) : on demande
            // l'arrêt sans le joindre — un join ici bloquerait l'appelant
            // aussi longtemps que le thread. Marqué, il se terminera s'il
            // se débloque, et le warn rend la fuite visible dans les logs.
            arret.store(true, Ordering::Relaxed);
            main_loop.quit();
            warn!("serveur RTSP bloqué au démarrage — thread abandonné (voir logs GStreamer)");
            return Err("le serveur RTSP n'a pas démarré à temps".into());
        }
    };

    Ok(RtspHandle {
        main_loop,
        thread: Some(thread),
        port,
        arret,
    })
}

/// Pousse les frames composées dans l'appsrc, à la cadence demandée, dans
/// un thread dédié — jusqu'à l'arrêt du serveur ou du pipeline (Flushing
/// quand le dernier client part : fin propre, un nouveau client relance).
fn pousser_frames(
    appsrc: gstreamer_app::AppSrc,
    state: watch::Receiver<NodeState>,
    video: watch::Receiver<Option<VideoFrame>>,
    arret: Arc<AtomicBool>,
    largeur: u32,
    hauteur: u32,
    fps: u32,
) {
    let resultat = std::thread::Builder::new()
        .name("toolbox-rtsp-frames".into())
        .spawn(move || {
            let periode = std::time::Duration::from_millis(u64::from(1000 / fps.max(1)));
            // Rendu composé partagé avec la sortie KMS : tampons réutilisés,
            // état recopié seulement quand le bus a changé, cache de LUT.
            let mut compositeur =
                toolbox_engine::composite::Compositeur::new(state, video, largeur, hauteur);
            loop {
                if arret.load(Ordering::Relaxed) {
                    break;
                }
                let tic = std::time::Instant::now();
                let rgb = compositeur.frame();
                let tampon = gstreamer::Buffer::from_slice(rgb.to_vec());
                if appsrc.push_buffer(tampon).is_err() {
                    // Pipeline en Flushing : plus de client, on s'arrête.
                    break;
                }
                if let Some(reste) = periode.checked_sub(tic.elapsed()) {
                    std::thread::sleep(reste);
                }
            }
        });
    if let Err(err) = resultat {
        error!(%err, "thread des frames RTSP impossible à créer");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le serveur répond à un DESCRIBE RTSP réel (TCP brut) avec un SDP —
    /// preuve que le pipeline preroll (l'appsrc pousse bien des frames).
    #[test]
    fn le_serveur_repond_a_describe() {
        use std::io::{Read, Write};

        let (_etat_tx, etat_rx) = watch::channel(NodeState::default());
        let (_video_tx, video_rx) = watch::channel::<Option<VideoFrame>>(None);
        let handle = demarrer(
            RtspConfig {
                port: 0, // éphémère : le test ne se bat pas pour 8554
                largeur: 320,
                hauteur: 180,
                fps: 10,
            },
            etat_rx,
            video_rx,
        )
        .expect("serveur RTSP");
        let port = handle.port();
        assert_ne!(port, 0, "port réellement lié");

        let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connexion");
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("timeout");
        write!(
            tcp,
            "DESCRIBE rtsp://127.0.0.1:{port}/sortie RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n"
        )
        .expect("requête");

        let mut reponse = Vec::new();
        let mut morceau = [0u8; 4096];
        // On lit jusqu'à voir le SDP (m=video) ou la fermeture.
        while let Ok(n) = tcp.read(&mut morceau) {
            if n == 0 {
                break;
            }
            reponse.extend_from_slice(&morceau[..n]);
            if reponse.windows(7).any(|w| w == b"m=video") {
                break;
            }
        }
        let texte = String::from_utf8_lossy(&reponse);
        assert!(
            texte.starts_with("RTSP/1.0 200 OK"),
            "réponse : {}",
            &texte[..texte.len().min(200)]
        );
        assert!(texte.contains("application/sdp"), "pas de SDP : {texte}");
        assert!(texte.contains("m=video"), "SDP sans piste vidéo : {texte}");

        handle.arreter();
    }
}
