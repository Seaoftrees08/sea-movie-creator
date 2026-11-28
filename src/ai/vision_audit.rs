//! タイムライン上の既存画像スライドと全重複字幕の Vision クロス監査＆向き自動再フィット計算。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use crate::ai::context_builder::{TimelineSlidePairSummary, build_timeline_slide_pairs};
use crate::ai::media_inspector::{
    ContactSheetInput, ContactSheetOptions, ThumbnailEncodeOptions,
    build_contact_sheet_jpeg_base64_skipping_errors, encode_thumbnail_jpeg_base64_with_options,
};
use crate::ai::provider::{
    AiAgentTurnRequest, AiContentPart, AiProvider, AiTokenUsage, AiTurnMessage, VisionPackingMode,
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
    packing_mode: VisionPackingMode,
    individual_max_edge_px: u32,
    individual_batch_size: usize,
    sheet_opts: ContactSheetOptions,
}

/// 角度を `(-180.0, 180.0]` の範囲へ正規化する（時計回り正）。非有限値は `0.0` を返す。
#[must_use]
pub(crate) fn normalize_signed_deg(deg: f64) -> f64 {
    if !deg.is_finite() {
        return 0.0;
    }
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

/// 現在の回転角が `90°` 倍数から `±1.0°` 以内であれば厳密な `0 / 90 / 180 / 270` へスナップして返す。
#[must_use]
pub(crate) fn orthogonal_pre_rotation_deg(current_deg: f64) -> Option<i32> {
    if !current_deg.is_finite() {
        return None;
    }
    let wrapped = current_deg.rem_euclid(360.0);
    let rem = wrapped.rem_euclid(90.0);
    let dist = rem.min(90.0 - rem);
    if dist <= ORTHOGONAL_TOLERANCE_DEG {
        // wrapped は [0.0, 360.0) の有限値なので (wrapped / 90.0).round() は 0..=4 の非負整数に収まる
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let quarter = ((wrapped / 90.0).round() as u32) % 4;
        let snapped = match quarter {
            1 => 90,
            2 => 180,
            3 => 270,
            _ => 0,
        };
        Some(snapped)
    } else {
        None
    }
}

/// LLM が返した回転補正値を `{-90, 0, 90, 180}` に正規化する。
#[must_use]
// 入力を [-360, 360] にクランプしてから 90° 単位で丸めるため精度落ち・桁溢れの恐れはない
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

/// キャンバス全体を覆う `cover` 基準倍率を算出する（Backdrop 再フィット計算用）。
#[must_use]
pub(crate) fn cover_scale_for_dims(canvas_w: f64, canvas_h: f64, img_w: u32, img_h: u32) -> f64 {
    (canvas_w / f64::from(img_w.max(1))).max(canvas_h / f64::from(img_h.max(1)))
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
            "vision_unsupported: audit_timeline_slides requires a Vision-capable provider (cannot verify slide visuals or orientation without Vision)"
                .to_owned(),
        );
    };

    let target_indices = parse_slide_indices_arg(args)?;
    let doc = ctx.sandbox.draft_document().clone();
    let all_pairs = build_timeline_slide_pairs(&doc, ctx.project_dir);
    let total_slides = all_pairs.len();
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
            "total_slides": total_slides,
            "audited_count": 0,
            "second_pass_count": 0,
            "skipped_slides": [],
            "parse_warnings": [],
        }));
    }

    let (prepared, skipped_slides) = prepare_accessible_slides(&filtered, ctx.folder_scope);
    let pass_cfg = build_audit_pass_config(args, ctx);
    let mut acc = AuditRunState {
        parsed_by_slide: HashMap::new(),
        skipped_slides,
        parse_warnings: Vec::new(),
    };
    let mut second_pass_count = 0_usize;

    if !prepared.is_empty() {
        run_first_pass_audit(provider, &prepared, &pass_cfg, ctx, &mut acc)?;
        second_pass_count =
            run_second_pass_refinements(provider, &prepared, &pass_cfg, ctx, &mut acc)?;
    }

    let audits = build_final_audit_records(&doc, &prepared, &pass_cfg, &mut acc);

    Ok(json!({
        "audits": audits,
        "total_slides": total_slides,
        "audited_count": audits.len(),
        "second_pass_count": second_pass_count,
        "skipped_slides": acc.skipped_slides,
        "parse_warnings": acc.parse_warnings,
    }))
}

struct AuditRunState {
    parsed_by_slide: HashMap<usize, ParsedAuditItem>,
    skipped_slides: Vec<Value>,
    parse_warnings: Vec<String>,
}

type AuditTurnOutput = (Result<Vec<ParsedAuditItem>, String>, Option<AiTokenUsage>);

