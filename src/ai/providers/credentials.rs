//! API キー等の認証情報を環境変数または OS 資格情報ストアから安全に解決するモジュール。
//!
//! いかなる API キーもプロジェクト JSON ファイルへは保存しない。

use crate::ai::provider::{AiError, AiProviderKind, SecretString};
use crate::ai::settings::AiUserSettings;

/// OS 資格情報ストア上で本アプリが使用するサービス名。
pub const KEYRING_SERVICE_NAME: &str = "sea-movie-creator";

/// API キーがどの経路から解決されたかを示す診断種別。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// メモリ上の一時上書きキー。
    RuntimeOverride,
    /// OS 環境変数（設定済みの環境変数名を保持）。
    EnvVar(&'static str),
    /// `<settings_dir>/ai_settings.json` の `api_keys` 欄。
    SettingsJson,
    /// OS 資格情報ストア（Keyring）。
    Keyring,
    /// どの経路からも未設定。
    Missing,
}

/// 解決された API キーと、その取得元診断情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    /// 秘匿 API キー本体。
    pub secret: SecretString,
    /// 解決元。
    pub source: CredentialSource,
}

/// 4 段階優先順位（実行時上書き -> 環境変数 -> `ai_settings.json` -> OS Keyring）で解決する純粋関数。
#[must_use]
pub fn resolve_api_key_from(
    kind: &AiProviderKind,
    runtime_override: Option<&SecretString>,
    env_value: Option<&str>,
    settings_json_value: Option<&str>,
    keyring_value: Option<&SecretString>,
) -> Option<ResolvedCredential> {
    if let Some(secret) = runtime_override
        && !secret.is_empty()
    {
        return Some(ResolvedCredential {
            secret: secret.clone(),
            source: CredentialSource::RuntimeOverride,
        });
    }

    if let Some(env_var) = kind.default_env_var()
        && let Some(val) = env_value.map(str::trim).filter(|s| !s.is_empty())
    {
        return Some(ResolvedCredential {
            secret: SecretString::new(val),
            source: CredentialSource::EnvVar(env_var),
        });
    }

    if let Some(val) = settings_json_value.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(ResolvedCredential {
            secret: SecretString::new(val),
            source: CredentialSource::SettingsJson,
        });
    }

    if let Some(stored) = keyring_value
        && !stored.is_empty()
    {
        return Some(ResolvedCredential {
            secret: stored.clone(),
            source: CredentialSource::Keyring,
        });
    }

    None
}

/// 指定設定オブジェクトを参照して認証キーと取得元 (`CredentialSource`) を解決する。
#[must_use]
pub fn resolve_api_key_with_source(
    kind: &AiProviderKind,
    runtime_override: Option<&SecretString>,
    settings: Option<&AiUserSettings>,
) -> Option<ResolvedCredential> {
    let env_val = kind
        .default_env_var()
        .and_then(|var| std::env::var(var).ok());
    let json_val = settings.and_then(|s| s.api_key_for(kind));
    let keyring_val = kind.keyring_account().and_then(read_from_os_keyring);
    resolve_api_key_from(
        kind,
        runtime_override,
        env_val.as_deref(),
        json_val,
        keyring_val.as_ref(),
    )
}

/// 現在の有効な認証情報取得元のみを診断して返す。
#[must_use]
pub fn diagnose_credential_source(
    kind: &AiProviderKind,
    runtime_override: Option<&SecretString>,
    settings: Option<&AiUserSettings>,
) -> CredentialSource {
    resolve_api_key_with_source(kind, runtime_override, settings)
        .map_or(CredentialSource::Missing, |r| r.source)
}

/// 明示的な [`AiUserSettings`] 参照を用いて API キー本体のみを解決する。
#[must_use]
pub fn resolve_api_key_with_settings(
    kind: &AiProviderKind,
    runtime_override: Option<&SecretString>,
    settings: Option<&AiUserSettings>,
) -> Option<SecretString> {
    resolve_api_key_with_source(kind, runtime_override, settings).map(|r| r.secret)
}

/// 指定プロバイダの認証キーを「一時上書き -> 環境変数 -> `ai_settings.json` -> OS 資格情報ストア」の順で解決する。
///
/// 見つからない場合は [`None`] を返す（OS keyring 非稼働のヘッドレス CI でもパニックしない）。
#[must_use]
pub fn resolve_api_key(
    kind: &AiProviderKind,
    runtime_override: Option<&SecretString>,
) -> Option<SecretString> {
    let settings = AiUserSettings::load_default_if_exists();
    resolve_api_key_with_settings(kind, runtime_override, settings.as_ref())
}

