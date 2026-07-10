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

/// Annonce ce node et alimente `nodes` avec le parc découvert (JSON prêt
/// pour `/api/fleet`). Toute erreur est tracée : le node fonctionne sans
/// mDNS (réseau filtré…).
pub fn spawn(
    node_name: String,
    http_port: u16,
    version: String,
    nodes: watch::Sender<serde_json::Value>,
) {
    let thread = std::thread::Builder::new()
        .name("toolbox-fleet".into())
        .spawn(move || run(node_name, http_port, version, &nodes));
    if let Err(err) = thread {
        warn!(%err, "découverte réseau non démarrée");
    }
}

fn run(
    node_name: String,
    http_port: u16,
    version: String,
    nodes: &watch::Sender<serde_json::Value>,
) {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(err) => {
            warn!(%err, "mDNS indisponible — pas de découverte réseau");
            return;
        }
    };

    // Annonce de ce node (l'IP est résolue automatiquement).
    let instance = sanitize_instance(&node_name);
    let host = format!("{instance}.local.");
    let properties = [("version", version.as_str())];
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
                let name = service
                    .get_fullname()
                    .trim_end_matches(&format!(".{SERVICE_TYPE}"))
                    .to_string();
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
                    soi_meme: name == instance,
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
}
