//! Pricing catalog entries against the machine.
//!
//! A model's memory need at a window is its weights, the vision projector,
//! the KV cache for that window and a runtime overhead (compute buffers:
//! 512 MiB plus 2% of the weights). The quantisation is the highest one
//! that fits the GPU budget at [`MIN_WINDOW`]; when none does, the smallest
//! one (never below [`MIN_BITS`]) is priced against GPU plus free RAM. The
//! speed estimate is memory-bound decoding: bandwidth over the bytes of the
//! active weights read per token, at 60% efficiency.

use serde::Serialize;

use super::{Catalog, Entry, Quant};
use crate::backend::Selection;
use crate::hardware::{CPU_BANDWIDTH_GBPS, GIB};

/// No build below this many bits per weight is offered.
pub const MIN_BITS: f64 = 4.0;
/// The window every recommended model is guaranteed.
pub const MIN_WINDOW: u64 = 65_536;
/// Tokens per second a recommended model must reach.
pub const SPEED_FLOOR: f64 = 20.0;

const OVERHEAD_FIXED: u64 = GIB / 2;
const EFFICIENCY: f64 = 0.6;

/// Where a model's memory goes on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Entirely in GPU (or unified) memory.
    FitsGpu,
    /// Part of the weights spill to system RAM.
    UsesSystemRam,
    /// Does not fit at all.
    TooBig,
}

/// One catalog entry priced for this machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Priced {
    /// Library id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The memory fit.
    pub fit: Fit,
    /// The quantisation selected for this machine.
    pub quant: String,
    /// Its bits per weight.
    pub bits: f64,
    /// The window the model starts at.
    pub context_start: u64,
    /// The model's native window.
    pub context_max: u64,
    /// Bytes to download (weights and projector).
    pub download_bytes: u64,
    /// Bytes of memory at the starting window.
    pub memory_bytes: u64,
    /// Predicted decoding speed.
    pub tokens_per_second: f64,
    /// Whether this is the recommended model.
    pub recommended: bool,
    /// Why a `too_big` model does not fit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Curated by Prometheus (community models from search are not).
    pub validated: bool,
}

/// Every entry of `catalog` priced for `selection`, catalog order, with at
/// most one marked `recommended`.
#[must_use]
pub fn price(catalog: &Catalog, selection: &Selection) -> Vec<Priced> {
    let mut rows: Vec<Priced> = catalog
        .models
        .iter()
        .filter_map(|entry| price_entry(entry, selection))
        .collect();
    let floor = |r: &&Priced| r.fit == Fit::FitsGpu && r.tokens_per_second >= SPEED_FLOOR;
    let vendor = catalog
        .models
        .iter()
        .find(|m| m.vendor_default)
        .and_then(|m| {
            rows.iter()
                .position(|r| r.id == m.id && r.fit == Fit::FitsGpu)
        });
    let best = vendor.or_else(|| {
        rows.iter()
            .filter(floor)
            .max_by_key(|r| quality(catalog, &r.id))
            .and_then(|r| rows.iter().position(|x| x.id == r.id))
    });
    if let Some(index) = best {
        rows[index].recommended = true;
    }
    rows
}

fn quality(catalog: &Catalog, id: &str) -> u32 {
    catalog
        .models
        .iter()
        .find(|m| m.id == id)
        .map_or(0, |m| m.quality)
}

/// Memory `quant` of `entry` needs at `window` tokens.
#[must_use]
fn need(entry: &Entry, quant: &Quant, window: u64) -> u64 {
    let weights = quant.bytes() + entry.projector.as_ref().map_or(0, |p| p.size);
    weights + entry.kv_bytes_per_token * window + OVERHEAD_FIXED + weights / 50
}

fn price_entry(entry: &Entry, selection: &Selection) -> Option<Priced> {
    let mut quants: Vec<&Quant> = entry.quants.iter().filter(|q| q.bits >= MIN_BITS).collect();
    quants.sort_by(|a, b| b.bits.total_cmp(&a.bits));
    let min_window = MIN_WINDOW.min(entry.context_max);
    let gpu = selection.gpu_budget;
    let fitting = quants
        .iter()
        .find(|q| gpu > 0 && need(entry, q, min_window) <= gpu);
    let (quant, fit) = if let Some(q) = fitting {
        (*q, Fit::FitsGpu)
    } else {
        let smallest = *quants.last()?;
        let spill = need(entry, smallest, min_window) <= gpu + selection.ram_budget;
        (
            smallest,
            if spill {
                Fit::UsesSystemRam
            } else {
                Fit::TooBig
            },
        )
    };
    let context_start = match fit {
        Fit::FitsGpu => grown_window(entry, quant, min_window, gpu),
        _ => min_window,
    };
    let memory_bytes = need(entry, quant, context_start);
    Some(Priced {
        id: entry.id.clone(),
        name: entry.name.clone(),
        fit,
        quant: quant.quant.clone(),
        bits: quant.bits,
        context_start,
        context_max: entry.context_max,
        download_bytes: quant.bytes() + entry.projector.as_ref().map_or(0, |p| p.size),
        memory_bytes,
        tokens_per_second: speed(entry, quant, fit, selection),
        recommended: false,
        reason: (fit == Fit::TooBig).then(|| too_big(memory_bytes, selection)),
        validated: true,
    })
}

