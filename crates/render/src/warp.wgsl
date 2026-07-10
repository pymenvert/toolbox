// Chaîne de rendu de la fenêtre de sortie — implémentation GPU de la
// référence CPU testée (raster.rs). Toute divergence visuelle avec elle est
// un bug : mêmes matrices, mêmes mires, même ordre de correction couleur.

struct Uniforms {
    // Matrices 3x3 en colonnes (colonne-major, .w inutilisé) : warp inverse
    // (sortie → quad unité) puis flip/rotation/recadrage (quad → texture).
    warp_inv_c0: vec4<f32>,
    warp_inv_c1: vec4<f32>,
    warp_inv_c2: vec4<f32>,
    uv_c0: vec4<f32>,
    uv_c1: vec4<f32>,
    uv_c2: vec4<f32>,
    // luminosité, contraste, gamma, saturation
    color_a: vec4<f32>,
    // teinte (radians), gain R, gain V, gain B
    color_b: vec4<f32>,
    // largeur, hauteur, mode (0 noir, 1 grille, 2 damier, 3 coins, 4 vidéo)
    misc: vec4<f32>,
    // pixellisation, postérisation, bruit, accentuation (intensités 0..1)
    fx_a: vec4<f32>,
    // miroir, temps (secondes, pour le bruit animé)
    fx_b: vec4<f32>,
    // fondu de bords : gauche, droite, haut, bas (largeurs 0..0.45)
    blending_a: vec4<f32>,
    // fondu de bords : gamma, nombre de masques
    blending_b: vec4<f32>,
    // masques : 8 quadrilatères × 2 vec4 (x0,y0,x1,y1) puis (x2,y2,x3,y3)
    masques: array<vec4<f32>, 16>,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var video_tex: texture_2d<f32>;
@group(0) @binding(2) var video_smp: sampler;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // Triangle plein écran (aucun vertex buffer).
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(pos[index], 0.0, 1.0);
}

// Applique une matrice 3x3 homogène (division perspective incluse).
fn apply3(c0: vec4<f32>, c1: vec4<f32>, c2: vec4<f32>, p: vec2<f32>) -> vec2<f32> {
    let m = mat3x3<f32>(c0.xyz, c1.xyz, c2.xyz);
    let r = m * vec3<f32>(p, 1.0);
    return r.xy / r.z;
}

// Grille de convergence 12×12 + croix centrale + cadre (cf. raster.rs).
fn grid_color(p: vec2<f32>) -> vec3<f32> {
    let cells = 12.0;
    let line = 0.004;
    let f = fract(p * cells);
    let near = min(f, vec2<f32>(1.0) - f);
    let on_line = near.x < line * cells || near.y < line * cells;
    let border = p.x < line || p.x > 1.0 - line || p.y < line || p.y > 1.0 - line;
    let cross = abs(p.x - 0.5) < line || abs(p.y - 0.5) < line;
    if border || cross {
        return vec3<f32>(1.0);
    }
    if on_line {
        return vec3<f32>(0.55);
    }
    return vec3<f32>(0.06, 0.06, 0.10);
}

// Damier 8×8, deux gris.
fn checker_color(p: vec2<f32>) -> vec3<f32> {
    let cell = (u32(p.x * 8.0) + u32(p.y * 8.0)) % 2u;
    if cell == 0u {
        return vec3<f32>(0.85);
    }
    return vec3<f32>(0.12);
}

// Mire « coins » : quart teinté + gros chiffre (bitmaps 3×5 de raster.rs,
// encodés 15 bits par chiffre, lignes de haut en bas).
fn corners_color(p: vec2<f32>) -> vec3<f32> {
    var index = 0u;
    if p.x >= 0.5 && p.y < 0.5 {
        index = 1u;
    } else if p.x >= 0.5 && p.y >= 0.5 {
        index = 2u;
    } else if p.x < 0.5 && p.y >= 0.5 {
        index = 3u;
    }
    var tints = array<vec3<f32>, 4>(
        vec3<f32>(0.9, 0.15, 0.15),
        vec3<f32>(0.15, 0.8, 0.2),
        vec3<f32>(0.2, 0.4, 0.95),
        vec3<f32>(0.9, 0.8, 0.1),
    );
    var boxes = array<vec2<f32>, 4>(
        vec2<f32>(0.08, 0.10),
        vec2<f32>(0.77, 0.10),
        vec2<f32>(0.77, 0.60),
        vec2<f32>(0.08, 0.60),
    );
    let d = (p - boxes[index]) / vec2<f32>(0.15, 0.30);
    if d.x >= 0.0 && d.x < 1.0 && d.y >= 0.0 && d.y < 1.0 {
        let col = min(u32(d.x * 3.0), 2u);
        let row = min(u32(d.y * 5.0), 4u);
        var digits = array<u32, 4>(0x7B6Fu, 0x2C97u, 0x73E7u, 0x73CFu);
        if ((digits[index] >> ((4u - row) * 3u + (2u - col))) & 1u) == 1u {
            return vec3<f32>(1.0);
        }
    }
    let strength = abs(p.x - 0.5) + abs(p.y - 0.5);
    return tints[index] * (0.25 + 0.75 * strength);
}

