//! `wgpu` ベースの多層 GPU 合成エンジンおよびレンダリングシーン構築モジュール。
//!
//! プレビューと動画書き出しの両方で同一の WGSL シェーダ・座標変換・色調整を使用し、
//! 編集時の見た目と最終出力画質の一致を保証する。

pub mod texture_pool;

use std::sync::Arc;
use std::sync::mpsc;

use bytemuck::{Pod, Zeroable};
use thiserror::Error;
use wgpu::util::DeviceExt as _;

use crate::core::animation::EvaluationContext;
use crate::core::color::RgbaColor;
use crate::core::ids::AssetId;
use crate::core::time::FrameIndex;
use crate::media::decoder::VideoFrameBuffer;
use crate::media::visual_properties::{BlendMode, EvaluatedVisual};
use crate::timeline::document::{AssetStatus, Clip, ClipKind, ProjectDocument};
use texture_pool::{CompositorTargets, GpuTextureKey, GpuTexturePool};

const SHADER_SOURCE: &str = include_str!("shaders/layer_composite.wgsl");

/// GPU 合成処理で発生し得るエラー。
#[derive(Debug, Error)]
pub enum CompositorError {
    /// 利用可能な GPU またはソフトウェアフォールバックアダプタが見つからない。
    #[error("no suitable wgpu graphics adapter found")]
    NoAdapter,
    /// デバイス初期化に失敗した。
    #[error("failed to create wgpu device: {0}")]
    DeviceCreation(String),
    /// 読み出し用ステージングバッファのマップに失敗した。
    #[error("failed to map GPU readback buffer: {0}")]
    BufferReadback(String),
    /// GPU 待機ポーリングに失敗した。
    #[error("wgpu device poll error: {0}")]
    PollError(String),
}

/// GPU 頂点バッファへ渡す変形済み四角形の 1 頂点。
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, PartialEq)]
pub struct LayerVertex {
    /// クリップ空間座標 (`-1.0..=1.0`, `+X` 右, `+Y` 上)。
    pub ndc_pos: [f32; 2],
    /// 素材テクスチャ座標 (`0.0..=1.0`, `+Y` 下)。
    pub uv: [f32; 2],
}

impl LayerVertex {
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
    };
}

/// WGSL `LayerUniforms` と厳密一致するユニフォーム構造体（16 バイト境界）。
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct LayerUniforms {
    opacity: f32,
    brightness: f32,
    contrast: f32,
    saturation: f32,
    hue_rad: f32,
    gamma: f32,
    blend_mode: u32,
    _pad: u32,
}

/// 純粋 Rust で評価された 1 レイヤー分の描画指令。
#[derive(Debug, Clone)]
pub struct RenderLayerItem {
    /// テクスチャキャッシュ識別子。
    pub cache_key: GpuTextureKey,
    /// 元画像 / 動画 / ラスタライズ済み字幕の BGRA8 バッファ。
    pub source_buffer: Arc<VideoFrameBuffer>,
    /// 評価済み視覚パラメータおよび色調整値。
    pub visual: EvaluatedVisual,
    /// プロジェクト解像度上で計算された 4 隅の NDC 頂点 `[左上, 右上, 右下, 左下]`。
    pub quad_vertices: [LayerVertex; 4],
    /// 元素材が欠損 (`AssetStatus::Missing`) 状態のプレースホルダ描画かどうか。
    pub is_missing_placeholder: bool,
}

/// ある 1 フレームにおけるレンダリング対象シーン全体。
#[derive(Debug, Clone)]
pub struct RenderScene {
    /// プロジェクト基準解像度幅。
    pub project_width: u32,
    /// プロジェクト基準解像度高さ。
    pub project_height: u32,
    /// 背景塗りつぶし色。
    pub background_color: RgbaColor,
    /// 下層から上層へ並んだ可視レイヤー項目一覧。
    pub layers: Vec<RenderLayerItem>,
    /// 現在フレームで欠損素材が参照されていた場合 `true`。
    pub has_missing_media: bool,
}

