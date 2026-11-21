//! HTTP through the host (under the plugin's network grant) and the
//! failure prefixes the host classifies a `send` error by (WIT `send`):
//! `rate_limited[: <secs>]`, `server: ..`, `permanent: ..`; anything else
//! is a network failure, retried.

use serde_json::Value;

use crate::bindings::prometheus::host::http;

/// An answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// Header pairs.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// The body as JSON (`null` when it is not JSON).
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The body as text.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// A header (names compared without case).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends a request; a transport failure is a network error (retried).
///
/// # Errors
///
/// The host refused or could not complete the request.
pub fn send(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Result<Response, String> {
    let request = http::Request {
        method: method.to_owned(),
        url: url.to_owned(),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        body,
    };
    let answer = http::send(&request).map_err(|e| format!("network: {e:?}"))?;
    Ok(Response {
        status: answer.status,
        headers: answer.headers,
        body: answer.body,
    })
}

/// Sends JSON with `headers` and classifies the answer ([`classify`]).
///
/// # Errors
///
/// As [`classify`].
pub fn json(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> Result<Value, String> {
    let mut all = vec![("content-type", "application/json")];
    all.extend_from_slice(headers);
    let response = send(method, url, &all, Some(body.to_string().into_bytes()))?;
    classify(&response)?;
    Ok(response.json())
}

/// A request without a body, classified.
///
/// # Errors
///
/// As [`classify`].
pub fn get(url: &str, headers: &[(&str, &str)]) -> Result<Value, String> {
    let response = send("GET", url, headers, None)?;
    classify(&response)?;
    Ok(response.json())
}

/// The failure an answer means, as the host classifies a `send` error:
/// `429` rate limited (with the wait from a `retry_after` body field or
/// the `Retry-After` header), `5xx` server, other `4xx` permanent.
///
/// # Errors
///
/// The classified failure.
pub fn classify(response: &Response) -> Result<(), String> {
    match response.status {
        429 => {
            let body = response.json();
            let wait = body["retry_after"]
                .as_f64()
                .or_else(|| body["parameters"]["retry_after"].as_f64())
                .or_else(|| response.header("retry-after").and_then(|v| v.parse().ok()));
            Err(match wait {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                Some(secs) => format!("rate_limited: {}", secs.ceil() as u64),
                None => "rate_limited".to_owned(),
            })
        }
        500.. => Err(format!("server: status {}", response.status)),
        400.. => Err(format!(
            "permanent: status {}: {}",
            response.status,
            short(&response.text())
        )),
        _ => Ok(()),
    }
}

fn short(text: &str) -> String {
    text.chars().take(200).collect()
}
