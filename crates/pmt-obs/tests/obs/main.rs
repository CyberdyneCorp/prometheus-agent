//! pmt-obs behaviour contracts (spec: observability): the canonical
//! rendering of a recorded trajectory, the writer, retention, the
//! structured log layer, the usage ledger and budgets. One test binary
//! (modules below) to keep the build small.

mod budget;
mod golden;
mod logs;
mod retention;
mod support;
mod usage;
mod writer;
