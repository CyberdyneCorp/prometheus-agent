//! Hardline commands: blocked under every approval mode, backend and
//! context, before any rule, allowlist or bypass is looked at (spec:
//! tool-system, Approval floors cannot be bypassed).
//!
//! Recursive deletes of `/`, the user home or a system directory, `mkfs`
//! and other filesystem creation, writes to raw block devices, fork bombs,
//! `kill -1`, shutdown/reboot/halt/poweroff, and deleting the running
//! Prometheus binary or home.

use std::sync::LazyLock;

use regex::Regex;

use super::Floors;
use super::paths::{is_ancestor_or_same, resolve};
use super::shell::{Script, Segment};

/// Directories whose recursive delete is hardline (lower case, `/`
/// separators; matched after resolving the target).
const SYSTEM_DIRS: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib32",
    "/lib64",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/srv",
    "/sys",
    "/usr",
    "/var",
    "/applications",
    "/library",
    "/private",
    "/system",
    "/users",
    "c:/windows",
    "c:/program files",
    "c:/program files (x86)",
    "c:/programdata",
    "c:/users",
];

/// Raw block devices (`/dev/sda`, `/dev/nvme0n1`, `/dev/disk2`...).
static BLOCK_DEVICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(/dev/(sd[a-z]|hd[a-z]|vd[a-z]|xvd[a-z]|nvme\d|mmcblk\d|r?disk\d|md\d|dm-\d|loop\d|sr\d|mapper/)|\\\\\.\\physicaldrive\d)",
    )
    .expect("valid regex")
});

/// `name(){ name|name& }` (any function name, `:` included).
static FORK_BOMB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([\w:.]+)\s*\(\)\s*\{\s*([\w:.]+)\s*\|\s*([\w:.]+)\s*&\s*;?\s*\}")
        .expect("valid regex")
});

/// The hardline reason for `line`, if any.
#[must_use]
pub fn check(line: &str, script: &Script, floors: &Floors) -> Option<String> {
    if fork_bomb(line) {
        return Some("fork bomb".to_owned());
    }
    script.segments.iter().find_map(|s| segment(s, floors))
}

fn fork_bomb(line: &str) -> bool {
    FORK_BOMB
        .captures_iter(line)
        .any(|c| c[1] == c[2] && c[2] == c[3])
}

fn segment(segment: &Segment, floors: &Floors) -> Option<String> {
    if let Some(device) = raw_device_write(segment) {
        return Some(format!("write to raw block device {device}"));
    }
    let (program, args) = segment.program()?;
    let lower = program.to_ascii_lowercase();
    match lower.as_str() {
        "rm" | "unlink" | "remove-item" | "rmdir" | "rd" | "del" | "erase" | "ri" => {
            delete(&lower, args, floors)
        }
        "shutdown" | "reboot" | "halt" | "poweroff" => {
            Some(format!("system shutdown or reboot ({program})"))
        }
        "systemctl" => args
            .iter()
            .find(|a| matches!(a.as_str(), "poweroff" | "reboot" | "halt" | "kexec"))
            .map(|verb| format!("system shutdown or reboot (systemctl {verb})")),
        "init" | "telinit" => args
            .first()
            .filter(|a| matches!(a.as_str(), "0" | "6"))
            .map(|level| format!("system shutdown or reboot ({program} {level})")),
        "kill" => kill_all(args).then(|| "kill of every process (kill -1)".to_owned()),
        "diskutil"
            if args.first().is_some_and(|verb| {
                let verb = verb.to_ascii_lowercase();
                verb.starts_with("erase")
                    || verb.ends_with("disk") && verb != "listdisk" && verb != "infodisk"
            }) =>
        {
            Some(format!("filesystem creation (diskutil {})", args[0]))
        }
        _ if lower.starts_with("mkfs") || lower == "mke2fs" || lower.starts_with("newfs") => {
            Some(format!("filesystem creation ({program})"))
        }
        _ => None,
    }
}

