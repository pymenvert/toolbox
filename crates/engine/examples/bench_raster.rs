//! Banc de mesure du rendu CPU (`cargo run --release -p toolbox-engine
//! --example bench_raster`) : ms/frame et fps équivalents pour plusieurs
//! définitions, avec une scène chargée (mire + couleur + effets + blending
//! + masque) et une scène simple (mire seule).
//!
//! Sert à mesurer AVANT d'optimiser, et à re-mesurer après — les chiffres
//! vont dans le message de commit.

// Banc de mesure, pas du code de prod : un état invalide doit faire
// échouer le banc immédiatement.
#![allow(clippy::expect_used)]

use toolbox_core::{Command, NodeState};
use toolbox_engine::raster::render_frame;

fn scene_chargee() -> NodeState {
    let mut state = NodeState::default();
    let commandes = [
        Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Grid),
        },
        Command::CornerSet {
            index: 0,
            x: 0.05,
            y: 0.08,
        },
        Command::CornerSet {
            index: 2,
            x: 0.93,
            y: 0.95,
        },
        Command::ColorSet {
            param: toolbox_core::ColorParam::Saturation,
            value: 1.4,
        },
        Command::ColorSet {
            param: toolbox_core::ColorParam::Gamma,
            value: 1.8,
        },
        Command::EffectSet {
            param: toolbox_core::state::EffectParam::Noise,
            value: 0.3,
        },
        Command::EffectSet {
            param: toolbox_core::state::EffectParam::Sharpen,
            value: 0.4,
        },
        Command::BlendingSet {
            gauche: 0.15,
            droite: 0.15,
            haut: 0.0,
            bas: 0.0,
            gamma: 2.2,
        },
        Command::MasqueSet {
            index: 0,
            corners: [
                toolbox_core::state::Corner { x: 0.4, y: 0.4 },
                toolbox_core::state::Corner { x: 0.6, y: 0.4 },
                toolbox_core::state::Corner { x: 0.6, y: 0.6 },
                toolbox_core::state::Corner { x: 0.4, y: 0.6 },
            ],
        },
    ];
    for commande in commandes {
        state.apply(&commande).expect("commande de scène");
    }
    state
}

fn scene_simple() -> NodeState {
    let mut state = NodeState::default();
    state
        .apply(&Command::SetTestPattern {
            pattern: Some(toolbox_core::TestPattern::Grid),
        })
        .expect("mire");
    state
}

fn mesure(nom: &str, state: &NodeState, largeur: u32, hauteur: u32) {
    let mut out = vec![0u32; (largeur * hauteur) as usize];
    // Échauffement.
    render_frame(state, None, 0.0, largeur, hauteur, &mut out);
    let iterations = 30u32;
    let depart = std::time::Instant::now();
    for i in 0..iterations {
        render_frame(state, None, i as f32 * 0.03, largeur, hauteur, &mut out);
    }
    let total = depart.elapsed();
    let par_frame = total / iterations;
    println!(
        "{nom:<16} {largeur}x{hauteur:<5} {:>8.2} ms/frame  ({:>6.1} fps)",
        par_frame.as_secs_f64() * 1000.0,
        1.0 / par_frame.as_secs_f64()
    );
}

fn main() {
    let chargee = scene_chargee();
    let simple = scene_simple();
    for (largeur, hauteur) in [(640u32, 360u32), (1280, 720), (1920, 1080)] {
        mesure("scène simple", &simple, largeur, hauteur);
        mesure("scène chargée", &chargee, largeur, hauteur);
    }
}