/// The largest doubling of `start` up to the native window that still fits
/// the GPU.
fn grown_window(entry: &Entry, quant: &Quant, start: u64, budget: u64) -> u64 {
    let mut window = start;
    while window * 2 <= entry.context_max && need(entry, quant, window * 2) <= budget {
        window *= 2;
    }
    window
}

fn speed(entry: &Entry, quant: &Quant, fit: Fit, selection: &Selection) -> f64 {
    let bandwidth = match (fit, &selection.gpu) {
        (Fit::FitsGpu, Some(gpu)) => gpu.bandwidth(),
        (Fit::TooBig, _) => return 0.0,
        _ => CPU_BANDWIDTH_GBPS,
    };
    let bytes_per_token = entry.active_params_b * quant.bits / 8.0;
    let tps = bandwidth * EFFICIENCY / bytes_per_token;
    (tps * 10.0).round() / 10.0
}

#[allow(clippy::cast_precision_loss)] // a one-decimal GiB figure
fn too_big(need: u64, selection: &Selection) -> String {
    let gib = |b: u64| format!("{:.1} GiB", b as f64 / GIB as f64);
    format!(
        "needs {} at a {MIN_WINDOW}-token window; this machine has {} of GPU memory and {} of free RAM",
        gib(need),
        gib(selection.gpu_budget),
        gib(selection.ram_budget)
    )
}

#[cfg(test)]
mod tests {
    use pmt_config::daemon_local::LocalBackend;

    use super::{Fit, MIN_BITS, price};
    use crate::backend::select;
    use crate::catalog::Catalog;
    use crate::hardware::{GIB, Gpu, Hardware, Vendor};

    fn machine(gpu_memory: u64, ram: u64) -> Hardware {
        Hardware {
            platform: "linux-x64".to_owned(),
            ram_total: ram,
            ram_available: ram,
            gpus: vec![Gpu {
                vendor: Vendor::Nvidia,
                name: "NVIDIA GeForce RTX 4060".to_owned(),
                memory_total: gpu_memory,
                memory_used: 0,
                integrated: false,
                cuda: Some(13),
                bandwidth_gbps: None,
            }],
        }
    }

    #[test]
    fn an_8_gb_gpu_spills_or_refuses_a_35b_model_and_never_goes_below_4_bits() {
        let catalog = Catalog::embedded();
        let rows = price(
            &catalog,
            &select(LocalBackend::Auto, &machine(8 * GIB, 32 * GIB)),
        );
        assert_eq!(rows.len(), catalog.models.len());
        let big = rows.iter().find(|r| r.id == "qwen3.6-35b-a3b").unwrap();
        assert!(
            matches!(big.fit, Fit::UsesSystemRam | Fit::TooBig),
            "{big:?}"
        );
        assert!(rows.iter().all(|r| r.bits >= MIN_BITS));
        let small = rows.iter().find(|r| r.id == "gemma-4-e4b").unwrap();
        assert_eq!(small.fit, Fit::FitsGpu);
        assert!(small.context_start >= 65_536);
        let huge = rows.iter().find(|r| r.id == "gpt-oss-120b").unwrap();
        assert_eq!(huge.fit, Fit::TooBig);
        assert!(huge.reason.as_deref().unwrap().contains("GPU memory"));
    }

    #[test]
    fn the_best_quant_that_fits_is_chosen_and_one_model_is_recommended() {
        let catalog = Catalog::embedded();
        let rows = price(
            &catalog,
            &select(LocalBackend::Auto, &machine(48 * GIB, 64 * GIB)),
        );
        let e4b = rows.iter().find(|r| r.id == "gemma-4-e4b").unwrap();
        assert_eq!(e4b.quant, "Q8_0");
        let recommended: Vec<_> = rows.iter().filter(|r| r.recommended).collect();
        assert_eq!(recommended.len(), 1, "{rows:#?}");
        assert_eq!(recommended[0].fit, Fit::FitsGpu);
        assert!(recommended[0].tokens_per_second >= super::SPEED_FLOOR);
    }

    #[test]
    fn nothing_is_recommended_without_a_gpu() {
        let mut cpu_only = machine(0, 16 * GIB);
        cpu_only.gpus.clear();
        let rows = price(&Catalog::embedded(), &select(LocalBackend::Auto, &cpu_only));
        assert!(rows.iter().all(|r| !r.recommended && r.fit != Fit::FitsGpu));
    }
}
