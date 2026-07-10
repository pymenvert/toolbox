//! [`GstBackend`] : implémentation GStreamer du trait
//! [`toolbox_engine::PlayerBackend`].
//!
//! Pipeline : `playbin3` (décodage + audio système) avec un `appsink` RGBA
//! comme sortie vidéo. Chaque frame décodée est copiée (stride compacté) en
//! [`VideoFrame`] et publiée sur un canal `watch` — la fenêtre de sortie ne
//! peint que la dernière frame, le backend n'attend jamais le rendu.
//!
//! Pas de boucle GLib : les messages du bus GStreamer (fin de média, erreurs)
//! sont dépilés dans [`PlayerBackend::take_events`], appelé par le tick du
//! player (200 ms) — même contrat que le backend mémoire.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video::VideoInfo;
use tokio::sync::watch;
use tracing::{info, warn};

use toolbox_engine::{BackendEvent, PlayerBackend, PlayerError, VideoFrame};

/// Backend GStreamer. Créé une fois au démarrage du node.
pub struct GstBackend {
    playbin: gst::Element,
    frames: watch::Sender<Option<VideoFrame>>,
    /// URI chargée (None tant qu'aucun `load` n'a réussi) — le transport,
    /// lui, appartient au player (miroir de l'état du bus).
    uri: Option<String>,
    /// Boucle sans coupure demandée (lue par le signal `about-to-finish`,
    /// qui tourne sur un thread GStreamer).
    gapless: Arc<AtomicBool>,
}

/// Mode portable : si les plugins GStreamer sont livrés à côté de l'exe
/// (`lib/gstreamer-1.0`, cas du pack Windows autonome), on les déclare AVANT
/// `gst::init()` — aucune installation système requise. Une variable
/// `GST_PLUGIN_PATH` déjà posée par l'utilisateur reste prioritaire.
fn declare_bundled_plugins() {
    if std::env::var_os("GST_PLUGIN_PATH").is_some() {
        return;
    }
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    else {
        return;
    };
    let plugins = exe_dir.join("lib").join("gstreamer-1.0");
    if plugins.is_dir() {
        info!(dossier = %plugins.display(), "plugins GStreamer embarqués détectés");
        std::env::set_var("GST_PLUGIN_PATH", &plugins);
    }
}

impl GstBackend {
    /// Initialise GStreamer et construit le pipeline. Échoue proprement si
    /// le runtime GStreamer n'est ni installé ni livré à côté de l'exe.
    pub fn new(frames: watch::Sender<Option<VideoFrame>>) -> Result<Self, PlayerError> {
        declare_bundled_plugins();
        gst::init().map_err(|e| PlayerError::Backend(format!("gstreamer absent : {e}")))?;

        // playbin3 (sélection auto des décodeurs, accélération matérielle
        // incluse quand la plateforme en a une — V4L2 sur Pi, D3D11 sous
        // Windows, VA-API sous Linux).
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("playbin").build())
            .map_err(|e| {
                PlayerError::Backend(format!(
                    "playbin indisponible (plugins-base manquant ?) : {e}"
                ))
            })?;

        // Sortie vidéo : appsink RGBA. `drop=true` + 2 tampons max : si le
        // rendu est plus lent que le décodage, on jette des frames au lieu
        // de bloquer le pipeline.
        let appsink = gst_app::AppSink::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .build(),
            )
            .max_buffers(2)
            .drop(true)
            .build();
        let frames_cb = frames.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    match frame_from_sample(&sample) {
                        Some(frame) => {
                            frames_cb.send_replace(Some(frame));
                            Ok(gst::FlowSuccess::Ok)
                        }
                        None => {
                            // Frame illisible : tracé, la lecture continue.
                            warn!("frame vidéo illisible (caps/stride inattendus)");
                            Ok(gst::FlowSuccess::Ok)
                        }
                    }
                })
                .build(),
        );
        playbin.set_property("video-sink", appsink.upcast_ref::<gst::Element>());

        // Boucle sans coupure : juste avant la fin du média, redonner la
        // même uri à playbin enchaîne la relecture SANS émettre de fin de
        // média — zéro hoquet au rebouclage (mode boucle « un »).
        let gapless = Arc::new(AtomicBool::new(false));
        let gapless_signal = gapless.clone();
        playbin.connect("about-to-finish", false, move |values| {
            if !gapless_signal.load(Ordering::Relaxed) {
                return None;
            }
            let playbin = values.first().and_then(|v| v.get::<gst::Element>().ok())?;
            if let Some(uri) = playbin.property::<Option<String>>("current-uri") {
                playbin.set_property("uri", uri);
            }
            None
        });

        let version = gst::version_string();
        info!(%version, "backend GStreamer initialisé");
        Ok(Self {
            playbin,
            frames,
            uri: None,
            gapless,
        })
    }

    fn set_state(&self, state: gst::State) -> Result<(), PlayerError> {
        self.playbin
            .set_state(state)
            .map(|_| ())
            .map_err(|e| PlayerError::Backend(format!("changement d'état {state:?} : {e}")))
    }

    fn query_seconds(&self, position: bool) -> Option<f64> {
        self.uri.as_ref()?;
        let time: Option<gst::ClockTime> = if position {
            self.playbin.query_position()
        } else {
            self.playbin.query_duration()
        };
        time.map(|t| t.nseconds() as f64 / 1e9)
    }
}