fn parse_slide_indices_arg(args: &Value) -> Result<Option<Vec<usize>>, String> {
    match args.get("slide_indices") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(arr)) => {
            let mut indices = Vec::with_capacity(arr.len());
            for v in arr {
                let n = v
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n >= 1)
                    .ok_or_else(|| {
                        "audit_timeline_slides: `slide_indices` items must be integers >= 1"
                            .to_owned()
                    })?;
                indices.push(n);
            }
            Ok(Some(indices))
        }
        Some(_) => Err("audit_timeline_slides: `slide_indices` must be an array".to_owned()),
    }
}

fn prepare_accessible_slides<'a>(
    filtered: &[&'a TimelineSlidePairSummary],
    scope: &crate::ai::media_inspector::AllowedFolderScope,
) -> (Vec<PreparedSlide<'a>>, Vec<Value>) {
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
        if scope.validate_path(&raw_path).is_err() {
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
    (prepared, skipped_slides)
}

fn build_audit_pass_config<'a>(
    args: &'a Value,
    ctx: &ToolExecutionContext<'_>,
) -> AuditPassConfig<'a> {
    let s = ctx.runtime_settings.sanitized();
    AuditPassConfig {
        check_subtitles: args["check_subtitle_mismatch"].as_bool().unwrap_or(true),
        check_orientation: args["check_orientation"].as_bool().unwrap_or(true),
        focus_hint: args["focus_hint"].as_str(),
        packing_mode: s.vision_packing_mode,
        individual_max_edge_px: s.vision_max_edge_px,
        individual_batch_size: s.vision_batch_size,
        sheet_opts: ContactSheetOptions {
            cols: u32::from(s.vision_contact_sheet_cols),
            rows: u32::from(s.vision_contact_sheet_rows),
            tile_max_edge_px: s.vision_contact_sheet_tile_px,
            jpeg_quality: s.vision_jpeg_quality,
        },
    }
}

fn run_first_pass_audit(
    provider: &dyn AiProvider,
    prepared: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
    ctx: &mut ToolExecutionContext<'_>,
    acc: &mut AuditRunState,
) -> Result<(), String> {
    match cfg.packing_mode {
        VisionPackingMode::ContactSheet => {
            run_first_pass_contact_sheets(provider, prepared, cfg, ctx, acc)
        }
        VisionPackingMode::Individual => {
            run_first_pass_individual(provider, prepared, cfg, ctx, acc)
        }
    }
}

fn run_first_pass_contact_sheets(
    provider: &dyn AiProvider,
    prepared: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
    ctx: &mut ToolExecutionContext<'_>,
    acc: &mut AuditRunState,
) -> Result<(), String> {
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

        // 破損・巨大画像 1 枚で他スライドの監査まで全停止しないよう、単一パスでデコード＆個別隔離する
        let inputs: Vec<ContactSheetInput> = chunk
            .iter()
            .map(|slide| ContactSheetInput {
                path: slide.asset_path.clone(),
                badge_index: slide.pair.slide_index,
                extra_rotation_deg: slide.pre_rotation_deg.unwrap_or(0),
            })
            .collect();
        let res = match build_contact_sheet_jpeg_base64_skipping_errors(
            ctx.folder_scope,
            &inputs,
            cfg.sheet_opts,
            false,
        ) {
            Ok(r) => r,
            Err(err) => {
                // チャンク内の全スライドがデコード不可でも後続チャンクは継続できるよう記録して進む
                for slide in chunk {
                    acc.skipped_slides.push(json!({
                        "slide_index": slide.pair.slide_index,
                        "image_clip_id": slide.pair.image_clip_id,
                        "reason": format!("image_decode_failed: {err}"),
                    }));
                }
                continue;
            }
        };
        for failure in &res.skipped {
            if let Some(slide) = chunk.get(failure.input_index) {
                acc.skipped_slides.push(json!({
                    "slide_index": slide.pair.slide_index,
                    "image_clip_id": slide.pair.image_clip_id,
                    "reason": format!("image_decode_failed: {}", failure.reason),
                }));
            }
        }
        let valid_chunk: Vec<PreparedSlide<'_>> = res
            .accepted_input_indices
            .iter()
            .filter_map(|&idx| {
                chunk.get(idx).map(|slide| PreparedSlide {
                    pair: slide.pair,
                    asset_path: slide.asset_path.clone(),
                    pre_rotation_deg: slide.pre_rotation_deg,
                })
            })
            .collect();
        if valid_chunk.is_empty() {
            continue;
        }
        let expected_indices: Vec<usize> = valid_chunk.iter().map(|s| s.pair.slide_index).collect();
        let sheet = res.sheet;
        let prompt = build_contact_sheet_audit_prompt(&valid_chunk, cfg);
        let req = build_vision_audit_turn_request(
            prompt,
            vec![AiContentPart::ImageData {
                mime_type: sheet.mime_type,
                base64_data: sheet.base64_data,
            }],
        );
        collect_audit_turn_into_map(provider, &req, &expected_indices, ctx, acc)?;
    }
    Ok(())
}

