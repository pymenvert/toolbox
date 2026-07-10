//! # toolbox-control-http (P1.5)
//!
//! Le point de contrôle réseau du node : REST + WebSocket + web UI embarquée.
//!
//! - `GET /` : la web UI (un seul fichier HTML embarqué dans le binaire —
//!   compatible portable, rien à déployer) ;
//! - `POST /api/command` : LE point d'entrée des commandes (même vocabulaire
//!   JSON que le WebSocket et les autres interfaces) ;
//! - `GET /ws` : événements en direct (état + position de lecture) ;
//! - `GET /ws/logs` : page de logs en direct ;
//! - médiathèque : liste, upload (streaming, borné), renommage, suppression ;
//! - presets : liste, suppression (save/load passent par des commandes) —
//!   idem pour les presets de mapping (`/api/mapping-presets`) ;
//! - `GET /api/system` : monitoring (CPU, mémoire, température).
//!
//! Sécurité V1 : réseau local de confiance (pas d'authentification — P4.4
//! ajoutera mot de passe + token). Tout ce qui touche au disque revalide les
//! noms/chemins côté core.

pub mod monitor;
pub mod oscquery;

use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Request, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

use toolbox_core::media::validate_upload_name;
use toolbox_core::state::Event;
use toolbox_core::{
    BusHandle, Command, CoreError, LogBuffer, MappingStore, MediaInfo, MediaLibrary, MonitorInfo,
    OutputSettings, PresetStore, Source,
};
use toolbox_engine::PlaybackPosition;

/// La page web embarquée (assets/index.html, un seul fichier).
const INDEX_HTML: &str = include_str!("../assets/index.html");

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("impossible d'écouter sur {addr} : {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("serveur HTTP arrêté sur erreur : {0}")]
    Serve(std::io::Error),
}

/// Configuration du serveur.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub bind: String,
    pub port: u16,
    pub node_name: String,
    pub version: String,
}

/// Contrôle de la fenêtre de sortie : écrans détectés (publiés par la
/// fenêtre) et réglages appliqués à chaud (écran cible, plein écran).
/// Sans module de rendu, la liste reste vide et les réglages sont inertes.
#[derive(Clone)]
pub struct OutputControl {
    pub monitors: watch::Receiver<Vec<MonitorInfo>>,
    pub settings: std::sync::Arc<watch::Sender<OutputSettings>>,
    /// Frames par seconde réellement présentées par la fenêtre de sortie.
    pub fps: watch::Receiver<f32>,
}

impl OutputControl {
    /// Contrôle inerte (module de rendu absent) : aucun écran, réglages sans
    /// destinataire. Utilisé aussi par les tests.
    pub fn disconnected() -> Self {
        let (_, monitors) = watch::channel(Vec::new());
        let (settings, _) = watch::channel(OutputSettings::default());
        let (_, fps) = watch::channel(0.0);
        Self {
            monitors,
            settings: std::sync::Arc::new(settings),
            fps,
        }
    }
}

/// Dépendances partagées par tous les handlers.
#[derive(Clone)]
pub struct AppState {
    bus: BusHandle,
    presets: PresetStore,
    mapping_presets: MappingStore,
    media: MediaLibrary,
    logs: LogBuffer,
    position: watch::Receiver<PlaybackPosition>,
    /// Signal d'arrêt : les WebSockets ouverts se ferment dessus, sinon le
    /// graceful shutdown d'axum attendrait indéfiniment une UI affichée.
    shutdown: watch::Receiver<bool>,
    output: OutputControl,
    started_at: Instant,
    node_name: String,
    version: String,
}

impl AppState {
    #[allow(clippy::too_many_arguments)] // constructeur d'assemblage, appelé une fois.
    pub fn new(
        bus: BusHandle,
        presets: PresetStore,
        mapping_presets: MappingStore,
        media: MediaLibrary,
        logs: LogBuffer,
        position: watch::Receiver<PlaybackPosition>,
        shutdown: watch::Receiver<bool>,
        output: OutputControl,
        node_name: String,
        version: String,
    ) -> Self {
        Self {
            bus,
            presets,
            mapping_presets,
            media,
            logs,
            position,
            shutdown,
            output,
            started_at: Instant::now(),
            node_name,
            version,
        }
    }
}

/// Erreur d'API : convertit les [`CoreError`] en réponses HTTP propres.
struct ApiError(CoreError);

