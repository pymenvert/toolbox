//! Fondu entre presets (brief 7.4) : le service « fader ».
//!
//! Le bus valide `preset_fade` et émet [`Event::PresetFadeStarted`] ; ce
//! service, abonné aux événements, mène l'interpolation en renvoyant des
//! commandes ordinaires (~30 pas/s) : chaque interface (web UI, OSC feedback,
//! fenêtre de sortie) voit donc le fondu comme n'importe quel réglage manuel.
//!
//! Glissent en continu : coins du mapping, recadrage, couleur, effets,
//! volume. Basculent à la fin : rotation, miroirs, mire, bypass du mapping.
//! Jamais touchés : média, transport, playlist — un fondu n'interrompt pas
//! le show. Un nouveau fondu reprend depuis l'état courant (retarget doux).

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{info, warn};

use crate::bus::{BusHandle, Source};
use crate::command::{ColorParam, Command};
use crate::preset::{MappingStore, PresetStore};
use crate::state::{EffectParam, Event, NodeState};

/// Cadence d'interpolation (~30 pas par seconde).
const TICK: Duration = Duration::from_millis(33);

/// Adoucissement « smoothstep » : départ et arrivée sans à-coup.
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Les commandes d'un pas de fondu entre `from` et `to`, à la progression
/// `t` ∈ 0..=1 (déjà adoucie). Seuls les paramètres qui diffèrent entre les
/// deux états sont émis ; à `t = 1.0` s'ajoutent les bascules discrètes
/// (rotation, miroirs, mire, bypass).
pub fn plan(from: &NodeState, to: &NodeState, t: f32) -> Vec<Command> {
    let t = t.clamp(0.0, 1.0);
    let differs = |a: f32, b: f32| (a - b).abs() > 1e-6;
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    let mut commands = Vec::new();

    for index in 0..4u8 {
        let a = from.mapping.corners[usize::from(index)];
        let b = to.mapping.corners[usize::from(index)];
        if differs(a.x, b.x) || differs(a.y, b.y) {
            commands.push(Command::CornerSet {
                index,
                x: lerp(a.x, b.x),
                y: lerp(a.y, b.y),
            });
        }
    }

    let (ca, cb) = (&from.mapping.crop, &to.mapping.crop);
    if differs(ca.left, cb.left)
        || differs(ca.top, cb.top)
        || differs(ca.right, cb.right)
        || differs(ca.bottom, cb.bottom)
    {
        commands.push(Command::SetCrop {
            left: lerp(ca.left, cb.left),
            top: lerp(ca.top, cb.top),
            right: lerp(ca.right, cb.right),
            bottom: lerp(ca.bottom, cb.bottom),
        });
    }

    let colors = [
        (
            ColorParam::Brightness,
            from.color.brightness,
            to.color.brightness,
        ),
        (ColorParam::Contrast, from.color.contrast, to.color.contrast),
        (ColorParam::Gamma, from.color.gamma, to.color.gamma),
        (
            ColorParam::Saturation,
            from.color.saturation,
            to.color.saturation,
        ),
        (ColorParam::Hue, from.color.hue, to.color.hue),
        (ColorParam::GainR, from.color.gain_r, to.color.gain_r),
        (ColorParam::GainG, from.color.gain_g, to.color.gain_g),
        (ColorParam::GainB, from.color.gain_b, to.color.gain_b),
    ];
    for (param, a, b) in colors {
        if differs(a, b) {
            commands.push(Command::ColorSet {
                param,
                value: lerp(a, b),
            });
        }
    }

    let effects = [
        (
            EffectParam::Pixelate,
            from.effects.pixelate,
            to.effects.pixelate,
        ),
        (
            EffectParam::Posterize,
            from.effects.posterize,
            to.effects.posterize,
        ),
        (EffectParam::Noise, from.effects.noise, to.effects.noise),
        (
            EffectParam::Sharpen,
            from.effects.sharpen,
            to.effects.sharpen,
        ),
        (EffectParam::Mirror, from.effects.mirror, to.effects.mirror),
    ];
    for (param, a, b) in effects {
        if differs(a, b) {
            commands.push(Command::EffectSet {
                param,
                value: lerp(a, b),
            });
        }
    }

    if differs(from.player.volume, to.player.volume) {
        commands.push(Command::SetVolume {
            volume: lerp(from.player.volume, to.player.volume),
        });
    }

    // Fondu de bords : continu, il glisse aussi.
    let (ba, bb) = (&from.blending, &to.blending);
    if differs(ba.gauche, bb.gauche)
        || differs(ba.droite, bb.droite)
        || differs(ba.haut, bb.haut)
        || differs(ba.bas, bb.bas)
        || differs(ba.gamma, bb.gamma)
    {
        commands.push(Command::BlendingSet {
            gauche: lerp(ba.gauche, bb.gauche),
            droite: lerp(ba.droite, bb.droite),
            haut: lerp(ba.haut, bb.haut),
            bas: lerp(ba.bas, bb.bas),
            gamma: lerp(ba.gamma, bb.gamma),
        });
    }

    if t >= 1.0 {
        if from.mapping.enabled != to.mapping.enabled {
            commands.push(Command::SetMappingEnabled {
                enabled: to.mapping.enabled,
            });
        }
        if from.mapping.rotation != to.mapping.rotation {
            commands.push(Command::SetRotation {
                degrees: to.mapping.rotation.degrees(),
            });
        }
        if from.mapping.flip_h != to.mapping.flip_h || from.mapping.flip_v != to.mapping.flip_v {
            commands.push(Command::SetFlip {
                horizontal: to.mapping.flip_h,
                vertical: to.mapping.flip_v,
            });
        }
        if from.test_pattern != to.test_pattern {
            commands.push(Command::SetTestPattern {
                pattern: to.test_pattern,
            });
        }
        // Bascules discrètes ajoutées après coup (masques v2, mesh/LUT v3) :
        // sans elles, un fondu « réussi » laissait l'ancien mesh, les anciens
        // masques et l'ancienne LUT en place — l'image finale ne
        // correspondait PAS au preset cible, silencieusement.
        if from.mapping.mesh != to.mapping.mesh {
            match &to.mapping.mesh {
                Some(mesh) => commands.push(Command::MeshSet {
                    colonnes: mesh.colonnes,
                    lignes: mesh.lignes,
                    offsets: mesh.offsets.clone(),
                }),
                None => commands.push(Command::MeshReset),
            }
        }
        if from.lut != to.lut {
            commands.push(Command::LutSet {
                name: to.lut.clone(),
            });
        }
        if from.masques != to.masques {
            // Resynchronise index par index, puis retire les excédents (du
            // plus haut index au plus bas pour garder les index valides).
            for (index, masque) in to.masques.iter().enumerate() {
                if let Ok(index) = u8::try_from(index) {
                    commands.push(Command::MasqueSet {
                        index,
                        corners: masque.corners,
                    });
                }
            }
            for index in (to.masques.len()..from.masques.len()).rev() {
                if let Ok(index) = u8::try_from(index) {
                    commands.push(Command::MasqueSupprime { index });
                }
            }
        }
    }

    commands
}

