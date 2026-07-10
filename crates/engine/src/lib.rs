//! # toolbox-engine
//!
//! Moteur de rendu et de lecture :
//! - [`homography`] : mathématiques du mapping 4 coins, validées contre
//!   l'implémentation de référence `tools/mapping/homography_ref.py` ;
//! - [`render`] : état du node → matrices + uniformes prêts pour le shader ;
//! - [`player`] : machine à états de lecture derrière le trait
//!   [`PlayerBackend`] — le backend GStreamer (Pi/desktop) s'y branchera,
//!   [`MemoryBackend`] simule en attendant.
//!
//! Les shaders GLSL vivent dans `shaders/` à la racine du crate et sont
//! embarqués dans le binaire via `include_str!` (voir [`shaders`]).

pub mod homography;
pub mod player;
pub mod raster;
pub mod render;
pub mod video;

pub use homography::{HomographyError, Mat3};
pub use player::{
    BackendEvent, MemoryBackend, PlaybackPosition, Player, PlayerBackend, PlayerError,
};
pub use raster::{appliquer_blackout, niveau_rampe, render_frame};
pub use render::{ColorUniforms, RenderParams};
pub use video::VideoFrame;

/// Sources GLSL embarquées (GLES 3.0). Un seul endroit à modifier.
pub mod shaders {
    /// Vertex shader du warp : applique l'homographie au quad plein écran.
    pub const WARP_VERT: &str = include_str!("../shaders/warp.vert");
    /// Fragment shader : échantillonnage de la texture + correction couleur.
    pub const WARP_FRAG: &str = include_str!("../shaders/warp.frag");
}
