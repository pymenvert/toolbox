//! Monitoring système (P2.5 partiel) : CPU, mémoire, température, uptime.
//!
//! Zéro dépendance : lecture directe de `/proc` et `/sys` sous Linux
//! (Raspberry Pi compris). Sur les autres OS, les champs indisponibles sont
//! `null` — l'UI les masque. La suite (FPS, frames perdues) viendra avec le
//! backend GStreamer.

use std::time::Instant;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SystemStats {
    /// OS cible (compile-time) : "linux", "windows", "macos"…
    pub os: &'static str,
    /// Architecture : "x86_64", "aarch64"…
    pub arch: &'static str,
    /// Uptime du process node, en secondes.
    pub uptime_s: u64,
    /// Charge système 1 min (Linux uniquement).
    pub load_1min: Option<f32>,
    /// Mémoire totale / disponible de la MACHINE en Mo (Linux uniquement).
    pub mem_total_mb: Option<u64>,
    pub mem_available_mb: Option<u64>,
    /// Mémoire du process Lanterne lui-même, en Mo.
    ///
    /// Les deux champs au-dessus décrivent la machine : une fuite de Lanterne
    /// s'y noyait dans le bruit des autres programmes, et sous Windows on ne
    /// voyait strictement rien. Celui-ci est le seul chiffre qui permette de
    /// dire « le node a grossi de 40 Mo en six heures ».
    ///
    /// Ce qui est mesuré diffère selon l'OS, et c'est voulu : `VmRSS` sous
    /// Linux, mémoire **privée** (pas le working set) sous Windows. Dans les
    /// deux cas, l'objectif est la même question — le process retient-il de
    /// plus en plus ? — pas une comparaison chiffrée entre plateformes.
    pub rss_mb: Option<u64>,
    /// Température CPU en °C (Linux : thermal_zone0 — fiable sur Pi).
    pub temperature_c: Option<f32>,
    /// Espace disque libre / total du dossier de travail, en Go.
    pub disk_free_gb: Option<f32>,
    pub disk_total_gb: Option<f32>,
    /// État Tailscale si le binaire est installé : "connecté (100.x.y.z)"
    /// ou "déconnecté". `None` = Tailscale absent.
    pub tailscale: Option<String>,
}

/// Collecte les statistiques. Ne panique jamais : tout ce qui n'est pas
/// lisible devient `None`.
pub fn collect(started_at: Instant) -> SystemStats {
    // Un seul appel : sous Windows, disque et mémoire du process sortent de
    // la MÊME sous-commande PowerShell (deux invocations toutes les 5 s
    // coûteraient plus cher que tout le reste de la page Système réunie).
    let (disk, rss_mb) = disque_et_memoire();
    SystemStats {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        uptime_s: started_at.elapsed().as_secs(),
        load_1min: read_load(),
        mem_total_mb: read_meminfo_kb("MemTotal:").map(|kb| kb / 1024),
        mem_available_mb: read_meminfo_kb("MemAvailable:").map(|kb| kb / 1024),
        rss_mb,
        temperature_c: read_temperature(),
        disk_free_gb: disk.map(|(free, _)| free),
        disk_total_gb: disk.map(|(_, total)| total),
        tailscale: read_tailscale(),
    }
}

/// Disque du volume de travail et mémoire résidente du process, en un seul
/// passage. Sous Linux ce sont deux lectures de fichiers (gratuites) ; sous
/// Windows, une seule sous-commande PowerShell rapporte les deux.
#[cfg(target_os = "linux")]
fn disque_et_memoire() -> (Option<(f32, f32)>, Option<u64>) {
    (read_disk(), read_meminfo_self_kb().map(|kb| kb / 1024))
}

