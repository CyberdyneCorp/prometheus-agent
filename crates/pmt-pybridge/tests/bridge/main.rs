//! The python bridge against real sidecars on the managed interpreter
//! (spec: python-bridge). Needs uv on PATH (see `pmt_pybridge::testkit`).

mod environments;
mod hermes;
mod lifecycle;
mod protocol;
mod providers;
mod rig;
