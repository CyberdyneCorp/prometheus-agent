//! Probing the real machine. Every OS tool is optional: a missing
//! `nvidia-smi` means no NVIDIA GPU, an unreadable sysfs entry is skipped,
//! and a machine where nothing answers is a CPU-only machine.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use super::{GIB, Gpu, Hardware, Vendor};

const MIB: u64 = 1 << 20;

/// Probes this machine.
#[must_use]
pub fn probe() -> Hardware {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let platform = pmt_update::runtimes::platform();
    let mut hardware = Hardware {
        platform,
        ram_total: system.total_memory(),
        ram_available: system.available_memory(),
        gpus: Vec::new(),
    };
    hardware.gpus = match hardware.os() {
        "darwin" => vec![apple_gpu(&hardware)],
        "linux" => linux_gpus(),
        "windows" => {
            let mut gpus = nvidia();
            gpus.extend(
                run("powershell", &["-NoProfile", "-Command", WINDOWS_QUERY])
                    .map(|out| windows_gpus(&out))
                    .unwrap_or_default(),
            );
            gpus
        }
        _ => Vec::new(),
    };
    hardware
}

/// macOS: one GPU driven through Metal that shares system RAM.
fn apple_gpu(hardware: &Hardware) -> Gpu {
    let name = run("sysctl", &["-n", "machdep.cpu.brand_string"])
        .map_or_else(|| "Apple GPU".to_owned(), |s| s.trim().to_owned());
    Gpu {
        vendor: Vendor::Apple,
        name,
        memory_total: hardware.ram_total,
        memory_used: 0,
        integrated: true,
        cuda: None,
        bandwidth_gbps: None,
    }
}

fn linux_gpus() -> Vec<Gpu> {
    let mut gpus = nvidia();
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return gpus;
    };
    let mut names: Vec<_> = cards
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("card") && !n.contains('-'))
        .collect();
    names.sort();
    gpus.extend(
        names
            .iter()
            .filter_map(|n| sysfs_gpu(&Path::new("/sys/class/drm").join(n).join("device"))),
    );
    gpus
}

/// NVIDIA GPUs from `nvidia-smi`, with the driver's CUDA version.
fn nvidia() -> Vec<Gpu> {
    let Some(csv) = run(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,memory.used",
            "--format=csv,noheader,nounits",
        ],
    ) else {
        return Vec::new();
    };
    let cuda = run("nvidia-smi", &[]).and_then(|banner| cuda_version(&banner));
    nvidia_gpus(&csv, cuda)
}

/// Parses `nvidia-smi --query-gpu=name,memory.total,memory.used
/// --format=csv,noheader,nounits` (MiB values).
#[must_use]
pub fn nvidia_gpus(csv: &str, cuda: Option<u32>) -> Vec<Gpu> {
    csv.lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let name = fields.next().filter(|n| !n.is_empty())?;
            let total: u64 = fields.next()?.parse().ok()?;
            let used: u64 = fields.next()?.parse().ok()?;
            Some(Gpu {
                vendor: Vendor::Nvidia,
                name: name.to_owned(),
                memory_total: total * MIB,
                memory_used: used * MIB,
                integrated: false,
                cuda,
                bandwidth_gbps: None,
            })
        })
        .collect()
}

/// The `CUDA Version: 13.0` major number of the `nvidia-smi` banner.
#[must_use]
pub fn cuda_version(banner: &str) -> Option<u32> {
    let rest = banner.split("CUDA Version:").nth(1)?.trim_start();
    let major = rest.split(['.', ' ', '|']).next()?;
    major.parse().ok()
}

/// An AMD or Intel GPU from its sysfs `device` directory (NVIDIA cards are
/// left to `nvidia-smi`). AMD reports its VRAM (`mem_info_vram_*`); a card
/// with less than 2 GiB of it is an APU carve-out and counts as
/// integrated, as does an Intel GPU without dedicated memory.
#[must_use]
pub fn sysfs_gpu(device: &Path) -> Option<Gpu> {
    let read = |name: &str| std::fs::read_to_string(device.join(name)).ok();
    let number = |name: &str| read(name).and_then(|s| s.trim().parse::<u64>().ok());
    let vendor = match read("vendor")?.trim() {
        "0x1002" => Vendor::Amd,
        "0x8086" => Vendor::Intel,
        _ => return None,
    };
    let total = number("mem_info_vram_total").unwrap_or(0);
    let used = number("mem_info_vram_used").unwrap_or(0);
    let integrated = total < 2 * GIB;
    let name = read("product_name")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            match vendor {
                Vendor::Amd => "AMD GPU",
                _ => "Intel GPU",
            }
            .to_owned()
        });
    Some(Gpu {
        vendor,
        name,
        memory_total: if integrated { 0 } else { total },
        memory_used: if integrated { 0 } else { used },
        integrated,
        cuda: None,
        bandwidth_gbps: None,
    })
}

