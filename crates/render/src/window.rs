//! Fenêtre de sortie native : winit + softbuffer, dans un thread dédié.
//!
//! L'event loop tourne hors du thread principal (`with_any_thread`, Windows
//! et Linux X11/Wayland — les cibles du node ; macOS n'est pas supporté ici).
//! Un relais forwarde vers l'event loop (via
//! [`winit::event_loop::EventLoopProxy`]) : les mutations d'état du bus et
//! les frames vidéo (redraw), les réglages de sortie (changement d'écran ou
//! de plein écran à chaud, depuis l'UI web) et le signal d'arrêt. La fenêtre
//! publie en retour la liste des écrans détectés. Raccourcis : F11 bascule
//! le plein écran, Échap le quitte. Fermer la fenêtre n'arrête pas le node.

use std::num::NonZeroU32;
use std::sync::Arc;

use tokio::sync::watch;
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use toolbox_core::{MonitorInfo, NodeState, OutputSettings};
use toolbox_engine::VideoFrame;

use crate::gpu::GpuPainter;

/// Réglages fixes de la fenêtre (les réglages à chaud passent par
/// [`OutputChannels::settings`]).
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Titre de la fenêtre (nom du node).
    pub title: String,
    /// Rendu par la carte graphique (repli CPU automatique en cas d'échec).
    pub gpu: bool,
}

/// Le peintre de la fenêtre : GPU (wgpu) ou CPU (softbuffer, repli).
/// GPU boxé : la variante est bien plus grosse que la surface CPU.
enum Painter {
    Gpu(Box<GpuPainter>),
    Cpu(softbuffer::Surface<Arc<Window>, Arc<Window>>),
}

/// Les canaux qui relient la fenêtre au reste du node.
pub struct OutputChannels {
    /// État du node (bus) : chaque mutation redessine.
    pub state: watch::Receiver<NodeState>,
    /// Dernière frame vidéo décodée (`None` sans backend vidéo).
    pub video: watch::Receiver<Option<VideoFrame>>,
    /// Réglages de sortie appliqués à chaud (écran cible, plein écran).
    pub settings: watch::Receiver<OutputSettings>,
    /// Liste des écrans détectés, publiée pour l'API `/api/outputs`.
    pub monitors: watch::Sender<Vec<MonitorInfo>>,
    /// Frames réellement présentées par seconde, publiées pour l'UI
    /// (indicateur de fluidité du rendu, rafraîchi ~1 fois/s).
    pub fps: watch::Sender<f32>,
    /// Interrupteur « fenêtre de sortie » (onglet Fonctions) : à `false`,
    /// la fenêtre est masquée et le peintre détruit (surface GPU rendue,
    /// aucun redraw — 0 % CPU/GPU) ; à `true`, tout est recréé à chaud.
    pub enabled: watch::Receiver<bool>,
    /// Signal d'arrêt du node.
    pub shutdown: watch::Receiver<bool>,
}

/// Événements injectés dans l'event loop depuis le monde async.
#[derive(Debug)]
enum Wake {
    /// État ou frame vidéo : re-dessiner.
    Redraw,
    /// Réglages de sortie modifiés : déplacer/basculer la fenêtre.
    SettingsChanged,
    /// Interrupteur de la fonction basculé : sommeil ou réveil.
    EnabledChanged,
    /// Arrêt du node : fermer la fenêtre et sortir de la boucle.
    Quit,
}

/// Lance la fenêtre de sortie dans un thread dédié et retourne son handle.
///
/// Ne bloque pas ; en cas d'échec (pas de serveur graphique…), l'erreur est
/// tracée et le node continue sans fenêtre — la sortie est un service, pas
/// une condition de démarrage.
pub fn spawn(config: WindowConfig, channels: OutputChannels) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("toolbox-sortie".into())
        .spawn(move || run_event_loop(config, channels))
        .unwrap_or_else(|err| {
            error!(%err, "impossible de créer le thread de la fenêtre de sortie");
            // Thread de repli déjà terminé : join() immédiat côté appelant.
            std::thread::spawn(|| {})
        })
}