/// `Matrix2D` と元フレーム寸法・プロジェクト解像度から 4 隅の NDC 頂点を計算する。
///
/// 頂点順: `[0: 左上, 1: 右上, 2: 右下, 3: 左下]`。
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn compute_layer_quad_ndc(
    src_width: u32,
    src_height: u32,
    project_width: u32,
    project_height: u32,
    visual: &EvaluatedVisual,
) -> [LayerVertex; 4] {
    let half_src_w = f64::from(src_width.max(1)) * 0.5;
    let half_src_h = f64::from(src_height.max(1)) * 0.5;
    let inv_half_proj_w = 1.0 / (f64::from(project_width.max(1)) * 0.5);
    let inv_half_proj_h = 1.0 / (f64::from(project_height.max(1)) * 0.5);

    let corners = [
        (-half_src_w, -half_src_h, [0.0_f32, 0.0_f32]),
        (half_src_w, -half_src_h, [1.0_f32, 0.0_f32]),
        (half_src_w, half_src_h, [1.0_f32, 1.0_f32]),
        (-half_src_w, half_src_h, [0.0_f32, 1.0_f32]),
    ];

    corners.map(|(lx, ly, uv)| {
        let (px, py) = visual.matrix.transform_point(lx, ly);
        LayerVertex {
            ndc_pos: [
                (px * inv_half_proj_w) as f32,
                -(py * inv_half_proj_h) as f32,
            ],
            uv,
        }
    })
}

/// タイムライン上の指定フレームから GPU 非依存の [`RenderScene`] を構築する。
pub fn build_render_scene(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    lookup_asset_frame: &mut dyn FnMut(&AssetId, u64) -> Option<Arc<VideoFrameBuffer>>,
) -> RenderScene {
    let project_width = doc.resolution.width.get();
    let project_height = doc.resolution.height.get();
    let mut layers = Vec::new();
    let mut has_missing_media = false;

    for layer in &doc.layers {
        if !layer.visible {
            continue;
        }
        for clip in &layer.clips {
            if !clip_contains_frame(clip, playhead) {
                continue;
            }
            if let Some(item) = build_clip_layer_item(
                doc,
                clip,
                playhead,
                lookup_asset_frame,
                &mut has_missing_media,
            ) {
                layers.push(item);
            }
        }
    }

    RenderScene {
        project_width,
        project_height,
        background_color: doc.background_color,
        layers,
        has_missing_media,
    }
}

const fn clip_contains_frame(clip: &Clip, frame: FrameIndex) -> bool {
    let start = clip.timeline_start.get();
    let end = start.saturating_add(clip.duration.get());
    frame.get() >= start && frame.get() < end
}

fn build_clip_layer_item(
    doc: &ProjectDocument,
    clip: &Clip,
    playhead: FrameIndex,
    lookup_asset_frame: &mut dyn FnMut(&AssetId, u64) -> Option<Arc<VideoFrameBuffer>>,
    has_missing_media: &mut bool,
) -> Option<RenderLayerItem> {
    let canvas_size = (doc.resolution.width.get(), doc.resolution.height.get());
    let local_frame = playhead.get().saturating_sub(clip.timeline_start.get());
    let ctx = EvaluationContext::new(
        local_frame,
        clip.effect_offset_frames,
        clip.effect_duration_frames.unwrap_or(clip.duration),
        doc.fps,
    );

    match &clip.kind {
        ClipKind::Video {
            asset_id,
            source_in,
            playback_rate,
            visual,
            ..
        } => {
            if is_asset_missing(doc, asset_id) {
                *has_missing_media = true;
                return Some(missing_media_layer_item(
                    asset_id,
                    canvas_size.0,
                    canvas_size.1,
                ));
            }
            #[allow(clippy::cast_precision_loss)]
            let local_sec = (local_frame as f64) / doc.fps.as_f64();
            let media_sec = source_in.as_seconds_f64() + local_sec * playback_rate.as_f64();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let src_frame_idx = (media_sec * doc.fps.as_f64()).max(0.0).round() as u64;
            let evaluated = evaluate_visual_or_default(visual, ctx);
            let buffer = lookup_asset_frame(asset_id, src_frame_idx)?;
            Some(make_layer_item(
                GpuTextureKey {
                    asset_id: asset_id.clone(),
                    frame_index: src_frame_idx,
                    width: buffer.width,
                    height: buffer.height,
                },
                buffer,
                evaluated,
                canvas_size,
                false,
            ))
        }
        ClipKind::Image { asset_id, visual } => {
            if is_asset_missing(doc, asset_id) {
                *has_missing_media = true;
                return Some(missing_media_layer_item(
                    asset_id,
                    canvas_size.0,
                    canvas_size.1,
                ));
            }
            let evaluated = evaluate_visual_or_default(visual, ctx);
            let buffer = lookup_asset_frame(asset_id, 0)?;
            Some(make_layer_item(
                GpuTextureKey {
                    asset_id: asset_id.clone(),
                    frame_index: 0,
                    width: buffer.width,
                    height: buffer.height,
                },
                buffer,
                evaluated,
                canvas_size,
                false,
            ))
        }
        ClipKind::Subtitle(sub) => {
            let evaluated = evaluate_visual_or_default(&sub.visual, ctx);
            #[allow(clippy::cast_possible_truncation)]
            let font_size = sub.font_size.evaluate(ctx).unwrap_or(42.0).max(6.0) as f32;
            let wrap_width = canvas_size.0.clamp(128, 3840);
            let content_hash =
                super::subtitle_renderer::subtitle_content_hash(sub, font_size, wrap_width);
            let buffer =
                super::subtitle_renderer::render_subtitle_cached(sub, font_size, wrap_width);
            let key_id = AssetId::new(format!("subtitle_{}", clip.id)).ok()?;
            Some(make_layer_item(
                GpuTextureKey {
                    asset_id: key_id,
                    frame_index: content_hash,
                    width: buffer.width,
                    height: buffer.height,
                },
                buffer,
                evaluated,
                canvas_size,
                false,
            ))
        }
        ClipKind::Audio { .. } => None,
    }
}

