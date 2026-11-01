//! The sandbox launcher as its own executable, for the crate's tests;
//! `prometheus` itself plays this role in production.

fn main() {
    std::process::exit(
        pmt_sandbox::launcher::run_if_requested().unwrap_or(pmt_sandbox::launcher::LAUNCH_FAILED),
    );
}