fn run_event_loop(config: WindowConfig, channels: OutputChannels) {
    let OutputChannels {
        state,
        video,
        settings,
        monitors,
        fps,
        enabled,
        shutdown,
    } = channels;

    let mut builder = EventLoop::<Wake>::with_user_event();
    // L'event loop vit dans ce thread, pas le principal.
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        builder.with_any_thread(true);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // X11 et Wayland exposent chacun leur trait avec la même méthode :
        // appels qualifiés.
        winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(&mut builder, true);
        winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(&mut builder, true);
    }
    let event_loop = match builder.build() {
        Ok(el) => el,
        Err(err) => {
            error!(%err, "fenêtre de sortie indisponible (pas d'environnement graphique ?) — le node continue sans");
            return;
        }
    };

    // Relais : bus/vidéo/réglages/interrupteur/arrêt (async) → event loop.
    spawn_wake_relay(
        event_loop.create_proxy(),
        state.clone(),
        video.clone(),
        settings.clone(),
        enabled.clone(),
        shutdown,
    );

    let snapshot = state.borrow().clone();
    let mut app = OutputApp {
        config,
        state,
        snapshot,
        video,
        settings,
        monitors,
        fps,
        enabled,
        frames_since: 0,
        fps_window_start: std::time::Instant::now(),
        derniere_frame: std::time::Instant::now(),
        dernier_scan_ecrans: std::time::Instant::now(),
        started_at: std::time::Instant::now(),
        window: None,
        painter: None,
        forcer_cpu: false,
        blackout_prec: false,
        blackout_depart: 0.0,
        blackout_depuis: std::time::Instant::now(),
        blackout_niveau: 0.0,
        frame_gelee: None,
        lut_cache: None,
    };
    if let Err(err) = event_loop.run_app(&mut app) {
        error!(%err, "event loop de la fenêtre de sortie terminé en erreur");
    }
    info!("fenêtre de sortie fermée");
}

/// Forwarde chaque signal async vers l'event loop. Runtime minimal dédié.
fn spawn_wake_relay(
    proxy: EventLoopProxy<Wake>,
    mut state: watch::Receiver<NodeState>,
    mut video: watch::Receiver<Option<VideoFrame>>,
    mut settings: watch::Receiver<OutputSettings>,
    mut enabled: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let relay = std::thread::Builder::new()
        .name("toolbox-sortie-relais".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().build() else {
                warn!("relais de la fenêtre de sortie indisponible : redraw sur événements fenêtre uniquement");
                return;
            };
            runtime.block_on(async move {
                // Chaque canal peut se fermer indépendamment (pas de backend
                // vidéo → canal frames fermé d'office, module http absent →
                // canal réglages fermé) : on désactive la branche concernée
                // sans tuer le relais. État ou arrêt fermés = fin du node.
                let mut video_alive = true;
                let mut settings_alive = true;
                let mut enabled_alive = true;
                loop {
                    let wake = tokio::select! {
                        changed = state.changed() => {
                            if changed.is_err() { Wake::Quit } else { Wake::Redraw }
                        }
                        changed = video.changed(), if video_alive => match changed {
                            Ok(()) => Wake::Redraw,
                            Err(_) => {
                                video_alive = false;
                                continue;
                            }
                        },
                        changed = settings.changed(), if settings_alive => match changed {
                            Ok(()) => Wake::SettingsChanged,
                            Err(_) => {
                                settings_alive = false;
                                continue;
                            }
                        },
                        changed = enabled.changed(), if enabled_alive => match changed {
                            Ok(()) => Wake::EnabledChanged,
                            Err(_) => {
                                enabled_alive = false;
                                continue;
                            }
                        },
                        _ = shutdown.changed() => Wake::Quit,
                    };
                    let quit = matches!(wake, Wake::Quit);
                    if proxy.send_event(wake).is_err() || quit {
                        break;
                    }
                }
            });
        });
    if let Err(err) = relay {
        warn!(%err, "relais de la fenêtre de sortie non démarré");
    }
}