fn evaluate_visual_or_default(
    visual: &crate::media::visual_properties::VisualProperties,
    ctx: EvaluationContext,
) -> EvaluatedVisual {
    visual.evaluate(ctx).unwrap_or_else(|_| {
        EvaluatedVisual::from_components(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0, visual.blend_mode)
    })
}

fn is_asset_missing(doc: &ProjectDocument, asset_id: &AssetId) -> bool {
    !matches!(
        doc.find_asset(asset_id).map(|asset| &asset.status),
        Some(AssetStatus::Available)
    )
}

fn make_layer_item(
    cache_key: GpuTextureKey,
    source_buffer: Arc<VideoFrameBuffer>,
    visual: EvaluatedVisual,
    canvas_size: (u32, u32),
    is_missing_placeholder: bool,
) -> RenderLayerItem {
    let quad_vertices = compute_layer_quad_ndc(
        source_buffer.width,
        source_buffer.height,
        canvas_size.0,
        canvas_size.1,
        &visual,
    );
    RenderLayerItem {
        cache_key,
        source_buffer,
        visual,
        quad_vertices,
        is_missing_placeholder,
    }
}

fn missing_media_layer_item(
    asset_id: &AssetId,
    project_width: u32,
    project_height: u32,
) -> RenderLayerItem {
    let w = (project_width / 4).max(32);
    let h = (project_height / 6).max(24);
    let buf = Arc::new(VideoFrameBuffer::solid_bgra8(
        w,
        h,
        RgbaColor::new(180, 28, 32, 220),
    ));
    let visual =
        EvaluatedVisual::from_components(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0, BlendMode::Normal);
    make_layer_item(
        GpuTextureKey {
            asset_id: asset_id.clone(),
            frame_index: u64::MAX,
            width: buf.width,
            height: buf.height,
        },
        buf,
        visual,
        (project_width, project_height),
        true,
    )
}

/// `wgpu` を用いて [`RenderScene`] をオフスクリーン合成・ダウンスケールするコンポジタ本体。
pub struct WgpuCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layer_pipeline: wgpu::RenderPipeline,
    blit_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    blit_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    index_buffer: wgpu::Buffer,
    dummy_bg_view: wgpu::TextureView,
    _dummy_bg_tex: wgpu::Texture,
    targets: Option<CompositorTargets>,
    texture_pool: GpuTexturePool,
}

