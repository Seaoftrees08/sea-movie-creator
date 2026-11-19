//! VS Code スタイルの AI セッション永続化（作成・一覧・再開・改名・削除）およびエージェント設定保存。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sea_agent_protocol::{AgentAskChoice, AgentAskField};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::ai::provider::{
    AgentRuntimeSettings, AiApprovalMode, AiProviderKind, AiRole, AiThinkingLevel,
};
use crate::core::user_config_file_path;

static SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

/// セッション保存・読込・設定永続化におけるエラー型。
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// セッション ID が空またはファイル名として不正（パストラバーサル等）。
    #[error("invalid session id `{0}`")]
    InvalidSessionId(String),
    /// 対象セッションが見つからない。
    #[error("session `{0}` not found")]
    NotFound(String),
    /// ファイルシステムの読み書き失敗。
    #[error("session store I/O failed at {path}: {reason}")]
    Io {
        /// 失敗対象パス。
        path: String,
        /// 原因詳細。
        reason: String,
    },
    /// JSON のシリアライズ／デシリアライズ失敗。
    #[error("session JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// 永続化されるチャットメッセージ 1 件分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredChatTurn {
    /// 発言ロール。
    pub role: AiRole,
    /// メッセージ本文。
    pub text: String,
    /// UNIX エポック秒。
    #[serde(default)]
    pub timestamp_unix_secs: u64,
}

/// セッション再開時にも再表示できる保留中 `ask_user` 質問カードのスナップショット。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAgentQuestionSnapshot {
    /// リクエスト ID。
    pub request_id: String,
    /// カード見出し。
    pub title: String,
    /// 質問本文。
    pub message: String,
    /// 入力欄一覧。
    #[serde(default)]
    pub fields: Vec<AgentAskField>,
    /// クイック選択ボタン一覧。
    #[serde(default)]
    pub choices: Vec<AgentAskChoice>,
    /// スキップボタンを許可するか。
    #[serde(default)]
    pub allow_skip: bool,
}

/// 1 つのチャット／エージェントセッションの完全な永続状態。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiSessionRecord {
    /// フォーマット世代番号。
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// 一意なセッション ID（英数字・`_`・`-` のみ）。
    pub session_id: String,
    /// セッション一覧に表示する見出し。
    pub title: String,
    /// 作成 UNIX 秒。
    #[serde(default)]
    pub created_at_unix_secs: u64,
    /// 最終更新 UNIX 秒。
    #[serde(default)]
    pub updated_at_unix_secs: u64,
    /// 選択されていたバックエンド AI プロバイダ（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AiProviderKind>,
    /// 選択されていた自律エージェントプラグイン ID（単発チャット時は `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_plugin_id: Option<String>,
    /// 使用していたモデル名。
    #[serde(default)]
    pub model_name: String,
    /// 思考レベル設定。
    #[serde(default)]
    pub thinking_level: AiThinkingLevel,
    /// 承認モード（`Manual` / `Autopilot`）。
    #[serde(default)]
    pub approval_mode: AiApprovalMode,
    /// 添付されていたメディアフォルダパス一覧。
    #[serde(default)]
    pub attached_folders: Vec<String>,
    /// 会話履歴一覧。
    #[serde(default)]
    pub turns: Vec<StoredChatTurn>,
    /// 回答待ち中のリッチ対話質問（存在する場合）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_question: Option<PendingAgentQuestionSnapshot>,
    /// エージェントプラグインが保持・更新する構造化メモリ（解析済み画像要約や最新設計等）。
    #[serde(default)]
    pub agent_memory: Value,
}

/// セッション一覧 UI 向けの軽量メタデータ要約。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiSessionSummary {
    /// セッション ID。
    pub session_id: String,
    /// セッションタイトル。
    pub title: String,
    /// 作成 UNIX 秒。
    pub created_at_unix_secs: u64,
    /// 最終更新 UNIX 秒。
    pub updated_at_unix_secs: u64,
    /// 記録されているターン数。
    pub turn_count: usize,
    /// 添付フォルダ数。
    pub folder_count: usize,
    /// 紐付くエージェントプラグイン ID（単発モード時は `None`）。
    pub agent_plugin_id: Option<String>,
}

/// セッション JSON とランタイム設定をローカルディスクへ保存・読込するストア。
#[derive(Debug, Clone)]
pub struct AiSessionStore {
    sessions_dir: PathBuf,
    settings_path: PathBuf,
}

