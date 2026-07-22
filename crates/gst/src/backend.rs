//! [`GstBackend`] : implémentation GStreamer du trait
//! [`toolbox_engine::PlayerBackend`].
//!
//! Deux modes de pipeline selon la source (voir
//! [`toolbox_core::MediaSource`]) :
//! - **playbin3** pour les fichiers vidéo et les URL réseau (rtsp, srt,
//!   http…) : décodage + audio système, sortie vidéo vers un `appsink` RGBA ;
//! - **pipeline dédié** pour la capture locale (`capture://N` — webcam ou
//!   carte HDMI UVC), le NDI (`ndi://Nom`, plugin optionnel) et les images
//!   fixes (`imagefreeze`).
//!
//! Chaque frame décodée est copiée (stride compacté) en [`VideoFrame`] et
//! publiée sur un canal `watch` — la fenêtre de sortie ne peint que la
//! dernière frame, le backend n'attend jamais le rendu. Pas de boucle GLib :
//! les messages du bus (fin de média, erreurs) sont dépilés dans
//! [`PlayerBackend::take_events`], appelé par le tick du player (200 ms).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video::VideoInfo;
use tokio::sync::watch;
use tracing::{info, warn};

use toolbox_core::MediaSource;
use toolbox_engine::{BackendEvent, PlayerBackend, PlayerError, VideoFrame};

/// Extensions traitées comme images fixes (affichées en continu).
const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff"];

/// Backend GStreamer. Créé une fois au démarrage du node.
pub struct GstBackend {
    /// Pipeline fichiers/réseau (toujours construit).
    playbin: gst::Element,
    /// Pipeline dédié actif (capture, NDI, image) — prime sur playbin
    /// tant qu'il existe.
    custom: Option<gst::Element>,
    frames: watch::Sender<Option<VideoFrame>>,
    /// Source chargée (None tant qu'aucun `load` n'a réussi).
    source: Option<MediaSource>,
    /// Boucle sans coupure demandée (lue par le signal `about-to-finish`,
    /// qui tourne sur un thread GStreamer).
    gapless: Arc<AtomicBool>,
}

impl GstBackend {
    /// Initialise GStreamer et construit playbin. Échoue proprement si le
    /// runtime GStreamer n'est ni installé ni livré à côté de l'exe.
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
        let appsink = make_appsink(frames.clone());
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
            custom: None,
            frames,
            source: None,
            gapless,
        })
    }

    /// Le pipeline qui joue actuellement (dédié s'il existe, sinon playbin).
    fn active(&self) -> &gst::Element {
        self.custom.as_ref().unwrap_or(&self.playbin)
    }

    /// Arrête et démonte tout (les deux pipelines), sortie noire.
    fn teardown(&mut self) {
        let _ = self.playbin.set_state(gst::State::Null);
        if let Some(custom) = self.custom.take() {
            let _ = custom.set_state(gst::State::Null);
        }
        self.frames.send_replace(None);
    }

    fn set_state(&self, state: gst::State) -> Result<(), PlayerError> {
        self.active()
            .set_state(state)
            .map(|_| ())
            .map_err(|e| PlayerError::Backend(format!("changement d'état {state:?} : {e}")))
    }

    /// Construit un pipeline dédié depuis sa description et branche son
    /// appsink (nommé `sortie`) sur le canal de frames.
    fn build_custom(&self, description: &str) -> Result<gst::Element, PlayerError> {
        let pipeline = gst::parse::launch(description)
            .map_err(|e| PlayerError::Media(format!("pipeline source : {e}")))?;
        let bin = pipeline
            .clone()
            .downcast::<gst::Bin>()
            .map_err(|_| PlayerError::Backend("pipeline source inattendu".into()))?;
        let appsink = bin
            .by_name("sortie")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| PlayerError::Backend("appsink absent du pipeline".into()))?;
        attach_frame_callbacks(&appsink, self.frames.clone());
        Ok(pipeline)
    }

    fn query_seconds(&self, position: bool) -> Option<f64> {
        let source = self.source.as_ref()?;
        if source.is_live() {
            return None; // une capture ou un flux NDI n'a ni durée ni position
        }
        let time: Option<gst::ClockTime> = if position {
            self.active().query_position()
        } else {
            self.active().query_duration()
        };
        time.map(|t| t.nseconds() as f64 / 1e9)
    }
}

