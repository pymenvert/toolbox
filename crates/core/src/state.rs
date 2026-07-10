//! État du node : ce que le moteur rend à l'écran, entièrement sérialisable.
//!
//! Règles :
//! - Toute mutation passe par [`NodeState::apply`] qui **valide** puis publie
//!   un ou plusieurs [`Event`]. Aucune écriture directe des champs depuis
//!   l'extérieur.
//! - L'état complet est un document JSON → presets, export, clonage gratuits.
//! - Une commande refusée ne modifie JAMAIS l'état (testé).

use serde::{Deserialize, Serialize};

use crate::command::{ColorParam, Command, TestPattern};
use crate::error::CoreError;

/// Un coin du quad de mapping, coordonnées normalisées (0,0 = haut-gauche).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Corner {
    pub x: f32,
    pub y: f32,
}

/// Rotation de la source par quarts de tour (appliquée avant le warp).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    pub fn degrees(self) -> u16 {
        match self {
            Rotation::R0 => 0,
            Rotation::R90 => 90,
            Rotation::R180 => 180,
            Rotation::R270 => 270,
        }
    }

    pub fn from_degrees(degrees: u16) -> Option<Self> {
        match degrees {
            0 => Some(Rotation::R0),
            90 => Some(Rotation::R90),
            180 => Some(Rotation::R180),
            270 => Some(Rotation::R270),
            _ => None,
        }
    }
}

/// Recadrage de la source : fraction rognée sur chaque bord (0.0..=0.45).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CropState {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Fondu de bords (edge blending, V2) : sur chaque bord de la SORTIE, une
/// bande où l'image glisse vers le noir — pour recouvrir deux projecteurs
/// sans sur-brillance. Largeurs en fraction de la sortie (0 = pas de bande),
/// `gamma` compense la réponse du projecteur (2.2 typique).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BlendingState {
    pub gauche: f32,
    pub droite: f32,
    pub haut: f32,
    pub bas: f32,
    pub gamma: f32,
}

impl Default for BlendingState {
    fn default() -> Self {
        Self {
            gauche: 0.0,
            droite: 0.0,
            haut: 0.0,
            bas: 0.0,
            gamma: 2.2,
        }
    }
}

/// Un masque (V2) : un quadrilatère NOIR en espace de sortie (cacher une
/// fenêtre, un relief…). Coordonnées normalisées comme les coins du mapping.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Masque {
    pub corners: [Corner; 4],
}

/// Mapping 4 coins + orientation + recadrage.
/// Ordre des coins : 0=HG, 1=HD, 2=BD, 3=BG.
///
/// Les champs ajoutés après la première version ont des défauts serde :
/// les presets écrits par d'anciennes versions restent chargeables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MappingState {
    /// Mapping actif ? À `false`, le rendu ignore tout le bloc (coins,
    /// rotation, miroirs, recadrage) : image brute plein cadre. Les valeurs
    /// sont conservées et reprennent effet à la réactivation.
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub corners: [Corner; 4],
    #[serde(default)]
    pub rotation: Rotation,
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
    #[serde(default)]
    pub crop: CropState,
}

impl Default for MappingState {
    fn default() -> Self {
        Self {
            enabled: true,
            corners: [
                Corner { x: 0.0, y: 0.0 },
                Corner { x: 1.0, y: 0.0 },
                Corner { x: 1.0, y: 1.0 },
                Corner { x: 0.0, y: 1.0 },
            ],
            rotation: Rotation::R0,
            flip_h: false,
            flip_v: false,
            crop: CropState::default(),
        }
    }
}

