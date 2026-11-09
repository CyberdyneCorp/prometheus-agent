//! The machine the local runtime runs on (spec: local-models, Backend
//! auto-selection; Hardware-priced model catalog): host platform, system
//! RAM and the GPUs, each with its memory and what other programs hold.
//!
//! [`probe`] asks the OS (`nvidia-smi`, `/sys/class/drm`, the Windows video
//! controller list, the macOS chip name); the parsers it uses are pure
//! functions over the tools' output so they are tested with recorded
//! fixtures. A [`Hardware`] value can also be read from JSON, which is how
//! tests and `prometheus local` fixtures pin a machine.

mod probe;

use serde::{Deserialize, Serialize};

pub use probe::{cuda_version, nvidia_gpus, probe, sysfs_gpu, windows_gpus};

/// One GiB.
pub const GIB: u64 = 1 << 30;

/// Who made a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vendor {
    /// NVIDIA.
    Nvidia,
    /// AMD.
    Amd,
    /// Intel.
    Intel,
    /// Apple silicon (or any macOS GPU, driven through Metal).
    Apple,
}

/// One GPU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    /// Its maker.
    pub vendor: Vendor,
    /// Marketing name.
    pub name: String,
    /// Its own memory in bytes (an integrated GPU: 0, it uses system RAM).
    #[serde(default)]
    pub memory_total: u64,
    /// Bytes of that memory other programs hold now.
    #[serde(default)]
    pub memory_used: u64,
    /// Shares system RAM (Apple silicon, most Intel GPUs, AMD APUs).
    #[serde(default)]
    pub integrated: bool,
    /// The newest CUDA major version the NVIDIA driver supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cuda: Option<u32>,
    /// Memory bandwidth in GB/s, when known (drives the speed estimate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_gbps: Option<f64>,
}

/// The probed machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hardware {
    /// `<os>-<arch>[-musl]` as the runtime store names platforms.
    pub platform: String,
    /// System RAM in bytes.
    pub ram_total: u64,
    /// RAM other programs leave free, in bytes.
    pub ram_available: u64,
    /// The GPUs found.
    #[serde(default)]
    pub gpus: Vec<Gpu>,
}

impl Hardware {
    /// The OS part of the platform (`darwin`, `linux`, `windows`).
    #[must_use]
    pub fn os(&self) -> &str {
        self.platform.split('-').next().unwrap_or_default()
    }

    /// Whether the CPU is `x86_64`.
    #[must_use]
    pub fn is_x64(&self) -> bool {
        self.platform.split('-').nth(1) == Some("x64")
    }

    /// Reads a machine description written as JSON (test fixtures).
    ///
    /// # Errors
    ///
    /// The parse error.
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
}

impl Gpu {
    /// Bytes a model may use on this GPU: a discrete GPU's own memory
    /// minus what other programs hold (and a small reserve for the
    /// display), an integrated GPU's share of the RAM other programs
    /// leave free (macOS lets the GPU wire about three quarters of RAM).
    #[must_use]
    pub fn budget(&self, hardware: &Hardware) -> u64 {
        if self.integrated {
            let share = hardware.ram_total / 4 * 3;
            return share.min(hardware.ram_available);
        }
        self.memory_total
            .saturating_sub(self.memory_used)
            .saturating_sub(GIB / 2)
    }

    /// Memory bandwidth in GB/s: the probed value, else a conservative
    /// figure by kind (the catalog's speed floor needs an estimate, not a
    /// benchmark).
    #[must_use]
    pub fn bandwidth(&self) -> f64 {
        if let Some(known) = self.bandwidth_gbps {
            return known;
        }
        match (self.vendor, self.integrated) {
            (Vendor::Apple, _) => apple_bandwidth(&self.name),
            (_, true) => 90.0,
            (Vendor::Nvidia, false) => nvidia_bandwidth(&self.name),
            (Vendor::Amd | Vendor::Intel, false) => 450.0,
        }
    }
}

/// Memory bandwidth of system RAM when a model runs on the CPU.
pub const CPU_BANDWIDTH_GBPS: f64 = 60.0;

fn apple_bandwidth(chip: &str) -> f64 {
    let tier = if chip.contains("Ultra") {
        800.0
    } else if chip.contains("Max") {
        400.0
    } else if chip.contains("Pro") {
        200.0
    } else {
        100.0
    };
    // M4 and later raised every tier; the base figures stay conservative.
    if ["M4", "M5"].iter().any(|g| chip.contains(g)) {
        tier * 1.2
    } else {
        tier
    }
}

fn nvidia_bandwidth(name: &str) -> f64 {
    const KNOWN: &[(&str, f64)] = &[
        ("H200", 4800.0),
        ("H100", 3350.0),
        ("A100", 1555.0),
        ("5090", 1790.0),
        ("4090", 1008.0),
        ("3090", 936.0),
        ("5080", 960.0),
        ("4080", 717.0),
        ("3080", 760.0),
        ("5070", 672.0),
        ("4070", 504.0),
        ("3070", 448.0),
        ("4060", 272.0),
        ("3060", 360.0),
        ("L40", 864.0),
        ("L4", 300.0),
        ("T4", 320.0),
    ];
    KNOWN
        .iter()
        .find(|(model, _)| name.contains(model))
        .map_or(400.0, |(_, gbps)| *gbps)
}

#[cfg(test)]
mod tests {
    use super::{GIB, Gpu, Hardware, Vendor};

    fn machine(gpus: Vec<Gpu>) -> Hardware {
        Hardware {
            platform: "linux-x64".to_owned(),
            ram_total: 32 * GIB,
            ram_available: 20 * GIB,
            gpus,
        }
    }

    #[test]
    fn a_discrete_gpu_is_sized_from_its_own_memory_minus_what_others_hold() {
        let gpu = Gpu {
            vendor: Vendor::Amd,
            name: "Radeon RX 7600".to_owned(),
            memory_total: 8 * GIB,
            memory_used: GIB,
            integrated: false,
            cuda: None,
            bandwidth_gbps: None,
        };
        assert_eq!(gpu.budget(&machine(vec![])), 7 * GIB - GIB / 2);
    }

    #[test]
    fn an_integrated_gpu_is_sized_from_system_ram() {
        let gpu = Gpu {
            vendor: Vendor::Intel,
            name: "Iris Xe".to_owned(),
            memory_total: 0,
            memory_used: 0,
            integrated: true,
            cuda: None,
            bandwidth_gbps: None,
        };
        // Three quarters of 32 GiB is 24 GiB, but only 20 GiB are free.
        assert_eq!(gpu.budget(&machine(vec![])), 20 * GIB);
    }
}
