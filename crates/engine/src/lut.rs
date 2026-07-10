//! LUT 3D au format `.cube` (Adobe/Resolve) : étalonnage complet en un
//! fichier. Parse + interpolation trilinéaire — la référence CPU ; le GPU
//! échantillonne une texture 3D en filtrage linéaire, même résultat aux
//! arrondis f32 près.

/// Une LUT 3D chargée : `taille`³ entrées RGB, r variant le plus vite
/// (ordre du format .cube).
#[derive(Debug, Clone, PartialEq)]
pub struct Lut3d {
    pub taille: usize,
    /// `taille³` triplets, ordre : b (lent) → g → r (rapide).
    pub data: Vec<[f32; 3]>,
}

impl Lut3d {
    /// Parse un fichier `.cube`. Accepté : commentaires `#`, `TITLE`,
    /// `LUT_3D_SIZE n`, `DOMAIN_MIN/MAX` (0..1 uniquement), lignes `r g b`.
    /// Les LUT 1D (`LUT_1D_SIZE`) sont refusées.
    pub fn depuis_texte(texte: &str) -> Result<Self, String> {
        let mut taille: Option<usize> = None;
        let mut data: Vec<[f32; 3]> = Vec::new();
        for (num, ligne) in texte.lines().enumerate() {
            let ligne = ligne.trim();
            if ligne.is_empty() || ligne.starts_with('#') {
                continue;
            }
            let mut mots = ligne.split_whitespace();
            let premier = mots.next().unwrap_or_default();
            match premier {
                "TITLE" => {}
                "LUT_1D_SIZE" => return Err("LUT 1D non prise en charge (3D seulement)".into()),
                "LUT_3D_SIZE" => {
                    let n: usize = mots
                        .next()
                        .and_then(|m| m.parse().ok())
                        .ok_or_else(|| format!("ligne {} : LUT_3D_SIZE invalide", num + 1))?;
                    if !(2..=129).contains(&n) {
                        return Err(format!("taille de LUT hors bornes : {n} (2..129)"));
                    }
                    taille = Some(n);
                }
                "DOMAIN_MIN" | "DOMAIN_MAX" => {
                    // Seul le domaine standard 0..1 est accepté : les
                    // valeurs sont vérifiées, pas remises à l'échelle.
                    let attendu = if premier == "DOMAIN_MIN" { 0.0 } else { 1.0 };
                    for m in mots {
                        let v: f32 = m
                            .parse()
                            .map_err(|_| format!("ligne {} : domaine invalide", num + 1))?;
                        if (v - attendu).abs() > 1e-6 {
                            return Err("seul le domaine 0..1 est pris en charge".into());
                        }
                    }
                }
                _ => {
                    let r: f32 = premier
                        .parse()
                        .map_err(|_| format!("ligne {} : « {premier} » inattendu", num + 1))?;
                    let g: f32 = mots
                        .next()
                        .and_then(|m| m.parse().ok())
                        .ok_or_else(|| format!("ligne {} : triplet incomplet", num + 1))?;
                    let b: f32 = mots
                        .next()
                        .and_then(|m| m.parse().ok())
                        .ok_or_else(|| format!("ligne {} : triplet incomplet", num + 1))?;
                    data.push([r, g, b]);
                }
            }
        }
        let taille = taille.ok_or("LUT_3D_SIZE absent")?;
        if data.len() != taille * taille * taille {
            return Err(format!(
                "{} entrées lues, {} attendues (taille {taille})",
                data.len(),
                taille * taille * taille
            ));
        }
        Ok(Self { taille, data })
    }

    /// Valeur de la grille à l'indice (r, g, b) — bornes déjà garanties.
    fn grille(&self, r: usize, g: usize, b: usize) -> [f32; 3] {
        self.data[(b * self.taille + g) * self.taille + r]
    }

