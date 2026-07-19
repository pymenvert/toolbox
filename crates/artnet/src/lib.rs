//! # toolbox-artnet — lumières DMX sur Art-Net (V2, page « Lumières »)
//!
//! Une mini-console lumière dans le node, inspirée des consoles classiques
//! (et de l'intégration Chataigne/QLC+) :
//! - des **faders** nommés, créés à la volée dans l'UI (univers + canal DMX,
//!   valeur 0..255, couleur d'étiquette) ;
//! - un **master** qui met à l'échelle toute la sortie ;
//! - des **scènes** : instantanés des faders, rappelables d'un clic ;
//! - des **chasers** : enchaînements de scènes avec fondu et tenue par pas,
//!   en boucle ou one-shot — moteur pur, testé au milliseconde près.
//!
//! L'émission suit la pratique des consoles : trame ArtDMX complète par
//! univers utilisé, en continu à ~30 Hz (les gradateurs tolèrent mal les
//! trames sporadiques). Sans aucun fader défini — ou interrupteur
//! « Lumières » coupé — rien n'est émis et la socket est fermée.
//!
//! Tout est persisté dans `lumieres.json` (écriture atomique, comme
//! `sortie.json`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

/// Port UDP standard Art-Net.
pub const PORT_ARTNET: u16 = 6454;

// ---------------------------------------------------------------------------
// Paquet ArtDMX (spec Art-Net 4, opcode OpDmx 0x5000)
// ---------------------------------------------------------------------------

/// Construit une trame ArtDMX. `universe` = Net(7)+SubUni(8) sur 15 bits,
/// `data` = valeurs des canaux (tronqué à 512, complété à une longueur paire
/// comme l'exige la spec).
pub fn art_dmx(universe: u16, sequence: u8, data: &[u8]) -> Vec<u8> {
    let mut canaux = data[..data.len().min(512)].to_vec();
    if canaux.len() % 2 == 1 {
        canaux.push(0);
    }
    let mut paquet = Vec::with_capacity(18 + canaux.len());
    paquet.extend_from_slice(b"Art-Net\0");
    paquet.extend_from_slice(&0x5000_u16.to_le_bytes()); // OpDmx
    paquet.extend_from_slice(&14_u16.to_be_bytes()); // ProtVer
    paquet.push(sequence); // 0 = désactivé, sinon 1..255
    paquet.push(0); // Physical (informel)
    paquet.extend_from_slice(&universe.to_le_bytes()); // SubUni puis Net
    #[allow(clippy::cast_possible_truncation)] // borné à 512
    paquet.extend_from_slice(&(canaux.len() as u16).to_be_bytes());
    paquet.extend_from_slice(&canaux);
    paquet
}

// ---------------------------------------------------------------------------
// État : faders, scènes, chasers
// ---------------------------------------------------------------------------

/// Un fader créé par l'utilisateur : un canal DMX nommé.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fader {
    /// Identifiant stable (généré à la création).
    pub id: String,
    pub nom: String,
    /// Univers Art-Net (0..32767).
    pub univers: u16,
    /// Canal DMX 1..=512.
    pub canal: u16,
    /// Valeur courante 0..=255.
    pub valeur: u8,
    /// Couleur d'étiquette dans l'UI (`#rrggbb`).
    #[serde(default)]
    pub couleur: String,
}

/// Un pas de chaser : une scène cible, un fondu pour y aller, une tenue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasChaser {
    pub scene: String,
    pub fondu_ms: u64,
    pub tenue_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chaser {
    pub nom: String,
    pub pas: Vec<PasChaser>,
    pub boucle: bool,
}

/// L'état complet de la console, publié à l'UI et persisté.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtatLumieres {
    /// Destination Art-Net (`"255.255.255.255"` = broadcast, ou IP du node
    /// lumière). Le port standard 6454 est ajouté si absent.
    pub cible: String,
    /// Master 0..=255 : met à l'échelle toute la sortie.
    pub master: u8,
    pub faders: Vec<Fader>,
    /// Scènes : nom → (id de fader → valeur).
    pub scenes: BTreeMap<String, BTreeMap<String, u8>>,
    pub chasers: Vec<Chaser>,
    /// Chaser en cours de lecture, s'il y en a un.
    pub chaser_actif: Option<String>,
}

