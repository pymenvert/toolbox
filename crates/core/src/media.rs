//! Médiathèque : le dossier `media/` du node (P1.7).
//!
//! Liste, dépôt (upload), renommage et suppression de fichiers médias, avec
//! les mêmes exigences que les presets : noms validés (aucune traversée de
//! chemin possible depuis l'API réseau), écriture atomique (fichier partiel
//! jamais visible), erreurs explicites.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::state::validate_media_path;

/// Extensions acceptées à l'upload. Volontairement large (vidéo, image,
/// audio) mais fermée : pas d'exécutables ni d'inconnues.
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "mpg", "mpeg", "ts", "jpg", "jpeg", "png", "gif",
    "bmp", "webp", "wav", "mp3", "flac", "ogg", "aac", "m4a",
];

/// Un fichier de la médiathèque.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MediaInfo {
    /// Chemin relatif au dossier `media/`, séparateur `/`.
    pub path: String,
    pub bytes: u64,
}

/// Valide un nom de fichier déposé (plat : pas de sous-dossier à l'upload).
/// Public : l'upload HTTP en streaming refait la même vérification.
pub fn validate_upload_name(name: &str) -> Result<(), CoreError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ' | '(' | ')'));
    if !ok {
        return Err(CoreError::InvalidMediaPath(name.to_string()));
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext {
        Some(ext) if ALLOWED_EXTENSIONS.contains(&ext.as_str()) => Ok(()),
        _ => Err(CoreError::UnsupportedMediaType(name.to_string())),
    }
}

/// La médiathèque sur disque.
#[derive(Debug, Clone)]
pub struct MediaLibrary {
    root: PathBuf,
    /// Taille maximale d'un fichier déposé, en octets.
    max_upload_bytes: u64,
}

impl MediaLibrary {
    /// Ouvre (et crée si besoin) le dossier `media/`.
    pub fn open(root: impl Into<PathBuf>, max_upload_bytes: u64) -> Result<Self, CoreError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| CoreError::io(root.display().to_string(), e))?;
        Ok(Self {
            root,
            max_upload_bytes,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Taille maximale acceptée pour un dépôt de fichier, en octets.
    pub fn max_upload_bytes(&self) -> u64 {
        self.max_upload_bytes
    }

    /// Résout un chemin relatif validé vers le chemin absolu sur disque.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, CoreError> {
        validate_media_path(rel)?;
        Ok(self.root.join(rel))
    }

    /// Liste récursive (3 niveaux max) des médias, triée par chemin.
    pub fn list(&self) -> Result<Vec<MediaInfo>, CoreError> {
        let mut out = Vec::new();
        self.walk(&self.root, 0, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn walk(&self, dir: &Path, depth: u8, out: &mut Vec<MediaInfo>) -> Result<(), CoreError> {
        if depth > 3 {
            return Ok(());
        }
        let entries = fs::read_dir(dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| CoreError::io(dir.display().to_string(), e))?;
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue; // nom non-UTF8 : ignoré plutôt que planté
            };
            if name.starts_with('.') {
                continue; // fichiers cachés + temporaires d'écriture atomique
            }
            if path.is_dir() {
                self.walk(&path, depth + 1, out)?;
            } else if let Ok(meta) = entry.metadata() {
                let rel = path
                    .strip_prefix(&self.root)
                    .map_err(|_| CoreError::InvalidMediaPath(path.display().to_string()))?;
                // Chemin relatif canonique avec `/` même sous Windows.
                let rel = rel
                    .components()
                    .filter_map(|c| c.as_os_str().to_str())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push(MediaInfo {
                    path: rel,
                    bytes: meta.len(),
                });
            }
        }
        Ok(())
    }

    /// Dépose un fichier (upload) : nom plat validé, taille bornée, écriture
    /// atomique (`.tmp` + rename), fsync — un upload interrompu ne laisse
    /// jamais un média corrompu visible.
    pub fn save(&self, name: &str, mut reader: impl Read) -> Result<MediaInfo, CoreError> {
        validate_upload_name(name)?;
        let final_path = self.root.join(name);
        let tmp_path = self.root.join(format!(".{name}.upload.tmp"));

        match write_capped(&tmp_path, &mut reader, self.max_upload_bytes, name) {
            Ok(written) => {
                fs::rename(&tmp_path, &final_path)
                    .map_err(|e| CoreError::io(final_path.display().to_string(), e))?;
                Ok(MediaInfo {
                    path: name.to_string(),
                    bytes: written,
                })
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp_path); // nettoyage best-effort
                Err(err)
            }
        }
    }

    /// Renomme un média (à plat : `old` peut être dans un sous-dossier,
    /// `new` est un nom plat validé placé dans le même dossier que `old`).
    pub fn rename(&self, old: &str, new: &str) -> Result<(), CoreError> {
        let old_path = self.resolve(old)?;
        validate_upload_name(new)?;
        if !old_path.is_file() {
            return Err(CoreError::MediaNotFound(old.to_string()));
        }
        let parent = old_path
            .parent()
            .ok_or_else(|| CoreError::InvalidMediaPath(old.to_string()))?;
        let new_path = parent.join(new);
        if new_path.exists() {
            return Err(CoreError::MediaAlreadyExists(new.to_string()));
        }
        fs::rename(&old_path, &new_path)
            .map_err(|e| CoreError::io(new_path.display().to_string(), e))
    }

    /// Supprime un média.
    pub fn delete(&self, rel: &str) -> Result<(), CoreError> {
        let path = self.resolve(rel)?;
        if !path.is_file() {
            return Err(CoreError::MediaNotFound(rel.to_string()));
        }
        fs::remove_file(&path).map_err(|e| CoreError::io(path.display().to_string(), e))
    }

    /// Le fichier existe-t-il ?
    pub fn exists(&self, rel: &str) -> bool {
        self.resolve(rel).map(|p| p.is_file()).unwrap_or(false)
    }
}

