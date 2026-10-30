//! The Landlock ruleset of one child: path rules from the policy roots with
//! credential stores carved out, TCP port rules and signal scoping.
//!
//! Landlock only grants, so a denied store below a granted root is carved
//! out: the root is replaced by its entries minus the store, recursively
//! along the path to the store, and the directories on that path keep only
//! `ReadDir`. Entries created in those directories after the child starts
//! are therefore not reachable; everything else below the root is.

use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, NetPort, PathBeneath,
    PathFd, Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr, Scope,
};

use crate::policy::{Policy, Ports, is_strictly_below};

/// The newest Landlock ABI this kernel fully supports, or `None` without
/// Landlock.
pub(super) fn kernel_abi() -> Option<ABI> {
    [ABI::V6, ABI::V5, ABI::V4, ABI::V3, ABI::V2, ABI::V1]
        .into_iter()
        .find(|abi| supports(*abi))
}

fn supports(abi: ABI) -> bool {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .and_then(Ruleset::create)
        .is_ok()
}

/// Device files every child may use.
const DEVICES_RW: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/tty"];
const DEVICES_RO: &[&str] = &["/dev/random", "/dev/urandom"];

/// One path rule.
struct PathRule {
    path: PathBuf,
    access: BitFlags<AccessFs>,
}

/// Builds the ruleset for `policy` on a kernel with `abi`.
pub(super) fn ruleset(policy: &Policy, abi: ABI) -> Result<RulesetCreated, String> {
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?;
    if abi >= ABI::V4 {
        let mut net = BitFlags::<AccessNet>::empty();
        if !policy.connect().is_all() {
            net |= AccessNet::ConnectTcp;
        }
        if !policy.bind().is_all() {
            net |= AccessNet::BindTcp;
        }
        if !net.is_empty() {
            ruleset = ruleset.handle_access(net).map_err(|e| e.to_string())?;
        }
    }
    if abi >= ABI::V6 {
        ruleset = ruleset
            .scope(Scope::from_all(abi))
            .map_err(|e| e.to_string())?;
    }
    let mut created = ruleset.create().map_err(|e| e.to_string())?;
    for rule in path_rules(policy, abi) {
        created = add_path(created, &rule, abi)?;
    }
    if abi >= ABI::V4 {
        created = add_ports(created, policy.connect(), AccessNet::ConnectTcp)?;
        created = add_ports(created, policy.bind(), AccessNet::BindTcp)?;
    }
    Ok(created)
}

fn add_path(created: RulesetCreated, rule: &PathRule, abi: ABI) -> Result<RulesetCreated, String> {
    let Ok(fd) = PathFd::new(&rule.path) else {
        // A root that does not exist (or vanished) grants nothing.
        return Ok(created);
    };
    let access = if rule.path.is_dir() {
        rule.access
    } else {
        rule.access & AccessFs::from_file(abi)
    };
    created
        .add_rule(PathBeneath::new(fd, access))
        .map_err(|e| format!("{}: {e}", rule.path.display()))
}

fn add_ports(
    created: RulesetCreated,
    ports: &Ports,
    access: AccessNet,
) -> Result<RulesetCreated, String> {
    let Ports::Only(ports) = ports else {
        return Ok(created);
    };
    ports.iter().try_fold(created, |created, port| {
        created
            .add_rule(NetPort::new(*port, access))
            .map_err(|e| e.to_string())
    })
}

fn path_rules(policy: &Policy, abi: ABI) -> Vec<PathRule> {
    let denied = policy.denied();
    let read = AccessFs::from_read(abi);
    let all = AccessFs::from_all(abi);
    let mut rules = Vec::new();
    for root in policy.read_roots() {
        carve(root, read, &denied, &mut rules);
    }
    for root in policy.write_roots() {
        carve(root, all, &denied, &mut rules);
    }
    let devices = DEVICES_RW
        .iter()
        .map(|d| (d, all))
        .chain(DEVICES_RO.iter().map(|d| (d, read)));
    rules.extend(devices.map(|(device, access)| PathRule {
        path: PathBuf::from(device),
        access,
    }));
    rules
}

/// Grants `access` below `root` except under every `denied` path inside it.
fn carve(root: &Path, access: BitFlags<AccessFs>, denied: &[&Path], rules: &mut Vec<PathRule>) {
    let inside: Vec<&Path> = denied
        .iter()
        .copied()
        .filter(|d| is_strictly_below(d, root))
        .collect();
    if inside.is_empty() {
        rules.push(PathRule {
            path: root.to_path_buf(),
            access,
        });
        return;
    }
    rules.push(PathRule {
        path: root.to_path_buf(),
        access: AccessFs::ReadDir.into(),
    });
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !inside.iter().any(|d| path.starts_with(d)) {
            carve(&path, access, &inside, rules);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carving_skips_the_store_and_keeps_its_siblings() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
        std::fs::create_dir_all(&base).unwrap();
        let dir = tempfile::tempdir_in(base).unwrap();
        let home = dir.path();
        for sub in [".ssh", ".config/gcloud", ".config/other", "notes"] {
            std::fs::create_dir_all(home.join(sub)).unwrap();
        }
        let denied = [home.join(".ssh"), home.join(".config/gcloud")];
        let denied: Vec<&Path> = denied.iter().map(PathBuf::as_path).collect();
        let mut rules = Vec::new();
        carve(home, AccessFs::from_read(ABI::V1), &denied, &mut rules);
        let paths: Vec<&Path> = rules.iter().map(|r| r.path.as_path()).collect();
        assert!(paths.contains(&home.join("notes").as_path()));
        assert!(paths.contains(&home.join(".config/other").as_path()));
        assert!(!paths.iter().any(|p| p.starts_with(home.join(".ssh"))));
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with(home.join(".config/gcloud")))
        );
        let config = rules
            .iter()
            .find(|r| r.path == home.join(".config"))
            .unwrap();
        assert_eq!(config.access, BitFlags::from(AccessFs::ReadDir));
    }
}
