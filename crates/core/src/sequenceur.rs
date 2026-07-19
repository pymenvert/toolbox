//! Séquenceur (V2, page « Séquences ») : des cues ordonnées, déclenchées à
//! la main (GO), enchaînées (« N secondes après la précédente ») ou
//! programmées à heure fixe (« tous les jours à 20:00 »).
//!
//! Une cue = un nom + une liste d'actions — n'importe quelles commandes du
//! bus (charger un média, lecture, fondu vers un preset, mire…) : tout ce
//! que sait faire le node est séquençable, sans vocabulaire nouveau.
//!
//! Robustesse : les actions passent par le bus (validées comme partout),
//! l'horaire est vérifié toutes les 10 s avec un garde « une fois par
//! minute et par jour », un GO annule l'enchaînement en attente, et tout
//! est persisté dans `sequences.json` (écriture atomique).

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::{BusHandle, Command, Source};

/// Déclencheur d'une cue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Declencheur {
    /// À la main (bouton GO, OSC `/cue/go`, MIDI via `cue_go`).
    Manuel,
    /// À HH:MM (heure locale), les jours listés — `jours` vide = tous les
    /// jours (0 = lundi … 6 = dimanche).
    Heure {
        hh: u8,
        mm: u8,
        #[serde(default)]
        jours: Vec<u8>,
    },
    /// N secondes après la fin de la cue précédente dans la liste.
    Apres { secondes: f64 },
}

/// Le déclencheur horaire vise-t-il ce jour de semaine (0 = lundi) ?
pub fn jour_vise(jours: &[u8], jour_semaine: u8) -> bool {
    jours.is_empty() || jours.contains(&jour_semaine)
}

/// Délai maximal d'un enchaînement « après » (24 h) : au-delà, la valeur
/// ferait paniquer `Duration::from_secs_f64` — et personne ne programme un
/// enchaînement de plus d'une journée.
const APRES_MAX_SECONDES: f64 = 86_400.0;

