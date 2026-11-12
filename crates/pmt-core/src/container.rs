//! Whether this process runs inside a container (spec: distribution-and-
//! update, Container audio bridge and host services): Docker leaves
//! `/.dockerenv`, Podman `/run/.containerenv`, and systemd-nspawn, LXC and
//! Podman set `container` in PID 1's environment (inherited here). Used to
//! give container-specific advice (a provider on `localhost` is the
//! container's own loopback) and to seed the image's runtimes.

use std::path::Path;

/// Whether this process runs in a container.
#[must_use]
pub fn in_container() -> bool {
    cfg!(target_os = "linux")
        && (Path::new("/.dockerenv").exists()
            || Path::new("/run/.containerenv").exists()
            || std::env::var_os("container").is_some_and(|v| !v.is_empty()))
}
