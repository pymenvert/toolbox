//! Sortie NDI : la sortie composée du node (mapping, couleur, LUT,
//! blackout) annoncée sur le réseau comme source NDI « Lanterne » —
//! visible dans OBS, vMix, les moniteurs NDI et les autres nodes.
//!
//! Le SDK NDI (propriétaire NewTek/Vizrt) n'est PAS embarqué : la
//! bibliothèque (`Processing.NDI.Lib.x64.dll` / `libndi.so.6`) est
//! chargée DYNAMIQUEMENT à l'exécution. Sans elle, le service explique
//! où la trouver et le node continue. Les définitions FFI ci-dessous
//! sont recopiées des en-têtes officiels du SDK v6 (ABI stable).
//!
//! Sur Raspberry Pi : le SDK Linux fournit `libndi.so.6` pour aarch64
//! et armhf — la copier dans /usr/local/lib (ou à côté du binaire).

use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::watch;
use tracing::{info, warn};

use toolbox_core::NodeState;
use toolbox_engine::composite::Compositeur;
use toolbox_engine::VideoFrame;

// --- FFI (Processing.NDI.Lib, v6) -------------------------------------------

/// `NDIlib_send_create_t` (Processing.NDI.Send.h).
#[repr(C)]
struct SendCreate {
    p_ndi_name: *const c_char,
    p_groups: *const c_char,
    clock_video: bool,
    clock_audio: bool,
}

/// `NDIlib_video_frame_v2_t` (Processing.NDI.structs.h).
#[repr(C)]
struct VideoFrameV2 {
    xres: i32,
    yres: i32,
    fourcc: u32,
    frame_rate_n: i32,
    frame_rate_d: i32,
    picture_aspect_ratio: f32,
    frame_format_type: i32,
    timecode: i64,
    p_data: *const u8,
    line_stride_in_bytes: i32,
    p_metadata: *const c_char,
    timestamp: i64,
}

/// FourCC 'RGBA' (petit-boutiste, comme NDI_LIB_FOURCC).
const FOURCC_RGBA: u32 =
    (b'R' as u32) | ((b'G' as u32) << 8) | ((b'B' as u32) << 16) | ((b'A' as u32) << 24);
/// `NDIlib_frame_format_type_progressive`.
const PROGRESSIF: i32 = 1;
/// `NDIlib_send_timecode_synthesize` : le SDK fabrique le timecode.
const TIMECODE_AUTO: i64 = i64::MAX;

type FnInitialize = unsafe extern "C" fn() -> bool;
type FnSendCreate = unsafe extern "C" fn(*const SendCreate) -> *mut c_void;
type FnSendDestroy = unsafe extern "C" fn(*mut c_void);
type FnSendVideoV2 = unsafe extern "C" fn(*mut c_void, *const VideoFrameV2);

/// La bibliothèque NDI chargée + les fonctions de l'envoi
/// (`NDIlib_initialize` est appelée une fois au chargement, pas stockée).
pub struct Bibliotheque {
    // L'ordre des champs compte : les pointeurs de fonctions doivent
    // tomber avant la bibliothèque qui les porte.
    send_create: libloading::Symbol<'static, FnSendCreate>,
    send_destroy: libloading::Symbol<'static, FnSendDestroy>,
    send_video: libloading::Symbol<'static, FnSendVideoV2>,
    _lib: &'static libloading::Library,
}

