#version 300 es
// Échantillonnage de la source + orientation/recadrage + correction couleur
// + mires de test intégrées (V1 complète — voir DECISIONS.md).
//
// L'ordre des opérations couleur est fixé et documenté :
//   texture → saturation → hue → contrast → brightness → gains RGB → gamma
// Il devra rester identique dans la preview web pour que "ce que je règle"
// = "ce que le VP projette".

precision highp float;

in vec3 v_texcoord;
out vec4 frag_color;

uniform sampler2D u_source;

// Orientation + recadrage : UV de sortie → UV de texture source
// (flip, rotation inverse, fenêtre de crop — voir engine::render).
uniform mat3 u_uv_transform;

// Correction couleur (neutres : 1,1,1,1,0,(1,1,1)).
uniform float u_brightness; // 0..2
uniform float u_contrast;   // 0..2
uniform float u_gamma;      // 0.2..4
uniform float u_saturation; // 0..2
uniform float u_hue;        // -180..180 (degrés)
uniform vec3 u_gain;        // gains RGB, 0..2 chacun

// Mire de test : 0 = média, 1 = grille, 2 = damier, 3 = coins.
uniform int u_pattern;

// Luminance Rec.709.
const vec3 LUMA = vec3(0.2126, 0.7152, 0.0722);

vec3 apply_hue(vec3 color, float degrees) {
    // Rotation de teinte par la méthode de l'axe YIQ approximé — suffisant
    // pour un réglage artistique, peu coûteux (pas de conversion HSV).
    float rad = radians(degrees);
    float c = cos(rad);
    float s = sin(rad);
    mat3 rot = mat3(
        0.299 + 0.701 * c + 0.168 * s, 0.587 - 0.587 * c + 0.330 * s, 0.114 - 0.114 * c - 0.497 * s,
        0.299 - 0.299 * c - 0.328 * s, 0.587 + 0.413 * c + 0.035 * s, 0.114 - 0.114 * c + 0.292 * s,
        0.299 - 0.300 * c + 1.250 * s, 0.587 - 0.588 * c - 1.050 * s, 0.114 + 0.886 * c - 0.203 * s
    );
    return color * rot;
}

// Grille de convergence 8×8 : lignes blanches d'~2 px sur fond sombre.
vec3 pattern_grid(vec2 uv) {
    vec2 cell = fract(uv * 8.0);
    vec2 width = fwidth(uv) * 8.0 * 1.5;
    vec2 line = step(cell, width) + step(1.0 - width, cell);
    float on = clamp(line.x + line.y, 0.0, 1.0);
    return mix(vec3(0.08), vec3(1.0), on);
}

// Damier 8×8.
vec3 pattern_checker(vec2 uv) {
    vec2 cells = floor(uv * 8.0);
    float parity = mod(cells.x + cells.y, 2.0);
    return mix(vec3(0.1), vec3(0.9), parity);
}

// Identification des coins : carré coloré par coin (0=HG rouge, 1=HD vert,
// 2=BD bleu, 3=BG jaune) + croix centrale. Les numéros sont affichés par
// l'UI ; ici la couleur suffit à identifier chaque coin depuis la scène.
vec3 pattern_corners(vec2 uv) {
    float m = 0.18; // taille des marqueurs
    if (uv.x < m && uv.y < m) { return vec3(1.0, 0.15, 0.15); }
    if (uv.x > 1.0 - m && uv.y < m) { return vec3(0.15, 1.0, 0.15); }
    if (uv.x > 1.0 - m && uv.y > 1.0 - m) { return vec3(0.2, 0.4, 1.0); }
    if (uv.x < m && uv.y > 1.0 - m) { return vec3(1.0, 0.9, 0.15); }
    // Croix centrale fine.
    vec2 w = fwidth(uv) * 2.0;
    if (abs(uv.x - 0.5) < w.x || abs(uv.y - 0.5) < w.y) { return vec3(1.0); }
    return pattern_grid(uv) * 0.6;
}

void main() {
    // Division perspective de la coordonnée homogène (voir warp.vert).
    vec2 uv = v_texcoord.xy / v_texcoord.z;

    // Hors du quad source : noir (les bords du warp ne doivent rien "étirer").
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        frag_color = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }

    vec3 color;
    if (u_pattern == 1) {
        color = pattern_grid(uv);
    } else if (u_pattern == 2) {
        color = pattern_checker(uv);
    } else if (u_pattern == 3) {
        color = pattern_corners(uv);
    } else {
        // Orientation + recadrage, puis échantillonnage du média.
        vec3 suv = u_uv_transform * vec3(uv, 1.0);
        color = texture(u_source, suv.xy / suv.z).rgb;
    }

    // Saturation : interpolation depuis le gris de même luminance.
    float luma = dot(color, LUMA);
    color = mix(vec3(luma), color, u_saturation);

    // Teinte.
    color = apply_hue(color, u_hue);

    // Contraste autour du gris moyen, puis luminosité, puis gains RGB.
    color = (color - 0.5) * u_contrast + 0.5;
    color *= u_brightness;
    color *= u_gain;

    // Gamma (clamp avant pow : pow(x<0) est indéfini en GLSL).
    color = pow(clamp(color, 0.0, 1.0), vec3(1.0 / u_gamma));

    frag_color = vec4(color, 1.0);
}
