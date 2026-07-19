//! Compositeur partagé des sorties réseau (RTSP, KMS, NDI) : rend la
//! sortie composée du node (mapping, couleur, LUT, blackout, gel — la
//! même image que la fenêtre, par la référence CPU) dans des tampons
//! RÉUTILISÉS, avec cache d'état (recopié seulement quand le bus a
//! changé) et cache de LUT par nom de fichier.

use tokio::sync::watch;
use tracing::warn;

use crate::VideoFrame;
use toolbox_core::NodeState;

pub struct Compositeur {
    state: watch::Receiver<NodeState>,
    video: watch::Receiver<Option<VideoFrame>>,
    snapshot: NodeState,
    lut_cache: Option<(String, Option<crate::Lut3d>)>,
    largeur: u32,
    hauteur: u32,
    depart: std::time::Instant,
    // Rampe du blackout de régie — même mécanique que la fenêtre de
    // sortie : au changement de consigne, la rampe repart du niveau
    // courant (relâcher en plein fondu redescend de là).
    blackout_prec: bool,
    blackout_depart: f32,
    blackout_depuis: std::time::Instant,
    blackout_niveau: f32,
    /// Frame retenue pendant un gel d'image (`state.freeze`) : la sortie
    /// réseau gèle comme la fenêtre.
    frame_gelee: Option<VideoFrame>,
    pixels: Vec<u32>,
    rgb: Vec<u8>,
    rgba: Vec<u8>,
}

impl Compositeur {
    pub fn new(
        state: watch::Receiver<NodeState>,
        video: watch::Receiver<Option<VideoFrame>>,
        largeur: u32,
        hauteur: u32,
    ) -> Self {
        let snapshot = state.borrow().clone();
        // Multiplication en usize : une résolution aberrante ne doit pas
        // déborder en u32 avant le cast (les appelants clampent déjà,
        // ceinture et bretelles).
        let pixels = vec![0u32; largeur as usize * hauteur as usize];
        Self {
            state,
            video,
            snapshot,
            lut_cache: None,
            largeur,
            hauteur,
            depart: std::time::Instant::now(),
            blackout_prec: false,
            blackout_depart: 0.0,
            blackout_depuis: std::time::Instant::now(),
            blackout_niveau: 0.0,
            frame_gelee: None,
            rgb: vec![0u8; pixels.len() * 3],
            rgba: vec![255u8; pixels.len() * 4],
            pixels,
        }
    }

    /// Rendu composé dans `self.pixels` (0RGB) — commun aux deux formats.
    fn rendre(&mut self) {
        if self.state.has_changed().unwrap_or(false) {
            self.snapshot = self.state.borrow_and_update().clone();
        }
        match (&self.snapshot.lut, &mut self.lut_cache) {
            (None, cache) => *cache = None,
            (Some(nom), Some((connu, _))) if connu == nom => {}
            (Some(nom), cache) => {
                let charge = std::fs::read_to_string(std::path::Path::new("luts").join(nom))
                    .map_err(|e| e.to_string())
                    .and_then(|t| crate::Lut3d::depuis_texte(&t));
                if let Err(err) = &charge {
                    warn!(nom, %err, "LUT illisible pour la sortie réseau — ignorée");
                }
                *cache = Some((nom.clone(), charge.ok()));
            }
        }
        let lut = self.lut_cache.as_ref().and_then(|(_, l)| l.as_ref());
        // Gel d'image : on retient la frame courante tant que le gel dure
        // (parité avec la fenêtre de sortie).
        let video = if self.snapshot.freeze {
            if self.frame_gelee.is_none() {
                self.frame_gelee = self.video.borrow().clone();
            }
            self.frame_gelee.clone()
        } else {
            self.frame_gelee = None;
            self.video.borrow().clone()
        };
        // Temps des effets animés replié sur l'heure : après des jours de
        // marche continue, un f32 « secondes depuis le boot » n'a plus la
        // précision d'une frame (les animations saccadent) — le repli garde
        // la précision, les motifs sont périodiques de toute façon.
        #[allow(clippy::cast_possible_truncation)] // < 3600 par construction
        let time = (self.depart.elapsed().as_secs_f64() % 3600.0) as f32;
        crate::raster::render_frame_lut(
            &self.snapshot,
            video.as_ref(),
            lut,
            time,
            self.largeur,
            self.hauteur,
            &mut self.pixels,
        );
        // Rampe du blackout : mêmes règles que la fenêtre (niveau_rampe).
        let cible = if self.snapshot.blackout.actif {
            1.0
        } else {
            0.0
        };
        if self.snapshot.blackout.actif != self.blackout_prec {
            self.blackout_prec = self.snapshot.blackout.actif;
            self.blackout_depart = self.blackout_niveau;
            self.blackout_depuis = std::time::Instant::now();
        }
        #[allow(clippy::cast_possible_truncation)] // fondu borné à 10 s
        let ecoule = self.blackout_depuis.elapsed().as_millis() as u64;
        let niveau = crate::raster::niveau_rampe(
            cible,
            self.blackout_depart,
            ecoule,
            self.snapshot.blackout.fondu_ms,
        );
        self.blackout_niveau = niveau;
        if niveau > 0.0 {
            crate::raster::appliquer_blackout(&mut self.pixels, niveau);
        }
    }

