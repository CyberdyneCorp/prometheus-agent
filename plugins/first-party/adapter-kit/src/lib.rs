//! The toolkit of the first-party catalog adapter plugins (spec:
//! platform-adapters, Catalog adapter set). A plugin implements
//! [`bindings::exports::prometheus::platform_adapter::plugin::Guest`] and
//! exports it with `adapter_kit::bindings::export_adapter!`; the modules hold what every
//! adapter needs, so each plugin keeps only its platform's protocol.
//!
//! Plugins have no clock (the sandbox offers none): the host polls every
//! 250 ms and [`clock`] counts those polls, which is what heartbeats,
//! cooldowns and backoffs measure.

pub mod bindings {
    //! The `platform-adapter` world.
    wit_bindgen::generate!({
        path: ["../../../wit/host", "../../../wit/platform-adapter"],
        world: "prometheus:platform-adapter/platform-adapter",
        generate_all,
        pub_export_macro: true,
        export_macro_name: "export_adapter",
        default_bindings_module: "adapter_kit::bindings",
    });
}

pub mod bridge;
pub mod clock;
pub mod host;
pub mod message;
pub mod net;
pub mod policy;
pub mod sign;
pub mod text;

pub use bindings::exports::prometheus::platform_adapter::plugin::{
    Guest, HttpRequest, HttpResponse,
};

/// An HTTP answer for `handle-http`.
#[must_use]
pub fn respond(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".to_owned(), content_type.to_owned())],
        body: body.into(),
    }
}

/// A header of `request` (names compared without case).
#[must_use]
pub fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The query parameters of a `handle-http` path (`/x?a=1&b=2`), decoded.
#[must_use]
pub fn query(path: &str) -> Vec<(String, String)> {
    path.split_once('?')
        .map(|(_, q)| text::form_decode(q))
        .unwrap_or_default()
}
