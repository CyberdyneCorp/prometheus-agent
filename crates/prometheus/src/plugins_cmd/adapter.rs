//! `prometheus plugins test-adapter <platform|path>` (spec:
//! platform-adapters, Adapter conformance kit): runs the conformance suite
//! against a mock of the platform in a throwaway home, prints one
//! `pass`/`FAIL`/`skip` line per check and exits 0 only when nothing
//! failed. Every first-party adapter is tested by its platform name in a
//! build with the `adapter-kits` feature (the mocks are developer and CI
//! tooling, left out of release binaries to keep them within the size
//! budget); a `platform-adapter` plugin directory is tested in every build
//! with the gateway, against the kit's reference platform (see
//! `adapter_plugin`).

use pmt_core::error::Error;
#[cfg(feature = "gateway")]
use pmt_core::error::codes;

#[cfg(not(feature = "gateway"))]
pub(super) fn test_adapter(_path: &str) -> Result<u8, Error> {
    crate::surface::require("gateway").map(|()| 0)
}

#[cfg(feature = "gateway")]
pub(super) fn test_adapter(path: &str) -> Result<u8, Error> {
    let dir = std::path::Path::new(path);
    if dir.join("plugin.toml").is_file() {
        return super::adapter_plugin::test_plugin(dir);
    }
    first_party(path)
}

#[cfg(all(feature = "gateway", not(feature = "adapter-kits")))]
fn first_party(path: &str) -> Result<u8, Error> {
    if pmt_config::platforms::first_party(&path.to_lowercase()).is_some() {
        return Err(Error::new(
            &codes::SURFACE_NOT_INCLUDED,
            format!("the '{path}' adapter kit is not included in this build"),
        )
        .with_hint(
            "build prometheus with `--features adapter-kits` to test first-party adapters",
        ));
    }
    Err(not_an_adapter(path))
}

#[cfg(feature = "adapter-kits")]
fn first_party(path: &str) -> Result<u8, Error> {
    let Some(kit) = kits::find(path) else {
        return Err(not_an_adapter(path));
    };
    let home = tempfile::Builder::new()
        .prefix("prometheus-test-adapter-")
        .tempdir()
        .map_err(|e| Error::io(&codes::HOME_IO, &std::env::temp_dir(), &e))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Error::new(
                &codes::TASK_PANICKED,
                format!("cannot start the runtime: {e}"),
            )
        })?;
    let report = runtime.block_on(async {
        let (adapter, platform) = kit(home.path()).await?;
        Ok::<_, Error>(pmt_gateway::messaging::conformance::run(adapter, platform).await)
    })?;
    crate::output::print_raw(&report.render())?;
    Ok(u8::from(!report.passed()))
}

/// The refusal of something that is neither a first-party adapter nor a
/// plugin directory.
#[cfg(feature = "gateway")]
fn not_an_adapter(path: &str) -> Error {
    Error::new(
        &codes::CLI_USAGE,
        format!(
            "'{path}' is neither a first-party adapter (telegram, discord, slack, email, matrix, signal, whatsapp, webhook) nor a plugin directory"
        ),
    )
}

#[cfg(feature = "adapter-kits")]
mod kits {
    //! The first-party adapters with their mocks.

    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;

    use pmt_core::error::Error;
    use pmt_gateway::messaging::Adapter;
    use pmt_gateway::messaging::conformance::MockPlatform;

    type Built = (Arc<dyn Adapter>, Arc<dyn MockPlatform>);
    type KitFuture<'a> = Pin<Box<dyn Future<Output = Result<Built, Error>> + Send + 'a>>;
    pub(super) type Kit = for<'a> fn(&'a Path) -> KitFuture<'a>;

    fn telegram(home: &Path) -> KitFuture<'_> {
        Box::pin(pmt_adapters_telegram::mock::kit(home))
    }
    fn discord(home: &Path) -> KitFuture<'_> {
        Box::pin(pmt_adapters_discord::mock::kit(home))
    }
    fn slack(home: &Path) -> KitFuture<'_> {
        Box::pin(pmt_adapters_slack::mock::kit(home))
    }
    fn email(home: &Path) -> KitFuture<'_> {
        Box::pin(pmt_adapters_email::mock::kit(home))
    }
    fn matrix(home: &Path) -> KitFuture<'_> {
        Box::pin(pmt_adapters_matrix::mock::kit(home))
    }
    fn signal(home: &Path) -> KitFuture<'_> {
        Box::pin(pmt_adapters_signal::mock::kit(home))
    }
    fn whatsapp(home: &Path) -> KitFuture<'_> {
        Box::pin(pmt_adapters_whatsapp::mock::kit(home))
    }
    fn webhook(home: &Path) -> KitFuture<'_> {
        Box::pin(pmt_adapters_webhook::mock::kit(home))
    }

    /// Every first-party adapter's kit, by platform name.
    const KITS: [(&str, Kit); 8] = [
        (pmt_adapters_telegram::PLATFORM, telegram),
        (pmt_adapters_discord::PLATFORM, discord),
        (pmt_adapters_slack::PLATFORM, slack),
        (pmt_adapters_email::PLATFORM, email),
        (pmt_adapters_matrix::PLATFORM, matrix),
        (pmt_adapters_signal::PLATFORM, signal),
        (pmt_adapters_whatsapp::PLATFORM, whatsapp),
        (pmt_adapters_webhook::PLATFORM, webhook),
    ];

    /// The kit of a first-party platform name (any case).
    pub(super) fn find(name: &str) -> Option<Kit> {
        let name = name.to_lowercase();
        KITS.iter().find(|(p, _)| *p == name).map(|(_, kit)| *kit)
    }
}