/// Copie `reader` vers `tmp_path` en refusant de dépasser `max_bytes`,
/// avec fsync final. Retourne le nombre d'octets écrits.
fn write_capped(
    tmp_path: &Path,
    reader: &mut impl Read,
    max_bytes: u64,
    name: &str,
) -> Result<u64, CoreError> {
    let mut file =
        fs::File::create(tmp_path).map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| CoreError::io(name.to_string(), e))?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > max_bytes {
            return Err(CoreError::MediaTooLarge {
                name: name.to_string(),
                max: max_bytes,
            });
        }
        file.write_all(&buf[..n])
            .map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
    }
    file.sync_all()
        .map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> (tempfile::TempDir, MediaLibrary) {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib = MediaLibrary::open(dir.path().join("media"), 1024).expect("open");
        (dir, lib)
    }

    #[test]
    fn save_list_rename_delete_roundtrip() {
        let (_tmp, lib) = lib();
        let info = lib.save("clip 01.mp4", &b"0123456789"[..]).expect("save");
        assert_eq!(info.bytes, 10);
        assert_eq!(
            lib.list().expect("list"),
            vec![MediaInfo {
                path: "clip 01.mp4".into(),
                bytes: 10
            }]
        );
        assert!(lib.exists("clip 01.mp4"));

        lib.rename("clip 01.mp4", "boucle.mp4").expect("rename");
        assert!(!lib.exists("clip 01.mp4"));
        assert!(lib.exists("boucle.mp4"));

        lib.delete("boucle.mp4").expect("delete");
        assert!(lib.list().expect("list").is_empty());
        assert!(matches!(
            lib.delete("boucle.mp4"),
            Err(CoreError::MediaNotFound(_))
        ));
    }

    #[test]
    fn upload_size_limit_is_enforced_and_tmp_cleaned() {
        let (_tmp, lib) = lib();
        let big = vec![0u8; 4096];
        assert!(matches!(
            lib.save("gros.mp4", &big[..]),
            Err(CoreError::MediaTooLarge { .. })
        ));
        // Ni le fichier final ni le temporaire ne doivent exister.
        assert!(lib.list().expect("list").is_empty());
        assert_eq!(std::fs::read_dir(lib.root()).expect("dir").count(), 0);
    }

    #[test]
    fn dangerous_upload_names_are_rejected() {
        let (_tmp, lib) = lib();
        for bad in [
            "../evil.mp4",
            "a/b.mp4",
            "a\\b.mp4",
            ".cache.mp4",
            "",
            "virus.exe",
            "sans_extension",
            "trop_bizarre\u{202e}.mp4",
        ] {
            assert!(
                lib.save(bad, &b"x"[..]).is_err(),
                "aurait dû refuser {bad:?}"
            );
        }
    }

    #[test]
    fn list_walks_subfolders_and_skips_hidden() {
        let (_tmp, lib) = lib();
        std::fs::create_dir_all(lib.root().join("clips")).expect("mkdir");
        std::fs::write(lib.root().join("clips/a.mp4"), b"aa").expect("write");
        std::fs::write(lib.root().join(".hidden.mp4"), b"hh").expect("write");
        let listed = lib.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "clips/a.mp4");
    }

    #[test]
    fn rename_collision_is_refused() {
        let (_tmp, lib) = lib();
        lib.save("a.mp4", &b"a"[..]).expect("save a");
        lib.save("b.mp4", &b"b"[..]).expect("save b");
        assert!(matches!(
            lib.rename("a.mp4", "b.mp4"),
            Err(CoreError::MediaAlreadyExists(_))
        ));
    }
}
