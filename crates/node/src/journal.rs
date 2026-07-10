//! Journal sur disque : un fichier par jour dans le dossier `paths.logs`.
//!
//! Le ring buffer en mémoire (page de logs, diagnostic) disparaît à chaque
//! redémarrage — sur une installation permanente, c'est justement après un
//! crash ou une coupure de courant qu'on veut lire les logs. Ici :
//! `logs/toolbox.log.AAAA-MM-JJ`, écriture non bloquante (une carte SD
//! lente ne fige jamais le node), purge des fichiers au-delà de
//! [`JOURS_GARDES`] à chaque démarrage.

use std::fs;
use std::io;
use std::path::Path;

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};

/// Nombre de fichiers quotidiens conservés (≈ deux semaines d'historique).
pub const JOURS_GARDES: usize = 14;

/// Préfixe des fichiers (`tracing-appender` ajoute `.AAAA-MM-JJ`).
const PREFIXE: &str = "toolbox.log";

/// Prépare le dossier, purge l'historique et rend l'écrivain non bloquant.
/// Le [`WorkerGuard`] doit vivre aussi longtemps que le process (à sa chute,
/// les dernières lignes sont vidées sur disque).
pub fn disk_writer(dir: &Path) -> io::Result<(NonBlocking, WorkerGuard)> {
    fs::create_dir_all(dir)?;
    prune(dir, JOURS_GARDES);
    let appender = tracing_appender::rolling::daily(dir, PREFIXE);
    Ok(tracing_appender::non_blocking(appender))
}

/// Supprime les fichiers de journal les plus anciens au-delà de `keep`.
/// Les dates ISO se trient par le nom ; toute erreur est tracée sur stderr
/// (le tracing n'est pas encore installé) et n'empêche jamais le démarrage.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut journaux: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            name.starts_with(&format!("{PREFIXE}.")).then_some(name)
        })
        .collect();
    if journaux.len() <= keep {
        return;
    }
    // Tri croissant : les plus vieux d'abord.
    journaux.sort();
    let excedent = journaux.len() - keep;
    for name in journaux.into_iter().take(excedent) {
        if let Err(err) = fs::remove_file(dir.join(&name)) {
            eprintln!("toolbox-node : purge du journal {name} impossible : {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_the_most_recent_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        for day in ["2026-07-01", "2026-07-02", "2026-07-03", "2026-07-04"] {
            std::fs::write(dir.path().join(format!("{PREFIXE}.{day}")), b"x").expect("write");
        }
        // Un fichier étranger ne doit jamais être touché.
        std::fs::write(dir.path().join("autre.txt"), b"x").expect("write");

        prune(dir.path(), 2);

        let mut restants: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .collect();
        restants.sort();
        assert_eq!(
            restants,
            vec![
                "autre.txt".to_string(),
                format!("{PREFIXE}.2026-07-03"),
                format!("{PREFIXE}.2026-07-04"),
            ]
        );
    }

    #[test]
    fn prune_below_threshold_does_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(format!("{PREFIXE}.2026-07-01")), b"x").expect("write");
        prune(dir.path(), 14);
        assert!(dir.path().join(format!("{PREFIXE}.2026-07-01")).exists());
    }

    #[test]
    fn disk_writer_creates_the_directory_and_writes() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs");
        let (mut writer, guard) = disk_writer(&logs).expect("writer");
        writer.write_all(b"ligne de test\n").expect("write");
        writer.flush().expect("flush");
        drop(guard); // vide le canal sur disque
        let mut fichiers = std::fs::read_dir(&logs).expect("read_dir").flatten();
        let fichier = fichiers.next().expect("un fichier de journal").path();
        let contenu = std::fs::read_to_string(fichier).expect("read");
        assert!(contenu.contains("ligne de test"));
    }
}
