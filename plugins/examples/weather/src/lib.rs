//! The plugin authoring guide's example (`docs/guide/plugin-authoring.md`):
//! a `tool` world plugin with one tool, `forecast`, that asks wttr.in for
//! a city's current weather.
//!
//! It shows the host interfaces a tool usually needs: `http` (only to the
//! hosts its `network` capability names), `config` (its own settings,
//! `plugins.entries.weather.*`), `state` (data that survives restarts,
//! under the profile's `plugin-data/weather/`) and `log`.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::tool::plugin::{Guest, ToolSpec};
use bindings::prometheus::host::log::{Level, log};
use bindings::prometheus::host::types::HostError;
use bindings::prometheus::host::{config, http, state};

struct Weather;

const PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "city": {"type": "string", "description": "City name; defaults to the plugin's `city` setting."}
  }
}"#;

impl Guest for Weather {
    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "forecast".to_owned(),
            description: "Current weather for a city.".to_owned(),
            parameters: PARAMETERS.to_owned(),
        }]
    }

    fn call(name: String, arguments: String) -> Result<String, String> {
        if name != "forecast" {
            return Err(format!("unknown tool {name}"));
        }
        let city = string_field(&arguments, "city")
            .or_else(default_city)
            .ok_or("no city given and no plugins.entries.weather.city setting")?;
        forecast(&city)
    }
}

/// Asks wttr.in; when it cannot be reached, answers the last forecast
/// fetched for the city, saying so.
fn forecast(city: &str) -> Result<String, String> {
    let request = http::Request {
        method: "GET".to_owned(),
        url: format!("https://wttr.in/{}?format=3", city.replace(' ', "+")),
        headers: vec![("accept".to_owned(), "text/plain".to_owned())],
        body: None,
    };
    let key = format!("last:{city}");
    match http::send(&request) {
        Ok(response) if response.status == 200 => {
            let text = String::from_utf8_lossy(&response.body).trim().to_owned();
            // Best effort: a full disk only loses the fallback.
            let _ = state::set(&key, text.as_bytes());
            Ok(text)
        }
        Ok(response) => fallback(&key, &format!("wttr.in answered {}", response.status)),
        Err(err) => fallback(&key, &describe(err)),
    }
}

fn fallback(key: &str, why: &str) -> Result<String, String> {
    log(Level::Warn, why);
    match state::get(key) {
        Ok(Some(last)) => Ok(format!(
            "{} (cached; {why})",
            String::from_utf8_lossy(&last)
        )),
        _ => Err(why.to_owned()),
    }
}

fn describe(err: HostError) -> String {
    match err {
        HostError::Denied(m) | HostError::Invalid(m) | HostError::Failed(m) => m,
    }
}

/// `plugins.entries.weather.city`, a JSON string.
fn default_city() -> Option<String> {
    let value = config::get("city").ok().flatten()?;
    let city = value.trim().trim_matches('"').to_owned();
    (!city.is_empty()).then_some(city)
}

/// The string value of `"<key>": "<text>"` in a flat JSON object (enough
/// for the tool's one argument without a JSON dependency).
fn string_field(json: &str, key: &str) -> Option<String> {
    let (_, rest) = json.split_once(&format!("\"{key}\""))?;
    let (_, rest) = rest.split_once(':')?;
    let rest = rest.trim_start().strip_prefix('"')?;
    let (value, _) = rest.split_once('"')?;
    (!value.is_empty()).then(|| value.to_owned())
}

bindings::export!(Weather with_types_in bindings);
