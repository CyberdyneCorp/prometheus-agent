//! Evolution engine contracts against test doubles of its ports (spec:
//! harness-evolution, Parallel mode; Sequential mode; Stage acceptance,
//! retry and fallback; Evidence context; Leak globs and verdict
//! tripwire; Budget ledger and spend cap; Concurrency limits; Blind
//! anonymised selection; User-driven selection; Proposals and adoption
//! policy; Stale proposal protection; Job lifecycle; Resume and
//! content-addressed session cache); and the experience library it
//! feeds (spec: experience-library).

mod budget;
mod library;
mod modes;
mod proposals;
mod resume;
mod selection;
mod stages;
mod support;