/// Mémoire résidente du process courant, en Ko (`/proc/self/status`).
#[cfg(target_os = "linux")]
fn read_meminfo_self_kb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = text.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Windows : espace disque + mémoire privée du process, une invocation.
///
/// **`PrivateMemorySize64`, PAS `WorkingSet64`.** Le working set est la
/// partie du process actuellement en RAM physique : Windows le vide à sa
/// guise (mise en veille, pression mémoire, réduction de fenêtre) SANS que
/// le programme ait libéré quoi que ce soit. Mesuré sur ce poste : 300 Mo
/// alloués et toujours référencés, working set tombé de 387 à 11 Mo après
/// une réduction demandée par l'OS, mémoire privée inchangée à 370 Mo. Un
/// test d'endurance basé sur le working set aurait donc conclu « aucune
/// fuite » sur une fuite franche.
#[cfg(target_os = "windows")]
fn disque_et_memoire() -> (Option<(f32, f32)>, Option<u64>) {
    let script = format!(
        // Un chemin réseau (UNC) n'a pas de lettre de lecteur : `Get-PSDrive`
        // échouait alors et sa ligne vide décalait tout le reste.
        "$d = if ((Get-Location).Drive) {{ \
             Get-PSDrive -Name (Get-Location).Drive.Name -ErrorAction SilentlyContinue \
         }} else {{ $null }}; \
         $p = Get-Process -Id {} -ErrorAction SilentlyContinue; \
         \"$($d.Free)`n$($d.Used)`n$($p.PrivateMemorySize64)\"",
        std::process::id()
    );
    let Ok(out) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
    else {
        return (None, None);
    };
    decoder_sortie(&String::from_utf8_lossy(&out.stdout))
}

/// Décode les trois lignes du script ci-dessus. Séparé pour être testable
/// sans lancer PowerShell — et parce que le découpage se voulait
/// « positionnel » alors qu'il ne l'était pas : `split_whitespace` fusionne
/// les séparateurs, donc une valeur vide décalait toutes les suivantes et la
/// mémoire prenait la valeur du disque. Une ligne par valeur, et un champ
/// absent reste absent.
#[cfg(target_os = "windows")]
fn decoder_sortie(text: &str) -> (Option<(f32, f32)>, Option<u64>) {
    let mut lignes = text.lines();
    let libre = lignes.next().unwrap_or("").trim().parse::<f64>().ok();
    let occupe = lignes.next().unwrap_or("").trim().parse::<f64>().ok();
    let prive = lignes.next().unwrap_or("").trim().parse::<u64>().ok();
    let disque = match (libre, occupe) {
        (Some(free), Some(used)) => {
            let gib = 1024.0 * 1024.0 * 1024.0;
            #[allow(clippy::cast_possible_truncation)] // Go : la précision f32 suffit
            Some(((free / gib) as f32, ((free + used) / gib) as f32))
        }
        _ => None,
    };
    (disque, prive.map(|octets| octets / (1024 * 1024)))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn disque_et_memoire() -> (Option<(f32, f32)>, Option<u64>) {
    (read_disk(), None)
}

/// Espace libre du volume de travail, en Go. `None` = mesure indisponible
/// (on ne bloque alors rien : mieux vaut laisser passer que refuser à tort).
///
/// Appelle un sous-processus (`df` / `wmic`) : à réserver aux gestes ponctuels
/// (début d'un upload, d'une mise à jour), jamais à un chemin chaud.
pub fn espace_libre_go() -> Option<f32> {
    read_disk().map(|(libre, _)| libre)
}

/// Espace disque (libre, total) du volume courant, en Go.
/// `df -k .` : POSIX, présent partout (Pi compris), sans unsafe.
#[cfg(target_os = "linux")]
fn read_disk() -> Option<(f32, f32)> {
    let out = std::process::Command::new("df")
        .args(["-k", "."])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let mut cols = line.split_whitespace();
    let total_kb: f64 = cols.nth(1)?.parse().ok()?;
    let free_kb: f64 = cols.nth(1)?.parse().ok()?; // colonne "Available"
    Some((
        (free_kb / 1_048_576.0) as f32,
        (total_kb / 1_048_576.0) as f32,
    ))
}

/// Espace disque via PowerShell (toujours présent ; une requête toutes les
/// 5 s — la cadence de la page Système — reste négligeable).
#[cfg(target_os = "windows")]
fn read_disk() -> Option<(f32, f32)> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "$d = Get-PSDrive -Name (Get-Location).Drive.Name; \"$($d.Free) $($d.Used)\"",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut cols = text.split_whitespace();
    let free: f64 = cols.next()?.parse().ok()?;
    let used: f64 = cols.next()?.parse().ok()?;
    let gib = 1024.0 * 1024.0 * 1024.0;
    Some(((free / gib) as f32, ((free + used) / gib) as f32))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn read_disk() -> Option<(f32, f32)> {
    None
}

/// État Tailscale (brique optionnelle du brief 3.9) via son CLI.
fn read_tailscale() -> Option<String> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()?; // binaire absent → None (l'UI n'affiche rien)
    if out.status.success() {
        let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if ip.is_empty() {
            Some("déconnecté".into())
        } else {
            Some(format!("connecté ({ip})"))
        }
    } else {
        Some("déconnecté".into())
    }
}

