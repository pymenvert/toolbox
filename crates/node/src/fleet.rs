//! Découverte réseau des nodes Toolbox (brief 3.13) : chaque node s'annonce
//! en mDNS (`_toolbox._tcp.local.`) et écoute les annonces des autres — la
//! page Système liste le parc sans configuration ni cloud.
//!
//! Tourne dans un thread dédié (mdns-sd gère ses propres sockets) ; la liste
//! découverte est publiée sur un canal `watch` consommé par l'API HTTP.

use std::collections::HashMap;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::watch;
use tracing::{info, warn};

pub const SERVICE_TYPE: &str = "_toolbox._tcp.local.";
/// Type de service standard OSCQuery : Chataigne (et les autres hôtes
/// compatibles) scannent celui-ci pour proposer les nodes sans taper d'IP.
pub const OSCQUERY_SERVICE_TYPE: &str = "_oscjson._tcp.local.";

/// Un node vu sur le réseau.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FleetNode {
    pub name: String,
    /// URL de son interface web.
    pub url: String,
    pub version: String,
    /// Ce node-ci ?
    pub soi_meme: bool,
}

/// Poignée du service : l'arrêt éteint le daemon mDNS — les annonces sont
/// retirées du réseau et le fil de découverte se termine (zéro ressource).
pub struct FleetHandle {
    daemon: ServiceDaemon,
}

impl FleetHandle {
    pub fn arreter(self) {
        if let Err(err) = self.daemon.shutdown() {
            warn!(%err, "arrêt mDNS incomplet");
        }
        info!("découverte réseau retirée (mDNS éteint)");
    }
}

/// Annonce ce node et alimente `nodes` avec le parc découvert (JSON prêt
/// pour `/api/fleet`). Toute erreur est tracée : le node fonctionne sans
/// mDNS (réseau filtré…) — `None` si le service n'a pas pu démarrer.
pub fn spawn(
    node_name: String,
    http_port: u16,
    oscquery_port: Option<u16>,
    version: String,
    nodes: watch::Sender<serde_json::Value>,
) -> Option<FleetHandle> {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(err) => {
            warn!(%err, "mDNS indisponible — pas de découverte réseau");
            return None;
        }
    };
    let daemon_thread = daemon.clone();
    let thread = std::thread::Builder::new()
        .name("toolbox-fleet".into())
        .spawn(move || {
            run(
                &daemon_thread,
                node_name,
                http_port,
                oscquery_port,
                version,
                &nodes,
            )
        });
    if let Err(err) = thread {
        warn!(%err, "découverte réseau non démarrée");
        return None;
    }
    Some(FleetHandle { daemon })
}

