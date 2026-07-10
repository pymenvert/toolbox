//! Rendu CPU d'une frame de sortie depuis l'état du node.
//!
//! Chaîne par pixel de sortie `(u, v)` — la même que documentée dans
//! `toolbox_engine::render` (le futur shader GL appliquera exactement ceci) :
//! 1. warp inverse : sortie → quad unité, hors quad = noir ;
//! 2. `uv_transform` (flip + rotation inverse + recadrage) : quad → texture ;
//! 3. échantillonnage de la source — ici une mire procédurale, plus tard la
//!    frame vidéo ;
//! 4. correction couleur (gains RVB, luminosité, contraste, gamma,
//!    saturation, teinte).
//!
//! La source échantillonnée est, par priorité :
//! 1. la mire de test si une est sélectionnée (le calibrage prime) ;
//! 2. la dernière frame vidéo décodée, si un média est chargé et que le
//!    transport n'est pas à l'arrêt ;
//! 3. sinon noir — un vidéoprojecteur de spectacle n'affiche rien par défaut.

use crate::{Mat3, RenderParams, VideoFrame};
use toolbox_core::command::TestPattern;
use toolbox_core::state::{EffectsState, NodeState, Transport};
use tracing::warn;

/// Ce que le pixel échantillonne : mire procédurale ou frame vidéo.
enum Source<'a> {
    Pattern(TestPattern),
    Video(&'a VideoFrame),
}

/// Rend une frame `width`×`height` dans `out` (format softbuffer `0RGB`,
/// une entrée `u32` par pixel, lignes de haut en bas).
///
/// `out` est retaillé par l'appelant : la fonction ne panique jamais, elle
/// s'arrête à `out.len()`.
pub fn render_frame(
    state: &NodeState,
    video: Option<&VideoFrame>,
    time: f32,
    width: u32,
    height: u32,
    out: &mut [u32],
) {
    let source = match (state.test_pattern, video) {
        (Some(pattern), _) => Source::Pattern(pattern),
        (None, Some(frame)) if state.player.transport != Transport::Stopped => Source::Video(frame),
        // Ni mire ni vidéo en cours : noir immédiat (cas nominal en show).
        _ => {
            out.fill(0);
            return;
        }
    };
    let params = match RenderParams::from_state(state) {
        Ok(params) => params,
        Err(err) => {
            // Mapping dégénéré (coins alignés) : noir plutôt que panique.
            warn!(%err, "paramètres de rendu indisponibles — sortie noire");
            out.fill(0);
            return;
        }
    };
    // `RenderParams` publie l'inverse en f32 colonne-major pour GL ; ici on
    // ré-inverse le warp f64 (même source de vérité, précision maximale).
    let Some(warp_inv) = params.warp.inverse() else {
        out.fill(0);
        return;
    };

    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let effects = state.effects;
    let blending = state.blending;
    let masques = &state.masques;
    // Parallélisé par lignes (rayon) : chaque pixel est indépendant — le
    // repli CPU et l'aperçu web gagnent ~un facteur nb-de-cœurs.
    use rayon::prelude::*;
    out.par_chunks_mut(w)
        .take(h)
        .enumerate()
        .for_each(|(y, ligne)| {
            let v = (y as f64 + 0.5) / h as f64;
            for (x, px) in ligne.iter_mut().enumerate() {
                let u = (x as f64 + 0.5) / w as f64;
                // Masques : zones NOIRES en espace de sortie, avant tout calcul.
                if dans_un_masque(masques, u, v) {
                    *px = 0;
                    continue;
                }
                let couleur = shade(&source, &warp_inv, &params, &effects, time, u, v);
                // Fondu de bords : atténuation finale en espace de sortie.
                *px = appliquer_blending(&blending, couleur, u, v);
            }
        });
}

/// Le pixel (u, v) est-il couvert par un masque ? Test « même côté » sur les
/// quatre arêtes du quadrilatère (convexe, sens libre). MÊME formule que
/// `warp.wgsl` — toute divergence est un bug.
pub fn dans_un_masque(masques: &[toolbox_core::Masque], u: f64, v: f64) -> bool {
    masques.iter().any(|masque| {
        let c = &masque.corners;
        let mut positifs = 0;
        let mut negatifs = 0;
        for i in 0..4 {
            let a = c[i];
            let b = c[(i + 1) % 4];
            let croix = (f64::from(b.x) - f64::from(a.x)) * (v - f64::from(a.y))
                - (f64::from(b.y) - f64::from(a.y)) * (u - f64::from(a.x));
            if croix >= 0.0 {
                positifs += 1;
            }
            if croix <= 0.0 {
                negatifs += 1;
            }
        }
        positifs == 4 || negatifs == 4
    })
}

