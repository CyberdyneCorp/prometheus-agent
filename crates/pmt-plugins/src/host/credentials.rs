//! The `credentials` host import (spec: plugin-system, Host credential
//! handles). A plugin names a credential by an opaque handle its
//! `credentials` grant covers; the host attaches the secret headers to the
//! outbound request itself, so the plugin never holds them. Everything
//! fails closed: an ungranted or unknown handle, a credential that is not
//! available, a URL that is not `https` or whose host is not one of the
//! credential's own hosts is refused before anything is sent, and the
//! request still passes the `network` grant and the egress policy like any
//! `http.send`.

use pmt_core::capability::{Capability, PluginRight};

use super::bindings::prometheus::credentials::handles::{self, Failure, Info};
use super::bindings::prometheus::host::types::HostError;
use super::imports::{HostState, destination};
use super::ports::{Credential, CredentialLookup};

impl HostState {
    /// The credential `handle` names, after the grant check.
    fn credential(&self, handle: &str) -> Result<Credential, Failure> {
        let right = Capability::Plugin {
            right: PluginRight::Credential,
            target: Some(handle.to_owned()),
        };
        self.require(&right)
            .map_err(|_| Failure::Denied(format!("capability_denied: credentials {handle}")))?;
        match self.ports().credentials.resolve(&self.ctx.plugin, handle) {
            CredentialLookup::Ready(credential) => Ok(credential),
            CredentialLookup::Unavailable(why) => Err(Failure::Unavailable(why)),
            CredentialLookup::Unknown => Err(Failure::UnknownHandle(format!(
                "no host credential named '{handle}'"
            ))),
        }
    }
}

/// Whether `url` is `https` to one of `credential`'s hosts (a host entry
/// without a port allows any port of that host).
fn allowed(credential: &Credential, url: &str) -> Result<(), Failure> {
    let refused = || Failure::HostNotAllowed(format!("the credential may not be sent to {url}"));
    if !url
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    {
        return Err(refused());
    }
    let (host, port) = destination(url).ok_or_else(refused)?;
    let matches =
        credential
            .hosts
            .iter()
            .any(|entry| match pmt_core::capability::split_port(entry) {
                Some((name, None)) => name.eq_ignore_ascii_case(&host),
                Some((name, Some(p))) => name.eq_ignore_ascii_case(&host) && p == port,
                None => false,
            });
    if matches { Ok(()) } else { Err(refused()) }
}

fn failure(error: HostError) -> Failure {
    match error {
        HostError::Denied(why) => Failure::Denied(why),
        HostError::Invalid(why) | HostError::Failed(why) => Failure::Failed(why),
    }
}

impl handles::Host for HostState {
    fn describe(&mut self, handle: String) -> Result<Info, Failure> {
        let credential = self.credential(&handle)?;
        Ok(Info {
            kind: credential.kind,
            origin: credential.origin,
            billing_source: credential.billing_source,
        })
    }

    fn send(
        &mut self,
        handle: String,
        request: handles::Request,
    ) -> Result<handles::Response, Failure> {
        let credential = self.credential(&handle)?;
        allowed(&credential, &request.url)?;
        let mut headers: Vec<(String, String)> = request
            .headers
            .into_iter()
            .filter(|(name, _)| {
                !credential
                    .headers
                    .iter()
                    .any(|(own, _)| own.eq_ignore_ascii_case(name))
            })
            .collect();
        headers.extend(
            credential
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.expose().to_owned())),
        );
        let response = self
            .send_http(request.method, request.url, headers, request.body)
            .map_err(failure)?;
        Ok(handles::Response {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modal() -> Credential {
        Credential {
            kind: "direct".to_owned(),
            origin: "https://api.modal.com".to_owned(),
            billing_source: None,
            hosts: vec!["api.modal.com".to_owned(), "gw.example:8443".to_owned()],
            headers: Vec::new(),
        }
    }

    #[test]
    fn a_credential_goes_only_to_its_own_hosts_over_https() {
        let credential = modal();
        assert!(
            allowed(
                &credential,
                "https://api.modal.com/modal.client.ModalClient/X"
            )
            .is_ok()
        );
        assert!(allowed(&credential, "HTTPS://API.MODAL.COM:443/").is_ok());
        assert!(allowed(&credential, "https://gw.example:8443/v1").is_ok());
        for refused in [
            "http://api.modal.com/",
            "https://api.modal.com.evil.example/",
            "https://evil.example/?h=api.modal.com",
            "https://gw.example/v1",
            "https://user@api.modal.com/",
            "nonsense",
        ] {
            assert!(
                matches!(
                    allowed(&credential, refused),
                    Err(Failure::HostNotAllowed(_))
                ),
                "{refused}"
            );
        }
    }
}