impl WgpuCompositor {
    /// `wgpu` インスタンス・アダプタ・パイプラインを初期化する。
    ///
    /// ハードウェアアダプタが存在しない環境ではソフトウェアフォールバックアダプタも試行する。
    ///
    /// # Errors
    ///
    /// アダプタが見つからない、またはデバイス作成に失敗した場合 [`CompositorError`] を返す。
    pub fn try_new() -> Result<Self, CompositorError> {
        let instance = wgpu::Instance::default();
        let adapter = request_best_adapter(&instance)?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("sea_wgpu_compositor_device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|err| CompositorError::DeviceCreation(err.to_string()))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sea_layer_composite_shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()),
        });

        let bind_group_layout = create_layer_bind_group_layout(&device);
        let blit_layout = create_blit_bind_group_layout(&device);
        let layer_pipeline = create_layer_pipeline(&device, &shader, &bind_group_layout);
        let blit_pipeline = create_blit_pipeline(&device, &shader, &blit_layout);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sea_bilinear_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("sea_quad_indices"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        let (dummy_bg_tex, dummy_bg_view) =
            GpuTexturePool::create_solid_texture_view(&device, &queue, 1, 1, [0, 0, 0, 255]);

        Ok(Self {
            device,
            queue,
            layer_pipeline,
            blit_pipeline,
            bind_group_layout,
            blit_layout,
            sampler,
            index_buffer,
            dummy_bg_view,
            _dummy_bg_tex: dummy_bg_tex,
            targets: None,
            texture_pool: GpuTexturePool::new(),
        })
    }

    /// 内部テクスチャプールへの参照を返す。
    #[must_use]
    pub const fn texture_pool(&self) -> &GpuTexturePool {
        &self.texture_pool
    }

