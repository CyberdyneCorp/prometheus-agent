//! Where a local model's memory goes and at which window it runs (spec:
//! local-models, Context window and memory placement).
//!
//! A model's memory at a window is its weights, the vision projector, the
//! KV cache for that window, the runtime's compute buffers and, for a
//! model with multi-token prediction, the draft buffers; both buffers
//! scale with the batch. [`launch`] picks the largest window (doubling from
//! [`MIN_WINDOW`] up to the native maximum) that fits entirely in GPU
//! memory, trying smaller batches before a smaller window, so
//! multi-token prediction is never switched off to make room. When even
//! [`MIN_WINDOW`] does not fit, the overflow goes to system RAM in the
//! least harmful order: the mixture-of-experts expert weights first, then
//! the dense layers; the KV cache never leaves the GPU. [`grow`] answers
//! whether a running model can move to a larger window when the
//! conversation needs one, or why not (so compression runs instead).

use serde::{Deserialize, Serialize};

use crate::catalog::{MIN_WINDOW, SPEED_FLOOR};
use crate::hardware::{CPU_BANDWIDTH_GBPS, GIB};

/// The batch sizes tried, largest first.
pub const BATCHES: [u32; 4] = [2048, 1024, 512, 256];
/// Compute buffers at the largest batch, besides 2% of the weights.
const BUFFERS_AT_MAX_BATCH: u64 = GIB / 2;
/// Multi-token prediction draft buffers at the largest batch.
const MTP_AT_MAX_BATCH: u64 = GIB / 4;
const EFFICIENCY: f64 = 0.6;

/// What a model needs, independent of the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    /// Weight bytes.
    pub weights: u64,
    /// Vision projector bytes.
    pub projector: u64,
    /// The share of `weights` that are mixture-of-experts expert tensors.
    pub experts: u64,
    /// KV cache bytes per token of window.
    pub kv_per_token: u64,
    /// The model predicts several tokens at once (draft buffers needed).
    pub mtp: bool,
    /// The native maximum window.
    pub native: u64,
    /// Bytes of weights read per generated token (active parameters).
    pub active_bytes: f64,
}

/// The memory a placement may use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// GPU (or unified) memory, bytes.
    pub gpu: u64,
    /// Free system RAM, bytes.
    pub ram: u64,
    /// GPU memory bandwidth, GB/s.
    pub gpu_bandwidth: f64,
}

/// Where a model runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// The context window, tokens.
    pub window: u64,
    /// The batch size.
    pub batch: u32,
    /// Weight bytes placed in system RAM.
    pub ram_bytes: u64,
    /// Whether only expert weights are in RAM.
    pub experts_only: bool,
    /// Bytes of dense (non-expert) weights in RAM.
    #[serde(default)]
    pub dense_ram_bytes: u64,
    /// Bytes of dense weights in all.
    #[serde(default)]
    pub dense_bytes: u64,
}

impl Placement {
    /// The llama-server preset lines of this placement for a model of
    /// `blocks` layers: the batch, the expert tensors in RAM
    /// (`cpu-moe`), and the layers kept on the GPU when dense weights
    /// spill too (`n-gpu-layers`; left to llama.cpp when the layer count
    /// is unknown).
    #[must_use]
    pub fn preset_lines(&self, blocks: Option<u64>) -> Vec<String> {
        let mut lines = vec![
            format!("batch-size = {}", self.batch),
            format!("ubatch-size = {}", self.batch.min(512)),
        ];
        if self.ram_bytes > 0 {
            lines.push("cpu-moe = true".to_owned());
        }
        if let (Some(blocks), true) = (blocks, self.dense_ram_bytes > 0 && self.dense_bytes > 0) {
            let moved = (blocks * self.dense_ram_bytes)
                .div_ceil(self.dense_bytes)
                .min(blocks);
            lines.push(format!("n-gpu-layers = {}", blocks - moved));
        }
        lines
    }
}

/// Why a model cannot grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocked {
    /// It already runs at its native maximum.
    NativeMax,
    /// A larger window fits nowhere.
    NoFit,
    /// A larger window fits only by spilling so much to RAM that
    /// generation falls below the speed floor.
    BelowSpeedFloor,
}

impl Blocked {
    /// The words a notice uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeMax => "native_max",
            Self::NoFit => "no_fit",
            Self::BelowSpeedFloor => "below_speed_floor",
        }
    }
}

