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
    /// Mémoire totale / disponible en Mo (Linux uniquement).
    pub mem_total_mb: Option<u64>,
    pub mem_available_mb: Option<u64>,
    /// Température CPU en °C (Linux : thermal_zone0 — fiable sur Pi).
    pub temperature_c: Option<f32>,
}

/// Collecte les statistiques. Ne panique jamais : tout ce qui n'est pas
/// lisible devient `None`.
pub fn collect(started_at: Instant) -> SystemStats {
    SystemStats {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        uptime_s: started_at.elapsed().as_secs(),
        load_1min: read_load(),
        mem_total_mb: read_meminfo_kb("MemTotal:").map(|kb| kb / 1024),
        mem_available_mb: read_meminfo_kb("MemAvailable:").map(|kb| kb / 1024),
        temperature_c: read_temperature(),
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