fn run_first_pass_individual(
    provider: &dyn AiProvider,
    prepared: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
    ctx: &mut ToolExecutionContext<'_>,
    acc: &mut AuditRunState,
) -> Result<(), String> {
    let chunk_cap = cfg.individual_batch_size.max(1);
    let total = prepared.len();

    for (chunk_idx, chunk) in prepared.chunks(chunk_cap).enumerate() {
        if ctx.is_cancelled() {
            return Err("audit_timeline_slides cancelled by user".to_owned());
        }
        let done = (chunk_idx * chunk_cap).min(total);
        (ctx.on_progress)(&format!(
            "タイムラインのスライド画像を個別監査中 ({done}/{total} 枚)..."
        ));
        ctx.throttle_before_llm_call()?;

        let mut valid_chunk = Vec::new();
        let mut image_parts = Vec::new();
        for slide in chunk {
            match encode_thumbnail_jpeg_base64_with_options(
                ctx.folder_scope,
                &slide.asset_path,
                ThumbnailEncodeOptions {
                    max_edge_px: cfg.individual_max_edge_px,
                    jpeg_quality: cfg.sheet_opts.jpeg_quality,
                    extra_rotation_deg: slide.pre_rotation_deg.unwrap_or(0),
                },
            ) {
                Ok(thumb) => {
                    valid_chunk.push(PreparedSlide {
                        pair: slide.pair,
                        asset_path: slide.asset_path.clone(),
                        pre_rotation_deg: slide.pre_rotation_deg,
                    });
                    image_parts.push(AiContentPart::text(format!(
                        "Slide `slide_index={}`:",
                        slide.pair.slide_index
                    )));
                    image_parts.push(AiContentPart::ImageData {
                        mime_type: thumb.mime_type,
                        base64_data: thumb.base64_data,
                    });
                }
                Err(err) => {
                    acc.skipped_slides.push(json!({
                        "slide_index": slide.pair.slide_index,
                        "image_clip_id": slide.pair.image_clip_id,
                        "reason": format!("image_decode_failed: {err}"),
                    }));
                }
            }
        }
        if valid_chunk.is_empty() {
            continue;
        }
        let expected_indices: Vec<usize> = valid_chunk.iter().map(|s| s.pair.slide_index).collect();
        let prompt = build_contact_sheet_audit_prompt(&valid_chunk, cfg);
        let req = build_vision_audit_turn_request(prompt, image_parts);
        collect_audit_turn_into_map(provider, &req, &expected_indices, ctx, acc)?;
    }
    Ok(())
}

fn collect_audit_turn_into_map(
    provider: &dyn AiProvider,
    req: &AiAgentTurnRequest,
    expected_indices: &[usize],
    ctx: &mut ToolExecutionContext<'_>,
    acc: &mut AuditRunState,
) -> Result<(), String> {
    let (parsed_res, usage) = execute_audit_llm_turn(provider, req, ctx.cancelled)?;
    if let (Some(cb), Some(u)) = (ctx.on_usage.as_deref_mut(), usage) {
        cb(u);
    }
    match parsed_res {
        Ok(items) => {
            for item in items {
                if expected_indices.contains(&item.slide_index) {
                    acc.parsed_by_slide.insert(item.slide_index, item);
                } else {
                    acc.parse_warnings.push(format!(
                        "ignored unexpected slide_index {} outside batch {:?}",
                        item.slide_index, expected_indices
                    ));
                }
            }
        }
        Err(warn) => acc.parse_warnings.push(warn),
    }
    Ok(())
}

