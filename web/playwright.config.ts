// End-to-end tests of the dashboard in a real browser. They run against a
// daemon the Rust test harness starts (crates/prometheus/tests/
// dashboard_e2e.rs), which passes the binary and the test home in
// PMT_BIN and PMT_HOME; each spec is one scenario the harness names.
import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "e2e",
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: [["list"]],
  outputDir: process.env.PMT_PW_OUT ?? "../target/playwright-results",
  use: {
    headless: true,
    trace: "off",
  },
});
