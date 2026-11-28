//! AI 実行中の VS Code 風アクティビティ・思考ライブ表示および完了後折りたたみ履歴 UI。

use std::time::Instant;

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px, rgba,
};

use crate::ai::session_store::{AiActivityStep, AiActivityStepStatus, AiTurnActivitySummary};
use crate::ui::ai_chat_panel::{AiChatState, AiExecutionState, format_elapsed_duration};
use crate::ui::i18n::{tr, tr_fmt};
use crate::ui::window::SeaEditorWindow;

/// 1 ターン内に保持するアクティビティステップの上限件数。
pub const MAX_ACTIVITY_STEPS: usize = 64;
/// ライブ思考バッファの保持上限文字数。
pub const MAX_THOUGHT_BUFFER_CHARS: usize = 16_000;
/// 展開前にライブ表示する最新ステップ行数。
const LIVE_VISIBLE_STEPS: usize = 6;
/// 完了・キャンセル等で JSON を展開しない状態のライブプレビュー最大行数。
const LIVE_PREVIEW_LINES: usize = 4;

/// ストリーミング中の制御 JSON から抽出したインクリメンタル進捗情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingJsonProgress {
    /// 検出された直近ツール名（例: `apply_draft_edit_command`）。
    pub tool_name: Option<String>,
    /// 表示用の動詞カテゴリ（例: `Editing`, `Analyzed`）。
    pub verb: String,
    /// 表示用の対象名（例: `タイムライン編集`, `ムービー構成`）。
    pub target: String,
    /// バッチ内で現在までに検出された個別編集コマンド件数。
    pub command_count: usize,
    /// 末尾付近で見つかった `text` または `clip_id` の抜粋（最大 24 文字）。
    pub latest_snippet: Option<String>,
}

/// 1 回の AI 応答／エージェントターン中に蓄積されるライブ進捗状態。
#[derive(Debug, Clone, Default)]
pub struct AiLiveActivityState {
    /// ターン開始時刻。
    pub started_at: Option<Instant>,
    /// 進行中および完了済みのツール／活動ステップ一覧（最大 [`MAX_ACTIVITY_STEPS`] 件）。
    pub steps: Vec<AiActivityStep>,
    /// 受信した生思考テキストバッファ。
    pub thought_buffer: String,
    /// `thought_buffer` 更新時に事前計算した末尾数行プレビュー。
    pub thought_tail_cache: String,
    /// 生テキストストリームの受信済み文字数。
    pub streaming_raw_chars: usize,
    /// ストリーミング JSON から抽出した構造化進捗ヒント。
    pub streaming_progress: Option<StreamingJsonProgress>,
    /// 本文ストリームの前置き自然文プレビュー末尾キャッシュ（`on_stream_updated` 時のみ再計算）。
    pub readable_prefix_tail_cache: Option<String>,
    /// ライブ表示内の全ステップ一覧を展開しているか。
    pub steps_expanded: bool,
    /// ライブ思考プレビューの全文を展開しているか。
    pub thought_expanded: bool,
    next_hint_seq: u64,
    accumulated_active_duration: std::time::Duration,
    active_segment_started_at: Option<Instant>,
}

/// ツール実行完了時に [`AiLiveActivityState::on_tool_completed`] へ渡す更新情報。
#[derive(Debug, Clone, Copy)]
pub struct ToolCompletionUpdate<'a> {
    /// 呼び出し識別子。
    pub call_id: &'a str,
    /// VS Code 風動詞カテゴリ。
    pub verb: &'a str,
    /// 対象名。
    pub target: &'a str,
    /// 成功フラグ。
    pub ok: bool,
    /// 結果サマリ。
    pub summary: &'a str,
    /// 実実行所要時間（ミリ秒）。
    pub duration_ms: Option<u64>,
}

impl AiLiveActivityState {
    /// すべてのライブ進捗・思考バッファを初期状態へ戻す。
    pub fn reset(&mut self) {
        self.started_at = None;
        self.steps.clear();
        self.thought_buffer.clear();
        self.thought_tail_cache.clear();
        self.streaming_raw_chars = 0;
        self.streaming_progress = None;
        self.readable_prefix_tail_cache = None;
        self.steps_expanded = false;
        self.thought_expanded = false;
        self.next_hint_seq = 0;
        self.accumulated_active_duration = std::time::Duration::ZERO;
        self.active_segment_started_at = None;
    }