/// Facteur de fondu de bords au pixel (u, v) : produit des rampes de chaque
/// bord actif, corrigées gamma. 1.0 = plein, 0.0 = noir au ras du bord.
/// MÊME formule que `warp.wgsl`.
pub fn facteur_blending(blending: &toolbox_core::BlendingState, u: f64, v: f64) -> f64 {
    let gamma = f64::from(blending.gamma.max(0.5));
    let mut facteur = 1.0_f64;
    let rampe = |distance: f64, largeur: f32| -> f64 {
        let largeur = f64::from(largeur);
        if largeur <= 0.0 || distance >= largeur {
            1.0
        } else {
            (distance.max(0.0) / largeur).powf(gamma)
        }
    };
    facteur *= rampe(u, blending.gauche);
    facteur *= rampe(1.0 - u, blending.droite);
    facteur *= rampe(v, blending.haut);
    facteur *= rampe(1.0 - v, blending.bas);
    facteur
}

/// Applique le fondu de bords à un pixel packé `0RGB`.
fn appliquer_blending(blending: &toolbox_core::BlendingState, pixel: u32, u: f64, v: f64) -> u32 {
    if blending.gauche <= 0.0
        && blending.droite <= 0.0
        && blending.haut <= 0.0
        && blending.bas <= 0.0
    {
        return pixel;
    }
    let facteur = facteur_blending(blending, u, v);
    if facteur >= 1.0 {
        return pixel;
    }
    let canal = |decalage: u32| -> u32 {
        let brut = f64::from((pixel >> decalage) & 0xFF);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            ((brut * facteur).round() as u32).min(255)
        }
    };
    (canal(16) << 16) | (canal(8) << 8) | canal(0)
}

/// Niveau courant d'une rampe de blackout : progresse linéairement de
/// `depart` vers `cible` en `fondu_ms`. Pure — l'appelant fournit le temps
/// écoulé depuis le changement de consigne. `fondu_ms == 0` : saut direct.
pub fn niveau_rampe(cible: f32, depart: f32, ecoule_ms: u64, fondu_ms: u64) -> f32 {
    if fondu_ms == 0 || ecoule_ms >= fondu_ms {
        return cible;
    }
    #[allow(clippy::cast_precision_loss)] // bornés à 10 000 ms
    let t = (ecoule_ms as f32 / fondu_ms as f32).clamp(0.0, 1.0);
    depart + (cible - depart) * t
}

/// Voile noir de régie sur un buffer `0RGB` : chaque canal est multiplié
/// par `1 - niveau` (0 = intact, 1 = noir). MÊME formule que `warp.wgsl`.
pub fn appliquer_blackout(out: &mut [u32], niveau: f32) {
    let niveau = niveau.clamp(0.0, 1.0);
    if niveau <= 0.0 {
        return;
    }
    if niveau >= 1.0 {
        out.fill(0);
        return;
    }
    let garde = f64::from(1.0 - niveau);
    for px in out.iter_mut() {
        let canal = |decalage: u32| -> u32 {
            let brut = f64::from((*px >> decalage) & 0xFF);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                ((brut * garde).round() as u32).min(255)
            }
        };
        *px = (canal(16) << 16) | (canal(8) << 8) | canal(0);
    }
}

/// Couleur d'un pixel de sortie, packée en `0RGB`.
fn shade(
    source: &Source<'_>,
    warp_inv: &Mat3,
    params: &RenderParams,
    effects: &EffectsState,
    time: f32,
    u: f64,
    v: f64,
) -> u32 {
    // 1. Warp inverse : hors du quad de mapping, rien n'est projeté.
    let (qu, qv) = warp_inv.apply(u, v);
    if !(0.0..=1.0).contains(&qu) || !(0.0..=1.0).contains(&qv) {
        return 0;
    }
    // 2. Flip + rotation inverse + recadrage.
    let (tu, tv) = params.uv_transform.apply(qu, qv);
    if !(0.0..=1.0).contains(&tu) || !(0.0..=1.0).contains(&tv) {
        return 0;
    }
    // 3. Effets géométriques (miroir, pixellisation), puis échantillonnage
    //    (avec accentuation à 5 prélèvements si demandée).
    let (tu, tv) = apply_uv_effects(effects, tu, tv);
    let rgb = if effects.sharpen > 0.0 {
        sharpen_sample(source, effects.sharpen, tu, tv)
    } else {
        sample_source(source, tu, tv)
    };
    // 4. Correction couleur puis effets de teinte (postérisation, bruit).
    let rgb = apply_color(&params.color, rgb);
    let rgb = apply_pixel_effects(effects, rgb, tu, tv, time);
    pack(rgb)
}

