//! Peintre GPU de la fenêtre de sortie (wgpu : Vulkan/DX12/Metal/GL selon la
//! machine — Vulkan V3DV sur Raspberry Pi 4/5).
//!
//! Le shader (`warp.wgsl`) reproduit EXACTEMENT la chaîne CPU de référence
//! testée dans [`crate::raster`] : warp inverse, uv_transform, mires
//! procédurales ou texture vidéo, correction couleur. Si l'initialisation
//! GPU échoue (pilote absent, VM…), la fenêtre retombe sur le peintre CPU.

use std::sync::Arc;

use tracing::{info, warn};
use winit::window::Window;

use toolbox_core::command::TestPattern;
use toolbox_core::state::{NodeState, Transport};
use toolbox_engine::{RenderParams, VideoFrame};

/// Uniforms du shader — disposition identique à `struct Uniforms` de
/// `warp.wgsl` (29 × vec4).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    warp_inv: [[f32; 4]; 3],
    uv: [[f32; 4]; 3],
    /// luminosité, contraste, gamma, saturation
    color_a: [f32; 4],
    /// teinte (radians), gains RVB
    color_b: [f32; 4],
    /// largeur, hauteur, mode, inutilisé
    misc: [f32; 4],
    /// pixellisation, postérisation, bruit, accentuation
    fx_a: [f32; 4],
    /// miroir, temps (secondes), colonnes et lignes du mesh (0 = aucun)
    fx_b: [f32; 4],
    /// fondu de bords : gauche, droite, haut, bas
    blending_a: [f32; 4],
    /// fondu de bords : gamma, nombre de masques, niveau de blackout,
    /// taille de grille de la LUT (0 = aucune)
    blending_b: [f32; 4],
    /// masques : 8 quadrilatères × 4 coins = 2 vec4 (x0,y0,x1,y1) chacun
    masques: [[f32; 4]; 16],
    /// mesh warp : 81 déplacements max (9×9), 2 par vec4 (xy puis zw)
    mesh: [[f32; 4]; 41],
}

/// Colonnes vec4 d'une matrice 3x3 exportée colonne-major (`to_gl`).
fn columns(gl: [f32; 9]) -> [[f32; 4]; 3] {
    [
        [gl[0], gl[1], gl[2], 0.0],
        [gl[3], gl[4], gl[5], 0.0],
        [gl[6], gl[7], gl[8], 0.0],
    ]
}

const IDENTITY_COLUMNS: [[f32; 4]; 3] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

/// Issue d'un rendu GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultatRendu {
    /// Frame présentée à l'écran.
    Presentee,
    /// Frame sautée transitoirement (fenêtre réduite/occultée, retard) — on
    /// réessaiera au prochain redraw, rien à faire.
    Sautee,
    /// Device GPU perdu (pilote réinitialisé, écran débranché) : l'appelant
    /// doit replier sur le rendu CPU pour ne pas rester en sortie noire.
    DevicePerdu,
}

/// Le peintre GPU : surface, pipeline et texture vidéo.
pub struct GpuPainter {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    sampler: wgpu::Sampler,
    bind_layout: wgpu::BindGroupLayout,
    video_texture: wgpu::Texture,
    video_size: (u32, u32),
    bind_group: wgpu::BindGroup,
    lut_buffer: wgpu::Buffer,
    /// Nom du fichier de la LUT téléversée (évite un re-téléversement par
    /// frame) et sa taille de grille (0 = aucune).
    lut_nom: Option<String>,
    lut_taille: u32,
}