impl PlayerBackend for GstBackend {
    fn load(&mut self, path: &Path) -> Result<(), PlayerError> {
        let raw = path.to_string_lossy().to_string();
        // Les fichiers arrivent déjà résolus (media/…) donc en chemin
        // absolu-relatif machine : hors grammaire des sources → fichier.
        let source = MediaSource::parse(&raw).unwrap_or(MediaSource::File(raw.clone()));

        self.teardown();
        match &source {
            MediaSource::File(_) => {
                let absolute = std::fs::canonicalize(path).map_err(|e| {
                    PlayerError::Media(format!("fichier introuvable : {} ({e})", path.display()))
                })?;
                let uri = gst::glib::filename_to_uri(&absolute, None)
                    .map_err(|e| PlayerError::Media(format!("uri impossible : {e}")))?;
                if is_image(&absolute) {
                    // Image fixe : flux continu via imagefreeze.
                    let pipeline = self.build_custom(
                        "uridecodebin name=src ! imagefreeze ! videoconvert ! \
                         video/x-raw,format=RGBA ! appsink name=sortie max-buffers=2 drop=true",
                    )?;
                    if let Some(src) = pipeline
                        .clone()
                        .downcast::<gst::Bin>()
                        .ok()
                        .and_then(|b| b.by_name("src"))
                    {
                        src.set_property("uri", uri.as_str());
                    }
                    self.custom = Some(pipeline);
                } else {
                    self.playbin.set_property("uri", uri.as_str());
                }
            }
            MediaSource::Network(url) => {
                // playbin ouvre rtsp/srt/http directement (plugins requis).
                self.playbin.set_property("uri", url.as_str());
            }
            MediaSource::Capture(index) => {
                #[cfg(target_os = "windows")]
                let desc = format!(
                    "mfvideosrc device-index={index} ! videoconvert ! \
                     video/x-raw,format=RGBA ! appsink name=sortie max-buffers=2 drop=true sync=false"
                );
                #[cfg(target_os = "linux")]
                let desc = format!(
                    "v4l2src device=/dev/video{index} ! videoconvert ! \
                     video/x-raw,format=RGBA ! appsink name=sortie max-buffers=2 drop=true sync=false"
                );
                #[cfg(not(any(target_os = "windows", target_os = "linux")))]
                let desc = format!(
                    "autovideosrc ! videoconvert ! video/x-raw,format=RGBA ! \
                     appsink name=sortie max-buffers=2 drop=true sync=false # {index}"
                );
                self.custom = Some(self.build_custom(&desc)?);
            }
            MediaSource::Ndi(name) => {
                if gst::ElementFactory::find("ndisrc").is_none() {
                    return Err(PlayerError::Media(
                        "plugin NDI absent (gst-plugin-ndi + runtime NDI requis — voir le manuel)"
                            .into(),
                    ));
                }
                // `name` est validé par le core : ni guillemet ni antislash.
                let desc = format!(
                    "ndisrc ndi-name=\"{name}\" ! ndisrcdemux name=demux demux.video ! queue ! \
                     videoconvert ! video/x-raw,format=RGBA ! appsink name=sortie max-buffers=2 drop=true sync=false"
                );
                self.custom = Some(self.build_custom(&desc)?);
            }
        }

        // Préchargement en pause : le média est prêt, position 0, aucune
        // lecture. Les sources live rendent NO_PREROLL (succès aussi).
        self.source = Some(source);
        if let Err(err) = self.set_state(gst::State::Paused) {
            self.source = None;
            // Le pipeline dédié DOIT passer à Null avant d'être détruit,
            // sinon ses threads et le périphérique (webcam, capture) fuient —
            // fuite CUMULATIVE à chaque reprise (toutes les 3 s) d'une source
            // live indisponible.
            if let Some(custom) = self.custom.take() {
                let _ = custom.set_state(gst::State::Null);
            }
            return Err(err);
        }
        info!(source = %raw, "source chargée (GStreamer)");
        Ok(())
    }

    fn play(&mut self) -> Result<(), PlayerError> {
        if self.source.is_none() {
            return Err(PlayerError::Backend("play sans média".into()));
        }
        self.set_state(gst::State::Playing)
    }