/// L'application winit : une fenêtre, une surface softbuffer, l'état du node.
struct OutputApp {
    config: WindowConfig,
    state: watch::Receiver<NodeState>,
    /// Copie locale de l'état, rafraîchie SEULEMENT quand le bus a publié
    /// un changement — pas un clone complet (playlist, masques, mesh…)
    /// par frame vidéo.
    snapshot: NodeState,
    video: watch::Receiver<Option<VideoFrame>>,
    settings: watch::Receiver<OutputSettings>,
    monitors: watch::Sender<Vec<MonitorInfo>>,
    fps: watch::Sender<f32>,
    /// Interrupteur de la fonction (onglet Fonctions).
    enabled: watch::Receiver<bool>,
    /// Frames présentées depuis le début de la fenêtre de mesure courante.
    frames_since: u32,
    fps_window_start: std::time::Instant,
    /// Instant de la dernière frame présentée : sert à faire retomber le
    /// badge img/s à 0 quand le rendu s'arrête (sinon il resterait figé).
    derniere_frame: std::time::Instant,
    /// Dernier scan des écrans (throttle du rafraîchissement à chaud).
    dernier_scan_ecrans: std::time::Instant,
    /// Origine du temps des effets animés (bruit).
    started_at: std::time::Instant,
    window: Option<Arc<Window>>,
    painter: Option<Painter>,
    /// Repli CPU verrouillé après une perte de device GPU : on ne retente
    /// plus le GPU (il vient de lâcher) pour cette session de fenêtre.
    forcer_cpu: bool,
    /// Rampe du blackout de régie : consigne précédente, niveau au moment
    /// du dernier changement, instant du changement, niveau courant.
    blackout_prec: bool,
    blackout_depart: f32,
    blackout_depuis: std::time::Instant,
    blackout_niveau: f32,
    /// Frame retenue pendant un gel d'image (`state.freeze`).
    frame_gelee: Option<VideoFrame>,
    /// LUT chargée depuis `luts/<nom>` — `None` dans la paire : fichier
    /// illisible (mémorisé pour ne pas relire le disque à chaque frame).
    /// LUT chargée : nom, date de modification du fichier au chargement, et
    /// la LUT décodée (`None` si le fichier était illisible). La mtime permet
    /// de RECHARGER quand le fichier change sur disque à nom constant.
    lut_cache: Option<(
        String,
        Option<std::time::SystemTime>,
        Option<toolbox_engine::Lut3d>,
    )>,
}

impl OutputApp {
    /// Détecte les écrans, publie la liste pour l'API et retourne la cible.
    fn refresh_monitors(
        &self,
        event_loop: &ActiveEventLoop,
        target: usize,
    ) -> Option<winit::monitor::MonitorHandle> {
        let monitors: Vec<_> = event_loop.available_monitors().collect();
        let infos: Vec<MonitorInfo> = monitors
            .iter()
            .enumerate()
            .map(|(index, m)| MonitorInfo {
                index,
                name: m.name().unwrap_or_else(|| format!("écran {index}")),
                width: m.size().width,
                height: m.size().height,
            })
            .collect();
        for info in &infos {
            info!(
                ecran = info.index,
                nom = %info.name,
                largeur = info.width,
                hauteur = info.height,
                "écran détecté"
            );
        }
        self.monitors.send_replace(infos);
        if target > 0 && target >= monitors.len() {
            warn!(
                demande = target,
                detectes = monitors.len(),
                "écran demandé introuvable : premier écran utilisé"
            );
        }
        monitors.get(target).or_else(|| monitors.first()).cloned()
    }

    /// Republie la liste des écrans si elle a CHANGÉ (branchement/débranchement
    /// à chaud) — sans log ni recalcul de cible, appelée périodiquement. Sinon
    /// `/api/outputs` et la carte « Sortie » resteraient figés sur la liste du
    /// démarrage jusqu'à la prochaine modification de réglages.
    fn rafraichir_liste_ecrans(&self, event_loop: &ActiveEventLoop) {
        let infos: Vec<MonitorInfo> = event_loop
            .available_monitors()
            .enumerate()
            .map(|(index, m)| MonitorInfo {
                index,
                name: m.name().unwrap_or_else(|| format!("écran {index}")),
                width: m.size().width,
                height: m.size().height,
            })
            .collect();
        if *self.monitors.borrow() != infos {
            info!(
                ecrans = infos.len(),
                "liste des écrans mise à jour (branchement à chaud)"
            );
            self.monitors.send_replace(infos);
        }
    }