/// 必須プロバイダ向けに API キーを解決し、不在時は分かりやすい [`AiError::MissingCredentials`] を返す。
///
/// # Errors
///
/// `kind.requires_api_key()` が `true` でかつ取得元すべてが未設定の場合。
pub fn require_api_key(
    kind: &AiProviderKind,
    runtime_override: Option<&SecretString>,
) -> Result<Option<SecretString>, AiError> {
    if let Some(key) = resolve_api_key(kind, runtime_override) {
        return Ok(Some(key));
    }
    if kind.requires_api_key() {
        return Err(AiError::MissingCredentials {
            provider: kind.display_label(),
            env_hint: kind.default_env_var().unwrap_or("API_KEY").to_owned(),
        });
    }
    Ok(None)
}

/// OS 資格情報ストア（Windows Credential Manager / macOS Keychain / Linux Secret Service）へ保存する。
///
/// # Errors
///
/// 空キー指定または OS 資格情報ストアが利用不能な場合。
pub fn save_secret_to_keyring(account: &str, secret: &SecretString) -> Result<(), AiError> {
    if secret.is_empty() {
        return Err(AiError::PluginError(
            "空の API キーは OS 資格情報ストアに保存できません".to_owned(),
        ));
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE_NAME, account)
        .map_err(|err| AiError::PluginError(format!("OS keyring entry init failed: {err}")))?;
    entry
        .set_password(secret.expose())
        .map_err(|err| AiError::PluginError(format!("OS keyring store failed: {err}")))?;
    Ok(())
}

/// OS 資格情報ストアから指定アカウントの秘密値を削除する（未登録時も正常扱い）。
///
/// # Errors
///
/// OS 資格情報ストアへのアクセス自体に失敗した場合。
pub fn delete_secret_from_keyring(account: &str) -> Result<(), AiError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE_NAME, account)
        .map_err(|err| AiError::PluginError(format!("OS keyring entry init failed: {err}")))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(AiError::PluginError(format!(
            "OS keyring delete failed: {err}"
        ))),
    }
}

fn read_from_os_keyring(account: &str) -> Option<SecretString> {
    let entry = keyring::Entry::new(KEYRING_SERVICE_NAME, account).ok()?;
    let password = entry.get_password().ok()?;
    let trimmed = password.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(SecretString::new(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_override_takes_precedence_over_env_and_missing_key_errors_cleanly() {
        let explicit = SecretString::new("override-token-xyz");
        let resolved = resolve_api_key(&AiProviderKind::OpenAi, Some(&explicit));
        assert_eq!(
            resolved.as_ref().map(SecretString::expose),
            Some("override-token-xyz")
        );

        let local = require_api_key(&AiProviderKind::OllamaCompat, None);
        assert!(local.is_ok());

        // 未設定確認用: Mock は常に認証不要
        assert!(!AiProviderKind::Mock.requires_api_key());
        assert!(AiProviderKind::OpenRouter.requires_api_key());
    }

    #[test]
    fn credential_resolution_follows_four_tier_priority_and_reports_source()
    -> Result<(), Box<dyn std::error::Error>> {
        let kind = AiProviderKind::OpenRouter;
        let runtime = SecretString::new("rt-key");
        let keyring = SecretString::new("kr-key");

        // 1. RuntimeOverride > EnvVar > SettingsJson > Keyring
        let r1 = resolve_api_key_from(
            &kind,
            Some(&runtime),
            Some("env-key"),
            Some("json-key"),
            Some(&keyring),
        )
        .ok_or("expected runtime credential")?;
        assert_eq!(r1.source, CredentialSource::RuntimeOverride);
        assert_eq!(r1.secret.expose(), "rt-key");

        // 2. EnvVar > SettingsJson > Keyring
        let r2 = resolve_api_key_from(
            &kind,
            None,
            Some("env-key"),
            Some("json-key"),
            Some(&keyring),
        )
        .ok_or("expected env credential")?;
        assert_eq!(r2.source, CredentialSource::EnvVar("OPENROUTER_API_KEY"));
        assert_eq!(r2.secret.expose(), "env-key");

        // 3. SettingsJson > Keyring
        let r3 = resolve_api_key_from(&kind, None, Some("  "), Some("json-key"), Some(&keyring))
            .ok_or("expected json credential")?;
        assert_eq!(r3.source, CredentialSource::SettingsJson);
        assert_eq!(r3.secret.expose(), "json-key");

        // 4. Keyring fallback
        let r4 = resolve_api_key_from(&kind, None, None, None, Some(&keyring))
            .ok_or("expected keyring credential")?;
        assert_eq!(r4.source, CredentialSource::Keyring);
        assert_eq!(r4.secret.expose(), "kr-key");

        // 5. All missing
        assert!(resolve_api_key_from(&kind, None, None, None, None).is_none());
        Ok(())
    }
}
