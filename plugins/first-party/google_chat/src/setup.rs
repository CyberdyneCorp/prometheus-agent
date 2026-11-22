//! `/setup-files start|<redirect-url>|revoke` (and bare `/setup-files`
//! for the status): each user authorizes the app once to post files as
//! them (scope `chat.messages.create`, the OAuth client of
//! `options.oauth_client_id` and the secret
//! `GOOGLE_CHAT_OAUTH_CLIENT_SECRET`). `start` answers with the consent
//! link; the browser then fails to load the redirect, whose URL (or just
//! its code) the user pastes back; the refresh token it trades for goes to
//! the secrets store (`GOOGLE_CHAT_USER_TOKENS`, by email). `revoke`
//! revokes it at Google and removes it. The `state` parameter binds a
//! pasted URL to the user who started.

use adapter_kit::{host, net, sign, text};
use serde_json::json;

use crate::GoogleChat;
use crate::auth::{CLIENT_SECRET, USER_SCOPE, USER_TOKENS};

/// The `state` of `email`'s consent link.
fn state_of(secret: &str, email: &str) -> String {
    sign::hex(&sign::hmac_sha256(secret.as_bytes(), email.as_bytes()))[..24].to_owned()
}

/// The code (and state) of a pasted redirect URL, or a bare code.
fn code_of(pasted: &str) -> (String, Option<String>) {
    let query = adapter_kit::query(pasted);
    let field = |k: &str| {
        query
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    match field("code") {
        Some(code) => (code, field("state")),
        None => (pasted.trim().to_owned(), None),
    }
}

impl GoogleChat {
    pub(crate) fn setup_files(
        &mut self,
        space: &str,
        thread: Option<&str>,
        email: &str,
        arg: &str,
    ) {
        let reply = if email.is_empty() {
            "I cannot tell who you are, so I cannot keep a token for you.".to_owned()
        } else {
            match arg {
                "" => self.files_status(email),
                "start" => self.consent_link(email),
                "revoke" => self.revoke(email),
                pasted => self.exchange(email, pasted),
            }
        };
        if let Err(why) = self.create(space, thread, &json!({"text": reply})) {
            host::warn(&format!("could not answer /setup-files: {why}"));
        }
    }

    fn client(&self) -> Option<(String, String)> {
        let id = self.settings.oauth_client_id.clone()?;
        let secret = host::secret(CLIENT_SECRET)?;
        Some((id, secret))
    }

    fn files_status(&self, email: &str) -> String {
        if self.auth.tokens()[email].is_string() {
            format!("Native file delivery is on for {email}.")
        } else if self.client().is_none() {
            "Native file delivery is not configured on this agent (it needs an OAuth client)."
                .to_owned()
        } else {
            "Native file delivery is off for you: send /setup-files start.".to_owned()
        }
    }

    fn consent_link(&self, email: &str) -> String {
        let Some((id, secret)) = self.client() else {
            return "Native file delivery is not configured on this agent (it needs an OAuth client).".to_owned();
        };
        let query = text::form_encode(&[
            ("client_id", &id),
            ("redirect_uri", &self.settings.redirect_uri),
            ("response_type", "code"),
            ("scope", USER_SCOPE),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("login_hint", email),
            ("state", &state_of(&secret, email)),
        ]);
        format!(
            "Open {}?{query} and allow access. Your browser then fails to load a localhost page: send me its address as /setup-files <address>.",
            self.auth.urls.authorize
        )
    }

    fn exchange(&mut self, email: &str, pasted: &str) -> String {
        let Some((id, secret)) = self.client() else {
            return "Native file delivery is not configured on this agent (it needs an OAuth client).".to_owned();
        };
        let (code, state) = code_of(pasted);
        if state.is_some_and(|s| !sign::same(s.as_bytes(), state_of(&secret, email).as_bytes())) {
            return "That link was started by someone else: send /setup-files start yourself."
                .to_owned();
        }
        let token_url = self.auth.urls.token.clone();
        let response = net::send(
            "POST",
            &token_url,
            &[("content-type", "application/x-www-form-urlencoded")],
            Some(
                text::form_encode(&[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("client_id", &id),
                    ("client_secret", &secret),
                    ("redirect_uri", &self.settings.redirect_uri),
                ])
                .into_bytes(),
            ),
        );
        let refresh = response
            .ok()
            .filter(|r| r.status < 300)
            .and_then(|r| r.json()["refresh_token"].as_str().map(str::to_owned));
        let Some(refresh) = refresh else {
            return "Google did not accept that code: send /setup-files start and try again."
                .to_owned();
        };
        let tokens = self.auth.set_token(email, Some(&refresh));
        self.store_tokens(&tokens);
        "Done: files I send you now arrive as native attachments.".to_owned()
    }

    fn revoke(&mut self, email: &str) -> String {
        let Some(refresh) = self.auth.tokens()[email].as_str().map(str::to_owned) else {
            return "You have no file-delivery authorization to revoke.".to_owned();
        };
        let url = format!(
            "{}?{}",
            self.auth.urls.revoke,
            text::form_encode(&[("token", &refresh)])
        );
        if let Err(why) = net::send("POST", &url, &[], None) {
            host::warn(&format!("Google did not take the revocation: {why}"));
        }
        let tokens = self.auth.set_token(email, None);
        self.store_tokens(&tokens);
        "Revoked: I will send you file notices instead of attachments.".to_owned()
    }

    fn store_tokens(&mut self, tokens: &serde_json::Value) {
        self.writes
            .push(json!({"store_secret": {"name": USER_TOKENS, "value": tokens.to_string()}}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_redirect_yields_its_code_and_state() {
        let (code, state) = code_of("http://localhost:8085/?state=abc&code=4%2F0AX&scope=x");
        assert_eq!((code.as_str(), state.as_deref()), ("4/0AX", Some("abc")));
        assert_eq!(code_of("4/0AX").0, "4/0AX");
    }
}