/// A `>` redirection, `dd of=` or `tee` onto a raw block device.
fn raw_device_write(segment: &Segment) -> Option<String> {
    let redirected = segment
        .redirects
        .iter()
        .filter(|r| r.writes())
        .map(|r| r.target.as_str());
    let (program, args) = segment.program().unwrap_or(("", &[]));
    let named: Vec<&str> = match program {
        "dd" => args.iter().filter_map(|a| a.strip_prefix("of=")).collect(),
        "tee" | "cp" | "cat" => args.iter().map(String::as_str).collect(),
        _ => Vec::new(),
    };
    redirected
        .chain(named)
        .find(|target| BLOCK_DEVICE.is_match(target))
        .map(str::to_owned)
}

/// `kill -1`, or `-1` among the pids after the signal option.
fn kill_all(args: &[String]) -> bool {
    if args.len() == 1 && args[0] == "-1" {
        return true;
    }
    let mut pids = args.iter().peekable();
    if let Some(first) = pids.peek() {
        if matches!(first.as_str(), "-s" | "-n" | "--signal") {
            pids.next();
            pids.next();
        } else if first.starts_with('-') && first.as_str() != "-1" && first.as_str() != "--" {
            pids.next();
        } else if first.as_str() == "-1" {
            // `kill -1 <pid>` sends signal 1 to <pid>.
            pids.next();
        }
    }
    pids.any(|pid| pid == "-1")
}

/// Recursive deletes of `/`, the home or a system directory, and any
/// delete reaching the Prometheus home or binary.
fn delete(program: &str, args: &[String], floors: &Floors) -> Option<String> {
    let recursive = recursive_delete(program, args);
    let mut options_done = false;
    for arg in args {
        if !options_done && arg == "--" {
            options_done = true;
            continue;
        }
        if !options_done && is_option(program, arg) {
            continue;
        }
        let target = resolve(arg, floors);
        if recursive && let Some(reason) = protected_tree(&target, floors) {
            return Some(reason);
        }
        if let Some(reason) = prometheus_target(&target, floors) {
            return Some(reason);
        }
    }
    None
}

fn is_option(program: &str, arg: &str) -> bool {
    arg.starts_with('-') && arg.len() > 1
        || matches!(program, "rmdir" | "rd" | "del" | "erase")
            && arg.starts_with('/')
            && arg.len() == 2
}

fn recursive_delete(program: &str, args: &[String]) -> bool {
    args.iter().any(|a| {
        let lower = a.to_ascii_lowercase();
        match program {
            "rmdir" | "rd" | "del" | "erase" => lower == "/s",
            "remove-item" | "ri" => lower.len() >= 2 && "-recurse".starts_with(&lower),
            _ => {
                lower == "--recursive"
                    || lower == "-recurse"
                    || (a.starts_with('-')
                        && !a.starts_with("--")
                        && (a.contains('r') || a.contains('R')))
            }
        }
    })
}

fn protected_tree(target: &str, floors: &Floors) -> Option<String> {
    if target == "/" || is_drive_root(target) {
        return Some("recursive delete of root filesystem".to_owned());
    }
    if floors.user_home.as_deref() == Some(target) {
        return Some("recursive delete of the home directory".to_owned());
    }
    SYSTEM_DIRS
        .iter()
        .find(|dir| target.eq_ignore_ascii_case(dir))
        .map(|dir| format!("recursive delete of system directory {dir}"))
}

fn is_drive_root(target: &str) -> bool {
    let bytes = target.as_bytes();
    bytes.len() <= 3 && bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn prometheus_target(target: &str, floors: &Floors) -> Option<String> {
    if let Some(home) = &floors.prometheus_home
        && is_ancestor_or_same(target, home)
    {
        return Some("deletion of the Prometheus home".to_owned());
    }
    if let Some(binary) = &floors.binary
        && is_ancestor_or_same(target, binary)
    {
        return Some("deletion of the running Prometheus binary".to_owned());
    }
    None
}