impl From<CoreError> for ApiError {
    fn from(err: CoreError) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            CoreError::PresetNotFound(_) | CoreError::MediaNotFound(_) => StatusCode::NOT_FOUND,
            CoreError::MediaTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            CoreError::MediaAlreadyExists(_) => StatusCode::CONFLICT,
            CoreError::Io { .. } | CoreError::Serde(_) | CoreError::Config(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            _ => StatusCode::BAD_REQUEST,
        };
        warn!(error = %self.0, %status, "erreur API");
        (
            status,
            Json(serde_json::json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}

/// Construit le routeur complet (séparé de [`serve`] pour les tests).
pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/state", get(get_state))
        .route("/api/position", get(get_position))
        .route("/api/command", post(post_command))
        .route("/api/presets", get(presets_list))
        .route("/api/presets/{name}", delete(preset_delete))
        .route("/api/mapping-presets", get(mapping_presets_list))
        .route("/api/mapping-presets/{name}", delete(mapping_preset_delete))
        .route("/api/media", get(media_list))
        // Un seul motif pour PUT et DELETE : deux motifs différents sur le
        // même segment ({name} et {*path}) seraient un conflit de routage.
        // L'upload revalide de toute façon que le nom est plat.
        .route("/api/media/{*path}", put(media_upload).delete(media_delete))
        .route("/api/media-rename", post(media_rename))
        .route("/api/logs", get(logs_snapshot))
        .route("/api/system", get(system_stats))
        .route("/api/outputs", get(outputs_get))
        .route("/api/output", post(output_set))
        .route("/ws", get(ws_events_upgrade))
        .route("/ws/logs", get(ws_logs_upgrade))
        .with_state(app)
}

/// Démarre le serveur ; s'arrête proprement quand `shutdown` change.
pub async fn serve(
    config: HttpConfig,
    app: AppState,
    shutdown: watch::Receiver<bool>,
) -> Result<(), HttpError> {
    let addr = format!("{}:{}", config.bind, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| HttpError::Bind {
            addr: addr.clone(),
            source,
        })?;
    info!(%addr, "HTTP démarré — web UI : http://<ip-du-node>:{}/", config.port);
    serve_on(listener, app, shutdown).await
}

/// Sert sur un listener déjà lié (séparé de [`serve`] pour tester l'arrêt
/// propre sur un port éphémère).
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    app: AppState,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), HttpError> {
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
            info!("arrêt du serveur HTTP demandé");
        })
        .await
        .map_err(HttpError::Serve)
}

// ---------------------------------------------------------------------------
// Handlers REST
// ---------------------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health(State(app): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "name": app.node_name,
        "version": app.version,
        "uptime_s": app.started_at.elapsed().as_secs(),
    }))
}

async fn get_state(State(app): State<AppState>) -> Json<toolbox_core::NodeState> {
    Json(app.bus.snapshot())
}

async fn get_position(State(app): State<AppState>) -> Json<PlaybackPosition> {
    Json(*app.position.borrow())
}

/// Envoie une commande sur le bus. `202 Accepted` : la validation définitive
/// est faite par le bus (le refus éventuel est visible dans les logs et
/// l'absence d'événement) — même contrat que l'OSC.
async fn post_command(
    State(app): State<AppState>,
    Json(command): Json<Command>,
) -> impl IntoResponse {
    if app.bus.send(Source::Http, command).await {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"accepted": true})),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"accepted": false, "error": "bus arrêté"})),
        )
    }
}

async fn presets_list(State(app): State<AppState>) -> Result<Json<Vec<String>>, ApiError> {
    Ok(Json(app.presets.list()?))
}

async fn preset_delete(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.presets.delete(&name)?;
    info!(preset = %name, "preset supprimé via l'API");
    Ok(StatusCode::NO_CONTENT)
}

async fn mapping_presets_list(State(app): State<AppState>) -> Result<Json<Vec<String>>, ApiError> {
    Ok(Json(app.mapping_presets.list()?))
}

async fn mapping_preset_delete(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.mapping_presets.delete(&name)?;
    info!(mapping = %name, "preset de mapping supprimé via l'API");
    Ok(StatusCode::NO_CONTENT)
}

async fn media_list(State(app): State<AppState>) -> Result<Json<Vec<MediaInfo>>, ApiError> {
    Ok(Json(app.media.list()?))
}