    /// Applique la LUT à un triplet RGB 0..1 par interpolation trilinéaire.
    pub fn appliquer(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.taille;
        #[allow(clippy::cast_precision_loss)] // n ≤ 129
        let echelle = (n - 1) as f32;
        // Position continue dans la grille + indices des 8 sommets.
        let mut idx = [0usize; 3];
        let mut frac = [0f32; 3];
        for c in 0..3 {
            let pos = rgb[c].clamp(0.0, 1.0) * echelle;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let i = (pos.floor() as usize).min(n - 2);
            idx[c] = i;
            #[allow(clippy::cast_precision_loss)]
            {
                frac[c] = pos - i as f32;
            }
        }
        let (ir, ig, ib) = (idx[0], idx[1], idx[2]);
        let (fr, fg, fb) = (frac[0], frac[1], frac[2]);
        let mut sortie = [0f32; 3];
        for (c, valeur) in sortie.iter_mut().enumerate() {
            // Trilinéaire : lerp sur r, puis g, puis b.
            let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
            let c00 = lerp(
                self.grille(ir, ig, ib)[c],
                self.grille(ir + 1, ig, ib)[c],
                fr,
            );
            let c10 = lerp(
                self.grille(ir, ig + 1, ib)[c],
                self.grille(ir + 1, ig + 1, ib)[c],
                fr,
            );
            let c01 = lerp(
                self.grille(ir, ig, ib + 1)[c],
                self.grille(ir + 1, ig, ib + 1)[c],
                fr,
            );
            let c11 = lerp(
                self.grille(ir, ig + 1, ib + 1)[c],
                self.grille(ir + 1, ig + 1, ib + 1)[c],
                fr,
            );
            *valeur = lerp(lerp(c00, c10, fg), lerp(c01, c11, fg), fb);
        }
        sortie
    }

    /// Les texels RGBA (f32 → u8 non nécessaire : format Rgba32Float côté
    /// GPU serait lourd ; on livre des f32 bruts pour la texture 3D).
    /// Ordre : b (couche) → g (ligne) → r (colonne), RGBA (alpha 1).
    pub fn texels_rgba_f32(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.data.len() * 4);
        for t in &self.data {
            out.extend_from_slice(&[t[0], t[1], t[2], 1.0]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITE_2: &str = "\
# identité 2x2x2
TITLE \"id\"
LUT_3D_SIZE 2
0 0 0
1 0 0
0 1 0
1 1 0
0 0 1
1 0 1
0 1 1
1 1 1
";

    #[test]
    fn parse_et_identite_trilineaire() {
        let lut = Lut3d::depuis_texte(IDENTITE_2).expect("parse");
        assert_eq!(lut.taille, 2);
        // L'identité rend chaque triplet inchangé, y compris entre sommets.
        for rgb in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.25, 0.5, 0.75]] {
            let out = lut.appliquer(rgb);
            for c in 0..3 {
                assert!((out[c] - rgb[c]).abs() < 1e-6, "{rgb:?} -> {out:?}");
            }
        }
    }

    #[test]
    fn une_lut_inversee_inverse() {
        // Négatif : sortie = 1 - entrée.
        let texte = "LUT_3D_SIZE 2\n1 1 1\n0 1 1\n1 0 1\n0 0 1\n1 1 0\n0 1 0\n1 0 0\n0 0 0\n";
        let lut = Lut3d::depuis_texte(texte).expect("parse");
        let out = lut.appliquer([0.2, 0.5, 0.8]);
        assert!((out[0] - 0.8).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
        assert!((out[2] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn les_fichiers_invalides_sont_refuses_proprement() {
        assert!(Lut3d::depuis_texte("").is_err(), "vide");
        assert!(
            Lut3d::depuis_texte("LUT_3D_SIZE 2\n0 0 0\n").is_err(),
            "entrées manquantes"
        );
        assert!(
            Lut3d::depuis_texte("LUT_1D_SIZE 4\n0\n0.3\n0.6\n1\n").is_err(),
            "LUT 1D"
        );
        assert!(
            Lut3d::depuis_texte("LUT_3D_SIZE 1\n0 0 0\n").is_err(),
            "taille 1"
        );
        assert!(
            Lut3d::depuis_texte("LUT_3D_SIZE 2\nabc def ghi\n").is_err(),
            "triplet non numérique"
        );
    }
}
