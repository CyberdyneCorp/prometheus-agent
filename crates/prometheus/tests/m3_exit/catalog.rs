//! Task 81.3, catalog adapters: a catalog adapter plugin, the first-party
//! Feishu/Lark adapter built for wasm32 with its socket bridge sidecar,
//! passes the adapter conformance kit against a mock Feishu (spec:
//! platform-adapters, Adapter conformance kit; Catalog adapter set).

use std::sync::Arc;

use pmt_gateway::messaging::conformance::MockPlatform;
use serde_json::json;

use crate::common::catalog::{self, Bridge};
use crate::common::catalog_feishu::Feishu;

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_adapter_passes_the_conformance_kit() {
    let mock = Feishu::start().await;
    let bridge = Bridge::start(&[mock.server.host()]);
    // Text batching off: the kit counts each message on its own.
    let (kit, _root) = mock.install(Some(&bridge), &json!({"batch_delay_ms": 0}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&mock) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
    let rendered = report.render();
    for check in [
        "connect",
        "inbound normalization",
        "deduplication",
        "splitting",
    ] {
        assert!(
            rendered.lines().any(|l| l == format!("pass  {check}")),
            "{rendered}"
        );
    }
}
