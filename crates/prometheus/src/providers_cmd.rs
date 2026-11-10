//! `prometheus providers` (spec: model-providers, Provider health
//! scoring): `providers health [--json]` prints the health record of
//! every (provider, model, credential) the profile called, with its score
//! after idle decay.

use std::fmt::Write as _;
use std::sync::Arc;

use clap::Subcommand;
use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_providers::health::StoreHealth;

use crate::context::Context;
use crate::output::{document, print_raw};

/// `prometheus providers` subcommands.
#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    /// Health of every route called: success rate, errors by class, time
    /// to first token, output speed and the score fallback walks use.
    Health,
}

/// Runs one `providers` subcommand.
///
/// # Errors
///
/// Profile, config and store errors.
pub fn run(command: &ProvidersCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        ProvidersCommand::Health => health(ctx),
    }
}

fn health(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = Arc::new(pmt_store::Store::open(&scope)?);
    let records = StoreHealth::new(store, scope.config()).records(Timestamp::now())?;
    if ctx.json {
        return document(
            "providers health",
            &serde_json::json!({ "routes": records }),
        );
    }
    if records.is_empty() {
        return print_raw("no provider calls recorded yet\n");
    }
    let mut text = String::new();
    for r in &records {
        let errors: Vec<String> = r.errors.iter().map(|(c, n)| format!("{c} {n}")).collect();
        let _ = writeln!(
            text,
            "{}/{} [{}]  score {:.2}  ok {:.0}% of {}  ttft p50 {} p95 {}  {}",
            r.provider,
            r.model,
            r.credential,
            r.score,
            r.success_rate * 100.0,
            r.calls,
            ms(r.ttft_p50_ms),
            ms(r.ttft_p95_ms),
            if errors.is_empty() {
                "no errors".to_owned()
            } else {
                errors.join(", ")
            }
        );
    }
    print_raw(&text)
}

fn ms(value: Option<u64>) -> String {
    value.map_or_else(|| "—".to_owned(), |ms| format!("{ms}ms"))
}
