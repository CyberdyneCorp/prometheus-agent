//! `prometheus auth import codex-cli` (spec: model-providers, OpenAI Codex
//! ChatGPT plan sign-in): copies a Codex CLI sign-in into the secrets
//! store when its `auth.json` holds both an access token and a refresh
//! token. The file is only read.

use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::Value;

use super::credentials::Obtained;
use super::jwt;
use super::token::TokenSet;

/// The provider a Codex CLI sign-in belongs to.
pub const PROVIDER: &str = "openai-codex";

/// The warning shown after an import.
pub const SHARED_FAMILY_WARNING: &str = "Prometheus and the Codex CLI now share one rotating refresh-token family: when one of them refreshes, the other's copy stops working and must sign in again. Sign in separately with: prometheus auth add openai-codex";

/// The Codex CLI sign-in file under `user_home`.
#[must_use]
pub fn default_path(user_home: &Path) -> std::path::PathBuf {
    user_home.join(".codex").join("auth.json")
}

/// Reads the sign-in at `path`.
///
/// # Errors
///
/// `auth.import_incomplete` (exit 1) when the file is missing, unreadable,
/// or lacks the access token or the refresh token.
pub fn read(path: &Path) -> Result<(TokenSet, Obtained), Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| incomplete(&format!("cannot read {}: {e}", path.display())))?;
    let json: Value = serde_json::from_str(&text).map_err(|_| {
        incomplete(&format!(
            "{} is not a Codex CLI sign-in file",
            path.display()
        ))
    })?;
    let tokens = json.get("tokens").unwrap_or(&Value::Null);
    let field = |name: &str| {
        tokens
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let (Some(access), Some(refresh)) = (field("access_token"), field("refresh_token")) else {
        return Err(incomplete(&format!(
            "{} does not hold both an access token and a refresh token",
            path.display()
        )));
    };
    let id_token = field("id_token");
    let account = jwt::account(&[id_token.as_deref().unwrap_or_default(), &access]);
    let expires_at = jwt::claims(&access)
        .and_then(|c| c.get("exp").and_then(Value::as_i64))
        .and_then(|exp| jiff::Timestamp::from_second(exp).ok());
    let obtained = Obtained {
        source: "import:codex-cli".to_owned(),
        account_id: account.account_id.or_else(|| field("account_id")),
        plan: account.plan,
    };
    let tokens = TokenSet {
        access_token: SecretString::new(access),
        refresh_token: Some(SecretString::new(refresh)),
        expires_at,
        id_token: id_token.map(SecretString::new),
    };
    Ok((tokens, obtained))
}

fn incomplete(message: &str) -> Error {
    Error::new(
        &codes::AUTH_IMPORT_INCOMPLETE,
        format!("nothing was imported: {message}"),
    )
    .with_hint("sign in with: prometheus auth add openai-codex")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn write(dir: &Path, value: &Value) -> std::path::PathBuf {
        let path = dir.join("auth.json");
        std::fs::write(&path, value.to_string()).unwrap();
        path
    }

    #[test]
    fn complete_file_yields_tokens_and_account() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let access = jwt::encode(&json!({
            "exp": 4_102_444_800_i64,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-9", "chatgpt_plan_type": "plus"}
        }));
        let path = write(
            dir.path(),
            &json!({"tokens": {"access_token": access, "refresh_token": "rt"}}),
        );
        let (tokens, obtained) = read(&path).unwrap();
        assert_eq!(tokens.refresh_token.unwrap().expose(), "rt");
        assert_eq!(obtained.account_id.as_deref(), Some("acct-9"));
        assert_eq!(obtained.plan.as_deref(), Some("plus"));
        assert!(tokens.expires_at.is_some());
    }

    #[test]
    fn missing_refresh_token_imports_nothing() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let path = write(dir.path(), &json!({"tokens": {"access_token": "at"}}));
        let err = read(&path).unwrap_err();
        assert_eq!(err.code().code, "auth.import_incomplete");
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.hint()
                .unwrap()
                .contains("prometheus auth add openai-codex")
        );
    }
}