impl MappingState {
    /// Invariants du mapping seul (partagés avec la validation de l'état
    /// complet et le chargement d'un preset de mapping).
    pub fn validate(&self) -> Result<(), CoreError> {
        for corner in &self.corners {
            check_range("corner.x", corner.x, -0.5, 1.5)?;
            check_range("corner.y", corner.y, -0.5, 1.5)?;
        }
        check_range("crop.left", self.crop.left, 0.0, 0.45)?;
        check_range("crop.top", self.crop.top, 0.0, 0.45)?;
        check_range("crop.right", self.crop.right, 0.0, 0.45)?;
        check_range("crop.bottom", self.crop.bottom, 0.0, 0.45)?;
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

fn one() -> f32 {
    1.0
}

/// Correction couleur. Valeurs neutres par défaut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorState {
    pub brightness: f32,
    pub contrast: f32,
    pub gamma: f32,
    pub saturation: f32,
    pub hue: f32,
    #[serde(default = "one")]
    pub gain_r: f32,
    #[serde(default = "one")]
    pub gain_g: f32,
    #[serde(default = "one")]
    pub gain_b: f32,
}

impl Default for ColorState {
    fn default() -> Self {
        Self {
            brightness: 1.0,
            contrast: 1.0,
            gamma: 1.0,
            saturation: 1.0,
            hue: 0.0,
            gain_r: 1.0,
            gain_g: 1.0,
            gain_b: 1.0,
        }
    }
}

/// Bornes autorisées par paramètre couleur : (min, max).
/// Public : l'UI, l'OSC et le MIDI mettent leurs valeurs à l'échelle dessus.
pub fn color_bounds(param: ColorParam) -> (f32, f32) {
    match param {
        ColorParam::Brightness
        | ColorParam::Contrast
        | ColorParam::Saturation
        | ColorParam::GainR
        | ColorParam::GainG
        | ColorParam::GainB => (0.0, 2.0),
        ColorParam::Gamma => (0.2, 4.0),
        ColorParam::Hue => (-180.0, 180.0),
    }
}

/// État de transport du player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// Mode de boucle du player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    /// Fin de média : suivant de la playlist, sinon stop.
    #[default]
    Off,
    /// Reboucle le média courant.
    One,
    /// Boucle sur toute la playlist (ou le média seul s'il n'y a pas de playlist).
    All,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    pub transport: Transport,
    /// Média chargé (chemin relatif au dossier `media/`), s'il y en a un.
    pub media: Option<String>,
    #[serde(default)]
    pub loop_mode: LoopMode,
    /// Volume 0.0..=1.0.
    pub volume: f32,
    /// Vitesse de lecture 0.25..=4.0 (1.0 = normale). La synchro multi-node
    /// s'en sert pour les micro-corrections de dérive (±2 %).
    #[serde(default = "vitesse_normale")]
    pub rate: f32,
    /// Playlist : chemins relatifs au dossier `media/`.
    #[serde(default)]
    pub playlist: Vec<String>,
    /// Position dans la playlist si le média courant en provient.
    #[serde(default)]
    pub playlist_index: Option<usize>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            transport: Transport::default(),
            media: None,
            loop_mode: LoopMode::default(),
            // Piège terrain évité : un node fraîchement démarré ne doit pas
            // jouer en silence. Le défaut est plein volume, pas muet.
            volume: 1.0,
            rate: 1.0,
            playlist: Vec::new(),
            playlist_index: None,
        }
    }
}

fn vitesse_normale() -> f32 {
    1.0
}

/// Effets appliqués après la correction couleur (brief 3.3). Chaque effet a
/// une intensité 0..1 (0 = inactif) — pas de booléen séparé : un fader OSC
/// ou MIDI suffit à l'activer et le doser.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EffectsState {
    /// Pixellisation (taille des blocs).
    pub pixelate: f32,
    /// Postérisation (réduction des niveaux).
    pub posterize: f32,
    /// Bruit animé.
    pub noise: f32,
    /// Accentuation de netteté.
    pub sharpen: f32,
    /// Miroir kaléidoscope au centre.
    pub mirror: f32,
}

/// Paramètre d'effet adressable (`/effect/<param>` en OSC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectParam {
    Pixelate,
    Posterize,
    Noise,
    Sharpen,
    Mirror,
}

impl EffectsState {
    pub fn get(&self, param: EffectParam) -> f32 {
        match param {
            EffectParam::Pixelate => self.pixelate,
            EffectParam::Posterize => self.posterize,
            EffectParam::Noise => self.noise,
            EffectParam::Sharpen => self.sharpen,
            EffectParam::Mirror => self.mirror,
        }
    }

    fn set(&mut self, param: EffectParam, value: f32) {
        match param {
            EffectParam::Pixelate => self.pixelate = value,
            EffectParam::Posterize => self.posterize = value,
            EffectParam::Noise => self.noise = value,
            EffectParam::Sharpen => self.sharpen = value,
            EffectParam::Mirror => self.mirror = value,
        }
    }

    /// Tous les invariants (chaque intensité dans 0..1).
    pub fn validate(&self) -> Result<(), CoreError> {
        for (name, value) in [
            ("effect.pixelate", self.pixelate),
            ("effect.posterize", self.posterize),
            ("effect.noise", self.noise),
            ("effect.sharpen", self.sharpen),
            ("effect.mirror", self.mirror),
        ] {
            check_range(name, value, 0.0, 1.0)?;
        }
        Ok(())
    }
}

/// L'état complet du node — LE document que l'on preset/exporte/clone.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NodeState {
    pub player: PlayerState,
    pub mapping: MappingState,
    pub color: ColorState,
    /// Effets (défauts serde : les presets anciens restent chargeables).
    #[serde(default)]
    pub effects: EffectsState,
    /// Fondu de bords multi-projecteurs (défauts serde : anciens presets OK).
    #[serde(default)]
    pub blending: BlendingState,
    /// Masques noirs en espace de sortie (8 max).
    #[serde(default)]
    pub masques: Vec<Masque>,
    /// Mire de test affichée à la place du média (`None` = média normal).
    /// Sauvegardée dans les presets : un preset "réglage VP" est légitime.
    #[serde(default)]
    pub test_pattern: Option<TestPattern>,
}

