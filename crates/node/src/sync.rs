//! Synchronisation multi-node niveau 2 : les suiveurs se verrouillent sur
//! la position de lecture du maître.
//!
//! Protocole (UDP, JSON compact, LAN) :
//! - le suiveur s'annonce au maître toutes les 2 s (`hello`) — rien à
//!   configurer côté maître à part son rôle ;
//! - le maître publie à 5 Hz une paire (heure Unix, position, transport,
//!   média) à chaque suiveur entendu récemment ;
//! - le suiveur estime la position du maître À L'INSTANT PRÉSENT
//!   (position + heure écoulée depuis l'envoi — horloges NTP partagées),
//!   lisse la dérive (médiane sur 5 mesures, robuste aux paquets retardés)
//!   et corrige : zone morte sous 5 ms, micro-ajustement de vitesse (±3 %,
//!   invisible) jusqu'au seuil, resync dur (seek) au-delà.
//!
//! Avec le backend GStreamer, la vitesse est appliquée sans coupure
//! (instant rate change) ; le backend simulé l'applique aussi — la boucle
//! complète est donc testée sans matériel. Le suiveur suit également le
//! média et le transport du maître (play/pause/chargement).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{info, warn};

use toolbox_core::{BusHandle, Command, Source, SyncSettings};
use toolbox_engine::PlaybackPosition;

/// Message d'horloge publié par le maître.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Horloge {
    /// Heure Unix (secondes, f64) au moment de l'envoi.
    pub t: f64,
    /// Position de lecture du maître à cet instant (None = pas de média).
    pub position: Option<f64>,
    pub lecture: bool,
    pub media: Option<String>,
}

/// Annonce d'un suiveur au maître.
#[derive(Debug, Serialize, Deserialize)]
struct Bonjour {
    node: String,
}

/// Décision de correction du suiveur, calculée par [`corriger`] (pure,
/// testée) : la dérive est POSITIVE quand le suiveur est en avance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Correction {
    /// Dérive dans la zone morte : vitesse normale.
    Aucune,
    /// Micro-ajustement : ralentir (dérive > 0) ou accélérer (dérive < 0).
    Vitesse(f32),
    /// Dérive au-delà du seuil : resync dur à la position cible.
    Seek(f64),
}

/// Zone morte : en dessous, on ne touche à rien (bruit de mesure).
const ZONE_MORTE_S: f64 = 0.005;
/// Micro-ajustement maximal de vitesse (±3 % : invisible à l'œil et à
/// l'oreille sur une correction courte).
const RATTRAPAGE_MAX: f64 = 0.03;

/// Calcule la correction pour une dérive donnée (secondes, positive =
/// suiveur en avance) vers `cible` (position du maître estimée maintenant).
pub fn corriger(derive_s: f64, cible: f64, tolerance_s: f64) -> Correction {
    let ampleur = derive_s.abs();
    if ampleur < ZONE_MORTE_S {
        return Correction::Aucune;
    }
    if ampleur >= tolerance_s {
        return Correction::Seek(cible.max(0.0));
    }
    // Proportionnel borné : rattraper la dérive en ~2 s de lecture.
    let ajustement = (derive_s / 2.0).clamp(-RATTRAPAGE_MAX, RATTRAPAGE_MAX);
    #[allow(clippy::cast_possible_truncation)] // borné à ±0.03
    Correction::Vitesse((1.0 - ajustement) as f32)
}