    /// 新しいターン開始時にタイマーのみ起動し直してバッファを空にする。
    pub fn begin_turn(&mut self) {
        self.reset();
        let now = Instant::now();
        self.started_at = Some(now);
        self.active_segment_started_at = Some(now);
    }

    /// `AskUser` や承認待ちに入った際、待機時間を作業時間から除外するためタイマーを一時停止する。
    pub fn pause_active_timer(&mut self) {
        if let Some(started) = self.active_segment_started_at.take() {
            self.accumulated_active_duration += started.elapsed();
        }
    }

    /// ユーザー応答や承認後に AI 作業が再開された際、アクティブ作業タイマーを再開する。
    pub fn resume_active_timer(&mut self) {
        let now = Instant::now();
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if self.active_segment_started_at.is_none() {
            self.active_segment_started_at = Some(now);
        }
    }

    /// 待機時間を除いた正味の AI 作業経過秒数を返す。
    #[must_use]
    pub fn active_elapsed_secs(&self) -> u64 {
        let live = self
            .active_segment_started_at
            .map_or(std::time::Duration::ZERO, |t| t.elapsed());
        (self.accumulated_active_duration + live).as_secs()
    }

    /// 待機時間を除いた正味の経過秒数を返す（互換エイリアス）。
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        self.active_elapsed_secs()
    }

    /// ツールまたは対話確認の開始イベントを記録する。
    pub fn on_tool_started(&mut self, call_id: &str, verb: &str, target: &str) {
        if self.started_at.is_none() {
            self.resume_active_timer();
        }
        if let Some(existing) = self.steps.iter_mut().find(|s| s.id == call_id) {
            verb.clone_into(&mut existing.verb);
            target.clone_into(&mut existing.target);
            existing.status = AiActivityStepStatus::Running;
            return;
        }
        self.push_step_bounded(AiActivityStep {
            id: call_id.to_owned(),
            verb: verb.to_owned(),
            target: target.to_owned(),
            detail: None,
            status: AiActivityStepStatus::Running,
            duration_ms: None,
        });
    }

    /// ツールまたは対話確認の完了・失敗イベントを記録する。
    pub fn on_tool_completed(&mut self, update: ToolCompletionUpdate<'_>) {
        let status = if update.ok {
            AiActivityStepStatus::Done
        } else {
            AiActivityStepStatus::Failed
        };
        let detail = if update.ok {
            None
        } else {
            let trimmed = update.summary.trim();
            (!trimmed.is_empty()).then(|| truncate_chars_with_ellipsis(trimmed, 48))
        };
        if let Some(existing) = self.steps.iter_mut().find(|s| s.id == update.call_id) {
            update.verb.clone_into(&mut existing.verb);
            update.target.clone_into(&mut existing.target);
            existing.status = status;
            existing.detail = detail;
            existing.duration_ms = update.duration_ms;
            return;
        }
        self.push_step_bounded(AiActivityStep {
            id: update.call_id.to_owned(),
            verb: update.verb.to_owned(),
            target: update.target.to_owned(),
            detail,
            status,
            duration_ms: update.duration_ms,
        });
    }

    /// ACP 等からの汎用アクティビティラベルを 1 ステップとして記録する（直前同一ラベルは重複追加しない）。
    pub fn on_activity_hint(&mut self, label: &str) {
        let trimmed = label.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.started_at.is_none() {
            self.resume_active_timer();
        }
        if let Some(last) = self.steps.last()
            && last.target == trimmed
        {
            return;
        }
        for step in &mut self.steps {
            if step.id.starts_with("hint_") && step.status == AiActivityStepStatus::Running {
                step.status = AiActivityStepStatus::Done;
            }
        }
        let seq = self.next_hint_seq;
        self.next_hint_seq = self.next_hint_seq.saturating_add(1);
        self.push_step_bounded(AiActivityStep {
            id: format!("hint_{seq}"),
            verb: "Step".to_owned(),
            target: trimmed.to_owned(),
            detail: None,
            status: AiActivityStepStatus::Running,
            duration_ms: None,
        });
    }

    /// 思考テキストデルタを追記し、プレビューキャッシュを更新する。
    pub fn on_thought_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if self.started_at.is_none() {
            self.resume_active_timer();
        }
        self.thought_buffer.push_str(delta);
        trim_leading_to_max_chars(&mut self.thought_buffer, MAX_THOUGHT_BUFFER_CHARS);
        self.thought_tail_cache = extract_tail_lines(&self.thought_buffer, LIVE_PREVIEW_LINES);
    }

    /// 本文ストリーム更新時に JSON 構築ヒントと前置き自然文キャッシュを再計算する。
    pub fn on_stream_updated(&mut self, full_raw_preview: &str) {
        self.streaming_raw_chars = full_raw_preview.chars().count();
        self.streaming_progress = inspect_streaming_json_progress(full_raw_preview);
        self.readable_prefix_tail_cache = extract_readable_prefix_tail(full_raw_preview);
    }

    /// 本文バッファクリア時にストリームヒントと前置きキャッシュを消去する。
    pub fn clear_stream_progress(&mut self) {
        self.streaming_raw_chars = 0;
        self.streaming_progress = None;
        self.readable_prefix_tail_cache = None;
    }

    /// ターン完了またはキャンセル時に永続化用の [`AiTurnActivitySummary`] を確定してリセットする。
    pub fn finalize_summary(
        &mut self,
        worked_secs_override: Option<u64>,
        cancelled: bool,
    ) -> Option<AiTurnActivitySummary> {
        if self.steps.is_empty() && self.thought_buffer.trim().is_empty() {
            self.reset();
            return None;
        }
        for step in &mut self.steps {
            if step.status == AiActivityStepStatus::Running {
                step.status = if cancelled {
                    AiActivityStepStatus::Cancelled
                } else {
                    AiActivityStepStatus::Done
                };
            }
        }
        let worked_secs = worked_secs_override.unwrap_or_else(|| self.active_elapsed_secs());
        let thought_excerpt = (!self.thought_buffer.trim().is_empty())
            .then(|| extract_tail_lines(&self.thought_buffer, LIVE_PREVIEW_LINES));
        let steps = std::mem::take(&mut self.steps);
        self.reset();
        Some(AiTurnActivitySummary {
            worked_secs,
            steps,
            thought_excerpt,
        })
    }

    fn push_step_bounded(&mut self, step: AiActivityStep) {
        if self.steps.len() >= MAX_ACTIVITY_STEPS {
            if let Some(done_idx) = self
                .steps
                .iter()
                .position(|s| s.status != AiActivityStepStatus::Running)
            {
                self.steps.remove(done_idx);
            } else {
                self.steps.remove(0);
            }
        }
        self.steps.push(step);
    }
}

