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
mod zipper;

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
    /// Dernière frame vidéo décodée — pour l'aperçu web de la sortie.
    pub video: watch::Receiver<Option<toolbox_engine::VideoFrame>>,
}

impl OutputControl {
    /// Contrôle inerte (module de rendu absent) : aucun écran, réglages sans
    /// destinataire. Utilisé aussi par les tests.
    pub fn disconnected() -> Self {
        let (_, monitors) = watch::channel(Vec::new());
        let (settings, _) = watch::channel(OutputSettings::default());
        let (_, fps) = watch::channel(0.0);
        let (_, video) = watch::channel(None);
        Self {
            monitors,
            settings: std::sync::Arc::new(settings),
            fps,
            video,
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
    /// Parc de nodes découverts en mDNS (JSON prêt à servir ; vide sans
    /// découverte). Publié par le module fleet du binaire.
    fleet: watch::Receiver<serde_json::Value>,
    /// Mot de passe de l'UI/API (`[security] password`). `None` = ouvert
    /// (réseau local de confiance, comportement historique).
    password: Option<String>,
    started_at: Instant,
    node_name: String,
    version: String,
    /// Cache du dernier aperçu PNG (voir [`preview_png`]) : plusieurs
    /// dashboards ouverts ne coûtent qu'un rendu CPU par fenêtre de 250 ms,
    /// et les requêtes concurrentes attendent le même rendu au lieu d'en
    /// lancer chacune un (important sur Pi).
    preview: std::sync::Arc<tokio::sync::Mutex<PreviewCache>>,
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
        fleet: watch::Receiver<serde_json::Value>,
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
            fleet,
            password: None,
            started_at: Instant::now(),
            node_name,
            version,
            preview: std::sync::Arc::new(tokio::sync::Mutex::new(PreviewCache::default())),
        }
    }

    /// Active le mot de passe HTTP Basic (tout identifiant, ce mot de passe).
    #[must_use]
    pub fn with_password(mut self, password: Option<String>) -> Self {
        self.password = password.filter(|p| !p.is_empty());
        self
    }

    /// Canal fleet inerte (tests, découverte désactivée).
    pub fn no_fleet() -> watch::Receiver<serde_json::Value> {
        watch::channel(serde_json::Value::Array(Vec::new())).1
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
        .route("/api/fleet", get(fleet_get))
        .route("/api/identify", post(identify))
        .route("/api/system/reboot", post(system_reboot))
        .route("/api/system/shutdown", post(system_shutdown))
        .route("/api/preview.png", get(preview_png))
        .route("/api/diagnostic.zip", get(diagnostic_zip))
        .route("/ws", get(ws_events_upgrade))
        .route("/ws/logs", get(ws_logs_upgrade))
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            require_password,
        ))
        .with_state(app)
}

/// Mot de passe optionnel (HTTP Basic, tout identifiant accepté). Le
/// navigateur affiche sa boîte de connexion native ; les identifiants
/// suivent aussi les WebSocket de la même origine. Protection de niveau
/// réseau local — pour Internet, passer par Tailscale.
async fn require_password(
    State(app): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(expected) = &app.password else {
        return next.run(request).await;
    };
    use base64::Engine as _;
    let authorized = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|credentials| {
            credentials
                .split_once(':')
                .map(|(_, password)| password == expected)
        })
        .unwrap_or(false);
    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                axum::http::header::WWW_AUTHENTICATE,
                "Basic realm=\"Toolbox\", charset=\"UTF-8\"",
            )],
        )
            .into_response()
    }
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

/// Redémarre la MACHINE (pas seulement le node) — pour les installations
/// permanentes pilotées à distance. Confirmé côté UI.
async fn system_reboot() -> StatusCode {
    warn!("redémarrage machine demandé via l'API");
    machine_power(true)
}

/// Éteint la machine. Confirmé côté UI.
async fn system_shutdown() -> StatusCode {
    warn!("extinction machine demandée via l'API");
    machine_power(false)
}

