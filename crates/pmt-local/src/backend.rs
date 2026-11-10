//! Backend selection (spec: local-models, Backend auto-selection):
//! `backend = "auto"` resolves from the probed hardware, an explicit
//! backend is taken as configured, and the GPU the models will run on is
//! sized for the catalog.
//!
//! Auto picks CUDA for an NVIDIA GPU whose driver supports CUDA 13 on Linux
//! (`x86_64` and `aarch64`) and Windows, Metal on macOS, Vulkan for AMD and
//! Intel GPUs (and NVIDIA GPUs without a CUDA 13 driver) on Linux and
//! Windows `x86_64`, and the CPU otherwise. HIP is never chosen
//! automatically.

use pmt_config::daemon_local::LocalBackend;
use serde::Serialize;

use crate::hardware::{Gpu, Hardware, Vendor};

/// The CUDA major version the pinned CUDA builds need.
pub const CUDA_MAJOR: u32 = 13;

/// The resolved backend and the GPU it runs on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Selection {
    /// Never `auto`.
    pub backend: LocalBackend,
    /// The GPU models are placed on (`None`: CPU).
    pub gpu: Option<Gpu>,
    /// Bytes a model may use on that GPU (0 on the CPU).
    pub gpu_budget: u64,
    /// RAM other programs leave free, for spilled weights.
    pub ram_budget: u64,
}

/// Resolves `configured` on `hardware`.
#[must_use]
pub fn select(configured: LocalBackend, hardware: &Hardware) -> Selection {
    let backend = match configured {
        LocalBackend::Auto => auto(hardware),
        other => other,
    };
    let gpu = gpu_for(backend, hardware).cloned();
    let gpu_budget = gpu.as_ref().map_or(0, |g| g.budget(hardware));
    // Unified memory is one pool: what the GPU takes is not left for a
    // spill.
    let shared = gpu.as_ref().is_some_and(|g| g.integrated);
    let ram_budget = if shared {
        hardware.ram_available.saturating_sub(gpu_budget)
    } else {
        hardware.ram_available
    } / 10
        * 8;
    Selection {
        backend,
        gpu,
        gpu_budget,
        ram_budget,
    }
}

fn auto(hardware: &Hardware) -> LocalBackend {
    let os = hardware.os();
    if os == "darwin" {
        return LocalBackend::Metal;
    }
    if !matches!(os, "linux" | "windows") || hardware.platform.ends_with("-musl") {
        return LocalBackend::Cpu;
    }
    let cuda = hardware
        .gpus
        .iter()
        .any(|g| g.vendor == Vendor::Nvidia && g.cuda.is_some_and(|v| v >= CUDA_MAJOR));
    if cuda {
        return LocalBackend::Cuda;
    }
    if hardware.is_x64() && !hardware.gpus.is_empty() {
        return LocalBackend::Vulkan;
    }
    LocalBackend::Cpu
}

/// The GPU `backend` drives: the one with the most usable memory among
/// those it supports.
fn gpu_for(backend: LocalBackend, hardware: &Hardware) -> Option<&Gpu> {
    let drives = |g: &&Gpu| match backend {
        LocalBackend::Cuda => g.vendor == Vendor::Nvidia,
        LocalBackend::Metal => g.vendor == Vendor::Apple,
        LocalBackend::Hip => g.vendor == Vendor::Amd,
        LocalBackend::Vulkan => g.vendor != Vendor::Apple,
        LocalBackend::Cpu | LocalBackend::Auto => false,
    };
    hardware
        .gpus
        .iter()
        .filter(drives)
        .max_by_key(|g| g.budget(hardware))
}

#[cfg(test)]
mod tests {
    use pmt_config::daemon_local::LocalBackend;

    use super::select;
    use crate::hardware::{GIB, Gpu, Hardware, Vendor};

    fn gpu(vendor: Vendor, memory: u64, cuda: Option<u32>) -> Gpu {
        Gpu {
            vendor,
            name: "gpu".to_owned(),
            memory_total: memory,
            memory_used: 0,
            integrated: memory == 0,
            cuda,
            bandwidth_gbps: None,
        }
    }

    fn machine(platform: &str, gpus: Vec<Gpu>) -> Hardware {
        Hardware {
            platform: platform.to_owned(),
            ram_total: 32 * GIB,
            ram_available: 24 * GIB,
            gpus,
        }
    }

    #[test]
    fn an_amd_gpu_on_linux_selects_vulkan() {
        let hw = machine("linux-x64", vec![gpu(Vendor::Amd, 16 * GIB, None)]);
        let chosen = select(LocalBackend::Auto, &hw);
        assert_eq!(chosen.backend, LocalBackend::Vulkan);
        assert_eq!(chosen.gpu_budget, 16 * GIB - GIB / 2);
    }

    #[test]
    fn auto_follows_the_hardware_table() {
        let nvidia13 = gpu(Vendor::Nvidia, 8 * GIB, Some(13));
        let nvidia12 = gpu(Vendor::Nvidia, 8 * GIB, Some(12));
        let cases = [
            (
                machine("linux-x64", vec![nvidia13.clone()]),
                LocalBackend::Cuda,
            ),
            (
                machine("linux-arm64", vec![nvidia13.clone()]),
                LocalBackend::Cuda,
            ),
            (machine("windows-x64", vec![nvidia13]), LocalBackend::Cuda),
            (
                machine("linux-x64", vec![nvidia12.clone()]),
                LocalBackend::Vulkan,
            ),
            (machine("linux-arm64", vec![nvidia12]), LocalBackend::Cpu),
            (
                machine("windows-x64", vec![gpu(Vendor::Intel, 0, None)]),
                LocalBackend::Vulkan,
            ),
            (
                machine("darwin-arm64", vec![gpu(Vendor::Apple, 0, None)]),
                LocalBackend::Metal,
            ),
            (machine("linux-x64", vec![]), LocalBackend::Cpu),
        ];
        for (hw, expected) in cases {
            assert_eq!(select(LocalBackend::Auto, &hw).backend, expected, "{hw:?}");
        }
    }

    #[test]
    fn hip_is_only_ever_configured() {
        let hw = machine("linux-x64", vec![gpu(Vendor::Amd, 16 * GIB, None)]);
        assert_ne!(select(LocalBackend::Auto, &hw).backend, LocalBackend::Hip);
        let hip = select(LocalBackend::Hip, &hw);
        assert_eq!(hip.backend, LocalBackend::Hip);
        assert!(hip.gpu.is_some());
    }
}
