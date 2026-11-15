//! `cosmic-text` を用いた純粋 Rust 字幕テキスト・アウトラインラスタライザ。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex};

use cosmic_text::{
    Align, Attrs, Buffer, Color as CosmicColor, Family, FontSystem, Metrics, Shaping, SwashCache,
};

use crate::core::color::RgbaColor;

use super::decoder::VideoFrameBuffer;
use super::visual_properties::{SubtitleProperties, TextAlignment};

const MAX_CACHED_SUBTITLES: usize = 128;

static GLOBAL_SUBTITLE_RENDERER: LazyLock<Mutex<SubtitleRenderer>> =
    LazyLock::new(|| Mutex::new(SubtitleRenderer::new()));

/// テキスト・書式・フォントサイズが同じ字幕をキャッシュ再利用するためのハッシュを算出する。
#[must_use]
pub fn subtitle_content_hash(sub: &SubtitleProperties, font_size: f32, max_width: u32) -> u64 {
    let mut hasher = DefaultHasher::new();
    sub.text.hash(&mut hasher);
    sub.font_family.hash(&mut hasher);
    font_size_quantized(font_size).hash(&mut hasher);
    max_width.hash(&mut hasher);
    sub.text_color.red.hash(&mut hasher);
    sub.text_color.green.hash(&mut hasher);
    sub.text_color.blue.hash(&mut hasher);
    sub.text_color.alpha.hash(&mut hasher);
    sub.outline_color.red.hash(&mut hasher);
    sub.outline_color.green.hash(&mut hasher);
    sub.outline_color.blue.hash(&mut hasher);
    sub.outline_color.alpha.hash(&mut hasher);
    outline_width_quantized(sub.outline_width).hash(&mut hasher);
    match sub.alignment {
        TextAlignment::Left => 0_u8.hash(&mut hasher),
        TextAlignment::Center => 1_u8.hash(&mut hasher),
        TextAlignment::Right => 2_u8.hash(&mut hasher),
    }
    hasher.finish()
}

/// グローバルの字幕キャッシュ付きラスタライザで BGRA8 画像を生成して返す。
#[must_use]
pub fn render_subtitle_cached(
    sub: &SubtitleProperties,
    font_size: f32,
    max_width: u32,
) -> Arc<VideoFrameBuffer> {
    match GLOBAL_SUBTITLE_RENDERER.lock() {
        Ok(mut renderer) => renderer.render_cached(sub, font_size, max_width),
        Err(poisoned) => poisoned
            .into_inner()
            .render_cached(sub, font_size, max_width),
    }
}

/// `FontSystem` / `SwashCache` とラスタライズ済みキャッシュを保持する字幕レンダラ。
pub struct SubtitleRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    cache: HashMap<u64, Arc<VideoFrameBuffer>>,
    order: VecDeque<u64>,
}

impl Default for SubtitleRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl SubtitleRenderer {
    /// システムフォントを読み込んだ新しい字幕レンダラを生成する。
    #[must_use]
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            cache: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// キャッシュを参照しつつ字幕テキストを BGRA8 へラスタライズする。
    pub fn render_cached(
        &mut self,
        sub: &SubtitleProperties,
        font_size: f32,
        max_width: u32,
    ) -> Arc<VideoFrameBuffer> {
        let key = subtitle_content_hash(sub, font_size, max_width);
        if let Some(hit) = self.cache.get(&key) {
            return Arc::clone(hit);
        }

        let frame = Arc::new(self.render_subtitle(sub, font_size, max_width));
        if self.cache.len() >= MAX_CACHED_SUBTITLES
            && let Some(oldest) = self.order.pop_front()
        {
            self.cache.remove(&oldest);
        }
        self.order.push_back(key);
        self.cache.insert(key, Arc::clone(&frame));
        frame
    }