fn run_second_pass_refinements(
    provider: &dyn AiProvider,
    prepared: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
    ctx: &mut ToolExecutionContext<'_>,
    acc: &mut AuditRunState,
) -> Result<usize, String> {
    if !cfg.check_orientation {
        return Ok(0);
    }
    let uncertain: Vec<&PreparedSlide<'_>> = prepared
        .iter()
        .filter(|s| {
            s.pre_rotation_deg.is_some()
                && !s.pair.rotation_is_animated
                && acc
                    .parsed_by_slide
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

        let thumb = match encode_thumbnail_jpeg_base64_with_options(
            ctx.folder_scope,
            &slide.asset_path,
            ThumbnailEncodeOptions {
                max_edge_px: SECOND_PASS_SINGLE_EDGE_PX,
                jpeg_quality: cfg.sheet_opts.jpeg_quality.max(72),
                extra_rotation_deg: slide.pre_rotation_deg.unwrap_or(0),
            },
        ) {
            Ok(t) => t,
            Err(err) => {
                acc.parse_warnings.push(format!(
                    "slide {} second-pass thumbnail skipped: {err}",
                    slide.pair.slide_index
                ));
                continue;
            }
        };

        let prompt = build_single_slide_recheck_prompt(slide, cfg);
        let req = build_vision_audit_turn_request(
            prompt,
            vec![AiContentPart::ImageData {
                mime_type: thumb.mime_type,
                base64_data: thumb.base64_data,
            }],
        );
        match execute_audit_llm_turn(provider, &req, ctx.cancelled) {
            Ok((parsed_res, usage)) => {
                if let (Some(cb), Some(u)) = (ctx.on_usage.as_deref_mut(), usage) {
                    cb(u);
                }
                match parsed_res {
                    Ok(items) => {
                        if let Some(mut updated) = items
                            .into_iter()
                            .find(|it| it.slide_index == slide.pair.slide_index)
                        {
                            updated.second_pass_verified = true;
                            acc.parsed_by_slide.insert(slide.pair.slide_index, updated);
                            count += 1;
                        }
                    }
                    Err(warn) => acc.parse_warnings.push(warn),
                }
            }
            Err(err) if err.contains("vision_unsupported") || err.contains("cancelled") => {
                return Err(err);
            }
            Err(err) => {
                // 2 段目単発失敗で 1 段目の監査結果を丸ごと破棄しない
                acc.parse_warnings.push(format!(
                    "slide {} second-pass fallback kept first pass: {err}",
                    slide.pair.slide_index
                ));
            }
        }
    }
    Ok(count)
}

fn execute_audit_llm_turn(
    provider: &dyn AiProvider,
    req: &AiAgentTurnRequest,
    cancelled: Option<&Arc<AtomicBool>>,
) -> Result<AuditTurnOutput, String> {
    let resp = provider
        .send_agent_turn_cancellable(req, cancelled, &mut |_| {})
        .map_err(|err| {
            let (code, is_unsupported, msg) = classify_vision_api_error(&err);
            if is_unsupported {
                format!(
                    "vision_unsupported: audit_timeline_slides aborted (provider does not support Vision): {msg}"
                )
            } else if code == "cancelled" {
                format!("cancelled: {msg}")
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
    let items_res = parse_audit_response_items(&raw_text);
    Ok((items_res, Some(usage)))
}

fn parse_rotation_value(val: &Value) -> Option<i64> {
    if let Some(i) = val.as_i64() {
        return Some(i);
    }
    if let Some(f) = val.as_f64()
        && f.is_finite()
    {
        // f は finite 前提で 90° スナップ用に丸める
        #[allow(clippy::cast_possible_truncation)]
        return Some(f.clamp(-3600.0, 3600.0).round() as i64);
    }
    if let Some(s) = val.as_str() {
        let trimmed = s.trim().trim_start_matches('+');
        if let Ok(f) = trimmed.parse::<f64>()
            && f.is_finite()
        {
            #[allow(clippy::cast_possible_truncation)]
            return Some(f.clamp(-3600.0, 3600.0).round() as i64);
        }
    }
    None
}

fn parse_audit_response_items(raw_text: &str) -> Result<Vec<ParsedAuditItem>, String> {
    let Some(val) = crate::ai::planner::extract_json_object_from_text(raw_text) else {
        return Err("Vision audit response did not contain valid JSON".to_owned());
    };
    let Some(arr) = val.get("audits").and_then(Value::as_array) else {
        return Err("Vision audit JSON missed required `audits` array".to_owned());
    };
    let parsed: Vec<ParsedAuditItem> = arr
        .iter()
        .filter_map(|item| {
            let slide_idx = item
                .get("slide_index")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())?;
            let raw_rot = item
                .get("needs_rotation_deg")
                .and_then(parse_rotation_value)
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
        .collect();
    if parsed.is_empty() && !arr.is_empty() {
        return Err("Vision audit JSON `audits` items lacked valid slide_index".to_owned());
    }
    Ok(parsed)
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

fn format_slide_manifest_line(slide: &PreparedSlide<'_>, is_tile: bool) -> String {
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
    let orient_note = if slide.pair.rotation_is_animated {
        "orientation_auditable=false (animated rotation; return needs_rotation_deg=0)".to_owned()
    } else if slide.pre_rotation_deg.is_some() {
        "orientation_auditable=true".to_owned()
    } else {
        "orientation_auditable=false (non-orthogonal custom angle; return needs_rotation_deg=0)"
            .to_owned()
    };
    if is_tile {
        format!(
            "- Tile badge `#{}` (slide_index={}): file={fname}, {orient_note}, subtitles=[{sub_desc}]",
            slide.pair.slide_index, slide.pair.slide_index
        )
    } else {
        format!(
            "- Slide `slide_index={}`: file={fname}, {orient_note}, subtitles=[{sub_desc}]",
            slide.pair.slide_index
        )
    }
}

fn build_contact_sheet_audit_prompt(
    chunk: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
) -> String {
    let is_tile = cfg.packing_mode == VisionPackingMode::ContactSheet;
    let lines: Vec<String> = chunk
        .iter()
        .map(|s| format_slide_manifest_line(s, is_tile))
        .collect();
    let focus_line = cfg
        .focus_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(String::new, |h| format!("\nFocus hint from user: {h}"));
    let intro = if is_tile {
        "Inspect the attached contact sheet image. Each tile has a yellow badge `#N` in its top-left corner matching `slide_index=N` below."
    } else {
        "Inspect each attached individual image matching `slide_index=N` below."
    };
    let sample_slide_index = chunk.first().map_or(1, |s| s.pair.slide_index);
    format!(
        "{intro}\n\
         Important: each image already reflects the current timeline orthogonal rotation.{focus_line}\n\
         Checks enabled: check_subtitle_mismatch={}, check_orientation={}.\n\
         Rotation rule (if photo is sideways/upside-down relative to upright viewing):\n\
         - `+90` = rotate 90 deg clockwise (e.g., head is pointing LEFT)\n\
         - `-90` = rotate 90 deg counter-clockwise (e.g., head is pointing RIGHT)\n\
         - `180` = upside-down\n\
         - `0` = already upright\n\
         Set `rotation_confidence` to `\"high\"`, `\"medium\"`, or `\"low\"`.\n\
         If any overlapping subtitle contradicts the visible scene, set `subtitle_matches_image=false`, explain in `subtitle_mismatch_reason`, and propose a natural Japanese `recommended_subtitle` (12-26 chars).\n\
         Return ONLY valid JSON:\n\
         {{\"audits\":[{{\"slide_index\":{sample_slide_index},\"visual_summary\":\"...\",\"subtitle_matches_image\":true,\"subtitle_mismatch_reason\":null,\"recommended_subtitle\":null,\"needs_rotation_deg\":0,\"rotation_confidence\":\"high\"}}]}}\n\
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
    let line = format_slide_manifest_line(slide, false);
    let focus_line = cfg
        .focus_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(String::new, |h| format!("\nFocus hint from user: {h}"));
    format!(
        "Second-pass high-resolution orientation and subtitle check for a single slide (`slide_index={}`).{focus_line}\n\
         Verify carefully whether people/buildings/horizon are upright (`0`), need `+90` CW (e.g., head pointing LEFT), `-90` CCW (e.g., head pointing RIGHT), or `180` (upside-down).\n\
         Checks enabled: check_subtitle_mismatch={}, check_orientation={}.\n\
         Return ONLY JSON:\n\
         {{\"audits\":[{{\"slide_index\":{},\"visual_summary\":\"...\",\"subtitle_matches_image\":true,\"subtitle_mismatch_reason\":null,\"recommended_subtitle\":null,\"needs_rotation_deg\":0,\"rotation_confidence\":\"high\"}}]}}\n\
         Slide:\n{line}",
        slide.pair.slide_index, cfg.check_subtitles, cfg.check_orientation, slide.pair.slide_index
    )
}

fn find_clip_visual<'a>(
    doc: &'a ProjectDocument,
    layer_id: Option<&str>,
    clip_id: &str,
) -> Option<&'a VisualProperties> {
    doc.layers
        .iter()
        .filter(|l| layer_id.is_none_or(|id| l.id.as_str() == id))
        .flat_map(|l| l.clips.iter())
        .find(|c| c.id.as_str() == clip_id)
        .and_then(|c| match &c.kind {
            ClipKind::Image { visual, .. } => Some(visual),
            _ => None,
        })
}

fn build_final_audit_records(
    doc: &ProjectDocument,
    prepared: &[PreparedSlide<'_>],
    cfg: &AuditPassConfig<'_>,
    acc: &mut AuditRunState,
) -> Vec<Value> {
    let canvas_w = f64::from(doc.resolution.width.get().max(1));
    let canvas_h = f64::from(doc.resolution.height.get().max(1));
    let mut records = Vec::new();

    for slide in prepared {
        let pair = slide.pair;
        let Some(parsed) = acc.parsed_by_slide.get(&pair.slide_index) else {
            // 前段デコード失敗等で既に skipped_slides 記録済みの場合は二重登録しない
            let already_skipped = acc
                .skipped_slides
                .iter()
                .any(|item| item["slide_index"].as_u64() == u64::try_from(pair.slide_index).ok());
            if !already_skipped {
                acc.skipped_slides.push(json!({
                    "slide_index": pair.slide_index,
                    "image_clip_id": pair.image_clip_id,
                    "reason": "llm_response_missing_or_unparseable",
                }));
            }
            continue;
        };

        let orientation_skipped = if pair.rotation_is_animated {
            Some("animated_rotation")
        } else if slide.pre_rotation_deg.is_none() {
            Some("non_orthogonal_rotation")
        } else {
            None
        };
        let needs_rotation_deg = if cfg.check_orientation && orientation_skipped.is_none() {
            parsed.needs_rotation_deg
        } else {
            0
        };
        let recommended_rotation_deg =
            normalize_signed_deg(pair.current_rotation_deg + f64::from(needs_rotation_deg));

        let (rec_scale_x, rec_scale_y) = compute_recommended_scales(
            doc,
            &ScaleRefInput {
                clip_id: &pair.image_clip_id,
                layer_id: Some(&pair.image_layer_id),
                asset_dimensions: pair.asset_dimensions,
                pre_rotation_deg: slide.pre_rotation_deg,
                needs_rotation_deg,
                canvas_size: (canvas_w, canvas_h),
                use_cover_scale: pair.is_cover_scaled,
            },
        );
        let backdrop_updates =
            build_backdrop_scale_updates(doc, pair, needs_rotation_deg, (canvas_w, canvas_h));
        let subtitle_matches = if cfg.check_subtitles {
            parsed.subtitle_matches_image
        } else {
            true
        };
        let mismatch_reason = if cfg.check_subtitles {
            parsed.subtitle_mismatch_reason.as_deref()
        } else {
            None
        };
        let recommended_subtitle = if cfg.check_subtitles {
            parsed.recommended_subtitle.as_deref()
        } else {
            None
        };

        records.push(json!({
            "slide_index": pair.slide_index,
            "image_clip_id": pair.image_clip_id,
            "image_layer_id": pair.image_layer_id,
            "companion_backdrop_clip_ids": pair.companion_backdrop_clip_ids,
            "companion_backdrop_updates": backdrop_updates,
            "asset_path": pair.asset_path,
            "asset_dimensions": pair.asset_dimensions,
            "current_rotation_deg": pair.current_rotation_deg,
            "rotation_is_animated": pair.rotation_is_animated,
            "needs_rotation_deg": needs_rotation_deg,
            "recommended_rotation_deg": recommended_rotation_deg,
            "rotation_confidence": parsed.rotation_confidence.as_str(),
            "second_pass_verified": parsed.second_pass_verified,
            "orientation_audit_skipped": orientation_skipped,
            "scale_is_animated": pair.scale_is_animated,
            "recommended_scale_x_param": rec_scale_x,
            "recommended_scale_y_param": rec_scale_y,
            "overlapping_subtitles": pair.overlapping_subtitles,
            "visual_summary": parsed.visual_summary.as_str(),
            "subtitle_matches_image": subtitle_matches,
            "subtitle_mismatch_reason": mismatch_reason,
            "recommended_subtitle": recommended_subtitle,
        }));
    }
    records
}

fn build_backdrop_scale_updates(
    doc: &ProjectDocument,
    pair: &TimelineSlidePairSummary,
    needs_rotation_deg: i32,
    canvas_size: (f64, f64),
) -> Vec<Value> {
    if needs_rotation_deg == 0 || pair.companion_backdrop_clip_ids.is_empty() {
        return Vec::new();
    }
    pair.companion_backdrop_clip_ids
        .iter()
        .filter_map(|clip_id| {
            let visual = find_clip_visual(doc, None, clip_id)?;
            let AnimatedParameter::Constant { value: bd_rot_deg } = visual.rotation_deg else {
                return None;
            };
            let bd_pre_rot = orthogonal_pre_rotation_deg(bd_rot_deg)?;
            let bd_rec_rot = normalize_signed_deg(bd_rot_deg + f64::from(needs_rotation_deg));
            let (sx, sy) = compute_recommended_scales(
                doc,
                &ScaleRefInput {
                    clip_id,
                    layer_id: None,
                    asset_dimensions: pair.asset_dimensions,
                    pre_rotation_deg: Some(bd_pre_rot),
                    needs_rotation_deg,
                    canvas_size,
                    use_cover_scale: true,
                },
            );
            let mut obj = serde_json::Map::new();
            obj.insert("clip_id".to_owned(), Value::String(clip_id.clone()));
            obj.insert("recommended_rotation_deg".to_owned(), json!(bd_rec_rot));
            if let (Some(rec_sx), Some(rec_sy)) = (sx, sy) {
                obj.insert("recommended_scale_x_param".to_owned(), rec_sx);
                obj.insert("recommended_scale_y_param".to_owned(), rec_sy);
            }
            Some(Value::Object(obj))
        })
        .collect()
}

struct ScaleRefInput<'a> {
    clip_id: &'a str,
    layer_id: Option<&'a str>,
    asset_dimensions: Option<[u32; 2]>,
    pre_rotation_deg: Option<i32>,
    needs_rotation_deg: i32,
    canvas_size: (f64, f64),
    use_cover_scale: bool,
}

fn compute_recommended_scales(
    doc: &ProjectDocument,
    input: &ScaleRefInput<'_>,
) -> (Option<Value>, Option<Value>) {
    if input.needs_rotation_deg.abs() != 90 {
        return (None, None);
    }
    let Some([raw_w, raw_h]) = input.asset_dimensions else {
        return (None, None);
    };
    let Some(visual) = find_clip_visual(doc, input.layer_id, input.clip_id) else {
        return (None, None);
    };
    let pre_swapped = input
        .pre_rotation_deg
        .is_some_and(|d| d.rem_euclid(180) != 0);
    let (pre_w, pre_h) = if pre_swapped {
        (raw_h, raw_w)
    } else {
        (raw_w, raw_h)
    };
    let (post_w, post_h) = (pre_h, pre_w);
    let (old_ref, new_ref) = if input.use_cover_scale {
        (
            cover_scale_for_dims(input.canvas_size.0, input.canvas_size.1, pre_w, pre_h),
            cover_scale_for_dims(input.canvas_size.0, input.canvas_size.1, post_w, post_h),
        )
    } else {
        (
            fit_scale_for_dims(input.canvas_size.0, input.canvas_size.1, pre_w, pre_h),
            fit_scale_for_dims(input.canvas_size.0, input.canvas_size.1, post_w, post_h),
        )
    };
    let scale_ratio = if old_ref > f64::EPSILON {
        new_ref / old_ref
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
            thinking_blocks: Vec::new(),
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
            thinking_blocks: Vec::new(),
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
            project_dir: None,
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
            project_dir: None,
        };
        let Err(err) = execute_audit_timeline_slides(&json!({}), &mut ctx) else {
            return Err("must abort on vision_unsupported".into());
        };
        assert!(err.contains("vision_unsupported"));
        Ok(())
    }

    #[test]
    fn rotation_snapping_nan_guards_and_unparseable_response_handling_work()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(orthogonal_pre_rotation_deg(90.6), Some(90));
        assert_eq!(orthogonal_pre_rotation_deg(0.6), Some(0));
        assert_eq!(orthogonal_pre_rotation_deg(-0.8), Some(0));
        assert_eq!(orthogonal_pre_rotation_deg(f64::NAN), None);
        assert_eq!(orthogonal_pre_rotation_deg(f64::INFINITY), None);
        assert_eq!(normalize_signed_deg(f64::NAN), 0.0);
        assert_eq!(normalize_signed_deg(f64::NEG_INFINITY), 0.0);

        let guard = TempDirGuard::create("unparseable")?;
        let p1 = guard.0.join("a.jpg");
        let p2 = guard.0.join("b.jpg");
        let bad = guard.0.join("corrupt.jpg");
        write_sample_jpg(&p1, 400, 300)?;
        write_sample_jpg(&p2, 400, 300)?;
        std::fs::write(&bad, b"not a valid jpeg")?;

        let mut doc = build_audit_test_doc(&p1, &p2, &bad)?;
        // slide 2 にアニメーション回転を設定し、rotation_is_animated で保護されることを確認
        if let ClipKind::Image { visual, .. } = &mut doc.layers[0].clips[1].kind {
            visual.rotation_deg = AnimatedParameter::Keyframes {
                keyframes: vec![
                    Keyframe::with_easing(0, -10.0, EasingCurve::Linear),
                    Keyframe::new(59, 10.0),
                ],
                extrapolation: ExtrapolationMode::Clamp,
            };
        }
        let mut sandbox = DraftSandbox::new(doc);
        let scope = AllowedFolderScope::from_folders([&guard.0]);
        let settings = AgentRuntimeSettings {
            vision_packing_mode: VisionPackingMode::Individual,
            ..AgentRuntimeSettings::default()
        };

        // slide 2 のみ返し、slide 1 を欠落させた＋小数文字列回転を含む応答
        let resp = AiAgentTurnResponse {
            text: Some(
                json!({
                    "audits": [{
                        "slide_index": 2,
                        "visual_summary": "揺れアニメ付き",
                        "subtitle_matches_image": true,
                        "needs_rotation_deg": "90.0",
                        "rotation_confidence": "high"
                    }]
                })
                .to_string(),
            ),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            model: "mock-vision".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        };
        let provider = MockAiProvider::with_agent_turns([resp]);
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
            project_dir: None,
        };

        // 空フィルタでも total_slides はタイムライン実数 3 を返すこと
        let empty_filter =
            execute_audit_timeline_slides(&json!({ "slide_indices": [99] }), &mut ctx)?;
        assert_eq!(empty_filter["total_slides"], 3);
        assert_eq!(empty_filter["audited_count"], 0);

        // 個別モード＋破損画像混在＋部分応答でも healthy な slide 2 のみ監査し、slide 1/3 は skipped_slides へ隔離
        let out = execute_audit_timeline_slides(&json!({}), &mut ctx)?;
        assert_eq!(out["audited_count"], 1);
        let audits = out["audits"].as_array().ok_or("audits array")?;
        assert_eq!(audits[0]["slide_index"], 2);
        assert_eq!(audits[0]["rotation_is_animated"], true);
        assert_eq!(audits[0]["orientation_audit_skipped"], "animated_rotation");
        assert_eq!(audits[0]["needs_rotation_deg"], 0);
        let skipped = out["skipped_slides"].as_array().ok_or("skipped array")?;
        assert_eq!(skipped.len(), 2);
        Ok(())
    }

    #[test]
    fn slide_indices_backdrop_and_out_of_batch_guards_work()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::create("backdrop_out_of_batch")?;
        let p1 = guard.0.join("a.jpg");
        let p2 = guard.0.join("b.jpg");
        write_sample_jpg(&p1, 400, 300)?;
        write_sample_jpg(&p2, 400, 300)?;
        let scope = AllowedFolderScope::from_folders([&guard.0]);

        // slide_indices バリデーション: 0 以下や非整数を拒否すること
        assert!(parse_slide_indices_arg(&json!({ "slide_indices": [0] })).is_err());
        assert!(parse_slide_indices_arg(&json!({ "slide_indices": ["1"] })).is_err());
        assert!(parse_slide_indices_arg(&json!({ "slide_indices": [1, 3] }))?.is_some());

        // バッチ範囲外 slide_index の無視（parse_warnings 記録）＋ backdrop 90°/180° 更新案検証
        let mut bd_doc = build_audit_test_doc(&p1, &p2, &p1)?;
        let mut bd_layer = Layer::new(LayerId::new("L_BG")?, "Backdrop");
        bd_layer.clips.push(Clip {
            id: ClipId::new("BG1")?,
            name: "Backdrop 1".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(120)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Image {
                asset_id: AssetId::new("a1")?,
                visual: VisualProperties {
                    rotation_deg: AnimatedParameter::constant(0.0),
                    scale_x: AnimatedParameter::constant(cover_scale_for_dims(
                        1920.0, 1080.0, 400, 300,
                    )),
                    scale_y: AnimatedParameter::constant(cover_scale_for_dims(
                        1920.0, 1080.0, 400, 300,
                    )),
                    ..VisualProperties::default()
                },
            },
        });
        bd_doc.layers.insert(0, bd_layer);
        let bd_pairs = build_timeline_slide_pairs(&bd_doc, None);
        let updates_90 = build_backdrop_scale_updates(&bd_doc, &bd_pairs[0], 90, (1920.0, 1080.0));
        assert_eq!(updates_90.len(), 1);
        assert_eq!(updates_90[0]["recommended_rotation_deg"], 90.0);
        let rec_bd_sx: AnimatedParameter =
            serde_json::from_value(updates_90[0]["recommended_scale_x_param"].clone())?;
        assert!(matches!(rec_bd_sx, AnimatedParameter::Constant { .. }));
        let updates_180 =
            build_backdrop_scale_updates(&bd_doc, &bd_pairs[0], 180, (1920.0, 1080.0));
        assert_eq!(updates_180.len(), 1);
        assert_eq!(updates_180[0]["recommended_rotation_deg"], 180.0);
        assert!(updates_180[0].get("recommended_scale_x_param").is_none());

        // バッチ外 slide_index を含む応答でも無視され parse_warnings が記録されること
        let out_of_batch_resp = AiAgentTurnResponse {
            text: Some(
                json!({
                    "audits": [{
                        "slide_index": 999,
                        "visual_summary": "不正インデックス",
                        "subtitle_matches_image": true,
                        "needs_rotation_deg": 0,
                        "rotation_confidence": "high"
                    }]
                })
                .to_string(),
            ),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            model: "mock-vision".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        };
        let mut bd_sandbox = DraftSandbox::new(bd_doc);
        let bd_settings = AgentRuntimeSettings::default();
        let bd_provider = MockAiProvider::with_agent_turns([out_of_batch_resp]);
        let mut bd_last = None;
        let mut bd_prog = |_: &str| {};
        let mut bd_ctx = ToolExecutionContext {
            folder_scope: &scope,
            sandbox: &mut bd_sandbox,
            llm_provider: Some(&bd_provider),
            runtime_settings: &bd_settings,
            last_llm_call_at: &mut bd_last,
            cancelled: None,
            on_progress: &mut bd_prog,
            on_usage: None,
            extra_extensions: Vec::new(),
            project_dir: None,
        };
        let bd_out = execute_audit_timeline_slides(&json!({ "slide_indices": [1] }), &mut bd_ctx)?;
        assert_eq!(bd_out["audited_count"], 0);
        let warnings = bd_out["parse_warnings"]
            .as_array()
            .ok_or("parse_warnings")?;
        assert_ne!(warnings.len(), 0);

        Ok(())
    }
}