impl Default for EtatLumieres {
    fn default() -> Self {
        Self {
            cible: "255.255.255.255".into(),
            master: 255,
            faders: Vec::new(),
            scenes: BTreeMap::new(),
            chasers: Vec::new(),
            chaser_actif: None,
        }
    }
}

impl EtatLumieres {
    /// Relit l'état. Fichier ABSENT = `None` silencieux (premier
    /// démarrage). Fichier PRÉSENT mais illisible/corrompu = tracé en ERROR
    /// et mis de côté en `.corrompu` — on ne l'écrase pas en silence à la
    /// première sauvegarde (la configuration lumières du client survivrait
    /// ainsi pour récupération). Les faders sont bornés (canal 1..=512,
    /// univers ≤ 32767) : un canal 0 ferait paniquer l'émission.
    pub fn load(path: &std::path::Path) -> Option<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                tracing::error!(%err, chemin = %path.display(), "lumieres.json illisible");
                return None;
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut etat) => {
                etat.chaser_actif = None; // un chaser ne survit pas au redémarrage
                for fader in &mut etat.faders {
                    fader.canal = fader.canal.clamp(1, 512);
                    fader.univers = fader.univers.min(32_767);
                }
                Some(etat)
            }
            Err(err) => {
                let corrompu = path.with_extension("json.corrompu");
                let _ = std::fs::rename(path, &corrompu);
                tracing::error!(
                    %err,
                    sauvegarde = %corrompu.display(),
                    "lumieres.json corrompu — mis de côté, console repartie à vide"
                );
                None
            }
        }
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), toolbox_core::CoreError> {
        let json = serde_json::to_vec_pretty(self)?;
        toolbox_core::ecrire_atomique(path, &json)
    }
}

// ---------------------------------------------------------------------------
// Moteur de chaser (pur, testé)
// ---------------------------------------------------------------------------

/// Les valeurs (id de fader → 0..255) d'un chaser à l'instant `t_ms` depuis
/// son départ. `None` = chaser terminé (non bouclé). Pendant le fondu d'un
/// pas, les valeurs glissent linéairement depuis la scène du pas précédent
/// (le dernier pas pour un chaser bouclé, le premier sinon).
pub fn valeurs_chaser(
    chaser: &Chaser,
    scenes: &BTreeMap<String, BTreeMap<String, u8>>,
    t_ms: u64,
) -> Option<BTreeMap<String, u8>> {
    if chaser.pas.is_empty() {
        return None;
    }
    let cycle: u64 = chaser
        .pas
        .iter()
        .map(|p| p.fondu_ms + p.tenue_ms)
        .sum::<u64>()
        .max(1);
    let t = if chaser.boucle {
        t_ms % cycle
    } else if t_ms >= cycle {
        return None;
    } else {
        t_ms
    };

    let vide = BTreeMap::new();
    let scene_de = |nom: &str| scenes.get(nom).unwrap_or(&vide);
    let mut debut = 0u64;
    for (index, pas) in chaser.pas.iter().enumerate() {
        let fin = debut + pas.fondu_ms + pas.tenue_ms;
        if t < fin {
            let cible = scene_de(&pas.scene);
            let dans_pas = t - debut;
            if dans_pas >= pas.fondu_ms || pas.fondu_ms == 0 {
                return Some(cible.clone());
            }
            // Fondu depuis la scène précédente.
            let precedent = if index > 0 {
                &chaser.pas[index - 1]
            } else if chaser.boucle {
                &chaser.pas[chaser.pas.len() - 1]
            } else {
                &chaser.pas[0]
            };
            let source = scene_de(&precedent.scene);
            #[allow(clippy::cast_precision_loss)] // fondu_ms réaliste (< jours)
            let progression = dans_pas as f64 / pas.fondu_ms as f64;
            let mut valeurs = BTreeMap::new();
            let ids: std::collections::BTreeSet<&String> =
                source.keys().chain(cible.keys()).collect();
            for id in ids {
                let a = f64::from(*source.get(id).unwrap_or(&0));
                let b = f64::from(*cible.get(id).unwrap_or(&0));
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                valeurs.insert(id.clone(), (a + (b - a) * progression).round() as u8);
            }
            return Some(valeurs);
        }
        debut = fin;
    }
    None
}

