//! GPU アダプタ非存在環境向けの純粋 Rust CPU フォールバック合成器。

use crate::media::compositor::{RenderLayerItem, RenderScene};
use crate::media::decoder::{FramePixelFormat, VideoFrameBuffer};
use crate::media::visual_properties::Matrix2D;

/// [`RenderScene`] を CPU 上でアフィン変形・アルファ合成して BGRA8 バッファとして出力する。
#[must_use]
pub fn render_scene_cpu_bgra8(
    scene: &RenderScene,
    out_width: u32,
    out_height: u32,
) -> VideoFrameBuffer {
    let ow = out_width.max(1);
    let oh = out_height.max(1);
    let proj_w = scene.project_width.max(1);
    let proj_h = scene.project_height.max(1);

    let mut frame = VideoFrameBuffer::solid_bgra8(ow, oh, scene.background_color);
    let scale_x = f64::from(proj_w) / f64::from(ow);
    let scale_y = f64::from(proj_h) / f64::from(oh);

    for layer in &scene.layers {
        composite_layer_into_bgra8(&mut frame, layer, (proj_w, proj_h), (scale_x, scale_y));
    }
    frame
}

fn composite_layer_into_bgra8(
    dst: &mut VideoFrameBuffer,
    layer: &RenderLayerItem,
    project_size: (u32, u32),
    scale_to_proj: (f64, f64),
) {
    let opacity = layer.visual.opacity.clamp(0.0, 1.0);
    if opacity <= 0.0 || layer.source_buffer.width == 0 || layer.source_buffer.height == 0 {
        return;
    }
    let Some(inv_matrix) = invert_affine(&layer.visual.matrix) else {
        return;
    };

    let half_proj_w = f64::from(project_size.0) * 0.5;
    let half_proj_h = f64::from(project_size.1) * 0.5;
    let half_src_w = f64::from(layer.source_buffer.width) * 0.5;
    let half_src_h = f64::from(layer.source_buffer.height) * 0.5;
    let limit_cols = i32::try_from(layer.source_buffer.width).unwrap_or(i32::MAX);
    let limit_rows = i32::try_from(layer.source_buffer.height).unwrap_or(i32::MAX);

    for out_y in 0..dst.height {
        let proj_y = (f64::from(out_y) + 0.5) * scale_to_proj.1 - half_proj_h;
        for out_x in 0..dst.width {
            let proj_x = (f64::from(out_x) + 0.5) * scale_to_proj.0 - half_proj_w;
            let (lx, ly) = inv_matrix.transform_point(proj_x, proj_y);
            let sx = (lx + half_src_w).floor();
            let sy = (ly + half_src_h).floor();
            #[allow(clippy::cast_possible_truncation)]
            let ix = sx as i32;
            #[allow(clippy::cast_possible_truncation)]
            let iy = sy as i32;
            if ix < 0 || iy < 0 || ix >= limit_cols || iy >= limit_rows {
                continue;
            }
            let src_idx = (iy.unsigned_abs() as usize * layer.source_buffer.width as usize
                + ix.unsigned_abs() as usize)
                * 4;
            let dst_idx = (out_y as usize * dst.width as usize + out_x as usize) * 4;
            blend_bgra_pixel(
                &mut dst.data[dst_idx..dst_idx + 4],
                &layer.source_buffer.data[src_idx..src_idx + 4],
                layer.source_buffer.format,
                opacity,
            );
        }
    }
}

fn invert_affine(mat: &Matrix2D) -> Option<Matrix2D> {
    let [m00, m01, tx, m10, m11, ty] = mat.coeffs;
    let det = m00 * m11 - m01 * m10;
    if det.abs() < 1e-9 {
        return None;
    }
    let inv_det = 1.0 / det;
    let inv_00 = m11 * inv_det;
    let inv_01 = -m01 * inv_det;
    let inv_10 = -m10 * inv_det;
    let inv_11 = m00 * inv_det;
    let shift_horiz = -(inv_00 * tx + inv_01 * ty);
    let shift_vert = -(inv_10 * tx + inv_11 * ty);
    Some(Matrix2D {
        coeffs: [inv_00, inv_01, shift_horiz, inv_10, inv_11, shift_vert],
    })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn blend_bgra_pixel(dst: &mut [u8], src: &[u8], fmt: FramePixelFormat, opacity: f64) {
    let (sb, sg, sr, sa_u8) = match fmt {
        FramePixelFormat::Bgra8 => (src[0], src[1], src[2], src[3]),
        FramePixelFormat::Rgba8 => (src[2], src[1], src[0], src[3]),
    };
    let sa = (f64::from(sa_u8) / 255.0) * opacity;
    if sa <= 0.0 {
        return;
    }
    let inv_a = 1.0 - sa;
    dst[0] = (f64::from(sb).mul_add(sa, f64::from(dst[0]) * inv_a))
        .round()
        .clamp(0.0, 255.0) as u8;
    dst[1] = (f64::from(sg).mul_add(sa, f64::from(dst[1]) * inv_a))
        .round()
        .clamp(0.0, 255.0) as u8;
    dst[2] = (f64::from(sr).mul_add(sa, f64::from(dst[2]) * inv_a))
        .round()
        .clamp(0.0, 255.0) as u8;
    dst[3] = 255;
}
