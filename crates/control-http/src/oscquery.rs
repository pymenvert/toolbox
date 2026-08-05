//! Serveur OSCQuery : expose le namespace OSC du node en HTTP/JSON pour
//! l'auto-découverte par Chataigne (et tout hôte compatible OSCQuery).
//!
//! - `GET /?HOST_INFO` : identité du node + port OSC ;
//! - `GET /` : l'arbre complet (chaque paramètre avec type, accès, valeur
//!   courante et bornes) ;
//! - `GET /color/gamma` : le sous-arbre ou la feuille demandée ;
//! - `GET /volume?VALUE` : un attribut isolé.
//!
//! Le namespace est reconstruit à chaque requête depuis l'état du bus : les
//! VALEURS affichées dans Chataigne sont toujours les valeurs courantes.

use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::watch;
use tracing::info;

use toolbox_core::{BusHandle, NodeState};

/// Dépendances du serveur OSCQuery.
#[derive(Clone)]
pub struct OscQueryState {
    pub bus: BusHandle,
    pub node_name: String,
    /// Port UDP où envoyer les messages OSC décrits par ce namespace.
    pub osc_port: u16,
}

/// Démarre le serveur OSCQuery ; s'arrête proprement sur le signal.
pub async fn serve(
    bind: String,
    port: u16,
    state: OscQueryState,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), super::HttpError> {
    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| super::HttpError::Bind {
            addr: addr.clone(),
            source,
        })?;
    info!(%addr, "OSCQuery démarré (Chataigne : hôte + port {port})");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .map_err(super::HttpError::Serve)
}

/// Routeur : tout chemin GET est une requête de namespace.
pub fn router(state: OscQueryState) -> Router {
    Router::new().fallback(any(query)).with_state(state)
}

async fn query(State(state): State<OscQueryState>, uri: Uri) -> impl IntoResponse {
    let attribute = uri.query().map(str::to_ascii_uppercase);
    if attribute.as_deref() == Some("HOST_INFO") {
        return (
            StatusCode::OK,
            Json(json!({
                "NAME": state.node_name,
                "OSC_IP": Value::Null,
                "OSC_PORT": state.osc_port,
                "OSC_TRANSPORT": "UDP",
                "EXTENSIONS": {
                    "ACCESS": true,
                    "VALUE": true,
                    "RANGE": true,
                    "DESCRIPTION": true,
                },
            })),
        );
    }

    let tree = namespace(&state.bus.snapshot());
    // Marche dans l'arbre : /color/gamma → CONTENTS.color.CONTENTS.gamma.
    let mut node = &tree;
    for segment in uri.path().split('/').filter(|s| !s.is_empty()) {
        match node.get("CONTENTS").and_then(|c| c.get(segment)) {
            Some(child) => node = child,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": format!("nœud inconnu : {}", uri.path()) })),
                );
            }
        }
    }
    // Attribut isolé (`?VALUE`, `?RANGE`…) ou nœud complet.
    match attribute {
        Some(attr) => match node.get(&attr) {
            Some(value) => (StatusCode::OK, Json(json!({ attr: value }))),
            None => (
                StatusCode::NO_CONTENT,
                Json(json!({ "error": "attribut absent" })),
            ),
        },
        None => (StatusCode::OK, Json(node.clone())),
    }
}

// ---------------------------------------------------------------------------
// Construction du namespace (pur, testé)
// ---------------------------------------------------------------------------

/// Feuille : paramètre écrivable, avec valeur courante et bornes éventuelles.
fn leaf(
    path: &str,
    types: &str,
    description: &str,
    value: Option<Value>,
    range: Option<Value>,
) -> Value {
    let mut node = json!({
        "FULL_PATH": path,
        "TYPE": types,
        "DESCRIPTION": description,
        // 2 = écriture seule (déclencheur), 3 = lecture/écriture.
        "ACCESS": if value.is_some() { 3 } else { 2 },
    });
    if let (Some(v), Some(obj)) = (value, node.as_object_mut()) {
        obj.insert("VALUE".into(), v);
    }
    if let (Some(r), Some(obj)) = (range, node.as_object_mut()) {
        obj.insert("RANGE".into(), r);
    }
    node
}

