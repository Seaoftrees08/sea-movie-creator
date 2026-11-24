//! タイムライン上の既存画像スライドと全重複字幕の Vision クロス監査＆向き自動再フィット計算。

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::ai::context_builder::{TimelineSlidePairSummary, build_timeline_slide_pairs};
use crate::ai::media_inspector::{
    ContactSheetInput, ContactSheetOptions, ThumbnailEncodeOptions,
    build_contact_sheet_jpeg_base64, encode_thumbnail_jpeg_base64_with_options,
};
use crate::ai::provider::{
    AiAgentTurnRequest, AiContentPart, AiProvider, AiTokenUsage, AiTurnMessage,
};
use crate::ai::toolbox::{ToolExecutionContext, classify_vision_api_error};
use crate::core::animation::AnimatedParameter;
use crate::media::visual_properties::VisualProperties;
use crate::timeline::document::{ClipKind, ProjectDocument};

const SECOND_PASS_SINGLE_EDGE_PX: u32 = 448;
const ORTHOGONAL_TOLERANCE_DEG: f64 = 1.0;

#[derive(Debug, Clone)]
struct ParsedAuditItem {
    slide_index: usize,
    visual_summary: String,
    subtitle_matches_image: bool,
    subtitle_mismatch_reason: Option<String>,
    recommended_subtitle: Option<String>,
    needs_rotation_deg: i32,
    rotation_confidence: String,
    second_pass_verified: bool,
}

struct PreparedSlide<'a> {
    pair: &'a TimelineSlidePairSummary,
    asset_path: PathBuf,
    pre_rotation_deg: Option<i32>,
}

struct AuditPassConfig<'a> {
    check_subtitles: bool,
    check_orientation: bool,
    focus_hint: Option<&'a str>,
    sheet_opts: ContactSheetOptions,
}

/// 角度を `(-180.0, 180.0]` の範囲へ正規化する（時計回り正）。
#[must_use]
pub(crate) fn normalize_signed_deg(deg: f64) -> f64 {
    let mut wrapped = deg.rem_euclid(360.0);
    if wrapped > 180.0 {
        wrapped -= 360.0;
    }
    if wrapped.abs() < 1e-6 {
        0.0
    } else {
        (wrapped * 1000.0).round() / 1000.0
    }
}

/// 現在の回転角が `90°` 倍数から `±1.0°` 以内であれば丸めた事前回転角を返す。
#[must_use]
pub(crate) fn orthogonal_pre_rotation_deg(current_deg: f64) -> Option<i32> {
    let rem = current_deg.rem_euclid(90.0);
    let dist = rem.min(90.0 - rem);
    if dist <= ORTHOGONAL_TOLERANCE_DEG {
        #[allow(clippy::cast_possible_truncation)]
        Some(current_deg.round() as i32)
    } else {
        None
    }
}

/// LLM が返した回転補正値を `{-90, 0, 90, 180}` に正規化する。
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub(crate) fn normalize_quarter_turn_delta(raw_deg: i64) -> i32 {
    let clamped = raw_deg.clamp(-360, 360);
    let snapped = ((clamped as f64 / 90.0).round() as i64) * 90;
    let mut wrapped = snapped.rem_euclid(360);
    if wrapped > 180 {
        wrapped -= 360;
    }
    i32::try_from(wrapped).unwrap_or(0)
}

/// キャンバス内に収める基準倍率を算出する（`blueprint.rs` の計算式と同一）。
#[must_use]
pub(crate) fn fit_scale_for_dims(canvas_w: f64, canvas_h: f64, img_w: u32, img_h: u32) -> f64 {
    (canvas_w / f64::from(img_w.max(1))).min(canvas_h / f64::from(img_h.max(1)))
}

/// 固定値・Easing・Keyframes・Expression すべての `AnimatedParameter` へ比例倍率を掛ける。
#[must_use]
pub(crate) fn scale_animated_parameter(param: &AnimatedParameter, ratio: f64) -> AnimatedParameter {
    match param {
        AnimatedParameter::Constant { value } => AnimatedParameter::Constant {
            value: value * ratio,
        },
        AnimatedParameter::Easing {
            start_value,
            end_value,
            curve,
            extrapolation,
        } => AnimatedParameter::Easing {
            start_value: start_value * ratio,
            end_value: end_value * ratio,
            curve: *curve,
            extrapolation: *extrapolation,
        },
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => {
            let scaled = keyframes
                .iter()
                .map(|kf| crate::core::animation::Keyframe {
                    frame: kf.frame,
                    value: kf.value * ratio,
                    easing: kf.easing,
                })
                .collect();
            AnimatedParameter::Keyframes {
                keyframes: scaled,
                extrapolation: *extrapolation,
            }
        }
        AnimatedParameter::Expression {
            expr,
            base_offset,
            scale,
        } => AnimatedParameter::Expression {
            expr: expr.clone(),
            base_offset: base_offset * ratio,
            scale: scale * ratio,
        },
    }
}

