//! What one call into a plugin may spend (spec: plugin-system, WASM sandbox
//! resource limits: CPU bounded per call by `plugins.wasm.call_timeout_ms`).
//!
//! A [`Meter`] lives in the store and is consulted when the epoch deadline
//! passes. A wall meter interrupts the call there: the call's whole time
//! counts. A CPU meter counts only the time spent running wasm, never the
//! time blocked in a host import (an HTTP request, a model call), so a
//! terminal backend waiting on a remote command for minutes is not stopped
//! by the plugin call timeout while a guest spinning in its own code still
//! is. Its optional wall limit stops a call that outlives the command it
//! runs, once control returns to wasm.

use std::time::{Duration, Instant};

use wasmtime::{CallHook, UpdateDeadline};

use super::super::ticks;

/// The budget of the call in progress.
#[derive(Debug, Clone)]
pub(crate) struct Meter {
    /// Wasm time allowed; `None` for a wall-clock deadline.
    cpu: Option<Duration>,
    /// With `cpu`: total time allowed.
    wall: Option<Duration>,
    started: Instant,
    /// Time spent in host imports that returned.
    in_host: Duration,
    /// Host calls in progress: a host import can run wasm again (the
    /// guest's allocator receiving a result), which calls the host in
    /// turn, so only the outermost call is timed.
    depth: u32,
    /// When the outermost host call in progress started.
    entered_host: Option<Instant>,
}

impl Meter {
    /// The epoch deadline alone bounds the call.
    pub(crate) fn wall() -> Self {
        Self::new(None, None)
    }

    /// `cpu` of wasm time, and at most `wall` in all.
    pub(crate) fn cpu(cpu: Duration, wall: Option<Duration>) -> Self {
        Self::new(Some(cpu), wall)
    }

    fn new(cpu: Option<Duration>, wall: Option<Duration>) -> Self {
        Self {
            cpu,
            wall,
            started: Instant::now(),
            in_host: Duration::ZERO,
            depth: 0,
            entered_host: None,
        }
    }

    /// Tracks the time spent in host imports.
    pub(crate) fn transition(&mut self, hook: CallHook) {
        match hook {
            CallHook::CallingHost => {
                if self.depth == 0 {
                    self.entered_host = Some(Instant::now());
                }
                self.depth += 1;
            }
            CallHook::ReturningFromHost => {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0
                    && let Some(entered) = self.entered_host.take()
                {
                    self.in_host += entered.elapsed();
                }
            }
            CallHook::CallingWasm | CallHook::ReturningFromWasm => {}
        }
    }

    /// What to do when the epoch deadline passes: interrupt the call once
    /// it spent its budget, else wait for the rest of it.
    pub(crate) fn on_deadline(&self) -> UpdateDeadline {
        let Some(cpu) = self.cpu else {
            return UpdateDeadline::Interrupt;
        };
        let elapsed = self.started.elapsed();
        if self.wall.is_some_and(|wall| elapsed >= wall) {
            return UpdateDeadline::Interrupt;
        }
        let host = self.in_host + self.entered_host.map_or(Duration::ZERO, |e| e.elapsed());
        let spent = elapsed.saturating_sub(host);
        match cpu.checked_sub(spent) {
            Some(left) if !left.is_zero() => UpdateDeadline::Continue(ticks(left)),
            _ => UpdateDeadline::Interrupt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_in_host_imports_is_not_spent() {
        let mut meter = Meter::cpu(Duration::from_millis(50), None);
        meter.transition(CallHook::CallingHost);
        std::thread::sleep(Duration::from_millis(80));
        meter.transition(CallHook::ReturningFromHost);
        assert!(matches!(meter.on_deadline(), UpdateDeadline::Continue(_)));
        std::thread::sleep(Duration::from_millis(60));
        assert!(matches!(meter.on_deadline(), UpdateDeadline::Interrupt));
    }

    #[test]
    fn wasm_run_from_inside_a_host_call_does_not_end_its_timing() {
        let mut meter = Meter::cpu(Duration::from_millis(50), None);
        meter.transition(CallHook::CallingHost);
        std::thread::sleep(Duration::from_millis(80));
        // The host hands a result to the guest's allocator, whose epoch
        // check calls the host again.
        meter.transition(CallHook::CallingWasm);
        meter.transition(CallHook::CallingHost);
        assert!(matches!(meter.on_deadline(), UpdateDeadline::Continue(_)));
        meter.transition(CallHook::ReturningFromHost);
        meter.transition(CallHook::ReturningFromWasm);
        meter.transition(CallHook::ReturningFromHost);
        assert!(matches!(meter.on_deadline(), UpdateDeadline::Continue(_)));
    }

    #[test]
    fn a_wall_meter_or_limit_interrupts_at_the_deadline() {
        assert!(matches!(
            Meter::wall().on_deadline(),
            UpdateDeadline::Interrupt
        ));
        let mut meter = Meter::cpu(Duration::from_mins(1), Some(Duration::from_millis(20)));
        meter.transition(CallHook::CallingHost);
        std::thread::sleep(Duration::from_millis(30));
        meter.transition(CallHook::ReturningFromHost);
        assert!(matches!(meter.on_deadline(), UpdateDeadline::Interrupt));
    }
}
