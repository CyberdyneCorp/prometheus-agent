//! The typed catalog (spec: workspace-architecture, Contracts generated from
//! types and checked in; Catalog index of capability-owned methods and
//! events): the [`index`] of every name with its owning capability, plus
//! the JSON Schema of each declared contract. It validates params, results
//! and payloads at runtime and renders `schema/rpc.json`.

pub mod contracts;
pub mod index;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use schemars::generate::{SchemaGenerator, SchemaSettings};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use self::contracts::Version;
use self::contracts::{Event, Method, ServerRequest};
pub use self::index::{INDEX, IndexEntry, Kind};
use crate::codes::{CloseCode, ERROR_TABLE};
use crate::frame::RpcError;
use crate::validate::{self, Violation};

/// The schemas of one declared name.
#[derive(Debug, Clone)]
pub struct Contract {
    /// Method params, server-request params or event payload.
    pub params: Value,
    /// Method or server-request result; `None` for an event.
    pub result: Option<Value>,
}

/// Collects contracts while a catalog is built.
pub struct Declarations {
    generator: SchemaGenerator,
    contracts: Vec<(&'static str, Kind, Contract)>,
}

impl Declarations {
    fn schema<T: schemars::JsonSchema>(&mut self) -> Value {
        self.generator.subschema_for::<T>().to_value()
    }

    /// Declares method `M`.
    pub fn method<M: Method>(&mut self) -> &mut Self {
        let contract = Contract {
            params: self.schema::<M::Params>(),
            result: Some(self.schema::<M::Result>()),
        };
        self.contracts.push((M::NAME, Kind::Method, contract));
        self
    }

    /// Declares server request `R`.
    pub fn server_request<R: ServerRequest>(&mut self) -> &mut Self {
        let contract = Contract {
            params: self.schema::<R::Params>(),
            result: Some(self.schema::<R::Result>()),
        };
        self.contracts
            .push((R::NAME, Kind::ServerRequest, contract));
        self
    }

    /// Declares event `E`.
    pub fn event<E: Event>(&mut self) -> &mut Self {
        let contract = Contract {
            params: self.schema::<E::Payload>(),
            result: None,
        };
        self.contracts.push((E::NAME, Kind::Event, contract));
        self
    }
}

/// An index with its declared contracts.
#[derive(Debug, Clone)]
pub struct Catalog {
    protocol: Version,
    index: BTreeMap<&'static str, IndexEntry>,
    contracts: BTreeMap<&'static str, Contract>,
    defs: Map<String, Value>,
}

static BUILTIN: LazyLock<Catalog> = LazyLock::new(|| {
    Catalog::build(
        pmt_core::protocol::RPC_PROTOCOL.into(),
        INDEX,
        declare_builtin,
    )
    .expect("every built-in contract is indexed")
});

/// The contracts this build declares.
pub fn declare_builtin(d: &mut Declarations) {
    use self::contracts as c;
    d.method::<c::Hello>()
        .method::<c::Ping>()
        .method::<c::SessionCreate>()
        .method::<c::SessionAttach>()
        .method::<c::SessionDetach>()
        .method::<c::SessionListing>()
        .method::<c::SessionResume>()
        .method::<c::SessionInterrupt>()
        .method::<c::SessionResolve>()
        .method::<c::SessionFork>()
        .method::<c::SessionCompact>()
        .method::<c::SessionTranscript>()
        .method::<c::SessionEventsSince>()
        .method::<c::SessionContextUsage>()
        .method::<c::PromptSubmit>()
        .method::<c::CommandDispatch>()
        .method::<c::ApprovalRespond>()
        .server_request::<c::ApprovalAsk>()
        .server_request::<c::ClarifyAsk>()
        .server_request::<c::SecretAsk>()
        .server_request::<c::SudoAsk>()
        .event::<c::RequestCancelled>()
        .event::<c::RequestDone>()
        .event::<c::SessionEvent>()
        .event::<c::TurnEnded>();
}

impl Catalog {
    /// The catalog of this build.
    #[must_use]
    pub fn builtin() -> &'static Self {
        &BUILTIN
    }

    /// Builds a catalog from `index` and the contracts `declare` adds.
    ///
    /// # Errors
    ///
    /// A contract whose name is not indexed, is indexed as another kind or
    /// is declared twice; an index naming one name twice.
    pub fn build(
        protocol: Version,
        index: &[IndexEntry],
        declare: impl FnOnce(&mut Declarations),
    ) -> Result<Self, String> {
        let mut by_name = BTreeMap::new();
        for entry in index {
            if by_name.insert(entry.name, *entry).is_some() {
                return Err(format!("{} is indexed twice", entry.name));
            }
        }
        let settings = SchemaSettings::draft2020_12();
        let mut declarations = Declarations {
            generator: SchemaGenerator::new(settings),
            contracts: Vec::new(),
        };
        declare(&mut declarations);
        let mut contracts = BTreeMap::new();
        for (name, kind, contract) in declarations.contracts {
            match by_name.get(name) {
                None => {
                    return Err(format!(
                        "unregistered RPC name: {name} is not in the catalog index"
                    ));
                }
                Some(entry) if entry.kind != kind => {
                    return Err(format!(
                        "{name} is indexed as {:?}, declared as {kind:?}",
                        entry.kind
                    ));
                }
                Some(_) => {}
            }
            if contracts.insert(name, contract).is_some() {
                return Err(format!("{name} has two contracts"));
            }
        }
        let defs = declarations.generator.take_definitions(true);
        Ok(Self {
            protocol,
            index: by_name,
            contracts,
            defs,
        })
    }