const KNOWN_STREAM_TOOLS: &[&str] = &[
    "apply_draft_edit_command",
    "compile_movie_blueprint",
    "get_timeline_summary",
    "inspect_media_folder",
    "inspect_images_vision",
    "audit_timeline_slides",
    "validate_draft_plan",
    "ask_user",
    "get_capabilities",
];

fn extract_streaming_tool_name(raw: &str) -> Option<String> {
    let search_slice = raw.rfind("\"tool_calls\"").map_or(raw, |idx| &raw[idx..]);
    let args_pos = search_slice.find("\"arguments\"");

    let key_pattern = "\"name\"";
    let mut latest_known: Option<String> = None;
    let mut first_before_args: Option<String> = None;
    let mut search_from = 0_usize;
    while let Some(rel) = search_slice[search_from..].find(key_pattern) {
        let name_pos = search_from + rel;
        let after_key_idx = name_pos + key_pattern.len();
        let after_colon = search_slice[after_key_idx..].trim_start();
        if let Some(after_c) = after_colon.strip_prefix(':')
            && let Some(val) = parse_json_string_prefix(after_c.trim_start())
            && !val.trim().is_empty()
        {
            if KNOWN_STREAM_TOOLS.contains(&val.as_str()) {
                latest_known = Some(val);
            } else if first_before_args.is_none()
                && raw.contains("\"tool_calls\"")
                && args_pos.is_none_or(|ap| name_pos < ap)
            {
                first_before_args = Some(val);
            }
        }
        search_from = after_key_idx;
    }
    latest_known.or(first_before_args)
}