/// Upload en streaming : le fichier n'est jamais entièrement en RAM
/// (indispensable sur Pi), taille bornée, écriture atomique.
async fn media_upload(
    State(app): State<AppState>,
    Path(name): Path<String>,
    request: Request,
) -> Result<(StatusCode, Json<MediaInfo>), ApiError> {
    validate_upload_name(&name)?;
    let root = app.media.root().to_path_buf();
    let tmp_path = root.join(format!(".{name}.upload.tmp"));
    let final_path = root.join(&name);
    let max = app.media.max_upload_bytes();

    let mut stream = request.into_body().into_data_stream();
    let mut written: u64 = 0;
    let result: Result<(), ApiError> = async {
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| CoreError::InvalidCommand(format!("upload interrompu : {e}")))?;
            written += chunk.len() as u64;
            if written > max {
                return Err(CoreError::MediaTooLarge {
                    name: name.clone(),
                    max,
                }
                .into());
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
        }
        file.sync_all()
            .await
            .map_err(|e| CoreError::io(tmp_path.display().to_string(), e))?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            tokio::fs::rename(&tmp_path, &final_path)
                .await
                .map_err(|e| CoreError::io(final_path.display().to_string(), e))?;
            info!(media = %name, bytes = written, "média déposé");
            Ok((
                StatusCode::CREATED,
                Json(MediaInfo {
                    path: name,
                    bytes: written,
                }),
            ))
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(err)
        }
    }
}

async fn media_delete(
    State(app): State<AppState>,
    Path(path): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.media.delete(&path)?;
    info!(media = %path, "média supprimé via l'API");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct RenameRequest {
    from: String,
    to: String,
}

async fn media_rename(
    State(app): State<AppState>,
    Json(request): Json<RenameRequest>,
) -> Result<StatusCode, ApiError> {
    app.media.rename(&request.from, &request.to)?;
    info!(from = %request.from, to = %request.to, "média renommé via l'API");
    Ok(StatusCode::NO_CONTENT)
}

async fn logs_snapshot(State(app): State<AppState>) -> Json<Vec<toolbox_core::LogEntry>> {
    Json(app.logs.snapshot())
}

async fn system_stats(State(app): State<AppState>) -> Json<monitor::SystemStats> {
    Json(monitor::collect(app.started_at))
}

/// Écrans détectés + réglages courants de la fenêtre de sortie.
async fn outputs_get(State(app): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "monitors": *app.output.monitors.borrow(),
        "settings": *app.output.settings.borrow(),
        "fps": *app.output.fps.borrow(),
    }))
}