    /// [`RenderScene`] をフル解像度で GPU 合成し、指定出力サイズへ縮小して BGRA8 バッファを読み出す。
    ///
    /// # Errors
    ///
    /// 読み出しバッファのマップまたは GPU 同期に失敗した場合 [`CompositorError`] を返す。
    pub fn render_scene_to_bgra8(
        &mut self,
        scene: &RenderScene,
        out_width: u32,
        out_height: u32,
    ) -> Result<VideoFrameBuffer, CompositorError> {
        let proj_w = scene.project_width.max(1);
        let proj_h = scene.project_height.max(1);
        let ow = out_width.max(1);
        let oh = out_height.max(1);

        let targets = self
            .targets
            .get_or_insert_with(|| CompositorTargets::new(&self.device, proj_w, proj_h, ow, oh));
        targets.ensure_sizes(&self.device, proj_w, proj_h, ow, oh);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sea_composite_encoder"),
            });

        // パス 0: ping をプロジェクト背景色でクリア
        clear_target(&mut encoder, &targets.ping.1, scene.background_color);

        // 各レイヤーを ping <-> pong 交互に出力し、前段結果を bg_tex としてブレンド適用
        let mut front_is_ping = true;
        for layer in &scene.layers {
            self.encode_layer_draw(&mut encoder, layer, front_is_ping);
            front_is_ping = !front_is_ping;
        }

        let Some(targets_ref) = self.targets.as_ref() else {
            return Err(CompositorError::DeviceCreation(
                "missing compositor render targets".to_owned(),
            ));
        };
        let (final_tex, final_view) = if front_is_ping {
            (&targets_ref.ping.0, &targets_ref.ping.1)
        } else {
            (&targets_ref.pong.0, &targets_ref.pong.1)
        };

        // 出力サイズがプロジェクト解像度と一致する場合はコピーのみ、異なる場合はバイリニア縮小描画
        let readback_source_tex = if ow == proj_w && oh == proj_h {
            final_tex
        } else {
            self.encode_blit_downscale(&mut encoder, final_view, &targets_ref.output.1);
            &targets_ref.output.0
        };

        self.readback_texture_bgra8(encoder, readback_source_tex, ow, oh)
    }

    fn encode_blit_downscale(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        src_view: &wgpu::TextureView,
        dst_view: &wgpu::TextureView,
    ) {
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sea_blit_bg"),
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("sea_blit_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.blit_pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    fn encode_layer_draw(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        layer: &RenderLayerItem,
        front_is_ping: bool,
    ) {
        let Some(targets) = self.targets.as_ref() else {
            return;
        };
        let ((src_bg_tex, src_bg_view), (dst_tex, dst_view)) = if front_is_ping {
            (
                (&targets.ping.0, &targets.ping.1),
                (&targets.pong.0, &targets.pong.1),
            )
        } else {
            (
                (&targets.pong.0, &targets.pong.1),
                (&targets.ping.0, &targets.ping.1),
            )
        };
        let copy_extent = wgpu::Extent3d {
            width: targets.project_width,
            height: targets.project_height,
            depth_or_array_layers: 1,
        };
        // 前段の合成済みキャンバス全体を描画先へ GPU 内コピーしておき、四角形外の領域を保持する
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: src_bg_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: dst_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            copy_extent,
        );

        let layer_view = self.texture_pool.get_or_upload_bgra(
            &self.device,
            &self.queue,
            &layer.cache_key,
            &layer.source_buffer,
        );

        let vertex_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("sea_layer_quad_vb"),
                contents: bytemuck::cast_slice(&layer.quad_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let uniforms = build_uniforms(&layer.visual);
        let uniform_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("sea_layer_uniforms"),
                contents: bytemuck::bytes_of(&uniforms),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sea_layer_bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(layer_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(src_bg_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: uniform_buf.as_entire_binding(),
                },
            ],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("sea_layer_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.layer_pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed(0..6, 0, 0..1);
    }

    fn readback_texture_bgra8(
        &self,
        mut encoder: wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
    ) -> Result<VideoFrameBuffer, CompositorError> {
        let _ = &self.dummy_bg_view;
        let unpadded_bpr = width.saturating_mul(4);
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bpr = unpadded_bpr.div_ceil(align).saturating_mul(align);
        let buf_size = u64::from(padded_bpr).saturating_mul(u64::from(height));

        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sea_readback_staging"),
            size: buf_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = staging.slice(..);
        let (tx, rx) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| CompositorError::PollError(err.to_string()))?;

        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(CompositorError::BufferReadback(err.to_string())),
            Err(err) => return Err(CompositorError::BufferReadback(err.to_string())),
        }

        let mapped = slice
            .get_mapped_range()
            .map_err(|err| CompositorError::BufferReadback(err.to_string()))?;

        let tight_row = usize::try_from(unpadded_bpr).unwrap_or(0);
        let padded_row = usize::try_from(padded_bpr).unwrap_or(0);
        let h_usize = usize::try_from(height).unwrap_or(0);
        let mut out = Vec::with_capacity(tight_row.saturating_mul(h_usize));
        for row_idx in 0..h_usize {
            let start = row_idx.saturating_mul(padded_row);
            let end = start.saturating_add(tight_row);
            out.extend_from_slice(&mapped[start..end]);
        }
        drop(mapped);
        staging.unmap();

        Ok(VideoFrameBuffer {
            width,
            height,
            format: crate::media::decoder::FramePixelFormat::Bgra8,
            data: out,
        })
    }
}

fn request_best_adapter(instance: &wgpu::Instance) -> Result<wgpu::Adapter, CompositorError> {
    let hw_opt = wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
        ..Default::default()
    };
    if let Ok(adapter) = pollster::block_on(instance.request_adapter(&hw_opt)) {
        return Ok(adapter);
    }
    let sw_opt = wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: true,
        compatible_surface: None,
        ..Default::default()
    };
    pollster::block_on(instance.request_adapter(&sw_opt)).map_err(|_| CompositorError::NoAdapter)
}

#[allow(clippy::cast_possible_truncation)]
fn build_uniforms(visual: &EvaluatedVisual) -> LayerUniforms {
    LayerUniforms {
        opacity: visual.opacity as f32,
        brightness: visual.brightness as f32,
        contrast: visual.contrast as f32,
        saturation: visual.saturation as f32,
        hue_rad: visual.hue_deg.to_radians() as f32,
        gamma: visual.gamma.max(0.01) as f32,
        blend_mode: match visual.blend_mode {
            BlendMode::Normal => 0,
            BlendMode::Add => 1,
            BlendMode::Multiply => 2,
            BlendMode::Screen => 3,
            BlendMode::Overlay => 4,
        },
        _pad: 0,
    }
}

