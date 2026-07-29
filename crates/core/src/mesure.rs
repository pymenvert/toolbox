//! Mesure du temps passé à produire une frame de sortie.
//!
//! Le node savait dire COMBIEN de frames il présentait (le badge img/s), mais
//! pas COMBIEN DE TEMPS chacune coûtait. Deux pannes très différentes
//! donnaient donc le même 60 img/s à l'écran : une machine tranquille à 3 ms
//! par frame, et une machine au bord du décrochage à 16 ms. Ce module
//! comble ce trou.
//!
//! **Contrainte de conception : le chemin chaud n'a droit à rien.** Pas
//! d'allocation, pas de verrou, pas de syscall en dehors de la lecture
//! d'horloge — sans quoi la mesure coûterait plus cher que ce qu'elle
//! mesure. On écrit donc dans un anneau de taille fixe, et le résumé
//! (médiane, p95, max) n'est calculé qu'au moment de le publier, environ une
//! fois par seconde.

use serde::Serialize;

/// Échantillons gardés : 256 frames, soit ~4 s à 60 img/s. Confortablement
/// plus que la fenêtre de publication (1 s), donc rien n'est écrasé avant
/// d'avoir été résumé, même si la sortie s'emballe.
const ECHANTILLONS: usize = 256;

/// Résumé publié pour l'UI et `/api/system`.
///
/// Les percentiles décrivent la **dernière seconde** (ce que l'opérateur voit
/// à l'instant T) ; `sautees` est un **cumul depuis le démarrage** (une frame
/// ratée reste un incident, même passée).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct RenduMesures {
    /// Frame médiane, en microsecondes : le régime de croisière.
    pub p50_us: u32,
    /// 95ᵉ centile : ce que subissent les 5 % de frames les plus lentes.
    ///
    /// Il révèle une gêne **soutenue** — une machine qui décroche une frame
    /// sur dix. Un pic ISOLÉ (une frame sur cent) ne le fait pas bouger d'une
    /// microseconde : c'est `max_us` qui le voit. Les deux sont donc
    /// complémentaires, et lire l'un sans l'autre induit en erreur.
    pub p95_us: u32,
    /// Pire frame de la fenêtre — le seul indicateur qui attrape un à-coup
    /// isolé.
    pub max_us: u32,
    /// Frames non présentées depuis le démarrage (surface occupée, délai
    /// dépassé, device perdu). En croissance continue = quelque chose ne va
    /// pas.
    pub sautees: u64,
    /// Frames perdues sur la **dernière seconde** seulement.
    ///
    /// Le cumul ci-dessus ne redescend jamais : quelques frames ratées au
    /// démarrage (le temps que la surface se configure) suffisaient à
    /// allumer une alerte pour le restant de la session. C'est ce compteur-ci
    /// qui dit si ça se produit ENCORE.
    pub sautees_fenetre: u32,
    /// Images réellement mesurées dans la fenêtre de publication.
    ///
    /// Sans ce chiffre, impossible de savoir ce que vaut le p95 : sur trois
    /// échantillons, il EST le maximum. Une sortie au ralenti (le node ne
    /// repeint que sur changement d'état) produit donc un « p95 » qui n'a
    /// aucune valeur statistique, et rien ne le disait.
    pub echantillons: u32,
}

/// Accumulateur de durées de frame. Un seul thread écrit (celui de la
/// fenêtre), donc pas de synchronisation : c'est un simple champ de l'état de
/// la boucle de rendu.
#[derive(Debug)]
pub struct Chrono {
    echantillons: [u32; ECHANTILLONS],
    /// Écritures depuis le dernier résumé. Sert d'index d'anneau ET de compte
    /// d'échantillons valides.
    ecrits: usize,
    sautees: u64,
    sautees_fenetre: u32,
}

impl Default for Chrono {
    fn default() -> Self {
        Self::new()
    }
}

impl Chrono {
    #[must_use]
    pub fn new() -> Self {
        Self {
            echantillons: [0; ECHANTILLONS],
            ecrits: 0,
            sautees: 0,
            sautees_fenetre: 0,
        }
    }

    /// Enregistre une frame présentée. Coût : un modulo et une écriture.
    pub fn ajouter(&mut self, duree: std::time::Duration) {
        // Bornage explicite avant la conversion : une frame de plus de
        // ~71 minutes n'existe pas, mais on ne veut pas d'un repli à zéro si
        // l'horloge fait un bond (veille, changement d'heure système).
        #[allow(clippy::cast_possible_truncation)] // borné juste au-dessus
        let micros = duree.as_micros().min(u128::from(u32::MAX)) as u32;
        self.echantillons[self.ecrits % ECHANTILLONS] = micros;
        self.ecrits = self.ecrits.saturating_add(1);
    }

    /// Enregistre une frame perdue (rien n'a été présenté).
    pub fn sautee(&mut self) {
        self.sautees = self.sautees.saturating_add(1);
        self.sautees_fenetre = self.sautees_fenetre.saturating_add(1);
    }

