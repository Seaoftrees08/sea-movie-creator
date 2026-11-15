//! プレビューフレームの GPUI 転送ブリッジ (`PreviewSurfaceBridge`) と Phase A 最小ソフト合成器。

use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Frame as ImageFrame, RgbaImage};

use crate::core::animation::EvaluationContext;
use crate::core::ids::AssetId;
use crate::core::time::FrameIndex;
use crate::media::compositor::{WgpuCompositor, build_render_scene};
use crate::media::decoder::{FramePixelFormat, VideoFrameBuffer};
use crate::media::visual_properties::EvaluatedVisual;
use crate::timeline::document::{AssetStatus, Clip, ClipKind, ProjectDocument};

/// GPU や OS 固有サーフェスへの差し替え余地を保ったプレビュー転送抽象トレイト。
pub trait PreviewSurfaceBridge {
    /// BGRA8（または RGBA8）フレームバッファを受理して表示用フロントバッファを更新する。
    fn submit_bgra_frame(&mut self, frame: &VideoFrameBuffer);

    /// 現在表示中のフロントバッファ世代番号を返す（未送信時は `0`）。
    fn current_frame_id(&self) -> u64;

    /// 現在保持しているフレームのピクセル寸法を返す。
    fn current_dimensions(&self) -> Option<(u32, u32)>;
}

/// Phase A 既定実装: BGRA `Arc<RenderImage>` ダブルバッファ＋旧テクスチャ破棄キュー。
///
/// GPUI のスプライトアトラスに古いフレームが滞留しないよう、差し替え時に旧 `Arc<RenderImage>` を
/// 保留キューへ積み、描画パス側で `window.drop_image(...)` により解放する。
#[derive(Default)]
pub struct RenderImageSurfaceBridge {
    front_image: Option<Arc<RenderImage>>,
    frame_id: u64,
    dimensions: Option<(u32, u32)>,
    pending_drops: Vec<Arc<RenderImage>>,
}

impl RenderImageSurfaceBridge {
    /// 空のブリッジを作成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 現在のフロントバッファ `Arc<RenderImage>` を取得する。
    #[must_use]
    pub fn front_image(&self) -> Option<Arc<RenderImage>> {
        self.front_image.clone()
    }

    /// 次回 `Window::drop_image` で解放すべき旧フレーム一覧を取り出す。
    pub fn drain_pending_drops(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.pending_drops)
    }
}

impl PreviewSurfaceBridge for RenderImageSurfaceBridge {
    fn submit_bgra_frame(&mut self, frame: &VideoFrameBuffer) {
        if frame.width == 0 || frame.height == 0 {
            return;
        }
        // GPUI の RenderImage は内部で BGRA バイト列として解釈する。
        let bgra = match frame.format {
            FramePixelFormat::Bgra8 => frame.data.clone(),
            FramePixelFormat::Rgba8 => frame.to_bgra8(),
        };
        let Some(rgba_container) = RgbaImage::from_raw(frame.width, frame.height, bgra) else {
            return;
        };
        let image_frame = ImageFrame::new(rgba_container);
        let next = Arc::new(RenderImage::new([image_frame]));
        if let Some(previous) = self.front_image.replace(next) {
            self.pending_drops.push(previous);
        }
        self.frame_id = self.frame_id.saturating_add(1);
        self.dimensions = Some((frame.width, frame.height));
    }

    fn current_frame_id(&self) -> u64 {
        self.frame_id
    }

    fn current_dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions
    }
}

/// 1 フレーム分のタイムラインソフトウェア合成結果。
#[derive(Debug, Clone)]
pub struct CompositePreviewOutput {
    /// プレビュー解像度の BGRA8 バッファ。
    pub buffer: VideoFrameBuffer,
    /// 現在フレームで参照中の素材に `AssetStatus::Missing` が含まれる場合 `true`。
    pub has_missing_media: bool,
}