/// Échantillonne la source au point (u, v) — mire procédurale ou vidéo.
fn sample_source(source: &Source<'_>, u: f64, v: f64) -> [f32; 3] {
    match source {
        Source::Pattern(pattern) => pattern_color(*pattern, u, v),
        Source::Video(frame) => sample_video(frame, u, v),
    }
}

/// Miroir kaléidoscope puis pixellisation, sur les coordonnées de texture.
/// MÊMES formules que `warp.wgsl` — toute divergence est un bug.
fn apply_uv_effects(effects: &EffectsState, mut u: f64, mut v: f64) -> (f64, f64) {
    if effects.mirror > 0.0 {
        let mirrored = (u * 2.0 - 1.0).abs();
        u += f64::from(effects.mirror) * (mirrored - u);
    }
    if effects.pixelate > 0.0 {
        // Intensité 0..1 → blocs de 256 (imperceptible) à 8 (très gros).
        let blocks = 256.0 - f64::from(effects.pixelate) * 248.0;
        u = ((u * blocks).floor() + 0.5) / blocks;
        v = ((v * blocks).floor() + 0.5) / blocks;
    }
    (u.clamp(0.0, 1.0), v.clamp(0.0, 1.0))
}

/// Accentuation : 5 prélèvements en croix (décalage fixe en espace texture).
fn sharpen_sample(source: &Source<'_>, amount: f32, u: f64, v: f64) -> [f32; 3] {
    const OFFSET: f64 = 1.0 / 512.0;
    let center = sample_source(source, u, v);
    let mut neighbours = [0.0f32; 3];
    for (du, dv) in [(-OFFSET, 0.0), (OFFSET, 0.0), (0.0, -OFFSET), (0.0, OFFSET)] {
        let s = sample_source(source, (u + du).clamp(0.0, 1.0), (v + dv).clamp(0.0, 1.0));
        for (n, c) in neighbours.iter_mut().zip(s) {
            *n += c;
        }
    }
    let k = amount * 0.8;
    let mut out = [0.0f32; 3];
    for i in 0..3 {
        out[i] = center[i] * (1.0 + 4.0 * k) - neighbours[i] * k;
    }
    out
}

/// Postérisation puis bruit animé (après la correction couleur).
fn apply_pixel_effects(
    effects: &EffectsState,
    mut rgb: [f32; 3],
    u: f64,
    v: f64,
    time: f32,
) -> [f32; 3] {
    if effects.posterize > 0.0 {
        // Intensité 0..1 → 64 niveaux (imperceptible) à 3 (très marqué).
        let levels = 64.0 - effects.posterize * 61.0;
        for c in &mut rgb {
            *c = (c.clamp(0.0, 1.0) * levels).floor() / levels;
        }
    }
    if effects.noise > 0.0 {
        let n = hash2(
            u as f32 * 311.7 + time.fract() * 17.0,
            v as f32 * 173.3 + time.fract() * 29.0,
        );
        let grain = (n - 0.5) * effects.noise * 0.35;
        for c in &mut rgb {
            *c += grain;
        }
    }
    rgb
}

/// Hachage pseudo-aléatoire stable (même formule que le shader).
fn hash2(x: f32, y: f32) -> f32 {
    let d = x * 12.9898 + y * 78.233;
    (d.sin() * 43758.547).fract().abs()
}

/// Échantillonnage plus proche voisin de la frame vidéo (rapide ; le
/// filtrage bilinéaire viendra avec la passe GPU).
fn sample_video(frame: &VideoFrame, u: f64, v: f64) -> [f32; 3] {
    let x = ((u * f64::from(frame.width)) as usize).min(frame.width as usize - 1);
    let y = ((v * f64::from(frame.height)) as usize).min(frame.height as usize - 1);
    let i = (y * frame.width as usize + x) * 4;
    [
        f32::from(frame.rgba[i]) / 255.0,
        f32::from(frame.rgba[i + 1]) / 255.0,
        f32::from(frame.rgba[i + 2]) / 255.0,
    ]
}

