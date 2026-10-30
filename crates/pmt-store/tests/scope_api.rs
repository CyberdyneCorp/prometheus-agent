//! Store access needs a `ProfileScope` (spec: configuration-and-profiles,
//! Explicit `ProfileScope`): a store location cannot be built from a path,
//! a profile directory or a literal. Each file under `compile-fail/` must
//! not compile.

#[test]
fn store_access_needs_a_scope() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/compile-fail/*.rs");
}