/// Événement publié après chaque mutation réussie. Les abonnés (web UI, OSC
/// feedback, moteur de rendu, page de logs) reçoivent tous le même flux.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    TransportChanged {
        transport: Transport,
    },
    MediaLoaded {
        path: String,
    },
    Seeked {
        seconds: f64,
    },
    LoopChanged {
        mode: LoopMode,
    },
    VolumeChanged {
        volume: f32,
    },
    /// Vitesse de lecture changée (1.0 = normale). Les micro-corrections de
    /// la synchro passent aussi par là — visibles donc dans les logs.
    RateChanged {
        rate: f32,
    },
    PlaylistChanged {
        items: Vec<String>,
        index: Option<usize>,
    },
    PlaylistPositionChanged {
        index: usize,
        path: String,
    },
    CornerMoved {
        index: u8,
        x: f32,
        y: f32,
    },
    RotationChanged {
        degrees: u16,
    },
    FlipChanged {
        horizontal: bool,
        vertical: bool,
    },
    CropChanged {
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    },
    ColorChanged {
        param: ColorParam,
        value: f32,
    },
    EffectChanged {
        param: EffectParam,
        value: f32,
    },
    MappingReset,
    MappingEnabledChanged {
        enabled: bool,
    },
    /// Un preset de mapping a été sauvegardé sur disque.
    MappingSaved {
        name: String,
    },
    /// Un preset de mapping a remplacé le mapping courant. Contrairement à
    /// `StateReplaced`, le player n'est pas concerné : la lecture continue.
    MappingLoaded {
        name: String,
        mapping: MappingState,
    },
    TestPatternChanged {
        pattern: Option<TestPattern>,
    },
    /// Fondu de bords modifié.
    BlendingChanged {
        blending: BlendingState,
    },
    /// Masque posé ou remplacé.
    MasqueChanged {
        index: u8,
        corners: [Corner; 4],
    },
    /// Masque supprimé.
    MasqueSupprimeEvt {
        index: u8,
    },
    PresetSaved {
        name: String,
    },
    PresetLoaded {
        name: String,
    },
    /// Un fondu vers le preset `name` démarre (durée `seconds`). Le service
    /// fader (abonné) mène l'interpolation ; chaque pas produit ses propres
    /// événements (`CornerMoved`, `ColorChanged`…).
    PresetFadeStarted {
        name: String,
        seconds: f32,
    },
    /// Un fondu vers le preset de mapping `name` démarre (durée `seconds`).
    /// Seul le mapping glisse ; couleur, effets et lecture ne bougent pas.
    MappingFadeStarted {
        name: String,
        seconds: f32,
    },
    /// Départ synchronisé programmé : le player lancera la lecture à `at`
    /// (heure Unix en secondes).
    SyncScheduled {
        at: f64,
    },
    /// L'état complet a été remplacé (chargement de preset, import…).
    /// Les abonnés doivent tout resynchroniser depuis `state`.
    StateReplaced {
        state: Box<NodeState>,
    },
}

/// Valide une source média : fichier relatif à `media/` (sans traversée),
/// URL réseau autorisée, `capture://N` ou `ndi://Nom` — voir
/// [`crate::source::MediaSource`]. Refusé ici = refusé pour TOUTES les
/// interfaces.
pub fn validate_media_path(path: &str) -> Result<(), CoreError> {
    crate::source::MediaSource::parse(path).map(|_| ())
}

