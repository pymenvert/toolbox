#!/usr/bin/env python3
"""Implémentation de référence de l'homographie 4 coins (corner pinning).

Rôle : servir de vérité mathématique pour le code Rust/GLSL du moteur.
- calcule la matrice 3x3 qui envoie le quad unité (0,0)(1,0)(1,1)(0,1)
  sur les 4 coins choisis par l'utilisateur ;
- vérifie les propriétés attendues (aller-retour, inversibilité, identité) ;
- imprime des vecteurs de test embarqués dans les tests Rust
  (crates/engine/src/homography.rs).

Méthode : résolution directe du système linéaire 8x8 (DLT avec h33=1),
sans dépendance externe (pas de numpy) pour rester exécutable partout.
"""

from __future__ import annotations


Matrix3 = list[list[float]]
Point = tuple[float, float]

# Quad unité, ordre = celui du projet : 0=HG, 1=HD, 2=BD, 3=BG.
UNIT_QUAD: list[Point] = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]


def solve_linear(a: list[list[float]], b: list[float]) -> list[float]:
    """Résout a·x = b par élimination de Gauss avec pivot partiel."""
    n = len(b)
    m = [row[:] + [b[i]] for i, row in enumerate(a)]
    for col in range(n):
        pivot = max(range(col, n), key=lambda r: abs(m[r][col]))
        if abs(m[pivot][col]) < 1e-12:
            raise ValueError("système singulier : coins dégénérés (colinéaires ?)")
        m[col], m[pivot] = m[pivot], m[col]
        for r in range(col + 1, n):
            f = m[r][col] / m[col][col]
            for c in range(col, n + 1):
                m[r][c] -= f * m[col][c]
    x = [0.0] * n
    for r in range(n - 1, -1, -1):
        x[r] = (m[r][n] - sum(m[r][c] * x[c] for c in range(r + 1, n))) / m[r][r]
    return x


def homography_from_corners(corners: list[Point]) -> Matrix3:
    """Matrice H (3x3, h33=1) telle que H · quad_unité = corners.

    Pour chaque paire (u,v) → (x,y) :
        x = (h11·u + h12·v + h13) / (h31·u + h32·v + 1)
        y = (h21·u + h22·v + h23) / (h31·u + h32·v + 1)
    soit 8 équations linéaires pour 8 inconnues.
    """
    if len(corners) != 4:
        raise ValueError("il faut exactement 4 coins")
    a: list[list[float]] = []
    b: list[float] = []
    for (u, v), (x, y) in zip(UNIT_QUAD, corners):
        a.append([u, v, 1, 0, 0, 0, -u * x, -v * x])
        b.append(x)
        a.append([0, 0, 0, u, v, 1, -u * y, -v * y])
        b.append(y)
    h = solve_linear(a, b)
    m: Matrix3 = [[h[0], h[1], h[2]], [h[3], h[4], h[5]], [h[6], h[7], 1.0]]
    # Le système 8x8 peut se résoudre même pour des coins dégénérés (3 coins
    # colinéaires) : la dégénérescence apparaît alors dans le déterminant de H,
    # pas dans le pivot. On la refuse ici — le moteur fera pareil.
    if abs(det3(m)) < 1e-9:
        raise ValueError("coins dégénérés : l'homographie résultante n'est pas inversible")
    return m


def det3(h: Matrix3) -> float:
    (a, b, c), (d, e, f), (g, hh, i) = h
    return a * (e * i - f * hh) - b * (d * i - f * g) + c * (d * hh - e * g)


def apply(h: Matrix3, p: Point) -> Point:
    """Applique H à un point (coordonnées homogènes, division perspective)."""
    u, v = p
    w = h[2][0] * u + h[2][1] * v + h[2][2]
    if abs(w) < 1e-12:
        raise ValueError("point à l'infini")
    x = (h[0][0] * u + h[0][1] * v + h[0][2]) / w
    y = (h[1][0] * u + h[1][1] * v + h[1][2]) / w
    return (x, y)


def invert(h: Matrix3) -> Matrix3:
    """Inverse 3x3 par cofacteurs. Utile côté moteur : le fragment shader a
    besoin de H⁻¹ (sortie → source) pour échantillonner la texture."""
    (a, b, c), (d, e, f), (g, hh, i) = h
    det = a * (e * i - f * hh) - b * (d * i - f * g) + c * (d * hh - e * g)
    if abs(det) < 1e-12:
        raise ValueError("matrice non inversible")
    return [
        [(e * i - f * hh) / det, (c * hh - b * i) / det, (b * f - c * e) / det],
        [(f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det],
        [(d * hh - e * g) / det, (b * g - a * hh) / det, (a * e - b * d) / det],
    ]


def check(name: str, ok: bool) -> None:
    print(f"  [{'OK' if ok else 'ÉCHEC'}] {name}")
    if not ok:
        raise SystemExit(f"échec : {name}")


def close(p: Point, q: Point, eps: float = 1e-9) -> bool:
    return abs(p[0] - q[0]) < eps and abs(p[1] - q[1]) < eps


def main() -> None:
    print("Vérification de l'implémentation de référence :")

    # 1. Identité : le quad unité vers lui-même => matrice identité.
    h_id = homography_from_corners(UNIT_QUAD)
    check(
        "quad unité → identité",
        all(abs(h_id[r][c] - (1.0 if r == c else 0.0)) < 1e-9 for r in range(3) for c in range(3)),
    )

    # 2. Cas réaliste : projecteur de biais (keystone typique).
    corners: list[Point] = [(0.08, 0.05), (0.97, 0.02), (1.0, 0.93), (0.03, 0.98)]
    h = homography_from_corners(corners)
    for src, dst in zip(UNIT_QUAD, corners):
        check(f"coin {src} → {dst}", close(apply(h, src), dst))

    # 3. Aller-retour par l'inverse (ce que fera le fragment shader).
    h_inv = invert(h)
    for p in [(0.5, 0.5), (0.25, 0.75), (0.1, 0.9), (0.999, 0.001)]:
        check(f"H⁻¹(H({p})) = {p}", close(apply(h_inv, apply(h, p)), p))

    # 4. Coins dégénérés refusés (3 points colinéaires).
    try:
        homography_from_corners([(0, 0), (0.5, 0), (1, 0), (0, 1)])
        check("coins dégénérés rejetés", False)
    except ValueError:
        check("coins dégénérés rejetés", True)

    # 5. Vecteurs de test pour les tests Rust (à copier tels quels).
    print("\nVecteurs de test pour crates/engine/src/homography.rs :")
    print(f"  corners = {corners}")
    print("  H (row-major) =")
    for row in h:
        print(f"    {row[0]:.12f}, {row[1]:.12f}, {row[2]:.12f},")
    mid = apply(h, (0.5, 0.5))
    print(f"  H(0.5, 0.5) = ({mid[0]:.12f}, {mid[1]:.12f})")

    print("\nTout est vérifié.")


if __name__ == "__main__":
    main()