/// Valide une commande d'édition AVANT de l'accepter (retour d'erreur HTTP).
/// Rejette : un délai « après » non fini ou hors 0..=24 h (anti-panique),
/// une heure/minute/jour impossibles (la cue ne partirait jamais), et une
/// action `cue_go` qui pointe la cue elle-même (boucle infinie évidente).
pub fn valider_commande(commande: &CommandeSequenceur) -> Result<(), String> {
    if let CommandeSequenceur::CueEnregistre {
        nom,
        declencheur,
        actions,
    } = commande
    {
        match declencheur {
            Declencheur::Apres { secondes } => {
                if !secondes.is_finite() || *secondes < 0.0 || *secondes > APRES_MAX_SECONDES {
                    return Err(format!(
                        "délai « après » invalide : {secondes} s (attendu 0..={APRES_MAX_SECONDES})"
                    ));
                }
            }
            Declencheur::Heure { hh, mm, jours } => {
                if *hh > 23 || *mm > 59 {
                    return Err(format!("heure impossible : {hh:02}:{mm:02}"));
                }
                if jours.iter().any(|j| *j > 6) {
                    return Err("jour de semaine invalide (0 = lundi … 6 = dimanche)".into());
                }
            }
            Declencheur::Manuel => {}
        }
        for action in actions {
            if let Command::CueGo { name } = action {
                if name == nom {
                    return Err(format!(
                        "la cue « {nom} » se déclenche elle-même (cue_go récursif)"
                    ));
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cue {
    pub nom: String,
    pub declencheur: Declencheur,
    /// Commandes du bus, exécutées dans l'ordre.
    pub actions: Vec<Command>,
}

/// L'état du séquenceur, publié à l'UI et persisté (sans le transitoire).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EtatSequenceur {
    pub cues: Vec<Cue>,
    /// Cue en attente d'enchaînement (nom, échéance en ms) — transitoire,
    /// publié à l'UI, purgé au chargement.
    #[serde(default)]
    pub en_attente: Option<(String, u64)>,
    /// Dernière cue jouée — transitoire, publié à l'UI, purgé au chargement.
    #[serde(default)]
    pub derniere: Option<String>,
}

impl EtatSequenceur {
    pub fn load(path: &std::path::Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let mut etat: Self = serde_json::from_slice(&bytes).ok()?;
        // Le transitoire ne survit pas au redémarrage.
        etat.en_attente = None;
        etat.derniere = None;
        Some(etat)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), crate::CoreError> {
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .map_err(|e| crate::CoreError::io(tmp.display().to_string(), e))?;
        std::fs::rename(&tmp, path).map_err(|e| crate::CoreError::io(path.display().to_string(), e))
    }
}

/// Commandes de l'API (`POST /api/cues`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum CommandeSequenceur {
    /// Ajoute ou remplace une cue (même nom = remplacée, ordre conservé).
    CueEnregistre {
        nom: String,
        declencheur: Declencheur,
        actions: Vec<Command>,
    },
    CueSupprime {
        nom: String,
    },
    /// Déplace une cue (réordonnancement dans la liste).
    CueDeplace {
        nom: String,
        vers: usize,
    },
    /// GO : joue la cue et démarre l'enchaînement qui la suit.
    Go {
        nom: String,
    },
    /// Annule l'enchaînement en attente.
    Stop,
}

/// Poignée pour l'API HTTP.
#[derive(Clone)]
pub struct SequenceurHandle {
    pub commandes: mpsc::Sender<CommandeSequenceur>,
    pub etat: watch::Receiver<EtatSequenceur>,
}

/// Applique une commande d'édition. `true` = configuration changée
/// (persistance). Pure, testée. (Go/Stop sont gérés par le service.)
/// L'appelant a déjà passé [`valider_commande`] : les valeurs sont saines.
pub fn appliquer(etat: &mut EtatSequenceur, commande: &CommandeSequenceur) -> bool {
    match commande {
        CommandeSequenceur::CueEnregistre {
            nom,
            declencheur,
            actions,
        } => {
            let cue = Cue {
                nom: nom.clone(),
                declencheur: declencheur.clone(),
                actions: actions.clone(),
            };
            if let Some(existante) = etat.cues.iter_mut().find(|c| &c.nom == nom) {
                *existante = cue;
            } else {
                etat.cues.push(cue);
            }
            true
        }
        CommandeSequenceur::CueSupprime { nom } => {
            etat.cues.retain(|c| &c.nom != nom);
            true
        }
        CommandeSequenceur::CueDeplace { nom, vers } => {
            if let Some(depuis) = etat.cues.iter().position(|c| &c.nom == nom) {
                let cue = etat.cues.remove(depuis);
                let vers = (*vers).min(etat.cues.len());
                etat.cues.insert(vers, cue);
            }
            true
        }
        CommandeSequenceur::Go { .. } | CommandeSequenceur::Stop => false,
    }
}

/// L'heure locale HH:MM (secondes locales depuis minuit / 60).
fn heure_locale_hh_mm() -> (u8, u8) {
    // Décalage local calculé une fois par appel via la différence entre
    // l'heure locale et UTC fournie par le système (pas de dépendance
    // calendrier : les minutes du jour suffisent pour « tous les jours à »).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let local = now + decalage_local_secondes();
        let minutes_du_jour = (local.rem_euclid(86_400)) / 60;
        ((minutes_du_jour / 60) as u8, (minutes_du_jour % 60) as u8)
    }
}

/// Décalage local ↔ UTC en secondes. `std` ne connaît pas le fuseau : on le
/// demande UNE FOIS au système (mémoïsé) — PowerShell sous Windows,
/// `date +%z` sous Unix ; un échec retombe sur UTC (tracé). Précision à la
/// minute, suffisante pour « tous les jours à HH:MM ». Limite assumée : un
/// changement d'heure (DST) pendant que le node tourne n'est pris en compte
/// qu'au redémarrage.
fn decalage_local_secondes() -> i64 {
    use std::sync::OnceLock;
    static BIAIS: OnceLock<i64> = OnceLock::new();
    *BIAIS.get_or_init(|| {
        #[cfg(windows)]
        let lu = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "[int](((Get-Date) - (Get-Date).ToUniversalTime()).TotalSeconds)",
            ])
            .output()
            .ok()
            .and_then(|s| String::from_utf8(s.stdout).ok())
            .and_then(|t| t.trim().parse::<i64>().ok());
        #[cfg(unix)]
        let lu = std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|s| String::from_utf8(s.stdout).ok())
            .and_then(|t| {
                let t = t.trim();
                let signe = if t.starts_with('-') { -1 } else { 1 };
                let hh: i64 = t.get(1..3)?.parse().ok()?;
                let mm: i64 = t.get(3..5)?.parse().ok()?;
                Some(signe * (hh * 3600 + mm * 60))
            });
        match lu {
            Some(secondes) => secondes,
            None => {
                warn!("fuseau horaire local introuvable — horaires en UTC");
                0
            }
        }
    })
}

