//! Presets : documents JSON sauvegardés/rechargés sur disque.
//!
//! Un preset = un fichier `<name>.json` dans son dossier. Deux dépôts :
//! - [`PresetStore`] : l'état complet du node (`presets/`) ;
//! - [`MappingStore`] : le mapping seul (`presets/mapping/`), pour
//!   enregistrer/charger un calage sans toucher au média ni à la couleur.
//!
//! Écriture atomique (fichier temporaire + rename) : un crash en pleine
//! sauvegarde ne corrompt jamais un preset existant — exigence "solide".

use std::fs;
use std::io::Write;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use crate::error::CoreError;
use crate::state::{MappingState, NodeState};

/// Caractères autorisés dans un nom de preset (sécurité : pas de traversée
/// de chemin type `../../etc/passwd`, pas d'espaces exotiques).
fn validate_name(name: &str) -> Result<(), CoreError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(CoreError::InvalidPresetName(name.to_string()))
    }
}

/// Un document stockable : sérialisable et auto-validé au chargement.
pub trait Validated: serde::Serialize + serde::de::DeserializeOwned {
    fn validate(&self) -> Result<(), CoreError>;
}

impl Validated for NodeState {
    fn validate(&self) -> Result<(), CoreError> {
        NodeState::validate(self)
    }
}

impl Validated for MappingState {
    fn validate(&self) -> Result<(), CoreError> {
        MappingState::validate(self)
    }
}

/// Dépôt de presets d'état complet (`presets/`).
pub type PresetStore = Store<NodeState>;
/// Dépôt de presets de mapping seul (`presets/mapping/`).
pub type MappingStore = Store<MappingState>;

/// Dépôt de documents JSON sur disque, générique sur le type stocké.
#[derive(Debug)]
pub struct Store<T> {
    dir: PathBuf,
    _stored: PhantomData<T>,
}

// Clone manuel : `derive` exigerait `T: Clone` alors que le store ne
// contient aucun `T`.
impl<T> Clone for Store<T> {
    fn clone(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            _stored: PhantomData,
        }
    }
}