/// Applique des réglages de sortie à chaud (écran cible, plein écran).
/// L'index d'écran est validé contre la liste détectée.
async fn output_set(
    State(app): State<AppState>,
    Json(settings): Json<OutputSettings>,
) -> Result<StatusCode, ApiError> {
    let monitors = app.output.monitors.borrow().len();
    if monitors > 0 && settings.monitor >= monitors {
        return Err(CoreError::InvalidCommand(format!(
            "écran {} inconnu ({} détecté(s))",
            settings.monitor, monitors
        ))
        .into());
    }
    app.output.settings.send_replace(settings);
    info!(
        ecran = settings.monitor,
        plein_ecran = settings.fullscreen,
        "réglages de sortie appliqués via l'API"
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// WebSockets
// ---------------------------------------------------------------------------

async fn ws_events_upgrade(ws: WebSocketUpgrade, State(app): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| ws_events(socket, app))
}

/// Période du ping serveur : détecte les clients disparus sans FIN (tablette
/// sortie du WiFi…) au lieu de garder la tâche vivante jusqu'au timeout TCP.
const WS_PING_PERIOD: std::time::Duration = std::time::Duration::from_secs(20);

/// Canal principal de l'UI : état initial complet, puis chaque événement du
/// bus, plus la position de lecture. Accepte aussi des commandes entrantes
/// (JSON identique à `POST /api/command`).
async fn ws_events(mut socket: WebSocket, app: AppState) {
    let mut events = app.bus.subscribe();
    let mut position = app.position.clone();
    let mut position_alive = true;
    let mut shutdown = app.shutdown.clone();
    let mut ping = tokio::time::interval(WS_PING_PERIOD);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.reset(); // pas de ping immédiat à la connexion

    // État initial : l'UI se peint entièrement dès la connexion.
    let snapshot = Event::StateReplaced {
        state: Box::new(app.bus.snapshot()),
    };
    if send_json(&mut socket, &snapshot).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            received = events.recv() => match received {
                Ok(event) => {
                    if send_json(&mut socket, &event).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(missed, "client WS en retard : renvoi de l'état complet");
                    let snapshot = Event::StateReplaced {
                        state: Box::new(app.bus.snapshot()),
                    };
                    if send_json(&mut socket, &snapshot).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            changed = position.changed(), if position_alive => match changed {
                Ok(()) => {
                    let p = *position.borrow_and_update();
                    let message = serde_json::json!({
                        "event": "position",
                        "position": p.position,
                        "duration": p.duration,
                        // Fluidité de la fenêtre de sortie (0 = pas de rendu).
                        "fps": *app.output.fps.borrow(),
                    });
                    if socket.send(Message::Text(message.to_string().into())).await.is_err() {
                        break;
                    }
                }
                Err(_) => {
                    // Émetteur de position disparu (player arrêté) : on
                    // continue sans position plutôt que de boucler.
                    position_alive = false;
                }
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<Command>(text.as_str()) {
                        Ok(command) => {
                            if !app.bus.send(Source::WebSocket, command).await {
                                break;
                            }
                        }
                        Err(err) => {
                            let reply = serde_json::json!({
                                "error": format!("commande invalide : {err}"),
                            });
                            if socket.send(Message::Text(reply.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // ping/pong gérés par axum
                Some(Err(_)) => break,
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            },
            // Arrêt du node : on ferme, sinon le graceful shutdown attendrait
            // la fermeture spontanée de chaque UI ouverte.
            _ = shutdown.changed() => break,
        }
    }
}

async fn ws_logs_upgrade(ws: WebSocketUpgrade, State(app): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| ws_logs(socket, app))
}

/// Page de logs : l'historique du ring buffer, puis le direct.
async fn ws_logs(mut socket: WebSocket, app: AppState) {
    let mut live = app.logs.subscribe();
    let mut shutdown = app.shutdown.clone();
    let mut ping = tokio::time::interval(WS_PING_PERIOD);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.reset();
    for entry in app.logs.snapshot() {
        if send_json(&mut socket, &entry).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            received = live.recv() => match received {
                Ok(entry) => {
                    if send_json(&mut socket, &entry).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(missed, "client WS logs en retard");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            },
            _ = shutdown.changed() => break,
        }
    }
}

async fn send_json<T: serde::Serialize>(
    socket: &mut WebSocket,
    value: &T,
) -> Result<(), axum::Error> {
    match serde_json::to_string(value) {
        Ok(text) => socket.send(Message::Text(text.into())).await,
        Err(err) => {
            // Ne devrait jamais arriver (types maîtrisés) ; tracé, pas fatal.
            warn!(%err, "sérialisation WS impossible");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (routeur complet, sans réseau)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use toolbox_core::Bus;
    use tower::ServiceExt;

    struct TestBed {
        router: Router,
        _dir: tempfile::TempDir,
    }

    /// Monte un node complet en mémoire : bus qui tourne, presets et
    /// médiathèque sur un dossier temporaire.
    fn testbed() -> TestBed {
        let dir = tempfile::tempdir().expect("tempdir");
        let presets = PresetStore::open(dir.path().join("presets")).expect("presets");
        let mapping_presets =
            MappingStore::open(dir.path().join("presets").join("mapping")).expect("mappings");
        let media = MediaLibrary::open(dir.path().join("media"), 1024 * 1024).expect("media");
        let logs = LogBuffer::new(64);
        let bus = Bus::new(64, 64)
            .with_presets(presets.clone())
            .with_mapping_presets(mapping_presets.clone());
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (_position_tx, position_rx) = watch::channel(PlaybackPosition::default());
        // On garde l'émetteur vivant via une tâche, sinon la branche position
        // du WS se désactive (comportement testé par ailleurs).
        tokio::spawn(async move {
            let _keep = _position_tx;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            let _keep = _shutdown_tx;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        let app = AppState::new(
            handle,
            presets,
            mapping_presets,
            media,
            logs,
            position_rx,
            shutdown_rx,
            OutputControl::disconnected(),
            "test-node".into(),
            "0.0.0-test".into(),
        );
        TestBed {
            router: router(app),
            _dir: dir,
        }
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn health_answers() {
        let bed = testbed();
        let response = bed
            .router
            .oneshot(
                HttpRequest::get("/api/health")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["status"], "ok");
        assert_eq!(json["name"], "test-node");
    }

    #[tokio::test]
    async fn index_serves_embedded_ui() {
        let bed = testbed();
        let response = bed
            .router
            .oneshot(HttpRequest::get("/").body(Body::empty()).expect("req"))
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn command_changes_state() {
        let bed = testbed();
        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::post("/api/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"cmd":"set_volume","volume":0.42}"#))
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Le bus traite la commande de façon asynchrone.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let response = bed
            .router
            .oneshot(
                HttpRequest::get("/api/state")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        let json = body_json(response).await;
        assert_eq!(json["player"]["volume"], 0.42);
    }

    #[tokio::test]
    async fn malformed_command_is_rejected() {
        let bed = testbed();
        let response = bed
            .router
            .oneshot(
                HttpRequest::post("/api/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"cmd":"self_destruct"}"#))
                    .expect("req"),
            )
            .await
            .expect("resp");
        // axum::Json refuse le JSON invalide avant le handler.
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn media_upload_list_delete_roundtrip() {
        let bed = testbed();
        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::put("/api/media/clip.mp4")
                    .body(Body::from(&b"0123456789"[..]))
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::CREATED);
        let json = body_json(response).await;
        assert_eq!(json["bytes"], 10);

        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::get("/api/media")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        let json = body_json(response).await;
        assert_eq!(json[0]["path"], "clip.mp4");

        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::delete("/api/media/clip.mp4")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn dangerous_upload_names_are_rejected() {
        let bed = testbed();
        for (uri, expected) in [
            ("/api/media/evil.exe", StatusCode::BAD_REQUEST),
            ("/api/media/..%2Fevil.mp4", StatusCode::BAD_REQUEST),
        ] {
            let response = bed
                .router
                .clone()
                .oneshot(HttpRequest::put(uri).body(Body::from("x")).expect("req"))
                .await
                .expect("resp");
            assert_eq!(response.status(), expected, "uri: {uri}");
        }
    }

    #[tokio::test]
    async fn upload_too_large_is_refused_and_cleaned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let presets = PresetStore::open(dir.path().join("presets")).expect("presets");
        let mapping_presets =
            MappingStore::open(dir.path().join("presets").join("mapping")).expect("mappings");
        let media = MediaLibrary::open(dir.path().join("media"), 8).expect("media");
        let logs = LogBuffer::new(8);
        let bus = Bus::new(8, 8);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (_ptx, prx) = watch::channel(PlaybackPosition::default());
        let (_stx, srx) = watch::channel(false);
        let app = AppState::new(
            handle,
            presets,
            mapping_presets,
            media.clone(),
            logs,
            prx,
            srx,
            OutputControl::disconnected(),
            "t".into(),
            "0".into(),
        );
        let response = router(app)
            .oneshot(
                HttpRequest::put("/api/media/gros.mp4")
                    .body(Body::from(vec![0u8; 64]))
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(media.list().expect("list").is_empty());
    }

    #[tokio::test]
    async fn preset_api_lists_and_deletes() {
        let bed = testbed();
        // Sauvegarde via commande (comme l'UI le ferait).
        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::post("/api/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"cmd":"preset_save","name":"scene"}"#))
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::get("/api/presets")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        let json = body_json(response).await;
        assert_eq!(json[0], "scene");

        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::delete("/api/presets/scene")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = bed
            .router
            .oneshot(
                HttpRequest::delete("/api/presets/scene")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn mapping_preset_api_lists_and_deletes() {
        let bed = testbed();
        // Sauvegarde du mapping seul via commande (comme l'UI le ferait).
        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::post("/api/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"cmd":"mapping_save","name":"salon"}"#))
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Listé dans le dépôt de mapping…
        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::get("/api/mapping-presets")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        let json = body_json(response).await;
        assert_eq!(json[0], "salon");

        // …et PAS dans les presets d'état complet (dépôts séparés).
        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::get("/api/presets")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        let json = body_json(response).await;
        assert_eq!(json.as_array().map(Vec::len), Some(0));

        let response = bed
            .router
            .clone()
            .oneshot(
                HttpRequest::delete("/api/mapping-presets/salon")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = bed
            .router
            .oneshot(
                HttpRequest::delete("/api/mapping-presets/salon")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn system_stats_respond() {
        let bed = testbed();
        let response = bed
            .router
            .oneshot(
                HttpRequest::get("/api/system")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert!(json["os"].is_string());
    }

    /// L'API de sortie liste les écrans publiés par la fenêtre et applique
    /// les réglages via le canal watch (index validé contre la liste).
    #[tokio::test]
    async fn output_api_lists_monitors_and_applies_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let presets = PresetStore::open(dir.path().join("presets")).expect("presets");
        let mapping_presets =
            MappingStore::open(dir.path().join("presets").join("mapping")).expect("mappings");
        let media = MediaLibrary::open(dir.path().join("media"), 1024).expect("media");
        let logs = LogBuffer::new(16);
        let bus = Bus::new(16, 16);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (_ptx, prx) = watch::channel(PlaybackPosition::default());
        let (_stx, srx) = watch::channel(false);
        // Deux écrans « publiés par la fenêtre ».
        let (_monitors_tx, monitors_rx) = watch::channel(vec![
            MonitorInfo {
                index: 0,
                name: "principal".into(),
                width: 2560,
                height: 1600,
            },
            MonitorInfo {
                index: 1,
                name: "VP".into(),
                width: 1920,
                height: 1080,
            },
        ]);
        let (settings_tx, mut settings_rx) = watch::channel(OutputSettings::default());
        let (_fps_tx, fps_rx) = watch::channel(30.0);
        let output = OutputControl {
            monitors: monitors_rx,
            settings: std::sync::Arc::new(settings_tx),
            fps: fps_rx,
        };
        let app = AppState::new(
            handle,
            presets,
            mapping_presets,
            media,
            logs,
            prx,
            srx,
            output,
            "t".into(),
            "0".into(),
        );
        let router = router(app);

        // GET : les deux écrans et les réglages par défaut.
        let response = router
            .clone()
            .oneshot(
                HttpRequest::get("/api/outputs")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        let json = body_json(response).await;
        assert_eq!(json["monitors"].as_array().map(Vec::len), Some(2));
        assert_eq!(json["monitors"][1]["name"], "VP");
        assert_eq!(json["settings"]["monitor"], 0);

        // POST valide : le canal reçoit les réglages.
        let response = router
            .clone()
            .oneshot(
                HttpRequest::post("/api/output")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"monitor":1,"fullscreen":true}"#))
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(settings_rx.has_changed().expect("canal vivant"));
        let applied = *settings_rx.borrow_and_update();
        assert_eq!(applied.monitor, 1);
        assert!(applied.fullscreen);

        // POST hors bornes : refusé, canal intact. (`router` reste vivant :
        // il porte l'émetteur des réglages via AppState.)
        let response = router
            .clone()
            .oneshot(
                HttpRequest::post("/api/output")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"monitor":7,"fullscreen":false}"#))
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!settings_rx.has_changed().expect("canal vivant"));
        drop(router);
    }

    /// L'arrêt du node ferme les WebSockets ouverts : sans cela, le graceful
    /// shutdown d'axum attendrait indéfiniment qu'une UI affichée se ferme
    /// d'elle-même (5 s d'« arrêt forcé » à chaque extinction).
    #[tokio::test]
    async fn shutdown_closes_open_websockets_quickly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let presets = PresetStore::open(dir.path().join("presets")).expect("presets");
        let mapping_presets =
            MappingStore::open(dir.path().join("presets").join("mapping")).expect("mappings");
        let media = MediaLibrary::open(dir.path().join("media"), 1024).expect("media");
        let logs = LogBuffer::new(16);
        let bus = Bus::new(16, 16);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (_ptx, prx) = watch::channel(PlaybackPosition::default());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = AppState::new(
            handle,
            presets,
            mapping_presets,
            media,
            logs,
            prx,
            shutdown_rx.clone(),
            OutputControl::disconnected(),
            "t".into(),
            "0".into(),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(serve_on(listener, app, shutdown_rx));

        // Client réel connecté au /ws : il reçoit l'état initial puis reste
        // ouvert sans jamais fermer de lui-même.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("connect");
        let first = ws.next().await.expect("frame").expect("état initial");
        assert!(first.is_text());

        shutdown_tx.send(true).expect("signal d'arrêt");
        let done = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
        assert!(
            done.is_ok(),
            "le serveur doit s'arrêter sans attendre la fermeture du client"
        );
    }
}