    fn pause(&mut self) -> Result<(), PlayerError> {
        if self.source.is_some() {
            self.set_state(gst::State::Paused)?;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlayerError> {
        if self.source.is_some() {
            // Ready libère les décodeurs (précieux sur Pi) ; le prochain
            // play repart du début, comme le backend mémoire.
            self.set_state(gst::State::Ready)?;
        }
        self.frames.send_replace(None); // sortie noire à l'arrêt
        Ok(())
    }

    fn seek(&mut self, seconds: f64) -> Result<(), PlayerError> {
        match &self.source {
            None => Err(PlayerError::Backend("seek sans média".into())),
            Some(source) if source.is_live() => {
                Err(PlayerError::Backend("source live : pas de position".into()))
            }
            Some(_) => {
                let target = gst::ClockTime::from_nseconds((seconds.max(0.0) * 1e9) as u64);
                self.active()
                    .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target)
                    .map_err(|e| PlayerError::Backend(format!("seek : {e}")))
            }
        }
    }

    fn set_volume(&mut self, volume: f32) -> Result<(), PlayerError> {
        // Seul playbin porte l'audio ; les pipelines dédiés sont muets.
        self.playbin
            .set_property("volume", f64::from(volume.clamp(0.0, 1.0)));
        Ok(())
    }

    fn set_rate(&mut self, rate: f64) -> Result<(), PlayerError> {
        match &self.source {
            None => Err(PlayerError::Backend("vitesse sans média".into())),
            // Une source live n'a pas de vitesse ; la synchro n'a de toute
            // façon pas de sens dessus — appel ignoré sans erreur.
            Some(source) if source.is_live() => Ok(()),
            Some(_) => {
                // INSTANT_RATE_CHANGE (GStreamer ≥ 1.18) : la vitesse change
                // SANS flush ni coupure d'image — exactement ce qu'il faut
                // pour les micro-corrections de synchro (±3 %).
                let seek = gst::event::Seek::new(
                    rate.clamp(0.25, 4.0),
                    gst::SeekFlags::INSTANT_RATE_CHANGE,
                    gst::SeekType::None,
                    gst::ClockTime::NONE,
                    gst::SeekType::None,
                    gst::ClockTime::NONE,
                );
                if self.active().send_event(seek) {
                    Ok(())
                } else {
                    Err(PlayerError::Backend(
                        "changement de vitesse refusé par le pipeline".into(),
                    ))
                }
            }
        }
    }

    fn position_seconds(&self) -> Option<f64> {
        self.query_seconds(true)
    }

    fn duration_seconds(&self) -> Option<f64> {
        self.query_seconds(false)
    }

    fn take_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        if let Some(bus) = self.active().bus() {
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

    fn set_gapless_loop(&mut self, enabled: bool) {
        self.gapless.store(enabled, Ordering::Relaxed);
    }
}

impl Drop for GstBackend {
    fn drop(&mut self) {
        self.teardown();
    }
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

/// Construit un appsink RGBA (2 tampons max, frames en retard jetées) et
/// branche ses callbacks sur le canal de frames.
fn make_appsink(frames: watch::Sender<Option<VideoFrame>>) -> gst_app::AppSink {
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        )
        .max_buffers(2)
        .drop(true)
        .build();
    attach_frame_callbacks(&appsink, frames);
    appsink
}

fn attach_frame_callbacks(appsink: &gst_app::AppSink, frames: watch::Sender<Option<VideoFrame>>) {
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                match frame_from_sample(&sample) {
                    Some(frame) => {
                        frames.send_replace(Some(frame));
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

/// L'extension désigne-t-elle une image fixe ?
fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test : le backend se construit (runtime GStreamer requis — ce
    /// test tourne dans le job CI dédié), refuse un fichier absent et une
    /// source NDI sans plugin, sans jamais paniquer.
    #[test]
    fn backend_builds_and_rejects_bad_sources() {
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
        // NDI sans plugin : erreur propre.
        let ndi = backend.load(Path::new("ndi://Régie"));
        if gst::ElementFactory::find("ndisrc").is_none() {
            assert!(matches!(ndi, Err(PlayerError::Media(_))));
        }
        // Capture : soit la machine a une caméra (Ok), soit erreur propre —
        // dans les deux cas, pas de panique.
        let _ = backend.load(Path::new("capture://0"));
        // Sans média valide : play/seek refusés, stop/volume tolérés.
        let _ = backend.stop();
        assert!(backend.set_volume(0.5).is_ok());
        assert!(backend.take_events().len() < 100);
    }

    #[test]
    fn image_extensions_are_detected() {
        assert!(is_image(Path::new("media/affiche.PNG")));
        assert!(is_image(Path::new("x.webp")));
        assert!(!is_image(Path::new("clip.mp4")));
        assert!(!is_image(Path::new("sans_extension")));
    }
}