    /// Rend une frame composée et retourne le tampon RGB (3 octets/pixel,
    /// lignes de haut en bas). Le tampon appartient au compositeur : à
    /// copier avant la frame suivante.
    pub fn frame(&mut self) -> &[u8] {
        self.rendre();
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

    /// Rend une frame composée en RGBA (4 octets/pixel, alpha 255) — le
    /// format des frames NDI. Même tampon interne réutilisé.
    pub fn frame_rgba(&mut self) -> &[u8] {
        self.rendre();
        for (px, dst) in self.pixels.iter().zip(self.rgba.chunks_exact_mut(4)) {
            #[allow(clippy::cast_possible_truncation)]
            {
                dst[0] = (px >> 16) as u8;
                dst[1] = (px >> 8) as u8;
                dst[2] = *px as u8;
                dst[3] = 255;
            }
        }
        &self.rgba
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compositeur(etat: NodeState) -> (watch::Sender<NodeState>, Compositeur) {
        let (etat_tx, etat_rx) = watch::channel(etat);
        let (_video_tx, video_rx) = watch::channel(None);
        let compositeur = Compositeur::new(etat_rx, video_rx, 64, 36);
        (etat_tx, compositeur)
    }

    /// Le blackout des sorties réseau suit une rampe (fondu), pas une
    /// coupure nette : à mi-fondu, l'image n'est ni pleine ni noire.
    #[test]
    fn le_blackout_rampe_dans_les_sorties_reseau() {
        // Le damier a de grandes cases claires (0,85) : le max mesure la
        // lumière sans dépendre de lignes fines.
        let mut etat = NodeState {
            test_pattern: Some(toolbox_core::TestPattern::Checker),
            ..NodeState::default()
        };
        let (etat_tx, mut compositeur) = compositeur(etat.clone());

        let max = |frame: &[u8]| frame.iter().copied().max().unwrap_or(0);
        let plein = max(compositeur.frame());
        assert!(plein > 200, "mire attendue lumineuse avant blackout");

        etat.blackout.actif = true;
        etat.blackout.fondu_ms = 1_000;
        etat_tx.send(etat.clone()).expect("send");
        // La rampe démarre à la frame qui VOIT le changement…
        let _ = compositeur.frame();
        // …et on échantillonne en plein fondu (~40 %).
        std::thread::sleep(std::time::Duration::from_millis(400));
        let pendant = max(compositeur.frame());
        assert!(
            pendant > 0 && pendant + 20 < plein,
            "à mi-fondu l'image doit être atténuée, pas coupée net (lu {pendant})"
        );

        etat.blackout.fondu_ms = 0; // coupure immédiate
        etat.blackout.actif = false;
        etat_tx.send(etat.clone()).expect("send");
        let _ = compositeur.frame();
        etat.blackout.actif = true;
        etat_tx.send(etat).expect("send");
        assert_eq!(max(compositeur.frame()), 0, "fondu 0 ms = noir immédiat");
    }

    /// Le gel d'image fige la sortie réseau comme la fenêtre.
    #[test]
    fn le_gel_fige_la_video_reseau() {
        let mut etat = NodeState::default();
        // La vidéo ne s'affiche que transport actif (même règle partout).
        etat.player.transport = toolbox_core::Transport::Playing;
        let (etat_tx, etat_rx) = watch::channel(etat.clone());
        let (video_tx, video_rx) = watch::channel(None);
        let mut compositeur = Compositeur::new(etat_rx, video_rx, 4, 4);

        let frame = |v: u8| VideoFrame::new(4, 4, vec![v; 4 * 4 * 4].into());
        video_tx.send(frame(10)).expect("send");
        let _ = compositeur.frame();

        // Gel : la frame suivante du flux ne doit PAS apparaître.
        let mut gele = etat.clone();
        gele.freeze = true;
        etat_tx.send(gele).expect("send");
        let _ = compositeur.frame();
        video_tx.send(frame(200)).expect("send");
        let avec_gel = compositeur.frame()[0];

        // Dégel : la frame fraîche revient.
        etat_tx.send(etat).expect("send");
        let sans_gel = compositeur.frame()[0];
        assert_ne!(
            avec_gel, sans_gel,
            "le gel doit retenir l'ancienne frame, le dégel montrer la nouvelle"
        );
    }
}
