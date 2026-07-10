//! Paramètres de rendu (P1.3/P1.4) : traduit l'état du node en matrices et
//! uniformes prêts pour le shader de warp.
//!
//! Tout est pur et testé ici ; le futur renderer GL (GStreamer + GLES) n'aura
//! qu'à téléverser ces valeurs. La web UI utilise la même convention pour sa
//! prévisualisation (implémentation JS vérifiée contre la référence Python).
//!
//! Chaîne d'échantillonnage, pour un pixel de sortie en UV `(u, v)` :
//! 1. warp inverse (homographie 4 coins) : sortie → quad unité ;
//! 2. flip écran (miroir H/V) ;
//! 3. rotation inverse (la source est affichée tournée de 0/90/180/270°
//!    horaire) ;
//! 4. recadrage : fenêtre `crop` dans la texture source.
//!
//! Les étapes 2-4 sont combinées dans [`RenderParams::uv_transform`].

use toolbox_core::state::{NodeState, Rotation};

use crate::homography::{from_mapping, HomographyError, Mat3};

/// Uniformes de correction couleur (P1.4), directement mappables en GLSL.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorUniforms {
    pub brightness: f32,
    pub contrast: f32,
    pub gamma: f32,
    pub saturation: f32,
    /// Teinte en degrés (-180..=180) — le shader convertit en radians.
    pub hue_degrees: f32,
    /// Gains RGB (r, g, b).
    pub gain: [f32; 3],
}

/// Tout ce que le renderer doit téléverser pour une frame.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderParams {
    /// Homographie H : quad unité → coins du mapping (espace de sortie).
    pub warp: Mat3,
    /// H⁻¹ en column-major f32 pour `glUniformMatrix3fv` (le fragment shader
    /// échantillonne sortie → source).
    pub warp_inv_gl: [f32; 9],
    /// Transforme un UV (post-warp) en UV de texture source :
    /// flip + rotation inverse + recadrage.
    pub uv_transform: Mat3,
    /// Idem en column-major f32 pour GL.
    pub uv_transform_gl: [f32; 9],
    pub color: ColorUniforms,
}

impl RenderParams {
    /// Calcule les paramètres de rendu depuis l'état complet du node.
    ///
    /// Mapping désactivé (`mapping.enabled == false`) : warp et UV passent en
    /// identité (image brute plein cadre), sans même évaluer les coins — un
    /// mapping dégradé stocké ne doit pas empêcher le bypass. La correction
    /// couleur, elle, reste active.
    pub fn from_state(state: &NodeState) -> Result<Self, HomographyError> {
        let (warp, warp_inv, uv_transform) = if state.mapping.enabled {
            let warp = from_mapping(&state.mapping)?;
            let warp_inv = warp.inverse().ok_or(HomographyError::Degenerate)?;
            let flip = flip_matrix(state.mapping.flip_h, state.mapping.flip_v);
            let rotation_inv = rotation_inverse_matrix(state.mapping.rotation);
            let crop = crop_matrix(&state.mapping);
            // Ordre d'application sur un vecteur colonne : flip d'abord,
            // puis rotation inverse, puis recadrage.
            (warp, warp_inv, crop.mul(&rotation_inv).mul(&flip))
        } else {
            (Mat3::IDENTITY, Mat3::IDENTITY, Mat3::IDENTITY)
        };

        Ok(Self {
            warp,
            warp_inv_gl: warp_inv.to_gl(),
            uv_transform,
            uv_transform_gl: uv_transform.to_gl(),
            color: ColorUniforms {
                brightness: state.color.brightness,
                contrast: state.color.contrast,
                gamma: state.color.gamma,
                saturation: state.color.saturation,
                hue_degrees: state.color.hue,
                gain: [state.color.gain_r, state.color.gain_g, state.color.gain_b],
            },
        })
    }
}

