//! # toolbox-core
//!
//! Colonne vertébrale du node : **bus de commandes**, **état sérialisable**,
//! **presets** et **config**.
//!
//! Principe central (voir docs/ARCHITECTURE.md du projet) : la web UI, l'OSC,
//! le MIDI, le REST et le séquenceur émettent les *mêmes* [`Command`]. L'état
//! les applique et publie des [`Event`] que tous les abonnés (UI, OSC feedback,
//! logs) reçoivent. Une feature = une commande = disponible partout.

pub mod bus;
pub mod command;
pub mod config;
pub mod error;
pub mod preset;
pub mod state;

pub use bus::{Bus, BusHandle};
pub use command::Command;
pub use config::NodeConfig;
pub use error::CoreError;
pub use state::{Event, NodeState};