    /// The protocol version the catalog describes.
    #[must_use]
    pub fn protocol(&self) -> Version {
        self.protocol
    }

    /// The index entry of `name`.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&IndexEntry> {
        self.index.get(name)
    }

    /// Every index entry, by name.
    pub fn entries(&self) -> impl Iterator<Item = &IndexEntry> {
        self.index.values()
    }

    /// The contract of `name`, when one is declared.
    #[must_use]
    pub fn contract(&self, name: &str) -> Option<&Contract> {
        self.contracts.get(name)
    }

    /// Checks method params before the handler runs.
    ///
    /// # Errors
    ///
    /// `-32602` naming the offending path; `-32601` for an unindexed name.
    pub fn check_params(&self, method: &str, params: &Value) -> Result<(), RpcError> {
        let Some(contract) = self.contracts.get(method) else {
            return Err(RpcError::unknown_method(method));
        };
        validate::validate(params, &contract.params, &self.defs).map_err(|v| {
            RpcError::invalid_params(&v.path, v.unknown_field, &format!("invalid params: {v}"))
        })
    }

    /// Checks what the daemon is about to send: a result (`result = true`)
    /// or an event payload / server-request params.
    ///
    /// # Errors
    ///
    /// The violation; the caller applies the contract-violation policy.
    pub fn check_outbound(&self, name: &str, value: &Value, result: bool) -> Result<(), Violation> {
        let Some(contract) = self.contracts.get(name) else {
            return Err(Violation {
                path: String::new(),
                unknown_field: false,
                message: format!("{name} has no contract"),
            });
        };
        let schema = if result {
            contract.result.as_ref().unwrap_or(&Value::Bool(true))
        } else {
            &contract.params
        };
        validate::validate(value, schema, &self.defs)
    }

    /// Checks a client's answer to a server request.
    ///
    /// # Errors
    ///
    /// The violation.
    pub fn check_answer(&self, request: &str, value: &Value) -> Result<(), Violation> {
        self.check_outbound(request, value, true)
    }

    /// Removes fields introduced after `minor` from an outbound value.
    pub fn strip_newer(&self, name: &str, value: &mut Value, result: bool, minor: u16) {
        if let Some(contract) = self.contracts.get(name) {
            let schema = if result {
                contract.result.as_ref()
            } else {
                Some(&contract.params)
            };
            if let Some(schema) = schema {
                validate::strip_newer(value, schema, &self.defs, minor);
            }
        }
    }

    /// Whether a client speaking `minor` that advertised `events` may
    /// receive `event`.
    #[must_use]
    pub fn event_visible(&self, event: &str, minor: u16, advertised: &[String]) -> bool {
        self.index
            .get(event)
            .is_some_and(|e| e.since <= minor || advertised.iter().any(|a| a == event))
    }

    /// `schema/rpc.json`.
    #[must_use]
    pub fn schema(&self) -> Value {
        let mut sections: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
        for kind in [Kind::Method, Kind::ServerRequest, Kind::Event] {
            sections.insert(kind.section(), Map::new());
        }
        for entry in self.index.values() {
            let mut item = Map::new();
            item.insert("owner".into(), entry.owner.into());
            item.insert(
                "since".into(),
                format!("{}.{}", self.protocol.major, entry.since).into(),
            );
            match self.contracts.get(entry.name) {
                Some(contract) if entry.kind == Kind::Event => {
                    item.insert("payload".into(), contract.params.clone());
                }
                Some(contract) => {
                    item.insert("params".into(), contract.params.clone());
                    item.insert(
                        "result".into(),
                        contract.result.clone().unwrap_or(Value::Null),
                    );
                }
                None => {
                    item.insert("declared".into(), false.into());
                }
            }
            if let Some(section) = sections.get_mut(entry.kind.section()) {
                section.insert(entry.name.into(), Value::Object(item));
            }
        }
        let close_codes: Map<String, Value> = CloseCode::ALL
            .iter()
            .map(|c| (c.code().to_string(), c.name().into()))
            .collect();
        let error_codes: Map<String, Value> = ERROR_TABLE
            .iter()
            .map(|r| (r.code.to_string(), r.meaning.into()))
            .collect();
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Prometheus client protocol",
            "protocol": self.protocol.to_string(),
            "error_codes": error_codes,
            "close_codes": close_codes,
            "methods": sections.remove("methods"),
            "server_requests": sections.remove("server_requests"),
            "events": sections.remove("events"),
            "$defs": self.defs,
        })
    }

    /// The `methods_digest` of `rpc.hello`: SHA-256 of the schema.
    #[must_use]
    pub fn digest(&self) -> String {
        let hash = Sha256::digest(self.schema().to_string().as_bytes());
        hash.iter().fold(String::with_capacity(64), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
    }
}

#[cfg(test)]
mod tests;