/// Miroir en espace écran : `u' = 1-u` (horizontal) et/ou `v' = 1-v`.
fn flip_matrix(flip_h: bool, flip_v: bool) -> Mat3 {
    let (a, c) = if flip_h { (-1.0, 1.0) } else { (1.0, 0.0) };
    let (e, f) = if flip_v { (-1.0, 1.0) } else { (1.0, 0.0) };
    Mat3([[a, 0.0, c], [0.0, e, f], [0.0, 0.0, 1.0]])
}

/// Rotation **inverse** : la source est affichée tournée de `rotation`
/// horaire, donc l'échantillonnage applique la rotation opposée.
///
/// Vérité pinnée par tests : R90 ⇒ `src(u,v) = (v, 1-u)` — le coin
/// haut-droit de l'écran affiche le haut-gauche de la source.
fn rotation_inverse_matrix(rotation: Rotation) -> Mat3 {
    match rotation {
        Rotation::R0 => Mat3::IDENTITY,
        Rotation::R90 => Mat3([[0.0, 1.0, 0.0], [-1.0, 0.0, 1.0], [0.0, 0.0, 1.0]]),
        Rotation::R180 => Mat3([[-1.0, 0.0, 1.0], [0.0, -1.0, 1.0], [0.0, 0.0, 1.0]]),
        Rotation::R270 => Mat3([[0.0, -1.0, 1.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]),
    }
}

/// Fenêtre de recadrage : mappe [0,1]² sur la sous-fenêtre restante.
fn crop_matrix(mapping: &toolbox_core::state::MappingState) -> Mat3 {
    let c = &mapping.crop;
    let (l, t, r, b) = (
        f64::from(c.left),
        f64::from(c.top),
        f64::from(c.right),
        f64::from(c.bottom),
    );
    Mat3([
        [1.0 - l - r, 0.0, l],
        [0.0, 1.0 - t - b, t],
        [0.0, 0.0, 1.0],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolbox_core::{Command, NodeState};

    // L'état stocke les valeurs en f32 : les attentes écrites en f64 divergent
    // de ~1e-8 après conversion (cf. tolérance homographie).
    const EPS: f64 = 1e-6;

    fn close(p: (f64, f64), q: (f64, f64)) -> bool {
        (p.0 - q.0).abs() < EPS && (p.1 - q.1).abs() < EPS
    }

    fn state_with(commands: &[Command]) -> NodeState {
        let mut state = NodeState::default();
        for command in commands {
            state.apply(command).expect("commande valide");
        }
        state
    }

    #[test]
    fn default_state_gives_identity_everywhere() {
        let params = RenderParams::from_state(&NodeState::default()).expect("params");
        for (i, (got, want)) in params
            .uv_transform_gl
            .iter()
            .zip(Mat3::IDENTITY.to_gl().iter())
            .enumerate()
        {
            assert!((got - want).abs() < 1e-6, "uv[{i}]");
        }
        for (i, (got, want)) in params
            .warp_inv_gl
            .iter()
            .zip(Mat3::IDENTITY.to_gl().iter())
            .enumerate()
        {
            assert!((got - want).abs() < 1e-6, "warp_inv[{i}]");
        }
        assert_eq!(params.color.gain, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn rotation_90_samples_correctly() {
        let state = state_with(&[Command::SetRotation { degrees: 90 }]);
        let params = RenderParams::from_state(&state).expect("params");
        // Haut-droit de l'écran (1,0) → haut-gauche de la source (0,0).
        assert!(close(params.uv_transform.apply(1.0, 0.0), (0.0, 0.0)));
        // Bas-droit (1,1) → haut-droit (1,0).
        assert!(close(params.uv_transform.apply(1.0, 1.0), (1.0, 0.0)));
        // Centre → centre.
        assert!(close(params.uv_transform.apply(0.5, 0.5), (0.5, 0.5)));
    }

    #[test]
    fn rotation_270_is_inverse_of_90() {
        let quarter = rotation_inverse_matrix(Rotation::R90);
        let three_quarters = rotation_inverse_matrix(Rotation::R270);
        let composed = quarter.mul(&three_quarters);
        for (i, (got, want)) in composed
            .to_gl()
            .iter()
            .zip(Mat3::IDENTITY.to_gl().iter())
            .enumerate()
        {
            assert!((got - want).abs() < 1e-6, "m[{i}]");
        }
    }

    #[test]
    fn flip_h_mirrors_horizontally() {
        let state = state_with(&[Command::SetFlip {
            horizontal: true,
            vertical: false,
        }]);
        let params = RenderParams::from_state(&state).expect("params");
        assert!(close(params.uv_transform.apply(0.0, 0.25), (1.0, 0.25)));
        assert!(close(params.uv_transform.apply(1.0, 0.25), (0.0, 0.25)));
    }

    #[test]
    fn crop_maps_into_window() {
        let state = state_with(&[Command::SetCrop {
            left: 0.1,
            top: 0.2,
            right: 0.3,
            bottom: 0.0,
        }]);
        let params = RenderParams::from_state(&state).expect("params");
        // (0,0) → coin haut-gauche de la fenêtre recadrée.
        assert!(close(params.uv_transform.apply(0.0, 0.0), (0.1, 0.2)));
        // (1,1) → coin bas-droit : 1-right, 1-bottom.
        assert!(close(params.uv_transform.apply(1.0, 1.0), (0.7, 1.0)));
    }

    #[test]
    fn rotation_and_flip_compose_in_screen_space() {
        // Rotation 90 horaire puis miroir horizontal (en espace écran).
        let state = state_with(&[
            Command::SetRotation { degrees: 90 },
            Command::SetFlip {
                horizontal: true,
                vertical: false,
            },
        ]);
        let params = RenderParams::from_state(&state).expect("params");
        // Écran (0,0) —flip→ (1,0) —rot⁻¹→ source (0,0).
        assert!(close(params.uv_transform.apply(0.0, 0.0), (0.0, 0.0)));
    }

    #[test]
    fn color_uniforms_follow_state() {
        let state = state_with(&[
            Command::ColorSet {
                param: toolbox_core::ColorParam::Gamma,
                value: 2.2,
            },
            Command::ColorSet {
                param: toolbox_core::ColorParam::GainB,
                value: 0.5,
            },
            Command::ColorSet {
                param: toolbox_core::ColorParam::Hue,
                value: 45.0,
            },
        ]);
        let params = RenderParams::from_state(&state).expect("params");
        assert!((params.color.gamma - 2.2).abs() < f32::EPSILON);
        assert!((params.color.gain[2] - 0.5).abs() < f32::EPSILON);
        assert!((params.color.hue_degrees - 45.0).abs() < f32::EPSILON);
    }

    #[test]
    fn disabled_mapping_bypasses_warp_but_keeps_color() {
        // Réglages agressifs partout : coins, rotation, flip, crop, couleur.
        let state = state_with(&[
            Command::CornerSet {
                index: 0,
                x: 0.3,
                y: 0.2,
            },
            Command::SetRotation { degrees: 90 },
            Command::SetFlip {
                horizontal: true,
                vertical: false,
            },
            Command::SetCrop {
                left: 0.1,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            },
            Command::ColorSet {
                param: toolbox_core::ColorParam::Gamma,
                value: 2.0,
            },
            Command::SetMappingEnabled { enabled: false },
        ]);
        let params = RenderParams::from_state(&state).expect("params");
        // Tout le bloc mapping est ignoré : identité partout…
        for (i, (got, want)) in params
            .uv_transform_gl
            .iter()
            .zip(Mat3::IDENTITY.to_gl().iter())
            .enumerate()
        {
            assert!((got - want).abs() < 1e-6, "uv[{i}]");
        }
        for (i, (got, want)) in params
            .warp_inv_gl
            .iter()
            .zip(Mat3::IDENTITY.to_gl().iter())
            .enumerate()
        {
            assert!((got - want).abs() < 1e-6, "warp_inv[{i}]");
        }
        // …mais la couleur reste appliquée.
        assert!((params.color.gamma - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn degenerate_mapping_is_an_error() {
        let mut state = NodeState::default();
        // Trois coins alignés → homographie dégénérée.
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
        assert_eq!(
            RenderParams::from_state(&state),
            Err(HomographyError::Degenerate)
        );
    }
}
