//! Compositeur partagé des sorties GStreamer (RTSP, KMS) : rend la sortie
//! composée du node (mapping, couleur, LUT, blackout — la même image que
//! la fenêtre, par la référence CPU) dans des tampons RÉUTILISÉS, avec
//! cache d'état (recopié seulement quand le bus a changé) et cache de LUT
//! par nom de fichier.

use tokio::sync::watch;
use tracing::warn;

use toolbox_core::NodeState;
use toolbox_engine::VideoFrame;

pub struct Compositeur {
    state: watch::Receiver<NodeState>,
    video: watch::Receiver<Option<VideoFrame>>,
    snapshot: NodeState,
    lut_cache: Option<(String, Option<toolbox_engine::Lut3d>)>,
    largeur: u32,
    hauteur: u32,
    depart: std::time::Instant,
    pixels: Vec<u32>,
    rgb: Vec<u8>,
}

impl Compositeur {
    pub fn new(
        state: watch::Receiver<NodeState>,
        video: watch::Receiver<Option<VideoFrame>>,
        largeur: u32,
        hauteur: u32,
    ) -> Self {
        let snapshot = state.borrow().clone();
        let pixels = vec![0u32; (largeur * hauteur) as usize];
        let rgb = vec![0u8; pixels.len() * 3];
        Self {
            state,
            video,
            snapshot,
            lut_cache: None,
            largeur,
            hauteur,
            depart: std::time::Instant::now(),
            pixels,
            rgb,
        }
    }

    /// Rend une frame composée et retourne le tampon RGB (3 octets/pixel,
    /// lignes de haut en bas). Le tampon appartient au compositeur : à
    /// copier avant la frame suivante.
    pub fn frame(&mut self) -> &[u8] {
        if self.state.has_changed().unwrap_or(false) {
            self.snapshot = self.state.borrow_and_update().clone();
        }
        match (&self.snapshot.lut, &mut self.lut_cache) {
            (None, cache) => *cache = None,
            (Some(nom), Some((connu, _))) if connu == nom => {}
            (Some(nom), cache) => {
                let charge = std::fs::read_to_string(std::path::Path::new("luts").join(nom))
                    .map_err(|e| e.to_string())
                    .and_then(|t| toolbox_engine::Lut3d::depuis_texte(&t));
                if let Err(err) = &charge {
                    warn!(nom, %err, "LUT illisible pour la sortie GStreamer — ignorée");
                }
                *cache = Some((nom.clone(), charge.ok()));
            }
        }
        let lut = self.lut_cache.as_ref().and_then(|(_, l)| l.as_ref());
        let video = self.video.borrow().clone();
        toolbox_engine::raster::render_frame_lut(
            &self.snapshot,
            video.as_ref(),
            lut,
            self.depart.elapsed().as_secs_f32(),
            self.largeur,
            self.hauteur,
            &mut self.pixels,
        );
        if self.snapshot.blackout.actif {
            toolbox_engine::appliquer_blackout(&mut self.pixels, 1.0);
        }
        for (px, dst) in self.pixels.iter().zip(self.rgb.chunks_exact_mut(3)) {
            #[allow(clippy::cast_possible_truncation)]
            {
                dst[0] = (px >> 16) as u8;
                dst[1] = (px >> 8) as u8;
                dst[2] = *px as u8;
            }
        }
        &self.rgb
    }
}
