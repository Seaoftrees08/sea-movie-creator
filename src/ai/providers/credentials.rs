//! API キー等の認証情報を環境変数または OS 資格情報ストアから安全に解決するモジュール。
//!
//! いかなる API キーもプロジェクト JSON ファイルへは保存しない。

use crate::ai::provider::{AiError, AiProviderKind, SecretString};

/// OS 資格情報ストア上で本アプリが使用するサービス名。
pub const KEYRING_SERVICE_NAME: &str = "sea-movie-creator";

/// 指定プロバイダの認証キーを「明示指定 -> 環境変数 -> OS 資格情報ストア」の順で解決する。
///
/// 見つからない場合は [`None`] を返す（OS keyring 非稼働のヘッドレス CI でもパニックしない）。
#[must_use]
pub fn resolve_api_key(
    kind: &AiProviderKind,
    runtime_override: Option<&SecretString>,
) -> Option<SecretString> {
    if let Some(secret) = runtime_override
        && !secret.is_empty()
    {
        return Some(secret.clone());
    }

    if let Some(env_var) = kind.default_env_var()
        && let Ok(val) = std::env::var(env_var)
        && !val.trim().is_empty()
    {
        return Some(SecretString::new(val));
    }

    if let Some(account) = kind.keyring_account()
        && let Some(stored) = read_from_os_keyring(account)
    {
        return Some(stored);
    }

    None
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
}
