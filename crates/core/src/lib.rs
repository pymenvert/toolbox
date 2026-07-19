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
pub mod reglages;
pub mod sequenceur;
pub mod source;
pub mod state;

pub use bus::{Bus, BusHandle, Source};
pub use command::{ColorParam, Command, TestPattern};
pub use config::{NodeConfig, Resolution, SortieMode, SyncRole, SyncSettings};
pub use error::CoreError;
pub use features::FeatureFlags;
pub use logging::{LogBuffer, LogEntry};
pub use media::{MediaInfo, MediaLibrary};
pub use output::{MonitorInfo, OutputSettings};
pub use preset::{MappingStore, PresetStore};
pub use reglages::Reglages;
pub use source::MediaSource;
pub use state::{
    valider_mesh, valider_nom_lut, BlendingState, Event, LoopMode, MappingState, Masque, MeshState,
    NodeState, Rotation, Transport,
};

/// Écriture atomique ET durable d'un fichier d'état : temporaire à côté,
/// `sync_all` (flush disque — un Pi peut perdre le courant à tout instant,
/// et un rename sans flush peut laisser un fichier VIDE au reboot sur
/// ext4/FAT), puis rename par-dessus l'ancien. À utiliser pour tous les
/// `*.json` de configuration.
pub fn ecrire_atomique(path: &std::path::Path, bytes: &[u8]) -> Result<(), CoreError> {
    use std::io::Write as _;
    let tmp = path.with_extension("json.tmp");
    {
        let mut fichier =
            std::fs::File::create(&tmp).map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
        fichier
            .write_all(bytes)
            .map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
        fichier
            .sync_all()
            .map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path.display().to_string(), e))
}