/// Un fondu en cours.
struct Fade {
    from: NodeState,
    to: NodeState,
    started: Instant,
    duration: Duration,
}

/// Boucle du service : attend les [`Event::PresetFadeStarted`] et
/// [`Event::MappingFadeStarted`], et mène chaque fondu à ~30 pas/s. Un
/// nouveau fondu remplace l'ancien en repartant de l'état courant.
pub async fn run(
    bus: BusHandle,
    store: PresetStore,
    mappings: MappingStore,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut events = bus.subscribe();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut current: Option<Fade> = None;
    info!("fader de presets prêt");
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            received = events.recv() => match received {
                Ok(Event::PresetFadeStarted { name, seconds }) => match store.load(&name) {
                    Ok(target) => {
                        info!(%name, seconds, "fondu démarré");
                        current = Some(Fade {
                            from: bus.snapshot(),
                            to: target,
                            started: Instant::now(),
                            duration: Duration::from_secs_f32(seconds),
                        });
                        ticker.reset();
                    }
                    // Le bus a déjà validé le preset ; un échec ici est une
                    // course rare (fichier supprimé entre-temps).
                    Err(err) => warn!(%name, %err, "fondu abandonné : preset illisible"),
                },
                // Fondu du mapping seul : la cible est l'état courant avec
                // le mapping remplacé — seul le calage glisse.
                Ok(Event::MappingFadeStarted { name, seconds }) => match mappings.load(&name) {
                    Ok(mapping) => {
                        info!(%name, seconds, "fondu de mapping démarré");
                        let from = bus.snapshot();
                        let mut to = from.clone();
                        to.mapping = mapping;
                        current = Some(Fade {
                            from,
                            to,
                            started: Instant::now(),
                            duration: Duration::from_secs_f32(seconds),
                        });
                        ticker.reset();
                    }
                    Err(err) => warn!(%name, %err, "fondu abandonné : mapping illisible"),
                },
                // Reprise en main directe pendant un fondu : charger un preset
                // ou un mapping (ou réinitialiser) DOIT annuler le fondu en
                // cours, sinon les pas suivants (valeurs absolues figées au
                // départ) écrasent l'état fraîchement chargé et l'image repart
                // vers l'ancienne cible. Ces événements ne sont jamais émis
                // par les pas du fader : pas d'auto-annulation.
                Ok(Event::PresetLoaded { .. })
                | Ok(Event::StateReplaced { .. })
                | Ok(Event::MappingLoaded { .. })
                | Ok(Event::MappingReset) => {
                    if current.take().is_some() {
                        info!("fondu annulé par un chargement direct");
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(missed, "fader en retard : événements sautés");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = ticker.tick(), if current.is_some() => {
                let Some(fade) = &current else { continue };
                let raw = fade.started.elapsed().as_secs_f32()
                    / fade.duration.as_secs_f32().max(f32::EPSILON);
                let t = ease(raw.min(1.0));
                let done = raw >= 1.0;
                for command in plan(&fade.from, &fade.to, if done { 1.0 } else { t }) {
                    if !bus.send(Source::Internal, command).await {
                        return;
                    }
                }
                if done {
                    info!("fondu terminé");
                    current = None;
                }
            }
        }
    }
    info!("fader de presets arrêté");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;
    use crate::state::Corner;

    fn state_with(volume: f32, corner0_x: f32) -> NodeState {
        let mut state = NodeState::default();
        state.player.volume = volume;
        state.mapping.corners[0] = Corner {
            x: corner0_x,
            y: 0.0,
        };
        state
    }

    #[test]
    fn ease_is_smooth_and_bounded() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!(ease(0.5) > 0.49 && ease(0.5) < 0.51);
        assert!(ease(-1.0) >= 0.0 && ease(2.0) <= 1.0);
    }

    #[test]
    fn plan_between_identical_states_is_empty() {
        let state = state_with(0.5, 0.1);
        assert!(plan(&state, &state, 0.5).is_empty());
        assert!(plan(&state, &state, 1.0).is_empty());
    }

    #[test]
    fn plan_interpolates_continuous_params() {
        let from = state_with(0.0, 0.0);
        let to = state_with(1.0, 0.2);
        let commands = plan(&from, &to, 0.5);
        assert!(commands.contains(&Command::SetVolume { volume: 0.5 }));
        assert!(commands.iter().any(|c| matches!(
            c,
            Command::CornerSet { index: 0, x, .. } if (x - 0.1).abs() < 1e-6
        )));
        // Pas de bascule discrète avant la fin.
        assert!(!commands
            .iter()
            .any(|c| matches!(c, Command::SetRotation { .. })));
    }

    #[test]
    fn plan_applies_discrete_switches_at_the_end() {
        let from = NodeState::default();
        let mut to = NodeState::default();
        to.mapping.rotation = crate::state::Rotation::R180;
        to.test_pattern = Some(crate::command::TestPattern::Grid);
        to.mapping.enabled = false;

        assert!(plan(&from, &to, 0.99).is_empty());
        let at_end = plan(&from, &to, 1.0);
        assert!(at_end.contains(&Command::SetRotation { degrees: 180 }));
        assert!(at_end.contains(&Command::SetMappingEnabled { enabled: false }));
        assert!(at_end.contains(&Command::SetTestPattern {
            pattern: Some(crate::command::TestPattern::Grid)
        }));
    }

    #[test]
    fn plan_applies_mesh_lut_masques_at_the_end() {
        let from = NodeState::default();
        let mut to = NodeState::default();
        to.mapping.mesh = Some(crate::state::MeshState {
            colonnes: 2,
            lignes: 2,
            offsets: vec![Corner { x: 0.0, y: 0.0 }; 4],
        });
        to.lut = Some("scene.cube".into());
        to.masques = vec![crate::state::Masque {
            corners: [Corner { x: 0.0, y: 0.0 }; 4],
        }];

        // Rien de tout ça avant la toute fin.
        assert!(plan(&from, &to, 0.99).is_empty());
        let fin = plan(&from, &to, 1.0);
        assert!(fin.iter().any(|c| matches!(
            c,
            Command::MeshSet {
                colonnes: 2,
                lignes: 2,
                ..
            }
        )));
        assert!(fin.contains(&Command::LutSet {
            name: Some("scene.cube".into())
        }));
        assert!(fin
            .iter()
            .any(|c| matches!(c, Command::MasqueSet { index: 0, .. })));

        // Sens inverse : mesh retiré, LUT retirée, masque excédentaire ôté.
        let retour = plan(&to, &from, 1.0);
        assert!(retour.contains(&Command::MeshReset));
        assert!(retour.contains(&Command::LutSet { name: None }));
        assert!(retour.contains(&Command::MasqueSupprime { index: 0 }));
    }

    /// Charger un preset PENDANT un fondu annule ce fondu : le chargement
    /// direct l'emporte, il n'est pas ré-écrasé pas à pas vers l'ancienne
    /// cible.
    #[tokio::test]
    async fn direct_load_cancels_an_ongoing_fade() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PresetStore::open(dir.path().join("presets")).expect("open");
        let mappings =
            MappingStore::open(dir.path().join("presets").join("mapping")).expect("open");
        let bus = Bus::new(256, 1024)
            .with_presets(store.clone())
            .with_mapping_presets(mappings.clone());
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(run(handle.clone(), store, mappings, shutdown_rx));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let poser_coin = |x: f32| Command::CornerSet {
            index: 0,
            x,
            y: 0.0,
        };
        // Preset « lent » : coin 0 à 0.5. Preset « coupe » : coin 0 à 0.9.
        handle.send(Source::Http, poser_coin(0.5)).await;
        handle
            .send(
                Source::Http,
                Command::PresetSave {
                    name: "lent".into(),
                },
            )
            .await;
        handle.send(Source::Http, poser_coin(0.9)).await;
        handle
            .send(
                Source::Http,
                Command::PresetSave {
                    name: "coupe".into(),
                },
            )
            .await;
        // Départ à 0.0, fondu LONG vers « lent ».
        handle.send(Source::Http, poser_coin(0.0)).await;
        handle
            .send(
                Source::Http,
                Command::PresetFade {
                    name: "lent".into(),
                    seconds: 2.0,
                },
            )
            .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        // En plein fondu, on charge « coupe » directement.
        handle
            .send(
                Source::Http,
                Command::PresetLoad {
                    name: "coupe".into(),
                },
            )
            .await;
        // Laisser le temps au fondu de re-écraser s'il n'était pas annulé.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let x = handle.snapshot().mapping.corners[0].x;
        assert!(
            x > 0.85,
            "le chargement direct doit tenir (coin à 0.9), pas être ramené vers 0.5 ; obtenu {x}"
        );
    }

    /// De bout en bout : le fondu amène l'état au preset cible sans toucher
    /// à la lecture en cours.
    #[tokio::test]
    async fn fade_reaches_the_target_preset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PresetStore::open(dir.path().join("presets")).expect("open");
        let mappings =
            MappingStore::open(dir.path().join("presets").join("mapping")).expect("open");
        let bus = Bus::new(256, 1024)
            .with_presets(store.clone())
            .with_mapping_presets(mappings.clone());
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(run(handle.clone(), store, mappings, shutdown_rx));
        // Laisser le fader s'abonner avant d'envoyer le fondu (course
        // broadcast déjà rencontrée sur le player).
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Preset cible : volume 0.2, coin 0 déplacé.
        handle
            .send(
                Source::Http,
                Command::CornerSet {
                    index: 0,
                    x: 0.3,
                    y: 0.1,
                },
            )
            .await;
        handle
            .send(Source::Http, Command::SetVolume { volume: 0.2 })
            .await;
        handle
            .send(
                Source::Http,
                Command::PresetSave {
                    name: "cible".into(),
                },
            )
            .await;
        // État de départ différent + lecture en cours.
        handle
            .send(
                Source::Http,
                Command::Load {
                    path: "a.mp4".into(),
                },
            )
            .await;
        handle.send(Source::Http, Command::Play).await;
        handle.send(Source::Http, Command::MappingReset).await;
        handle
            .send(Source::Http, Command::SetVolume { volume: 1.0 })
            .await;

        handle
            .send(
                Source::Http,
                Command::PresetFade {
                    name: "cible".into(),
                    seconds: 0.15,
                },
            )
            .await;

        // Attendre la fin du fondu (marge large pour les CI lentes).
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let state = handle.snapshot();
            let arrived = (state.player.volume - 0.2).abs() < 1e-5
                && (state.mapping.corners[0].x - 0.3).abs() < 1e-5;
            if arrived {
                // La lecture n'a pas été interrompue par le fondu.
                assert_eq!(state.player.transport, crate::state::Transport::Playing);
                assert_eq!(state.player.media.as_deref(), Some("a.mp4"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fondu jamais terminé : {state:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Le fondu de mapping ne fait glisser QUE le mapping : la couleur et le
    /// volume restent en place.
    #[tokio::test]
    async fn mapping_fade_moves_only_the_mapping() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PresetStore::open(dir.path().join("presets")).expect("open");
        let mappings =
            MappingStore::open(dir.path().join("presets").join("mapping")).expect("open");
        let bus = Bus::new(256, 1024)
            .with_presets(store.clone())
            .with_mapping_presets(mappings.clone());
        let handle = bus.handle();
        tokio::spawn(bus.run());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(run(handle.clone(), store, mappings, shutdown_rx));
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Mapping cible : coin 2 déplacé, sauvegardé seul.
        handle
            .send(
                Source::Http,
                Command::CornerSet {
                    index: 2,
                    x: 0.7,
                    y: 0.8,
                },
            )
            .await;
        handle
            .send(
                Source::Http,
                Command::MappingSave {
                    name: "salon".into(),
                },
            )
            .await;
        // État de départ : mapping neutre, couleur et volume personnalisés.
        handle.send(Source::Http, Command::MappingReset).await;
        handle
            .send(Source::Http, Command::SetVolume { volume: 0.6 })
            .await;
        handle
            .send(
                Source::Http,
                Command::ColorSet {
                    param: ColorParam::Brightness,
                    value: 1.4,
                },
            )
            .await;

        handle
            .send(
                Source::Http,
                Command::MappingFade {
                    name: "salon".into(),
                    seconds: 0.15,
                },
            )
            .await;

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let state = handle.snapshot();
            if (state.mapping.corners[2].x - 0.7).abs() < 1e-5
                && (state.mapping.corners[2].y - 0.8).abs() < 1e-5
            {
                // Couleur et volume n'ont pas bougé.
                assert!((state.player.volume - 0.6).abs() < 1e-6);
                assert!((state.color.brightness - 1.4).abs() < 1e-6);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fondu de mapping jamais terminé : {state:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
