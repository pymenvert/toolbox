//! Homographie 4 coins (corner pinning).
//!
//! Miroir exact de `tools/mapping/homography_ref.py` (implémentation de
//! référence exécutable). Les vecteurs de test en bas de ce fichier ont été
//! générés par ce script — si ce module et le script divergent, c'est un bug.
//!
//! Convention : quad unité (0,0)(1,0)(1,1)(0,1), ordre des coins
//! 0=HG, 1=HD, 2=BD, 3=BG, (0,0) en haut-gauche. Le fragment shader reçoit
//! l'INVERSE de la matrice (sortie → source) pour échantillonner la texture.

use thiserror::Error;

use toolbox_core::state::MappingState;

#[derive(Debug, Error, PartialEq)]
pub enum HomographyError {
    #[error("coins dégénérés : trois coins sont (quasi) colinéaires")]
    Degenerate,
}

/// Matrice 3x3 row-major : `m[ligne][colonne]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3(pub [[f64; 3]; 3]);

impl Mat3 {
    pub const IDENTITY: Mat3 = Mat3([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    pub fn det(&self) -> f64 {
        let [[a, b, c], [d, e, f], [g, h, i]] = self.0;
        a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
    }

    /// Inverse par cofacteurs. `None` si non inversible.
    pub fn inverse(&self) -> Option<Mat3> {
        let det = self.det();
        if det.abs() < 1e-9 {
            return None;
        }
        let [[a, b, c], [d, e, f], [g, h, i]] = self.0;
        Some(Mat3([
            [
                (e * i - f * h) / det,
                (c * h - b * i) / det,
                (b * f - c * e) / det,
            ],
            [
                (f * g - d * i) / det,
                (a * i - c * g) / det,
                (c * d - a * f) / det,
            ],
            [
                (d * h - e * g) / det,
                (b * g - a * h) / det,
                (a * e - b * d) / det,
            ],
        ]))
    }

    /// Applique la matrice à un point (division perspective).
    pub fn apply(&self, u: f64, v: f64) -> (f64, f64) {
        let m = &self.0;
        let w = m[2][0] * u + m[2][1] * v + m[2][2];
        (
            (m[0][0] * u + m[0][1] * v + m[0][2]) / w,
            (m[1][0] * u + m[1][1] * v + m[1][2]) / w,
        )
    }

    /// Export pour `glUniformMatrix3fv` : column-major, f32 (convention GL).
    pub fn to_gl(&self) -> [f32; 9] {
        let m = &self.0;
        [
            m[0][0] as f32,
            m[1][0] as f32,
            m[2][0] as f32,
            m[0][1] as f32,
            m[1][1] as f32,
            m[2][1] as f32,
            m[0][2] as f32,
            m[1][2] as f32,
            m[2][2] as f32,
        ]
    }
}

/// Calcule H telle que H · quad_unité = coins du mapping.
///
/// Résolution directe du système 8x8 (DLT, h33=1) par élimination de Gauss
/// avec pivot partiel — pas de dépendance d'algèbre linéaire pour 8 équations.
pub fn from_mapping(mapping: &MappingState) -> Result<Mat3, HomographyError> {
    const UNIT: [(f64, f64); 4] = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];

    let mut a = [[0.0f64; 9]; 8]; // colonne 8 = second membre (matrice augmentée)
    for (k, ((u, v), corner)) in UNIT.iter().zip(mapping.corners.iter()).enumerate() {
        let (x, y) = (f64::from(corner.x), f64::from(corner.y));
        a[2 * k] = [*u, *v, 1.0, 0.0, 0.0, 0.0, -u * x, -v * x, x];
        a[2 * k + 1] = [0.0, 0.0, 0.0, *u, *v, 1.0, -u * y, -v * y, y];
    }

    // Élimination de Gauss, pivot partiel.
    for col in 0..8 {
        let pivot = (col..8)
            .max_by(|&r1, &r2| {
                a[r1][col]
                    .abs()
                    .partial_cmp(&a[r2][col].abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(col);
        if a[pivot][col].abs() < 1e-12 {
            return Err(HomographyError::Degenerate);
        }
        a.swap(col, pivot);
        for r in (col + 1)..8 {
            let f = a[r][col] / a[col][col];
            for c in col..9 {
                a[r][c] -= f * a[col][c];
            }
        }
    }
    let mut h = [0.0f64; 8];
    for r in (0..8).rev() {
        let sum: f64 = ((r + 1)..8).map(|c| a[r][c] * h[c]).sum();
        h[r] = (a[r][8] - sum) / a[r][r];
    }

    let m = Mat3([[h[0], h[1], h[2]], [h[3], h[4], h[5]], [h[6], h[7], 1.0]]);
    // Comme dans la référence Python : le système peut se résoudre pour des
    // coins dégénérés, la dégénérescence se voit au déterminant.
    if m.det().abs() < 1e-9 {
        return Err(HomographyError::Degenerate);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolbox_core::state::Corner;

    const EPS: f64 = 1e-9;

    fn close(p: (f64, f64), q: (f64, f64)) -> bool {
        (p.0 - q.0).abs() < EPS && (p.1 - q.1).abs() < EPS
    }

    fn mapping(corners: [(f32, f32); 4]) -> MappingState {
        MappingState {
            corners: corners.map(|(x, y)| Corner { x, y }),
        }
    }

    #[test]
    fn unit_quad_gives_identity() {
        let h = from_mapping(&MappingState::default()).expect("identity");
        for r in 0..3 {
            for c in 0..3 {
                let expected = if r == c { 1.0 } else { 0.0 };
                assert!((h.0[r][c] - expected).abs() < EPS, "m[{r}][{c}]");
            }
        }
    }

    /// Vecteurs générés par tools/mapping/homography_ref.py — ne pas modifier
    /// à la main : relancer le script si la convention change.
    #[test]
    fn matches_python_reference() {
        let m = mapping([(0.08, 0.05), (0.97, 0.02), (1.0, 0.93), (0.03, 0.98)]);
        let h = from_mapping(&m).expect("homography");
        let expected = [
            [0.906894367790, -0.052490386790, 0.080000000000],
            [-0.029651662520, 0.848647364850, 0.050000000000],
            [0.017416874010, -0.083012893011, 1.000000000000],
        ];
        for r in 0..3 {
            for c in 0..3 {
                assert!(
                    (h.0[r][c] - expected[r][c]).abs() < 1e-9,
                    "m[{r}][{c}] = {} != {}",
                    h.0[r][c],
                    expected[r][c]
                );
            }
        }
        assert!(close(h.apply(0.5, 0.5), (0.524401309635, 0.475079513564)));
    }

    #[test]
    fn corners_map_exactly() {
        let corners = [(0.08, 0.05), (0.97, 0.02), (1.0, 0.93), (0.03, 0.98)];
        let h = from_mapping(&mapping(corners)).expect("homography");
        let unit = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        for ((u, v), (x, y)) in unit.iter().zip(corners.iter()) {
            assert!(close(h.apply(*u, *v), (f64::from(*x), f64::from(*y))));
        }
    }

    #[test]
    fn inverse_roundtrip() {
        let h = from_mapping(&mapping([
            (0.08, 0.05),
            (0.97, 0.02),
            (1.0, 0.93),
            (0.03, 0.98),
        ]))
        .expect("homography");
        let inv = h.inverse().expect("inverse");
        for p in [(0.5, 0.5), (0.25, 0.75), (0.1, 0.9), (0.999, 0.001)] {
            let (x, y) = h.apply(p.0, p.1);
            assert!(close(inv.apply(x, y), p));
        }
    }

    #[test]
    fn degenerate_corners_rejected() {
        // Trois coins colinéaires (mêmes valeurs que la référence Python).
        let res = from_mapping(&mapping([(0.0, 0.0), (0.5, 0.0), (1.0, 0.0), (0.0, 1.0)]));
        assert_eq!(res, Err(HomographyError::Degenerate));
    }

    #[test]
    fn gl_export_is_column_major() {
        let h = Mat3([[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]]);
        assert_eq!(h.to_gl(), [1.0, 4.0, 7.0, 2.0, 5.0, 8.0, 3.0, 6.0, 9.0]);
    }
}