/// 受信中の生ストリームテキスト（JSON やツール引数構築中を含む）から構造化ヒントを抽出する。
#[must_use]
pub fn inspect_streaming_json_progress(raw: &str) -> Option<StreamingJsonProgress> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let looks_like_json = trimmed.starts_with('{')
        || raw.contains("```json")
        || raw.contains("\"tool_calls\"")
        || raw.contains("\"apply_draft_edit_command\"")
        || raw.contains("\"compile_movie_blueprint\"")
        || raw.contains("\"audit_timeline_slides\"");
    if !looks_like_json {
        return None;
    }

    let tool_name = extract_streaming_tool_name(raw);
    let (verb, target) = match tool_name.as_deref() {
        Some("apply_draft_edit_command") => {
            ("Editing".to_owned(), tr("ai.activity_target_timeline_edit"))
        }
        Some("compile_movie_blueprint") => {
            ("Editing".to_owned(), tr("ai.activity_target_blueprint"))
        }
        Some("get_timeline_summary") => (
            "Analyzed".to_owned(),
            tr("ai.activity_target_timeline_summary"),
        ),
        Some("inspect_media_folder") => {
            ("Analyzed".to_owned(), tr("ai.activity_target_media_folder"))
        }
        Some("inspect_images_vision") => ("Analyzed".to_owned(), tr("ai.activity_target_vision")),
        Some("audit_timeline_slides") => {
            ("Analyzed".to_owned(), tr("ai.activity_target_slide_audit"))
        }
        Some("validate_draft_plan") => ("Verified".to_owned(), tr("ai.activity_target_validate")),
        Some("ask_user") => ("Asking".to_owned(), tr("ai.activity_target_ask_user")),
        Some("get_capabilities") => ("Checked".to_owned(), "Capabilities".to_owned()),
        Some(other) => ("Building".to_owned(), other.to_owned()),
        None => ("Editing".to_owned(), tr("ai.activity_target_edit_command")),
    };

    let command_count = count_inner_command_types(raw);
    let latest_snippet = extract_last_json_string_field(raw, "text")
        .or_else(|| extract_last_json_string_field(raw, "clip_id"))
        .map(|s| truncate_chars_with_ellipsis(&s, 24));

    Some(StreamingJsonProgress {
        tool_name,
        verb,
        target,
        command_count,
        latest_snippet,
    })
}

fn count_inner_command_types(raw: &str) -> usize {
    let mut count = 0_usize;
    let mut rest = raw;
    while let Some(pos) = rest.find("\"type\"") {
        let after_key = &rest[pos + 6..];
        let trimmed = after_key.trim_start();
        if let Some(after_colon) = trimmed.strip_prefix(':')
            && let Some(val) = parse_json_string_prefix(after_colon.trim_start())
            && !matches!(val.as_str(), "batch" | "function" | "object")
        {
            count = count.saturating_add(1);
        }
        rest = &rest[pos + 6..];
    }
    count
}

fn extract_last_json_string_field(raw: &str, field_name: &str) -> Option<String> {
    let key_pattern = format!("\"{field_name}\"");
    let mut latest: Option<String> = None;
    let mut search_from = 0_usize;
    while let Some(rel) = raw[search_from..].find(&key_pattern) {
        let after_key_idx = search_from + rel + key_pattern.len();
        let after_colon = raw[after_key_idx..].trim_start();
        if let Some(after_c) = after_colon.strip_prefix(':')
            && let Some(val) = parse_json_string_prefix(after_c.trim_start())
            && !val.trim().is_empty()
        {
            latest = Some(val);
        }
        search_from = after_key_idx;
    }
    latest
}

fn parse_json_string_prefix(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => {
                // チャンク境界で末尾が `\` のまま途切れた場合も確定済み文字列を返して表示点滅を防ぐ
                let Some(esc) = chars.next() else {
                    break;
                };
                match esc {
                    'n' => out.push(' '),
                    'r' | 't' => {}
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    other => out.push(other),
                }
            }
            other => out.push(other),
        }
    }
    (!out.is_empty()).then_some(out)
}