/// タイムライン上の画像スライドと全重複字幕を Vision でクロス監査するホストツール本体。
///
/// # Errors
///
/// Vision 非対応プロバイダ、キャンセル、または通信失敗時にエラーメッセージを返す。
pub fn execute_audit_timeline_slides(
    args: &Value,
    ctx: &mut ToolExecutionContext<'_>,
) -> Result<Value, String> {
    let Some(provider) = ctx.llm_provider.filter(|p| p.supports_vision()) else {
        return Err(
            "audit_timeline_slides requires a Vision-capable provider (cannot verify slide visuals or orientation without Vision)"
                .to_owned(),
        );
    };

    let doc = ctx.sandbox.draft_document().clone();
    let all_pairs = build_timeline_slide_pairs(&doc);
    let target_indices: Option<Vec<usize>> = args["slide_indices"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_u64().and_then(|n| usize::try_from(n).ok()))
            .collect()
    });
    let filtered: Vec<&TimelineSlidePairSummary> = all_pairs
        .iter()
        .filter(|pair| {
            target_indices
                .as_ref()
                .is_none_or(|list| list.contains(&pair.slide_index))
        })
        .collect();

    if filtered.is_empty() {
        return Ok(json!({
            "audits": [],
            "total_slides": 0,
            "audited_count": 0,
            "second_pass_count": 0,
            "skipped_slides": [],
        }));
    }

    let mut prepared = Vec::new();
    let mut skipped_slides = Vec::new();
    for pair in filtered {
        let Some(raw_path) = pair.asset_path.as_deref().map(PathBuf::from) else {
            skipped_slides.push(json!({
                "slide_index": pair.slide_index,
                "image_clip_id": pair.image_clip_id,
                "reason": "missing_asset_path",
            }));
            continue;
        };
        if ctx.folder_scope.validate_path(&raw_path).is_err() {
            skipped_slides.push(json!({
                "slide_index": pair.slide_index,
                "image_clip_id": pair.image_clip_id,
                "asset_path": raw_path.to_string_lossy(),
                "reason": "asset_not_accessible_or_missing",
            }));
            continue;
        }
        prepared.push(PreparedSlide {
            pair,
            asset_path: raw_path,
            pre_rotation_deg: orthogonal_pre_rotation_deg(pair.current_rotation_deg),
        });
    }

    let pass_cfg = AuditPassConfig {
        check_subtitles: args["check_subtitle_mismatch"].as_bool().unwrap_or(true),
        check_orientation: args["check_orientation"].as_bool().unwrap_or(true),
        focus_hint: args["focus_hint"].as_str(),
        sheet_opts: ContactSheetOptions {
            cols: u32::from(ctx.runtime_settings.vision_contact_sheet_cols.clamp(1, 4)),
            rows: u32::from(ctx.runtime_settings.vision_contact_sheet_rows.clamp(1, 4)),
            tile_max_edge_px: ctx
                .runtime_settings
                .vision_contact_sheet_tile_px
                .clamp(192, 512),
            jpeg_quality: ctx.runtime_settings.vision_jpeg_quality.clamp(40, 90),
        },
    };

    let mut parsed_by_slide = run_first_pass_contact_sheets(provider, &prepared, &pass_cfg, ctx)?;
    let second_pass_count =
        run_second_pass_refinements(provider, &prepared, &pass_cfg, &mut parsed_by_slide, ctx)?;

    let audits = build_final_audit_records(
        &doc,
        &prepared,
        &parsed_by_slide,
        pass_cfg.check_orientation,
    );

    Ok(json!({
        "audits": audits,
        "total_slides": all_pairs.len(),
        "audited_count": audits.len(),
        "second_pass_count": second_pass_count,
        "skipped_slides": skipped_slides,
    }))
}