/// 指定再生位置における全可視レイヤーを最小ソフト合成し、BGRA8 バッファを返す。
#[must_use]
pub fn composite_timeline_preview(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    preview_width: u32,
    preview_height: u32,
    lookup_asset_frame: &mut dyn FnMut(&AssetId, u64) -> Option<Arc<VideoFrameBuffer>>,
) -> CompositePreviewOutput {
    let w = preview_width.max(1);
    let h = preview_height.max(1);
    let mut canvas = VideoFrameBuffer::solid_bgra8(w, h, doc.background_color);
    let mut has_missing_media = false;

    for layer in &doc.layers {
        if !layer.visible {
            continue;
        }
        for clip in &layer.clips {
            if !clip_contains_frame(clip, playhead) {
                continue;
            }
            composite_single_clip(
                doc,
                clip,
                playhead,
                &mut canvas,
                lookup_asset_frame,
                &mut has_missing_media,
            );
        }
    }

    CompositePreviewOutput {
        buffer: canvas,
        has_missing_media,
    }
}

/// 利用可能な場合は `wgpu` GPU 合成（フル解像度評価→縮小→ステージング読出し）を使用し、
/// GPU アダプタ不在環境では CPU ソフト合成へフォールバックする統合エントリ。
pub fn composite_with_gpu_or_fallback(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    preview_width: u32,
    preview_height: u32,
    gpu_compositor: Option<&mut WgpuCompositor>,
    lookup_asset_frame: &mut dyn FnMut(&AssetId, u64) -> Option<Arc<VideoFrameBuffer>>,
) -> CompositePreviewOutput {
    if let Some(gpu) = gpu_compositor {
        let scene = build_render_scene(doc, playhead, lookup_asset_frame);
        if let Ok(buffer) = gpu.render_scene_to_bgra8(&scene, preview_width, preview_height) {
            return CompositePreviewOutput {
                buffer,
                has_missing_media: scene.has_missing_media,
            };
        }
    }
    composite_timeline_preview(
        doc,
        playhead,
        preview_width,
        preview_height,
        lookup_asset_frame,
    )
}

const fn clip_contains_frame(clip: &Clip, frame: FrameIndex) -> bool {
    let start = clip.timeline_start.get();
    let end = start.saturating_add(clip.duration.get());
    frame.get() >= start && frame.get() < end
}

fn composite_single_clip(
    doc: &ProjectDocument,
    clip: &Clip,
    playhead: FrameIndex,
    canvas: &mut VideoFrameBuffer,
    lookup_asset_frame: &mut dyn FnMut(&AssetId, u64) -> Option<Arc<VideoFrameBuffer>>,
    has_missing_media: &mut bool,
) {
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
                return;
            }
            #[allow(clippy::cast_precision_loss)]
            let local_sec = (local_frame as f64) / doc.fps.as_f64();
            let media_sec = source_in.as_seconds_f64() + local_sec * playback_rate.as_f64();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let src_frame_idx = (media_sec * doc.fps.as_f64()).max(0.0).round() as u64;
            let evaluated = visual.evaluate(ctx).unwrap_or_else(|_| {
                EvaluatedVisual::from_components(
                    0.0,
                    0.0,
                    1.0,
                    1.0,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                    visual.blend_mode,
                )
            });
            if let Some(src) = lookup_asset_frame(asset_id, src_frame_idx) {
                blit_scaled_layer(canvas, &src, evaluated, doc);
            }
        }
        ClipKind::Image { asset_id, visual } => {
            if is_asset_missing(doc, asset_id) {
                *has_missing_media = true;
                return;
            }
            let evaluated = visual.evaluate(ctx).unwrap_or_else(|_| {
                EvaluatedVisual::from_components(
                    0.0,
                    0.0,
                    1.0,
                    1.0,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                    visual.blend_mode,
                )
            });
            if let Some(src) = lookup_asset_frame(asset_id, 0) {
                blit_scaled_layer(canvas, &src, evaluated, doc);
            }
        }
        ClipKind::Subtitle(sub) => {
            if let Ok(evaluated) = sub.visual.evaluate(ctx) {
                #[allow(clippy::cast_possible_truncation)]
                let font_size = sub.font_size.evaluate(ctx).unwrap_or(42.0).max(6.0) as f32;
                let wrap_width = doc.resolution.width.get().clamp(128, 3840);
                let buffer = crate::media::subtitle_renderer::render_subtitle_cached(
                    sub, font_size, wrap_width,
                );
                blit_scaled_layer(canvas, &buffer, evaluated, doc);
            }
        }
        ClipKind::Audio { .. } => {}
    }
}

