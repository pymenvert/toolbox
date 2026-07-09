//! # toolbox-engine
//!
//! Moteur de rendu. Pour l'instant : les mathématiques du mapping
//! (homographie 4 coins), validées contre l'implémentation de référence
//! `tools/mapping/homography_ref.py`. La partie pipeline vidéo (GStreamer)
//! et le rendu GL arrivent en phase 1, après validation du bench phase 0.
//!
//! Les shaders GLSL vivent dans `shaders/` à la racine du crate et sont
//! embarqués dans le binaire via `include_str!` (voir [`shaders`]).

pub mod homography;

/// Sources GLSL embarquées (GLES 3.0). Un seul endroit à modifier.
pub mod shaders {
    /// Vertex shader du warp : applique l'homographie au quad plein écran.
    pub const WARP_VERT: &str = include_str!("../shaders/warp.vert");
    /// Fragment shader : échantillonnage de la texture + correction couleur.
    pub const WARP_FRAG: &str = include_str!("../shaders/warp.frag");
}