fn run_first_pass_contact_sheets(
    provider: &dyn AiProvider,
    prepared: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
    ctx: &mut ToolExecutionContext<'_>,
) -> Result<HashMap<usize, ParsedAuditItem>, String> {
    let mut results = HashMap::new();
    let chunk_cap = usize::try_from(cfg.sheet_opts.cols * cfg.sheet_opts.rows)
        .unwrap_or(6)
        .max(1);
    let total = prepared.len();

    for (chunk_idx, chunk) in prepared.chunks(chunk_cap).enumerate() {
        if ctx.is_cancelled() {
            return Err("audit_timeline_slides cancelled by user".to_owned());
        }
        let done = (chunk_idx * chunk_cap).min(total);
        (ctx.on_progress)(&format!(
            "タイムラインのスライド画像を監査中 ({done}/{total} 枚)..."
        ));
        ctx.throttle_before_llm_call()?;

        let inputs: Vec<ContactSheetInput> = chunk
            .iter()
            .map(|s| ContactSheetInput {
                path: s.asset_path.clone(),
                badge_index: s.pair.slide_index,
                extra_rotation_deg: s.pre_rotation_deg.unwrap_or(0),
            })
            .collect();
        let sheet = build_contact_sheet_jpeg_base64(ctx.folder_scope, &inputs, cfg.sheet_opts)
            .map_err(|e| e.to_string())?;

        let prompt = build_contact_sheet_audit_prompt(chunk, cfg);
        let req = build_vision_audit_turn_request(
            prompt,
            vec![AiContentPart::ImageData {
                mime_type: sheet.mime_type,
                base64_data: sheet.base64_data,
            }],
        );

        let (parsed, usage) = execute_audit_llm_turn(provider, &req)?;
        if let (Some(cb), Some(u)) = (ctx.on_usage.as_deref_mut(), usage) {
            cb(u);
        }
        for item in parsed {
            results.insert(item.slide_index, item);
        }
    }
    Ok(results)
}

fn run_second_pass_refinements(
    provider: &dyn AiProvider,
    prepared: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
    parsed_by_slide: &mut HashMap<usize, ParsedAuditItem>,
    ctx: &mut ToolExecutionContext<'_>,
) -> Result<usize, String> {
    if !cfg.check_orientation {
        return Ok(0);
    }
    let uncertain: Vec<&PreparedSlide<'_>> = prepared
        .iter()
        .filter(|s| {
            s.pre_rotation_deg.is_some()
                && parsed_by_slide
                    .get(&s.pair.slide_index)
                    .is_some_and(|item| item.rotation_confidence != "high")
        })
        .collect();
    if uncertain.is_empty() {
        return Ok(0);
    }

    let mut count = 0_usize;
    let total_uncertain = uncertain.len();
    for (idx, slide) in uncertain.into_iter().enumerate() {
        if ctx.is_cancelled() {
            return Err("audit_timeline_slides cancelled during second pass".to_owned());
        }
        (ctx.on_progress)(&format!(
            "判定が曖昧なスライドを高解像度で再確認中 ({}/{total_uncertain})...",
            idx + 1
        ));
        ctx.throttle_before_llm_call()?;

        let thumb = encode_thumbnail_jpeg_base64_with_options(
            ctx.folder_scope,
            &slide.asset_path,
            ThumbnailEncodeOptions {
                max_edge_px: SECOND_PASS_SINGLE_EDGE_PX,
                jpeg_quality: cfg.sheet_opts.jpeg_quality.max(72),
                extra_rotation_deg: slide.pre_rotation_deg.unwrap_or(0),
            },
        )
        .map_err(|e| e.to_string())?;

        let prompt = build_single_slide_recheck_prompt(slide, cfg);
        let req = build_vision_audit_turn_request(
            prompt,
            vec![AiContentPart::ImageData {
                mime_type: thumb.mime_type,
                base64_data: thumb.base64_data,
            }],
        );
        let (parsed, usage) = execute_audit_llm_turn(provider, &req)?;
        if let (Some(cb), Some(u)) = (ctx.on_usage.as_deref_mut(), usage) {
            cb(u);
        }
        if let Some(mut updated) = parsed
            .into_iter()
            .find(|it| it.slide_index == slide.pair.slide_index)
        {
            updated.second_pass_verified = true;
            parsed_by_slide.insert(slide.pair.slide_index, updated);
            count += 1;
        }
    }
    Ok(count)
}

fn execute_audit_llm_turn(
    provider: &dyn AiProvider,
    req: &AiAgentTurnRequest,
) -> Result<(Vec<ParsedAuditItem>, Option<AiTokenUsage>), String> {
    let resp = provider.send_agent_turn(req, &mut |_| {}).map_err(|err| {
        let (_code, is_unsupported, msg) = classify_vision_api_error(&err);
        if is_unsupported {
            format!("audit_timeline_slides aborted (provider does not support Vision): {msg}")
        } else {
            msg
        }
    })?;
    let in_chars = req.estimated_input_chars();
    let out_chars = resp.estimated_output_chars();
    let usage = resp
        .usage
        .unwrap_or_else(|| AiTokenUsage::estimate_from_chars(in_chars, out_chars));
    let raw_text = resp.text.unwrap_or_default();
    let items = parse_audit_response_items(&raw_text);
    Ok((items, Some(usage)))
}