/// Couleur de la mire au point `(u, v)` de la source, en RVB linéaire 0..1.
fn pattern_color(pattern: TestPattern, u: f64, v: f64) -> [f32; 3] {
    match pattern {
        TestPattern::Grid => grid_color(u, v),
        TestPattern::Checker => checker_color(u, v),
        TestPattern::Corners => corners_color(u, v),
    }
}

/// Grille de convergence 12×12 + croix centrale + cadre.
fn grid_color(u: f64, v: f64) -> [f32; 3] {
    const CELLS: f64 = 12.0;
    const LINE: f64 = 0.004;
    let near_line = |t: f64| (t * CELLS).fract().min(1.0 - (t * CELLS).fract()) < LINE * CELLS;
    let border = !(LINE..=1.0 - LINE).contains(&u) || !(LINE..=1.0 - LINE).contains(&v);
    let cross = (u - 0.5).abs() < LINE || (v - 0.5).abs() < LINE;
    if border || cross {
        [1.0, 1.0, 1.0]
    } else if near_line(u) || near_line(v) {
        [0.55, 0.55, 0.55]
    } else {
        [0.06, 0.06, 0.10]
    }
}

/// Damier 8×8 (deux gris, pas de blanc pur : lisible sans éblouir).
fn checker_color(u: f64, v: f64) -> [f32; 3] {
    const CELLS: f64 = 8.0;
    let cell = ((u * CELLS) as u32 + (v * CELLS) as u32) % 2;
    if cell == 0 {
        [0.85, 0.85, 0.85]
    } else {
        [0.12, 0.12, 0.12]
    }
}

/// Teintes des coins : 0=HG rouge, 1=HD vert, 2=BD bleu, 3=BG jaune —
/// l'ordre EXACT des poignées de l'UI, pour identifier chaque angle sur le VP.
const CORNER_TINTS: [[f32; 3]; 4] = [
    [0.9, 0.15, 0.15],
    [0.15, 0.8, 0.2],
    [0.2, 0.4, 0.95],
    [0.9, 0.8, 0.1],
];

/// Chiffres 0..3 en bitmap 3×5 (1 = pixel allumé), lisibles de loin.
const DIGITS: [[u8; 5]; 4] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
];

/// Mire « coins » : fond sombre, chaque quart teinté, gros chiffre du coin.
fn corners_color(u: f64, v: f64) -> [f32; 3] {
    // Index du coin du quart courant (même ordre que l'UI : 0=HG, 1=HD,
    // 2=BD, 3=BG).
    let index = match (u < 0.5, v < 0.5) {
        (true, true) => 0,
        (false, true) => 1,
        (false, false) => 2,
        (true, false) => 3,
    };
    let tint = CORNER_TINTS[index];
    // Chiffre dans une boîte proche du coin correspondant.
    let (bu, bv) = match index {
        0 => (0.08, 0.10),
        1 => (0.77, 0.10),
        2 => (0.77, 0.60),
        _ => (0.08, 0.60),
    };
    const DW: f64 = 0.15; // largeur de la boîte du chiffre (3 colonnes)
    const DH: f64 = 0.30; // hauteur (5 lignes)
    let (du, dv) = ((u - bu) / DW, (v - bv) / DH);
    if (0.0..1.0).contains(&du) && (0.0..1.0).contains(&dv) {
        let col = (du * 3.0) as usize;
        let row = (dv * 5.0) as usize;
        if DIGITS[index][row.min(4)] >> (2 - col.min(2)) & 1 == 1 {
            return [1.0, 1.0, 1.0];
        }
    }
    // Quart teinté, plus soutenu vers le coin extérieur.
    let strength = ((u - 0.5).abs() + (v - 0.5).abs()) as f32;
    [
        tint[0] * (0.25 + 0.75 * strength),
        tint[1] * (0.25 + 0.75 * strength),
        tint[2] * (0.25 + 0.75 * strength),
    ]
}

