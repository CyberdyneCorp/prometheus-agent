//! Reading claims of a JWT issued to us (no signature check: the token
//! came straight from the token endpoint over TLS, and the claims only
//! label the credential and pick the ChatGPT account header).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// The OpenAI claim namespace holding ChatGPT account details.
const OPENAI_AUTH: &str = "https://api.openai.com/auth";

/// The payload claims of `token`, `None` when it is not a JWT.
#[must_use]
pub fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// What a sign-in's tokens say about the account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    /// The ChatGPT account id (`ChatGPT-Account-ID` header).
    pub account_id: Option<String>,
    /// The plan label (`plus`, `pro`...).
    pub plan: Option<String>,
}

/// The account details in the first of `tokens` that carries them.
#[must_use]
pub fn account(tokens: &[&str]) -> Account {
    for token in tokens {
        let Some(auth) = claims(token).and_then(|c| c.get(OPENAI_AUTH).cloned()) else {
            continue;
        };
        let text = |name: &str| auth.get(name).and_then(Value::as_str).map(str::to_owned);
        return Account {
            account_id: text("chatgpt_account_id"),
            plan: text("chatgpt_plan_type"),
        };
    }
    Account::default()
}

#[cfg(test)]
pub(crate) fn encode(claims: &Value) -> String {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!(
        "{}.{}.sig",
        part(&serde_json::json!({"alg": "none"})),
        part(claims)
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn chatgpt_account_and_plan_come_from_the_auth_claim() {
        let token = encode(&json!({
            "sub": "user-1",
            OPENAI_AUTH: {"chatgpt_account_id": "acct-42", "chatgpt_plan_type": "pro"}
        }));
        let found = account(&["not-a-jwt", &token]);
        assert_eq!(found.account_id.as_deref(), Some("acct-42"));
        assert_eq!(found.plan.as_deref(), Some("pro"));
        assert_eq!(account(&["opaque"]), Account::default());
    }
}