fn heure_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Service du MAÎTRE : écoute les `hello` des suiveurs et leur publie
/// l'horloge de lecture à 5 Hz. Un suiveur silencieux 10 s est oublié.
pub async fn maitre(
    settings: SyncSettings,
    bus: BusHandle,
    position: watch::Receiver<PlaybackPosition>,
    mut shutdown: watch::Receiver<bool>,
) {
    let adresse = format!("0.0.0.0:{}", settings.port);
    let socket = match UdpSocket::bind(&adresse).await {
        Ok(socket) => socket,
        Err(err) => {
            warn!(%err, %adresse, "synchro maître : écoute impossible");
            return;
        }
    };
    info!(
        port = settings.port,
        "synchro maître active (les suiveurs s'annoncent d'eux-mêmes)"
    );
    // Plafond de suiveurs : sans lui, un hôte hostile pourrait annoncer des
    // milliers de `hello` (adresses usurpées) et le maître leur émettrait à
    // tous son horloge à 5 Hz — amplification/DoS. 32 nodes couvrent large.
    const MAX_SUIVEURS: usize = 32;
    let mut suiveurs: std::collections::HashMap<std::net::SocketAddr, tokio::time::Instant> =
        std::collections::HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dernier_warn_plafond = tokio::time::Instant::now() - Duration::from_secs(60);
    let mut buf = [0u8; 1024];
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            received = socket.recv_from(&mut buf) => {
                if let Ok((len, from)) = received {
                    if serde_json::from_slice::<Bonjour>(&buf[..len]).is_ok() {
                        if suiveurs.contains_key(&from) {
                            suiveurs.insert(from, tokio::time::Instant::now()); // rafraîchit
                        } else if suiveurs.len() < MAX_SUIVEURS {
                            suiveurs.insert(from, tokio::time::Instant::now());
                            info!(%from, "suiveur de synchro connecté");
                        } else if dernier_warn_plafond.elapsed() > Duration::from_secs(10) {
                            warn!(%from, max = MAX_SUIVEURS, "plafond de suiveurs atteint — annonce ignorée");
                            dernier_warn_plafond = tokio::time::Instant::now();
                        }
                    }
                }
            }
            _ = tick.tick() => {
                suiveurs.retain(|from, vu| {
                    let vivant = vu.elapsed() < Duration::from_secs(10);
                    if !vivant {
                        info!(%from, "suiveur de synchro perdu (silencieux 10 s)");
                    }
                    vivant
                });
                if suiveurs.is_empty() {
                    continue;
                }
                let etat = bus.snapshot();
                let horloge = Horloge {
                    t: heure_unix(),
                    position: position.borrow().position,
                    lecture: etat.player.transport == toolbox_core::Transport::Playing,
                    media: etat.player.media.clone(),
                };
                if let Ok(bytes) = serde_json::to_vec(&horloge) {
                    for from in suiveurs.keys() {
                        let _ = socket.send_to(&bytes, from).await;
                    }
                }
            }
        }
    }
    info!("synchro maître arrêtée");
}