/// Correction couleur — implémentation de référence de la future passe GLSL.
/// Ordre : gains RVB → luminosité → contraste → gamma → saturation → teinte.
fn apply_color(c: &crate::ColorUniforms, rgb: [f32; 3]) -> [f32; 3] {
    let mut out = [
        rgb[0] * c.gain[0] * c.brightness,
        rgb[1] * c.gain[1] * c.brightness,
        rgb[2] * c.gain[2] * c.brightness,
    ];
    for ch in &mut out {
        *ch = (*ch - 0.5) * c.contrast + 0.5;
        *ch = ch.clamp(0.0, 1.0).powf(1.0 / c.gamma.max(0.01));
    }
    // Saturation autour de la luma Rec. 709.
    let luma = 0.2126 * out[0] + 0.7152 * out[1] + 0.0722 * out[2];
    for ch in &mut out {
        *ch = luma + (*ch - luma) * c.saturation;
    }
    // Rotation de teinte (matrice YIQ approchée, standard).
    if c.hue_degrees.abs() > f32::EPSILON {
        let a = f64::from(c.hue_degrees).to_radians();
        let (cos, sin) = (a.cos() as f32, a.sin() as f32);
        let m = [
            [
                0.213 + cos * 0.787 - sin * 0.213,
                0.715 - cos * 0.715 - sin * 0.715,
                0.072 - cos * 0.072 + sin * 0.928,
            ],
            [
                0.213 - cos * 0.213 + sin * 0.143,
                0.715 + cos * 0.285 + sin * 0.140,
                0.072 - cos * 0.072 - sin * 0.283,
            ],
            [
                0.213 - cos * 0.213 - sin * 0.787,
                0.715 - cos * 0.715 + sin * 0.715,
                0.072 + cos * 0.928 + sin * 0.072,
            ],
        ];
        let src = out;
        for (i, row) in m.iter().enumerate() {
            out[i] = row[0] * src[0] + row[1] * src[1] + row[2] * src[2];
        }
    }
    out
}