fn is_asset_missing(doc: &ProjectDocument, asset_id: &AssetId) -> bool {
    !matches!(
        doc.find_asset(asset_id).map(|asset| &asset.status),
        Some(AssetStatus::Available)
    )
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn blit_scaled_layer(
    dst: &mut VideoFrameBuffer,
    src: &VideoFrameBuffer,
    visual: EvaluatedVisual,
    doc: &ProjectDocument,
) {
    if src.width == 0 || src.height == 0 || visual.opacity <= 0.001 {
        return;
    }
    let scale_canvas_x = f64::from(dst.width) / f64::from(doc.resolution.width.get());
    let scale_canvas_y = f64::from(dst.height) / f64::from(doc.resolution.height.get());
    let fit_scale = scale_canvas_x.min(scale_canvas_y);

    let drawn_w = (f64::from(src.width) * fit_scale * visual.scale_x.abs()).max(1.0);
    let drawn_h = (f64::from(src.height) * fit_scale * visual.scale_y.abs()).max(1.0);
    let center_x = f64::from(dst.width) * 0.5 + visual.x * scale_canvas_x;
    let center_y = f64::from(dst.height) * 0.5 + visual.y * scale_canvas_y;

    let min_x = (center_x - drawn_w * 0.5).floor().max(0.0) as u32;
    let max_x = (center_x + drawn_w * 0.5).ceil().min(f64::from(dst.width)) as u32;
    let min_y = (center_y - drawn_h * 0.5).floor().max(0.0) as u32;
    let max_y = (center_y + drawn_h * 0.5).ceil().min(f64::from(dst.height)) as u32;

    let left_edge = center_x - drawn_w * 0.5;
    let top_edge = center_y - drawn_h * 0.5;
    let src_bgra_owned = match src.format {
        FramePixelFormat::Bgra8 => None,
        FramePixelFormat::Rgba8 => Some(src.to_bgra8()),
    };
    let src_bytes = src_bgra_owned.as_deref().unwrap_or(&src.data);

    for py in min_y..max_y {
        let src_v = ((f64::from(py) + 0.5 - top_edge) / drawn_h).clamp(0.0, 0.999_999)
            * f64::from(src.height);
        let sy = (src_v as usize).min(src.height as usize - 1);
        for px in min_x..max_x {
            let src_u = ((f64::from(px) + 0.5 - left_edge) / drawn_w).clamp(0.0, 0.999_999)
                * f64::from(src.width);
            let sx = (src_u as usize).min(src.width as usize - 1);
            let src_idx = (sy * (src.width as usize) + sx) * 4;
            let dst_idx = ((py as usize) * (dst.width as usize) + (px as usize)) * 4;
            blend_bgra_pixel(
                &mut dst.data[dst_idx..dst_idx + 4],
                &src_bytes[src_idx..src_idx + 4],
                visual.opacity,
            );
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn blend_bgra_pixel(dst: &mut [u8], src: &[u8], layer_opacity: f64) {
    let alpha = (f64::from(src[3]) / 255.0) * layer_opacity.clamp(0.0, 1.0);
    if alpha <= 0.001 {
        return;
    }
    let inv = 1.0 - alpha;
    for ch in 0..3 {
        let blended = f64::from(src[ch]) * alpha + f64::from(dst[ch]) * inv;
        dst[ch] = blended.clamp(0.0, 255.0).round() as u8;
    }
    dst[3] = 255;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::animation::AnimatedParameter;
    use crate::core::color::RgbaColor;
    use crate::core::ids::{ClipId, LayerId};
    use crate::core::time::FrameCount;
    use crate::media::visual_properties::VisualProperties;
    use crate::timeline::document::{Asset, AssetKind, AssetPath, Layer};

    #[test]
    fn double_buffer_queues_previous_render_image_for_drop() {
        let mut bridge = RenderImageSurfaceBridge::new();
        assert_eq!(bridge.current_frame_id(), 0);

        let f1 = VideoFrameBuffer::solid_bgra8(16, 16, RgbaColor::BLACK);
        bridge.submit_bgra_frame(&f1);
        assert_eq!(bridge.current_frame_id(), 1);
        assert_eq!(bridge.current_dimensions(), Some((16, 16)));
        assert!(bridge.front_image().is_some());
        assert_eq!(bridge.drain_pending_drops().len(), 0);

        let f2 = VideoFrameBuffer::solid_bgra8(32, 18, RgbaColor::WHITE);
        bridge.submit_bgra_frame(&f2);
        assert_eq!(bridge.current_frame_id(), 2);
        assert_eq!(bridge.current_dimensions(), Some((32, 18)));
        let dropped = bridge.drain_pending_drops();
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn compositor_renders_asset_frame_and_detects_missing_status()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("CompositeTest");
        let ok_asset = AssetId::new("ok-img")?;
        let missing_asset = AssetId::new("missing-vid")?;
        doc.assets.push(Asset {
            id: ok_asset.clone(),
            name: "Red".to_owned(),
            path: AssetPath::new(
                Some("red.png".to_owned()),
                Some("C:/tmp/red.png".to_owned()),
            ),
            kind: AssetKind::Image {
                width: 64,
                height: 64,
            },
            status: AssetStatus::Available,
        });
        doc.assets.push(Asset {
            id: missing_asset.clone(),
            name: "Gone".to_owned(),
            path: AssetPath::from_absolute("C:/missing.mkv"),
            kind: AssetKind::Video {
                has_audio: false,
                default_fps: Some(crate::core::time::FrameRate::FPS_30),
                duration_frames: Some(FrameCount::new(30)?),
                width: Some(64),
                height: Some(64),
                source_duration: crate::core::time::RationalDuration::new(1, 1)?,
            },
            status: AssetStatus::Missing {
                expected_path: "C:/missing.mkv".to_owned(),
            },
        });

        let mut layer = Layer::new(LayerId::new("v1")?, "Video");
        layer.clips.push(Clip {
            id: ClipId::new("c1")?,
            name: "OkImg".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(30)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Image {
                asset_id: ok_asset.clone(),
                visual: VisualProperties {
                    opacity: AnimatedParameter::constant(1.0),
                    ..VisualProperties::default()
                },
            },
        });
        layer.clips.push(Clip {
            id: ClipId::new("c2")?,
            name: "MissingVid".to_owned(),
            timeline_start: FrameIndex::new(30),
            duration: FrameCount::new(30)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Video {
                asset_id: missing_asset,
                source_in: crate::core::time::RationalDuration::ZERO,
                playback_rate: crate::core::time::PlaybackRate::NORMAL,
                preserve_pitch: true,
                visual: VisualProperties::default(),
                audio: None,
            },
        });
        doc.layers.push(layer);

        let red = Arc::new(VideoFrameBuffer::solid_bgra8(
            64,
            64,
            RgbaColor::new(255, 0, 0, 255),
        ));
        let mut lookup = |id: &AssetId, _frame: u64| -> Option<Arc<VideoFrameBuffer>> {
            (id == &ok_asset).then(|| Arc::clone(&red))
        };

        let out_ok = composite_timeline_preview(&doc, FrameIndex::ZERO, 64, 64, &mut lookup);
        assert!(!out_ok.has_missing_media);
        let center_idx = (32 * 64 + 32) * 4;
        // BGRA の赤: [0, 0, 255, 255]
        assert_eq!(out_ok.buffer.data[center_idx + 2], 255);

        let out_missing =
            composite_timeline_preview(&doc, FrameIndex::new(35), 64, 64, &mut lookup);
        assert!(out_missing.has_missing_media);
        Ok(())
    }
}