fn parse_audit_response_items(raw_text: &str) -> Vec<ParsedAuditItem> {
    let Some(val) = crate::ai::planner::extract_json_object_from_text(raw_text) else {
        return Vec::new();
    };
    let Some(arr) = val.get("audits").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let slide_idx = item
                .get("slide_index")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())?;
            let raw_rot = item
                .get("needs_rotation_deg")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let confidence = item
                .get("rotation_confidence")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| matches!(*s, "high" | "medium" | "low"))
                .unwrap_or("medium")
                .to_owned();
            Some(ParsedAuditItem {
                slide_index: slide_idx,
                visual_summary: item
                    .get("visual_summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                subtitle_matches_image: item
                    .get("subtitle_matches_image")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                subtitle_mismatch_reason: item
                    .get("subtitle_mismatch_reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned),
                recommended_subtitle: item
                    .get("recommended_subtitle")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned),
                needs_rotation_deg: normalize_quarter_turn_delta(raw_rot),
                rotation_confidence: confidence,
                second_pass_verified: false,
            })
        })
        .collect()
}

fn build_vision_audit_turn_request(
    prompt_text: String,
    image_parts: Vec<AiContentPart>,
) -> AiAgentTurnRequest {
    let mut parts = vec![AiContentPart::text(prompt_text)];
    parts.extend(image_parts);
    AiAgentTurnRequest {
        system_prompt: "You are a precise visual timeline auditor. Always inspect the actual pixels and return strict JSON."
            .to_owned(),
        messages: vec![AiTurnMessage::User { parts }],
        tools: Vec::new(),
        response_json_schema: Some(json!({
            "type": "object",
            "required": ["audits"],
            "properties": {
                "audits": { "type": "array" }
            }
        })),
        temperature: Some(0.1),
    }
}

fn format_slide_manifest_line(slide: &PreparedSlide<'_>) -> String {
    let fname = slide
        .asset_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("photo");
    let subs: Vec<String> = slide
        .pair
        .overlapping_subtitles
        .iter()
        .map(|sub| format!("`{}`({})", sub.text, sub.clip_id))
        .collect();
    let sub_desc = if subs.is_empty() {
        "(none)".to_owned()
    } else {
        subs.join(", ")
    };
    let orient_note = if slide.pre_rotation_deg.is_some() {
        "orientation_auditable=true"
    } else {
        "orientation_auditable=false (non-orthogonal custom angle; return needs_rotation_deg=0)"
    };
    format!(
        "- Tile badge `#{}` (slide_index={}): file={fname}, {orient_note}, subtitles=[{sub_desc}]",
        slide.pair.slide_index, slide.pair.slide_index
    )
}

fn build_contact_sheet_audit_prompt(
    chunk: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
) -> String {
    let lines: Vec<String> = chunk.iter().map(format_slide_manifest_line).collect();
    let focus_line = cfg
        .focus_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(String::new, |h| format!("\nFocus hint from user: {h}"));
    format!(
        "Inspect the attached contact sheet image. Each tile has a yellow badge `#N` in its top-left corner matching `slide_index=N` below.\n\
         Important: each tile already reflects the current timeline orthogonal rotation.{focus_line}\n\
         Checks enabled: check_subtitle_mismatch={}, check_orientation={}.\n\
         Rotation rule (if photo is sideways/upside-down relative to upright viewing):\n\
         - `+90` = rotate 90 deg clockwise (e.g., head is pointing LEFT)\n\
         - `-90` = rotate 90 deg counter-clockwise (e.g., head is pointing RIGHT)\n\
         - `180` = upside-down\n\
         - `0` = already upright\n\
         Set `rotation_confidence` to `\"high\"`, `\"medium\"`, or `\"low\"`.\n\
         If any overlapping subtitle contradicts the visible scene, set `subtitle_matches_image=false`, explain in `subtitle_mismatch_reason`, and propose a natural Japanese `recommended_subtitle` (12-26 chars).\n\
         Return ONLY valid JSON:\n\
         {{\"audits\":[{{\"slide_index\":1,\"visual_summary\":\"...\",\"subtitle_matches_image\":true,\"subtitle_mismatch_reason\":null,\"recommended_subtitle\":null,\"needs_rotation_deg\":0,\"rotation_confidence\":\"high\"}}]}}\n\
         Slides:\n{}",
        cfg.check_subtitles,
        cfg.check_orientation,
        lines.join("\n")
    )
}