fn read_load() -> Option<f32> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

fn read_meminfo_kb(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with(key))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn read_temperature() -> Option<f32> {
    let text = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    let millideg: f32 = text.trim().parse().ok()?;
    Some(millideg / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_never_panics_and_reports_platform() {
        let stats = collect(Instant::now());
        assert!(!stats.os.is_empty());
        assert!(!stats.arch.is_empty());
        // Sous Linux (CI), les lectures /proc doivent fonctionner.
        if stats.os == "linux" {
            assert!(stats.load_1min.is_some());
            assert!(stats.mem_total_mb.is_some());
            // /proc/self/status est toujours lisible par le process lui-même :
            // pas de raison légitime que la mémoire du node soit absente.
            assert!(stats.rss_mb.is_some(), "VmRSS de /proc/self/status");
        }
    }

    #[test]
    fn la_memoire_du_process_est_plausible_quand_elle_est_mesuree() {
        // On ne peut pas exiger une valeur exacte (elle dépend de la machine),
        // mais un chiffre absurde signalerait une erreur d'unité — le piège
        // classique de ce genre de mesure (octets pris pour des Mo).
        let stats = collect(Instant::now());
        if let Some(rss) = stats.rss_mb {
            assert!(
                rss < 100_000,
                "un binaire de test à {rss} Mo : unité probablement fausse"
            );
        }
    }

    /// Le décodage Windows sur des sorties RÉELLES capturées sur le poste.
    /// Il n'était pas testé du tout, et son découpage prétendument
    /// positionnel se décalait dès qu'une valeur manquait.
    #[cfg(target_os = "windows")]
    #[test]
    fn le_decodage_windows_encaisse_les_sorties_degradees() {
        // Nominal : 61 Go libres, 1937 Go occupés, 78 Mo privés.
        let (disque, rss) = decoder_sortie("61397131264\n1937913286656\n82837504\n");
        assert!(disque.is_some(), "disque nominal");
        assert_eq!(rss, Some(79), "mémoire privée en Mo");

        // Chemin réseau : pas de lettre de lecteur, les deux premières
        // lignes sont vides. La mémoire doit RESTER lue — c'est le décalage
        // qui la rendait fausse auparavant.
        let (disque, rss) = decoder_sortie("\n\n82837504\n");
        assert!(disque.is_none(), "disque non mesurable");
        assert_eq!(rss, Some(79), "la mémoire ne doit pas être décalée");

        // Get-Process muet : le disque reste bon, la mémoire devient absente.
        let (disque, rss) = decoder_sortie("61397131264\n1937913286656\n\n");
        assert!(disque.is_some(), "disque toujours mesuré");
        assert_eq!(rss, None, "mémoire absente, pas inventée");

        // Sortie vide (PowerShell en échec) : rien, sans panique.
        assert_eq!(decoder_sortie(""), (None, None));
    }

    #[test]
    fn la_memoire_du_process_est_distincte_de_celle_de_la_machine() {
        // Le bug qu'on veut rendre impossible : recopier mem_available_mb
        // dans rss_mb. Le process de test pèse forcément moins que la RAM
        // totale de la machine.
        let stats = collect(Instant::now());
        if let (Some(rss), Some(total)) = (stats.rss_mb, stats.mem_total_mb) {
            assert!(rss < total, "RSS {rss} Mo >= RAM totale {total} Mo");
        }
    }

    #[test]
    fn stats_serialize_to_json() {
        let stats = collect(Instant::now());
        let json = serde_json::to_string(&stats).expect("serialize");
        assert!(json.contains("\"os\""));
        assert!(json.contains("\"uptime_s\""));
    }
}