    /// キャッシュを介さず字幕テキストと縁取りを BGRA8 バッファへ直接描画する。
    #[must_use]
    pub fn render_subtitle(
        &mut self,
        sub: &SubtitleProperties,
        font_size: f32,
        max_width: u32,
    ) -> VideoFrameBuffer {
        if sub.text.trim().is_empty() {
            return VideoFrameBuffer::solid_bgra8(2, 2, RgbaColor::TRANSPARENT);
        }

        let size_px = font_size.clamp(6.0, 240.0);
        let line_height = (size_px * 1.35).max(8.0);
        let wrap_width = max_width.clamp(64, 4096);
        let outline_px = sub.outline_width.clamp(0.0, 16.0);
        let pad = outline_pad_px(outline_px);

        let raster = self.rasterize_text_glyphs(sub, size_px, line_height, wrap_width, pad);
        let Some(bounds) = raster.bounds else {
            return VideoFrameBuffer::solid_bgra8(2, 2, RgbaColor::TRANSPARENT);
        };

        let outline_mask = if outline_px > 0.05 && sub.outline_color.alpha > 0 {
            compute_outline_alpha(&raster.fg, raster.width, raster.height, outline_px, bounds)
        } else {
            Vec::new()
        };

        composite_cropped_subtitle(&raster, bounds, &outline_mask, sub.outline_color, pad)
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn rasterize_text_glyphs(
        &mut self,
        sub: &SubtitleProperties,
        size_px: f32,
        line_height: f32,
        wrap_width: u32,
        pad: u32,
    ) -> GlyphRasterCanvas {
        let mut buffer = Buffer::new(&mut self.font_system, Metrics::new(size_px, line_height));
        let mut borrowed = buffer.borrow_with(&mut self.font_system);
        borrowed.set_size(Some(wrap_width as f32), None);

        let family = resolve_font_family(&sub.font_family);
        let align = match sub.alignment {
            TextAlignment::Left => Align::Left,
            TextAlignment::Center => Align::Center,
            TextAlignment::Right => Align::Right,
        };
        borrowed.set_text(
            &sub.text,
            &Attrs::new().family(family),
            Shaping::Advanced,
            Some(align),
        );
        borrowed.shape_until_scroll(true);

        let layout_bottom = borrowed
            .layout_runs()
            .map(|run| run.line_top + run.line_height)
            .fold(line_height, f32::max);
        let canvas_w = wrap_width.saturating_add(pad * 2).min(4096);
        let canvas_h = (layout_bottom.ceil() as u32)
            .saturating_add(pad * 2 + 12)
            .clamp(16, 4096);

        let mut canvas = GlyphRasterCanvas::new(canvas_w, canvas_h);
        let text_color = CosmicColor::rgba(
            sub.text_color.red,
            sub.text_color.green,
            sub.text_color.blue,
            sub.text_color.alpha,
        );
        let pad_i32 = i32::try_from(pad).unwrap_or(8);

        borrowed.draw(&mut self.swash_cache, text_color, |x, y, w, h, color| {
            canvas.paint_span(x + pad_i32, y + pad_i32, w, h, color);
        });
        canvas
    }
}

#[derive(Clone, Copy)]
struct PixelBounds {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
}

struct GlyphRasterCanvas {
    width: u32,
    height: u32,
    fg: Vec<[u8; 4]>,
    bounds: Option<PixelBounds>,
}

impl GlyphRasterCanvas {
    fn new(width: u32, height: u32) -> Self {
        let len = (width as usize).saturating_mul(height as usize);
        Self {
            width,
            height,
            fg: vec![[0, 0, 0, 0]; len],
            bounds: None,
        }
    }