fn pack(rgb: [f32; 3]) -> u32 {
    let to8 = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u32;
    to8(rgb[0]) << 16 | to8(rgb[1]) << 8 | to8(rgb[2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolbox_core::{Command, NodeState};

    /// La rampe de blackout progresse linéairement puis sature à la cible.
    #[test]
    fn la_rampe_blackout_est_lineaire_et_saturee() {
        // Montée 0 → 1 sur 400 ms.
        assert!((niveau_rampe(1.0, 0.0, 0, 400) - 0.0).abs() < 1e-6);
        assert!((niveau_rampe(1.0, 0.0, 100, 400) - 0.25).abs() < 1e-6);
        assert!((niveau_rampe(1.0, 0.0, 400, 400) - 1.0).abs() < 1e-6);
        assert!((niveau_rampe(1.0, 0.0, 4000, 400) - 1.0).abs() < 1e-6);
        // Descente depuis un niveau intermédiaire (relâché en pleine rampe).
        assert!((niveau_rampe(0.0, 0.6, 200, 400) - 0.3).abs() < 1e-6);
        // Sans fondu : saut direct.
        assert!((niveau_rampe(1.0, 0.0, 0, 0) - 1.0).abs() < 1e-6);
    }

    /// Le voile noir multiplie chaque canal par 1 - niveau (parité WGSL).
    #[test]
    fn le_blackout_assombrit_chaque_canal() {
        let mut px = [0x00FF8040_u32, 0x00000000];
        appliquer_blackout(&mut px, 0.0);
        assert_eq!(px[0], 0x00FF8040, "niveau 0 : intact");
        appliquer_blackout(&mut px, 0.5);
        assert_eq!(px[0], (128 << 16) | (64 << 8) | 32);
        let mut px = [0x00FF8040_u32];
        appliquer_blackout(&mut px, 1.0);
        assert_eq!(px[0], 0, "niveau 1 : noir");
    }

    /// Le fondu de bords suit la rampe gamma exacte, bord par bord.
    #[test]
    fn le_blending_suit_la_rampe_gamma() {
        let blending = toolbox_core::BlendingState {
            gauche: 0.2,
            droite: 0.0,
            haut: 0.0,
            bas: 0.0,
            gamma: 2.0,
        };
        // Hors bande : plein. (Tolérance 1e-6 : largeurs f32 — règle projet.)
        assert!((facteur_blending(&blending, 0.5, 0.5) - 1.0).abs() < 1e-6);
        // Mi-bande, gamma 2 : (0.5)^2 = 0.25.
        assert!((facteur_blending(&blending, 0.1, 0.5) - 0.25).abs() < 1e-6);
        // Au ras du bord : noir.
        assert!(facteur_blending(&blending, 0.0, 0.5) < 1e-6);
        // Deux bords opposés se multiplient.
        let double = toolbox_core::BlendingState {
            droite: 0.2,
            ..blending
        };
        assert!((facteur_blending(&double, 0.9, 0.5) - 0.25).abs() < 1e-6);
        // Application au pixel : blanc mi-bande → 25 %.
        let pixel = appliquer_blending(&blending, 0x00FF_FFFF, 0.1, 0.5);
        assert_eq!(pixel, 0x0040_4040); // 255*0.25 = 63.75 → 64 = 0x40
    }

    /// Le test point-dans-quadrilatère couvre les deux sens d'enroulement
    /// et les points extérieurs.
    #[test]
    fn les_masques_couvrent_leur_quadrilatere() {
        use toolbox_core::state::Corner;
        let carre = toolbox_core::Masque {
            corners: [
                Corner { x: 0.25, y: 0.25 },
                Corner { x: 0.75, y: 0.25 },
                Corner { x: 0.75, y: 0.75 },
                Corner { x: 0.25, y: 0.75 },
            ],
        };
        let masques = vec![carre];
        assert!(dans_un_masque(&masques, 0.5, 0.5));
        assert!(!dans_un_masque(&masques, 0.1, 0.5));
        assert!(!dans_un_masque(&masques, 0.5, 0.9));
        // Sens inverse (horaire) : même couverture.
        let mut inverse = carre;
        inverse.corners.reverse();
        assert!(dans_un_masque(&[inverse], 0.5, 0.5));
        assert!(!dans_un_masque(&[inverse], 0.9, 0.9));
        assert!(!dans_un_masque(&[], 0.5, 0.5));
    }

    /// Sur une image rendue : le masque rend noir, le blending assombrit le
    /// bord gauche sans toucher le centre.
    #[test]
    fn rendu_avec_masque_et_blending() {
        let mut state = state_with(&[Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Checker),
        }]);
        state
            .apply(&Command::BlendingSet {
                gauche: 0.25,
                droite: 0.0,
                haut: 0.0,
                bas: 0.0,
                gamma: 2.0,
            })
            .expect("blending");
        state
            .apply(&Command::MasqueSet {
                index: 0,
                corners: [
                    toolbox_core::state::Corner { x: 0.6, y: 0.4 },
                    toolbox_core::state::Corner { x: 0.9, y: 0.4 },
                    toolbox_core::state::Corner { x: 0.9, y: 0.6 },
                    toolbox_core::state::Corner { x: 0.6, y: 0.6 },
                ],
            })
            .expect("masque");
        let (w, h) = (64u32, 36u32);
        let mut out = vec![0u32; (w * h) as usize];
        render_frame(&state, None, 0.0, w, h, &mut out);
        let px = |x: u32, y: u32| out[(y * w + x) as usize];
        // Centre du masque : noir absolu.
        assert_eq!(px(48, 18), 0);
        // Bord gauche (dans la bande) : plus sombre que le centre-gauche
        // équivalent hors bande.
        let bord = px(2, 18);
        let plein = px(20, 18);
        assert!(
            bord < plein,
            "bord {bord:#x} pas plus sombre que {plein:#x}"
        );
    }

    fn state_with(commands: &[Command]) -> NodeState {
        let mut state = NodeState::default();
        for command in commands {
            state.apply(command).expect("commande valide");
        }
        state
    }

    fn frame(state: &NodeState, w: u32, h: u32) -> Vec<u32> {
        let mut out = vec![0xDEAD_BEEF; (w * h) as usize];
        render_frame(state, None, 0.0, w, h, &mut out);
        out
    }

    /// Frame 2×2 : rouge, vert / bleu, blanc — un quadrant par couleur.
    fn test_video() -> VideoFrame {
        let rgba: Vec<u8> = [
            [255u8, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ]
        .concat();
        VideoFrame::new(2, 2, rgba.into()).expect("frame")
    }

    fn px(buf: &[u32], w: u32, x: u32, y: u32) -> u32 {
        buf[(y * w + x) as usize]
    }

    #[test]
    fn no_pattern_is_black() {
        let buf = frame(&NodeState::default(), 16, 9);
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn checker_alternates_cells() {
        let state = state_with(&[Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Checker),
        }]);
        // 64×64 : cellules de 8 px. Centres des deux premières cellules.
        let buf = frame(&state, 64, 64);
        let light = px(&buf, 64, 4, 4);
        let dark = px(&buf, 64, 12, 4);
        assert_ne!(light, dark);
        assert!(light > dark, "première cellule claire, seconde sombre");
    }

    #[test]
    fn warp_confines_pattern_and_leaves_outside_black() {
        // Quad rétréci au centre : les bords de la sortie sont hors mapping.
        let mut cmds = vec![Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Checker),
        }];
        for (i, (x, y)) in [(0.25, 0.25), (0.75, 0.25), (0.75, 0.75), (0.25, 0.75)]
            .iter()
            .enumerate()
        {
            cmds.push(Command::CornerSet {
                index: u8::try_from(i).expect("index"),
                x: *x as f32,
                y: *y as f32,
            });
        }
        let state = state_with(&cmds);
        let buf = frame(&state, 64, 64);
        assert_eq!(px(&buf, 64, 2, 2), 0, "hors quad : noir");
        assert_eq!(px(&buf, 64, 61, 61), 0, "hors quad : noir");
        assert_ne!(px(&buf, 64, 32, 32), 0, "centre du quad : mire visible");
    }

    #[test]
    fn disabled_mapping_fills_whole_frame() {
        // Coins farfelus MAIS mapping désactivé : la mire remplit tout.
        let state = state_with(&[
            Command::SetTestPattern {
                pattern: Some(toolbox_core::TestPattern::Checker),
            },
            Command::CornerSet {
                index: 0,
                x: 0.45,
                y: 0.45,
            },
            Command::SetMappingEnabled { enabled: false },
        ]);
        let buf = frame(&state, 64, 64);
        assert_ne!(px(&buf, 64, 1, 1), 0);
        assert_ne!(px(&buf, 64, 62, 62), 0);
    }

    #[test]
    fn degenerate_mapping_renders_black_without_panic() {
        let mut state = state_with(&[Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Grid),
        }]);
        for (i, (x, y)) in [(0.0, 0.0), (0.5, 0.0), (1.0, 0.0), (0.0, 1.0)]
            .iter()
            .enumerate()
        {
            state
                .apply(&Command::CornerSet {
                    index: u8::try_from(i).expect("index"),
                    x: *x,
                    y: *y,
                })
                .expect("corner");
        }
        let buf = frame(&state, 16, 16);
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn gain_r_zero_kills_red_channel() {
        let state = state_with(&[
            Command::SetTestPattern {
                pattern: Some(toolbox_core::TestPattern::Checker),
            },
            Command::ColorSet {
                param: toolbox_core::ColorParam::GainR,
                value: 0.0,
            },
        ]);
        let buf = frame(&state, 64, 64);
        let p = px(&buf, 64, 4, 4); // cellule claire
        assert_eq!(p >> 16 & 0xFF, 0, "rouge éteint");
        assert!(p & 0xFF > 0, "bleu toujours présent");
    }

    #[test]
    fn corner_quadrants_have_distinct_tints() {
        let state = state_with(&[Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Corners),
        }]);
        let buf = frame(&state, 100, 100);
        // Points près des quatre coins (hors boîtes de chiffres).
        let hg = px(&buf, 100, 2, 2);
        let hd = px(&buf, 100, 97, 2);
        let bd = px(&buf, 100, 97, 97);
        let bg = px(&buf, 100, 2, 97);
        let all = [hg, hd, bd, bg];
        for (i, a) in all.iter().enumerate() {
            assert_ne!(*a, 0, "coin {i} teinté");
            for b in &all[i + 1..] {
                assert_ne!(a, b, "teintes de coins distinctes");
            }
        }
        // HG est rouge dominant, HD vert dominant.
        assert!(hg >> 16 & 0xFF > hg & 0xFF);
        assert!(hd >> 8 & 0xFF > hd >> 16 & 0xFF);
    }

    #[test]
    fn video_shows_when_playing_and_not_when_stopped() {
        let mut state = state_with(&[Command::Load {
            path: "clips/a.mp4".into(),
        }]);
        let video = test_video();

        // Transport à l'arrêt : pas de vidéo, sortie noire.
        let mut out = vec![0xDEAD_BEEF; 64 * 64];
        render_frame(&state, Some(&video), 0.0, 64, 64, &mut out);
        assert!(out.iter().all(|&p| p == 0), "stoppé = noir");

        // En lecture : chaque quadrant échantillonne sa couleur.
        state.apply(&Command::Play).expect("play");
        render_frame(&state, Some(&video), 0.0, 64, 64, &mut out);
        assert_eq!(px(&out, 64, 10, 10), 0x00FF_0000, "haut-gauche rouge");
        assert_eq!(px(&out, 64, 50, 10), 0x0000_FF00, "haut-droit vert");
        assert_eq!(px(&out, 64, 10, 50), 0x0000_00FF, "bas-gauche bleu");
        assert_eq!(px(&out, 64, 50, 50), 0x00FF_FFFF, "bas-droit blanc");

        // En pause : la dernière frame reste affichée.
        state.apply(&Command::Pause).expect("pause");
        render_frame(&state, Some(&video), 0.0, 64, 64, &mut out);
        assert_eq!(px(&out, 64, 10, 10), 0x00FF_0000);
    }

    #[test]
    fn pattern_takes_priority_over_video() {
        let state = state_with(&[
            Command::Load {
                path: "clips/a.mp4".into(),
            },
            Command::Play,
            Command::SetTestPattern {
                pattern: Some(toolbox_core::TestPattern::Checker),
            },
        ]);
        let video = test_video();
        let mut out = vec![0u32; 64 * 64];
        render_frame(&state, Some(&video), 0.0, 64, 64, &mut out);
        // Damier gris, pas les couleurs saturées de la vidéo.
        let p = px(&out, 64, 4, 4);
        let (r, g, b) = (p >> 16 & 0xFF, p >> 8 & 0xFF, p & 0xFF);
        assert_eq!(r, g);
        assert_eq!(g, b);
    }

    #[test]
    fn video_is_warped_by_mapping() {
        // Quad rétréci : la vidéo n'apparaît qu'au centre.
        let mut cmds = vec![
            Command::Load {
                path: "clips/a.mp4".into(),
            },
            Command::Play,
        ];
        for (i, (x, y)) in [(0.25, 0.25), (0.75, 0.25), (0.75, 0.75), (0.25, 0.75)]
            .iter()
            .enumerate()
        {
            cmds.push(Command::CornerSet {
                index: u8::try_from(i).expect("index"),
                x: *x as f32,
                y: *y as f32,
            });
        }
        let state = state_with(&cmds);
        let video = test_video();
        let mut out = vec![0u32; 64 * 64];
        render_frame(&state, Some(&video), 0.0, 64, 64, &mut out);
        assert_eq!(px(&out, 64, 2, 2), 0, "hors quad : noir");
        assert_ne!(px(&out, 64, 30, 30), 0, "vidéo visible dans le quad");
    }

    #[test]
    fn effects_change_pixels_and_stay_off_at_zero() {
        // Pixellisation sur la grille : ses lignes fines (0,004) disparaissent
        // dans des blocs de 1/8 — l'image change forcément.
        let mut grid = state_with(&[Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Grid),
        }]);
        let mut reference = vec![0u32; 64 * 64];
        render_frame(&grid, None, 0.0, 64, 64, &mut reference);
        grid.apply(&Command::EffectSet {
            param: toolbox_core::state::EffectParam::Pixelate,
            value: 1.0,
        })
        .expect("effet");
        let mut pixelated = vec![0u32; 64 * 64];
        render_frame(&grid, None, 0.0, 64, 64, &mut pixelated);
        assert_ne!(reference, pixelated, "la pixellisation change l'image");
        assert_eq!(
            px(&pixelated, 64, 30, 10),
            px(&pixelated, 64, 33, 10),
            "les voisins du même bloc sont identiques"
        );

        let video = test_video();
        let playing = state_with(&[
            Command::Load {
                path: "clips/a.mp4".into(),
            },
            Command::Play,
        ]);
        let mut reference = vec![0u32; 64 * 64];
        render_frame(&playing, Some(&video), 0.0, 64, 64, &mut reference);

        // Miroir à fond : symétrie gauche/droite.
        let mut mirrored_state = state_with(&[
            Command::Load {
                path: "clips/a.mp4".into(),
            },
            Command::Play,
            Command::EffectSet {
                param: toolbox_core::state::EffectParam::Mirror,
                value: 1.0,
            },
        ]);
        mirrored_state.effects.pixelate = 0.0;
        let mut mirrored = vec![0u32; 64 * 64];
        render_frame(&mirrored_state, Some(&video), 0.0, 64, 64, &mut mirrored);
        assert_eq!(
            px(&mirrored, 64, 10, 10),
            px(&mirrored, 64, 53, 10),
            "miroir : symétrie horizontale"
        );

        // À zéro (défaut) : image strictement identique à la référence.
        let neutral = state_with(&[
            Command::Load {
                path: "clips/a.mp4".into(),
            },
            Command::Play,
        ]);
        let mut out = vec![0u32; 64 * 64];
        render_frame(&neutral, Some(&video), 123.0, 64, 64, &mut out);
        assert_eq!(reference, out, "effets à zéro = aucun changement");
    }

    #[test]
    fn buffer_shorter_than_frame_never_panics() {
        let state = state_with(&[Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Grid),
        }]);
        let mut out = vec![0u32; 10]; // bien plus court que 64×64
        render_frame(&state, None, 0.0, 64, 64, &mut out);
    }
}