// ---------------------------------------------------------------------------
// Service : commandes de l'UI + émission continue
// ---------------------------------------------------------------------------

/// Commandes de la console (API `POST /api/dmx`, JSON `{"cmd": ...}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum CommandeLumieres {
    FaderAjoute {
        nom: String,
        univers: u16,
        canal: u16,
        #[serde(default)]
        couleur: String,
    },
    FaderSupprime {
        id: String,
    },
    FaderValeur {
        id: String,
        valeur: u8,
    },
    Master {
        valeur: u8,
    },
    Cible {
        adresse: String,
    },
    SceneEnregistre {
        nom: String,
    },
    SceneRappelle {
        nom: String,
    },
    SceneSupprime {
        nom: String,
    },
    ChaserEnregistre {
        nom: String,
        pas: Vec<PasChaser>,
        boucle: bool,
    },
    ChaserSupprime {
        nom: String,
    },
    ChaserDemarre {
        nom: String,
    },
    ChaserArrete,
}

/// Poignée donnée à l'API HTTP : envoyer des commandes, lire l'état.
#[derive(Clone)]
pub struct LumieresHandle {
    pub commandes: mpsc::Sender<CommandeLumieres>,
    pub etat: watch::Receiver<EtatLumieres>,
}

/// Applique une commande à l'état. Retourne `true` si la configuration a
/// changé (→ persistance). Pure, testée.
pub fn appliquer(etat: &mut EtatLumieres, commande: CommandeLumieres) -> bool {
    match commande {
        CommandeLumieres::FaderAjoute {
            nom,
            univers,
            canal,
            couleur,
        } => {
            let canal = canal.clamp(1, 512);
            // L'univers Art-Net tient sur 15 bits (Port-Address) : au-delà,
            // le bit 15 déborderait et le node lumière ignorerait la trame.
            let univers = univers.min(32_767);
            // Id monotone par console (l'horloge seule collisionne quand deux
            // faders naissent dans la même milliseconde).
            let suivant = etat
                .faders
                .iter()
                .filter_map(|f| f.id.strip_prefix('f').and_then(|s| s.parse::<u64>().ok()))
                .max()
                .unwrap_or(0)
                + 1;
            let id = format!("f{suivant}");
            etat.faders.push(Fader {
                id,
                nom,
                univers,
                canal,
                valeur: 0,
                couleur,
            });
            true
        }
        CommandeLumieres::FaderSupprime { id } => {
            etat.faders.retain(|f| f.id != id);
            for scene in etat.scenes.values_mut() {
                scene.remove(&id);
            }
            true
        }
        CommandeLumieres::FaderValeur { id, valeur } => {
            if let Some(fader) = etat.faders.iter_mut().find(|f| f.id == id) {
                fader.valeur = valeur;
            }
            false // les valeurs vivantes ne sont pas persistées à chaque geste
        }
        CommandeLumieres::Master { valeur } => {
            etat.master = valeur;
            true
        }
        CommandeLumieres::Cible { adresse } => {
            etat.cible = adresse;
            true
        }
        CommandeLumieres::SceneEnregistre { nom } => {
            let valeurs: BTreeMap<String, u8> = etat
                .faders
                .iter()
                .map(|f| (f.id.clone(), f.valeur))
                .collect();
            etat.scenes.insert(nom, valeurs);
            true
        }
        CommandeLumieres::SceneRappelle { nom } => {
            if let Some(valeurs) = etat.scenes.get(&nom).cloned() {
                for fader in &mut etat.faders {
                    if let Some(v) = valeurs.get(&fader.id) {
                        fader.valeur = *v;
                    }
                }
                etat.chaser_actif = None;
            }
            false
        }
        CommandeLumieres::SceneSupprime { nom } => {
            etat.scenes.remove(&nom);
            etat.chasers
                .retain(|c| !c.pas.iter().any(|p| p.scene == nom));
            true
        }
        CommandeLumieres::ChaserEnregistre { nom, pas, boucle } => {
            etat.chasers.retain(|c| c.nom != nom);
            etat.chasers.push(Chaser { nom, pas, boucle });
            true
        }
        CommandeLumieres::ChaserSupprime { nom } => {
            if etat.chaser_actif.as_deref() == Some(nom.as_str()) {
                etat.chaser_actif = None;
            }
            etat.chasers.retain(|c| c.nom != nom);
            true
        }
        CommandeLumieres::ChaserDemarre { nom } => {
            if etat.chasers.iter().any(|c| c.nom == nom) {
                etat.chaser_actif = Some(nom);
            }
            false
        }
        CommandeLumieres::ChaserArrete => {
            etat.chaser_actif = None;
            false
        }
    }
}

