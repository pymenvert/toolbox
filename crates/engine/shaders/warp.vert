#version 300 es
// Warp 4 coins : le quad unité est déformé par l'homographie u_homography
// (calculée côté CPU par toolbox-engine::homography, envoyée column-major).
//
// Approche vertex-shader : on transforme les sommets du quad et on laisse
// l'interpolation perspective du GPU faire le reste. Pour que l'interpolation
// soit correcte (non affine), on passe les coordonnées de texture en vec3
// homogène (v_texcoord.xy / v_texcoord.z dans le fragment shader).

precision highp float;

// Quad unité : positions dans [0,1]².
in vec2 a_position;

// Homographie quad unité → coins réglés par l'utilisateur (espace [0,1]²).
uniform mat3 u_homography;

// Coordonnée de texture homogène (perspective-correcte).
out vec3 v_texcoord;

void main() {
    // Position de sortie : coin déformé, converti de [0,1]² vers le clip
    // space [-1,1]² (y inversé : (0,0) = haut-gauche dans notre convention).
    vec3 warped = u_homography * vec3(a_position, 1.0);
    vec2 ndc = (warped.xy / warped.z) * 2.0 - 1.0;
    gl_Position = vec4(ndc.x, -ndc.y, 0.0, 1.0);

    // Coordonnée de texture : la position SOURCE (non déformée), pondérée par
    // w du point déformé pour une interpolation perspective correcte.
    v_texcoord = vec3(a_position * warped.z, warped.z);
}