// Correction couleur — même ordre que raster.rs :
// gains RVB → luminosité → contraste → gamma → saturation → teinte.
fn apply_color(rgb_in: vec3<f32>) -> vec3<f32> {
    let brightness = u.color_a.x;
    let contrast = u.color_a.y;
    let gamma = max(u.color_a.z, 0.01);
    let saturation = u.color_a.w;
    let hue = u.color_b.x;
    let gains = u.color_b.yzw;

    var c = rgb_in * gains * brightness;
    c = (c - vec3<f32>(0.5)) * contrast + vec3<f32>(0.5);
    c = pow(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / gamma));
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = vec3<f32>(luma) + (c - luma) * saturation;
    if abs(hue) > 1e-6 {
        let cs = cos(hue);
        let sn = sin(hue);
        // Colonnes de la matrice de rotation de teinte (YIQ approchée) —
        // transposée des lignes de raster.rs.
        let m = mat3x3<f32>(
            vec3<f32>(0.213 + cs * 0.787 - sn * 0.213, 0.213 - cs * 0.213 + sn * 0.143, 0.213 - cs * 0.213 - sn * 0.787),
            vec3<f32>(0.715 - cs * 0.715 - sn * 0.715, 0.715 + cs * 0.285 + sn * 0.140, 0.715 - cs * 0.715 + sn * 0.715),
            vec3<f32>(0.072 - cs * 0.072 + sn * 0.928, 0.072 - cs * 0.072 - sn * 0.283, 0.072 + cs * 0.928 + sn * 0.072),
        );
        c = m * c;
    }
    return c;
}

// Échantillonne la source (mire procédurale ou texture vidéo).
fn sample_source(mode: u32, p: vec2<f32>) -> vec3<f32> {
    switch mode {
        case 1u: { return grid_color(p); }
        case 2u: { return checker_color(p); }
        case 3u: { return corners_color(p); }
        default: { return textureSampleLevel(video_tex, video_smp, p, 0.0).rgb; }
    }
}

// Miroir kaléidoscope puis pixellisation — MÊMES formules que raster.rs.
fn apply_uv_effects(t_in: vec2<f32>) -> vec2<f32> {
    var t = t_in;
    let mirror = u.fx_b.x;
    if mirror > 0.0 {
        let mirrored = abs(t.x * 2.0 - 1.0);
        t.x = t.x + mirror * (mirrored - t.x);
    }
    let pixelate = u.fx_a.x;
    if pixelate > 0.0 {
        let blocks = 256.0 - pixelate * 248.0;
        t = (floor(t * blocks) + vec2<f32>(0.5)) / blocks;
    }
    return clamp(t, vec2<f32>(0.0), vec2<f32>(1.0));
}

// Hachage pseudo-aléatoire stable (même formule que raster.rs).
fn hash2(x: f32, y: f32) -> f32 {
    return abs(fract(sin(x * 12.9898 + y * 78.233) * 43758.547));
}

// Postérisation puis bruit animé, après la correction couleur.
fn apply_pixel_effects(rgb_in: vec3<f32>, t: vec2<f32>) -> vec3<f32> {
    var rgb = rgb_in;
    let posterize = u.fx_a.y;
    if posterize > 0.0 {
        let levels = 64.0 - posterize * 61.0;
        rgb = floor(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * levels) / levels;
    }
    let noise = u.fx_a.z;
    if noise > 0.0 {
        let time = u.fx_b.y;
        let n = hash2(t.x * 311.7 + fract(time) * 17.0, t.y * 173.3 + fract(time) * 29.0);
        rgb = rgb + vec3<f32>((n - 0.5) * noise * 0.35);
    }
    return rgb;
}