/// 複数行テキストの末尾 `max_lines` 行を UTF-8 安全に切り出し、先頭省略がある場合は `…` を付与する。
#[must_use]
pub fn extract_tail_lines(text: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return String::new();
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut rev_lines: Vec<&str> = Vec::with_capacity(max_lines);
    let mut has_more = false;
    for raw_line in trimmed.rsplit('\n') {
        if rev_lines.len() == max_lines {
            has_more = true;
            break;
        }
        rev_lines.push(raw_line.trim_end_matches('\r'));
    }
    rev_lines.reverse();
    let mut joined = rev_lines.join("\n");
    let max_preview_chars = 360_usize;
    let char_len = joined.chars().count();
    let truncated_chars = char_len > max_preview_chars;
    if truncated_chars {
        let skip = char_len - max_preview_chars;
        joined = joined.chars().skip(skip).collect();
    }
    if has_more || truncated_chars {
        format!("…\n{joined}")
    } else {
        joined
    }
}

fn trim_leading_to_max_chars(buf: &mut String, max_chars: usize) {
    let total = buf.chars().count();
    if total <= max_chars {
        return;
    }
    let drop_chars = total - max_chars;
    if let Some((byte_idx, _)) = buf.char_indices().nth(drop_chars) {
        buf.drain(..byte_idx);
    }
}

fn truncate_chars_with_ellipsis(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let prefix: String = text.chars().take(max_chars).collect();
    format!("{prefix}...")
}

fn format_step_duration(ms: Option<u64>) -> Option<String> {
    let ms = ms?;
    if ms < 1_000 {
        Some(format!("{ms}ms"))
    } else {
        let tenths = ms / 100;
        Some(format!("{}.{}s", tenths / 10, tenths % 10))
    }
}

fn step_status_icon_and_color(
    status: AiActivityStepStatus,
    theme: &Theme,
) -> (&'static str, gpui_kit::Hsla) {
    match status {
        AiActivityStepStatus::Running => ("◌", theme.primary),
        AiActivityStepStatus::Done => ("✓", rgba(0x22c5_5eff).into()),
        AiActivityStepStatus::Failed => ("✕", theme.danger),
        AiActivityStepStatus::Cancelled => ("■", theme.muted_foreground),
    }
}

fn render_activity_step_row(step: &AiActivityStep, theme: &Theme) -> impl IntoElement {
    let (icon, icon_color) = step_status_icon_and_color(step.status, theme);
    let dur_label = format_step_duration(step.duration_ms);
    let target_text = match &step.detail {
        Some(d) => format!("{} ({d})", step.target),
        None => step.target.clone(),
    };

    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_1p5()
        .px_1p5()
        .py_0p5()
        .rounded(px(4.))
        .bg(theme.background)
        .child(
            h_flex()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .gap_1p5()
                .child(div().flex_shrink_0().text_color(icon_color).child(icon))
                .child(
                    div()
                        .flex_shrink_0()
                        .font_semibold()
                        .text_color(theme.muted_foreground)
                        .child(step.verb.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(theme.foreground)
                        .child(target_text),
                ),
        )
        .children(dur_label.map(|dur| {
            div()
                .flex_shrink_0()
                .text_color(theme.muted_foreground)
                .child(dur)
        }))
}

fn extract_readable_prefix_tail(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') {
        return None;
    }
    let body = if let Some(fence_idx) = crate::ui::ai_chat_panel::find_json_fence_start(raw) {
        raw[..fence_idx].trim()
    } else {
        trimmed
    };
    (!body.is_empty()).then(|| extract_tail_lines(body, LIVE_PREVIEW_LINES))
}