impl NodeState {
    /// Applique une commande : valide, mute l'état, retourne les événements.
    ///
    /// Les commandes de preset ne sont PAS traitées ici (elles touchent le
    /// disque) : c'est le rôle du bus, qui porte le [`crate::PresetStore`].
    /// Les recevoir ici est une erreur de câblage.
    pub fn apply(&mut self, command: &Command) -> Result<Vec<Event>, CoreError> {
        match command {
            Command::Play => {
                if self.player.media.is_none() {
                    return Err(CoreError::InvalidCommand("play sans média chargé".into()));
                }
                self.player.transport = Transport::Playing;
                Ok(vec![Event::TransportChanged {
                    transport: Transport::Playing,
                }])
            }
            Command::Pause => {
                if self.player.media.is_none() {
                    return Err(CoreError::InvalidCommand("pause sans média chargé".into()));
                }
                self.player.transport = Transport::Paused;
                Ok(vec![Event::TransportChanged {
                    transport: Transport::Paused,
                }])
            }
            Command::Stop => {
                self.player.transport = Transport::Stopped;
                Ok(vec![Event::TransportChanged {
                    transport: Transport::Stopped,
                }])
            }
            Command::Seek { seconds } => {
                if self.player.media.is_none() {
                    return Err(CoreError::InvalidCommand("seek sans média chargé".into()));
                }
                if !seconds.is_finite() || *seconds < 0.0 {
                    return Err(CoreError::OutOfRange {
                        param: "seek.seconds",
                        value: *seconds,
                        min: 0.0,
                        max: f64::MAX,
                    });
                }
                Ok(vec![Event::Seeked { seconds: *seconds }])
            }
            Command::Load { path } => {
                let path = path.trim();
                validate_media_path(path)?;
                self.player.media = Some(path.to_string());
                // Média chargé hors playlist : la position n'a plus de sens.
                self.player.playlist_index = None;
                Ok(vec![Event::MediaLoaded {
                    path: path.to_string(),
                }])
            }
            Command::SetLoop { mode } => {
                self.player.loop_mode = *mode;
                Ok(vec![Event::LoopChanged { mode: *mode }])
            }
            Command::SetVolume { volume } => {
                check_range("volume", *volume, 0.0, 1.0)?;
                self.player.volume = *volume;
                Ok(vec![Event::VolumeChanged { volume: *volume }])
            }
            Command::SetRate { rate } => {
                check_range("rate", *rate, 0.25, 4.0)?;
                self.player.rate = *rate;
                Ok(vec![Event::RateChanged { rate: *rate }])
            }
            Command::PlaylistSet { items } => {
                for item in items {
                    validate_media_path(item.trim())?;
                }
                self.player.playlist = items.iter().map(|s| s.trim().to_string()).collect();
                self.player.playlist_index = None;
                Ok(vec![Event::PlaylistChanged {
                    items: self.player.playlist.clone(),
                    index: None,
                }])
            }
            Command::PlaylistGo { index } => self.playlist_jump_to(*index),
            Command::PlaylistNext => {
                let len = self.playlist_len_checked()?;
                match self.player.playlist_index {
                    None => self.playlist_jump_to(0),
                    Some(current) => {
                        let next = current + 1;
                        if next < len {
                            self.playlist_jump_to(next)
                        } else if self.player.loop_mode == LoopMode::All {
                            self.playlist_jump_to(0)
                        } else {
                            // Fin de playlist sans boucle : on s'arrête.
                            self.player.transport = Transport::Stopped;
                            Ok(vec![Event::TransportChanged {
                                transport: Transport::Stopped,
                            }])
                        }
                    }
                }
            }
            Command::PlaylistPrev => {
                let len = self.playlist_len_checked()?;
                match self.player.playlist_index {
                    None => self.playlist_jump_to(0),
                    Some(0) => {
                        if self.player.loop_mode == LoopMode::All {
                            self.playlist_jump_to(len - 1)
                        } else {
                            // Début de playlist : on recharge le premier.
                            self.playlist_jump_to(0)
                        }
                    }
                    Some(current) => self.playlist_jump_to(current - 1),
                }
            }
            Command::CornerSet { index, x, y } => {
                let i = usize::from(*index);
                if i >= 4 {
                    return Err(CoreError::InvalidCorner(*index));
                }
                // Marge de 0.5 hors cadre autorisée : utile pour "tirer" un coin
                // au-delà du bord physique du projecteur.
                check_range("corner.x", *x, -0.5, 1.5)?;
                check_range("corner.y", *y, -0.5, 1.5)?;
                self.mapping.corners[i] = Corner { x: *x, y: *y };
                Ok(vec![Event::CornerMoved {
                    index: *index,
                    x: *x,
                    y: *y,
                }])
            }
            Command::SetRotation { degrees } => {
                let rotation =
                    Rotation::from_degrees(*degrees).ok_or(CoreError::InvalidRotation(*degrees))?;
                self.mapping.rotation = rotation;
                Ok(vec![Event::RotationChanged { degrees: *degrees }])
            }
            Command::SetFlip {
                horizontal,
                vertical,
            } => {
                self.mapping.flip_h = *horizontal;
                self.mapping.flip_v = *vertical;
                Ok(vec![Event::FlipChanged {
                    horizontal: *horizontal,
                    vertical: *vertical,
                }])
            }
            Command::SetCrop {
                left,
                top,
                right,
                bottom,
            } => {
                check_range("crop.left", *left, 0.0, 0.45)?;
                check_range("crop.top", *top, 0.0, 0.45)?;
                check_range("crop.right", *right, 0.0, 0.45)?;
                check_range("crop.bottom", *bottom, 0.0, 0.45)?;
                self.mapping.crop = CropState {
                    left: *left,
                    top: *top,
                    right: *right,
                    bottom: *bottom,
                };
                Ok(vec![Event::CropChanged {
                    left: *left,
                    top: *top,
                    right: *right,
                    bottom: *bottom,
                }])
            }
            Command::ColorSet { param, value } => {
                let (min, max) = color_bounds(*param);
                check_range("color", *value, min, max)?;
                match param {
                    ColorParam::Brightness => self.color.brightness = *value,
                    ColorParam::Contrast => self.color.contrast = *value,
                    ColorParam::Gamma => self.color.gamma = *value,
                    ColorParam::Saturation => self.color.saturation = *value,
                    ColorParam::Hue => self.color.hue = *value,
                    ColorParam::GainR => self.color.gain_r = *value,
                    ColorParam::GainG => self.color.gain_g = *value,
                    ColorParam::GainB => self.color.gain_b = *value,
                }
                Ok(vec![Event::ColorChanged {
                    param: *param,
                    value: *value,
                }])
            }
            Command::EffectSet { param, value } => {
                check_range("effect", *value, 0.0, 1.0)?;
                self.effects.set(*param, *value);
                Ok(vec![Event::EffectChanged {
                    param: *param,
                    value: *value,
                }])
            }
            Command::BlendingSet {
                gauche,
                droite,
                haut,
                bas,
                gamma,
            } => {
                for (nom, valeur) in [
                    ("blending.gauche", *gauche),
                    ("blending.droite", *droite),
                    ("blending.haut", *haut),
                    ("blending.bas", *bas),
                ] {
                    check_range(nom, valeur, 0.0, 0.45)?;
                }
                check_range("blending.gamma", *gamma, 0.5, 4.0)?;
                self.blending = BlendingState {
                    gauche: *gauche,
                    droite: *droite,
                    haut: *haut,
                    bas: *bas,
                    gamma: *gamma,
                };
                Ok(vec![Event::BlendingChanged {
                    blending: self.blending,
                }])
            }
            Command::MasqueSet { index, corners } => {
                for corner in corners {
                    check_range("masque.x", corner.x, -0.5, 1.5)?;
                    check_range("masque.y", corner.y, -0.5, 1.5)?;
                }
                let index = usize::from(*index);
                if index < self.masques.len() {
                    self.masques[index].corners = *corners;
                } else if index == self.masques.len() && index < 8 {
                    self.masques.push(Masque { corners: *corners });
                } else {
                    return Err(CoreError::InvalidCommand(format!(
                        "index de masque invalide : {index} ({} masques, 8 max)",
                        self.masques.len()
                    )));
                }
                #[allow(clippy::cast_possible_truncation)] // < 8
                Ok(vec![Event::MasqueChanged {
                    index: index as u8,
                    corners: *corners,
                }])
            }
            Command::MasqueSupprime { index } => {
                let i = usize::from(*index);
                if i >= self.masques.len() {
                    return Err(CoreError::InvalidCommand(format!("masque {i} inexistant")));
                }
                self.masques.remove(i);
                Ok(vec![Event::MasqueSupprimeEvt { index: *index }])
            }
            Command::MappingReset => {
                self.mapping = MappingState::default();
                Ok(vec![Event::MappingReset])
            }
            Command::SetMappingEnabled { enabled } => {
                self.mapping.enabled = *enabled;
                Ok(vec![Event::MappingEnabledChanged { enabled: *enabled }])
            }
            Command::SetTestPattern { pattern } => {
                self.test_pattern = *pattern;
                Ok(vec![Event::TestPatternChanged { pattern: *pattern }])
            }
            Command::SyncArm => {
                if self.player.media.is_none() {
                    return Err(CoreError::InvalidCommand(
                        "sync/arm sans média chargé".into(),
                    ));
                }
                // Armé = prêt à partir : position 0, en pause, préchargé.
                self.player.transport = Transport::Paused;
                Ok(vec![
                    Event::Seeked { seconds: 0.0 },
                    Event::TransportChanged {
                        transport: Transport::Paused,
                    },
                ])
            }
            Command::SyncStartAt { at } => {
                if self.player.media.is_none() {
                    return Err(CoreError::InvalidCommand(
                        "sync/startAt sans média chargé".into(),
                    ));
                }
                if !at.is_finite() || *at < 0.0 {
                    return Err(CoreError::OutOfRange {
                        param: "sync.at",
                        value: *at,
                        min: 0.0,
                        max: f64::MAX,
                    });
                }
                // Le player (abonné) programme le départ ; l'état ne bouge
                // pas encore — le TransportChanged viendra du Play planifié.
                Ok(vec![Event::SyncScheduled { at: *at }])
            }
            Command::PresetSave { .. }
            | Command::PresetLoad { .. }
            | Command::PresetFade { .. }
            | Command::MappingSave { .. }
            | Command::MappingLoad { .. }
            | Command::MappingFade { .. } => Err(CoreError::InvalidCommand(
                "les presets sont gérés par le bus (stores sur disque), pas par l'état".into(),
            )),
        }
    }

