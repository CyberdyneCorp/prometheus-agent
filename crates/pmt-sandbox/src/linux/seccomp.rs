//! The seccomp filters of every child.
//!
//! Two filters stack: `clone3` fails with `ENOSYS` (its flags live in memory
//! a filter cannot read, so libc falls back to `clone`, whose flags it can),
//! and a deny list fails with `EPERM`: debugging and memory access to other
//! processes, kernel keyrings, BPF, perf, mounts and namespaces (user
//! namespaces would let a child build a fresh view of the system), module
//! and kexec loading, `io_uring` (its operations bypass seccomp), handle-based
//! opens, plus, by policy, non-TCP IP sockets and new processes.

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use crate::policy::Policy;

pub(super) type Program = BpfProgram;

#[cfg(target_arch = "x86_64")]
const ARCH: Option<TargetArch> = Some(TargetArch::x86_64);
#[cfg(target_arch = "aarch64")]
const ARCH: Option<TargetArch> = Some(TargetArch::aarch64);
#[cfg(target_arch = "riscv64")]
const ARCH: Option<TargetArch> = Some(TargetArch::riscv64);
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
)))]
const ARCH: Option<TargetArch> = None;

/// Syscalls no child needs.
const DENIED: &[libc::c_long] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_setns,
    libc::SYS_unshare,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_reboot,
    libc::SYS_kexec_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_acct,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_open_by_handle_at,
    libc::SYS_name_to_handle_at,
];

/// Whether this kernel can filter syscalls (and the architecture is known).
pub(super) fn supported() -> bool {
    ARCH.is_some()
        && std::fs::read_to_string("/proc/self/status")
            .is_ok_and(|status| status.lines().any(|l| l.starts_with("Seccomp:")))
}

/// The filters for `policy`, in installation order.
pub(super) fn programs(policy: &Policy) -> Result<Vec<Program>, String> {
    let arch = ARCH.ok_or("seccomp: unsupported architecture")?;
    let clone3 = BTreeMap::from([(libc::SYS_clone3, Vec::new())]);
    let mut denied: BTreeMap<i64, Vec<SeccompRule>> =
        DENIED.iter().map(|nr| (*nr, Vec::new())).collect();
    denied.insert(libc::SYS_clone, clone_rules(policy.may_spawn())?);
    if !policy.may_spawn() {
        add_fork(&mut denied);
    }
    if !policy.connect().is_all() {
        denied.insert(libc::SYS_socket, socket_rules()?);
    }
    [(clone3, libc::ENOSYS), (denied, libc::EPERM)]
        .into_iter()
        .map(|(rules, errno)| compile(rules, errno, arch))
        .collect()
}

fn compile(
    rules: BTreeMap<i64, Vec<SeccompRule>>,
    errno: i32,
    arch: TargetArch,
) -> Result<Program, String> {
    let errno = u32::try_from(errno).map_err(|e| e.to_string())?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(errno),
        arch,
    )
    .map_err(|e| e.to_string())?;
    BpfProgram::try_from(filter).map_err(|e| e.to_string())
}

/// A condition on argument `arg`; `int` arguments compare their low 32
/// bits only (the upper half of the register is unspecified).
fn condition(
    arg: u8,
    len: SeccompCmpArgLen,
    op: SeccompCmpOp,
    value: u64,
) -> Result<SeccompCondition, String> {
    SeccompCondition::new(arg, len, op, value).map_err(|e| e.to_string())
}

fn flags(bits: libc::c_int) -> u64 {
    u64::try_from(bits).unwrap_or_default()
}

/// `clone` creating a user namespace, and without `may_spawn` any `clone`
/// that is not a thread.
fn clone_rules(may_spawn: bool) -> Result<Vec<SeccompRule>, String> {
    let newuser = flags(libc::CLONE_NEWUSER);
    let thread = flags(libc::CLONE_THREAD);
    let mut rules = vec![rule(vec![condition(
        0,
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::MaskedEq(newuser),
        newuser,
    )?])?];
    if !may_spawn {
        rules.push(rule(vec![condition(
            0,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::MaskedEq(thread),
            0,
        )?])?);
    }
    Ok(rules)
}

#[cfg(target_arch = "x86_64")]
fn add_fork(denied: &mut BTreeMap<i64, Vec<SeccompRule>>) {
    denied.insert(libc::SYS_fork, Vec::new());
    denied.insert(libc::SYS_vfork, Vec::new());
}

#[cfg(not(target_arch = "x86_64"))]
fn add_fork(_denied: &mut BTreeMap<i64, Vec<SeccompRule>>) {}

/// IPv4/IPv6 sockets other than streams: TCP is left to Landlock's port
/// rules, everything else (UDP, raw, SCTP) is refused.
fn socket_rules() -> Result<Vec<SeccompRule>, String> {
    let mut rules = Vec::new();
    for domain in [libc::AF_INET, libc::AF_INET6] {
        for kind in [libc::SOCK_DGRAM, libc::SOCK_RAW, libc::SOCK_SEQPACKET] {
            rules.push(rule(vec![
                condition(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, flags(domain))?,
                condition(
                    1,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::MaskedEq(0xf),
                    flags(kind),
                )?,
            ])?);
        }
    }
    Ok(rules)
}

fn rule(conditions: Vec<SeccompCondition>) -> Result<SeccompRule, String> {
    SeccompRule::new(conditions).map_err(|e| e.to_string())
}