    /// Applique les réglages courants : écran cible + plein écran.
    fn apply_settings(&self, event_loop: &ActiveEventLoop) {
        let settings = *self.settings.borrow();
        let monitor = self.refresh_monitors(event_loop, settings.monitor);
        let Some(window) = &self.window else { return };
        if settings.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(monitor)));
        } else {
            window.set_fullscreen(None);
            if let Some(monitor) = monitor {
                window.set_outer_position(monitor.position());
            }
        }
        window.request_redraw();
    }

    fn toggle_fullscreen(&self) {
        if let Some(window) = &self.window {
            if window.fullscreen().is_some() {
                window.set_fullscreen(None);
            } else {
                window.set_fullscreen(Some(Fullscreen::Borderless(window.current_monitor())));
            }
        }
    }

    /// Fabrique le peintre (GPU si demandé, CPU en secours). `None` si aucun
    /// contexte d'affichage n'est possible.
    fn creer_peintre(&self, window: &Arc<Window>) -> Option<Painter> {
        if self.config.gpu && !self.forcer_cpu {
            match GpuPainter::new(window.clone()) {
                Ok(gpu) => return Some(Painter::Gpu(Box::new(gpu))),
                Err(err) => {
                    warn!(%err, "rendu GPU indisponible — repli sur le rendu CPU");
                }
            }
        } else if self.forcer_cpu {
            info!("rendu CPU forcé (device GPU perdu précédemment)");
        } else {
            info!("rendu GPU désactivé par la config ([output] gpu = false)");
        }
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(context) => context,
            Err(err) => {
                error!(%err, "contexte d'affichage indisponible");
                return None;
            }
        };
        match softbuffer::Surface::new(&context, window.clone()) {
            Ok(surface) => Some(Painter::Cpu(surface)),
            Err(err) => {
                error!(%err, "surface d'affichage indisponible");
                None
            }
        }
    }

    /// Sommeil/réveil de la fonction : masque la fenêtre et détruit le
    /// peintre (surface GPU rendue, plus aucun redraw), ou recrée tout.
    fn apply_enabled(&mut self, event_loop: &ActiveEventLoop) {
        let enabled = *self.enabled.borrow();
        let Some(window) = self.window.clone() else {
            return;
        };
        if enabled {
            if self.painter.is_none() {
                self.painter = self.creer_peintre(&window);
            }
            window.set_visible(true);
            self.apply_settings(event_loop);
            window.request_redraw();
            info!("fenêtre de sortie réveillée");
        } else {
            self.painter = None; // libère la surface (GPU comprise)
            window.set_visible(false);
            self.fps.send_replace(0.0);
            info!("fenêtre de sortie en sommeil (0 rendu, surface libérée)");
        }
    }

    /// Charge (et mémorise) la LUT nommée par l'état. Un fichier illisible
    /// est journalisé une fois puis ignoré.
    fn sync_lut_cache(&mut self, nom: Option<&str>) {
        /// Plafond de taille d'un `.cube` : une LUT 128³ pèse ~25 Mo ; au-delà
        /// c'est un fichier aberrant qu'on ne charge pas sur le thread de rendu.
        const LUT_MAX_OCTETS: u64 = 64 * 1024 * 1024;
        let Some(nom) = nom else {
            self.lut_cache = None;
            return;
        };
        let chemin = std::path::Path::new("luts").join(nom);
        let mtime = std::fs::metadata(&chemin).and_then(|m| m.modified()).ok();
        // Rechargement si le NOM ou la date de modification a changé (un
        // fichier réécrit sous le même nom est bien repris).
        let deja = self
            .lut_cache
            .as_ref()
            .is_some_and(|(n, m, _)| n == nom && *m == mtime);
        if deja {
            return;
        }
        // Plafond de taille avant lecture (bloquerait le rendu sinon).
        if let Ok(meta) = std::fs::metadata(&chemin) {
            if meta.len() > LUT_MAX_OCTETS {
                warn!(nom, octets = meta.len(), "LUT trop volumineuse — ignorée");
                self.lut_cache = Some((nom.to_string(), mtime, None));
                return;
            }
        }
        let charge = std::fs::read_to_string(&chemin)
            .map_err(|e| e.to_string())
            .and_then(|t| toolbox_engine::Lut3d::depuis_texte(&t));
        match &charge {
            Ok(lut) => info!(nom, taille = lut.taille, "LUT d'étalonnage chargée"),
            Err(err) => warn!(nom, %err, "LUT illisible — ignorée"),
        }
        self.lut_cache = Some((nom.to_string(), mtime, charge.ok()));
    }

    fn redraw(&mut self) {
        // Fonction coupée : aucun rendu, quel que soit l'événement.
        if !*self.enabled.borrow() {
            return;
        }
        // Les mutations de cache (LUT, gel, rampe) précèdent l'emprunt du
        // peintre : une méthode `&mut self` ne peut pas cohabiter avec lui.
        let Some(size) = self.window.as_ref().map(|w| w.inner_size()) else {
            return;
        };
        if self.painter.is_none() {
            return;
        }
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return; // fenêtre réduite : rien à peindre
        };
        // Le clone complet de l'état (playlist, masques, mesh…) ne se fait
        // qu'aux changements publiés par le bus — pas à chaque frame vidéo.
        if self.state.has_changed().unwrap_or(false) {
            self.snapshot = self.state.borrow_and_update().clone();
        }
        let nom_lut = self.snapshot.lut.clone();
        self.sync_lut_cache(nom_lut.as_deref());
        // Gel d'image : la frame affichée au moment du gel est retenue tant
        // que `freeze` est posé — le transport continue en dessous.
        let video = if self.snapshot.freeze {
            if self.frame_gelee.is_none() {
                self.frame_gelee = self.video.borrow().clone();
            }
            self.frame_gelee.clone()
        } else {
            self.frame_gelee = None;
            self.video.borrow().clone()
        };
        // Rampe du blackout : au changement de consigne, la rampe repart du
        // niveau courant (relâcher en plein fondu redescend de là).
        let cible = if self.snapshot.blackout.actif {
            1.0
        } else {
            0.0
        };
        if self.snapshot.blackout.actif != self.blackout_prec {
            self.blackout_prec = self.snapshot.blackout.actif;
            self.blackout_depart = self.blackout_niveau;
            self.blackout_depuis = std::time::Instant::now();
        }
        #[allow(clippy::cast_possible_truncation)] // fondu borné à 10 s
        let ecoule = self.blackout_depuis.elapsed().as_millis() as u64;
        let niveau = toolbox_engine::niveau_rampe(
            cible,
            self.blackout_depart,
            ecoule,
            self.snapshot.blackout.fondu_ms,
        );
        self.blackout_niveau = niveau;
        let time = self.started_at.elapsed().as_secs_f32();
        let lut = self
            .lut_cache
            .as_ref()
            .and_then(|(nom, _mtime, lut)| lut.as_ref().map(|l| (nom.as_str(), l)));
        // L'emprunt du peintre doit se terminer avant de compter la frame
        // (le compteur emprunte `self` à son tour).
        // Issue du rendu, calculée pendant l'emprunt du peintre, puis traitée
        // APRÈS l'avoir relâché (le repli CPU réassigne `self.painter`).
        enum Suite {
            Presentee,
            Sautee,
            ReplierCpu,
        }
        let Some(painter) = self.painter.as_mut() else {
            return;
        };
        let suite = match painter {
            Painter::Gpu(gpu) => match gpu.render(
                &self.snapshot,
                video.as_ref(),
                lut,
                time,
                w.get(),
                h.get(),
                niveau,
            ) {
                crate::gpu::ResultatRendu::Presentee => Suite::Presentee,
                crate::gpu::ResultatRendu::Sautee => Suite::Sautee,
                crate::gpu::ResultatRendu::DevicePerdu => Suite::ReplierCpu,
            },
            Painter::Cpu(surface) => {
                if let Err(err) = surface.resize(w, h) {
                    warn!(%err, "surface de sortie non retaillée");
                    return;
                }
                match surface.buffer_mut() {
                    Ok(mut buffer) => {
                        toolbox_engine::raster::render_frame_lut(
                            &self.snapshot,
                            video.as_ref(),
                            lut.map(|(_, l)| l),
                            time,
                            w.get(),
                            h.get(),
                            &mut buffer,
                        );
                        toolbox_engine::appliquer_blackout(&mut buffer, niveau);
                        match buffer.present() {
                            Ok(()) => Suite::Presentee,
                            Err(err) => {
                                warn!(%err, "frame de sortie non présentée");
                                Suite::Sautee
                            }
                        }
                    }
                    Err(err) => {
                        warn!(%err, "tampon de sortie inaccessible");
                        Suite::Sautee
                    }
                }
            }
        };
        match suite {
            Suite::Presentee => self.count_presented_frame(),
            Suite::Sautee => {}
            Suite::ReplierCpu => {
                // Repli CPU à chaud : le device GPU vient de lâcher (pilote
                // réinitialisé, écran débranché en plein écran). Plutôt qu'une
                // sortie noire définitive, on recrée un peintre CPU et on
                // verrouille ce mode pour cette session de fenêtre.
                warn!("device GPU perdu — bascule sur le rendu CPU");
                self.forcer_cpu = true;
                self.painter = None;
                if let Some(window) = self.window.clone() {
                    self.painter = self.creer_peintre(&window);
                    window.request_redraw();
                }
            }
        }
        // Rampe en cours : on continue à redessiner jusqu'à la cible.
        if (niveau - cible).abs() > 0.001 {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    /// Mesure du débit de frames présentées, publiée ~1 fois par seconde.
    fn count_presented_frame(&mut self) {
        self.derniere_frame = std::time::Instant::now();
        self.frames_since += 1;
        let elapsed = self.fps_window_start.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            self.fps.send_replace(self.frames_since as f32 / elapsed);
            self.frames_since = 0;
            self.fps_window_start = std::time::Instant::now();
        }
    }
}

