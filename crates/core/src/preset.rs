//! Presets : l'état complet du node sauvegardé/rechargé en JSON sur disque.
//!
//! Un preset = un fichier `<name>.json` dans le dossier `presets/`.
//! Écriture atomique (fichier temporaire + rename) : un crash en pleine
//! sauvegarde ne corrompt jamais un preset existant — exigence "solide".

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::CoreError;
use crate::state::NodeState;

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

/// Dépôt de presets sur disque.
#[derive(Debug, Clone)]
pub struct PresetStore {
    dir: PathBuf,
}

impl PresetStore {
    /// Ouvre (et crée si besoin) le dossier de presets.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        Ok(Self { dir })
    }

    fn path_of(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    /// Sauvegarde atomique de l'état sous `name` : écriture dans un fichier
    /// temporaire, `sync_all` (flush disque — un Pi peut perdre le courant à
    /// tout instant), puis rename atomique par-dessus l'ancien preset.
    pub fn save(&self, name: &str, state: &NodeState) -> Result<(), CoreError> {
        validate_name(name)?;
        let json = serde_json::to_vec_pretty(state)?;
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

    /// Charge le preset `name`. L'état est **validé** après lecture : un
    /// fichier corrompu ou édité à la main avec des valeurs hors bornes est
    /// refusé plutôt que de devenir l'état du node.
    pub fn load(&self, name: &str) -> Result<NodeState, CoreError> {
        validate_name(name)?;
        let path = self.path_of(name);
        if !path.exists() {
            return Err(CoreError::PresetNotFound(name.to_string()));
        }
        let bytes = fs::read(&path).map_err(|e| CoreError::io(path.display().to_string(), e))?;
        let state: NodeState = serde_json::from_slice(&bytes)?;
        state.validate()?;
        Ok(state)
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