    fn playlist_len_checked(&self) -> Result<usize, CoreError> {
        let len = self.player.playlist.len();
        if len == 0 {
            return Err(CoreError::InvalidCommand("playlist vide".into()));
        }
        Ok(len)
    }

    /// Charge l'élément `index` de la playlist comme média courant.
    fn playlist_jump_to(&mut self, index: usize) -> Result<Vec<Event>, CoreError> {
        let len = self.playlist_len_checked()?;
        let Some(path) = self.player.playlist.get(index) else {
            return Err(CoreError::InvalidCommand(format!(
                "index de playlist invalide : {index} (playlist de {len} éléments)"
            )));
        };
        let path = path.clone();
        self.player.media = Some(path.clone());
        self.player.playlist_index = Some(index);
        Ok(vec![
            Event::MediaLoaded { path: path.clone() },
            Event::PlaylistPositionChanged { index, path },
        ])
    }

    /// Vérifie tous les invariants de l'état. Utilisé avant d'accepter un
    /// état venu de l'extérieur (preset JSON édité à la main, import réseau) :
    /// un fichier trafiqué ou corrompu ne doit jamais devenir l'état du node.
    pub fn validate(&self) -> Result<(), CoreError> {
        check_range("player.volume", self.player.volume, 0.0, 1.0)?;
        check_range("player.rate", self.player.rate, 0.25, 4.0)?;
        for (nom, valeur) in [
            ("blending.gauche", self.blending.gauche),
            ("blending.droite", self.blending.droite),
            ("blending.haut", self.blending.haut),
            ("blending.bas", self.blending.bas),
        ] {
            check_range(nom, valeur, 0.0, 0.45)?;
        }
        check_range("blending.gamma", self.blending.gamma, 0.5, 4.0)?;
        if self.masques.len() > 8 {
            return Err(CoreError::InvalidCommand(format!(
                "trop de masques : {} (8 max)",
                self.masques.len()
            )));
        }
        for masque in &self.masques {
            for corner in &masque.corners {
                check_range("masque.x", corner.x, -0.5, 1.5)?;
                check_range("masque.y", corner.y, -0.5, 1.5)?;
            }
        }
        if let Some(media) = &self.player.media {
            validate_media_path(media)?;
        }
        for item in &self.player.playlist {
            validate_media_path(item)?;
        }
        if let Some(index) = self.player.playlist_index {
            if index >= self.player.playlist.len() {
                return Err(CoreError::InvalidCommand(format!(
                    "playlist_index {index} hors de la playlist ({} éléments)",
                    self.player.playlist.len()
                )));
            }
        }
        self.mapping.validate()?;
        self.effects.validate()?;
        for (param, value) in [
            (ColorParam::Brightness, self.color.brightness),
            (ColorParam::Contrast, self.color.contrast),
            (ColorParam::Gamma, self.color.gamma),
            (ColorParam::Saturation, self.color.saturation),
            (ColorParam::Hue, self.color.hue),
            (ColorParam::GainR, self.color.gain_r),
            (ColorParam::GainG, self.color.gain_g),
            (ColorParam::GainB, self.color.gain_b),
        ] {
            let (min, max) = color_bounds(param);
            check_range("color", value, min, max)?;
        }
        Ok(())
    }
}