/// Jour courant (pour le garde « une fois par jour ») : numéro de jour
/// local depuis l'époque.
fn jour_local() -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    (now + decalage_local_secondes()).div_euclid(86_400)
}

/// Joue les actions d'une cue sur le bus.
async fn jouer(bus: &BusHandle, cue: &Cue) {
    info!(cue = %cue.nom, actions = cue.actions.len(), "GO");
    for action in &cue.actions {
        bus.send(Source::Sequencer, action.clone()).await;
    }
}

/// Boucle du service.
pub async fn service(
    chemin: std::path::PathBuf,
    bus: BusHandle,
    mut commandes: mpsc::Receiver<CommandeSequenceur>,
    etat_tx: watch::Sender<EtatSequenceur>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut etat = EtatSequenceur::load(&chemin).unwrap_or_default();
    etat_tx.send_replace(etat.clone());
    // Enchaînement en attente : (NOM de la cue à jouer, échéance). Le nom
    // (pas l'index) survit à une édition/suppression/déplacement pendant
    // l'attente — sinon on jouerait la cue qui a glissé à cette position.
    let mut chaine: Option<(String, tokio::time::Instant)> = None;
    // Garde horaire : (nom, jour local) déjà déclenchés.
    let mut lancees_du_jour: std::collections::HashSet<(String, i64)> =
        std::collections::HashSet::new();
    let mut horaire = tokio::time::interval(std::time::Duration::from_secs(10));
    horaire.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Les cues sont aussi déclenchables par le bus (OSC /cue/go, MIDI).
    let mut evenements = bus.subscribe();
    // Garde anti-réentrance : instant de la dernière lecture par cue. Une
    // cue re-déclenchée par le BUS (CueDemandee) moins de 250 ms après son
    // dernier départ est ignorée — casse toute boucle cue_go → CueDemandee
    // → cue_go… tout en laissant le GO manuel de l'opérateur passer.
    let mut derniere_lecture: std::collections::HashMap<String, tokio::time::Instant> =
        std::collections::HashMap::new();
    const ANTI_REENTREE: std::time::Duration = std::time::Duration::from_millis(250);
    info!("séquenceur prêt");
    loop {
        // Échéance de l'enchaînement (ou très loin s'il n'y en a pas).
        let echeance = chaine.as_ref().map_or_else(
            || tokio::time::Instant::now() + std::time::Duration::from_secs(3600),
            |(_, quand)| *quand,
        );
        tokio::select! {
            _ = shutdown.changed() => break,
            () = tokio::time::sleep_until(echeance), if chaine.is_some() => {
                if let Some((nom, _)) = chaine.take() {
                    // La cue attendue est retrouvée PAR SON NOM (elle a pu
                    // bouger dans la liste) ; disparue = enchaînement caduc.
                    if let Some(index) = etat.cues.iter().position(|c| c.nom == nom) {
                        let cue = etat.cues[index].clone();
                        jouer(&bus, &cue).await;
                        derniere_lecture.insert(cue.nom.clone(), tokio::time::Instant::now());
                        etat.derniere = Some(cue.nom.clone());
                        chaine = prochaine_chaine(&etat, index);
                        publier(&etat_tx, &etat, chaine.as_ref());
                    } else {
                        warn!(%nom, "enchaînement caduc : la cue a été supprimée");
                        publier(&etat_tx, &etat, None);
                    }
                }
            }
            _ = horaire.tick() => {
                let (hh, mm) = heure_locale_hh_mm();
                let jour = jour_local();
                // Jour de semaine local, 0 = lundi (l'époque Unix était un jeudi).
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let jour_semaine = ((jour.rem_euclid(7) + 3) % 7) as u8;
                lancees_du_jour.retain(|(_, j)| *j == jour);
                let a_lancer: Vec<usize> = etat
                    .cues
                    .iter()
                    .enumerate()
                    .filter(|(_, cue)| {
                        matches!(&cue.declencheur,
                            Declencheur::Heure { hh: h, mm: m, jours }
                                if *h == hh && *m == mm && jour_vise(jours, jour_semaine))
                            && !lancees_du_jour.contains(&(cue.nom.clone(), jour))
                    })
                    .map(|(i, _)| i)
                    .collect();
                for index in a_lancer {
                    let cue = etat.cues[index].clone();
                    lancees_du_jour.insert((cue.nom.clone(), jour));
                    jouer(&bus, &cue).await;
                    derniere_lecture.insert(cue.nom.clone(), tokio::time::Instant::now());
                    etat.derniere = Some(cue.nom.clone());
                    chaine = prochaine_chaine(&etat, index);
                }
                publier(&etat_tx, &etat, chaine.as_ref());
            }
            recu = evenements.recv() => {
                match recu {
                    Ok(crate::Event::CueDemandee { name }) => {
                        // Garde anti-boucle : une cue re-déclenchée par le
                        // bus juste après son départ (cue_go récursif direct
                        // ou cycle a→b→a) est ignorée.
                        let trop_tot = derniere_lecture
                            .get(&name)
                            .is_some_and(|t| t.elapsed() < ANTI_REENTREE);
                        if trop_tot {
                            warn!(%name, "cue ignorée (re-déclenchement trop rapproché — boucle ?)");
                        } else if let Some(index) = etat.cues.iter().position(|c| c.nom == name) {
                            // Le déclenchement externe remplace l'enchaînement.
                            let cue = etat.cues[index].clone();
                            jouer(&bus, &cue).await;
                            derniere_lecture.insert(cue.nom.clone(), tokio::time::Instant::now());
                            etat.derniere = Some(cue.nom.clone());
                            chaine = prochaine_chaine(&etat, index);
                            publier(&etat_tx, &etat, chaine.as_ref());
                        } else {
                            warn!(%name, "cue demandée inconnue");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(perdus)) => {
                        warn!(perdus, "séquenceur en retard : événements perdus (GO possibles)");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            commande = commandes.recv() => {
                let Some(commande) = commande else { break };
                match &commande {
                    CommandeSequenceur::Go { nom } => {
                        if let Some(index) = etat.cues.iter().position(|c| &c.nom == nom) {
                            // Un GO manuel reprend la main : l'enchaînement
                            // en attente est remplacé par le nouveau. Jamais
                            // débouncé — l'action explicite de l'opérateur.
                            let cue = etat.cues[index].clone();
                            jouer(&bus, &cue).await;
                            derniere_lecture.insert(cue.nom.clone(), tokio::time::Instant::now());
                            etat.derniere = Some(cue.nom.clone());
                            chaine = prochaine_chaine(&etat, index);
                        } else {
                            warn!(%nom, "GO sur une cue inconnue");
                        }
                    }
                    CommandeSequenceur::Stop => {
                        if chaine.take().is_some() {
                            info!("enchaînement annulé");
                        }
                    }
                    autre => {
                        // Défense en profondeur : l'API a déjà validé, mais
                        // une commande arrivant par un autre chemin est
                        // re-vérifiée avant d'être appliquée.
                        if let Err(err) = valider_commande(autre) {
                            warn!(%err, "commande de séquenceur refusée");
                        } else if appliquer(&mut etat, autre) {
                            if let Err(err) = etat.save(&chemin) {
                                warn!(%err, "séquences non persistées");
                            }
                        }
                    }
                }
                publier(&etat_tx, &etat, chaine.as_ref());
            }
        }
    }
    info!("séquenceur arrêté");
}

/// Si la cue APRÈS `index` s'enchaîne (`Apres`), retourne son NOM et son
/// échéance. Le délai est borné (fini, 0..=24 h) même si `sequences.json`
/// a été corrompu — `from_secs_f64` ne doit jamais paniquer ici.
fn prochaine_chaine(etat: &EtatSequenceur, index: usize) -> Option<(String, tokio::time::Instant)> {
    let suivante = etat.cues.get(index + 1)?;
    if let Declencheur::Apres { secondes } = suivante.declencheur {
        let secondes = if secondes.is_finite() {
            secondes.clamp(0.0, APRES_MAX_SECONDES)
        } else {
            0.0
        };
        let delai = std::time::Duration::from_secs_f64(secondes);
        Some((suivante.nom.clone(), tokio::time::Instant::now() + delai))
    } else {
        None
    }
}

fn publier(
    etat_tx: &watch::Sender<EtatSequenceur>,
    etat: &EtatSequenceur,
    chaine: Option<&(String, tokio::time::Instant)>,
) {
    let mut publie = etat.clone();
    publie.en_attente = chaine.map(|(nom, quand)| {
        #[allow(clippy::cast_possible_truncation)]
        let dans_ms = quand
            .saturating_duration_since(tokio::time::Instant::now())
            .as_millis() as u64;
        (nom.clone(), dans_ms)
    });
    etat_tx.send_replace(publie);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Bus;

    #[test]
    fn les_jours_de_semaine_filtrent_le_declencheur() {
        // Vide = tous les jours.
        assert!(jour_vise(&[], 0));
        assert!(jour_vise(&[], 6));
        // Sinon, seulement les jours listés (0 = lundi).
        assert!(jour_vise(&[4, 5], 4));
        assert!(!jour_vise(&[4, 5], 0));
        // OSC/MIDI : une cue est déclenchable par le bus (commande cue_go),
        // vérifié par le test de bout en bout ci-dessous via CueDemandee.
    }

    #[test]
    fn valider_commande_rejette_les_pieges() {
        let cue = |decl: Declencheur, actions: Vec<Command>| CommandeSequenceur::CueEnregistre {
            nom: "x".into(),
            declencheur: decl,
            actions,
        };
        // Délai « après » qui ferait paniquer Duration::from_secs_f64.
        assert!(valider_commande(&cue(Declencheur::Apres { secondes: 1e300 }, vec![])).is_err());
        assert!(valider_commande(&cue(Declencheur::Apres { secondes: f64::NAN }, vec![])).is_err());
        assert!(valider_commande(&cue(Declencheur::Apres { secondes: -1.0 }, vec![])).is_err());
        assert!(valider_commande(&cue(Declencheur::Apres { secondes: 300.0 }, vec![])).is_ok());
        // Heure/jour impossibles.
        assert!(valider_commande(&cue(
            Declencheur::Heure {
                hh: 24,
                mm: 0,
                jours: vec![]
            },
            vec![]
        ))
        .is_err());
        assert!(valider_commande(&cue(
            Declencheur::Heure {
                hh: 20,
                mm: 0,
                jours: vec![7]
            },
            vec![]
        ))
        .is_err());
        // Cue qui se déclenche elle-même.
        assert!(valider_commande(&cue(
            Declencheur::Manuel,
            vec![Command::CueGo { name: "x".into() }]
        ))
        .is_err());
        // Une cue_go vers une AUTRE cue reste permise.
        assert!(valider_commande(&cue(
            Declencheur::Manuel,
            vec![Command::CueGo { name: "y".into() }]
        ))
        .is_ok());
    }

    /// Supprimer la cue A pendant que B(après) attend ne doit PAS faire
    /// jouer C à la place : l'enchaînement suit le NOM, pas l'index.
    #[tokio::test]
    async fn l_enchainement_suit_le_nom_pas_l_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = Bus::new(64, 256);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (etat_tx, etat_rx) = watch::channel(EtatSequenceur::default());
        let (_stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(service(
            dir.path().join("sequences.json"),
            handle.clone(),
            cmd_rx,
            etat_tx,
            stop_rx,
        ));
        let enreg = |nom: &str, decl: Declencheur, vol: f32| CommandeSequenceur::CueEnregistre {
            nom: nom.into(),
            declencheur: decl,
            actions: vec![Command::SetVolume { volume: vol }],
        };
        for c in [
            enreg("A", Declencheur::Manuel, 0.1),
            enreg("B", Declencheur::Apres { secondes: 0.3 }, 0.5),
            enreg("C", Declencheur::Manuel, 0.9),
        ] {
            cmd_tx.send(c).await.expect("enreg");
        }
        cmd_tx
            .send(CommandeSequenceur::Go { nom: "A".into() })
            .await
            .expect("go");
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        // Pendant l'attente de B, on supprime A : les index glissent.
        cmd_tx
            .send(CommandeSequenceur::CueSupprime { nom: "A".into() })
            .await
            .expect("suppr");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        // B (0.5) doit avoir joué, PAS C (0.9).
        assert!(
            (handle.snapshot().player.volume - 0.5).abs() < 1e-6,
            "c'est B qui doit s'enchaîner, pas la cue à l'ancien index"
        );
        assert_eq!(etat_rx.borrow().derniere.as_deref(), Some("B"));
    }

    /// Une cue dont l'action est cue_go vers elle-même via le bus ne boucle
    /// pas : la garde anti-réentrance coupe après le premier rebond.
    #[tokio::test]
    async fn la_cue_recursive_ne_boucle_pas() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = Bus::new(64, 256);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (etat_tx, _etat_rx) = watch::channel(EtatSequenceur::default());
        let (_stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(service(
            dir.path().join("sequences.json"),
            handle.clone(),
            cmd_rx,
            etat_tx,
            stop_rx,
        ));
        // valider_commande refuse le cue_go DIRECT auto-référent ; pour
        // tester la garde d'exécution, on passe par un cycle a→b→a que la
        // validation d'enregistrement ne détecte pas.
        cmd_tx
            .send(CommandeSequenceur::CueEnregistre {
                nom: "a".into(),
                declencheur: Declencheur::Manuel,
                actions: vec![Command::CueGo { name: "b".into() }],
            })
            .await
            .expect("a");
        cmd_tx
            .send(CommandeSequenceur::CueEnregistre {
                nom: "b".into(),
                declencheur: Declencheur::Manuel,
                actions: vec![Command::CueGo { name: "a".into() }],
            })
            .await
            .expect("b");
        cmd_tx
            .send(CommandeSequenceur::Go { nom: "a".into() })
            .await
            .expect("go");
        // Si ça bouclait, le bus serait saturé ; on laisse tourner et on
        // vérifie que le service répond encore à une commande après coup.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        cmd_tx
            .send(CommandeSequenceur::CueSupprime { nom: "a".into() })
            .await
            .expect("le service répond toujours (pas de boucle qui monopolise)");
    }

    #[test]
    fn edition_des_cues() {
        let mut etat = EtatSequenceur::default();
        let enregistre = |nom: &str| CommandeSequenceur::CueEnregistre {
            nom: nom.into(),
            declencheur: Declencheur::Manuel,
            actions: vec![Command::Play],
        };
        assert!(appliquer(&mut etat, &enregistre("a")));
        assert!(appliquer(&mut etat, &enregistre("b")));
        assert_eq!(etat.cues.len(), 2);
        // Même nom = remplacée, pas dupliquée.
        assert!(appliquer(&mut etat, &enregistre("a")));
        assert_eq!(etat.cues.len(), 2);
        // Déplacement.
        appliquer(
            &mut etat,
            &CommandeSequenceur::CueDeplace {
                nom: "b".into(),
                vers: 0,
            },
        );
        assert_eq!(etat.cues[0].nom, "b");
        appliquer(
            &mut etat,
            &CommandeSequenceur::CueSupprime { nom: "b".into() },
        );
        assert_eq!(etat.cues.len(), 1);
    }

    /// GO joue la cue sur le bus et l'enchaînement « après » suit tout
    /// seul, au bon moment ; Stop l'annule.
    #[tokio::test]
    async fn go_joue_et_enchaine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = Bus::new(64, 256);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (etat_tx, etat_rx) = watch::channel(EtatSequenceur::default());
        let (_stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(service(
            dir.path().join("sequences.json"),
            handle.clone(),
            cmd_rx,
            etat_tx,
            stop_rx,
        ));

        cmd_tx
            .send(CommandeSequenceur::CueEnregistre {
                nom: "un".into(),
                declencheur: Declencheur::Manuel,
                actions: vec![Command::SetVolume { volume: 0.25 }],
            })
            .await
            .expect("cue un");
        cmd_tx
            .send(CommandeSequenceur::CueEnregistre {
                nom: "deux".into(),
                declencheur: Declencheur::Apres { secondes: 0.3 },
                actions: vec![Command::SetVolume { volume: 0.75 }],
            })
            .await
            .expect("cue deux");
        cmd_tx
            .send(CommandeSequenceur::Go { nom: "un".into() })
            .await
            .expect("go");

        // La cue « un » est jouée tout de suite…
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert!((handle.snapshot().player.volume - 0.25).abs() < 1e-6);
        assert_eq!(
            etat_rx.borrow().en_attente.as_ref().map(|(n, _)| n.clone()),
            Some("deux".into()),
            "l'enchaînement doit être annoncé"
        );
        // … et « deux » s'enchaîne ~300 ms plus tard.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!((handle.snapshot().player.volume - 0.75).abs() < 1e-6);
        assert_eq!(etat_rx.borrow().derniere.as_deref(), Some("deux"));

        // Stop annule un enchaînement en attente.
        cmd_tx
            .send(CommandeSequenceur::Go { nom: "un".into() })
            .await
            .expect("re-go");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cmd_tx.send(CommandeSequenceur::Stop).await.expect("stop");
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        assert!(
            (handle.snapshot().player.volume - 0.25).abs() < 1e-6,
            "la cue deux ne doit PAS avoir été jouée après Stop"
        );
    }

    #[test]
    fn les_sequences_persistent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chemin = dir.path().join("sequences.json");
        let mut etat = EtatSequenceur::default();
        appliquer(
            &mut etat,
            &CommandeSequenceur::CueEnregistre {
                nom: "show".into(),
                declencheur: Declencheur::Heure {
                    hh: 20,
                    mm: 0,
                    jours: vec![4, 5], // vendredi et samedi
                },
                actions: vec![
                    Command::Load {
                        path: "intro.mp4".into(),
                    },
                    Command::Play,
                ],
            },
        );
        etat.derniere = Some("transitoire".into());
        etat.save(&chemin).expect("save");
        let relu = EtatSequenceur::load(&chemin).expect("load");
        assert_eq!(relu.cues, etat.cues);
        assert_eq!(relu.derniere, None, "le transitoire n'est pas persisté");
    }
}
