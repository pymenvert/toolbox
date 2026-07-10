//! Supervision des services du node : une tâche qui meurt avant l'arrêt
//! demandé n'est JAMAIS silencieuse.
//!
//! Sans elle, un service qui panique ou se termine trop tôt (bug, port qui
//! saute…) disparaît sans trace jusqu'au prochain redémarrage — le pire des
//! comportements sur une installation permanente. Le superviseur trace la
//! fin inattendue en ERROR (visible dans la page de logs et le diagnostic) ;
//! le node continue avec ses autres services.

use std::future::Future;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::error;

/// Lance `service` sous supervision : si la tâche se termine ou panique
/// alors que l'arrêt n'a pas été demandé, une erreur est tracée. La poignée
/// retournée se termine avec le service (l'arrêt propre s'y joint).
pub fn spawn_service<F>(
    name: &'static str,
    shutdown: watch::Receiver<bool>,
    service: F,
) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        // Tâche interne dédiée : une panique y est capturée par tokio et
        // remonte ici en `JoinError` au lieu de tuer le superviseur.
        let inner = tokio::spawn(service);
        match inner.await {
            Ok(()) => {
                if !*shutdown.borrow() {
                    error!(
                        service = name,
                        "service terminé avant l'arrêt demandé — le node continue sans lui"
                    );
                }
            }
            Err(err) if err.is_panic() => {
                error!(
                    service = name,
                    %err,
                    "service en panique — le node continue sans lui"
                );
            }
            // Annulée (arrêt du runtime) : rien à signaler.
            Err(_) => {}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une panique dans un service est capturée : le superviseur se termine
    /// proprement au lieu de propager la panique.
    #[tokio::test]
    async fn a_panicking_service_does_not_kill_the_supervisor() {
        let (_tx, rx) = watch::channel(false);
        let handle = spawn_service("test-panique", rx, async { panic!("boum") });
        handle
            .await
            .expect("le superviseur doit survivre à la panique du service");
    }

    /// Fin normale après l'arrêt demandé : le superviseur se joint sans bruit.
    #[tokio::test]
    async fn normal_end_after_shutdown_is_quiet() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).expect("signal");
        let handle = spawn_service("test-fin", rx, async {});
        handle.await.expect("join");
    }
}
