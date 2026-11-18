//! Growing a managed local model's window instead of compressing (spec:
//! local-models, Context window and memory placement): on the turn loop's
//! [`WindowGrowth`] port, a `llamacpp` route on the managed server asks
//! [`pmt_local::placement::grow`] for a larger window that fits this
//! machine; when one does, the library record and the server's preset
//! take it, the model is unloaded so its next request reloads it at that
//! window, the window is recorded as the route's served window (the
//! context gauge reads it) and `local_model.context_grown` goes to the
//! session's clients. Anything else answers why growth is impossible, and
//! the turn compresses.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use pmt_agent::turn::growth::{Growth, WindowGrowth};
use pmt_agent::turn::model::{BoxFuture, RouteInfo};
use pmt_local::library::{Model, Role};
use pmt_local::placement::{Budget, Shape};
use pmt_providers::metadata::Catalog;
use serde_json::{Value, json};

use super::Local;

/// Receives `local_model.context_grown` (the daemon publishes it to the
/// session's clients).
pub type GrownHook = Box<dyn Fn(&str, Value) + Send + Sync>;

static HOOK: OnceLock<GrownHook> = OnceLock::new();

/// Installs the process's receiver of `local_model.context_grown` (once).
pub fn on_grown(hook: GrownHook) {
    let _ = HOOK.set(hook);
}

/// The growth port of a session of `home`.
pub struct LocalGrowth {
    home: PathBuf,
    catalog: Arc<Catalog>,
    /// The hardware fixture directory of an in-process test (debug
    /// builds; the daemon reads `PMT_TEST_LOCAL_FIXTURES`).
    #[cfg(debug_assertions)]
    fixtures: Option<PathBuf>,
}

impl LocalGrowth {
    /// A port for sessions of `home` whose served windows `catalog` keeps.
    #[must_use]
    pub fn new(home: &Path, catalog: Arc<Catalog>) -> Self {
        Self {
            home: home.to_path_buf(),
            catalog,
            #[cfg(debug_assertions)]
            fixtures: None,
        }
    }

    /// The same port reading the machine from the fixtures in `dir`, so
    /// an in-process test sees the hardware its daemon sees, not the
    /// host's (debug builds).
    #[cfg(debug_assertions)]
    #[must_use]
    pub fn with_fixtures(self, dir: &Path) -> Self {
        Self {
            fixtures: Some(dir.to_path_buf()),
            ..self
        }
    }

    async fn try_grow(
        &self,
        session: &str,
        route: &RouteInfo,
        needed: u64,
    ) -> Result<Growth, String> {
        if route.provider != "llamacpp" {
            return Err("not the managed local runtime".to_owned());
        }
        let record = pmt_local::server::record::live(&self.home).ok_or_else(|| {
            "no managed server runs (a server someone else runs keeps its window)".to_owned()
        })?;
        let local = Local::for_home(&self.home, pmt_net::OutboundPolicy::default())
            .map_err(|e| e.to_string())?;
        #[cfg(debug_assertions)]
        let local = local.with_fixtures(self.fixtures.clone());
        let library = local.library();
        let mut model = library
            .models()
            .into_iter()
            .find(|m| m.id == route.model)
            .ok_or_else(|| format!("{} is not in the local library", route.model))?;
        let hardware = local.hardware();
        let selection = local.selection(&hardware);
        let budget = Budget {
            gpu: selection.gpu_budget,
            ram: selection.ram_budget,
            gpu_bandwidth: selection
                .gpu
                .as_ref()
                .map_or(0.0, pmt_local::hardware::Gpu::bandwidth),
        };
        let shape = shape_of(&local, &model)?;
        let from = model.context;
        let placed = pmt_local::placement::grow(&shape, &budget, from, needed)
            .map_err(|blocked| blocked.as_str().to_owned())?;
        model.context = placed.window;
        model.placement = Some(placed);
        library.publish(&model).map_err(|e| e.to_string())?;
        pmt_local::server::refresh(&self.home)
            .await
            .map_err(|e| e.to_string())?;
        let base_url = record.base_url();
        unload(record.port, &model.id).await;
        self.catalog
            .record_probe(&route.provider, &route.model, &base_url, placed.window);
        if let Some(hook) = HOOK.get() {
            let payload = json!({"session_id": session, "provider": route.provider,
                "model": route.model, "from": from, "to": placed.window});
            hook(session, payload);
        }
        Ok(Growth::Grown {
            from,
            to: placed.window,
        })
    }
}

impl WindowGrowth for LocalGrowth {
    fn grow<'a>(
        &'a self,
        session: &'a str,
        route: &'a RouteInfo,
        needed: u64,
    ) -> BoxFuture<'a, Growth> {
        Box::pin(async move {
            self.try_grow(session, route, needed)
                .await
                .unwrap_or_else(Growth::Impossible)
        })
    }
}

/// Unloads `model` so its next request loads it with the new preset.
async fn unload(port: u16, model: &str) {
    let Ok(client) = pmt_local::server::detect::loopback_client() else {
        return;
    };
    let url = format!("http://127.0.0.1:{port}/models/unload");
    let body = json!({"model": model}).to_string();
    let sent = client
        .post(url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await;
    if let Err(err) = sent {
        tracing::warn!(error = %err, "the grown model was not unloaded; it keeps its window until it reloads");
    }
}

/// What placing `model` needs: its catalog entry when it has one, else
/// its weights file's own header (a dense model).
fn shape_of(local: &Local, model: &Model) -> Result<Shape, String> {
    let catalog = local.catalog();
    if let Some(entry) = catalog.models.iter().find(|e| e.id == model.id) {
        let quant = model
            .quant
            .as_deref()
            .and_then(|q| entry.quants.iter().find(|x| x.quant == q))
            .or_else(|| entry.quants.first())
            .ok_or_else(|| format!("{} has no quantisation in the catalog", model.id))?;
        return Ok(pmt_local::catalog::shape(entry, quant));
    }
    let weights_path = model.weights().ok_or("the model has no weights file")?;
    let info = pmt_local::gguf::read(weights_path)?;
    let size = |role: Role| -> u64 {
        model
            .files
            .iter()
            .filter(|f| f.role == role)
            .map(|f| f.size)
            .sum()
    };
    let weights = size(Role::Weights);
    #[allow(clippy::cast_precision_loss)] // a byte count as a rate input
    let active_bytes = weights as f64;
    Ok(Shape {
        weights,
        projector: size(Role::Projector),
        experts: 0,
        kv_per_token: info.kv_bytes_per_token.unwrap_or(0),
        mtp: false,
        native: info
            .context_length
            .or(model.context_max)
            .unwrap_or(model.context),
        active_bytes,
    })
}
