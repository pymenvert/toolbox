//! # toolbox-core
//!
//! Colonne vertébrale du node : **bus de commandes**, **état sérialisable**,
//! **presets**, **médiathèque**, **journal de logs** et **config**.
//!
//! Principe central (voir docs/ARCHITECTURE.md du projet) : la web UI, l'OSC,
//! le MIDI, le REST et le séquenceur émettent les *mêmes* [`Command`]. L'état
//! les applique et publie des [`Event`] que tous les abonnés (UI, OSC feedback,
//! logs) reçoivent. Une feature = une commande = disponible partout.

pub mod bus;
pub mod command;
pub mod config;
pub mod error;
pub mod fader;
pub mod features;
pub mod logging;
pub mod media;
pub mod output;
pub mod preset;
pub mod source;
pub mod state;

pub use bus::{Bus, BusHandle, Source};
pub use command::{ColorParam, Command, TestPattern};
pub use config::NodeConfig;
pub use error::CoreError;
pub use features::FeatureFlags;
pub use logging::{LogBuffer, LogEntry};
pub use media::{MediaInfo, MediaLibrary};
pub use output::{MonitorInfo, OutputSettings};
pub use preset::{MappingStore, PresetStore};
pub use source::MediaSource;
pub use state::{Event, LoopMode, MappingState, NodeState, Rotation, Transport};
