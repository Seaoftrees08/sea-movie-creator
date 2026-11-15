// 多層映像コンポジット用 WGSL シェーダ（変形済み四角形描画 + 色調整5項目 + ブレンドモード + 縮小転送）。

struct LayerUniforms {
    opacity: f32,
    brightness: f32,
    contrast: f32,
    saturation: f32,
    hue_rad: f32,
    gamma: f32,
    blend_mode: u32,
    _pad: u32,
};

struct VertexInput {
    @location(0) ndc_pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) tex_uv: vec2<f32>,
    @location(1) bg_uv: vec2<f32>,
};

@group(0) @binding(0) var layer_tex: texture_2d<f32>;
@group(0) @binding(1) var bg_tex: texture_2d<f32>;
@group(0) @binding(2) var tex_sampler: sampler;
@group(0) @binding(3) var<uniform> params: LayerUniforms;

@vertex
fn vs_layer(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_pos = vec4<f32>(input.ndc_pos, 0.0, 1.0);
    out.tex_uv = input.uv;
    // NDC (-1..1, +Y上) をテクスチャ UV (0..1, +Y下) へ変換して背景サンプル位置を求める。
    out.bg_uv = vec2<f32>(input.ndc_pos.x * 0.5 + 0.5, 0.5 - input.ndc_pos.y * 0.5);
    return out;
}

fn rotate_hue(rgb: vec3<f32>, angle_rad: f32) -> vec3<f32> {
    let k = vec3<f32>(0.57735027, 0.57735027, 0.57735027);
    let c = cos(angle_rad);
    let s = sin(angle_rad);
    return rgb * c + cross(k, rgb) * s + k * dot(k, rgb) * (1.0 - c);
}

fn apply_color_adjustments(rgb_in: vec3<f32>) -> vec3<f32> {
    // 1) 明るさ加算  2) コントラスト(0.5中心)  3) 彩度(Rec.709輝度混合)
    var rgb = (rgb_in + vec3<f32>(params.brightness) - vec3<f32>(0.5)) * params.contrast + vec3<f32>(0.5);
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    rgb = mix(vec3<f32>(luma), rgb, params.saturation);
    // 4) 色相回転  5) ガンマ補正
    rgb = rotate_hue(rgb, params.hue_rad);
    let safe_rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    let inv_gamma = 1.0 / max(params.gamma, 0.01);
    return pow(safe_rgb, vec3<f32>(inv_gamma));
}

fn blend_channel_overlay(bg: f32, fg: f32) -> f32 {
    if bg < 0.5 {
        return 2.0 * bg * fg;
    }
    return 1.0 - 2.0 * (1.0 - bg) * (1.0 - fg);
}

fn apply_blend_mode(bg: vec3<f32>, fg: vec3<f32>, mode: u32) -> vec3<f32> {
    switch mode {
        case 1u: {
            // Add
            return min(bg + fg, vec3<f32>(1.0));
        }
        case 2u: {
            // Multiply
            return bg * fg;
        }
        case 3u: {
            // Screen
            return vec3<f32>(1.0) - (vec3<f32>(1.0) - bg) * (vec3<f32>(1.0) - fg);
        }
        case 4u: {
            // Overlay
            return vec3<f32>(
                blend_channel_overlay(bg.r, fg.r),
                blend_channel_overlay(bg.g, fg.g),
                blend_channel_overlay(bg.b, fg.b),
            );
        }
        default: {
            // Normal
            return fg;
        }
    }
}

@fragment
fn fs_layer(input: VertexOutput) -> @location(0) vec4<f32> {
    let fg_sample = textureSample(layer_tex, tex_sampler, input.tex_uv);
    let bg_sample = textureSample(bg_tex, tex_sampler, input.bg_uv);

    let alpha = clamp(fg_sample.a * params.opacity, 0.0, 1.0);
    let adjusted_fg = apply_color_adjustments(fg_sample.rgb);
    let blended_rgb = apply_blend_mode(bg_sample.rgb, adjusted_fg, params.blend_mode);

    let out_rgb = mix(bg_sample.rgb, blended_rgb, alpha);
    let out_a = alpha + bg_sample.a * (1.0 - alpha);
    return vec4<f32>(clamp(out_rgb, vec3<f32>(0.0), vec3<f32>(1.0)), clamp(out_a, 0.0, 1.0));
}

@vertex
fn vs_blit(@builtin(vertex_index) idx: u32) -> VertexOutput {
    // 画面全体を覆うフルスクリーントライアングル
    let x = f32(i32(idx) & 1) * 4.0 - 1.0;
    let y = f32((i32(idx) >> 1) & 1) * 4.0 - 1.0;
    var out: VertexOutput;
    out.clip_pos = vec4<f32>(x, y, 0.0, 1.0);
    out.tex_uv = vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5);
    out.bg_uv = out.tex_uv;
    return out;
}

@fragment
fn fs_blit(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(layer_tex, tex_sampler, input.tex_uv);
}
