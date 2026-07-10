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
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use toolbox_core::{MonitorInfo, NodeState, OutputSettings};
use toolbox_engine::VideoFrame;

use crate::raster::render_frame;

/// Réglages fixes de la fenêtre (les réglages à chaud passent par
/// [`OutputChannels::settings`]).
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Titre de la fenêtre (nom du node).
    pub title: String,
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

    // Relais : bus/vidéo/réglages/arrêt (async) → event loop (sync).
    spawn_wake_relay(
        event_loop.create_proxy(),
        state.clone(),
        video.clone(),
        settings.clone(),
        shutdown,
    );

    let mut app = OutputApp {
        config,
        state,
        video,
        settings,
        monitors,
        fps,
        frames_since: 0,
        fps_window_start: std::time::Instant::now(),
        window: None,
        surface: None,
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
    video: watch::Receiver<Option<VideoFrame>>,
    settings: watch::Receiver<OutputSettings>,
    monitors: watch::Sender<Vec<MonitorInfo>>,
    fps: watch::Sender<f32>,
    /// Frames présentées depuis le début de la fenêtre de mesure courante.
    frames_since: u32,
    fps_window_start: std::time::Instant,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
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

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return; // fenêtre réduite : rien à peindre
        };
        if let Err(err) = surface.resize(w, h) {
            warn!(%err, "surface de sortie non retaillée");
            return;
        }
        match surface.buffer_mut() {
            Ok(mut buffer) => {
                let snapshot = self.state.borrow().clone();
                let video = self.video.borrow().clone();
                render_frame(&snapshot, video.as_ref(), w.get(), h.get(), &mut buffer);
                if let Err(err) = buffer.present() {
                    warn!(%err, "frame de sortie non présentée");
                    return;
                }
                self.count_presented_frame();
            }
            Err(err) => warn!(%err, "tampon de sortie inaccessible"),
        }
    }

    /// Mesure du débit de frames présentées, publiée ~1 fois par seconde.
    fn count_presented_frame(&mut self) {
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
        let settings = *self.settings.borrow();
        let monitor = self.refresh_monitors(event_loop, settings.monitor);
        let mut attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(LogicalSize::new(960.0, 540.0));
        if settings.fullscreen {
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
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(context) => context,
            Err(err) => {
                error!(%err, "contexte d'affichage indisponible");
                event_loop.exit();
                return;
            }
        };
        match softbuffer::Surface::new(&context, window.clone()) {
            Ok(surface) => {
                info!("fenêtre de sortie ouverte (F11 : plein écran)");
                self.surface = Some(surface);
                self.window = Some(window);
            }
            Err(err) => {
                error!(%err, "surface d'affichage indisponible");
                event_loop.exit();
            }
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
            Wake::Quit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
                info!("fenêtre de sortie fermée par l'utilisateur (le node continue)");
                event_loop.exit();
            }
            _ => {}
        }
    }
}