impl AiSessionStore {
    /// 既定のユーザー設定配下（`<user_config>/ai_sessions/` と `agent_settings.json`）を使うストアを返す。
    #[must_use]
    pub fn default_user_store() -> Option<Self> {
        let sessions_dir = user_config_file_path("ai_sessions")?;
        let settings_path = user_config_file_path("agent_settings.json")?;
        Some(Self {
            sessions_dir,
            settings_path,
        })
    }

    /// 指定ディレクトリ配下にセッションを保存するストアを生成する（テストや任意保存先向け）。
    #[must_use]
    pub fn new(sessions_dir: impl Into<PathBuf>) -> Self {
        let sessions_dir = sessions_dir.into();
        let settings_path = sessions_dir
            .parent()
            .unwrap_or(&sessions_dir)
            .join("agent_settings.json");
        Self {
            sessions_dir,
            settings_path,
        }
    }

    /// セッションディレクトリと設定ファイルパスを明示指定して生成する。
    #[must_use]
    pub fn with_paths(sessions_dir: impl Into<PathBuf>, settings_path: impl Into<PathBuf>) -> Self {
        Self {
            sessions_dir: sessions_dir.into(),
            settings_path: settings_path.into(),
        }
    }

    /// セッション JSON 保存ディレクトリを返す。
    #[must_use]
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// 新規一意セッション ID を採番する（ファイル名安全な英数字・`_`・`-` のみ）。
    #[must_use]
    pub fn generate_session_id() -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        format!("sess_{millis}_{seq}")
    }

    /// 現在時刻の UNIX エポック秒を返す。
    #[must_use]
    pub fn now_unix_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// 最初のユーザー入力からセッション見出し用の短いタイトルを生成する。
    #[must_use]
    pub fn derive_title_from_prompt(prompt: &str) -> String {
        let first_line = prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("新しいセッション");
        let max_chars = 36;
        let mut chars = first_line.chars();
        let prefix: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_some() {
            format!("{prefix}...")
        } else {
            prefix
        }
    }

    /// セッションを保存（新規作成または上書き更新）する。
    ///
    /// # Errors
    ///
    /// ID 不正、JSON シリアライズ失敗、またはディスク書込失敗時に [`SessionStoreError`] を返す。
    pub fn save_session(&self, record: &AiSessionRecord) -> Result<(), SessionStoreError> {
        let path = self.session_file_path(&record.session_id)?;
        let json = serde_json::to_string_pretty(record)?;
        atomic_write_text(&path, &json)
    }

    /// 指定 ID のセッションを読み込む。
    ///
    /// # Errors
    ///
    /// ID 不正、未存在、ファイル読取失敗、または JSON パース失敗時に [`SessionStoreError`] を返す。
    pub fn load_session(&self, session_id: &str) -> Result<AiSessionRecord, SessionStoreError> {
        let path = self.session_file_path(session_id)?;
        let raw = read_regular_session_file(&path, session_id)?;
        let rec: AiSessionRecord = serde_json::from_str(&raw)?;
        validate_session_id(&rec.session_id)?;
        if rec.session_id != session_id {
            return Err(SessionStoreError::InvalidSessionId(rec.session_id));
        }
        Ok(rec)
    }

    /// 保存済みセッション一覧を更新日時の新しい順で返す。破損ファイルはスキップし他の読込を妨げない。
    ///
    /// # Errors
    ///
    /// 保存ディレクトリの走査自体に失敗した場合に [`SessionStoreError`] を返す。
    pub fn list_sessions(&self) -> Result<Vec<AiSessionSummary>, SessionStoreError> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let entries = fs::read_dir(&self.sessions_dir).map_err(|err| SessionStoreError::Io {
            path: self.sessions_dir.display().to_string(),
            reason: err.to_string(),
        })?;
        let mut list = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if validate_session_id(stem).is_err() {
                continue;
            }
            let Ok(raw) = read_regular_session_file(&path, stem) else {
                continue;
            };
            let Ok(header) = serde_json::from_str::<AiSessionSummaryHeader>(&raw) else {
                continue;
            };
            if header.session_id != stem || validate_session_id(&header.session_id).is_err() {
                continue;
            }
            list.push(AiSessionSummary {
                session_id: header.session_id,
                title: header.title,
                created_at_unix_secs: header.created_at_unix_secs,
                updated_at_unix_secs: header.updated_at_unix_secs,
                turn_count: header.turns.len(),
                folder_count: header.attached_folders.len(),
                agent_plugin_id: header.agent_plugin_id,
            });
        }
        list.sort_by(|a, b| {
            b.updated_at_unix_secs
                .cmp(&a.updated_at_unix_secs)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(list)
    }

    /// 既存セッションのタイトルを更新して保存する。
    ///
    /// # Errors
    ///
    /// セッション未存在・保存失敗時に [`SessionStoreError`] を返す。
    pub fn rename_session(
        &self,
        session_id: &str,
        new_title: &str,
    ) -> Result<AiSessionRecord, SessionStoreError> {
        let mut rec = self.load_session(session_id)?;
        let trimmed = new_title.trim();
        if trimmed.is_empty() {
            return Ok(rec);
        }
        trimmed.clone_into(&mut rec.title);
        rec.updated_at_unix_secs = Self::now_unix_secs();
        self.save_session(&rec)?;
        Ok(rec)
    }

    /// 指定 ID のセッションファイルを削除する。
    ///
    /// # Errors
    ///
    /// ID 不正・未存在・削除 I/O エラー時に [`SessionStoreError`] を返す。
    pub fn delete_session(&self, session_id: &str) -> Result<(), SessionStoreError> {
        let path = self.session_file_path(session_id)?;
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return Err(SessionStoreError::NotFound(session_id.to_owned()));
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(SessionStoreError::NotFound(session_id.to_owned()));
        }
        fs::remove_file(&path).map_err(|err| SessionStoreError::Io {
            path: path.display().to_string(),
            reason: err.to_string(),
        })
    }

    /// 永続化されたエージェントランタイム設定を読み込む（未作成時は既定値を返す）。
    ///
    /// # Errors
    ///
    /// ファイル存在時の読込・パース失敗時に [`SessionStoreError`] を返す。
    pub fn load_runtime_settings(&self) -> Result<AgentRuntimeSettings, SessionStoreError> {
        let Ok(meta) = fs::symlink_metadata(&self.settings_path) else {
            return Ok(AgentRuntimeSettings::default());
        };
        if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_SESSION_FILE_BYTES {
            return Ok(AgentRuntimeSettings::default());
        }
        let raw = fs::read_to_string(&self.settings_path).map_err(|err| SessionStoreError::Io {
            path: self.settings_path.display().to_string(),
            reason: err.to_string(),
        })?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// エージェントランタイム設定を永続化する。
    ///
    /// # Errors
    ///
    /// シリアライズ失敗・書込失敗時に [`SessionStoreError`] を返す。
    pub fn save_runtime_settings(
        &self,
        settings: &AgentRuntimeSettings,
    ) -> Result<(), SessionStoreError> {
        let json = serde_json::to_string_pretty(settings)?;
        atomic_write_text(&self.settings_path, &json)
    }

    fn session_file_path(&self, session_id: &str) -> Result<PathBuf, SessionStoreError> {
        validate_session_id(session_id)?;
        Ok(self.sessions_dir.join(format!("{session_id}.json")))
    }
}