impl PlayerBackend for GstBackend {
    fn load(&mut self, path: &Path) -> Result<(), PlayerError> {
        // Chemin absolu exigé par filename_to_uri ; un fichier absent est
        // refusé ici (même contrat que le backend mémoire en mode fichiers).
        let absolute = std::fs::canonicalize(path).map_err(|e| {
            PlayerError::Media(format!("fichier introuvable : {} ({e})", path.display()))
        })?;
        let uri = gst::glib::filename_to_uri(&absolute, None)
            .map_err(|e| PlayerError::Media(format!("uri impossible : {e}")))?;

        self.set_state(gst::State::Null)?;
        self.frames.send_replace(None); // pas de frame périmée de l'ancien média
        self.playbin.set_property("uri", uri.as_str());
        // Preroll en pause : le média est prêt, position 0, aucune lecture.
        self.set_state(gst::State::Paused)?;
        self.uri = Some(uri.to_string());
        info!(media = %path.display(), "média chargé (GStreamer)");
        Ok(())
    }

    fn play(&mut self) -> Result<(), PlayerError> {
        if self.uri.is_none() {
            return Err(PlayerError::Backend("play sans média".into()));
        }
        self.set_state(gst::State::Playing)?;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), PlayerError> {
        if self.uri.is_some() {
            self.set_state(gst::State::Paused)?;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlayerError> {
        if self.uri.is_some() {
            // Ready libère les décodeurs (précieux sur Pi) ; le prochain
            // play repart du début, comme le backend mémoire.
            self.set_state(gst::State::Ready)?;
        }
        self.frames.send_replace(None); // sortie noire à l'arrêt
        Ok(())
    }

    fn seek(&mut self, seconds: f64) -> Result<(), PlayerError> {
        if self.uri.is_none() {
            return Err(PlayerError::Backend("seek sans média".into()));
        }
        let target = gst::ClockTime::from_nseconds((seconds.max(0.0) * 1e9) as u64);
        self.playbin
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target)
            .map_err(|e| PlayerError::Backend(format!("seek : {e}")))
    }

    fn set_volume(&mut self, volume: f32) -> Result<(), PlayerError> {
        self.playbin
            .set_property("volume", f64::from(volume.clamp(0.0, 1.0)));
        Ok(())
    }

    fn position_seconds(&self) -> Option<f64> {
        self.query_seconds(true)
    }

    fn duration_seconds(&self) -> Option<f64> {
        self.query_seconds(false)
    }

    fn set_gapless_loop(&mut self, enabled: bool) {
        self.gapless.store(enabled, Ordering::Relaxed);
    }

    fn take_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        if let Some(bus) = self.playbin.bus() {
            while let Some(message) = bus.pop() {
                match message.view() {
                    gst::MessageView::Eos(_) => events.push(BackendEvent::EndOfStream),
                    gst::MessageView::Error(err) => {
                        events.push(BackendEvent::Error(format!(
                            "{} ({})",
                            err.error(),
                            err.debug().unwrap_or_default()
                        )));
                    }
                    _ => {}
                }
            }
        }
        events
    }
}

impl Drop for GstBackend {
    fn drop(&mut self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

/// Copie une frame GStreamer en [`VideoFrame`] compacte (stride retiré).
fn frame_from_sample(sample: &gst::Sample) -> Option<VideoFrame> {
    let caps = sample.caps()?;
    let info = VideoInfo::from_caps(caps).ok()?;
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;

    let (width, height) = (info.width(), info.height());
    let stride = usize::try_from(info.stride().first().copied()?).ok()?;
    let row = width as usize * 4;
    if stride < row || map.len() < stride * (height as usize - 1) + row {
        return None;
    }
    let mut rgba = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        rgba.extend_from_slice(&map[start..start + row]);
    }
    VideoFrame::new(width, height, rgba.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test : le backend se construit (runtime GStreamer requis — ce
    /// test tourne dans le job CI dédié) et refuse un fichier absent.
    #[test]
    fn backend_builds_and_rejects_missing_file() {
        let (frames_tx, _frames_rx) = watch::channel(None);
        let mut backend = match GstBackend::new(frames_tx) {
            Ok(backend) => backend,
            Err(err) => {
                // Machine sans GStreamer : le constructeur échoue proprement,
                // c'est exactement le contrat (le node se replie en mémoire).
                eprintln!("GStreamer indisponible ici : {err}");
                return;
            }
        };
        assert!(matches!(
            backend.load(Path::new("/nulle/part/fantome.mp4")),
            Err(PlayerError::Media(_))
        ));
        // Sans média : play/seek refusés, stop/volume tolérés.
        assert!(backend.play().is_err());
        assert!(backend.seek(1.0).is_err());
        assert!(backend.stop().is_ok());
        assert!(backend.set_volume(0.5).is_ok());
        assert_eq!(backend.position_seconds(), None);
        assert!(backend.take_events().is_empty());
    }
}
