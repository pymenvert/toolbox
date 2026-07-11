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

/// `NDIlib_source_t` (Processing.NDI.structs.h) — l'union d'adressage se
/// réduit à un seul pointeur.
#[repr(C)]
struct SourceNdi {
    p_ndi_name: *const c_char,
    p_url_address: *const c_char,
}

/// `NDIlib_recv_create_v3_t` (Processing.NDI.Recv.h).
#[repr(C)]
struct RecvCreateV3 {
    source_to_connect_to: SourceNdi,
    color_format: i32,
    bandwidth: i32,
    allow_video_fields: bool,
    p_ndi_recv_name: *const c_char,
}

/// `NDIlib_recv_color_format_RGBX_RGBA` : le SDK convertit tout en RGBA/X.
const COULEUR_RGBX_RGBA: i32 = 2;
/// `NDIlib_recv_bandwidth_highest` : pleine résolution.
const BANDE_MAX: i32 = 100;
/// `NDIlib_frame_type_video` (retour de capture_v2).
const FRAME_VIDEO: i32 = 1;

type FnInitialize = unsafe extern "C" fn() -> bool;
type FnSendCreate = unsafe extern "C" fn(*const SendCreate) -> *mut c_void;
type FnSendDestroy = unsafe extern "C" fn(*mut c_void);
type FnSendVideoV2 = unsafe extern "C" fn(*mut c_void, *const VideoFrameV2);
type FnRecvCreateV3 = unsafe extern "C" fn(*const RecvCreateV3) -> *mut c_void;
type FnRecvDestroy = unsafe extern "C" fn(*mut c_void);
type FnRecvCaptureV2 = unsafe extern "C" fn(
    *mut c_void,
    *mut VideoFrameV2,
    *mut c_void, // audio ignoré
    *mut c_void, // métadonnées ignorées
    u32,
) -> i32;
type FnRecvFreeVideoV2 = unsafe extern "C" fn(*mut c_void, *const VideoFrameV2);

