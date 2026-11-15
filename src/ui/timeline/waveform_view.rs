//! 音声クリップ内のリアルタイム波形プレビュー（`AudioMixer::waveform_peaks` 参照・`source_in` 追従）。

use gpui_kit::component::h_flex;
use gpui_kit::{IntoElement, ParentElement as _, Styled as _, div, px, rgba};

use crate::audio::mixer::AudioMixer;
use crate::timeline::document::{Clip, ClipKind, ProjectDocument};

const WAVEFORM_SAMPLES_PER_PEAK: usize = 512;

/// 指定音声クリップの `source_in` / `duration` / `playback_rate` に対応するピーク振幅 (`0.0..=1.0`) 列を抽出する。
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn sample_clip_waveform_bars(
    mixer: &AudioMixer,
    doc: &ProjectDocument,
    clip: &Clip,
    bar_count: usize,
) -> Vec<f32> {
    let count = bar_count.clamp(1, 256);
    let ClipKind::Audio {
        asset_id,
        source_in,
        playback_rate,
        ..
    } = &clip.kind
    else {
        return Vec::new();
    };
    let Some(peaks) = mixer.waveform_peaks(asset_id, WAVEFORM_SAMPLES_PER_PEAK) else {
        return Vec::new();
    };
    if peaks.bins.is_empty() || peaks.samples_per_peak == 0 {
        return Vec::new();
    }

    let fps = doc.fps.as_f64().max(1.0);
    let sr = f64::from(peaks.sample_rate.max(1));
    let src_in_samples = source_in.as_seconds_f64().max(0.0) * sr;
    let rate = playback_rate.as_f64().max(0.05);
    let src_span_samples = ((clip.duration.get() as f64 / fps) * rate * sr).max(1.0);
    let samples_per_bar = src_span_samples / (count as f64);
    let spp = peaks.samples_per_peak as f64;
    let last_bin = peaks.bins.len() - 1;

    let mut out = Vec::with_capacity(count);
    for ix in 0..count {
        let s0 = src_in_samples + (ix as f64) * samples_per_bar;
        let s1 = src_in_samples + ((ix + 1) as f64) * samples_per_bar;
        let b0 = ((s0 / spp).floor().max(0.0) as usize).min(last_bin);
        let b1 = ((s1 / spp).ceil().max(0.0) as usize).min(last_bin).max(b0);
        let mut amp = 0.0_f32;
        for bin in &peaks.bins[b0..=b1] {
            amp = amp.max(bin.max.abs().max(bin.min.abs()));
        }
        out.push(amp.clamp(0.0, 1.0));
    }
    out
}

/// 音声クリップ下層に緑 (`#4ADE80`) の波形バー列を描画する。
#[must_use]
pub fn render_audio_clip_waveform(bars: &[f32], lane_height: f32) -> impl IntoElement {
    let max_bar_h = (lane_height - 10.0).max(6.0);
    let mut strip = h_flex()
        .absolute()
        .left_1()
        .right_1()
        .bottom_0()
        .h(px(max_bar_h))
        .items_end()
        .justify_between()
        .gap(px(1.));
    for &amp in bars {
        let bar_h = (amp * max_bar_h).clamp(1.0, max_bar_h);
        strip = strip.child(
            div()
                .flex_1()
                .min_w(px(1.))
                .h(px(bar_h))
                .bg(rgba(0x4ade_80c8)),
        );
    }
    strip
}