    /// Nombre d'échantillons en attente de résumé.
    #[must_use]
    pub fn en_attente(&self) -> usize {
        self.ecrits.min(ECHANTILLONS)
    }

    /// Cumul de frames perdues, sans consommer la fenêtre.
    ///
    /// Sert à l'appelant pour savoir s'il a du neuf à publier : la seule
    /// publication périodique se faisait sur une frame RÉUSSIE, donc une
    /// sortie qui n'affiche plus rien du tout n'envoyait plus jamais son
    /// compteur — exactement la panne que ce compteur doit révéler.
    #[must_use]
    pub fn sautees(&self) -> u64 {
        self.sautees
    }

    /// Calcule le résumé et **repart sur une fenêtre vide**. À appeler à la
    /// cadence de publication, pas à chaque frame : le tri coûte plus cher
    /// qu'une frame.
    ///
    /// Sans aucun échantillon, les percentiles valent 0 — l'UI affiche alors
    /// un tiret plutôt qu'un faux « 0 ms ».
    pub fn resumer(&mut self) -> RenduMesures {
        let n = self.en_attente();
        self.ecrits = 0;
        let fenetre = std::mem::take(&mut self.sautees_fenetre);
        if n == 0 {
            return RenduMesures {
                sautees: self.sautees,
                sautees_fenetre: fenetre,
                ..RenduMesures::default()
            };
        }
        #[allow(clippy::cast_possible_truncation)] // n <= ECHANTILLONS = 256
        let echantillons = n as u32;
        // Copie sur la pile (1 Ko) puis tri en place : aucune allocation.
        let mut tri = [0u32; ECHANTILLONS];
        tri[..n].copy_from_slice(&self.echantillons[..n]);
        tri[..n].sort_unstable();
        RenduMesures {
            p50_us: tri[centile(n, 50)],
            p95_us: tri[centile(n, 95)],
            max_us: tri[n - 1],
            sautees: self.sautees,
            sautees_fenetre: fenetre,
            echantillons,
        }
    }
}

