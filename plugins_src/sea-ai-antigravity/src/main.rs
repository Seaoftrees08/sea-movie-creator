//! `sea-movie-creator` 用 Google Antigravity (`agy --print stream-json`) ヘッドレス CLI アダプタプラグイン。
//!
//! 認証バイパス・内部トークン読み取り等は一切行わず、公式 `agy` CLI バイナリの標準入出力を呼び出します。

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const INLINE_SCHEMA_LIMIT_BYTES: usize = 2048;
static SCHEMA_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempSchemaFile(Option<PathBuf>);

impl Drop for TempSchemaFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn prepare_schema_arg(schema_str: &str) -> Result<(String, TempSchemaFile), String> {
    if schema_str.len() <= INLINE_SCHEMA_LIMIT_BYTES {
        return Ok((schema_str.to_owned(), TempSchemaFile(None)));
    }
    let seq = SCHEMA_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("sea_agy_schema_{}_{seq}.json", std::process::id()));
    fs::write(&path, schema_str.as_bytes())
        .map_err(|err| format!("一時 JSON Schema ファイル作成に失敗しました: {err}"))?;
    let arg = path.to_string_lossy().into_owned();
    Ok((arg, TempSchemaFile(Some(path))))
}

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct RequestEnvelope {
    params: CallParams,
}

#[derive(Debug, Deserialize)]
struct CallParams {
    system_prompt: String,
    command_schema_json: Value,
    context_summary: Value,
    user_prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    external_cli: Option<String>,
    #[serde(default)]
    external_args: Vec<String>,
}

fn emit_event(event: &Value) {
    if let Ok(line) = serde_json::to_string(event) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

fn build_agy_command_args(schema_json_str: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--print".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--json-schema".to_owned(),
        schema_json_str.to_owned(),
    ];
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        args.push("--model".to_owned());
        args.push(m.to_owned());
    }
    args
}