/// Bytes `shape` needs at `window` with `batch`.
#[must_use]
pub fn need(shape: &Shape, window: u64, batch: u32) -> u64 {
    let scale = |at_max: u64| at_max * u64::from(batch) / u64::from(BATCHES[0]);
    let buffers = scale(BUFFERS_AT_MAX_BATCH) + shape.weights / 50;
    let mtp = if shape.mtp {
        scale(MTP_AT_MAX_BATCH) + shape.kv_per_token * u64::from(batch)
    } else {
        0
    };
    shape.weights + shape.projector + shape.kv_per_token * window + buffers + mtp
}

/// The largest batch with which `window` fits `gpu` entirely.
fn fitting_batch(shape: &Shape, window: u64, gpu: u64) -> Option<u32> {
    BATCHES
        .into_iter()
        .find(|batch| gpu > 0 && need(shape, window, *batch) <= gpu)
}

/// The window and batch a model launches with on `budget`: the largest
/// window that fits the GPU entirely, else [`MIN_WINDOW`] with the
/// overflow in RAM (experts first, never the KV cache); `None` when even
/// that does not fit.
#[must_use]
pub fn launch(shape: &Shape, budget: &Budget) -> Option<Placement> {
    let floor = MIN_WINDOW.min(shape.native);
    if fitting_batch(shape, floor, budget.gpu).is_none() {
        return spilled(shape, budget, floor);
    }
    let mut best = None;
    let mut window = floor;
    while window <= shape.native {
        let Some(batch) = fitting_batch(shape, window, budget.gpu) else {
            break;
        };
        best = Some(on_gpu(window, batch));
        window *= 2;
    }
    best
}

/// `window` with the smallest batch and the overflow in RAM: experts
/// first, then dense weights; the KV cache and buffers stay on the GPU.
fn spilled(shape: &Shape, budget: &Budget, window: u64) -> Option<Placement> {
    let batch = BATCHES[BATCHES.len() - 1];
    let overflow = need(shape, window, batch).saturating_sub(budget.gpu);
    let movable = shape.weights + shape.projector;
    if overflow > movable || overflow > budget.ram {
        return None;
    }
    let dense = shape.weights.saturating_sub(shape.experts);
    Some(Placement {
        window,
        batch,
        ram_bytes: overflow,
        experts_only: overflow <= shape.experts,
        dense_ram_bytes: overflow.saturating_sub(shape.experts).min(dense),
        dense_bytes: dense,
    })
}

/// Everything on the GPU.
const fn on_gpu(window: u64, batch: u32) -> Placement {
    Placement {
        window,
        batch,
        ram_bytes: 0,
        experts_only: true,
        dense_ram_bytes: 0,
        dense_bytes: 0,
    }
}

/// Predicted generation speed of `placement`, tokens per second: the
/// active weights read per token, the share in RAM at CPU bandwidth.
#[must_use]
#[allow(clippy::cast_precision_loss)] // byte counts to a ratio
pub fn speed(shape: &Shape, budget: &Budget, placement: &Placement) -> f64 {
    let gb = shape.active_bytes / 1e9;
    let ram_share = if shape.weights == 0 {
        0.0
    } else {
        (placement.ram_bytes as f64 / shape.weights as f64).min(1.0)
    };
    let seconds = gb * (1.0 - ram_share) / budget.gpu_bandwidth.max(1.0)
        + gb * ram_share / CPU_BANDWIDTH_GBPS;
    if seconds <= 0.0 {
        return f64::INFINITY;
    }
    EFFICIENCY / seconds
}

