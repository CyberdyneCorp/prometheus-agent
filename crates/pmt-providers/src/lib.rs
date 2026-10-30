//! Model providers (spec: model-providers): the typed profile-scoped
//! [`registry`] of provider [`descriptor`]s, route [`resolve`]-ution with
//! an explainable trace, the three native [`wire`] formats normalised into
//! one [`event`] stream, error [`classify`]-cation with same-route
//! retries, the [`fallback`] chain with turn-scoped restore, the model
//! [`metadata`] catalog with capability checks, and per-call cost
//! accounting ([`cost`]).

pub mod builtin;
pub mod caller;
pub mod capability;
pub mod classify;
pub mod cost;
pub mod descriptor;
pub mod event;
pub mod fallback;
pub mod metadata;
pub mod registry;
pub mod request;
pub mod resolve;
mod sse;
pub mod transport;
pub mod wire;

pub use caller::{CallError, Called, Failed, ModelCaller};
pub use event::{StopReason, StreamEvent, Usage};
pub use registry::{Provider, ProviderRegistry};
pub use request::Request;
pub use resolve::{Resolver, Route};
pub use transport::ModelStream;