const WINDOWS_QUERY: &str = "Get-CimInstance Win32_VideoController | Select-Object Name,AdapterCompatibility,AdapterRAM | ConvertTo-Json";

/// AMD and Intel GPUs from the Windows video controller list (NVIDIA ones
/// come from `nvidia-smi`). `AdapterRAM` is a 32-bit value, so a card
/// reporting 4 GiB or more is taken at 4 GiB; under 2 GiB is integrated.
#[must_use]
pub fn windows_gpus(json: &str) -> Vec<Gpu> {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let rows = match value {
        Value::Array(rows) => rows,
        row @ Value::Object(_) => vec![row],
        _ => Vec::new(),
    };
    rows.iter()
        .filter_map(|row| {
            let maker = row.get("AdapterCompatibility")?.as_str()?.to_lowercase();
            let vendor = if maker.contains("amd") || maker.contains("advanced micro") {
                Vendor::Amd
            } else if maker.contains("intel") {
                Vendor::Intel
            } else {
                return None;
            };
            let ram = row.get("AdapterRAM").and_then(Value::as_u64).unwrap_or(0);
            let integrated = ram < 2 * GIB;
            Some(Gpu {
                vendor,
                name: row.get("Name")?.as_str()?.to_owned(),
                memory_total: if integrated { 0 } else { ram },
                memory_used: 0,
                integrated,
                cuda: None,
                bandwidth_gbps: None,
            })
        })
        .collect()
}

/// A tool's stdout when it ran and succeeded.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{GIB, Vendor, cuda_version, nvidia_gpus, sysfs_gpu, windows_gpus};

    #[test]
    fn nvidia_smi_rows_and_the_cuda_banner_are_read() {
        let gpus = nvidia_gpus(
            "NVIDIA GeForce RTX 4070, 12282, 1024\nNVIDIA L4, 23034, 0\n",
            Some(13),
        );
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].memory_total, 12_282 << 20);
        assert_eq!(gpus[0].memory_used, 1024 << 20);
        assert_eq!(gpus[1].cuda, Some(13));
        let banner =
            "| NVIDIA-SMI 580.65.06   Driver Version: 580.65.06   CUDA Version: 13.0     |";
        assert_eq!(cuda_version(banner), Some(13));
        assert_eq!(cuda_version("no driver"), None);
    }

    #[test]
    fn an_amd_card_in_sysfs_is_a_discrete_gpu_with_its_vram() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path();
        std::fs::write(device.join("vendor"), "0x1002\n").unwrap();
        std::fs::write(
            device.join("mem_info_vram_total"),
            format!("{}\n", 16 * GIB),
        )
        .unwrap();
        std::fs::write(device.join("mem_info_vram_used"), format!("{GIB}\n")).unwrap();
        let gpu = sysfs_gpu(device).unwrap();
        assert_eq!(gpu.vendor, Vendor::Amd);
        assert!(!gpu.integrated);
        assert_eq!(gpu.memory_total, 16 * GIB);
        std::fs::write(device.join("vendor"), "0x10de\n").unwrap();
        assert!(sysfs_gpu(device).is_none(), "NVIDIA is nvidia-smi's");
    }

    #[test]
    fn windows_controllers_keep_amd_and_intel_only() {
        let json = r#"[{"Name":"AMD Radeon RX 7800 XT","AdapterCompatibility":"Advanced Micro Devices, Inc.","AdapterRAM":4293918720},
                      {"Name":"Intel(R) UHD Graphics","AdapterCompatibility":"Intel Corporation","AdapterRAM":1073741824},
                      {"Name":"NVIDIA GeForce RTX 4090","AdapterCompatibility":"NVIDIA","AdapterRAM":4293918720}]"#;
        let gpus = windows_gpus(json);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].vendor, Vendor::Amd);
        assert!(!gpus[0].integrated);
        assert!(gpus[1].integrated);
    }
}
