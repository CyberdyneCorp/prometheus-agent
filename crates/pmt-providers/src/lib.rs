//! Model providers (spec: model-providers): the typed profile-scoped
//! [`registry`] of provider [`descriptor`]s, route [`resolve`]-ution with
//! an explainable trace, the three native [`wire`] formats normalised into
//! one [`event`] stream, error [`classify`]-cation with same-route
//! retries, the [`fallback`] chain with turn-scoped restore, the model
//! [`metadata`] catalog with capability checks, per-call cost accounting
//! ([`cost`]), [`auxiliary`] task routing, [`reasoning`] effort, model
//! references and aliases ([`target`]), the `/model` [`switch`], local
//! inference servers ([`local`]), Mixture of Agents presets ([`moa`]),
//! credentials and subscription sign-in
//! ([`auth`]) and the deterministic in-process [`mock`] provider.

pub mod account;
pub mod auth;
pub mod auxiliary;
pub mod aws;
pub mod azure;
pub mod builtin;
pub mod caller;
pub mod calls;
pub mod canonical;
pub mod capability;
pub mod classify;
pub mod cost;
pub mod datause;
pub mod descriptor;
pub mod detect;
pub mod embeddings;
pub mod event;
pub mod external;
pub mod fallback;
pub mod fast;
pub mod health;
pub mod local;
pub mod local_runtime;
pub mod metadata;
pub mod moa;
pub mod mock;
pub mod pool;
pub mod quota;
pub mod reasoning;
pub mod registry;
pub mod request;
pub mod resolve;
pub mod retirement;
pub mod spend;
mod sse;
pub mod status;
pub mod switch;
pub mod target;
pub mod transport;
pub mod upstream;
pub mod wire;
pub mod wsl;

pub use caller::{CallError, CallObserver, Called, Failed, ModelCaller, Observed};
pub use event::{StopReason, StreamEvent, Usage};
pub use registry::{PluginProvider, Provider, ProviderRegistry};
pub use request::Request;
pub use resolve::{Resolver, Route};
pub use transport::ModelStream;