const MAX_SESSION_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Deserialize)]
struct AiSessionSummaryHeader {
    session_id: String,
    title: String,
    #[serde(default)]
    created_at_unix_secs: u64,
    #[serde(default)]
    updated_at_unix_secs: u64,
    #[serde(default)]
    turns: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    attached_folders: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    agent_plugin_id: Option<String>,
}

const fn default_schema_version() -> u32 {
    1
}

fn read_regular_session_file(path: &Path, session_id: &str) -> Result<String, SessionStoreError> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Err(SessionStoreError::NotFound(session_id.to_owned()));
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(SessionStoreError::NotFound(session_id.to_owned()));
    }
    if meta.len() > MAX_SESSION_FILE_BYTES {
        return Err(SessionStoreError::Io {
            path: path.display().to_string(),
            reason: format!("session file exceeds {MAX_SESSION_FILE_BYTES} bytes limit"),
        });
    }
    fs::read_to_string(path).map_err(|err| SessionStoreError::Io {
        path: path.display().to_string(),
        reason: err.to_string(),
    })
}

fn validate_session_id(session_id: &str) -> Result<(), SessionStoreError> {
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        || is_windows_reserved_device_name(session_id)
    {
        return Err(SessionStoreError::InvalidSessionId(session_id.to_owned()));
    }
    Ok(())
}