fn build_single_slide_recheck_prompt(
    slide: &PreparedSlide<'_>,
    cfg: &AuditPassConfig<'_>,
) -> String {
    let line = format_slide_manifest_line(slide);
    format!(
        "Second-pass high-resolution orientation and subtitle check for a single slide (`slide_index={}`).\n\
         Verify carefully whether people/buildings/horizon are upright (`0`), need `+90` CW, `-90` CCW, or `180`.\n\
         Checks enabled: check_subtitle_mismatch={}, check_orientation={}.\n\
         Return ONLY JSON:\n\
         {{\"audits\":[{{\"slide_index\":{},\"visual_summary\":\"...\",\"subtitle_matches_image\":true,\"subtitle_mismatch_reason\":null,\"recommended_subtitle\":null,\"needs_rotation_deg\":0,\"rotation_confidence\":\"high\"}}]}}\n\
         Slide:\n{line}",
        slide.pair.slide_index, cfg.check_subtitles, cfg.check_orientation, slide.pair.slide_index
    )
}

fn find_clip_visual<'a>(
    doc: &'a ProjectDocument,
    layer_id: &str,
    clip_id: &str,
) -> Option<&'a VisualProperties> {
    doc.layers
        .iter()
        .find(|l| l.id.as_str() == layer_id)
        .and_then(|l| l.clips.iter().find(|c| c.id.as_str() == clip_id))
        .and_then(|c| match &c.kind {
            ClipKind::Image { visual, .. } => Some(visual),
            _ => None,
        })
}