/// Les emplacements où chercher la bibliothèque, du plus explicite au plus
/// générique. `explicite` vient de `[ndi] bibliotheque` dans node.toml.
pub fn candidats(explicite: Option<&str>) -> Vec<PathBuf> {
    let mut liste = Vec::new();
    if let Some(chemin) = explicite {
        liste.push(PathBuf::from(chemin));
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(dir) = std::env::var("NDI_RUNTIME_DIR_V6") {
            liste.push(PathBuf::from(dir).join("Processing.NDI.Lib.x64.dll"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            for sdk in ["NDI 6 SDK", "NDI 6 Runtime", "NDI 5 SDK"] {
                liste.push(
                    PathBuf::from(&pf)
                        .join("NDI")
                        .join(sdk)
                        .join("Bin")
                        .join("x64")
                        .join("Processing.NDI.Lib.x64.dll"),
                );
            }
        }
        // À côté du binaire (pack portable), puis le PATH système.
        liste.push(PathBuf::from("Processing.NDI.Lib.x64.dll"));
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(dir) = std::env::var("NDI_RUNTIME_DIR_V6") {
            liste.push(PathBuf::from(dir).join("libndi.so.6"));
        }
        liste.push(PathBuf::from("/usr/local/lib/libndi.so.6"));
        liste.push(PathBuf::from("/usr/lib/libndi.so.6"));
        liste.push(PathBuf::from("libndi.so.6"));
    }
    liste
}

impl Bibliotheque {
    /// Charge la première bibliothèque trouvée et initialise le SDK.
    pub fn charger(explicite: Option<&str>) -> Result<Self, String> {
        let liste = candidats(explicite);
        let (chemin, lib) = liste
            .iter()
            .find_map(|c| unsafe { libloading::Library::new(c).ok().map(|l| (c.clone(), l)) })
            .ok_or_else(|| {
                format!(
                    "bibliothèque NDI introuvable (essayé : {}) — installer le \
                     runtime NDI ou renseigner [ndi] bibliotheque dans node.toml",
                    liste
                        .iter()
                        .map(|c| c.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        // La bibliothèque vit pour toute la durée du process : les symboles
        // 'static sont sûrs. Elle n'est jamais déchargée (Box::leak).
        let lib: &'static libloading::Library = Box::leak(Box::new(lib));
        let (initialize, send_create, send_destroy, send_video) = unsafe {
            (
                lib.get::<FnInitialize>(b"NDIlib_initialize\0")
                    .map_err(|e| format!("NDIlib_initialize : {e}"))?,
                lib.get::<FnSendCreate>(b"NDIlib_send_create\0")
                    .map_err(|e| format!("NDIlib_send_create : {e}"))?,
                lib.get::<FnSendDestroy>(b"NDIlib_send_destroy\0")
                    .map_err(|e| format!("NDIlib_send_destroy : {e}"))?,
                lib.get::<FnSendVideoV2>(b"NDIlib_send_send_video_v2\0")
                    .map_err(|e| format!("NDIlib_send_send_video_v2 : {e}"))?,
            )
        };
        if !unsafe { initialize() } {
            return Err("NDIlib_initialize a échoué (CPU non supporté ?)".into());
        }
        info!(chemin = %chemin.display(), "bibliothèque NDI chargée");
        Ok(Self {
            send_create,
            send_destroy,
            send_video,
            _lib: lib,
        })
    }
}

/// Un émetteur NDI : une source nommée sur le réseau.
pub struct Emetteur {
    bibliotheque: Arc<Bibliotheque>,
    instance: *mut c_void,
    // Garde la propriété de la chaîne C du nom pendant toute la vie de
    // l'émetteur (le SDK ne la copie pas forcément à la création).
    _nom: CString,
}

// SAFETY : l'instance NDI est utilisable depuis n'importe quel thread tant
// qu'un seul l'utilise à la fois — l'émetteur vit dans l'unique thread du
// service, la poignée n'est jamais partagée.
unsafe impl Send for Emetteur {}

impl Emetteur {
    pub fn new(bibliotheque: Arc<Bibliotheque>, nom: &str) -> Result<Self, String> {
        let nom_c =
            CString::new(nom).map_err(|_| "nom de source NDI invalide (NUL)".to_string())?;
        let create = SendCreate {
            p_ndi_name: nom_c.as_ptr(),
            p_groups: std::ptr::null(),
            // clock_video : le SDK cadence l'envoi au frame rate déclaré —
            // notre boucle peut pousser « au plus vite » sans dériver.
            clock_video: true,
            clock_audio: false,
        };
        let instance = unsafe { (bibliotheque.send_create)(&create) };
        if instance.is_null() {
            return Err("NDIlib_send_create a retourné NULL".into());
        }
        Ok(Self {
            bibliotheque,
            instance,
            _nom: nom_c,
        })
    }

    /// Envoie une frame RGBA (le tampon doit faire `largeur × hauteur × 4`).
    pub fn envoyer_rgba(&self, rgba: &[u8], largeur: u32, hauteur: u32, fps: u32) {
        debug_assert_eq!(rgba.len(), (largeur * hauteur * 4) as usize);
        #[allow(clippy::cast_possible_wrap)] // bornés bien avant i32::MAX
        let frame = VideoFrameV2 {
            xres: largeur as i32,
            yres: hauteur as i32,
            fourcc: FOURCC_RGBA,
            frame_rate_n: fps.max(1) as i32,
            frame_rate_d: 1,
            picture_aspect_ratio: 0.0, // pixels carrés
            frame_format_type: PROGRESSIF,
            timecode: TIMECODE_AUTO,
            p_data: rgba.as_ptr(),
            line_stride_in_bytes: (largeur * 4) as i32,
            p_metadata: std::ptr::null(),
            timestamp: 0,
        };
        unsafe { (self.bibliotheque.send_video)(self.instance, &frame) };
    }
}

impl Drop for Emetteur {
    fn drop(&mut self) {
        unsafe { (self.bibliotheque.send_destroy)(self.instance) };
    }
}

/// Réglages du service (copie de `[ndi]` de node.toml).
#[derive(Debug, Clone)]
pub struct NdiConfig {
    pub nom: String,
    pub largeur: u32,
    pub hauteur: u32,
    pub fps: u32,
    pub bibliotheque: Option<String>,
}

/// Poignée du service : arrêt propre.
pub struct NdiHandle {
    arret: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NdiHandle {
    pub fn arreter(mut self) {
        self.arret.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                warn!("thread NDI terminé en panique");
            }
        }
    }
}

/// Démarre la sortie NDI : charge la bibliothèque, crée la source et pousse
/// la sortie composée en continu (thread dédié, tampons réutilisés).
pub fn demarrer(
    config: NdiConfig,
    state: watch::Receiver<NodeState>,
    video: watch::Receiver<Option<VideoFrame>>,
) -> Result<NdiHandle, String> {
    let bibliotheque = Arc::new(Bibliotheque::charger(config.bibliotheque.as_deref())?);
    let (largeur, hauteur) = (
        config.largeur.clamp(64, 3840),
        config.hauteur.clamp(64, 2160),
    );
    let fps = config.fps.clamp(1, 60);
    let emetteur = Emetteur::new(bibliotheque, &config.nom)?;

    let arret = Arc::new(AtomicBool::new(false));
    let arret_thread = arret.clone();
    let nom = config.nom.clone();
    let thread = std::thread::Builder::new()
        .name("toolbox-ndi".into())
        .spawn(move || {
            let mut compositeur = Compositeur::new(state, video, largeur, hauteur);
            info!(
                nom,
                largeur, hauteur, fps, "sortie NDI annoncée sur le réseau"
            );
            while !arret_thread.load(Ordering::Relaxed) {
                // clock_video=true : le SDK bloque juste ce qu'il faut pour
                // tenir la cadence déclarée — pas de sleep à gérer ici.
                let rgba = compositeur.frame_rgba();
                emetteur.envoyer_rgba(rgba, largeur, hauteur, fps);
            }
            info!("sortie NDI arrêtée");
        })
        .map_err(|e| format!("thread NDI : {e}"))?;

    Ok(NdiHandle {
        arret,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test RÉEL quand la bibliothèque NDI est présente (machine de dev,
    /// SDK installé) : la source s'annonce et une frame part. Sur les
    /// machines sans SDK (CI), le test se saute proprement — le reste de
    /// la crate (candidats, config) reste couvert.
    #[test]
    fn la_source_ndi_s_annonce_si_le_sdk_est_la() {
        let bibliotheque = match Bibliotheque::charger(None) {
            Ok(b) => Arc::new(b),
            Err(err) => {
                eprintln!("SDK NDI absent — test sauté ({err})");
                return;
            }
        };
        let emetteur =
            Emetteur::new(bibliotheque, "Lanterne (test)").expect("création de la source");
        let rgba = vec![128u8; 64 * 36 * 4];
        emetteur.envoyer_rgba(&rgba, 64, 36, 10);
        // Pas de panique ni de fuite : l'émetteur se détruit proprement.
    }

    #[test]
    fn les_candidats_respectent_le_chemin_explicite() {
        let liste = candidats(Some("C:/ndi/perso.dll"));
        assert_eq!(liste[0], PathBuf::from("C:/ndi/perso.dll"));
        assert!(liste.len() > 1, "les emplacements standards suivent");
    }
}