/// Index du centile `p` dans un tableau trié de `n` éléments (`n >= 1`).
///
/// Méthode du **rang le plus proche** : `ceil(n × p / 100) - 1`. La version
/// naïve `floor(n × p / 100)` décale d'un cran chaque fois que `n × p` est
/// multiple de 100 — donc, pour p = 50, à TOUT n pair. À n = 2, l'effet est
/// maximal : elle renvoyait la PIRE des deux frames comme « médiane ». Ce
/// n'est pas un cas d'école : au repos, la fenêtre ne repeint que sur
/// changement d'état, donc une seconde de publication peut ne contenir que
/// deux frames.
fn centile(n: usize, p: usize) -> usize {
    let rang = n.saturating_mul(p).div_ceil(100).max(1);
    rang.min(n) - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn chrono_de(micros: &[u32]) -> Chrono {
        let mut c = Chrono::new();
        for &m in micros {
            c.ajouter(Duration::from_micros(u64::from(m)));
        }
        c
    }

    #[test]
    fn un_chrono_vide_ne_ment_pas() {
        let mut c = Chrono::new();
        assert_eq!(c.resumer(), RenduMesures::default());
    }

    #[test]
    fn les_percentiles_sortent_les_bonnes_valeurs() {
        // 100 frames : 1 ms … 100 ms.
        let mut c = Chrono::new();
        for i in 1..=100u32 {
            c.ajouter(Duration::from_micros(u64::from(i) * 1000));
        }
        let r = c.resumer();
        // Rang le plus proche : la 50ᵉ des 100 valeurs, pas la 51ᵉ.
        assert_eq!(r.p50_us, 50_000, "médiane");
        assert_eq!(r.p95_us, 95_000, "p95");
        assert_eq!(r.max_us, 100_000, "max");
        assert_eq!(r.echantillons, 100, "le lecteur doit savoir sur combien");
    }

    /// Le cas où l'ancienne formule se trompait le plus : deux frames.
    /// Elle publiait la pire des deux comme « habituel », et l'UI affichait
    /// « 100 ms · trop lent pour du 60 Hz » alors qu'une des deux frames
    /// avait coûté 1 ms.
    #[test]
    fn la_mediane_de_deux_frames_est_la_plus_rapide_pas_la_pire() {
        let mut c = chrono_de(&[1_000, 100_000]);
        let r = c.resumer();
        assert_eq!(r.p50_us, 1_000, "médiane de deux frames");
        assert_eq!(r.max_us, 100_000, "le max reste le max");
        // Sur deux images, le p95 EST le max : le chiffre d'échantillons est
        // la seule chose qui permette de ne pas s'y tromper.
        assert_eq!(r.echantillons, 2);
        assert_eq!(r.p95_us, r.max_us);
    }

    /// Un pic ISOLÉ (1 frame sur 100) ne bouge ni la médiane ni le p95 : seul
    /// le max le voit. C'est une propriété du p95, pas un défaut — mais elle
    /// se paie d'un contresens si on lit le p95 tout seul, d'où ce test qui
    /// la fige.
    #[test]
    fn un_a_coup_isole_ne_bouge_que_le_max() {
        let mut micros = vec![4_000u32; 99];
        micros.push(200_000);
        let mut c = chrono_de(&micros);
        let r = c.resumer();
        assert_eq!(r.p50_us, 4_000, "la médiane reste sereine");
        assert_eq!(r.p95_us, 4_000, "un pic sur cent ne remplit pas les 5 %");
        assert_eq!(r.max_us, 200_000, "le max voit l'à-coup");
    }

    /// La gêne SOUTENUE — une frame sur dix qui décroche — est en revanche
    /// exactement ce que le p95 doit révéler, pendant que la médiane continue
    /// d'afficher un régime tranquille. C'est le scénario qui fait saccader
    /// une projection sans faire bouger le badge img/s.
    ///
    /// La version précédente de ce test s'appelait « le p95 attrape l'à-coup »
    /// mais n'assertait JAMAIS le p95 : un p95 calculé par erreur comme la
    /// médiane passait au travers. Vérifié par injection de faute.
    #[test]
    fn une_gene_soutenue_fait_monter_le_p95_sans_la_mediane() {
        let mut micros = vec![4_000u32; 90];
        micros.extend_from_slice(&[20_000u32; 10]);
        let mut c = chrono_de(&micros);
        let r = c.resumer();
        assert_eq!(r.p50_us, 4_000, "la médiane ne voit rien");
        assert_eq!(r.p95_us, 20_000, "le p95 sort la gêne soutenue");
        assert!(r.p95_us > r.p50_us, "p95 confondu avec la médiane");
    }

    #[test]
    fn resumer_repart_sur_une_fenetre_vide() {
        let mut c = chrono_de(&[10_000, 20_000]);
        assert_eq!(c.resumer().max_us, 20_000);
        // Deuxième appel sans nouvelle frame : plus de percentiles.
        let r = c.resumer();
        assert_eq!(r.max_us, 0);
        assert_eq!(r.p50_us, 0);
    }

    #[test]
    fn l_anneau_ne_deborde_pas_et_garde_des_valeurs_plausibles() {
        // Trois fois la capacité de l'anneau : on ne doit ni paniquer, ni
        // résumer plus d'échantillons qu'il n'en tient.
        let mut c = Chrono::new();
        for i in 0..(ECHANTILLONS * 3) {
            c.ajouter(Duration::from_micros(i as u64));
        }
        assert_eq!(c.en_attente(), ECHANTILLONS);
        let r = c.resumer();
        // ATTENTES EXACTES, pas des tautologies. Les anciennes assertions
        // (`max > 0`, `p50 <= p95 <= max`) sont vraies de TOUT tableau trié :
        // une implémentation qui garderait les 256 PLUS ANCIENS échantillons
        // — l'exact contraire d'un anneau — les satisfaisait aussi. Vérifié
        // par injection de faute.
        //
        // Écrit : 0..767 µs. Un anneau de 256 ne doit garder que 512..767.
        let plus_ancien = (ECHANTILLONS * 2) as u32; // 512
        assert_eq!(
            r.max_us,
            (ECHANTILLONS * 3 - 1) as u32,
            "la dernière frame écrite doit être le max"
        );
        assert_eq!(r.p50_us, plus_ancien + 127, "médiane des 256 plus RÉCENTS");
        assert_eq!(r.p95_us, plus_ancien + 243, "p95 des 256 plus RÉCENTS");
    }

    #[test]
    fn les_frames_sautees_sont_cumulatives() {
        let mut c = Chrono::new();
        c.sautee();
        c.sautee();
        let r = c.resumer();
        assert_eq!(r.sautees, 2);
        assert_eq!(r.sautees_fenetre, 2, "les deux sont de la dernière seconde");
        c.sautee();
        let r = c.resumer();
        // Le cumul survit au résumé, contrairement aux percentiles…
        assert_eq!(r.sautees, 3);
        // …mais le compteur de fenêtre repart, sinon une poignée de frames
        // ratées au démarrage allumerait une alerte pour toute la session.
        assert_eq!(r.sautees_fenetre, 1, "seulement la dernière");
        assert_eq!(c.resumer().sautees_fenetre, 0, "plus rien ne se perd");
    }

    #[test]
    fn une_duree_absurde_ne_deborde_pas() {
        // Bond d'horloge (sortie de veille) : on sature, on ne replie pas.
        let mut c = Chrono::new();
        c.ajouter(Duration::from_secs(u64::from(u32::MAX)));
        assert_eq!(c.resumer().max_us, u32::MAX);
    }
}