/// La bibliothèque NDI chargée + les fonctions de l'envoi
/// (`NDIlib_initialize` est appelée une fois au chargement, pas stockée).
pub struct Bibliotheque {
    // L'ordre des champs compte : les pointeurs de fonctions doivent
    // tomber avant la bibliothèque qui les porte.
    send_create: libloading::Symbol<'static, FnSendCreate>,
    send_destroy: libloading::Symbol<'static, FnSendDestroy>,
    send_video: libloading::Symbol<'static, FnSendVideoV2>,
    recv_create: libloading::Symbol<'static, FnRecvCreateV3>,
    recv_destroy: libloading::Symbol<'static, FnRecvDestroy>,
    recv_capture: libloading::Symbol<'static, FnRecvCaptureV2>,
    recv_free_video: libloading::Symbol<'static, FnRecvFreeVideoV2>,
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
        // Un raccourci local : résoudre un symbole ou remonter son nom.
        macro_rules! symbole {
            ($type:ty, $nom:literal) => {
                unsafe { lib.get::<$type>($nom) }.map_err(|e| {
                    format!("{} : {e}", String::from_utf8_lossy(&$nom[..$nom.len() - 1]))
                })?
            };
        }
        let initialize = symbole!(FnInitialize, b"NDIlib_initialize\0");
        let send_create = symbole!(FnSendCreate, b"NDIlib_send_create\0");
        let send_destroy = symbole!(FnSendDestroy, b"NDIlib_send_destroy\0");
        let send_video = symbole!(FnSendVideoV2, b"NDIlib_send_send_video_v2\0");
        let recv_create = symbole!(FnRecvCreateV3, b"NDIlib_recv_create_v3\0");
        let recv_destroy = symbole!(FnRecvDestroy, b"NDIlib_recv_destroy\0");
        let recv_capture = symbole!(FnRecvCaptureV2, b"NDIlib_recv_capture_v2\0");
        let recv_free_video = symbole!(FnRecvFreeVideoV2, b"NDIlib_recv_free_video_v2\0");
        if !unsafe { initialize() } {
            return Err("NDIlib_initialize a échoué (CPU non supporté ?)".into());
        }
        info!(chemin = %chemin.display(), "bibliothèque NDI chargée");
        Ok(Self {
            send_create,
            send_destroy,
            send_video,
            recv_create,
            recv_destroy,
            recv_capture,
            recv_free_video,
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

/// Un récepteur NDI : connecté à une source par son NOM (le SDK découvre
/// l'adresse tout seul, même si la source n'existe pas encore).
pub struct Recepteur {
    bibliotheque: Arc<Bibliotheque>,
    instance: *mut c_void,
    _nom: CString,
}

// SAFETY : même contrat que l'émetteur — l'instance vit dans l'unique
// thread du service d'entrée.
unsafe impl Send for Recepteur {}

impl Recepteur {
    pub fn connecter(bibliotheque: Arc<Bibliotheque>, nom: &str) -> Result<Self, String> {
        let nom_c =
            CString::new(nom).map_err(|_| "nom de source NDI invalide (NUL)".to_string())?;
        let recv_nom = CString::new("Lanterne (entrée)").unwrap_or_default();
        let create = RecvCreateV3 {
            source_to_connect_to: SourceNdi {
                p_ndi_name: nom_c.as_ptr(),
                p_url_address: std::ptr::null(),
            },
            color_format: COULEUR_RGBX_RGBA,
            bandwidth: BANDE_MAX,
            allow_video_fields: false, // toujours progressif côté node
            p_ndi_recv_name: recv_nom.as_ptr(),
        };
        let instance = unsafe { (bibliotheque.recv_create)(&create) };
        if instance.is_null() {
            return Err("NDIlib_recv_create_v3 a retourné NULL".into());
        }
        Ok(Self {
            bibliotheque,
            instance,
            _nom: nom_c,
        })
    }

    /// Attend une frame vidéo (au plus `timeout_ms`) et la copie en RGBA
    /// serré (stride retiré). `None` : rien reçu dans le délai.
    pub fn capturer(&self, timeout_ms: u32) -> Option<VideoFrame> {
        let mut brute: VideoFrameV2 = unsafe { std::mem::zeroed() };
        let genre = unsafe {
            (self.bibliotheque.recv_capture)(
                self.instance,
                &mut brute,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                timeout_ms,
            )
        };
        if genre != FRAME_VIDEO {
            return None;
        }
        let frame = (|| {
            let (largeur, hauteur) = (
                u32::try_from(brute.xres).ok()?,
                u32::try_from(brute.yres).ok()?,
            );
            let stride = usize::try_from(brute.line_stride_in_bytes).ok()?;
            if brute.p_data.is_null() || largeur == 0 || hauteur == 0 {
                return None;
            }
            let ligne_utile = largeur as usize * 4;
            if stride < ligne_utile {
                return None; // format compressé inattendu : ignoré
            }
            let mut rgba = vec![0u8; ligne_utile * hauteur as usize];
            for y in 0..hauteur as usize {
                // SAFETY : p_data + stride décrivent un tampon du SDK valide
                // jusqu'au free_video ci-dessous ; on copie ligne par ligne.
                let src = unsafe {
                    std::slice::from_raw_parts(brute.p_data.add(y * stride), ligne_utile)
                };
                rgba[y * ligne_utile..(y + 1) * ligne_utile].copy_from_slice(src);
            }
            VideoFrame::new(largeur, hauteur, rgba.into())
        })();
        unsafe { (self.bibliotheque.recv_free_video)(self.instance, &brute) };
        frame
    }
}

impl Drop for Recepteur {
    fn drop(&mut self) {
        unsafe { (self.bibliotheque.recv_destroy)(self.instance) };
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

/// Entrée NDI : fait de `ndi://Nom` une vraie source du player. Le service
/// surveille l'état du bus ; dès qu'un média `ndi://` est chargé (et tant
/// que le transport n'est pas à l'arrêt), il se connecte à la source par
/// son nom et pousse ses frames dans le canal vidéo de la fenêtre — pause
/// = image gelée, stop/changement de média = déconnexion.
pub mod entree {
    use super::*;
    use toolbox_core::Transport;

    /// La source NDI à jouer selon l'état (`None` = rien à recevoir).
    fn cible(state: &NodeState) -> Option<String> {
        if state.player.transport == Transport::Stopped {
            return None;
        }
        state
            .player
            .media
            .as_deref()?
            .strip_prefix("ndi://")
            .map(str::to_string)
    }

    pub struct EntreeHandle {
        arret: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl EntreeHandle {
        pub fn arreter(mut self) {
            self.arret.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                if thread.join().is_err() {
                    warn!("thread d'entrée NDI terminé en panique");
                }
            }
        }
    }

    /// Démarre le service (thread dédié). La bibliothèque NDI n'est
    /// chargée qu'à la PREMIÈRE source `ndi://` demandée : un node qui
    /// n'en joue jamais ne paie rien.
    pub fn demarrer(
        state: watch::Receiver<NodeState>,
        video_tx: watch::Sender<Option<VideoFrame>>,
        bibliotheque_chemin: Option<String>,
    ) -> Result<EntreeHandle, String> {
        let arret = Arc::new(AtomicBool::new(false));
        let arret_thread = arret.clone();
        let thread = std::thread::Builder::new()
            .name("toolbox-ndi-entree".into())
            .spawn(move || {
                let mut bibliotheque: Option<Arc<Bibliotheque>> = None;
                let mut lib_en_echec = false;
                let mut connexion: Option<(String, Recepteur)> = None;
                while !arret_thread.load(Ordering::Relaxed) {
                    let etat = state.borrow().clone();
                    let voulu = cible(&etat);
                    // (Dé)connexion quand la cible change.
                    if connexion.as_ref().map(|(nom, _)| nom.as_str()) != voulu.as_deref() {
                        connexion = None; // drop = recv_destroy
                        if let Some(nom) = &voulu {
                            if bibliotheque.is_none() && !lib_en_echec {
                                match Bibliotheque::charger(bibliotheque_chemin.as_deref()) {
                                    Ok(b) => bibliotheque = Some(Arc::new(b)),
                                    Err(err) => {
                                        // Une seule plainte : sans lib, les
                                        // ndi:// resteront noirs.
                                        warn!(%err, "entrée NDI indisponible");
                                        lib_en_echec = true;
                                    }
                                }
                            }
                            if let Some(b) = &bibliotheque {
                                match Recepteur::connecter(b.clone(), nom) {
                                    Ok(r) => {
                                        info!(nom, "entrée NDI connectée");
                                        connexion = Some((nom.clone(), r));
                                    }
                                    Err(err) => warn!(nom, %err, "connexion NDI impossible"),
                                }
                            }
                        }
                    }
                    match (&connexion, etat.player.transport) {
                        (Some((_, recepteur)), Transport::Playing) => {
                            // Bloque au plus 100 ms : la boucle reste
                            // réactive aux changements d'état.
                            if let Some(frame) = recepteur.capturer(100) {
                                let _ = video_tx.send(Some(frame));
                            }
                        }
                        // Connecté mais en pause : frame gelée, on attend.
                        (Some(_), _) => std::thread::sleep(std::time::Duration::from_millis(50)),
                        (None, _) => std::thread::sleep(std::time::Duration::from_millis(200)),
                    }
                }
            })
            .map_err(|e| format!("thread d'entrée NDI : {e}"))?;
        Ok(EntreeHandle {
            arret,
            thread: Some(thread),
        })
    }
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

    /// Boucle locale RÉELLE émetteur → récepteur quand la bibliothèque est
    /// là : la frame envoyée revient par le réseau local (même machine).
    /// Sans SDK (CI) : sauté proprement.
    #[test]
    fn la_boucle_emetteur_recepteur_fonctionne() {
        let bibliotheque = match Bibliotheque::charger(None) {
            Ok(b) => Arc::new(b),
            Err(err) => {
                eprintln!("SDK NDI absent — test sauté ({err})");
                return;
            }
        };
        let emetteur = Emetteur::new(bibliotheque.clone(), "Lanterne (autotest)")
            .expect("création de l'émetteur");
        // Le nom réseau complet est « MACHINE (nom) » : on le reconstruit.
        let machine = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default()
            .to_uppercase();
        let nom_complet = format!("{machine} (Lanterne (autotest))");
        let recepteur =
            Recepteur::connecter(bibliotheque, &nom_complet).expect("création du récepteur");

        // Motif reconnaissable : rouge plein.
        let mut rgba = vec![0u8; 64 * 36 * 4];
        for px in rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&[200, 10, 10, 255]);
        }
        // La découverte + connexion locale prend un peu de temps : on
        // pousse pendant qu'on attend, jusqu'à 10 s.
        let depart = std::time::Instant::now();
        let mut recue = None;
        while recue.is_none() && depart.elapsed() < std::time::Duration::from_secs(10) {
            emetteur.envoyer_rgba(&rgba, 64, 36, 30);
            recue = recepteur.capturer(200);
        }
        let frame = recue.expect("aucune frame reçue en 10 s (pare-feu ?)");
        assert_eq!((frame.width, frame.height), (64, 36));
        // Le rouge domine nettement (le SDK peut recompresser légèrement).
        let (mut r, mut v) = (0u64, 0u64);
        for px in frame.rgba.chunks_exact(4) {
            r += u64::from(px[0]);
            v += u64::from(px[1]);
        }
        assert!(r > v * 5, "frame reçue non conforme (r={r}, v={v})");
    }

    #[test]
    fn les_candidats_respectent_le_chemin_explicite() {
        let liste = candidats(Some("C:/ndi/perso.dll"));
        assert_eq!(liste[0], PathBuf::from("C:/ndi/perso.dll"));
        assert!(liste.len() > 1, "les emplacements standards suivent");
    }
}
