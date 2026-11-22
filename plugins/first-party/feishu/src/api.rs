//! The Feishu Open API with the app's tenant access token: requests,
//! token caching (renewed 5 minutes before it expires, and at once when
//! Feishu calls it invalid) and the classification of refusals into the
//! host's failure prefixes. Feishu answers most refusals with HTTP 200
//! and a non-zero `code`.

use adapter_kit::{clock, host, net};
use serde_json::{Value, json};

/// The app secret.
pub const SECRET: &str = "FEISHU_APP_SECRET";

/// Codes meaning the tenant token is no longer valid.
const TOKEN_INVALID: &[i64] = &[99_991_661, 99_991_663, 99_991_664, 99_991_668];
/// Codes meaning "slow down".
const THROTTLED: &[i64] = &[99_991_400, 11_232, 11_233, 11_247];

/// The Open API origin of a `domain` option.
#[must_use]
pub fn origin(domain: &str) -> &'static str {
    if domain == "lark" {
        "https://open.larksuite.com"
    } else {
        "https://open.feishu.cn"
    }
}

/// A Feishu refusal: its `code` and message, classified.
#[must_use]
pub fn refusal(status: u16, body: &Value) -> Option<String> {
    let code = body["code"]
        .as_i64()
        .unwrap_or(if status < 300 { 0 } else { -1 });
    if status < 300 && code == 0 {
        return None;
    }
    let why = format!(
        "Feishu refused ({code}): {}",
        body["msg"].as_str().unwrap_or("no message")
    );
    Some(if status == 429 || THROTTLED.contains(&code) {
        "rate_limited".to_owned()
    } else if status >= 500 {
        format!("server: {why}")
    } else {
        format!("permanent: {why}")
    })
}

/// The `code` of a classified refusal, when it named one.
#[must_use]
pub fn code_of(error: &str) -> Option<i64> {
    let start = error.find("refused (")? + "refused (".len();
    let end = error[start..].find(')')? + start;
    error[start..end].parse().ok()
}

/// The app's API client.
pub struct Api {
    /// The origin (`https://open.feishu.cn`, or a test server).
    pub base: String,
    /// The app id.
    pub app_id: String,
    token: Option<(String, u64)>,
}

impl Api {
    /// A client of app `app_id` at `base`.
    #[must_use]
    pub fn new(base: &str, app_id: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            app_id: app_id.to_owned(),
            token: None,
        }
    }

    /// The app secret.
    ///
    /// # Errors
    ///
    /// `permanent:` when it is not set.
    pub fn secret() -> Result<String, String> {
        host::secret(SECRET).ok_or_else(|| format!("permanent: secret {SECRET} is not set"))
    }

    fn token(&mut self) -> Result<String, String> {
        if let Some((token, until)) = &self.token
            && clock::now_ms() < *until
        {
            return Ok(token.clone());
        }
        let body = json!({"app_id": self.app_id, "app_secret": Self::secret()?});
        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.base
        );
        let response = net::send(
            "POST",
            &url,
            &[("content-type", "application/json; charset=utf-8")],
            Some(body.to_string().into_bytes()),
        )?;
        let answer = response.json();
        if let Some(why) = refusal(response.status, &answer) {
            return Err(why);
        }
        let token = answer["tenant_access_token"]
            .as_str()
            .ok_or("permanent: Feishu sent no tenant_access_token")?
            .to_owned();
        let secs = answer["expire"]
            .as_u64()
            .unwrap_or(7200)
            .saturating_sub(300);
        self.token = Some((token.clone(), clock::now_ms() + secs * 1000));
        Ok(token)
    }

    /// Sends a request with the tenant token: the raw answer (a retry with
    /// a fresh token when Feishu calls the token invalid).
    ///
    /// # Errors
    ///
    /// The token cannot be had; a transport failure.
    pub fn raw(
        &mut self,
        method: &str,
        path: &str,
        content_type: &str,
        body: Option<Vec<u8>>,
    ) -> Result<net::Response, String> {
        for attempt in 0..2 {
            let auth = format!("Bearer {}", self.token()?);
            let response = net::send(
                method,
                &format!("{}{path}", self.base),
                &[("authorization", &auth), ("content-type", content_type)],
                body.clone(),
            )?;
            let code = response.json()["code"].as_i64().unwrap_or(0);
            if attempt == 0 && TOKEN_INVALID.contains(&code) {
                self.token = None;
                continue;
            }
            return Ok(response);
        }
        Err("server: Feishu kept refusing a fresh tenant token".to_owned())
    }

    /// A JSON request: the answer's `data`.
    ///
    /// # Errors
    ///
    /// A refusal, classified ([`refusal`]).
    pub fn call(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let bytes = body.map(|b| b.to_string().into_bytes());
        let response = self.raw(method, path, "application/json; charset=utf-8", bytes)?;
        let answer = response.json();
        match refusal(response.status, &answer) {
            Some(why) => Err(why),
            None => Ok(answer["data"].clone()),
        }
    }

    /// A file: its bytes.
    ///
    /// # Errors
    ///
    /// A refusal, classified.
    pub fn download(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let response = self.raw("GET", path, "application/json", None)?;
        if response.status >= 300 {
            return Err(refusal(response.status, &response.json())
                .unwrap_or_else(|| format!("permanent: status {}", response.status)));
        }
        Ok(response.body)
    }

    /// The bot's open id and name (`GET /open-apis/bot/v3/info`, whose
    /// answer carries `bot` beside `code`, not under `data`).
    ///
    /// # Errors
    ///
    /// A refusal, classified.
    pub fn bot(&mut self) -> Result<(String, String), String> {
        let response = self.raw("GET", "/open-apis/bot/v3/info", "application/json", None)?;
        let answer = response.json();
        if let Some(why) = refusal(response.status, &answer) {
            return Err(why);
        }
        let bot = if answer["bot"].is_object() {
            &answer["bot"]
        } else {
            &answer["data"]["bot"]
        };
        Ok((
            bot["open_id"].as_str().unwrap_or_default().to_owned(),
            bot["app_name"]
                .as_str()
                .or(bot["bot_name"].as_str())
                .unwrap_or_default()
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_classified_by_status_and_code() {
        assert_eq!(refusal(200, &json!({"code": 0})), None);
        assert_eq!(
            refusal(200, &json!({"code": 99_991_400})).as_deref(),
            Some("rate_limited")
        );
        let why = refusal(400, &json!({"code": 230_001, "msg": "bad content"})).unwrap();
        assert!(why.starts_with("permanent:"), "{why}");
        assert_eq!(code_of(&why), Some(230_001));
        assert!(refusal(502, &Value::Null).unwrap().starts_with("server:"));
        assert_eq!(origin("lark"), "https://open.larksuite.com");
    }
}