    fn paint_span(&mut self, x: i32, y: i32, w: u32, h: u32, color: CosmicColor) {
        let alpha = color.a();
        if alpha == 0 || w == 0 || h == 0 {
            return;
        }
        let max_w = i32::try_from(self.width).unwrap_or(i32::MAX);
        let max_h = i32::try_from(self.height).unwrap_or(i32::MAX);
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = x.saturating_add_unsigned(w).min(max_w);
        let y1 = y.saturating_add_unsigned(h).min(max_h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let src = [color.r(), color.g(), color.b(), alpha];
        for py_i in y0..y1 {
            let Ok(py) = u32::try_from(py_i) else {
                continue;
            };
            for px_i in x0..x1 {
                let Ok(px) = u32::try_from(px_i) else {
                    continue;
                };
                let idx = (py as usize) * (self.width as usize) + (px as usize);
                blend_straight_rgba(&mut self.fg[idx], src);
                self.expand_bounds(px, py);
            }
        }
    }

    fn expand_bounds(&mut self, px: u32, py: u32) {
        self.bounds = Some(match self.bounds {
            None => PixelBounds {
                min_x: px,
                min_y: py,
                max_x: px,
                max_y: py,
            },
            Some(b) => PixelBounds {
                min_x: b.min_x.min(px),
                min_y: b.min_y.min(py),
                max_x: b.max_x.max(px),
                max_y: b.max_y.max(py),
            },
        });
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]
fn compute_outline_alpha(
    fg: &[[u8; 4]],
    width: u32,
    height: u32,
    radius_px: f32,
    bounds: PixelBounds,
) -> Vec<u8> {
    let mut mask = vec![0_u8; fg.len()];
    let r_ceil = radius_px.ceil() as i32;
    let mut disk: Vec<(i32, i32, f32)> = Vec::new();
    for dy in -r_ceil..=r_ceil {
        for dx in -r_ceil..=r_ceil {
            let radius_len = ((dx * dx + dy * dy) as f32).sqrt();
            if radius_len <= radius_px + 0.5 {
                let falloff = (radius_px + 0.5 - radius_len).clamp(0.0, 1.0);
                disk.push((dx, dy, falloff));
            }
        }
    }

    let w_i32 = i32::try_from(width).unwrap_or(i32::MAX);
    let h_i32 = i32::try_from(height).unwrap_or(i32::MAX);
    for sy in bounds.min_y..=bounds.max_y {
        for sx in bounds.min_x..=bounds.max_x {
            let src_a = fg[(sy as usize) * (width as usize) + (sx as usize)][3];
            if src_a == 0 {
                continue;
            }
            for &(dx, dy, falloff) in &disk {
                let nx = (sx as i32) + dx;
                let ny = (sy as i32) + dy;
                if nx < 0 || ny < 0 || nx >= w_i32 || ny >= h_i32 {
                    continue;
                }
                let contrib = (f32::from(src_a) * falloff).round().clamp(0.0, 255.0) as u8;
                let dst_idx = (ny as usize) * (width as usize) + (nx as usize);
                mask[dst_idx] = mask[dst_idx].max(contrib);
            }
        }
    }
    mask
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn composite_cropped_subtitle(
    raster: &GlyphRasterCanvas,
    bounds: PixelBounds,
    outline_mask: &[u8],
    outline_color: RgbaColor,
    pad: u32,
) -> VideoFrameBuffer {
    let x0 = bounds.min_x.saturating_sub(pad);
    let y0 = bounds.min_y.saturating_sub(pad);
    let x1 = bounds.max_x.saturating_add(pad + 1).min(raster.width);
    let y1 = bounds.max_y.saturating_add(pad + 1).min(raster.height);
    let crop_w = x1.saturating_sub(x0).max(2);
    let crop_h = y1.saturating_sub(y0).max(2);

    let mut out = vec![0_u8; (crop_w as usize) * (crop_h as usize) * 4];
    for ry in 0..crop_h {
        let sy = (y0 + ry).min(raster.height.saturating_sub(1));
        for rx in 0..crop_w {
            let sx = (x0 + rx).min(raster.width.saturating_sub(1));
            let src_idx = (sy as usize) * (raster.width as usize) + (sx as usize);
            let fg = raster.fg[src_idx];
            let outline_coverage = outline_mask.get(src_idx).copied().unwrap_or(0);
            let out_a =
                ((u16::from(outline_coverage) * u16::from(outline_color.alpha)) / 255) as u8;
            let outline = [
                outline_color.red,
                outline_color.green,
                outline_color.blue,
                out_a,
            ];
            let blended = composite_fg_over_bg(fg, outline);
            let dst_idx = ((ry as usize) * (crop_w as usize) + (rx as usize)) * 4;
            out[dst_idx] = blended[2];
            out[dst_idx + 1] = blended[1];
            out[dst_idx + 2] = blended[0];
            out[dst_idx + 3] = blended[3];
        }
    }

    VideoFrameBuffer::new_bgra8(crop_w, crop_h, out)
        .unwrap_or_else(|_| VideoFrameBuffer::solid_bgra8(2, 2, RgbaColor::TRANSPARENT))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn blend_straight_rgba(dst: &mut [u8; 4], src: [u8; 4]) {
    if src[3] == 0 {
        return;
    }
    if dst[3] == 0 {
        *dst = src;
        return;
    }
    *dst = composite_fg_over_bg(src, *dst);
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn composite_fg_over_bg(fg: [u8; 4], bg: [u8; 4]) -> [u8; 4] {
    let fa = f64::from(fg[3]) / 255.0;
    let ba = f64::from(bg[3]) / 255.0;
    let inv_fa = 1.0 - fa;
    let out_a = fa + ba * inv_fa;
    if out_a <= 1e-4 {
        return [0, 0, 0, 0];
    }
    let mix = |fc: u8, bc: u8| -> u8 {
        let premul = f64::from(fc) * fa + f64::from(bc) * ba * inv_fa;
        (premul / out_a).clamp(0.0, 255.0).round() as u8
    };
    [
        mix(fg[0], bg[0]),
        mix(fg[1], bg[1]),
        mix(fg[2], bg[2]),
        (out_a * 255.0).clamp(0.0, 255.0).round() as u8,
    ]
}

fn resolve_font_family(font_family: &str) -> Family<'_> {
    let trimmed = font_family.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("sans-serif") {
        Family::SansSerif
    } else if trimmed.eq_ignore_ascii_case("serif") {
        Family::Serif
    } else if trimmed.eq_ignore_ascii_case("monospace") {
        Family::Monospace
    } else {
        Family::Name(trimmed)
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn outline_pad_px(outline_width: f32) -> u32 {
    (outline_width.ceil() as u32).saturating_add(4)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn font_size_quantized(size: f32) -> u32 {
    (size.clamp(1.0, 500.0) * 10.0).round() as u32
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn outline_width_quantized(width: f32) -> u32 {
    (width.clamp(0.0, 100.0) * 10.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::animation::AnimatedParameter;
    use crate::media::visual_properties::VisualProperties;

    #[test]
    fn renders_japanese_multiline_subtitle_with_outline_pixels() {
        let mut renderer = SubtitleRenderer::new();
        let sub = SubtitleProperties {
            text: "テスト字幕テスト\nSecond Line".to_owned(),
            font_family: "sans-serif".to_owned(),
            font_size: AnimatedParameter::constant(36.0),
            text_color: RgbaColor::new(255, 255, 255, 255),
            outline_color: RgbaColor::new(255, 0, 0, 255),
            outline_width: 3.0,
            alignment: TextAlignment::Center,
            visual: VisualProperties::default(),
        };

        let first = renderer.render_cached(&sub, 36.0, 640);
        assert!(first.width > 40);
        assert!(first.height > 40);

        let mut opaque_count = 0_usize;
        let mut has_red_outline = false;
        let mut has_white_fill = false;
        for px in first.data.as_chunks::<4>().0 {
            if px[3] > 200 {
                opaque_count += 1;
            }
            if px[3] > 180 && px[2] > 200 && px[0] < 60 && px[1] < 60 {
                has_red_outline = true;
            }
            if px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200 {
                has_white_fill = true;
            }
        }

        assert!(opaque_count > 100);
        assert!(has_white_fill, "expected white fill glyph pixels");
        assert!(has_red_outline, "expected red outline pixels around glyphs");

        let second = renderer.render_cached(&sub, 36.0, 640);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn empty_subtitle_produces_minimal_transparent_buffer() {
        let mut renderer = SubtitleRenderer::new();
        let sub = SubtitleProperties::new("   ");
        let frame = renderer.render_subtitle(&sub, 32.0, 640);
        assert_eq!((frame.width, frame.height), (2, 2));
        assert!(frame.data.as_chunks::<4>().0.iter().all(|px| px[3] == 0));
    }
}