fn machine_power(reboot: bool) -> StatusCode {
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("shutdown")
        .args([if reboot { "/r" } else { "/s" }, "/t", "5"])
        .spawn();
    #[cfg(not(target_os = "windows"))]
    let result = std::process::Command::new("systemctl")
        .arg(if reboot { "reboot" } else { "poweroff" })
        .spawn();
    match result {
        Ok(_) => StatusCode::ACCEPTED,
        Err(err) => {
            warn!(%err, "commande d'alimentation impossible");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Fenêtre de validité du cache d'aperçu : en dessous, les requêtes
/// partagent le même rendu. Les clients tirent toutes les 1,5 s : chacun
/// garde donc une image fraîche, même avec les effets animés.
const PREVIEW_TTL: std::time::Duration = std::time::Duration::from_millis(250);

/// Cache du dernier aperçu rendu (une seule entrée : tous les clients
/// demandent la même taille, et le snapshot plein format reste rare).
#[derive(Default)]
struct PreviewCache {
    entry: Option<PreviewEntry>,
}

struct PreviewEntry {
    width: u32,
    state: toolbox_core::NodeState,
    /// Identité de la frame vidéo affichée (pointeur des pixels).
    frame: Option<usize>,
    rendered_at: Instant,
    png: std::sync::Arc<Vec<u8>>,
}

impl PreviewCache {
    /// Ressert le rendu précédent si rien n'a changé (taille, état, frame)
    /// depuis moins de [`PREVIEW_TTL`] ; sinon appelle `render`.
    fn get_or_render(
        &mut self,
        width: u32,
        state: &toolbox_core::NodeState,
        frame: Option<usize>,
        render: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Option<std::sync::Arc<Vec<u8>>> {
        if let Some(entry) = &self.entry {
            if entry.width == width
                && entry.frame == frame
                && entry.rendered_at.elapsed() < PREVIEW_TTL
                && entry.state == *state
            {
                return Some(entry.png.clone());
            }
        }
        let png = std::sync::Arc::new(render()?);
        self.entry = Some(PreviewEntry {
            width,
            state: state.clone(),
            frame,
            rendered_at: Instant::now(),
            png: png.clone(),
        });
        Some(png)
    }
}

/// Aperçu de la sortie en PNG basse résolution : ce que projette le node,
/// vu depuis n'importe quel navigateur (`?w=480` optionnel, 64..960).
/// Rendu par la référence CPU — identique au shader GPU de la fenêtre.
/// Les requêtes concurrentes partagent un seul rendu (cache 250 ms sous
/// mutex) : plusieurs dashboards ouverts ne surchargent pas un Pi.
async fn preview_png(
    State(app): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    // 1920 max : le clic sur l'aperçu sert de snapshot pleine résolution.
    let width: u32 = params
        .get("w")
        .and_then(|w| w.parse().ok())
        .unwrap_or(480)
        .clamp(64, 1920);
    let height = width * 9 / 16;
    let state = app.bus.snapshot();
    let video = app.output.video.borrow().clone();
    let frame_key = video.as_ref().map(|f| f.rgba.as_ptr() as usize);
    let time = app.started_at.elapsed().as_secs_f32();

    let mut cache = app.preview.lock().await;
    let png = cache.get_or_render(width, &state, frame_key, || {
        let mut pixels = vec![0u32; (width * height) as usize];
        toolbox_engine::render_frame(&state, video.as_ref(), time, width, height, &mut pixels);
        let mut rgb = Vec::with_capacity(pixels.len() * 3);
        for px in &pixels {
            rgb.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
        }
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let encoded = encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(&rgb));
        match encoded {
            Ok(()) => Some(out),
            Err(err) => {
                warn!(%err, "aperçu PNG non encodé");
                None
            }
        }
    });
    drop(cache);

    match png {
        Some(png) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            (*png).clone(),
        )
            .into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Parc de nodes découverts en mDNS (ce node inclus).
async fn fleet_get(State(app): State<AppState>) -> Json<serde_json::Value> {
    Json(app.fleet.borrow().clone())
}

/// « Identifie » ce node : la mire « coins » s'affiche 4 secondes sur sa
/// sortie (et son nom est dedans via le titre de la fenêtre) — pratique pour
/// savoir quel projecteur appartient à quel node. La mire précédente est
/// restaurée ensuite.
async fn identify(State(app): State<AppState>) -> StatusCode {
    let before = app.bus.snapshot().test_pattern;
    let _ = app
        .bus
        .send(
            Source::Http,
            Command::SetTestPattern {
                pattern: Some(toolbox_core::TestPattern::Corners),
            },
        )
        .await;
    let bus = app.bus.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        let _ = bus
            .send(Source::Http, Command::SetTestPattern { pattern: before })
            .await;
    });
    info!("identification demandée : mire coins pendant 4 s");
    StatusCode::NO_CONTENT
}

/// Export diagnostic (brief 7.2) : une archive ZIP avec tout ce qu'il faut
/// pour comprendre un node à distance — état, journal, système, écrans,
/// médias, presets. Aucun secret dedans (ni node.toml, ni mot de passe).
async fn diagnostic_zip(State(app): State<AppState>) -> Result<Response, ApiError> {
    fn json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ApiError> {
        serde_json::to_vec_pretty(value).map_err(|e| ApiError::from(CoreError::from(e)))
    }

    let stats = monitor::collect(app.started_at);
    let rapport = format!(
        "Diagnostic du node Toolbox\n\
         ==========================\n\
         node      : {}\n\
         version   : {}\n\
         plateforme: {} ({})\n\
         genere    : {} s depuis l'epoque Unix\n\n\
         Contenu : etat.json (etat complet), journal.json (dernieres lignes\n\
         de log), systeme.json (CPU, memoire, disque, Tailscale),\n\
         sortie.json (ecran/plein ecran), ecrans.json, medias.json,\n\
         presets.json, fleet.json (nodes decouverts en mDNS).\n",
        app.node_name,
        app.version,
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );

    let mut zip = zipper::ZipWriter::new();
    zip.add("rapport.txt", rapport.as_bytes());
    zip.add("etat.json", &json(&app.bus.snapshot())?);
    zip.add("journal.json", &json(&app.logs.snapshot())?);
    zip.add("systeme.json", &json(&stats)?);
    zip.add("sortie.json", &json(&*app.output.settings.borrow())?);
    zip.add("ecrans.json", &json(&*app.output.monitors.borrow())?);
    zip.add("medias.json", &json(&app.media.list()?)?);
    zip.add(
        "presets.json",
        &json(&serde_json::json!({
            "complets": app.presets.list()?,
            "mapping": app.mapping_presets.list()?,
        }))?,
    );
    zip.add("fleet.json", &json(&*app.fleet.borrow())?);

    let filename = format!("diagnostic-{}.zip", app.node_name);
    Ok((
        StatusCode::OK,
        [
            ("content-type", "application/zip".to_string()),
            (
                "content-disposition",
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        zip.finish(),
    )
        .into_response())
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
            AppState::no_fleet(),
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
            AppState::no_fleet(),
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

    /// L'export diagnostic est une archive ZIP valide qui embarque l'état.
    #[tokio::test]
    async fn diagnostic_zip_responds_with_a_zip() {
        let bed = testbed();
        let response = bed
            .router
            .oneshot(
                HttpRequest::get("/api/diagnostic.zip")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "application/zip",
            "content-type"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
            .await
            .expect("body");
        // Signature d'en-tête local ZIP + fin d'archive présente.
        assert_eq!(&bytes[0..4], b"PK\x03\x04");
        assert_eq!(&bytes[bytes.len() - 22..bytes.len() - 18], b"PK\x05\x06");
        let haystack = String::from_utf8_lossy(&bytes);
        for name in [
            "rapport.txt",
            "etat.json",
            "journal.json",
            "systeme.json",
            "presets.json",
        ] {
            assert!(haystack.contains(name), "entrée manquante : {name}");
        }
    }

    /// Le mot de passe optionnel protège tout : 401 sans identifiants,
    /// accès avec le bon mot de passe (peu importe l'identifiant).
    #[tokio::test]
    async fn password_gates_everything_when_set() {
        use base64::Engine as _;
        let bed = testbed(); // sans mot de passe : ouvert (autres tests)
        drop(bed);

        let dir = tempfile::tempdir().expect("tempdir");
        let presets = PresetStore::open(dir.path().join("presets")).expect("presets");
        let mapping_presets =
            MappingStore::open(dir.path().join("presets").join("mapping")).expect("mappings");
        let media = MediaLibrary::open(dir.path().join("media"), 1024).expect("media");
        let bus = Bus::new(8, 8);
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (_ptx, prx) = watch::channel(PlaybackPosition::default());
        let (_stx, srx) = watch::channel(false);
        let app = AppState::new(
            handle,
            presets,
            mapping_presets,
            media,
            LogBuffer::new(8),
            prx,
            srx,
            OutputControl::disconnected(),
            AppState::no_fleet(),
            "t".into(),
            "0".into(),
        )
        .with_password(Some("sésame".into()));
        let router = router(app);

        // Sans identifiants : 401 + invite Basic.
        let response = router
            .clone()
            .oneshot(
                HttpRequest::get("/api/health")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response
            .headers()
            .get(axum::http::header::WWW_AUTHENTICATE)
            .is_some());

        // Mauvais mot de passe : 401.
        let bad = base64::engine::general_purpose::STANDARD.encode("pym:faux");
        let response = router
            .clone()
            .oneshot(
                HttpRequest::get("/api/health")
                    .header("authorization", format!("Basic {bad}"))
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Bon mot de passe, identifiant quelconque : accès.
        let good = base64::engine::general_purpose::STANDARD.encode("pym:sésame");
        let response = router
            .oneshot(
                HttpRequest::get("/api/health")
                    .header("authorization", format!("Basic {good}"))
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Le cache d'aperçu partage les rendus dans la fenêtre de 250 ms et
    /// re-rend dès que la taille ou l'état change.
    #[test]
    fn preview_cache_shares_renders_within_the_window() {
        let mut cache = PreviewCache::default();
        let state = toolbox_core::NodeState::default();
        let mut renders = 0;

        let a = cache
            .get_or_render(480, &state, None, || {
                renders += 1;
                Some(vec![1])
            })
            .expect("a");
        let b = cache
            .get_or_render(480, &state, None, || {
                renders += 1;
                Some(vec![2])
            })
            .expect("b");
        assert_eq!(renders, 1, "le deuxième appel ressert le cache");
        assert_eq!(*a, *b);

        // Autre taille → nouveau rendu.
        cache
            .get_or_render(960, &state, None, || {
                renders += 1;
                Some(vec![3])
            })
            .expect("c");
        assert_eq!(renders, 2);

        // État changé → nouveau rendu, même taille.
        let mut autre = toolbox_core::NodeState::default();
        autre.player.volume = 0.5;
        cache
            .get_or_render(960, &autre, None, || {
                renders += 1;
                Some(vec![4])
            })
            .expect("d");
        assert_eq!(renders, 3);

        // Nouvelle frame vidéo → nouveau rendu.
        cache
            .get_or_render(960, &autre, Some(42), || {
                renders += 1;
                Some(vec![5])
            })
            .expect("e");
        assert_eq!(renders, 4);
    }

    /// L'aperçu de la sortie est un vrai PNG aux dimensions demandées.
    #[tokio::test]
    async fn preview_png_renders() {
        let bed = testbed();
        let response = bed
            .router
            .oneshot(
                HttpRequest::get("/api/preview.png?w=128")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("image/png")
        );
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("corps")
            .to_bytes();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "signature PNG");
        // 128×72 attendu (16:9) — vérifié dans l'en-tête IHDR.
        assert_eq!(&bytes[16..20], &128u32.to_be_bytes());
        assert_eq!(&bytes[20..24], &72u32.to_be_bytes());
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
        let (_video_tx, video_rx) = watch::channel(None);
        let output = OutputControl {
            monitors: monitors_rx,
            settings: std::sync::Arc::new(settings_tx),
            fps: fps_rx,
            video: video_rx,
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
            AppState::no_fleet(),
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
            AppState::no_fleet(),
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