// Le pixel de sortie p est-il couvert par un masque ? Test « même côté »
// sur les quatre arêtes — MÊME formule que raster.rs (dans_un_masque).
fn dans_un_masque(p: vec2<f32>) -> bool {
    let nb = u32(u.blending_b.y);
    for (var m = 0u; m < nb; m = m + 1u) {
        let a = u.masques[m * 2u];
        let b = u.masques[m * 2u + 1u];
        let c0 = a.xy;
        let c1 = a.zw;
        let c2 = b.xy;
        let c3 = b.zw;
        var positifs = 0u;
        var negatifs = 0u;
        // Arête c0→c1, c1→c2, c2→c3, c3→c0.
        for (var i = 0u; i < 4u; i = i + 1u) {
            var s = c0; var e = c1;
            if i == 1u { s = c1; e = c2; }
            if i == 2u { s = c2; e = c3; }
            if i == 3u { s = c3; e = c0; }
            let croix = (e.x - s.x) * (p.y - s.y) - (e.y - s.y) * (p.x - s.x);
            if croix >= 0.0 { positifs = positifs + 1u; }
            if croix <= 0.0 { negatifs = negatifs + 1u; }
        }
        if positifs == 4u || negatifs == 4u {
            return true;
        }
    }
    return false;
}

// Rampe d'un bord du fondu — MÊME formule que raster.rs (facteur_blending).
fn rampe_blending(distance: f32, largeur: f32, gamma: f32) -> f32 {
    if largeur <= 0.0 || distance >= largeur {
        return 1.0;
    }
    return pow(max(distance, 0.0) / largeur, gamma);
}

fn facteur_blending(p: vec2<f32>) -> f32 {
    let gamma = max(u.blending_b.x, 0.5);
    var facteur = 1.0;
    facteur = facteur * rampe_blending(p.x, u.blending_a.x, gamma);
    facteur = facteur * rampe_blending(1.0 - p.x, u.blending_a.y, gamma);
    facteur = facteur * rampe_blending(p.y, u.blending_a.z, gamma);
    facteur = facteur * rampe_blending(1.0 - p.y, u.blending_a.w, gamma);
    return facteur;
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let black = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let mode = u32(u.misc.z);
    if mode == 0u {
        return black;
    }
    let p = frag.xy / u.misc.xy;
    // Masques : zones NOIRES en espace de sortie, avant tout calcul.
    if dans_un_masque(p) {
        return black;
    }
    // 1. Warp inverse : hors du quad de mapping, rien n'est projeté.
    let q = apply3(u.warp_inv_c0, u.warp_inv_c1, u.warp_inv_c2, p);
    if q.x < 0.0 || q.x > 1.0 || q.y < 0.0 || q.y > 1.0 {
        return black;
    }
    // 2. Flip + rotation inverse + recadrage.
    let t_brut = apply3(u.uv_c0, u.uv_c1, u.uv_c2, q);
    if t_brut.x < 0.0 || t_brut.x > 1.0 || t_brut.y < 0.0 || t_brut.y > 1.0 {
        return black;
    }
    // 3. Effets géométriques puis échantillonnage (accentuation : 5
    //    prélèvements en croix, décalage fixe comme raster.rs).
    let t = apply_uv_effects(t_brut);
    var rgb = sample_source(mode, t);
    let sharpen = u.fx_a.w;
    if sharpen > 0.0 {
        let o = 1.0 / 512.0;
        let voisins = sample_source(mode, clamp(t - vec2<f32>(o, 0.0), vec2<f32>(0.0), vec2<f32>(1.0)))
            + sample_source(mode, clamp(t + vec2<f32>(o, 0.0), vec2<f32>(0.0), vec2<f32>(1.0)))
            + sample_source(mode, clamp(t - vec2<f32>(0.0, o), vec2<f32>(0.0), vec2<f32>(1.0)))
            + sample_source(mode, clamp(t + vec2<f32>(0.0, o), vec2<f32>(0.0), vec2<f32>(1.0)));
        let k = sharpen * 0.8;
        rgb = rgb * (1.0 + 4.0 * k) - voisins * k;
    }
    // 4. Correction couleur puis effets de teinte.
    rgb = apply_color(rgb);
    rgb = apply_pixel_effects(rgb, t);
    // 5. Fondu de bords : atténuation finale en espace de sortie.
    rgb = rgb * facteur_blending(p);
    return vec4<f32>(rgb, 1.0);
}
