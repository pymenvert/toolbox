//! # toolbox-render
//!
//! Fenêtre de sortie du node : affiche les mires de test déformées EN DIRECT
//! par le mapping (homographie, rotation, miroirs, recadrage) avec la
//! correction couleur — de quoi caler un vidéoprojecteur dès aujourd'hui.
//!
//! Le rendu est fait au CPU ([`raster`]) : aucun GPU ni bibliothèque système
//! requis, et la chaîne par pixel est la référence testée de la future passe
//! GLSL. Le backend vidéo GStreamer (après bench Pi) remplacera la mire par
//! la frame vidéo dans cette même fenêtre.
//!
//! Sans mire sélectionnée, la sortie est noire (un VP de spectacle n'affiche
//! rien par défaut) : choisir une mire dans l'onglet Mapping de l'UI web.

pub mod raster;
pub mod window;

pub use raster::render_frame;
pub use window::{spawn, OutputChannels, WindowConfig};