/// Whether a model running at `current` can move to a window holding
/// `needed` tokens: the next doubling at least that large (capped at the
/// native maximum), on the GPU when it fits, else spilled to RAM if
/// generation stays above the speed floor.
///
/// # Errors
///
/// Why it cannot.
pub fn grow(
    shape: &Shape,
    budget: &Budget,
    current: u64,
    needed: u64,
) -> Result<Placement, Blocked> {
    if current >= shape.native {
        return Err(Blocked::NativeMax);
    }
    let mut window = current.max(1) * 2;
    while window < needed && window < shape.native {
        window *= 2;
    }
    let window = window.min(shape.native);
    if let Some(batch) = fitting_batch(shape, window, budget.gpu) {
        return Ok(on_gpu(window, batch));
    }
    let placement = spilled(shape, budget, window).ok_or(Blocked::NoFit)?;
    if speed(shape, budget, &placement) < SPEED_FLOOR {
        return Err(Blocked::BelowSpeedFloor);
    }
    Ok(placement)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 30B mixture-of-experts model at 4 bits: 16 GiB of weights, 14 of
    /// them experts, 3B active.
    fn moe(mtp: bool) -> Shape {
        Shape {
            weights: 16 * GIB,
            projector: 0,
            experts: 14 * GIB,
            kv_per_token: 48 * 1024,
            mtp,
            native: 262_144,
            active_bytes: 1.5e9,
        }
    }

    fn budget(gpu: u64, ram: u64) -> Budget {
        Budget {
            gpu,
            ram,
            gpu_bandwidth: 400.0,
        }
    }

    #[test]
    fn the_largest_window_fitting_the_gpu_is_chosen() {
        let shape = moe(false);
        let placed = launch(&shape, &budget(24 * GIB, 32 * GIB)).unwrap();
        assert_eq!(placed.ram_bytes, 0);
        assert!(placed.window > MIN_WINDOW, "{placed:?}");
        assert!(need(&shape, placed.window, placed.batch) <= 24 * GIB);
        assert!(need(&shape, placed.window * 2, BATCHES[3]) > 24 * GIB);
    }

    #[test]
    fn multi_token_prediction_takes_a_smaller_batch_not_a_smaller_window() {
        let shape = moe(true);
        // Just enough for the window with MTP at batch 512, not 2048.
        let gpu = need(&shape, MIN_WINDOW, 512) + 1;
        let placed = launch(&shape, &budget(gpu, 32 * GIB)).unwrap();
        assert_eq!(
            (placed.window, placed.batch, placed.ram_bytes),
            (MIN_WINDOW, 512, 0)
        );
    }

    #[test]
    fn overflow_goes_to_ram_experts_first_and_the_kv_cache_never_moves() {
        let shape = moe(false);
        let placed = launch(&shape, &budget(10 * GIB, 32 * GIB)).unwrap();
        assert_eq!(placed.window, MIN_WINDOW);
        assert!(placed.ram_bytes > 0 && placed.experts_only, "{placed:?}");
        // The GPU keeps the whole KV cache and the buffers.
        let gpu_part = need(&shape, MIN_WINDOW, placed.batch) - placed.ram_bytes;
        assert!(gpu_part >= shape.kv_per_token * MIN_WINDOW);
        // Too little RAM: no placement at all.
        assert_eq!(launch(&shape, &budget(10 * GIB, GIB)), None);
    }

    #[test]
    fn a_spilled_placement_tells_llama_server_what_moves() {
        let gpu_only = launch(&moe(false), &budget(24 * GIB, 32 * GIB)).unwrap();
        assert_eq!(
            gpu_only.preset_lines(Some(48)),
            ["batch-size = 2048", "ubatch-size = 512"]
        );
        let experts = launch(&moe(false), &budget(10 * GIB, 32 * GIB)).unwrap();
        assert!(
            experts
                .preset_lines(Some(48))
                .contains(&"cpu-moe = true".to_owned())
        );
        assert!(
            !experts
                .preset_lines(Some(48))
                .iter()
                .any(|l| l.starts_with("n-gpu-layers"))
        );
        // A dense model spills layers: fewer stay on the GPU.
        let dense = Shape {
            experts: 0,
            ..moe(false)
        };
        let spilled = launch(&dense, &budget(12 * GIB, 32 * GIB)).unwrap();
        let lines = spilled.preset_lines(Some(48));
        let layers = lines
            .iter()
            .find_map(|l| l.strip_prefix("n-gpu-layers = "))
            .unwrap();
        assert!(layers.parse::<u64>().unwrap() < 48, "{lines:?}");
    }

    #[test]
    fn growth_doubles_toward_the_need_or_says_why_not() {
        let shape = moe(false);
        let roomy = budget(48 * GIB, 32 * GIB);
        let grown = grow(&shape, &roomy, 65_536, 70_000).unwrap();
        assert_eq!(grown.window, 131_072);
        assert_eq!(
            grow(&shape, &roomy, 262_144, 300_000),
            Err(Blocked::NativeMax)
        );
        assert_eq!(
            grow(&shape, &budget(17 * GIB, GIB), 65_536, 70_000),
            Err(Blocked::NoFit)
        );
        // Fits only with most of a dense model in RAM: too slow.
        let dense = Shape {
            experts: 0,
            active_bytes: 16.0 * 1.073_741_824e9,
            ..shape
        };
        assert_eq!(
            grow(&dense, &budget(12 * GIB, 64 * GIB), 65_536, 70_000),
            Err(Blocked::BelowSpeedFloor)
        );
    }
}