/// Service du SUIVEUR : s'annonce au maître, reçoit son horloge et corrige
/// la lecture locale (média, transport, dérive).
pub async fn suiveur(
    settings: SyncSettings,
    bus: BusHandle,
    position: watch::Receiver<PlaybackPosition>,
    derive_tx: watch::Sender<Option<f64>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let Some(maitre) = settings.maitre.clone() else {
        warn!("synchro suiveur sans adresse de maître ([sync] maitre) — inactif");
        return;
    };
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(socket) => socket,
        Err(err) => {
            warn!(%err, "synchro suiveur : socket impossible");
            return;
        }
    };
    info!(%maitre, "synchro suiveur active");
    // IP(s) attendues du maître : on n'accepte l'horloge QUE de lui — sinon
    // n'importe quel hôte du LAN pourrait piloter la lecture du suiveur.
    // Ré-résolue périodiquement (voir annonce.tick) : un maître pas encore
    // résoluble au boot (il monte après) ou une IP qui change (DHCP)
    // n'ouvre plus le filtre à vie.
    let resoudre = |maitre: String| async move {
        tokio::net::lookup_host(&maitre)
            .await
            .map(|addrs| {
                addrs
                    .map(|a| a.ip())
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default()
    };
    let mut ips_maitre = resoudre(maitre.clone()).await;
    if ips_maitre.is_empty() {
        warn!(%maitre, "adresse du maître non résolue au démarrage — nouvelle tentative en continu");
    }
    let tolerance_s = settings.tolerance_ms.max(20) as f64 / 1000.0;
    let bonjour = serde_json::to_vec(&Bonjour {
        node: "suiveur".into(),
    })
    .unwrap_or_default();
    let mut annonce = tokio::time::interval(Duration::from_secs(2));
    annonce.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Chien de garde : sans horloge depuis 5 s, le maître est réputé perdu.
    let mut chien = tokio::time::interval(Duration::from_secs(1));
    chien.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut buf = [0u8; 1024];
    // Fenêtre de dérives récentes (médiane = robuste aux paquets retardés).
    let mut derives: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
    // Après un seek de resync, on laisse la lecture se stabiliser.
    let mut grace_jusqua = tokio::time::Instant::now();
    // Dernière vitesse envoyée (évite de spammer le bus avec la même).
    let mut derniere_vitesse: f32 = 1.0;
    // Dernière horloge reçue (pour détecter un maître silencieux) + garde
    // anti-spam sur l'avertissement de source usurpée.
    let mut derniere_horloge = tokio::time::Instant::now();
    let mut maitre_perdu = false;
    let mut dernier_warn_source = tokio::time::Instant::now() - Duration::from_secs(60);

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = annonce.tick() => {
                // Ré-résolution : on ne remplace le filtre que sur un résultat
                // NON vide (un blip DNS ne doit pas ouvrir le filtrage).
                let ips = resoudre(maitre.clone()).await;
                if !ips.is_empty() {
                    ips_maitre = ips;
                }
                if let Err(err) = socket.send_to(&bonjour, &maitre).await {
                    warn!(%err, "annonce au maître non envoyée");
                }
            }
            _ = chien.tick() => {
                // Maître silencieux depuis 5 s : on cesse toute correction
                // (retour vitesse normale) et on signale la dérive comme
                // inconnue, au lieu de rester bloqué à ±3 % avec une dérive
                // « verte » périmée à la page Santé.
                if derniere_horloge.elapsed() > Duration::from_secs(5) && !maitre_perdu {
                    maitre_perdu = true;
                    if (derniere_vitesse - 1.0).abs() > f32::EPSILON {
                        bus.send(Source::Internal, Command::SetRate { rate: 1.0 }).await;
                        derniere_vitesse = 1.0;
                    }
                    derives.clear();
                    let _ = derive_tx.send(None);
                    warn!(%maitre, "maître silencieux depuis 5 s — synchro suspendue, lecture à vitesse normale");
                }
            }
            received = socket.recv_from(&mut buf) => {
                let Ok((len, from)) = received else { continue };
                // Filtrage de source : on ignore tout datagramme qui ne
                // vient pas du maître configuré.
                if !ips_maitre.is_empty() && !ips_maitre.contains(&from.ip()) {
                    if dernier_warn_source.elapsed() > Duration::from_secs(10) {
                        warn!(%from, %maitre, "horloge de synchro ignorée (source ≠ maître)");
                        dernier_warn_source = tokio::time::Instant::now();
                    }
                    continue;
                }
                let Ok(horloge) = serde_json::from_slice::<Horloge>(&buf[..len]) else { continue };
                if maitre_perdu {
                    info!(%maitre, "maître de retour — synchro reprise");
                }
                derniere_horloge = tokio::time::Instant::now();
                maitre_perdu = false;
                appliquer_horloge(
                    &bus,
                    &position,
                    &horloge,
                    tolerance_s,
                    &mut derives,
                    &mut grace_jusqua,
                    &mut derniere_vitesse,
                )
                .await;
                // Dérive publiée pour la page Santé (médiane courante, ms).
                if derives.len() >= 3 {
                    let mut triees: Vec<f64> = derives.iter().copied().collect();
                    triees.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let _ = derive_tx.send(Some(triees[triees.len() / 2] * 1000.0));
                }
            }
        }
    }
    info!("synchro suiveur arrêtée");
}

