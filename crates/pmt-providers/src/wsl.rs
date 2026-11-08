//! WSL2 local endpoint diagnostics (spec: model-providers, WSL2 local
//! endpoint diagnostics): under WSL2's default NAT networking a local
//! server listening on the Windows host (Ollama, LM Studio) is not on the
//! Linux loopback. When a loopback endpoint does not answer there, the
//! advice names WSL mirrored networking and the Windows host address
//! from the default route.

use std::net::Ipv4Addr;

use pmt_config::provider_urls::{host_of, is_local_host};

/// What matters about running under WSL2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wsl {
    /// NAT networking (the default), not mirrored.
    pub nat: bool,
    /// The Windows host: the default route's gateway.
    pub host_ip: Option<Ipv4Addr>,
}

/// The running system's WSL2 facts; `None` outside WSL2.
#[must_use]
pub fn detect() -> Option<Wsl> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
    if !is_wsl2(&release) {
        return None;
    }
    let mode = std::process::Command::new("wslinfo")
        .arg("--networking-mode")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .to_ascii_lowercase()
        });
    let routes = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    Some(Wsl {
        // Without `wslinfo` (older WSL) the mode is the default, NAT.
        nat: mode.as_deref().is_none_or(|m| m == "nat"),
        host_ip: default_gateway(&routes),
    })
}

/// Whether a kernel release string is a WSL2 kernel
/// (`...-microsoft-standard-WSL2`).
#[must_use]
pub fn is_wsl2(release: &str) -> bool {
    let release = release.to_ascii_lowercase();
    release.contains("microsoft") && release.contains("wsl2")
}

/// The gateway of the default route in a `/proc/net/route` table.
#[must_use]
pub fn default_gateway(table: &str) -> Option<Ipv4Addr> {
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (destination, gateway) = (fields.get(1)?, fields.get(2)?);
        if *destination != "00000000" {
            return None;
        }
        // Little-endian hex, as the kernel prints it.
        let raw = u32::from_str_radix(gateway, 16).ok()?;
        Some(Ipv4Addr::from(raw.swap_bytes()))
    })
}

/// The advice for a loopback `base_url` that did not answer under `wsl`;
/// `None` for mirrored networking or a non-loopback endpoint.
#[must_use]
pub fn advice(wsl: &Wsl, base_url: &str) -> Option<String> {
    let host = host_of(base_url)?;
    if !wsl.nat || !is_local_host(&host) || !is_loopback(&host) {
        return None;
    }
    let port = url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.port_or_known_default());
    let windows = match (wsl.host_ip, port) {
        (Some(ip), Some(port)) => {
            format!(" or point the provider at the Windows host: http://{ip}:{port}")
        }
        (Some(ip), None) => format!(" or point the provider at the Windows host {ip}"),
        _ => String::new(),
    };
    Some(format!(
        "under WSL2 NAT networking a server on Windows is not on this loopback: enable mirrored networking (networkingMode=mirrored in .wslconfig){windows}"
    ))
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
eth0\t00000000\t0100A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
eth0\t0000A8C0\t00000000\t0001\t0\t0\t0\t00F0FFFF\t0\t0\t0\n";

    #[test]
    fn the_windows_host_is_the_default_gateway() {
        assert_eq!(default_gateway(ROUTES), Some(Ipv4Addr::new(192, 168, 0, 1)));
        assert_eq!(default_gateway("Iface\tDestination\tGateway\n"), None);
        assert!(is_wsl2("5.15.167.4-microsoft-standard-WSL2"));
        assert!(!is_wsl2("6.8.0-45-generic"));
    }

    #[test]
    fn advice_names_mirrored_networking_and_the_host_for_loopback_only() {
        let wsl = Wsl {
            nat: true,
            host_ip: Some(Ipv4Addr::new(172, 29, 64, 1)),
        };
        let text = advice(&wsl, "http://127.0.0.1:11434/v1").unwrap();
        assert!(text.contains("mirrored networking"), "{text}");
        assert!(text.contains("http://172.29.64.1:11434"), "{text}");
        assert!(advice(&wsl, "http://192.168.1.20:11434/v1").is_none());
        let mirrored = Wsl { nat: false, ..wsl };
        assert!(advice(&mirrored, "http://localhost:11434/v1").is_none());
    }
}
