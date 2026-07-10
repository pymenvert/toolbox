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
//! Sans mire sélectionnée la sortie est noire : un vidéoprojecteur de
//! spectacle ne doit jamais afficher de contenu par défaut.

use toolbox_core::command::TestPattern;
use toolbox_core::state::NodeState;
use toolbox_engine::{Mat3, RenderParams};
use tracing::warn;

/// Rend une frame `width`×`height` dans `out` (format softbuffer `0RGB`,
/// une entrée `u32` par pixel, lignes de haut en bas).
///
/// `out` est retaillé par l'appelant : la fonction ne panique jamais, elle
/// s'arrête à `out.len()`.
pub fn render_frame(state: &NodeState, width: u32, height: u32, out: &mut [u32]) {
    // Pas de mire : noir immédiat (chemin rapide, cas nominal en show).
    let Some(pattern) = state.test_pattern else {
        out.fill(0);
        return;
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
    for (i, px) in out.iter_mut().enumerate().take(w * h) {
        let (x, y) = (i % w, i / w);
        let u = (x as f64 + 0.5) / w as f64;
        let v = (y as f64 + 0.5) / h as f64;
        *px = shade(pattern, &warp_inv, &params, u, v);
    }
}

/// Couleur d'un pixel de sortie, packée en `0RGB`.
fn shade(pattern: TestPattern, warp_inv: &Mat3, params: &RenderParams, u: f64, v: f64) -> u32 {
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
    // 3. Mire procédurale.
    let rgb = pattern_color(pattern, tu, tv);
    // 4. Correction couleur.
    let rgb = apply_color(&params.color, rgb);
    pack(rgb)
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
    let border = u < LINE || u > 1.0 - LINE || v < LINE || v > 1.0 - LINE;
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
fn apply_color(c: &toolbox_engine::ColorUniforms, rgb: [f32; 3]) -> [f32; 3] {
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

    fn state_with(commands: &[Command]) -> NodeState {
        let mut state = NodeState::default();
        for command in commands {
            state.apply(command).expect("commande valide");
        }
        state
    }

    fn frame(state: &NodeState, w: u32, h: u32) -> Vec<u32> {
        let mut out = vec![0xDEAD_BEEF; (w * h) as usize];
        render_frame(state, w, h, &mut out);
        out
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
    fn buffer_shorter_than_frame_never_panics() {
        let state = state_with(&[Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Grid),
        }]);
        let mut out = vec![0u32; 10]; // bien plus court que 64×64
        render_frame(&state, 64, 64, &mut out);
    }
}