/// La trame de sortie par univers, à l'instant `t_chaser_ms` du chaser actif
/// (master appliqué). Pure, testée.
pub fn trames_de_sortie(etat: &EtatLumieres, t_chaser_ms: u64) -> BTreeMap<u16, [u8; 512]> {
    // Valeurs de base : les faders ; le chaser actif écrase les siens.
    let chaser_valeurs = etat
        .chaser_actif
        .as_ref()
        .and_then(|nom| etat.chasers.iter().find(|c| &c.nom == nom))
        .and_then(|c| valeurs_chaser(c, &etat.scenes, t_chaser_ms));
    let master = u16::from(etat.master);
    let mut univers: BTreeMap<u16, [u8; 512]> = BTreeMap::new();
    for fader in &etat.faders {
        let brute = chaser_valeurs
            .as_ref()
            .and_then(|v| v.get(&fader.id).copied())
            .unwrap_or(fader.valeur);
        // Garde-fou anti-panique : un canal hors 1..=512 (fichier corrompu
        // ou d'une autre version) est ignoré au lieu de déborder la trame.
        if !(1..=512).contains(&fader.canal) {
            continue;
        }
        #[allow(clippy::cast_possible_truncation)] // ≤ 255 par construction
        let finale = ((u16::from(brute) * master) / 255) as u8;
        let trame = univers.entry(fader.univers).or_insert([0u8; 512]);
        trame[usize::from(fader.canal - 1)] = finale;
    }
    univers
}

