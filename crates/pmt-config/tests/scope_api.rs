//! The `ProfileScope` contract is enforced by the compiler (spec:
//! configuration-and-profiles, Explicit `ProfileScope`): an effective
//! configuration is reachable only through a scope loaded from a profile,
//! and neither a scope nor a profile directory can be conjured from a path
//! or from ambient state. Each file under `compile-fail/` must not compile.

#[test]
fn profile_data_needs_a_scope() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/compile-fail/*.rs");
}