impl GpuPainter {
    /// Initialise wgpu sur la fenêtre. Toute erreur est retournée en texte :
    /// l'appelant retombe sur le peintre CPU.
    pub fn new(window: Arc<Window>) -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("surface : {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("aucun GPU compatible : {e}"))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| format!("device : {e}"))?;

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| "surface incompatible avec l'adaptateur".to_string())?;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("warp"),
            source: wgpu::ShaderSource::Wgsl(include_str!("warp.wgsl").into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sortie"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // LUT 3D en buffer storage : trilinéaire calculé dans le
                // shader avec EXACTEMENT la même formule que Lut3d (CPU) —
                // pas de texture 3D filtrée, dont la précision dépendrait
                // du matériel.
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sortie"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("warp"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Filtrage linéaire : la vidéo est lissée à l'agrandissement (le
        // peintre CPU, lui, reste au plus proche voisin).
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let video_texture = create_video_texture(&device, 1, 1);
        // Buffer LUT factice (une entrée) tant qu'aucune LUT n'est chargée.
        let lut_buffer = create_lut_buffer(&device, &[0.0, 0.0, 0.0, 1.0]);
        let bind_group = create_bind_group(
            &device,
            &bind_layout,
            &uniforms,
            &video_texture,
            &sampler,
            &lut_buffer,
        );

        info!(backend = ?adapter.get_info().backend, gpu = %adapter.get_info().name, "rendu GPU actif");
        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            uniforms,
            sampler,
            bind_layout,
            video_texture,
            video_size: (1, 1),
            bind_group,
            lut_buffer,
            lut_nom: None,
            lut_taille: 0,
        })
    }

    /// Rend une frame. Retourne l'issue : présentée, sautée (transitoire) ou
    /// device perdu (l'appelant doit replier sur le rendu CPU).
    #[allow(clippy::too_many_arguments)] // pipeline de rendu : tout est requis
    pub fn render(
        &mut self,
        state: &NodeState,
        video: Option<&VideoFrame>,
        lut: Option<(&str, &toolbox_engine::Lut3d)>,
        time: f32,
        width: u32,
        height: u32,
        blackout: f32,
    ) -> ResultatRendu {
        let (width, height) = (width.max(1), height.max(1));
        if self.config.width != width || self.config.height != height {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }

        if let Some(frame) = video {
            self.upload_video(frame);
        }
        self.sync_lut(lut);
        let u = self.uniforms_for(state, video.is_some(), time, width, height, blackout);
        self.queue
            .write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));

        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(frame) | Cst::Suboptimal(frame) => frame,
            Cst::Outdated | Cst::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Cst::Success(frame) | Cst::Suboptimal(frame) => frame,
                    // Surface toujours perdue APRÈS reconfiguration : le
                    // device lui-même est réputé perdu (pilote réinitialisé,
                    // écran débranché en plein écran). On le signale pour un
                    // repli CPU à chaud, au lieu de rester en sortie noire.
                    other => {
                        warn!(?other, "device GPU perdu (surface non récupérable)");
                        return ResultatRendu::DevicePerdu;
                    }
                }
            }
            // Fenêtre réduite ou frame en retard : on saute, sans bruit.
            Cst::Timeout | Cst::Occluded => return ResultatRendu::Sautee,
            other => {
                warn!(?other, "device GPU perdu (frame indisponible)");
                return ResultatRendu::DevicePerdu;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sortie"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("warp"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        ResultatRendu::Presentee
    }

    /// Téléverse la LUT quand elle change (nom comparé, pas le contenu).
    fn sync_lut(&mut self, lut: Option<(&str, &toolbox_engine::Lut3d)>) {
        match lut {
            Some((nom, lut)) => {
                if self.lut_nom.as_deref() != Some(nom) {
                    self.lut_buffer = create_lut_buffer(&self.device, &lut.texels_rgba_f32());
                    self.lut_nom = Some(nom.to_string());
                    #[allow(clippy::cast_possible_truncation)] // ≤ 129
                    {
                        self.lut_taille = lut.taille as u32;
                    }
                    self.rebuild_bind_group();
                }
            }
            None => {
                if self.lut_nom.is_some() {
                    self.lut_buffer = create_lut_buffer(&self.device, &[0.0, 0.0, 0.0, 1.0]);
                    self.lut_nom = None;
                    self.lut_taille = 0;
                    self.rebuild_bind_group();
                }
            }
        }
    }

    fn rebuild_bind_group(&mut self) {
        self.bind_group = create_bind_group(
            &self.device,
            &self.bind_layout,
            &self.uniforms,
            &self.video_texture,
            &self.sampler,
            &self.lut_buffer,
        );
    }

    /// Téléverse la frame vidéo (texture recréée si la taille change).
    fn upload_video(&mut self, frame: &VideoFrame) {
        if self.video_size != (frame.width, frame.height) {
            self.video_texture = create_video_texture(&self.device, frame.width, frame.height);
            self.video_size = (frame.width, frame.height);
            self.rebuild_bind_group();
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.video_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.width * 4),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Mêmes règles de sélection de source et mêmes matrices que raster.rs.
    fn uniforms_for(
        &self,
        state: &NodeState,
        has_video: bool,
        time: f32,
        width: u32,
        height: u32,
        blackout: f32,
    ) -> Uniforms {
        let mode = match (state.test_pattern, has_video) {
            (Some(TestPattern::Grid), _) => 1.0,
            (Some(TestPattern::Checker), _) => 2.0,
            (Some(TestPattern::Corners), _) => 3.0,
            (None, true) if state.player.transport != Transport::Stopped => 4.0,
            _ => 0.0,
        };
        let (mode, warp_inv, uv, color) = match RenderParams::from_state(state) {
            Ok(params) => (
                mode,
                columns(params.warp_inv_gl),
                columns(params.uv_transform_gl),
                params.color,
            ),
            Err(err) => {
                // Mapping dégénéré : noir, comme le peintre CPU.
                warn!(%err, "paramètres de rendu indisponibles — sortie noire");
                (
                    0.0,
                    IDENTITY_COLUMNS,
                    IDENTITY_COLUMNS,
                    toolbox_engine::ColorUniforms {
                        brightness: 1.0,
                        contrast: 1.0,
                        gamma: 1.0,
                        saturation: 1.0,
                        hue_degrees: 0.0,
                        gain: [1.0, 1.0, 1.0],
                    },
                )
            }
        };
        Uniforms {
            warp_inv,
            uv,
            color_a: [
                color.brightness,
                color.contrast,
                color.gamma,
                color.saturation,
            ],
            color_b: [
                color.hue_degrees.to_radians(),
                color.gain[0],
                color.gain[1],
                color.gain[2],
            ],
            misc: [width as f32, height as f32, mode, 0.0],
            fx_a: [
                state.effects.pixelate,
                state.effects.posterize,
                state.effects.noise,
                state.effects.sharpen,
            ],
            fx_b: {
                // Le mesh suit l'interrupteur du mapping, comme raster.rs.
                let mesh = if state.mapping.enabled {
                    state.mapping.mesh.as_ref()
                } else {
                    None
                };
                [
                    state.effects.mirror,
                    time,
                    mesh.map_or(0.0, |m| f32::from(m.colonnes)),
                    mesh.map_or(0.0, |m| f32::from(m.lignes)),
                ]
            },
            blending_a: [
                state.blending.gauche,
                state.blending.droite,
                state.blending.haut,
                state.blending.bas,
            ],
            #[allow(clippy::cast_precision_loss)] // ≤ 8 et ≤ 129
            blending_b: [
                state.blending.gamma,
                state.masques.len().min(8) as f32,
                blackout.clamp(0.0, 1.0),
                self.lut_taille as f32,
            ],
            masques: masques_vec4(&state.masques),
            mesh: mesh_vec4(if state.mapping.enabled {
                state.mapping.mesh.as_ref()
            } else {
                None
            }),
        }
    }
}