impl ApplicationHandler<Wake> for OutputApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let enabled = *self.enabled.borrow();
        let settings = *self.settings.borrow();
        let monitor = self.refresh_monitors(event_loop, settings.monitor);
        let mut attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(LogicalSize::new(960.0, 540.0))
            // Fonction coupée au démarrage : fenêtre créée cachée, sans
            // peintre (aucune surface allouée) — dormante jusqu'au réveil.
            .with_visible(enabled);
        if settings.fullscreen && enabled {
            attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(monitor)));
        } else if let Some(monitor) = monitor {
            // Fenêtré mais sur le bon écran.
            attributes = attributes.with_position(monitor.position());
        }
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(err) => {
                error!(%err, "création de la fenêtre de sortie impossible");
                event_loop.exit();
                return;
            }
        };
        self.window = Some(window.clone());
        if enabled {
            // Échec du peintre au boot (pilote pas prêt — un Pi démarre
            // parfois plus vite que sa pile graphique) : on reste DORMANT
            // au lieu de tuer la boucle — la bascule off/on de l'onglet
            // Fonctions retentera avec la pile prête.
            match self.creer_peintre(&window) {
                Some(painter) => {
                    self.painter = Some(painter);
                    info!("fenêtre de sortie ouverte (F11 : plein écran)");
                }
                None => {
                    window.set_visible(false);
                    error!(
                        "aucun contexte d'affichage au démarrage — fenêtre dormante \
                         (onglet Fonctions : sortie off/on pour réessayer)"
                    );
                }
            }
        } else {
            info!(
                "fenêtre de sortie dormante (fonction coupée — onglet Fonctions pour la réveiller)"
            );
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        match event {
            Wake::Redraw => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            Wake::SettingsChanged => self.apply_settings(event_loop),
            Wake::EnabledChanged => self.apply_enabled(event_loop),
            Wake::Quit => event_loop.exit(),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Fait retomber le badge img/s à 0 quand plus aucune frame n'est
        // présentée (source arrêtée, mire figée) : sinon il resterait bloqué
        // sur la dernière cadence et l'opérateur croirait que ça tourne.
        // On ne programme ce réveil périodique QUE si un peintre est actif :
        // fenêtre dormante = attente pure, 0 % CPU préservé.
        if self.painter.is_some() && *self.enabled.borrow() {
            if self.derniere_frame.elapsed() > std::time::Duration::from_millis(1200)
                && (*self.fps.borrow() - 0.0).abs() > f32::EPSILON
            {
                self.fps.send_replace(0.0);
            }
            // Écrans branchés/débranchés à chaud (throttle ~2 s).
            if self.dernier_scan_ecrans.elapsed() > std::time::Duration::from_secs(2) {
                self.rafraichir_liste_ecrans(event_loop);
                self.dernier_scan_ecrans = std::time::Instant::now();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                match event.logical_key {
                    Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
                    // Échap quitte SEULEMENT le plein écran (jamais la
                    // fenêtre : un show ne se ferme pas sur une fausse touche).
                    Key::Named(NamedKey::Escape) => {
                        if let Some(window) = &self.window {
                            window.set_fullscreen(None);
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::CloseRequested => {
                // Alt+F4 / clic sur la croix : on NE quitte PAS la boucle
                // d'événements (ça rendrait la sortie irrécupérable sans
                // redémarrer le node). On masque la fenêtre et on libère le
                // peintre — comme une mise en sommeil. La sortie se rouvre
                // depuis l'onglet Sortie/Fonctions (bascule off/on).
                self.painter = None;
                if let Some(window) = &self.window {
                    window.set_visible(false);
                }
                self.fps.send_replace(0.0);
                info!(
                    "fenêtre de sortie fermée par l'utilisateur — réactivable depuis l'UI (le node continue)"
                );
            }
            _ => {}
        }
    }
}
