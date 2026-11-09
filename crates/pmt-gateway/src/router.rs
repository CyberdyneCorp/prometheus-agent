//! Method dispatch (spec: workspace-architecture, Contracts generated from
//! types and checked in; Strict parameter validation and contract
//! violations). Handlers are registered per typed [`Method`], so a method
//! without a contract cannot be served, and a name missing from the
//! catalog index fails registration at startup. Params are validated
//! against the contract before the handler runs; a result that breaks its
//! contract is a daemon bug handled by the violation policy.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::Method;
use pmt_rpc::{Catalog, Kind, RpcError};
use serde_json::Value;

use crate::server::Call;

type Reply = Pin<Box<dyn Future<Output = Result<Value, RpcError>> + Send>>;
type Handler = Box<dyn Fn(Call, Value) -> Reply + Send + Sync>;

/// The served methods.
#[derive(Default)]
pub struct Router {
    handlers: BTreeMap<&'static str, Handler>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.handlers.keys()).finish()
    }
}

impl Router {
    /// Serves method `M` with `handler`.
    ///
    /// # Errors
    ///
    /// `unregistered method name` when `M` is not an indexed method with a
    /// declared contract in `catalog`.
    pub fn register<M, F, Fut>(
        &mut self,
        catalog: &Catalog,
        handler: F,
    ) -> Result<&mut Self, String>
    where
        M: Method,
        F: Fn(Call, M::Params) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Result, RpcError>> + Send + 'static,
    {
        let indexed = catalog
            .entry(M::NAME)
            .is_some_and(|e| e.kind == Kind::Method);
        if !indexed || catalog.contract(M::NAME).is_none() {
            return Err(format!("unregistered method name: {}", M::NAME));
        }
        let handler = Arc::new(handler);
        self.handlers.insert(
            M::NAME,
            Box::new(move |call, params| {
                let handler = Arc::clone(&handler);
                Box::pin(async move {
                    let params: M::Params = serde_json::from_value(params).map_err(|e| {
                        RpcError::invalid_params("", false, &format!("invalid params: {e}"))
                    })?;
                    let result = handler(call, params).await?;
                    Ok(serde_json::to_value(result).unwrap_or_default())
                })
            }),
        );
        Ok(self)
    }

    /// The methods with a handler.
    #[must_use]
    pub fn served(&self) -> Vec<String> {
        self.handlers.keys().map(|k| (*k).to_owned()).collect()
    }

    /// Validates `params`, runs the handler and checks its result.
    ///
    /// # Errors
    ///
    /// `-32601` for a name the catalog does not index as a method, `-32602`
    /// for params that break the contract, `-32006 rpc.not_served` for an
    /// indexed method without a handler, the handler's error, or `-32099`
    /// for a result that breaks its contract.
    pub async fn dispatch(
        &self,
        call: Call,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let server = Arc::clone(&call.server);
        let catalog = server.catalog();
        if catalog.entry(method).is_none_or(|e| e.kind != Kind::Method) {
            return Err(RpcError::unknown_method(method));
        }
        let Some(handler) = self.handlers.get(method) else {
            let err = Error::new(
                &codes::RPC_NOT_SERVED,
                format!("method {method} is not served by this daemon"),
            );
            return Err(RpcError::from_error(&err));
        };
        catalog.check_params(method, &params)?;
        let result = handler(call, params).await?;
        if let Err(violation) = catalog.check_outbound(method, &result, true) {
            server.violations().report(method, &violation);
            let err = Error::new(
                &codes::RPC_CONTRACT_VIOLATION,
                format!("{method} produced an invalid result"),
            );
            return Err(RpcError::from_error(&err));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use pmt_rpc::Catalog;
    use pmt_rpc::contracts::{Ack, Empty, Method};

    use super::Router;

    /// A handler for a name no capability indexed.
    struct Frobnicate;

    impl Method for Frobnicate {
        const NAME: &'static str = "session.frobnicate";
        type Params = Empty;
        type Result = Ack;
    }

    /// Indexed, but its capability declared no contract yet.
    struct JourneyList;

    impl Method for JourneyList {
        const NAME: &'static str = "journey.list";
        type Params = Empty;
        type Result = Ack;
    }

    #[test]
    fn an_unregistered_method_name_fails_at_startup() {
        let catalog = Catalog::builtin();
        let mut router = Router::default();
        let err = router
            .register::<Frobnicate, _, _>(catalog, |_, _| async { Ok(Ack::YES) })
            .unwrap_err();
        assert_eq!(err, "unregistered method name: session.frobnicate");
        assert!(
            router
                .register::<JourneyList, _, _>(catalog, |_, _| async { Ok(Ack::YES) })
                .is_err(),
            "a method without a declared contract cannot be served"
        );
        let mut builtin = Router::default();
        crate::methods::register(&mut builtin, catalog).unwrap();
        assert!(builtin.served().contains(&"prompt.submit".to_owned()));
    }
}