fn resolve_executable_in_path(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    let path_var = env::var_os("PATH")?;
    let mut suffixes = vec![String::new()];
    if cfg!(windows) {
        suffixes.extend([".exe".to_owned(), ".cmd".to_owned(), ".bat".to_owned()]);
    }
    for dir in env::split_paths(&path_var) {
        for ext in &suffixes {
            let full = if name.ends_with(ext.as_str()) {
                dir.join(name)
            } else {
                dir.join(format!("{name}{ext}"))
            };
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

fn resolve_agy_cli(override_cli: Option<&str>) -> Result<PathBuf, String> {
    if let Some(explicit) = override_cli.map(str::trim).filter(|s| !s.is_empty()) {
        return resolve_executable_in_path(explicit).ok_or_else(|| {
            format!("指定された `agy` 実行ファイルが見つかりませんでした: `{explicit}`")
        });
    }
    if let Ok(env_cli) = env::var("SEA_ANTIGRAVITY_CLI")
        && !env_cli.trim().is_empty()
    {
        return resolve_executable_in_path(env_cli.trim()).ok_or_else(|| {
            format!("環境変数 `SEA_ANTIGRAVITY_CLI` の実行ファイルが見つかりません: `{env_cli}`")
        });
    }
    resolve_executable_in_path("agy").ok_or_else(|| {
        "Antigravity CLI (`agy`) が PATH 上に見つかりませんでした。\
         Antigravity をインストールし `agy` にサインインした状態で PATH を設定してください。"
            .to_owned()
    })
}

fn parse_agy_stream_json_line(trimmed: &str) -> Result<(Option<String>, Option<String>), String> {
    let val: Value = serde_json::from_str(trimmed)
        .map_err(|err| format!("`agy` stream-json 解析エラー: {err}"))?;
    if let Some(delta) = val["delta"]["text"].as_str() {
        return Ok((Some(delta.to_owned()), None));
    }
    if val["type"].as_str() == Some("assistant")
        && let Some(content) = val["message"]["content"].as_array()
    {
        let mut buf = String::new();
        for item in content {
            if let Some(text) = item["text"].as_str() {
                buf.push_str(text);
            }
        }
        if !buf.is_empty() {
            return Ok((Some(buf), None));
        }
    }
    if let Some(text) = val["text"].as_str() {
        return Ok((Some(text.to_owned()), None));
    }
    if val["type"].as_str() == Some("result")
        && let Some(res) = val["result"].as_str()
    {
        return Ok((None, Some(res.to_owned())));
    }
    Ok((None, None))
}

fn run_request(params: &CallParams) -> Result<(), String> {
    let agy_bin = resolve_agy_cli(params.external_cli.as_deref()).map_err(|msg| {
        emit_event(&json!({
            "event": "error",
            "code": "missing_external_cli",
            "message": msg,
        }));
        msg
    })?;

    let schema_str = serde_json::to_string(&params.command_schema_json)
        .map_err(|err| format!("コマンドスキーマの直列化に失敗しました: {err}"))?;
    let (schema_arg, _schema_guard) = prepare_schema_arg(&schema_str)?;
    let mut args = build_agy_command_args(&schema_arg, params.model.as_deref());
    args.extend(params.external_args.iter().cloned());

    emit_event(&json!({
        "event": "status",
        "message": format!("Antigravity CLI ({}) を起動中...", agy_bin.display()),
    }));

    let mut child = Command::new(&agy_bin)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("`agy` 起動に失敗しました: {err}"))?;

    let full_prompt = format!(
        "{}\n\n# Timeline Context\n{}\n\n# Edit Request\n{}",
        params.system_prompt, params.context_summary, params.user_prompt
    );
    let input_frame = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": full_prompt }]
        }
    });
    if let Some(mut child_stdin) = child.stdin.take() {
        let encoded = serde_json::to_string(&input_frame)
            .map_err(|err| format!("`agy` stream-json 入力生成失敗: {err}"))?;
        writeln!(child_stdin, "{encoded}")
            .and_then(|()| child_stdin.flush())
            .map_err(|err| format!("`agy` stdin 書き込み失敗: {err}"))?;
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "`agy` stdout パイプ取得失敗".to_owned())?;
    let reader = BufReader::new(stdout);
    let mut streamed = String::new();
    let mut final_result = None;

    for line in reader.lines() {
        let line = line.map_err(|err| format!("`agy` stdout 読取エラー: {err}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (delta_opt, result_opt) = parse_agy_stream_json_line(trimmed)?;
        if let Some(delta) = delta_opt {
            streamed.push_str(&delta);
            emit_event(&json!({ "event": "delta", "text": delta }));
        }
        if result_opt.is_some() {
            final_result = result_opt;
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("`agy` 終了待機エラー: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "`agy` が終了コード {} で失敗しました: {stderr}",
            output.status
        ));
    }

    let chosen = final_result.unwrap_or(streamed);
    emit_event(&json!({
        "event": "completed",
        "raw_text": chosen,
        "model": params.model.clone().unwrap_or_else(|| "antigravity".to_owned()),
    }));
    Ok(())
}

fn main() -> ExitCode {
    let mut input = String::new();
    let mut stdin = std::io::stdin().lock();
    if let Err(err) = stdin.read_line(&mut input) {
        emit_event(&json!({ "event": "error", "code": "io_error", "message": err.to_string() }));
        return ExitCode::FAILURE;
    }
    let req: RequestEnvelope = match serde_json::from_str(input.trim()) {
        Ok(parsed) => parsed,
        Err(err) => {
            emit_event(&json!({
                "event": "error",
                "code": "invalid_request",
                "message": format!("無効な入力 JSON: {err}"),
            }));
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = run_request(&req.params) {
        emit_event(&json!({ "event": "error", "code": "execution_failed", "message": err }));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_agy_command_args_includes_stream_json_and_schema() {
        let args = build_agy_command_args(r#"{"title":"EditCommand"}"#, Some("gemini-2.5-pro"));
        assert_eq!(args[0], "--print");
        assert!(
            args.windows(2)
                .any(|w| w == ["--input-format", "stream-json"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--json-schema", r#"{"title":"EditCommand"}"#])
        );
        assert!(args.windows(2).any(|w| w == ["--model", "gemini-2.5-pro"]));
    }

    #[test]
    fn parse_agy_stream_json_line_handles_delta_and_result() -> Result<(), String> {
        let (d1, r1) = parse_agy_stream_json_line(
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"{\"type\":"}}"#,
        )?;
        assert_eq!(d1.as_deref(), Some("{\"type\":"));
        assert!(r1.is_none());

        let (d2, r2) =
            parse_agy_stream_json_line(r#"{"type":"result","result":"{\"type\":\"undo\"}"}"#)?;
        assert!(d2.is_none());
        assert_eq!(r2.as_deref(), Some("{\"type\":\"undo\"}"));
        Ok(())
    }

    #[test]
    fn prepare_schema_arg_spills_large_schema_to_temp_file_and_cleans_up() -> Result<(), String> {
        let large = "x".repeat(INLINE_SCHEMA_LIMIT_BYTES + 256);
        let (arg, guard) = prepare_schema_arg(&large)?;
        let path = PathBuf::from(&arg);
        assert!(path.is_file());
        assert_eq!(fs::read_to_string(&path).map_err(|e| e.to_string())?, large);
        drop(guard);
        assert!(!path.exists());
        Ok(())
    }
}
