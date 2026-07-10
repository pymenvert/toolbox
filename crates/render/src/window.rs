//! Fenêtre de sortie native : winit + softbuffer, dans un thread dédié.
//!
//! L'event loop tourne hors du thread principal (`with_any_thread`, Windows
//! et Linux X11/Wayland — les cibles du node ; macOS n'est pas supporté ici).
//! Un relais forwarde les changements d'état du bus vers l'event loop via
//! [`winit::event_loop::EventLoopProxy`] : chaque mutation (coin déplacé,
//! mire changée…) déclenche un redraw. Raccourcis : F11 bascule le plein
//! écran, Échap le quitte. Fermer la fenêtre n'arrête pas le node.

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

use toolbox_core::NodeState;

use crate::raster::render_frame;

/// Réglages de la fenêtre (issus de `[output]` dans node.toml).
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Écran cible, par index dans la liste détectée (0 = premier).
    pub monitor: usize,
    /// Démarre en plein écran sans bordure sur l'écran cible.
    pub fullscreen: bool,
    /// Titre de la fenêtre (nom du node).
    pub title: String,
}

/// Événements injectés dans l'event loop depuis le monde async.
#[derive(Debug)]
enum Wake {
    /// L'état du node a changé : re-dessiner.
    StateChanged,
    /// Arrêt du node : fermer la fenêtre et sortir de la boucle.
    Quit,
}

/// Lance la fenêtre de sortie dans un thread dédié et retourne son handle.
///
/// Ne bloque pas ; en cas d'échec (pas de serveur graphique…), l'erreur est
/// tracée et le node continue sans fenêtre — la sortie est un service, pas
/// une condition de démarrage.
pub fn spawn(
    config: WindowConfig,
    state: watch::Receiver<NodeState>,
    shutdown: watch::Receiver<bool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("toolbox-sortie".into())
        .spawn(move || run_event_loop(config, state, shutdown))
        .unwrap_or_else(|err| {
            error!(%err, "impossible de créer le thread de la fenêtre de sortie");
            // Thread de repli déjà terminé : join() immédiat côté appelant.
            std::thread::spawn(|| {})
        })
}

fn run_event_loop(
    config: WindowConfig,
    state: watch::Receiver<NodeState>,
    shutdown: watch::Receiver<bool>,
) {
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

    // Relais : bus/arrêt (async) → event loop (sync). Runtime minimal dédié.
    spawn_wake_relay(event_loop.create_proxy(), state.clone(), shutdown);

    let mut app = OutputApp {
        config,
        state,
        window: None,
        surface: None,
    };
    if let Err(err) = event_loop.run_app(&mut app) {
        error!(%err, "event loop de la fenêtre de sortie terminé en erreur");
    }
    info!("fenêtre de sortie fermée");
}

/// Forwarde chaque changement d'état (et l'arrêt) vers l'event loop.
fn spawn_wake_relay(
    proxy: EventLoopProxy<Wake>,
    mut state: watch::Receiver<NodeState>,
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
                loop {
                    tokio::select! {
                        changed = state.changed() => {
                            if changed.is_err() || proxy.send_event(Wake::StateChanged).is_err() {
                                break;
                            }
                        }
                        _ = shutdown.changed() => {
                            let _ = proxy.send_event(Wake::Quit);
                            break;
                        }
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
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
}

impl OutputApp {
    fn target_monitor(
        &self,
        event_loop: &ActiveEventLoop,
    ) -> Option<winit::monitor::MonitorHandle> {
        let monitors: Vec<_> = event_loop.available_monitors().collect();
        for (i, m) in monitors.iter().enumerate() {
            info!(
                ecran = i,
                nom = m.name().unwrap_or_else(|| "?".into()),
                largeur = m.size().width,
                hauteur = m.size().height,
                "écran détecté"
            );
        }
        if self.config.monitor > 0 && self.config.monitor >= monitors.len() {
            warn!(
                demande = self.config.monitor,
                detectes = monitors.len(),
                "écran demandé introuvable : premier écran utilisé"
            );
        }
        monitors
            .get(self.config.monitor)
            .or_else(|| monitors.first())
            .cloned()
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
                render_frame(&snapshot, w.get(), h.get(), &mut buffer);
                if let Err(err) = buffer.present() {
                    warn!(%err, "frame de sortie non présentée");
                }
            }
            Err(err) => warn!(%err, "tampon de sortie inaccessible"),
        }
    }
}

impl ApplicationHandler<Wake> for OutputApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let monitor = self.target_monitor(event_loop);
        let mut attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(LogicalSize::new(960.0, 540.0));
        if self.config.fullscreen {
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
            Wake::StateChanged => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
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
