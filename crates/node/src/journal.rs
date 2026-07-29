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

/// Budget disque TOTAL du journal, tous fichiers confondus. Le compte de
/// jours ne suffit pas : un service bavard (une boucle de reconnexion qui
/// trace) peut produire des centaines de Mo en 14 jours et remplir la carte
/// SD d'un node en installation permanente.
pub const OCTETS_GARDES: u64 = 200 * 1024 * 1024;

/// Intervalle de la purge périodique. Un node kiosque tourne des mois sans
/// redémarrer : purger seulement au démarrage ne purgeait jamais.
pub const INTERVALLE_PURGE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Préfixe des fichiers (`tracing-appender` ajoute `.AAAA-MM-JJ`).
const PREFIXE: &str = "toolbox.log";

/// Prépare le dossier, purge l'historique et rend l'écrivain non bloquant.
/// Le [`WorkerGuard`] doit vivre aussi longtemps que le process (à sa chute,
/// les dernières lignes sont vidées sur disque).
pub fn disk_writer(dir: &Path) -> io::Result<(NonBlocking, WorkerGuard)> {
    fs::create_dir_all(dir)?;
    purger(dir);
    let appender = tracing_appender::rolling::daily(dir, PREFIXE);
    Ok(tracing_appender::non_blocking(appender))
}

/// Purge aux valeurs par défaut : à appeler au démarrage ET périodiquement.
pub fn purger(dir: &Path) {
    prune(dir, JOURS_GARDES, OCTETS_GARDES);
}

/// Supprime les journaux les plus anciens : d'abord au-delà de `keep`
/// fichiers, puis tant que le total dépasse `budget` octets. Les dates ISO
/// se trient par le nom ; toute erreur est tracée sur stderr (le tracing
/// peut ne pas être installé) et n'empêche jamais le démarrage.
fn prune(dir: &Path, keep: usize, budget: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // (nom, taille) des seuls fichiers de journal.
    let mut journaux: Vec<(String, u64)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !name.starts_with(&format!("{PREFIXE}.")) {
                return None;
            }
            let taille = entry.metadata().map(|m| m.len()).unwrap_or(0);
            Some((name, taille))
        })
        .collect();
    // Tri croissant : les plus vieux d'abord.
    journaux.sort();

    let supprimer = |dir: &Path, name: &str| {
        if let Err(err) = fs::remove_file(dir.join(name)) {
            eprintln!("toolbox-node : purge du journal {name} impossible : {err}");
        }
    };

    // 1) Excédent en NOMBRE de jours.
    let excedent = journaux.len().saturating_sub(keep);
    for (name, _) in journaux.drain(..excedent) {
        supprimer(dir, &name);
    }

    // 2) Excédent en TAILLE : on retire les plus vieux jusqu'à repasser sous
    //    le budget, en gardant toujours le fichier du jour (le dernier).
    let mut total: u64 = journaux.iter().map(|(_, t)| *t).sum();
    while total > budget && journaux.len() > 1 {
        let (name, taille) = journaux.remove(0);
        total = total.saturating_sub(taille);
        eprintln!("toolbox-node : journal {name} purgé (budget de {budget} octets dépassé)");
        supprimer(dir, &name);
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

        prune(dir.path(), 2, OCTETS_GARDES);

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
        prune(dir.path(), 14, OCTETS_GARDES);
        assert!(dir.path().join(format!("{PREFIXE}.2026-07-01")).exists());
    }

    /// Le compte de jours ne suffit pas : un service bavard peut remplir le
    /// disque en restant sous les 14 fichiers. Le budget de TAILLE purge les
    /// plus vieux, mais garde toujours le journal du jour.
    #[test]
    fn prune_respecte_le_budget_de_taille() {
        let dir = tempfile::tempdir().expect("tempdir");
        for day in ["2026-07-01", "2026-07-02", "2026-07-03"] {
            std::fs::write(
                dir.path().join(format!("{PREFIXE}.{day}")),
                vec![b'x'; 1000],
            )
            .expect("write");
        }
        // Budget de 1500 octets pour 3000 : les deux plus vieux sautent.
        prune(dir.path(), 14, 1500);

        let restants: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .collect();
        assert_eq!(restants, vec![format!("{PREFIXE}.2026-07-03")]);

        // Un seul fichier trop gros : on ne le supprime PAS (sinon le node
        // perdrait le journal en cours d'écriture, celui qui sert justement
        // à comprendre pourquoi ça déborde).
        prune(dir.path(), 14, 10);
        assert!(dir.path().join(format!("{PREFIXE}.2026-07-03")).exists());
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
