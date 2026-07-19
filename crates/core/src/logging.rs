//! P1.0 — Journal en mémoire pour la page de logs de la web UI.
//!
//! Un [`LogBuffer`] est un ring buffer borné d'entrées de log + un canal de
//! diffusion pour le suivi en direct (WebSocket). Il se branche sur `tracing`
//! via [`LogBuffer::layer`] : tout ce que le node trace (bus, engine, HTTP…)
//! devient consultable à distance — exigence "diagnostic terrain sans SSH".
//!
//! Robustesse :
//! - borné : impossible de saturer la RAM d'un Pi, les vieilles entrées partent ;
//! - jamais bloquant côté émetteur : un abonné lent perd des entrées
//!   (`Lagged`), il ne fige jamais le node ;
//! - un mutex empoisonné (panic d'un autre thread) n'arrête pas les logs.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Une entrée de log, sérialisable telle quelle vers la web UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Numéro de séquence strictement croissant (détection de trous côté UI).
    pub seq: u64,
    /// Horodatage en millisecondes Unix.
    pub ts_ms: u64,
    /// Niveau : "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR".
    pub level: String,
    /// Module émetteur (`target` tracing), ex. `toolbox_core::bus`.
    pub target: String,
    /// Message, suivi des champs structurés (` clé=valeur`).
    pub message: String,
}

struct Ring {
    entries: VecDeque<LogEntry>,
}

struct Inner {
    ring: Mutex<Ring>,
    capacity: usize,
    seq: AtomicU64,
    live: broadcast::Sender<LogEntry>,
}

/// Journal borné, clonable et partageable entre threads/tâches.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogBuffer")
            .field("capacity", &self.inner.capacity)
            .field("len", &self.len())
            .finish()
    }
}

impl LogBuffer {
    /// Crée un journal gardant au plus `capacity` entrées (minimum 1).
    /// Le canal de diffusion en direct a la même capacité.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (live, _) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(Inner {
                ring: Mutex::new(Ring {
                    entries: VecDeque::with_capacity(capacity),
                }),
                capacity,
                seq: AtomicU64::new(0),
                live,
            }),
        }
    }

    /// Couche `tracing` à brancher sur le subscriber du binaire.
    pub fn layer(&self) -> LogLayer {
        LogLayer {
            buffer: self.clone(),
        }
    }

    /// Copie des entrées actuelles, de la plus ancienne à la plus récente.
    pub fn snapshot(&self) -> Vec<LogEntry> {
        let ring = self
            .inner
            .ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ring.entries.iter().cloned().collect()
    }

    /// S'abonne au flux en direct (chaque abonné reçoit tout ce qui suit).
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.inner.live.subscribe()
    }

    /// Nombre d'entrées actuellement retenues.
    pub fn len(&self) -> usize {
        let ring = self
            .inner
            .ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ring.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ajoute une entrée (utilisé par la couche tracing ; public pour que
    /// d'autres sources — ex. stderr d'un process enfant — puissent alimenter
    /// le même journal).
    pub fn push(&self, level: &str, target: &str, message: String) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let entry = {
            let mut ring = self
                .inner
                .ring
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // Le seq est tiré SOUS le verrou d'insertion : deux threads ne
            // peuvent plus insérer dans le désordre (l'UI lit les seq comme
            // strictement croissants pour détecter les trous).
            let entry = LogEntry {
                seq: self.inner.seq.fetch_add(1, Ordering::Relaxed),
                ts_ms,
                level: level.to_string(),
                target: target.to_string(),
                message,
            };
            if ring.entries.len() >= self.inner.capacity {
                ring.entries.pop_front();
            }
            ring.entries.push_back(entry.clone());
            entry
        };
        // Échoue seulement s'il n'y a aucun abonné en direct : pas une erreur.
        let _ = self.inner.live.send(entry);
    }
}

/// Couche `tracing-subscriber` qui copie chaque événement dans le [`LogBuffer`].
pub struct LogLayer {
    buffer: LogBuffer,
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = FieldCollector::default();
        event.record(&mut fields);
        let mut message = fields.message;
        message.push_str(&fields.extra);
        self.buffer.push(
            event.metadata().level().as_str(),
            event.metadata().target(),
            message,
        );
    }
}

/// Extrait `message` et met les autres champs en ` clé=valeur`.
#[derive(Default)]
struct FieldCollector {
    message: String,
    extra: String,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            // Écrire dans une String ne peut pas échouer.
            let _ = write!(self.extra, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.extra, " {}={:?}", field.name(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn ring_keeps_only_last_entries() {
        let buf = LogBuffer::new(3);
        for i in 0..5 {
            buf.push("INFO", "test", format!("m{i}"));
        }
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "m2");
        assert_eq!(snap[2].message, "m4");
        // Les seq restent globaux : les trous sont visibles.
        assert_eq!(snap[0].seq, 2);
        assert_eq!(snap[2].seq, 4);
    }

    #[test]
    fn sequence_is_strictly_increasing() {
        let buf = LogBuffer::new(8);
        for _ in 0..8 {
            buf.push("INFO", "t", "x".into());
        }
        let snap = buf.snapshot();
        for pair in snap.windows(2) {
            assert!(pair[1].seq == pair[0].seq + 1);
        }
    }

    #[test]
    fn live_subscribers_receive_entries() {
        let buf = LogBuffer::new(4);
        let mut rx = buf.subscribe();
        buf.push("WARN", "t", "attention".into());
        let got = rx.try_recv().expect("entrée en direct");
        assert_eq!(got.level, "WARN");
        assert_eq!(got.message, "attention");
    }

    #[test]
    fn tracing_events_are_captured_with_fields() {
        let buf = LogBuffer::new(16);
        let subscriber = tracing_subscriber::registry().with(buf.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(port = 8080, "serveur démarré");
            tracing::warn!("attention");
        });
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].level, "INFO");
        assert!(snap[0].target.contains("logging"));
        assert!(
            snap[0].message.contains("serveur démarré") && snap[0].message.contains("port=8080"),
            "message: {:?}",
            snap[0].message
        );
        assert_eq!(snap[1].level, "WARN");
    }

    #[test]
    fn entry_json_is_stable() {
        let entry = LogEntry {
            seq: 7,
            ts_ms: 1000,
            level: "INFO".into(),
            target: "core::bus".into(),
            message: "ok".into(),
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert_eq!(
            json,
            r#"{"seq":7,"ts_ms":1000,"level":"INFO","target":"core::bus","message":"ok"}"#
        );
    }

    #[test]
    fn zero_capacity_is_clamped() {
        let buf = LogBuffer::new(0);
        buf.push("INFO", "t", "a".into());
        buf.push("INFO", "t", "b".into());
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.snapshot()[0].message, "b");
    }
}
