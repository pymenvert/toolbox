//! # toolbox-gst
//!
//! Le backend vidéo réel du player, sur GStreamer (`playbin3`) : décodage
//! multiplateforme (Windows / Ubuntu / Raspberry Pi — accélération matérielle
//! choisie par GStreamer selon la machine), audio vers la sortie système,
//! frames vidéo RGBA poussées vers la fenêtre de sortie qui les warpe.
//!
//! Toute la crate vit derrière la feature `gstreamer` : sans elle, elle est
//! vide et le workspace compile sans les bibliothèques système GStreamer
//! (même modèle que le MIDI/ALSA). Le runtime GStreamer doit être installé
//! sur la machine cible (voir deploy/README.md).

#[cfg(feature = "gstreamer")]
mod backend;
#[cfg(feature = "gstreamer")]
pub use backend::GstBackend;