/// Emballe les déplacements du mesh (81 max) : deux points par vec4.
fn mesh_vec4(mesh: Option<&toolbox_core::MeshState>) -> [[f32; 4]; 41] {
    let mut out = [[0.0f32; 4]; 41];
    if let Some(mesh) = mesh {
        for (k, o) in mesh.offsets.iter().take(81).enumerate() {
            let moitie = (k % 2) * 2;
            out[k / 2][moitie] = o.x;
            out[k / 2][moitie + 1] = o.y;
        }
    }
    out
}

/// Emballe les masques (8 max) en paires de vec4 : (x0,y0,x1,y1), (x2,y2,x3,y3).
fn masques_vec4(masques: &[toolbox_core::Masque]) -> [[f32; 4]; 16] {
    let mut out = [[0.0f32; 4]; 16];
    for (i, masque) in masques.iter().take(8).enumerate() {
        let c = &masque.corners;
        out[i * 2] = [c[0].x, c[0].y, c[1].x, c[1].y];
        out[i * 2 + 1] = [c[2].x, c[2].y, c[3].x, c[3].y];
    }
    out
}

fn create_video_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("video"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    texture: &wgpu::Texture,
    sampler: &wgpu::Sampler,
    lut: &wgpu::Buffer,
) -> wgpu::BindGroup {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("sortie"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: lut.as_entire_binding(),
            },
        ],
    })
}

/// Buffer storage des entrées de LUT (vec4 par entrée, alpha ignoré).
fn create_lut_buffer(device: &wgpu::Device, texels: &[f32]) -> wgpu::Buffer {
    use wgpu::util::DeviceExt;
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("lut"),
        contents: bytemuck::cast_slice(texels),
        usage: wgpu::BufferUsages::STORAGE,
    })
}

#[cfg(test)]
mod tests {
    /// Le shader est validé en CI sans GPU : syntaxe ET typage (naga est le
    /// compilateur qu'utilise wgpu à l'exécution).
    #[test]
    fn wgsl_shader_is_valid() {
        let module = naga::front::wgsl::parse_str(include_str!("warp.wgsl")).expect("syntaxe WGSL");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("validation WGSL");
    }
}
