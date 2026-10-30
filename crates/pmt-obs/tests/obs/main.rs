//! pmt-obs behaviour contracts (spec: observability): the canonical
//! rendering of a recorded trajectory, the writer, retention and the
//! structured log layer. One test binary (modules below) to keep the
//! build small.

mod golden;
mod logs;
mod retention;
mod writer;