fn render_live_steps_section(
    chat: &AiChatState,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> Option<gpui_kit::AnyElement> {
    let total_steps = chat.live_activity.steps.len();
    if total_steps == 0 {
        return None;
    }
    let visible_slice = if chat.live_activity.steps_expanded || total_steps <= LIVE_VISIBLE_STEPS {
        &chat.live_activity.steps[..]
    } else {
        &chat.live_activity.steps[total_steps - LIVE_VISIBLE_STEPS..]
    };
    let mut steps_box = v_flex().w_full().gap_1();
    for step in visible_slice {
        steps_box = steps_box.child(render_activity_step_row(step, theme));
    }
    if total_steps > LIVE_VISIBLE_STEPS {
        let count_str = total_steps.to_string();
        let toggle_label = if chat.live_activity.steps_expanded {
            tr("ai.activity_hide_steps")
        } else {
            tr_fmt("ai.activity_show_all_steps", &[("count", &count_str)])
        };
        steps_box = steps_box.child(
            div()
                .id("ai-live-steps-toggle")
                .cursor_pointer()
                .text_color(theme.primary)
                .child(toggle_label)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.ai_chat.live_activity.steps_expanded =
                        !this.controller.ai_chat.live_activity.steps_expanded;
                    cx.notify();
                })),
        );
    }
    Some(steps_box.into_any_element())
}

fn render_live_thought_section(
    chat: &AiChatState,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> Option<gpui_kit::AnyElement> {
    if chat.live_activity.thought_buffer.trim().is_empty() {
        return None;
    }
    let body = if chat.live_activity.thought_expanded {
        chat.live_activity.thought_buffer.trim().to_owned()
    } else {
        chat.live_activity.thought_tail_cache.clone()
    };
    let toggle_label = if chat.live_activity.thought_expanded {
        tr("ai.activity_show_less")
    } else {
        tr("ai.activity_show_more")
    };
    Some(
        v_flex()
            .w_full()
            .gap_0p5()
            .p_1p5()
            .rounded(px(4.))
            .bg(theme.background)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(tr("ai.activity_thinking")),
                    )
                    .child(
                        div()
                            .id("ai-live-thought-toggle")
                            .cursor_pointer()
                            .text_color(theme.primary)
                            .child(toggle_label)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.ai_chat.live_activity.thought_expanded =
                                    !this.controller.ai_chat.live_activity.thought_expanded;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .text_color(theme.muted_foreground)
                    .child(body),
            )
            .into_any_element(),
    )
}

fn render_live_streaming_progress_line(
    chat: &AiChatState,
    progress: &StreamingJsonProgress,
) -> String {
    let snippet_suffix = progress
        .latest_snippet
        .as_ref()
        .map(|s| format!(": \"{s}\""))
        .unwrap_or_default();
    let count_str = progress.command_count.to_string();
    if progress.command_count > 0 {
        format!(
            "{}{}",
            tr_fmt(
                "ai.activity_building_commands",
                &[
                    ("verb", &progress.verb),
                    ("target", &progress.target),
                    ("count", &count_str),
                ],
            ),
            snippet_suffix
        )
    } else {
        let chars_str = chat.live_activity.streaming_raw_chars.to_string();
        format!(
            "{}{snippet_suffix}",
            tr_fmt(
                "ai.activity_building_chars",
                &[
                    ("verb", &progress.verb),
                    ("target", &progress.target),
                    ("chars", &chars_str),
                ],
            )
        )
    }
}

/// 実行中の AI ターン向けに VS Code / Antigravity 風の進捗・思考・ステップカードを描画する。
pub fn render_live_activity_card(
    chat: &AiChatState,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let (phase_label, step_label, elapsed_secs) = match &chat.execution_state {
        AiExecutionState::Responding { phase, step, .. } => {
            let step_txt = match step {
                Some((Some(s), Some(t))) => format!("[{s}/{t}] "),
                Some((Some(s), None)) => {
                    let s_str = s.to_string();
                    tr_fmt("ai.activity_step_badge", &[("step", &s_str)])
                }
                _ => String::new(),
            };
            (
                phase.clone(),
                step_txt,
                chat.live_activity.active_elapsed_secs(),
            )
        }
        _ => (
            chat.status_banner.clone(),
            String::new(),
            chat.live_activity.active_elapsed_secs(),
        ),
    };
    let elapsed_label = format_elapsed_duration(elapsed_secs);

    let mut card = v_flex()
        .w_full()
        .gap_1p5()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_1p5()
                .child(
                    h_flex()
                        .flex_1()
                        .min_w(px(0.))
                        .items_center()
                        .gap_1p5()
                        .child(Spinner::new().xsmall().color(theme.primary))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .font_semibold()
                                .text_color(theme.foreground)
                                .child(format!("{step_label}{phase_label}")),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.muted_foreground)
                        .child(elapsed_label),
                ),
        )
        .children(render_live_steps_section(chat, theme, cx))
        .children(render_live_thought_section(chat, theme, cx));

    if let Some(progress) = &chat.live_activity.streaming_progress {
        let line = render_live_streaming_progress_line(chat, progress);
        card = card.child(
            div()
                .w_full()
                .min_w(px(0.))
                .px_1p5()
                .py_1()
                .rounded(px(4.))
                .bg(theme.background)
                .text_color(theme.foreground)
                .child(line),
        );
    }
    if let Some(prefix_tail) = &chat.live_activity.readable_prefix_tail_cache {
        card = card.child(
            div()
                .w_full()
                .min_w(px(0.))
                .text_color(theme.muted_foreground)
                .child(prefix_tail.clone()),
        );
    }
    card
}