fn build_final_audit_records(
    doc: &ProjectDocument,
    prepared: &[PreparedSlide<'_>],
    parsed_by_slide: &HashMap<usize, ParsedAuditItem>,
    check_orientation: bool,
) -> Vec<Value> {
    let canvas_w = f64::from(doc.resolution.width.get().max(1));
    let canvas_h = f64::from(doc.resolution.height.get().max(1));

    prepared
        .iter()
        .map(|slide| {
            let pair = slide.pair;
            let parsed = parsed_by_slide.get(&pair.slide_index);
            let orientation_skipped = if slide.pre_rotation_deg.is_none() {
                Some("non_orthogonal_rotation")
            } else {
                None
            };
            let needs_rotation_deg = if check_orientation && orientation_skipped.is_none() {
                parsed.map_or(0, |it| it.needs_rotation_deg)
            } else {
                0
            };
            let recommended_rotation_deg =
                normalize_signed_deg(pair.current_rotation_deg + f64::from(needs_rotation_deg));

            let (rec_scale_x, rec_scale_y) = compute_recommended_scales(
                doc,
                pair,
                slide.pre_rotation_deg,
                needs_rotation_deg,
                (canvas_w, canvas_h),
            );

            json!({
                "slide_index": pair.slide_index,
                "image_clip_id": pair.image_clip_id,
                "image_layer_id": pair.image_layer_id,
                "asset_path": pair.asset_path,
                "asset_dimensions": pair.asset_dimensions,
                "current_rotation_deg": pair.current_rotation_deg,
                "needs_rotation_deg": needs_rotation_deg,
                "recommended_rotation_deg": recommended_rotation_deg,
                "rotation_confidence": parsed.map_or("low", |it| it.rotation_confidence.as_str()),
                "second_pass_verified": parsed.is_some_and(|it| it.second_pass_verified),
                "orientation_audit_skipped": orientation_skipped,
                "scale_is_animated": pair.scale_is_animated,
                "recommended_scale_x_param": rec_scale_x,
                "recommended_scale_y_param": rec_scale_y,
                "overlapping_subtitles": pair.overlapping_subtitles,
                "visual_summary": parsed.map_or("", |it| it.visual_summary.as_str()),
                "subtitle_matches_image": parsed.is_none_or(|it| it.subtitle_matches_image),
                "subtitle_mismatch_reason": parsed.and_then(|it| it.subtitle_mismatch_reason.as_deref()),
                "recommended_subtitle": parsed.and_then(|it| it.recommended_subtitle.as_deref()),
            })
        })
        .collect()
}

fn compute_recommended_scales(
    doc: &ProjectDocument,
    pair: &TimelineSlidePairSummary,
    pre_rotation_deg: Option<i32>,
    needs_rotation_deg: i32,
    canvas_size: (f64, f64),
) -> (Option<Value>, Option<Value>) {
    if needs_rotation_deg.abs() != 90 {
        return (None, None);
    }
    let Some([raw_w, raw_h]) = pair.asset_dimensions else {
        return (None, None);
    };
    let Some(visual) = find_clip_visual(doc, &pair.image_layer_id, &pair.image_clip_id) else {
        return (None, None);
    };
    let pre_swapped = pre_rotation_deg.is_some_and(|d| d.rem_euclid(180) != 0);
    let (pre_w, pre_h) = if pre_swapped {
        (raw_h, raw_w)
    } else {
        (raw_w, raw_h)
    };
    let (post_w, post_h) = (pre_h, pre_w);
    let old_fit = fit_scale_for_dims(canvas_size.0, canvas_size.1, pre_w, pre_h);
    let new_fit = fit_scale_for_dims(canvas_size.0, canvas_size.1, post_w, post_h);
    let scale_ratio = if old_fit > f64::EPSILON {
        new_fit / old_fit
    } else {
        1.0
    };

    let scaled_x = scale_animated_parameter(&visual.scale_x, scale_ratio);
    let scaled_y = scale_animated_parameter(&visual.scale_y, scale_ratio);
    (
        serde_json::to_value(scaled_x).ok(),
        serde_json::to_value(scaled_y).ok(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use image::{ImageFormat, Rgb, RgbImage};

    use super::*;
    use crate::ai::media_inspector::AllowedFolderScope;
    use crate::ai::provider::{
        AgentRuntimeSettings, AiAgentTurnResponse, AiError, AiRequest, AiResponse, AiStreamEvent,
    };
    use crate::ai::providers::mock::MockAiProvider;
    use crate::ai::toolbox::DraftSandbox;
    use crate::core::animation::{EasingCurve, ExtrapolationMode, Keyframe};
    use crate::core::ids::{AssetId, ClipId, LayerId};
    use crate::core::time::{FrameCount, FrameIndex};
    use crate::media::visual_properties::SubtitleProperties;
    use crate::timeline::document::{Asset, AssetKind, AssetPath, AssetStatus, Clip, Layer};

    struct FailingVisionProvider(String);

    impl AiProvider for FailingVisionProvider {
        fn id(&self) -> &'static str {
            "failing-vision"
        }

        fn display_name(&self) -> &'static str {
            "Failing Vision"
        }

        fn supports_vision(&self) -> bool {
            true
        }

        fn send_streaming(
            &self,
            _request: &AiRequest,
            _on_event: &mut dyn FnMut(AiStreamEvent),
        ) -> Result<AiResponse, AiError> {
            Err(AiError::PluginError(self.0.clone()))
        }

        fn send_agent_turn(
            &self,
            _request: &AiAgentTurnRequest,
            _on_event: &mut dyn FnMut(AiStreamEvent),
        ) -> Result<AiAgentTurnResponse, AiError> {
            Err(AiError::PluginError(self.0.clone()))
        }
    }

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn create(tag: &str) -> Result<Self, std::io::Error> {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("sea_audit_{tag}_{}_{n}", std::process::id()));
            std::fs::create_dir_all(&p)?;
            Ok(Self(p))
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_sample_jpg(path: &Path, w: u32, h: u32) -> Result<(), Box<dyn std::error::Error>> {
        let img = RgbImage::from_pixel(w, h, Rgb([210, 120, 150]));
        img.save_with_format(path, ImageFormat::Jpeg)?;
        Ok(())
    }

    fn build_audit_test_doc(
        p1: &Path,
        p2: &Path,
        p3: &Path,
    ) -> Result<ProjectDocument, Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("AuditTest");
        for (id_s, name, path, w, h) in [
            ("a1", "sakura.jpg", p1, 1200_u32, 800_u32),
            ("a2", "person_side.jpg", p2, 1200, 800),
            ("a3", "tilted.jpg", p3, 800, 600),
        ] {
            doc.assets.push(Asset {
                id: AssetId::new(id_s)?,
                name: name.to_owned(),
                path: AssetPath::new(None, Some(path.to_string_lossy().into_owned())),
                kind: AssetKind::Image {
                    width: w,
                    height: h,
                },
                status: AssetStatus::Available,
            });
        }
        let mut img_layer = Layer::new(LayerId::new("L_IMG")?, "Images");
        img_layer.clips.push(Clip {
            id: ClipId::new("C1")?,
            name: "sakura".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(120)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Image {
                asset_id: AssetId::new("a1")?,
                visual: VisualProperties::default(),
            },
        });
        // Ken Burns Keyframes 付きで 90 度回転が必要なスライド
        let old_fit = fit_scale_for_dims(1920.0, 1080.0, 1200, 800);
        let base_s = old_fit * 1.04;
        img_layer.clips.push(Clip {
            id: ClipId::new("C2")?,
            name: "person_side".to_owned(),
            timeline_start: FrameIndex::new(120),
            duration: FrameCount::new(120)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Image {
                asset_id: AssetId::new("a2")?,
                visual: VisualProperties {
                    scale_x: AnimatedParameter::Keyframes {
                        keyframes: vec![
                            Keyframe::with_easing(0, base_s, EasingCurve::Linear),
                            Keyframe::new(119, base_s * 1.08),
                        ],
                        extrapolation: ExtrapolationMode::Clamp,
                    },
                    scale_y: AnimatedParameter::Keyframes {
                        keyframes: vec![
                            Keyframe::with_easing(0, base_s, EasingCurve::Linear),
                            Keyframe::new(119, base_s * 1.08),
                        ],
                        extrapolation: ExtrapolationMode::Clamp,
                    },
                    rotation_deg: AnimatedParameter::constant(90.0),
                    ..VisualProperties::default()
                },
            },
        });
        // 斜め 15.0° 回転は回転監査スキップ
        img_layer.clips.push(Clip {
            id: ClipId::new("C3")?,
            name: "tilted".to_owned(),
            timeline_start: FrameIndex::new(240),
            duration: FrameCount::new(120)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Image {
                asset_id: AssetId::new("a3")?,
                visual: VisualProperties {
                    rotation_deg: AnimatedParameter::constant(15.0),
                    ..VisualProperties::default()
                },
            },
        });
        let mut sub_layer = Layer::new(LayerId::new("L_SUB")?, "Subs");
        sub_layer.clips.push(Clip {
            id: ClipId::new("S1")?,
            name: "S1".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(120)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(SubtitleProperties::new("りんご飴がおいしかった")),
        });
        doc.layers.push(img_layer);
        doc.layers.push(sub_layer);
        Ok(doc)
    }

    fn sample_two_pass_audit_responses() -> [AiAgentTurnResponse; 2] {
        // 1 巡目: slide 2 は confidence="low" を返し 2 段階再確認を発動させる
        let first_pass_resp = AiAgentTurnResponse {
            text: Some(
                json!({
                    "audits": [
                        {
                            "slide_index": 1,
                            "visual_summary": "満開の桜並木",
                            "subtitle_matches_image": false,
                            "subtitle_mismatch_reason": "桜の写真なのにりんご飴の字幕になっている",
                            "recommended_subtitle": "春の陽気と満開の桜",
                            "needs_rotation_deg": 0,
                            "rotation_confidence": "high"
                        },
                        {
                            "slide_index": 2,
                            "visual_summary": "横向き人物",
                            "subtitle_matches_image": true,
                            "subtitle_mismatch_reason": null,
                            "recommended_subtitle": null,
                            "needs_rotation_deg": 0,
                            "rotation_confidence": "low"
                        },
                        {
                            "slide_index": 3,
                            "visual_summary": "斜めフレーム",
                            "subtitle_matches_image": true,
                            "subtitle_mismatch_reason": null,
                            "recommended_subtitle": null,
                            "needs_rotation_deg": 90,
                            "rotation_confidence": "high"
                        }
                    ]
                })
                .to_string(),
            ),
            tool_calls: Vec::new(),
            model: "mock-vision".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        };
        // 2 段階目: slide 2 を 448px 単体で確認し +90 を確信
        let second_pass_resp = AiAgentTurnResponse {
            text: Some(
                json!({
                    "audits": [{
                        "slide_index": 2,
                        "visual_summary": "人物が横向きのまま",
                        "subtitle_matches_image": true,
                        "needs_rotation_deg": 90,
                        "rotation_confidence": "high"
                    }]
                })
                .to_string(),
            ),
            tool_calls: Vec::new(),
            model: "mock-vision".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        };
        [first_pass_resp, second_pass_resp]
    }

    #[test]
    fn audit_handles_ken_burns_rescaling_second_pass_and_non_orthogonal_skip()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::create("full_flow")?;
        let p1 = guard.0.join("sakura.jpg");
        let p2 = guard.0.join("person.jpg");
        let p3 = guard.0.join("tilted.jpg");
        write_sample_jpg(&p1, 1200, 800)?;
        write_sample_jpg(&p2, 1200, 800)?;
        write_sample_jpg(&p3, 800, 600)?;

        let doc = build_audit_test_doc(&p1, &p2, &p3)?;
        let mut sandbox = DraftSandbox::new(doc);
        let scope = AllowedFolderScope::from_folders_and_files(
            std::iter::empty::<PathBuf>(),
            [&p1, &p2, &p3],
        );
        let settings = AgentRuntimeSettings::default();
        let provider = MockAiProvider::with_agent_turns(sample_two_pass_audit_responses());
        let mut last_at = None;
        let mut logs = Vec::new();
        let mut progress = |msg: &str| logs.push(msg.to_owned());
        let mut ctx = ToolExecutionContext {
            folder_scope: &scope,
            sandbox: &mut sandbox,
            llm_provider: Some(&provider),
            runtime_settings: &settings,
            last_llm_call_at: &mut last_at,
            cancelled: None,
            on_progress: &mut progress,
            on_usage: None,
            extra_extensions: Vec::new(),
        };

        let result = execute_audit_timeline_slides(&json!({}), &mut ctx)?;
        assert_eq!(result["audited_count"], 3);
        assert_eq!(result["second_pass_count"], 1);
        let audits = result["audits"].as_array().ok_or("missing audits array")?;

        // slide 1: 字幕不一致が検出される
        assert_eq!(audits[0]["subtitle_matches_image"], false);
        assert_eq!(audits[0]["recommended_subtitle"], "春の陽気と満開の桜");

        // slide 2: 現行 90° + needs +90° -> recommended_rotation_deg = 180.0, Ken Burns Keyframes が比例倍される
        assert_eq!(audits[1]["second_pass_verified"], true);
        assert_eq!(audits[1]["needs_rotation_deg"], 90);
        assert_eq!(audits[1]["recommended_rotation_deg"], 180.0);
        assert_eq!(audits[1]["scale_is_animated"], true);
        let rec_sx: AnimatedParameter =
            serde_json::from_value(audits[1]["recommended_scale_x_param"].clone())?;
        let pre_fit = fit_scale_for_dims(1920.0, 1080.0, 800, 1200);
        let post_fit = fit_scale_for_dims(1920.0, 1080.0, 1200, 800);
        let expected_s0 =
            fit_scale_for_dims(1920.0, 1080.0, 1200, 800) * 1.04 * (post_fit / pre_fit);
        if let AnimatedParameter::Keyframes { keyframes, .. } = rec_sx {
            assert!((keyframes[0].value - expected_s0).abs() < 1e-6);
            assert!((keyframes[1].value - expected_s0 * 1.08).abs() < 1e-6);
        } else {
            return Err("expected Keyframes for recommended_scale_x_param".into());
        }

        // slide 3: 15.0° 斜め回転は LLM が 90 を返しても強制的に回転監査スキップ扱い
        assert_eq!(
            audits[2]["orientation_audit_skipped"],
            "non_orthogonal_rotation"
        );
        assert_eq!(audits[2]["needs_rotation_deg"], 0);
        assert_eq!(audits[2]["recommended_rotation_deg"], 15.0);
        Ok(())
    }

    #[test]
    fn audit_aborts_with_error_when_provider_rejects_vision()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::create("vision_err")?;
        let p1 = guard.0.join("a.jpg");
        let p2 = guard.0.join("b.jpg");
        let p3 = guard.0.join("c.jpg");
        write_sample_jpg(&p1, 400, 300)?;
        write_sample_jpg(&p2, 400, 300)?;
        write_sample_jpg(&p3, 400, 300)?;

        let doc = build_audit_test_doc(&p1, &p2, &p3)?;
        let mut sandbox = DraftSandbox::new(doc);
        let scope = AllowedFolderScope::from_folders([&guard.0]);
        let settings = AgentRuntimeSettings::default();
        let provider =
            FailingVisionProvider("vision_unsupported: ACP agent cannot process images".to_owned());
        let mut last_at = None;
        let mut progress = |_: &str| {};
        let mut ctx = ToolExecutionContext {
            folder_scope: &scope,
            sandbox: &mut sandbox,
            llm_provider: Some(&provider),
            runtime_settings: &settings,
            last_llm_call_at: &mut last_at,
            cancelled: None,
            on_progress: &mut progress,
            on_usage: None,
            extra_extensions: Vec::new(),
        };
        let Err(err) = execute_audit_timeline_slides(&json!({}), &mut ctx) else {
            return Err("must abort on vision_unsupported".into());
        };
        assert!(err.contains("vision_unsupported"));
        Ok(())
    }
}