fn check_range(param: &'static str, value: f32, min: f32, max: f32) -> Result<(), CoreError> {
    if !value.is_finite() || value < min || value > max {
        return Err(CoreError::OutOfRange {
            param,
            value: f64::from(value),
            min: f64::from(min),
            max: f64::from(max),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(s: &mut NodeState, path: &str) {
        s.apply(&Command::Load { path: path.into() }).expect("load");
    }

    #[test]
    fn play_without_media_is_rejected() {
        let mut s = NodeState::default();
        assert!(s.apply(&Command::Play).is_err());
        assert_eq!(s.player.transport, Transport::Stopped);
    }

    #[test]
    fn pause_without_media_is_rejected() {
        let mut s = NodeState::default();
        assert!(s.apply(&Command::Pause).is_err());
        assert_eq!(s.player.transport, Transport::Stopped);
    }

    #[test]
    fn load_then_play() {
        let mut s = NodeState::default();
        load(&mut s, "media/a.mp4");
        let ev = s.apply(&Command::Play).expect("play");
        assert_eq!(
            ev,
            vec![Event::TransportChanged {
                transport: Transport::Playing
            }]
        );
    }

    #[test]
    fn corner_validation() {
        let mut s = NodeState::default();
        // index hors bornes
        assert!(matches!(
            s.apply(&Command::CornerSet {
                index: 4,
                x: 0.5,
                y: 0.5
            }),
            Err(CoreError::InvalidCorner(4))
        ));
        // NaN refusé
        assert!(s
            .apply(&Command::CornerSet {
                index: 0,
                x: f32::NAN,
                y: 0.5
            })
            .is_err());
        // marge hors cadre OK
        s.apply(&Command::CornerSet {
            index: 0,
            x: -0.2,
            y: 1.3,
        })
        .expect("corner in margin");
        assert_eq!(s.mapping.corners[0], Corner { x: -0.2, y: 1.3 });
        // état inchangé sur erreur
        let before = s.clone();
        assert!(s
            .apply(&Command::CornerSet {
                index: 1,
                x: 9.0,
                y: 0.0
            })
            .is_err());
        assert_eq!(s, before);
    }

    #[test]
    fn color_bounds_enforced() {
        let mut s = NodeState::default();
        assert!(s
            .apply(&Command::ColorSet {
                param: ColorParam::Gamma,
                value: 0.0
            })
            .is_err());
        s.apply(&Command::ColorSet {
            param: ColorParam::Hue,
            value: -90.0,
        })
        .expect("hue");
        assert_eq!(s.color.hue, -90.0);
        s.apply(&Command::ColorSet {
            param: ColorParam::GainB,
            value: 1.4,
        })
        .expect("gain b");
        assert_eq!(s.color.gain_b, 1.4);
    }

    #[test]
    fn rotation_and_flip() {
        let mut s = NodeState::default();
        assert!(matches!(
            s.apply(&Command::SetRotation { degrees: 45 }),
            Err(CoreError::InvalidRotation(45))
        ));
        s.apply(&Command::SetRotation { degrees: 270 })
            .expect("rotation");
        assert_eq!(s.mapping.rotation, Rotation::R270);
        s.apply(&Command::SetFlip {
            horizontal: true,
            vertical: true,
        })
        .expect("flip");
        assert!(s.mapping.flip_h && s.mapping.flip_v);
        // MappingReset remet aussi rotation/flip/crop à zéro.
        s.apply(&Command::MappingReset).expect("reset");
        assert_eq!(s.mapping, MappingState::default());
    }

    #[test]
    fn crop_bounds_enforced() {
        let mut s = NodeState::default();
        assert!(s
            .apply(&Command::SetCrop {
                left: 0.5,
                top: 0.0,
                right: 0.0,
                bottom: 0.0
            })
            .is_err());
        s.apply(&Command::SetCrop {
            left: 0.1,
            top: 0.0,
            right: 0.45,
            bottom: 0.2,
        })
        .expect("crop");
        assert_eq!(s.mapping.crop.right, 0.45);
    }

    #[test]
    fn playlist_navigation_and_loop_modes() {
        let mut s = NodeState::default();
        // Playlist vide : refus.
        assert!(s.apply(&Command::PlaylistNext).is_err());

        s.apply(&Command::PlaylistSet {
            items: vec!["a.mp4".into(), "b.mp4".into(), "c.mp4".into()],
        })
        .expect("set");
        assert_eq!(s.player.playlist.len(), 3);
        assert_eq!(s.player.playlist_index, None);

        // Next sans position → premier élément, deux événements.
        let evs = s.apply(&Command::PlaylistNext).expect("next");
        assert_eq!(
            evs,
            vec![
                Event::MediaLoaded {
                    path: "a.mp4".into()
                },
                Event::PlaylistPositionChanged {
                    index: 0,
                    path: "a.mp4".into()
                },
            ]
        );

        s.apply(&Command::PlaylistNext).expect("next 1");
        s.apply(&Command::PlaylistNext).expect("next 2");
        assert_eq!(s.player.playlist_index, Some(2));

        // Fin de playlist, loop off : stop, position conservée.
        s.apply(&Command::Play).expect("play");
        let evs = s.apply(&Command::PlaylistNext).expect("fin");
        assert_eq!(
            evs,
            vec![Event::TransportChanged {
                transport: Transport::Stopped
            }]
        );
        assert_eq!(s.player.playlist_index, Some(2));

        // loop all : la fin reboucle au début.
        s.apply(&Command::SetLoop {
            mode: LoopMode::All,
        })
        .expect("loop all");
        let evs = s.apply(&Command::PlaylistNext).expect("wrap");
        assert_eq!(s.player.playlist_index, Some(0));
        assert_eq!(evs.len(), 2);

        // prev depuis 0 en loop all : dernier élément.
        s.apply(&Command::PlaylistPrev).expect("prev wrap");
        assert_eq!(s.player.playlist_index, Some(2));

        // go direct + hors bornes.
        s.apply(&Command::PlaylistGo { index: 1 }).expect("go");
        assert_eq!(s.player.media.as_deref(), Some("b.mp4"));
        assert!(s.apply(&Command::PlaylistGo { index: 9 }).is_err());

        // Un Load direct sort de la playlist.
        load(&mut s, "hors_playlist.mp4");
        assert_eq!(s.player.playlist_index, None);
        assert_eq!(s.player.playlist.len(), 3);
    }

    #[test]
    fn playlist_rejects_bad_paths() {
        let mut s = NodeState::default();
        assert!(s
            .apply(&Command::PlaylistSet {
                items: vec!["ok.mp4".into(), "../evil.mp4".into()],
            })
            .is_err());
        assert!(s.player.playlist.is_empty());
    }

    #[test]
    fn malicious_media_paths_are_rejected() {
        let mut s = NodeState::default();
        for bad in [
            "",
            "   ",
            "/etc/passwd",
            "\\\\serveur\\partage\\x.mp4",
            "C:\\Windows\\system32\\evil.mp4",
            "c:/x.mp4",
            "../secret.mp4",
            "sub/../../secret.mp4",
            "sub\\..\\..\\secret.mp4",
            "nul\0byte.mp4",
        ] {
            assert!(
                s.apply(&Command::Load { path: bad.into() }).is_err(),
                "aurait dû refuser {bad:?}"
            );
            assert_eq!(s.player.media, None, "état modifié par {bad:?}");
        }
        // Les chemins relatifs sains passent, espaces de bord tolérés.
        s.apply(&Command::Load {
            path: "  clips/boucle_01.mp4 ".into(),
        })
        .expect("chemin sain");
        assert_eq!(s.player.media.as_deref(), Some("clips/boucle_01.mp4"));
    }

    #[test]
    fn effects_validate_and_apply() {
        let mut s = NodeState::default();
        assert_eq!(s.effects, EffectsState::default(), "tout à zéro par défaut");
        let evs = s
            .apply(&Command::EffectSet {
                param: EffectParam::Pixelate,
                value: 0.5,
            })
            .expect("effet");
        assert_eq!(
            evs,
            vec![Event::EffectChanged {
                param: EffectParam::Pixelate,
                value: 0.5
            }]
        );
        assert_eq!(s.effects.pixelate, 0.5);
        // Hors bornes ou NaN : refusé, état intact.
        assert!(s
            .apply(&Command::EffectSet {
                param: EffectParam::Noise,
                value: 1.5
            })
            .is_err());
        assert!(s
            .apply(&Command::EffectSet {
                param: EffectParam::Mirror,
                value: f32::NAN
            })
            .is_err());
        assert_eq!(s.effects.noise, 0.0);
        // La validation d'un preset trafiqué attrape les effets aussi.
        let mut t = NodeState::default();
        t.effects.sharpen = 7.0;
        assert!(t.validate().is_err());
    }

    #[test]
    fn sync_commands_validate_and_schedule() {
        let mut s = NodeState::default();
        assert!(s.apply(&Command::SyncArm).is_err(), "arm sans média");
        assert!(s.apply(&Command::SyncStartAt { at: 1.0 }).is_err());

        load(&mut s, "a.mp4");
        let evs = s.apply(&Command::SyncArm).expect("arm");
        assert_eq!(s.player.transport, Transport::Paused);
        assert_eq!(
            evs,
            vec![
                Event::Seeked { seconds: 0.0 },
                Event::TransportChanged {
                    transport: Transport::Paused
                },
            ]
        );

        let evs = s.apply(&Command::SyncStartAt { at: 123.5 }).expect("start");
        assert_eq!(evs, vec![Event::SyncScheduled { at: 123.5 }]);
        // L'état ne bouge qu'au départ effectif (Play planifié par le player).
        assert_eq!(s.player.transport, Transport::Paused);
        assert!(s.apply(&Command::SyncStartAt { at: f64::NAN }).is_err());
        assert!(s.apply(&Command::SyncStartAt { at: -5.0 }).is_err());
    }

    #[test]
    fn network_capture_and_ndi_sources_are_accepted() {
        let mut s = NodeState::default();
        for src in ["rtsp://10.0.0.5:8554/cam", "capture://0", "ndi://Régie"] {
            s.apply(&Command::Load {
                path: (*src).into(),
            })
            .expect("source");
            assert_eq!(s.player.media.as_deref(), Some(src));
        }
        // file:// contournerait la validation des chemins : refusé.
        assert!(s
            .apply(&Command::Load {
                path: "file:///etc/passwd".into()
            })
            .is_err());
    }

    #[test]
    fn state_json_roundtrip() {
        let mut s = NodeState::default();
        load(&mut s, "media/a.mp4");
        s.apply(&Command::CornerSet {
            index: 2,
            x: 0.9,
            y: 0.95,
        })
        .expect("corner");
        s.apply(&Command::SetTestPattern {
            pattern: Some(TestPattern::Grid),
        })
        .expect("pattern");
        let json = serde_json::to_string_pretty(&s).expect("serialize");
        let back: NodeState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }

    /// Un preset écrit par la toute première version (sans rotation, crop,
    /// playlist, gains…) doit rester chargeable : défauts serde partout.
    #[test]
    fn legacy_preset_json_still_loads() {
        let legacy = r#"{
            "player": { "transport": "stopped", "media": "a.mp4", "volume": 0.8 },
            "mapping": { "corners": [
                {"x": 0.0, "y": 0.0}, {"x": 1.0, "y": 0.0},
                {"x": 1.0, "y": 1.0}, {"x": 0.0, "y": 1.0} ] },
            "color": { "brightness": 1.0, "contrast": 1.0, "gamma": 1.0,
                       "saturation": 1.0, "hue": 0.0 }
        }"#;
        let s: NodeState = serde_json::from_str(legacy).expect("legacy");
        assert_eq!(s.player.loop_mode, LoopMode::Off);
        assert_eq!(s.mapping.rotation, Rotation::R0);
        // Champ ajouté après coup : un preset ancien reste actif par défaut.
        assert!(s.mapping.enabled);
        assert_eq!(s.color.gain_r, 1.0);
        assert_eq!(s.test_pattern, None);
        s.validate().expect("valide");
    }

    #[test]
    fn presets_rejected_by_state() {
        let mut s = NodeState::default();
        assert!(s.apply(&Command::PresetSave { name: "x".into() }).is_err());
        assert!(s.apply(&Command::MappingSave { name: "x".into() }).is_err());
        assert!(s.apply(&Command::MappingLoad { name: "x".into() }).is_err());
    }

    #[test]
    fn mapping_enabled_toggles_and_survives_reset() {
        let mut s = NodeState::default();
        assert!(s.mapping.enabled, "actif par défaut");
        let ev = s
            .apply(&Command::SetMappingEnabled { enabled: false })
            .expect("disable");
        assert_eq!(ev, vec![Event::MappingEnabledChanged { enabled: false }]);
        assert!(!s.mapping.enabled);
        // Les réglages sont conservés pendant la désactivation.
        s.apply(&Command::SetRotation { degrees: 90 })
            .expect("rotation");
        assert_eq!(s.mapping.rotation, Rotation::R90);
        // Le reset du mapping réactive (état par défaut complet).
        s.apply(&Command::MappingReset).expect("reset");
        assert!(s.mapping.enabled);
    }

    #[test]
    fn validate_accepts_default_and_normal_states() {
        let mut s = NodeState::default();
        s.validate().expect("état par défaut valide");
        load(&mut s, "a.mp4");
        s.apply(&Command::CornerSet {
            index: 0,
            x: -0.4,
            y: 1.5,
        })
        .expect("corner");
        s.validate().expect("état modifié valide");
    }

    #[test]
    fn validate_rejects_tampered_states() {
        // Un preset JSON édité à la main avec des valeurs hors bornes.
        let mut s = NodeState::default();
        s.player.volume = 9.0;
        assert!(s.validate().is_err());

        let mut s = NodeState::default();
        s.mapping.corners[2].x = 42.0;
        assert!(s.validate().is_err());

        let mut s = NodeState::default();
        s.color.gamma = 0.0;
        assert!(s.validate().is_err());

        let mut s = NodeState::default();
        s.player.media = Some("../../etc/shadow".into());
        assert!(s.validate().is_err());

        let mut s = NodeState::default();
        s.player.playlist = vec!["a.mp4".into()];
        s.player.playlist_index = Some(3);
        assert!(s.validate().is_err());

        let mut s = NodeState::default();
        s.mapping.crop.left = 0.5;
        assert!(s.validate().is_err());
    }

    #[test]
    fn state_replaced_event_serializes_with_full_state() {
        let ev = Event::StateReplaced {
            state: Box::new(NodeState::default()),
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(json.starts_with(r#"{"event":"state_replaced","state":{"#));
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ev);
    }
}
