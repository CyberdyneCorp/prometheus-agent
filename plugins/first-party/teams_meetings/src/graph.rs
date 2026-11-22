//! Microsoft Graph for the pipeline: an app token (client credentials,
//! scope `https://graph.microsoft.com/.default`), subscriptions, the
//! meeting artifacts a notification names, and the meeting's chat.

use adapter_kit::net;
use adapter_kit::oauth::ClientCredentials;
use serde_json::{Value, json};

/// The secret holding the app's client secret.
pub(crate) const SECRET: &str = "MSGRAPH_CLIENT_SECRET";
const SCOPE: &str = "https://graph.microsoft.com/.default";

/// What a notification's resource names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Artifact {
    /// `transcripts` or `recordings`.
    pub(crate) kind: String,
    pub(crate) meeting: String,
    pub(crate) id: String,
    /// The resource as a path (`communications/onlineMeetings/<m>/transcripts/<t>`).
    pub(crate) path: String,
    /// The meeting's own path.
    pub(crate) meeting_path: String,
}

/// `X('id')` segments as `X/id`, no leading slash.
fn as_path(resource: &str) -> String {
    resource
        .trim_matches('/')
        .replace("('", "/")
        .replace("')", "")
}

/// The meeting artifact `resource` names, if it names one.
pub(crate) fn artifact(resource: &str) -> Option<Artifact> {
    let path = as_path(resource);
    let segments: Vec<&str> = path.split('/').collect();
    let at = segments.iter().position(|s| *s == "onlineMeetings")?;
    let meeting = (*segments.get(at + 1)?).to_owned();
    let kind = (*segments.get(at + 2)?).to_owned();
    if kind != "transcripts" && kind != "recordings" {
        return None;
    }
    let id = (*segments.get(at + 3)?).to_owned();
    Some(Artifact {
        meeting_path: segments[..=at + 1].join("/"),
        kind,
        meeting,
        id,
        path,
    })
}

/// A transcript in WebVTT as `Speaker: words` lines.
pub(crate) fn vtt_text(vtt: &str) -> String {
    let mut lines = Vec::new();
    for line in vtt.lines().map(str::trim) {
        if line.is_empty()
            || line == "WEBVTT"
            || line.contains("-->")
            || line.parse::<u64>().is_ok()
        {
            continue;
        }
        match line
            .strip_prefix("<v ")
            .and_then(|rest| rest.split_once('>'))
        {
            Some((speaker, words)) => {
                lines.push(format!("{speaker}: {}", words.trim_end_matches("</v>")));
            }
            None => lines.push(line.to_owned()),
        }
    }
    lines.join("\n")
}

/// Graph with the app's token.
#[derive(Debug, Clone)]
pub(crate) struct Graph {
    pub(crate) base: String,
    token: ClientCredentials,
}

impl Graph {
    pub(crate) fn new(options: &Value, client_id: &str, tenant: &str) -> Self {
        let login = options["login_base"]
            .as_str()
            .unwrap_or("https://login.microsoftonline.com")
            .trim_end_matches('/');
        Self {
            base: options["graph_base"]
                .as_str()
                .unwrap_or("https://graph.microsoft.com/v1.0")
                .trim_end_matches('/')
                .to_owned(),
            token: ClientCredentials::new(
                &format!("{login}/{tenant}/oauth2/v2.0/token"),
                client_id,
                SECRET,
                SCOPE,
            ),
        }
    }

    fn call(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<net::Response, String> {
        let bearer = self.token.bearer()?;
        let mut headers = vec![("authorization", bearer.as_str())];
        if body.is_some() {
            headers.push(("content-type", "application/json"));
        }
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let response = net::send(
            method,
            &url,
            &headers,
            body.map(|b| b.to_string().into_bytes()),
        )?;
        net::classify(&response)?;
        Ok(response)
    }

    pub(crate) fn json(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        self.call(method, path, body).map(|r| r.json())
    }

    /// A transcript's text.
    pub(crate) fn transcript(&mut self, path: &str) -> Result<String, String> {
        let response = self.call("GET", &format!("{path}/content?$format=text/vtt"), None)?;
        Ok(vtt_text(&response.text()))
    }

    /// The newest transcript of a meeting, as an artifact.
    pub(crate) fn latest(
        &mut self,
        meeting_path: &str,
        kind: &str,
    ) -> Result<Option<Artifact>, String> {
        let listing = self.json("GET", &format!("{meeting_path}/{kind}"), None)?;
        Ok(listing["value"]
            .as_array()
            .and_then(|v| v.last())
            .and_then(|item| item["id"].as_str())
            .and_then(|id| artifact(&format!("{meeting_path}/{kind}/{id}"))))
    }

    /// A recording's bytes.
    pub(crate) fn recording(&mut self, path: &str) -> Result<Vec<u8>, String> {
        self.call("GET", &format!("{path}/content"), None)
            .map(|r| r.body)
    }

    /// Posts `text` into the meeting's chat.
    pub(crate) fn post_to_chat(
        &mut self,
        meeting_path: &str,
        text: &str,
    ) -> Result<String, String> {
        let meeting = self.json("GET", meeting_path, None)?;
        let thread = meeting["chatInfo"]["threadId"]
            .as_str()
            .ok_or("permanent: the meeting has no chat")?;
        let posted = self.json(
            "POST",
            &format!("chats/{thread}/messages"),
            Some(&json!({"body": {"contentType": "text", "content": text}})),
        )?;
        Ok(posted["id"].as_str().unwrap_or_default().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_name_meeting_artifacts() {
        let a = artifact("communications/onlineMeetings('MSo1')/transcripts('TR1')").unwrap();
        assert_eq!(
            (a.kind.as_str(), a.meeting.as_str(), a.id.as_str()),
            ("transcripts", "MSo1", "TR1")
        );
        assert_eq!(a.path, "communications/onlineMeetings/MSo1/transcripts/TR1");
        assert_eq!(a.meeting_path, "communications/onlineMeetings/MSo1");
        let r = artifact("users/u1/onlineMeetings/M2/recordings/R9").unwrap();
        assert_eq!(
            (r.kind.as_str(), r.meeting_path.as_str()),
            ("recordings", "users/u1/onlineMeetings/M2")
        );
        assert_eq!(artifact("users/u1/messages/m1"), None);
    }

    #[test]
    fn vtt_becomes_speaker_lines() {
        let vtt = "WEBVTT\n\n1\n00:00:00.000 --> 00:00:02.000\n<v Ada>Ship it.</v>\n\n00:00:02.000 --> 00:00:04.000\n<v Bob>Friday.</v>\n";
        assert_eq!(vtt_text(vtt), "Ada: Ship it.\nBob: Friday.");
    }
}