fn run(
    daemon: &ServiceDaemon,
    node_name: String,
    http_port: u16,
    oscquery_port: Option<u16>,
    version: String,
    nodes: &watch::Sender<serde_json::Value>,
) {
    // Annonce de ce node (l'IP est résolue automatiquement).
    // Le nom d'INSTANCE mDNS sert aussi de hostname (`.local`) : deux nodes
    // du même nom (deux Pi fraîchement imagés, tous « raspberrypi », ou deux
    // configs identiques) entreraient en conflit et deviendraient MUTUELLEMENT
    // invisibles dans le parc. On rend l'instance unique par un suffixe
    // machine, et on garde le nom lisible dans la propriété « nom » pour l'UI.
    let instance = format!("{}-{}", sanitize_instance(&node_name), suffixe_unique());
    let host = format!("{instance}.local.");
    let properties = [("version", version.as_str()), ("nom", node_name.as_str())];
    match ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &host,
        (),
        http_port,
        &properties[..],
    ) {
        Ok(info) => {
            let info = info.enable_addr_auto();
            if let Err(err) = daemon.register(info) {
                warn!(%err, "annonce mDNS refusée");
            } else {
                info!(nom = %instance, port = http_port, "node annoncé en mDNS");
            }
        }
        Err(err) => warn!(%err, "annonce mDNS invalide"),
    }

    // Annonce OSCQuery (standard `_oscjson._tcp`) : Chataigne liste le node
    // dans son module OSCQuery sans qu'on tape la moindre IP.
    if let Some(port) = oscquery_port {
        let sans_props: &[(&str, &str)] = &[];
        match ServiceInfo::new(
            OSCQUERY_SERVICE_TYPE,
            &instance,
            &host,
            (),
            port,
            sans_props,
        ) {
            Ok(info) => {
                let info = info.enable_addr_auto();
                if let Err(err) = daemon.register(info) {
                    warn!(%err, "annonce OSCQuery mDNS refusée");
                } else {
                    info!(nom = %instance, port, "serveur OSCQuery annoncé en mDNS");
                }
            }
            Err(err) => warn!(%err, "annonce OSCQuery mDNS invalide"),
        }
    }

    // Écoute du parc.
    let receiver = match daemon.browse(SERVICE_TYPE) {
        Ok(receiver) => receiver,
        Err(err) => {
            warn!(%err, "écoute mDNS impossible");
            return;
        }
    };
    let mut connus: HashMap<String, FleetNode> = HashMap::new();
    while let Ok(event) = receiver.recv() {
        match event {
            ServiceEvent::ServiceResolved(service) => {
                let instance_resolue = service
                    .get_fullname()
                    .trim_end_matches(&format!(".{SERVICE_TYPE}"))
                    .to_string();
                // Nom lisible : la propriété « nom » (sans le suffixe machine) ;
                // à défaut (vieux node), l'instance brute.
                let name = service
                    .get_property_val_str("nom")
                    .map(str::to_string)
                    .unwrap_or_else(|| instance_resolue.clone());
                // IPv4 de préférence : une URL http://fe80::… est pénible à
                // cliquer et souvent invalide sans zone id.
                let addresses = service.get_addresses();
                let Some(ip) = addresses
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .or_else(|| addresses.iter().next())
                else {
                    continue;
                };
                let node = FleetNode {
                    soi_meme: instance_resolue == instance,
                    url: format!("http://{ip}:{}/", service.get_port()),
                    version: service
                        .get_property_val_str("version")
                        .unwrap_or("?")
                        .to_string(),
                    name,
                };
                connus.insert(service.get_fullname().to_string(), node);
                publish(nodes, &connus);
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                connus.remove(&fullname);
                publish(nodes, &connus);
            }
            _ => {}
        }
    }
    // Service coupé : liste vidée pour l'UI (sinon elle montrerait un parc figé).
    let _ = nodes.send(serde_json::Value::Array(Vec::new()));
    info!("découverte réseau arrêtée");
}

fn publish(nodes: &watch::Sender<serde_json::Value>, connus: &HashMap<String, FleetNode>) {
    let mut list: Vec<FleetNode> = connus.values().cloned().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    match serde_json::to_value(&list) {
        Ok(value) => {
            nodes.send_replace(value);
        }
        Err(err) => warn!(%err, "parc non sérialisable"),
    }
}

/// Nom d'instance mDNS : ASCII simple (les annonces exotiques cassent
/// certains résolveurs).
/// Suffixe hexadécimal court identifiant CETTE machine/instance, pour rendre
/// le nom mDNS unique (le hostname par défaut « raspberrypi » est identique
/// sur tous les Pi neufs). Dérivé du hostname + PID + heure de démarrage :
/// stable pendant la vie du process, distinct d'une machine à l'autre.
fn suffixe_unique() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
        .hash(&mut h);
    std::process::id().hash(&mut h);
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        d.as_nanos().hash(&mut h);
    }
    format!("{:04x}", h.finish() & 0xffff)
}

fn sanitize_instance(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if clean.is_empty() {
        "toolbox-node".into()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_are_sanitized() {
        assert_eq!(sanitize_instance("vp-01"), "vp-01");
        assert_eq!(sanitize_instance("Régie café"), "R-gie-caf-");
        assert_eq!(sanitize_instance(""), "toolbox-node");
    }

    #[test]
    fn suffixe_unique_est_hexa_court_et_valide_en_hostname() {
        let s = suffixe_unique();
        assert_eq!(s.len(), 4, "4 caractères hexa");
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        // Combiné à un nom, l'instance reste un hostname valide (alphanum + -).
        let instance = format!("{}-{}", sanitize_instance("raspberrypi"), s);
        assert!(instance
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