fn is_windows_reserved_device_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn atomic_write_text(path: &Path, text: &str) -> Result<(), SessionStoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| SessionStoreError::Io {
            path: parent.display().to_string(),
            reason: err.to_string(),
        })?;
    }
    let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    let tmp_path = path.with_file_name(format!("{file_name}.tmp.{pid}.{seq}"));
    fs::write(&tmp_path, text.as_bytes()).map_err(|err| SessionStoreError::Io {
        path: tmp_path.display().to_string(),
        reason: err.to_string(),
    })?;
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(SessionStoreError::Io {
            path: path.display().to_string(),
            reason: err.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct TempDirGuard(PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn session_crud_and_runtime_settings_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let base = std::env::temp_dir().join(format!(
            "sea_session_store_test_{}_{}",
            std::process::id(),
            AiSessionStore::generate_session_id()
        ));
        let _guard = TempDirGuard(base.clone());
        let store = AiSessionStore::new(base.join("sessions"));

        // パストラバーサルを含む不正 ID は拒否
        assert!(matches!(
            store.load_session("../secret"),
            Err(SessionStoreError::InvalidSessionId(_))
        ));

        let mut rec1 = AiSessionRecord {
            schema_version: 1,
            session_id: "sess_alpha".to_owned(),
            title: AiSessionStore::derive_title_from_prompt(
                "結婚式プロフィールムービーを3分で作りたいです",
            ),
            created_at_unix_secs: 100,
            updated_at_unix_secs: 100,
            provider: Some(AiProviderKind::Gemini),
            agent_plugin_id: Some("sea.agent.director".to_owned()),
            model_name: "gemini-3.8-flash".to_owned(),
            thinking_level: AiThinkingLevel::Medium,
            approval_mode: AiApprovalMode::Manual,
            attached_folders: vec!["D:/photos".to_owned()],
            turns: vec![StoredChatTurn {
                role: AiRole::User,
                text: "お願いします".to_owned(),
                timestamp_unix_secs: 100,
            }],
            pending_question: None,
            agent_memory: json!({"analyzed_count": 12}),
        };
        store.save_session(&rec1)?;

        let mut rec2 = rec1.clone();
        rec2.session_id = "sess_beta".to_owned();
        rec2.title = "旅行 Vlog".to_owned();
        rec2.updated_at_unix_secs = 250;
        store.save_session(&rec2)?;

        let list = store.list_sessions()?;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, "sess_beta");
        assert_eq!(list[1].session_id, "sess_alpha");

        rec1 = store.rename_session("sess_alpha", "太郎・花子 結婚式")?;
        assert_eq!(rec1.title, "太郎・花子 結婚式");
        let loaded = store.load_session("sess_alpha")?;
        assert_eq!(loaded.agent_memory["analyzed_count"], 12);

        store.delete_session("sess_beta")?;
        assert_eq!(store.list_sessions()?.len(), 1);

        let settings = AgentRuntimeSettings {
            max_steps_per_turn: 35,
            min_llm_interval_ms: 400,
            vision_batch_size: 8,
            vision_max_edge_px: 512,
        };
        store.save_runtime_settings(&settings)?;
        assert_eq!(store.load_runtime_settings()?, settings);

        // 空タイトルのリネームは更新せず早期リターン、Windows 予約デバイス名は拒否される
        let before_rename = rec1.updated_at_unix_secs;
        let unchanged = store.rename_session("sess_alpha", "   ")?;
        assert_eq!(unchanged.title, "太郎・花子 結婚式");
        assert_eq!(unchanged.updated_at_unix_secs, before_rename);
        assert!(matches!(
            store.load_session("CON"),
            Err(SessionStoreError::InvalidSessionId(_))
        ));
        assert!(matches!(
            store.load_session("nul"),
            Err(SessionStoreError::InvalidSessionId(_))
        ));
        Ok(())
    }

    #[test]
    fn list_sessions_accepts_header_without_timestamps() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "sea_session_no_ts_{}_{}",
            std::process::id(),
            AiSessionStore::generate_session_id()
        ));
        let _guard = TempDirGuard(dir.clone());
        let store = AiSessionStore::new(&dir);
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("sess_legacy.json"),
            r#"{"session_id":"sess_legacy","title":"古い形式","turns":[]}"#,
        )?;
        let loaded = store.load_session("sess_legacy")?;
        assert_eq!(loaded.created_at_unix_secs, 0);
        let list = store.list_sessions()?;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "sess_legacy");
        Ok(())
    }
}
