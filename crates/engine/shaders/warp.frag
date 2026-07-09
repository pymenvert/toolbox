#version 300 es
// Échantillonnage de la source + correction couleur MVP
// (brightness / contrast / gamma / saturation / hue — voir DECISIONS.md).
//
// L'ordre des opérations est fixé et documenté :
//   texture → saturation → hue → contrast → brightness → gamma
// Il devra rester identique dans la preview web pour que "ce que je règle"
// = "ce que le VP projette".

precision highp float;

in vec3 v_texcoord;
out vec4 frag_color;

uniform sampler2D u_source;

// Correction couleur (neutres : 1,1,1,1,0).
uniform float u_brightness; // 0..2
uniform float u_contrast;   // 0..2
uniform float u_gamma;      // 0.2..4
uniform float u_saturation; // 0..2
uniform float u_hue;        // -180..180 (degrés)

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

void main() {
    // Division perspective de la coordonnée homogène (voir warp.vert).
    vec2 uv = v_texcoord.xy / v_texcoord.z;

    // Hors du quad source : noir (les bords du warp ne doivent rien "étirer").
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        frag_color = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }

    vec3 color = texture(u_source, uv).rgb;

    // Saturation : interpolation depuis le gris de même luminance.
    float luma = dot(color, LUMA);
    color = mix(vec3(luma), color, u_saturation);

    // Teinte.
    color = apply_hue(color, u_hue);

    // Contraste autour du gris moyen, puis luminosité.
    color = (color - 0.5) * u_contrast + 0.5;
    color *= u_brightness;

    // Gamma (clamp avant pow : pow(x<0) est indéfini en GLSL).
    color = pow(clamp(color, 0.0, 1.0), vec3(1.0 / u_gamma));

    frag_color = vec4(color, 1.0);
}