fn clear_target(encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, bg: RgbaColor) {
    let clear_color = wgpu::Color {
        r: f64::from(bg.red) / 255.0,
        g: f64::from(bg.green) / 255.0,
        b: f64::from(bg.blue) / 255.0,
        a: f64::from(bg.alpha) / 255.0,
    };
    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("sea_clear_pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear_color),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
}

fn create_layer_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("sea_layer_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
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
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    })
}

fn create_blit_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("sea_blit_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
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
        ],
    })
}

fn create_layer_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("sea_layer_pipeline_layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("sea_layer_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_layer"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(LayerVertex::LAYOUT)],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_layer"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Bgra8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_blit_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("sea_blit_pipeline_layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("sea_blit_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_blit"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_blit"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Bgra8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::{ClipId, LayerId};
    use crate::core::time::FrameCount;
    use crate::timeline::document::{Asset, AssetKind, AssetPath, Layer};

    #[test]
    fn anchor_clockwise_rotation_transforms_quad_vertices_accurately() {
        // 100x50 矩形に対し、アンカー (10, 0)、時計回り90度、平行移動 (20, 40) を 200x200 キャンバス上で確認
        let visual = EvaluatedVisual::from_components(
            20.0,
            40.0,
            1.0,
            1.0,
            90.0,
            10.0,
            0.0,
            1.0,
            BlendMode::Normal,
        );
        let quad = compute_layer_quad_ndc(100, 50, 200, 200, &visual);
        // ローカル右上 (+50, -25) -> アンカー差 (+40, -25) -> 時計回り90度 (+25, +40) -> 平行移動 (+45, +80)
        // NDC(200x200): ndc_x = 45/100 = 0.45, ndc_y = -80/100 = -0.80
        assert!((quad[1].ndc_pos[0] - 0.45).abs() < 1e-4);
        assert!((quad[1].ndc_pos[1] - (-0.80)).abs() < 1e-4);
    }

    #[test]
    fn build_render_scene_preserves_layer_order_and_flags_missing_assets()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("SceneOrder");
        let ok_id = AssetId::new("ok_img")?;
        let missing_id = AssetId::new("missing_img")?;
        doc.assets.push(Asset {
            id: ok_id.clone(),
            name: "ok.png".to_owned(),
            path: AssetPath::from_absolute("ok.png"),
            kind: AssetKind::Image {
                width: 64,
                height: 64,
            },
            status: AssetStatus::Available,
        });
        doc.assets.push(Asset {
            id: missing_id.clone(),
            name: "gone.png".to_owned(),
            path: AssetPath::from_absolute("gone.png"),
            kind: AssetKind::Image {
                width: 64,
                height: 64,
            },
            status: AssetStatus::Missing {
                expected_path: "gone.png".to_owned(),
            },
        });

        let mut bottom = Layer::new(LayerId::new("bottom")?, "Bottom");
        bottom.clips.push(Clip {
            id: ClipId::new("c1")?,
            name: "BottomClip".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(60)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            kind: ClipKind::Image {
                asset_id: ok_id.clone(),
                visual: crate::media::visual_properties::VisualProperties::default(),
            },
        });
        let mut top = Layer::new(LayerId::new("top")?, "Top");
        top.clips.push(Clip {
            id: ClipId::new("c2")?,
            name: "MissingClip".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(60)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            kind: ClipKind::Image {
                asset_id: missing_id,
                visual: crate::media::visual_properties::VisualProperties::default(),
            },
        });
        doc.layers.push(bottom);
        doc.layers.push(top);

        let blue = Arc::new(VideoFrameBuffer::solid_bgra8(
            64,
            64,
            RgbaColor::new(0, 0, 255, 255),
        ));
        let mut lookup = |id: &AssetId, _: u64| -> Option<Arc<VideoFrameBuffer>> {
            (id == &ok_id).then(|| Arc::clone(&blue))
        };

        let scene = build_render_scene(&doc, FrameIndex::ZERO, &mut lookup);
        assert!(scene.has_missing_media);
        assert_eq!(scene.layers.len(), 2);
        assert_eq!(scene.layers[0].cache_key.asset_id, ok_id);
        assert!(!scene.layers[0].is_missing_placeholder);
        assert!(scene.layers[1].is_missing_placeholder);
        Ok(())
    }
}