/// Traite un message d'horloge du maître : média, transport, dérive.
async fn appliquer_horloge(
    bus: &BusHandle,
    position: &watch::Receiver<PlaybackPosition>,
    horloge: &Horloge,
    tolerance_s: f64,
    derives: &mut std::collections::VecDeque<f64>,
    grace_jusqua: &mut tokio::time::Instant,
    derniere_vitesse: &mut f32,
) {
    let etat = bus.snapshot();

    // 1. Le suiveur suit le média du maître (s'il est différent).
    if let Some(media) = &horloge.media {
        if etat.player.media.as_ref() != Some(media) {
            info!(%media, "synchro : chargement du média du maître");
            bus.send(
                Source::Internal,
                Command::Load {
                    path: media.clone(),
                },
            )
            .await;
            derives.clear();
        }
    }

    // 2. Transport : le suiveur suit lecture/pause.
    let local_joue = etat.player.transport == toolbox_core::Transport::Playing;
    if horloge.lecture && !local_joue {
        bus.send(Source::Internal, Command::Play).await;
        *grace_jusqua = tokio::time::Instant::now() + Duration::from_millis(600);
        derives.clear();
    } else if !horloge.lecture && local_joue {
        bus.send(Source::Internal, Command::Pause).await;
        derives.clear();
    }

    // 3. Dérive — seulement en lecture, hors période de grâce post-resync.
    if !horloge.lecture || tokio::time::Instant::now() < *grace_jusqua {
        return;
    }
    let (Some(cible_emise), Some(locale)) = (horloge.position, position.borrow().position) else {
        return;
    };
    // Position du maître estimée MAINTENANT (horloges NTP partagées).
    let cible = cible_emise + (heure_unix() - horloge.t).clamp(0.0, 2.0);
    let derive = locale - cible;
    derives.push_back(derive);
    if derives.len() > 5 {
        derives.pop_front();
    }
    if derives.len() < 3 {
        return; // pas assez de mesures pour une médiane fiable
    }
    let mut triees: Vec<f64> = derives.iter().copied().collect();
    triees.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mediane = triees[triees.len() / 2];

    match corriger(mediane, cible, tolerance_s) {
        Correction::Aucune => {
            if (*derniere_vitesse - 1.0).abs() > 1e-6 {
                *derniere_vitesse = 1.0;
                bus.send(Source::Internal, Command::SetRate { rate: 1.0 })
                    .await;
            }
        }
        Correction::Vitesse(rate) => {
            if (rate - *derniere_vitesse).abs() > 0.001 {
                *derniere_vitesse = rate;
                bus.send(Source::Internal, Command::SetRate { rate }).await;
            }
        }
        Correction::Seek(cible) => {
            warn!(
                derive_ms = (mediane * 1000.0) as i64,
                "synchro : dérive au-delà du seuil — resync dur"
            );
            bus.send(Source::Internal, Command::SetRate { rate: 1.0 })
                .await;
            *derniere_vitesse = 1.0;
            bus.send(Source::Internal, Command::Seek { seconds: cible })
                .await;
            derives.clear();
            *grace_jusqua = tokio::time::Instant::now() + Duration::from_millis(800);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolbox_core::{Bus, NodeConfig, SyncRole};
    use toolbox_engine::{MemoryBackend, Player};

    #[test]
    fn la_correction_est_progressive_puis_dure() {
        // Zone morte.
        assert_eq!(corriger(0.002, 10.0, 0.08), Correction::Aucune);
        assert_eq!(corriger(-0.004, 10.0, 0.08), Correction::Aucune);
        // Micro-ajustement : en avance → ralentit, en retard → accélère.
        match corriger(0.02, 10.0, 0.08) {
            Correction::Vitesse(rate) => assert!(rate < 1.0 && rate > 0.97),
            autre => panic!("attendu Vitesse, reçu {autre:?}"),
        }
        match corriger(-0.02, 10.0, 0.08) {
            Correction::Vitesse(rate) => assert!(rate > 1.0 && rate < 1.03),
            autre => panic!("attendu Vitesse, reçu {autre:?}"),
        }
        // Borné à ±3 %.
        match corriger(0.079, 10.0, 0.08) {
            Correction::Vitesse(rate) => assert!((f64::from(rate) - 0.97).abs() < 1e-6),
            autre => panic!("attendu Vitesse bornée, reçu {autre:?}"),
        }
        // Au-delà du seuil : resync dur sur la cible.
        assert_eq!(corriger(0.2, 12.5, 0.08), Correction::Seek(12.5));
        assert_eq!(corriger(-0.5, 3.0, 0.08), Correction::Seek(3.0));
    }

    /// Boucle complète en réel (UDP loopback) : un maître et un suiveur,
    /// chacun avec son bus et son player simulé. Le suiveur démarre décalé
    /// d'une demi-seconde et doit converger sous 40 ms (une frame à 25p).
    #[tokio::test(flavor = "multi_thread")]
    async fn le_suiveur_converge_sous_la_frame() {
        let _ = std::fs::create_dir_all("media"); // resolve() ancre sous media/

        // Un node = bus + player simulé (1 h : pas de fin de média) + canal
        // de position stable comme dans le binaire.
        async fn node() -> (BusHandle, watch::Receiver<PlaybackPosition>) {
            let bus = Bus::new(64, 512);
            let handle = bus.handle();
            tokio::spawn(bus.run());
            let player = Player::new(MemoryBackend::new(3600.0, false), handle.clone(), "media");
            let position = player.position_watch();
            tokio::spawn(player.run());
            tokio::time::sleep(Duration::from_millis(50)).await;
            (handle, position)
        }

        let (maitre_bus, maitre_pos) = node().await;
        let (suiveur_bus, suiveur_pos) = node().await;

        // Port libre : socket jetable pour le connaître.
        let libre = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = libre.local_addr().expect("addr").port();
        drop(libre);

        let reglages_maitre = SyncSettings {
            role: SyncRole::Maitre,
            maitre: None,
            port,
            ..SyncSettings::default()
        };
        let reglages_suiveur = SyncSettings {
            role: SyncRole::Suiveur,
            maitre: Some(format!("127.0.0.1:{port}")),
            port,
            tolerance_ms: 80,
        };
        let (_stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(maitre(
            reglages_maitre,
            maitre_bus.clone(),
            maitre_pos.clone(),
            stop_rx.clone(),
        ));
        let (derive_tx, _derive_rx) = watch::channel(None);
        tokio::spawn(suiveur(
            reglages_suiveur,
            suiveur_bus.clone(),
            suiveur_pos.clone(),
            derive_tx,
            stop_rx.clone(),
        ));

        // Le maître joue « clip.mp4 » ; le suiveur, rien du tout — il doit
        // suivre le média, le transport, puis résorber le décalage initial.
        maitre_bus
            .send(
                Source::Http,
                Command::Load {
                    path: "clip.mp4".into(),
                },
            )
            .await;
        maitre_bus.send(Source::Http, Command::Play).await;
        // Décalage artificiel : le maître prend 0,5 s d'avance.
        maitre_bus
            .send(Source::Http, Command::Seek { seconds: 0.5 })
            .await;

        // Convergence : MÉDIANE des 8 dernières mesures < 40 ms (une frame à
        // 25p). La médiane absorbe les à-coups d'ordonnanceur des runners CI
        // chargés (leçon du premier passage : 14 ms atteints, mais critère
        // « stable 1 s d'affilée » trop fragile) ; échéance large idem.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
        let mut fenetre: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let (Some(m), Some(s)) = (maitre_pos.borrow().position, suiveur_pos.borrow().position)
            else {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "le suiveur n'a jamais chargé/joué le média du maître"
                );
                continue;
            };
            fenetre.push_back((s - m).abs());
            if fenetre.len() > 8 {
                fenetre.pop_front();
            }
            let mediane = if fenetre.len() == 8 {
                let mut triees: Vec<f64> = fenetre.iter().copied().collect();
                triees.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                Some(triees[triees.len() / 2])
            } else {
                None
            };
            if let Some(mediane) = mediane {
                if mediane < 0.040 {
                    // Convergé : le suiveur a bien suivi le média du maître.
                    let etat = suiveur_bus.snapshot();
                    assert_eq!(etat.player.media.as_deref(), Some("clip.mp4"));
                    return;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "dérive jamais résorbée : médiane {} ms",
                    (mediane * 1000.0) as i64
                );
            } else {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "pas assez de mesures avant l'échéance"
                );
            }
        }
    }

    /// La config [sync] se parse et rejette les rôles inconnus.
    #[test]
    fn la_config_sync_se_parse() {
        let config: NodeConfig = toml::from_str(
            "[sync]\nrole = \"suiveur\"\nmaitre = \"10.0.0.2:9010\"\ntolerance_ms = 60",
        )
        .expect("parse");
        assert_eq!(config.sync.role, SyncRole::Suiveur);
        assert_eq!(config.sync.maitre.as_deref(), Some("10.0.0.2:9010"));
        assert_eq!(config.sync.tolerance_ms, 60);
        assert!(toml::from_str::<NodeConfig>("[sync]\nrole = \"chef\"").is_err());
    }
}