/// 完了済みターンに添える折りたたみ可能な「作業時間 · N ステップ」要約UIを描画する。
pub fn render_turn_activity_summary(
    turn_id: u64,
    summary: &AiTurnActivitySummary,
    is_expanded: bool,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let duration = format_elapsed_duration(summary.worked_secs);
    let worked_label = tr_fmt("ai.activity_worked_for", &[("duration", &duration)]);
    let steps_str = summary.steps.len().to_string();
    let steps_label = if summary.steps.len() == 1 {
        tr("ai.activity_steps_count_one")
    } else {
        tr_fmt("ai.activity_steps_count", &[("count", &steps_str)])
    };
    let arrow = if is_expanded { "▾" } else { "▸" };
    let header_text = if summary.steps.is_empty() {
        format!("{worked_label} {arrow}")
    } else {
        format!("{worked_label} · {steps_label} {arrow}")
    };

    let mut box_el = v_flex().w_full().gap_1().my_0p5().child(
        div()
            .id(("ai-turn-activity-toggle", turn_id))
            .cursor_pointer()
            .text_color(theme.muted_foreground)
            .child(header_text)
            .on_click(cx.listener(move |this, _, _, cx| {
                let set = &mut this.controller.ai_chat.expanded_activity_turns;
                if !set.remove(&turn_id) {
                    set.insert(turn_id);
                }
                cx.notify();
            })),
    );

    if is_expanded {
        let mut details = v_flex()
            .w_full()
            .gap_1()
            .p_1p5()
            .rounded(px(4.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary);
        for step in &summary.steps {
            details = details.child(render_activity_step_row(step, theme));
        }
        if let Some(excerpt) = &summary.thought_excerpt
            && !excerpt.trim().is_empty()
        {
            details = details.child(
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .pt_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(tr("ai.activity_thinking")),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .text_color(theme.muted_foreground)
                            .child(excerpt.clone()),
                    ),
            );
        }
        box_el = box_el.child(details);
    }
    box_el
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_streaming_json_progress_parses_partial_multibyte_batch()
    -> Result<(), Box<dyn std::error::Error>> {
        let partial = r#"確認してから反映します。
```json
{"tool_calls":[{"name":"apply_draft_edit_command","arguments":{"command":{"type":"batch","commands":[{"type":"set_parameter","clip_id":"clip_01","value":"a"},{"type":"set_parameter","clip_id":"clip_02","text":"2018年 春の桜と笑顔"#;
        let prog = inspect_streaming_json_progress(partial)
            .ok_or("partial batch JSON must produce StreamingJsonProgress")?;
        assert_eq!(prog.tool_name.as_deref(), Some("apply_draft_edit_command"));
        assert_eq!(prog.verb, "Editing");
        assert_eq!(prog.command_count, 2);
        assert_eq!(prog.latest_snippet.as_deref(), Some("2018年 春の桜と笑顔"));

        let long_multibyte = "{\"type\":\"set_parameter\",\"text\":\"あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほ";
        let prog_long = inspect_streaming_json_progress(long_multibyte)
            .ok_or("long multibyte text must truncate safely")?;
        assert!(
            prog_long
                .latest_snippet
                .as_deref()
                .is_some_and(|s| s.ends_with("..."))
        );

        assert_eq!(
            inspect_streaming_json_progress("通常の日本語メッセージです。"),
            None
        );
        Ok(())
    }

    #[test]
    fn extract_tail_lines_keeps_last_lines_and_handles_utf8() {
        let sample = "1行目\n2行目\n3行目\n4行目\n5行目\n6行目";
        assert_eq!(
            extract_tail_lines(sample, 4),
            "…\n3行目\n4行目\n5行目\n6行目"
        );
        assert_eq!(extract_tail_lines("1行目\n2行目", 4), "1行目\n2行目");
        assert_eq!(extract_tail_lines("", 4), "");
    }

    #[test]
    fn live_activity_state_caps_steps_and_marks_running_cancelled()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = AiLiveActivityState::default();
        state.begin_turn();
        for idx in 0..(MAX_ACTIVITY_STEPS + 10) {
            let id = format!("c_{idx}");
            state.on_tool_started(&id, "Analyzed", &format!("t_{idx}"));
            if idx < MAX_ACTIVITY_STEPS + 8 {
                state.on_tool_completed(ToolCompletionUpdate {
                    call_id: &id,
                    verb: "Analyzed",
                    target: &format!("t_{idx}"),
                    ok: true,
                    summary: "ok",
                    duration_ms: Some(5),
                });
            }
        }
        assert_eq!(state.steps.len(), MAX_ACTIVITY_STEPS);

        state.on_thought_delta("思考テスト1\n思考テスト2");
        let summary = state
            .finalize_summary(Some(15), true)
            .ok_or("summary should be emitted when steps or thoughts exist")?;
        assert_eq!(summary.worked_secs, 15);
        assert_eq!(
            summary.steps.last().map(|s| s.status),
            Some(AiActivityStepStatus::Cancelled)
        );
        assert!(
            summary
                .thought_excerpt
                .as_deref()
                .is_some_and(|t| t.contains("思考テスト2"))
        );
        assert_eq!(state.steps.len(), 0);
        assert_eq!(state.thought_buffer, "");
        Ok(())
    }

    #[test]
    fn streaming_progress_ignores_nested_name_handles_trailing_backslash_and_caches_prefix()
    -> Result<(), Box<dyn std::error::Error>> {
        // 引数内のネスト `"name"`（例: プロジェクト名やトラック名）でツール名が誤上書きされないこと
        let nested_name = r#"{"tool_calls":[{"name":"apply_draft_edit_command","arguments":{"command":{"type":"set_project_meta","name":"オープニング"}}}]}"#;
        let prog = inspect_streaming_json_progress(nested_name)
            .ok_or("nested name JSON must produce progress")?;
        assert_eq!(prog.tool_name.as_deref(), Some("apply_draft_edit_command"));
        assert_eq!(prog.verb, "Editing");

        // `audit_timeline_slides` が登録済みであること
        let audit = r#"{"tool_calls":[{"name":"audit_timeline_slides","arguments":{"focus_hint":"桜\n笑顔\"#;
        let prog_audit = inspect_streaming_json_progress(audit)
            .ok_or("audit_timeline_slides partial JSON must parse")?;
        assert_eq!(
            prog_audit.tool_name.as_deref(),
            Some("audit_timeline_slides")
        );
        assert_eq!(prog_audit.verb, "Analyzed");

        // チャンク境界 `\` で snippet が消えて点滅しないこと
        let trailing_bs = r#"{"tool_calls":[{"name":"apply_draft_edit_command","arguments":{"text":"テスト文字列\"#;
        let prog_bs = inspect_streaming_json_progress(trailing_bs)
            .ok_or("trailing backslash must still produce progress")?;
        assert_eq!(prog_bs.latest_snippet.as_deref(), Some("テスト文字列"));

        // フェンス無し `{"tool_calls"...}` を前置き自然文キャッシュへ漏らさないこと
        let mut live = AiLiveActivityState::default();
        live.on_stream_updated(
            "確認中です。\n{\"tool_calls\":[{\"name\":\"get_timeline_summary\"}]}",
        );
        assert_eq!(
            live.readable_prefix_tail_cache.as_deref(),
            Some("確認中です。")
        );
        live.on_stream_updated("{\"tool_calls\":[]}");
        assert_eq!(live.readable_prefix_tail_cache, None);
        Ok(())
    }
}