/// Feuille en LECTURE SEULE (`ACCESS` 1) : un état que le node publie en
/// retour OSC mais qu'on ne lui envoie pas. Sans ces nœuds, `event_to_osc`
/// émettait `/transport`, `/media`… vers des adresses que le namespace ne
/// déclarait nulle part : Chataigne recevait les messages sans avoir de
/// paramètre où les ranger, donc aucun voyant à l'écran.
fn feuille_lecture(path: &str, types: &str, description: &str, value: Value) -> Value {
    json!({
        "FULL_PATH": path,
        "TYPE": types,
        "DESCRIPTION": description,
        "ACCESS": 1,
        "VALUE": value,
    })
}

fn container(path: &str, contents: Value) -> Value {
    json!({ "FULL_PATH": path, "CONTENTS": contents })
}

fn range01() -> Value {
    json!([{ "MIN": 0.0, "MAX": 1.0 }])
}

/// L'arbre OSCQuery complet, valeurs courantes incluses.
pub fn namespace(state: &NodeState) -> Value {
    use toolbox_core::{state::color_bounds, ColorParam};

    let corners: serde_json::Map<String, Value> = (0..4usize)
        .map(|i| {
            let c = state.mapping.corners[i];
            (
                i.to_string(),
                leaf(
                    &format!("/corner/{i}"),
                    "ff",
                    "Coin du mapping (x y, 0..1, marge ±0,5)",
                    Some(json!([c.x, c.y])),
                    Some(json!([
                        { "MIN": -0.5, "MAX": 1.5 },
                        { "MIN": -0.5, "MAX": 1.5 },
                    ])),
                ),
            )
        })
        .collect();

    let color_params = [
        ("brightness", ColorParam::Brightness, state.color.brightness),
        ("contrast", ColorParam::Contrast, state.color.contrast),
        ("gamma", ColorParam::Gamma, state.color.gamma),
        ("saturation", ColorParam::Saturation, state.color.saturation),
        ("hue", ColorParam::Hue, state.color.hue),
        ("gain_r", ColorParam::GainR, state.color.gain_r),
        ("gain_g", ColorParam::GainG, state.color.gain_g),
        ("gain_b", ColorParam::GainB, state.color.gain_b),
    ];
    let colors: serde_json::Map<String, Value> = color_params
        .into_iter()
        .map(|(name, param, current)| {
            let (min, max) = color_bounds(param);
            (
                name.to_string(),
                leaf(
                    &format!("/color/{name}"),
                    "f",
                    "Correction couleur",
                    Some(json!([current])),
                    Some(json!([{ "MIN": min, "MAX": max }])),
                ),
            )
        })
        .collect();

    let crop = state.mapping.crop;
    let pattern = match state.test_pattern {
        Some(toolbox_core::TestPattern::Grid) => "grid",
        Some(toolbox_core::TestPattern::Checker) => "checker",
        Some(toolbox_core::TestPattern::Corners) => "corners",
        None => "off",
    };

    json!({
        "FULL_PATH": "/",
        "CONTENTS": {
            "play": leaf("/play", "", "Lecture", None, None),
            "pause": leaf("/pause", "", "Pause", None, None),
            "stop": leaf("/stop", "", "Stop", None, None),
            "seek": leaf("/seek", "f", "Position (secondes)", None, None),
            "load": leaf("/load", "s", "Charger une source (fichier, rtsp://, capture://N, ndi://Nom)", None, None),
            "volume": leaf("/volume", "f", "Volume", Some(json!([state.player.volume])), Some(range01())),
            "rate": leaf("/rate", "f", "Vitesse de lecture (1 = normale)",
                Some(json!([state.player.rate])), Some(json!([{ "MIN": 0.25, "MAX": 4.0 }]))),
            "lut": leaf("/lut", "s", "LUT d'étalonnage (.cube du dossier luts/, vide = retirer)",
                Some(json!([state.lut.clone().unwrap_or_default()])), None),
            "blackout": leaf("/blackout", "i", "Blackout de régie (0|1, + fondu ms optionnel)",
                Some(json!([i32::from(state.blackout.actif)])), None),
            "freeze": leaf("/freeze", "i", "Gel d'image (0|1)",
                Some(json!([i32::from(state.freeze)])), None),
            "loop": leaf("/loop", "s", "Boucle : off | one | all", Some(json!([match state.player.loop_mode {
                toolbox_core::LoopMode::Off => "off",
                toolbox_core::LoopMode::One => "one",
                toolbox_core::LoopMode::All => "all",
            }])), None),
            "playlist": container("/playlist", json!({
                "set": leaf("/playlist/set", "s", "Remplace la playlist (chemins)", None, None),
                "go": leaf("/playlist/go", "i", "Saute à l'élément", None, None),
                "next": leaf("/playlist/next", "", "Suivant", None, None),
                "prev": leaf("/playlist/prev", "", "Précédent", None, None),
                "position": feuille_lecture("/playlist/position", "is",
                    "Position courante dans la playlist (index, chemin)",
                    json!([
                        state.player.playlist_index.map_or(-1, |i| i as i64),
                        state.player.media.clone().unwrap_or_default(),
                    ])),
            })),
            // Les deux voyants les plus élémentaires d'un lecteur : ce qu'il
            // fait, et ce qu'il a chargé. Le node les émettait déjà en
            // feedback ; ils n'étaient simplement pas déclarés.
            "transport": feuille_lecture("/transport", "s",
                "Transport : stopped | playing | paused",
                json!([match state.player.transport {
                    toolbox_core::Transport::Stopped => "stopped",
                    toolbox_core::Transport::Playing => "playing",
                    toolbox_core::Transport::Paused => "paused",
                }])),
            "media": feuille_lecture("/media", "s", "Média chargé",
                json!([state.player.media.clone().unwrap_or_default()])),
            // NON déclarés volontairement : /preset/loaded, /mapping/loaded et
            // /sync/scheduled. Le feedback les émet, mais ce sont des
            // notifications sans état correspondant dans NodeState (aucun
            // champ ne retient le dernier preset chargé) : les publier
            // imposerait une VALUE perpétuellement vide, qui se lirait comme
            // « aucun preset » plutôt que « non mémorisé ».
            "corner": container("/corner", Value::Object(corners)),
            "rotation": leaf("/rotation", "i", "Rotation de la source",
                Some(json!([state.mapping.rotation.degrees()])),
                Some(json!([{ "VALS": [0, 90, 180, 270] }]))),
            "flip": leaf("/flip", "ii", "Miroirs horizontal / vertical (0|1)",
                Some(json!([i32::from(state.mapping.flip_h), i32::from(state.mapping.flip_v)])), None),
            "crop": leaf("/crop", "ffff", "Recadrage gauche haut droite bas (0..0,45)",
                Some(json!([crop.left, crop.top, crop.right, crop.bottom])),
                Some(json!([
                    { "MIN": 0.0, "MAX": 0.45 }, { "MIN": 0.0, "MAX": 0.45 },
                    { "MIN": 0.0, "MAX": 0.45 }, { "MIN": 0.0, "MAX": 0.45 },
                ]))),
            "color": container("/color", Value::Object(colors)),
            "effect": container("/effect", json!({
                "pixelate": leaf("/effect/pixelate", "f", "Pixellisation",
                    Some(json!([state.effects.pixelate])), Some(range01())),
                "posterize": leaf("/effect/posterize", "f", "Postérisation",
                    Some(json!([state.effects.posterize])), Some(range01())),
                "noise": leaf("/effect/noise", "f", "Bruit animé",
                    Some(json!([state.effects.noise])), Some(range01())),
                "sharpen": leaf("/effect/sharpen", "f", "Accentuation",
                    Some(json!([state.effects.sharpen])), Some(range01())),
                "mirror": leaf("/effect/mirror", "f", "Miroir kaléidoscope",
                    Some(json!([state.effects.mirror])), Some(range01())),
            })),
            "mapping": container("/mapping", json!({
                "reset": leaf("/mapping/reset", "", "Mapping aux valeurs neutres", None, None),
                "enabled": leaf("/mapping/enabled", "i", "Mapping actif (0|1)",
                    Some(json!([i32::from(state.mapping.enabled)])), None),
                "save": leaf("/mapping/save", "s", "Enregistre le mapping seul", None, None),
                "load": leaf("/mapping/load", "s", "Charge un mapping (sans couper la lecture)", None, None),
                "fade": leaf("/mapping/fade", "sf", "Fondu vers un mapping (nom, secondes)", None, None),
            })),
            "pattern": leaf("/pattern", "s", "Mire de test",
                Some(json!([pattern])),
                Some(json!([{ "VALS": ["grid", "checker", "corners", "off"] }]))),
            "preset": container("/preset", json!({
                "save": leaf("/preset/save", "s", "Sauvegarde l'état complet", None, None),
                "load": leaf("/preset/load", "s", "Charge un preset", None, None),
                "fade": leaf("/preset/fade", "sf", "Fondu vers un preset (nom, secondes)", None, None),
            })),
            "blending": leaf("/blending", "fffff",
                "Fondu de bords : gauche droite haut bas (0..0,5) puis gamma (1..3)",
                Some(json!([
                    state.blending.gauche, state.blending.droite,
                    state.blending.haut, state.blending.bas, state.blending.gamma,
                ])),
                Some(json!([
                    { "MIN": 0.0, "MAX": 0.5 }, { "MIN": 0.0, "MAX": 0.5 },
                    { "MIN": 0.0, "MAX": 0.5 }, { "MIN": 0.0, "MAX": 0.5 },
                    { "MIN": 1.0, "MAX": 3.0 },
                ]))),
            "sync": container("/sync", json!({
                "arm": leaf("/sync/arm", "", "Arme : média prêt, pause à 0", None, None),
                "startAt": leaf("/sync/startAt", "d", "Départ à l'heure Unix (secondes, double)", None, None),
            })),
            // Régie : le déclenchement d'un spectacle depuis Chataigne passe
            // par ces trois adresses. Elles fonctionnaient en OSC mais
            // n'étaient publiées nulle part — donc invisibles dans le module
            // OSCQuery, seul endroit où l'opérateur va les chercher.
            "cue": container("/cue", json!({
                "go": leaf("/cue/go", "s", "Déclenche une cue du séquenceur (par nom)", None, None),
            })),
            "dmx": container("/dmx", json!({
                "scene": leaf("/dmx/scene", "s", "Rappelle une scène de la console lumières", None, None),
                "chaser": leaf("/dmx/chaser", "s", "Lance un chaser (nom) ; chaîne VIDE : arrêt du chaser en cours", None, None),
                "master": leaf("/dmx/master", "i", "Grand master lumières : entier 0..255, ou flottant 0..1 (fader normalisé)",
                    None, Some(json!([{ "MIN": 0, "MAX": 255 }]))),
                "fader": leaf("/dmx/fader", "si", "Niveau d'un fader : identifiant, puis entier 0..255 ou flottant 0..1", None, None),
            })),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn test_state() -> OscQueryState {
        let bus = toolbox_core::Bus::new(4, 4);
        let state = OscQueryState {
            bus: bus.handle(),
            node_name: "vp-test".into(),
            osc_port: 9000,
        };
        tokio::spawn(bus.run());
        state
    }

    #[test]
    fn namespace_exposes_parameters_with_ranges_and_values() {
        let mut node_state = NodeState::default();
        node_state
            .apply(&toolbox_core::Command::SetVolume { volume: 0.4 })
            .expect("volume");
        let tree = namespace(&node_state);

        let volume = &tree["CONTENTS"]["volume"];
        assert_eq!(volume["FULL_PATH"], "/volume");
        assert_eq!(volume["TYPE"], "f");
        assert_eq!(volume["ACCESS"], 3);
        assert!((volume["VALUE"][0].as_f64().expect("valeur") - 0.4).abs() < 1e-6);
        assert_eq!(volume["RANGE"][0]["MAX"], 1.0);

        // Déclencheur : écriture seule, pas de valeur.
        assert_eq!(tree["CONTENTS"]["play"]["ACCESS"], 2);
        assert!(tree["CONTENTS"]["play"].get("VALUE").is_none());

        // Les huit paramètres couleur avec leurs bornes (bornes f32 : tolérance).
        let gamma = &tree["CONTENTS"]["color"]["CONTENTS"]["gamma"];
        assert!((gamma["RANGE"][0]["MIN"].as_f64().expect("borne") - 0.2).abs() < 1e-6);
        assert_eq!(
            tree["CONTENTS"]["color"]["CONTENTS"]
                .as_object()
                .expect("couleurs")
                .len(),
            8
        );
        // Synchro exposée.
        assert_eq!(tree["CONTENTS"]["sync"]["CONTENTS"]["startAt"]["TYPE"], "d");
        // Fondus exposés (nom + durée).
        assert_eq!(tree["CONTENTS"]["preset"]["CONTENTS"]["fade"]["TYPE"], "sf");
        assert_eq!(
            tree["CONTENTS"]["mapping"]["CONTENTS"]["fade"]["FULL_PATH"],
            "/mapping/fade"
        );
    }

    /// Chataigne ne connaît QUE ce que cet arbre déclare. Une adresse OSC
    /// acceptée par `control-osc` mais absente d'ici est une fonction
    /// injoignable en pratique : l'opérateur ne la voit nulle part.
    /// `/rate`, `/blending`, `/cue/go`, `/dmx/scene` et `/dmx/chaser`
    /// marchaient toutes en OSC sans figurer dans le namespace — vérifié en
    /// réel : `/rate 0.5` changeait bien la vitesse d'un node dont le module
    /// OSCQuery n'offrait aucun curseur de vitesse.
    #[test]
    fn namespace_publie_les_adresses_osc_pilotables() {
        let tree = namespace(&NodeState::default());
        let c = &tree["CONTENTS"];

        // Vitesse de lecture : écrivable, bornée comme la commande.
        assert_eq!(c["rate"]["FULL_PATH"], "/rate");
        assert_eq!(c["rate"]["ACCESS"], 3);
        assert_eq!(c["rate"]["RANGE"][0]["MIN"], 0.25);
        assert_eq!(c["rate"]["RANGE"][0]["MAX"], 4.0);

        // Fondu de bords : cinq floats, dans l'ordre exact de l'adresse OSC
        // (gauche, droite, haut, bas, gamma) — pas celui de /crop.
        assert_eq!(c["blending"]["TYPE"], "fffff");
        // Gamma stocké en f32 : tolérance, pas d'égalité stricte.
        let gamma = c["blending"]["VALUE"][4].as_f64().expect("gamma");
        assert!((gamma - 2.2).abs() < 1e-6);

        // Régie : cues et lumières, invisibles jusqu'ici.
        assert_eq!(c["cue"]["CONTENTS"]["go"]["FULL_PATH"], "/cue/go");
        assert_eq!(c["dmx"]["CONTENTS"]["scene"]["FULL_PATH"], "/dmx/scene");
        assert_eq!(c["dmx"]["CONTENTS"]["chaser"]["FULL_PATH"], "/dmx/chaser");
    }

    /// Le retour d'état émet `/transport` et `/media` : sans nœud déclaré,
    /// Chataigne reçoit les messages sans paramètre où les ranger.
    #[test]
    fn namespace_publie_letat_en_lecture_seule() {
        let mut node_state = NodeState::default();
        // `play` est refusé sans média : on charge d'abord, comme en vrai.
        node_state
            .apply(&toolbox_core::Command::Load {
                path: "clip.mp4".into(),
            })
            .expect("load");
        node_state
            .apply(&toolbox_core::Command::Play)
            .expect("play");
        let tree = namespace(&node_state);
        let c = &tree["CONTENTS"];

        // ACCESS 1 = lecture seule : Chataigne ne doit pas proposer d'écrire
        // dessus (le node n'accepte aucune commande sur ces adresses).
        assert_eq!(c["transport"]["ACCESS"], 1);
        assert_eq!(c["transport"]["VALUE"][0], "playing");
        assert_eq!(c["media"]["ACCESS"], 1);
        assert_eq!(c["playlist"]["CONTENTS"]["position"]["ACCESS"], 1);
        // Hors playlist, l'index vaut -1 et non 0 : 0 est un rang valide.
        assert_eq!(c["playlist"]["CONTENTS"]["position"]["VALUE"][0], -1);
    }

    #[tokio::test]
    async fn http_serves_host_info_tree_and_attributes() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;

        let router = router(test_state());

        let json = |resp: axum::response::Response| async {
            let bytes = resp.into_body().collect().await.expect("corps").to_bytes();
            serde_json::from_slice::<Value>(&bytes).expect("json")
        };

        // HOST_INFO : identité + port OSC.
        let resp = router
            .clone()
            .oneshot(
                Request::get("/?HOST_INFO")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        let info = json(resp).await;
        assert_eq!(info["NAME"], "vp-test");
        assert_eq!(info["OSC_PORT"], 9000);

        // Arbre complet à la racine.
        let resp = router
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).expect("req"))
            .await
            .expect("resp");
        let tree = json(resp).await;
        assert!(tree["CONTENTS"]["corner"]["CONTENTS"]["0"]["FULL_PATH"] == "/corner/0");

        // Feuille par chemin + attribut isolé.
        let resp = router
            .clone()
            .oneshot(
                Request::get("/color/gamma?VALUE")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        let value = json(resp).await;
        assert!((value["VALUE"][0].as_f64().expect("gamma") - 1.0).abs() < 1e-6);

        // Nœud inconnu : 404 propre.
        let resp = router
            .oneshot(
                Request::get("/nexiste/pas")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