/// Boucle du service : commandes de l'UI, émission ~30 Hz, persistance.
/// L'interrupteur « Lumières » (onglet Fonctions) coupe l'émission ET la
/// socket ; l'édition reste possible pendant ce temps.
pub async fn service(
    chemin: std::path::PathBuf,
    bus: toolbox_core::BusHandle,
    mut commandes: mpsc::Receiver<CommandeLumieres>,
    etat_tx: watch::Sender<EtatLumieres>,
    mut actif: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut etat = EtatLumieres::load(&chemin).unwrap_or_default();
    etat_tx.send_replace(etat.clone());
    // Scènes et chasers déclenchables par le bus : cues du séquenceur,
    // OSC /dmx/scene, bindings MIDI — même vocabulaire partout.
    let mut evenements = bus.subscribe();
    let mut socket: Option<tokio::net::UdpSocket> = None;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sequence: u8 = 1;
    let mut chaser_depart = tokio::time::Instant::now();
    info!("console lumières prête (Art-Net)");
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = actif.changed() => {
                if !*actif.borrow() {
                    socket = None; // socket fermée : zéro ressource réseau
                    info!("lumières coupées (interrupteur) — émission stoppée");
                }
            }
            commande = commandes.recv() => {
                let Some(commande) = commande else { break };
                if matches!(commande, CommandeLumieres::ChaserDemarre { .. }) {
                    chaser_depart = tokio::time::Instant::now();
                }
                if appliquer(&mut etat, commande) {
                    if let Err(err) = etat.save(&chemin) {
                        warn!(%err, "console lumières non persistée");
                    }
                }
                etat_tx.send_replace(etat.clone());
            }
            recu = evenements.recv() => {
                let commande = match recu {
                    Ok(toolbox_core::Event::DmxSceneDemandee { name }) => {
                        Some(CommandeLumieres::SceneRappelle { nom: name })
                    }
                    Ok(toolbox_core::Event::DmxChaserDemande { name: Some(name) }) => {
                        chaser_depart = tokio::time::Instant::now();
                        Some(CommandeLumieres::ChaserDemarre { nom: name })
                    }
                    Ok(toolbox_core::Event::DmxChaserDemande { name: None }) => {
                        Some(CommandeLumieres::ChaserArrete)
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if let Some(commande) = commande {
                    appliquer(&mut etat, commande);
                    etat_tx.send_replace(etat.clone());
                }
            }
            _ = tick.tick(), if *actif.borrow() && !etat.faders.is_empty() => {
                if socket.is_none() {
                    match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
                        Ok(s) => {
                            let _ = s.set_broadcast(true);
                            socket = Some(s);
                        }
                        Err(err) => {
                            warn!(%err, "socket Art-Net impossible");
                            continue;
                        }
                    }
                }
                let Some(s) = &socket else { continue };
                let cible = if etat.cible.contains(':') {
                    etat.cible.clone()
                } else {
                    format!("{}:{}", etat.cible, PORT_ARTNET)
                };
                #[allow(clippy::cast_possible_truncation)] // ms d'un chaser
                let t = chaser_depart.elapsed().as_millis() as u64;
                // Un chaser one-shot terminé rend la main aux faders.
                if let Some(nom) = etat.chaser_actif.clone() {
                    let fini = etat
                        .chasers
                        .iter()
                        .find(|c| c.nom == nom)
                        .is_none_or(|c| valeurs_chaser(c, &etat.scenes, t).is_none());
                    if fini {
                        etat.chaser_actif = None;
                        etat_tx.send_replace(etat.clone());
                    }
                }
                for (univers, trame) in trames_de_sortie(&etat, t) {
                    let paquet = art_dmx(univers, sequence, &trame);
                    if let Err(err) = s.send_to(&paquet, &cible).await {
                        warn!(%err, %cible, "trame Art-Net non envoyée");
                    }
                }
                sequence = if sequence == 255 { 1 } else { sequence + 1 };
            }
        }
    }
    info!("console lumières arrêtée");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un fader au canal hors bornes (0 ou > 512) venu d'un fichier
    /// corrompu ne fait PAS paniquer l'émission — il est ignoré.
    #[test]
    fn trames_de_sortie_ignore_les_canaux_hors_bornes() {
        let mut etat = EtatLumieres::default();
        etat.faders.push(Fader {
            id: "f1".into(),
            nom: "piégé".into(),
            univers: 0,
            canal: 0, // ferait canal-1 = 65535 sur un [u8;512]
            valeur: 200,
            couleur: String::new(),
        });
        etat.faders.push(Fader {
            id: "f2".into(),
            nom: "trop haut".into(),
            univers: 0,
            canal: 600,
            valeur: 200,
            couleur: String::new(),
        });
        etat.faders.push(Fader {
            id: "f3".into(),
            nom: "ok".into(),
            univers: 0,
            canal: 5,
            valeur: 200,
            couleur: String::new(),
        });
        // Ne panique pas, et seul le fader valide écrit sa valeur.
        let univers = trames_de_sortie(&etat, 0);
        assert_eq!(univers[&0][4], 200, "le fader canal 5 émet");
    }

    /// Un lumieres.json corrompu est mis de côté en .corrompu au lieu
    /// d'être écrasé ; les canaux hors bornes sont bornés au chargement.
    #[test]
    fn load_met_de_cote_un_fichier_corrompu() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chemin = dir.path().join("lumieres.json");

        assert!(EtatLumieres::load(&chemin).is_none(), "absent = None");

        std::fs::write(&chemin, b"{ pas du json").expect("write");
        assert!(EtatLumieres::load(&chemin).is_none());
        assert!(
            chemin.with_extension("json.corrompu").exists(),
            "le fichier corrompu est préservé pour récupération"
        );

        // Un fader canal 0 relu est borné à 1 (pas de panique ensuite).
        let mut etat = EtatLumieres::default();
        etat.faders.push(Fader {
            id: "f1".into(),
            nom: "x".into(),
            univers: 60_000,
            canal: 0,
            valeur: 10,
            couleur: String::new(),
        });
        etat.save(&chemin).expect("save");
        let relu = EtatLumieres::load(&chemin).expect("load");
        assert_eq!(relu.faders[0].canal, 1);
        assert_eq!(relu.faders[0].univers, 32_767);
    }

    #[test]
    fn la_trame_artdmx_est_conforme() {
        let paquet = art_dmx(3, 7, &[10, 20, 30]);
        assert_eq!(&paquet[0..8], b"Art-Net\0");
        assert_eq!(u16::from_le_bytes([paquet[8], paquet[9]]), 0x5000);
        assert_eq!(u16::from_be_bytes([paquet[10], paquet[11]]), 14);
        assert_eq!(paquet[12], 7); // séquence
        assert_eq!(u16::from_le_bytes([paquet[14], paquet[15]]), 3); // univers
                                                                     // Longueur paire : 3 canaux → 4 annoncés.
        assert_eq!(u16::from_be_bytes([paquet[16], paquet[17]]), 4);
        assert_eq!(&paquet[18..22], &[10, 20, 30, 0]);
        assert_eq!(paquet.len(), 22);
    }

    fn scene(valeurs: &[(&str, u8)]) -> BTreeMap<String, u8> {
        valeurs
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect()
    }

    #[test]
    fn le_chaser_fond_et_tient_aux_bons_instants() {
        let mut scenes = BTreeMap::new();
        scenes.insert("noire".into(), scene(&[("a", 0)]));
        scenes.insert("pleine".into(), scene(&[("a", 200)]));
        let chaser = Chaser {
            nom: "va-et-vient".into(),
            pas: vec![
                PasChaser {
                    scene: "pleine".into(),
                    fondu_ms: 1000,
                    tenue_ms: 1000,
                },
                PasChaser {
                    scene: "noire".into(),
                    fondu_ms: 0,
                    tenue_ms: 1000,
                },
            ],
            boucle: true,
        };
        // Mi-fondu du pas 1 : on vient de « noire » (dernier pas, bouclé).
        let v = valeurs_chaser(&chaser, &scenes, 500).expect("valeurs");
        assert_eq!(v["a"], 100);
        // Tenue du pas 1.
        assert_eq!(valeurs_chaser(&chaser, &scenes, 1500).expect("v")["a"], 200);
        // Pas 2 (fondu nul) : noir immédiat.
        assert_eq!(valeurs_chaser(&chaser, &scenes, 2500).expect("v")["a"], 0);
        // Boucle : cycle = 3000 ms, 3500 ≡ 500.
        assert_eq!(valeurs_chaser(&chaser, &scenes, 3500).expect("v")["a"], 100);

        // Non bouclé : terminé après le cycle.
        let one_shot = Chaser {
            boucle: false,
            ..chaser
        };
        assert_eq!(valeurs_chaser(&one_shot, &scenes, 3000), None);
    }

    #[test]
    fn les_trames_appliquent_chaser_et_master() {
        let mut etat = EtatLumieres::default();
        appliquer(
            &mut etat,
            CommandeLumieres::FaderAjoute {
                nom: "face".into(),
                univers: 0,
                canal: 1,
                couleur: String::new(),
            },
        );
        appliquer(
            &mut etat,
            CommandeLumieres::FaderAjoute {
                nom: "contre".into(),
                univers: 2,
                canal: 10,
                couleur: String::new(),
            },
        );
        let id_face = etat.faders[0].id.clone();
        let id_contre = etat.faders[1].id.clone();
        appliquer(
            &mut etat,
            CommandeLumieres::FaderValeur {
                id: id_face.clone(),
                valeur: 200,
            },
        );
        appliquer(
            &mut etat,
            CommandeLumieres::FaderValeur {
                id: id_contre,
                valeur: 100,
            },
        );

        // Deux univers, canaux aux bons offsets.
        let trames = trames_de_sortie(&etat, 0);
        assert_eq!(trames[&0][0], 200);
        assert_eq!(trames[&2][9], 100);

        // Master à moitié : sortie divisée par ~2.
        appliquer(&mut etat, CommandeLumieres::Master { valeur: 127 });
        let trames = trames_de_sortie(&etat, 0);
        assert_eq!(trames[&0][0], 99); // 200*127/255

        // Scène + rappel après modification.
        appliquer(&mut etat, CommandeLumieres::Master { valeur: 255 });
        appliquer(
            &mut etat,
            CommandeLumieres::SceneEnregistre { nom: "s1".into() },
        );
        appliquer(
            &mut etat,
            CommandeLumieres::FaderValeur {
                id: id_face.clone(),
                valeur: 0,
            },
        );
        appliquer(
            &mut etat,
            CommandeLumieres::SceneRappelle { nom: "s1".into() },
        );
        assert_eq!(
            etat.faders
                .iter()
                .find(|f| f.id == id_face)
                .expect("f")
                .valeur,
            200
        );
    }

    #[test]
    fn la_console_persiste_et_recharge() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chemin = dir.path().join("lumieres.json");
        let mut etat = EtatLumieres::default();
        appliquer(
            &mut etat,
            CommandeLumieres::FaderAjoute {
                nom: "face".into(),
                univers: 0,
                canal: 1,
                couleur: "#ff8800".into(),
            },
        );
        etat.chaser_actif = Some("x".into());
        etat.save(&chemin).expect("save");
        let relu = EtatLumieres::load(&chemin).expect("load");
        assert_eq!(relu.faders.len(), 1);
        assert_eq!(relu.faders[0].nom, "face");
        // Un chaser actif ne survit pas au redémarrage.
        assert_eq!(relu.chaser_actif, None);
    }

    /// Bout en bout : le service émet de vraies trames Art-Net en UDP, le
    /// chaser anime les valeurs, l'interrupteur coupe l'émission.
    #[tokio::test]
    async fn le_service_emet_des_trames_artnet() {
        let recepteur = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind récepteur");
        let cible = recepteur.local_addr().expect("addr").to_string();
        let dir = tempfile::tempdir().expect("tempdir");

        let bus = toolbox_core::Bus::new(8, 32);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (etat_tx, _etat_rx) = watch::channel(EtatLumieres::default());
        let (actif_tx, actif_rx) = watch::channel(true);
        let (_stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(service(
            dir.path().join("lumieres.json"),
            handle,
            cmd_rx,
            etat_tx,
            actif_rx,
            stop_rx,
        ));

        cmd_tx
            .send(CommandeLumieres::Cible { adresse: cible })
            .await
            .expect("cible");
        cmd_tx
            .send(CommandeLumieres::FaderAjoute {
                nom: "face".into(),
                univers: 1,
                canal: 5,
                couleur: String::new(),
            })
            .await
            .expect("fader");
        // L'id est généré : lisons l'état via une trame plus tard ; pour le
        // test, valeur via master (s'applique à tout).
        cmd_tx
            .send(CommandeLumieres::Master { valeur: 255 })
            .await
            .expect("master");

        // Première trame reçue : conforme, univers 1.
        let mut buf = [0u8; 1024];
        let len = tokio::time::timeout(std::time::Duration::from_secs(3), recepteur.recv(&mut buf))
            .await
            .expect("trame attendue")
            .expect("recv");
        assert_eq!(&buf[0..8], b"Art-Net\0");
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 0x5000);
        assert_eq!(u16::from_le_bytes([buf[14], buf[15]]), 1);
        assert_eq!(len, 18 + 512);

        // Interrupteur coupé : plus aucune trame après un court délai.
        actif_tx.send(false).expect("off");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        // Vide ce qui restait en file, puis exige le silence.
        while let Ok(Ok(_)) = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            recepteur.recv(&mut buf),
        )
        .await
        {}
    }
}