impl<T: Validated> Store<T> {
    /// Ouvre (et crée si besoin) le dossier de presets.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        Ok(Self {
            dir,
            _stored: PhantomData,
        })
    }

    fn path_of(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    /// Sauvegarde atomique du document sous `name` : écriture dans un fichier
    /// temporaire, `sync_all` (flush disque — un Pi peut perdre le courant à
    /// tout instant), puis rename atomique par-dessus l'ancien preset.
    pub fn save(&self, name: &str, value: &T) -> Result<(), CoreError> {
        validate_name(name)?;
        let json = serde_json::to_vec_pretty(value)?;
        let final_path = self.path_of(name);
        let tmp_path = self.dir.join(format!(".{name}.json.tmp"));
        {
            let mut file = fs::File::create(&tmp_path)
                .map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
            file.write_all(&json)
                .map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
            file.sync_all()
                .map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
        }
        fs::rename(&tmp_path, &final_path)
            .map_err(|e| CoreError::io(final_path.display().to_string(), e))?;
        Ok(())
    }

    /// Charge le preset `name`. Le document est **validé** après lecture : un
    /// fichier corrompu ou édité à la main avec des valeurs hors bornes est
    /// refusé plutôt que de devenir l'état du node.
    pub fn load(&self, name: &str) -> Result<T, CoreError> {
        validate_name(name)?;
        let path = self.path_of(name);
        if !path.exists() {
            return Err(CoreError::PresetNotFound(name.to_string()));
        }
        let bytes = fs::read(&path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
        let value: T = serde_json::from_slice(&bytes)?;
        value.validate()?;
        Ok(value)
    }

    /// Liste les presets disponibles (triés, sans extension).
    pub fn list(&self) -> Result<Vec<String>, CoreError> {
        let mut names = Vec::new();
        let entries = fs::read_dir(&self.dir)
            .map_err(|e| CoreError::io(self.dir.display().to_string(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| CoreError::io(self.dir.display().to_string(), e))?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    // Ignore les fichiers temporaires d'écriture atomique.
                    if !stem.starts_with('.') {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Supprime le preset `name`.
    pub fn delete(&self, name: &str) -> Result<(), CoreError> {
        validate_name(name)?;
        let path = self.path_of(name);
        if !path.exists() {
            return Err(CoreError::PresetNotFound(name.to_string()));
        }
        fs::remove_file(&path).map_err(|e| CoreError::io(path.display().to_string(), e))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;

    fn store() -> (tempfile::TempDir, PresetStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PresetStore::open(dir.path().join("presets")).expect("open");
        (dir, store)
    }

    #[test]
    fn save_load_roundtrip() {
        let (_tmp, store) = store();
        let mut state = NodeState::default();
        state
            .apply(&Command::CornerSet {
                index: 1,
                x: 0.87,
                y: 0.02,
            })
            .expect("corner");

        store.save("scene_01", &state).expect("save");
        let loaded = store.load("scene_01").expect("load");
        assert_eq!(loaded, state);
    }

    #[test]
    fn list_and_delete() {
        let (_tmp, store) = store();
        let state = NodeState::default();
        store.save("b", &state).expect("save b");
        store.save("a", &state).expect("save a");
        assert_eq!(store.list().expect("list"), vec!["a", "b"]);

        store.delete("a").expect("delete");
        assert_eq!(store.list().expect("list"), vec!["b"]);
        assert!(matches!(store.load("a"), Err(CoreError::PresetNotFound(_))));
    }

    #[test]
    fn path_traversal_is_rejected() {
        let (_tmp, store) = store();
        let state = NodeState::default();
        for bad in ["../evil", "a/b", "", "un nom avec espaces", "é"] {
            assert!(
                matches!(
                    store.save(bad, &state),
                    Err(CoreError::InvalidPresetName(_))
                ),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn tampered_preset_is_rejected_on_load() {
        let (_tmp, store) = store();
        store.save("ok", &NodeState::default()).expect("save");
        // Volume hors bornes écrit à la main dans le fichier.
        let path = store.dir().join("ok.json");
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, text.replace("\"volume\": 1.0", "\"volume\": 42.0")).expect("tamper");
        assert!(matches!(
            store.load("ok"),
            Err(CoreError::OutOfRange { .. })
        ));

        // JSON illisible → erreur propre, pas de panic.
        std::fs::write(&path, b"{pas du json").expect("corrupt");
        assert!(store.load("ok").is_err());
    }

    #[test]
    fn mapping_store_roundtrip_and_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store: MappingStore =
            Store::open(dir.path().join("presets").join("mapping")).expect("open");

        let mut mapping = MappingState::default();
        mapping.corners[2] = crate::state::Corner { x: 0.9, y: 0.85 };
        mapping.enabled = false;
        store.save("salon", &mapping).expect("save");
        assert_eq!(store.load("salon").expect("load"), mapping);
        assert_eq!(store.list().expect("list"), vec!["salon"]);

        // Un fichier trafiqué (coin hors bornes) est refusé au chargement.
        let path = store.dir().join("salon.json");
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, text.replace("0.9", "42.0")).expect("tamper");
        assert!(matches!(
            store.load("salon"),
            Err(CoreError::OutOfRange { .. })
        ));
    }

    #[test]
    fn overwrite_is_safe() {
        let (_tmp, store) = store();
        let mut state = NodeState::default();
        store.save("s", &state).expect("save 1");
        state
            .apply(&Command::SetVolume { volume: 0.3 })
            .expect("volume");
        store.save("s", &state).expect("save 2");
        assert_eq!(store.load("s").expect("load").player.volume, 0.3);
        // Pas de fichier temporaire qui traîne.
        assert_eq!(store.list().expect("list"), vec!["s"]);
    }
}
