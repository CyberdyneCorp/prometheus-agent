//! The `bench` world's host imports (spec: replay-and-evaluation,
//! Benchmark adapter protocol): `files` reaches only the directory of the
//! adapter call in progress and `requests` answers the adapter's requests
//! to the host. Both go to the [`BenchPort`](super::ports::BenchPort) the
//! caller set for that call; outside one every call fails.

use super::bindings::prometheus::bench::types::RpcError;
use super::bindings::prometheus::bench::{files, requests, types};
use super::imports::HostState;

/// The answer outside an adapter call.
const NO_CALL: &str = "no benchmark adapter call is in progress";

impl types::Host for HostState {}

impl files::Host for HostState {
    fn read(&mut self, path: String) -> Result<Vec<u8>, String> {
        self.bench.as_ref().ok_or(NO_CALL)?.read(&path)
    }

    fn write(&mut self, path: String, contents: Vec<u8>) -> Result<(), String> {
        self.bench.as_ref().ok_or(NO_CALL)?.write(&path, &contents)
    }

    fn entries(&mut self) -> Result<Vec<String>, String> {
        self.bench.as_ref().ok_or(NO_CALL)?.list()
    }
}

impl requests::Host for HostState {
    fn request(&mut self, method: String, params: String) -> Result<String, RpcError> {
        let (code, message) = match &self.bench {
            Some(port) => port.request(&method, &params),
            None => (-32601, NO_CALL.to_owned()),
        };
        Err(RpcError { code, message })
    }
}
