//! The trajectory compressor (spec: replay-and-evaluation, Trajectory
//! compressor): a `ShareGPT` record over `target_max_tokens` keeps its
//! head (through the first system, human, gpt and tool turns) and its
//! last `protect_last_turns` turns verbatim, and the smallest contiguous
//! middle span that brings it under the target becomes one `human` turn
//! holding a summary. Records already under the target are copied
//! unchanged without a model call. Tokens are estimated at four
//! characters per token (this build ships no tokenizer).

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::grader::BoxFuture;

/// The default `--target-max-tokens`.
pub const TARGET_MAX_TOKENS: u64 = 15_250;
/// The default `--summary-target-tokens`.
pub const SUMMARY_TARGET_TOKENS: u64 = 750;
/// The default `--protect-last-turns`.
pub const PROTECT_LAST_TURNS: usize = 4;

/// The settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// The token target.
    pub target_max_tokens: u64,
    /// The summary's token budget.
    pub summary_target_tokens: u64,
    /// Turns kept at the end.
    pub protect_last_turns: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target_max_tokens: TARGET_MAX_TOKENS,
            summary_target_tokens: SUMMARY_TARGET_TOKENS,
            protect_last_turns: PROTECT_LAST_TURNS,
        }
    }
}

/// One turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// `system`, `human`, `gpt` or `tool`.
    pub from: String,
    /// Its text.
    pub value: String,
}

/// The estimated tokens of `text`: ceil(chars / 4).
#[must_use]
pub fn tokens(text: &str) -> u64 {
    u64::try_from(text.chars().count().div_ceil(4)).unwrap_or(u64::MAX)
}

/// The tokens of `turns`.
#[must_use]
pub fn total(turns: &[Turn]) -> u64 {
    turns.iter().map(|t| tokens(&t.value)).sum()
}

/// Summarises a span of turns.
pub trait Summarizer: Sync {
    /// A summary of `turns` within `max_tokens`, and the call's cost
    /// (`None` when unpriced).
    fn summarize<'a>(
        &'a self,
        turns: &'a [Turn],
        max_tokens: u64,
    ) -> BoxFuture<'a, Result<(String, Option<f64>), String>>;
}

/// What became of one record.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Under the target: unchanged.
    Unchanged(Value),
    /// Compressed under the target.
    Compressed(Value),
    /// Still over the target (or not compressible).
    OverLimit(Value),
    /// Not a `ShareGPT` record, or the summary failed.
    Failed(String),
}

/// One record's result.
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    /// The outcome.
    pub outcome: Outcome,
    /// Tokens before.
    pub before: u64,
    /// Tokens after.
    pub after: u64,
    /// The summary call's cost (`None`: unpriced or no call).
    pub cost_usd: Option<f64>,
    /// A summary call was made.
    pub called: bool,
}

/// The end of the protected head: just past the first system, human, gpt
/// and tool turns (each that occurs).
fn head_end(turns: &[Turn]) -> usize {
    ["system", "human", "gpt", "tool"]
        .iter()
        .filter_map(|role| turns.iter().position(|t| t.from == *role))
        .max()
        .map_or(0, |i| i + 1)
}

/// The smallest span `[start, end)` within `lo..hi` whose replacement by
/// a summary of `summary` tokens brings `turns` to `target`; earliest
/// first on ties.
fn smallest_span(
    turns: &[Turn],
    (lo, hi): (usize, usize),
    summary: u64,
    target: u64,
) -> Option<(usize, usize)> {
    let all = total(turns);
    let mut best: Option<(usize, usize)> = None;
    for start in lo..hi {
        let mut removed = 0;
        for (end, turn) in turns.iter().enumerate().take(hi).skip(start) {
            removed += tokens(&turn.value);
            if all - removed + summary <= target {
                if best.is_none_or(|(s, e)| end + 1 - start < e - s) {
                    best = Some((start, end + 1));
                }
                break;
            }
        }
    }
    best
}

/// Compresses one record.
pub async fn compress(record: Value, settings: Settings, summarizer: &dyn Summarizer) -> Done {
    let Some(turns) = record
        .get("conversations")
        .and_then(|c| serde_json::from_value::<Vec<Turn>>(c.clone()).ok())
    else {
        return failed("not a ShareGPT record (no 'conversations' list)");
    };
    let before = total(&turns);
    let done = |outcome, after, cost_usd, called| Done {
        outcome,
        before,
        after,
        cost_usd,
        called,
    };
    if before <= settings.target_max_tokens {
        return done(Outcome::Unchanged(record), before, None, false);
    }
    let lo = head_end(&turns);
    let hi = turns
        .len()
        .saturating_sub(settings.protect_last_turns)
        .max(lo);
    let Some((start, end)) = smallest_span(
        &turns,
        (lo, hi),
        settings.summary_target_tokens,
        settings.target_max_tokens,
    ) else {
        return done(Outcome::OverLimit(record), before, None, false);
    };
    let (summary, cost) = match summarizer
        .summarize(&turns[start..end], settings.summary_target_tokens)
        .await
    {
        Ok(reply) => reply,
        Err(why) => return failed(&format!("summary failed: {why}")),
    };
    let summary = cap(&summary, settings.summary_target_tokens);
    let mut out: Vec<Turn> = turns[..start].to_vec();
    out.push(Turn {
        from: "human".to_owned(),
        value: format!("[Summary of {} earlier turns]\n{summary}", end - start),
    });
    out.extend_from_slice(&turns[end..]);
    let after = total(&out);
    let mut record = record;
    record["conversations"] = serde_json::to_value(&out).unwrap_or(Value::Null);
    let outcome = if after <= settings.target_max_tokens {
        Outcome::Compressed(record)
    } else {
        Outcome::OverLimit(record)
    };
    done(outcome, after, cost, true)
}

fn failed(why: &str) -> Done {
    Done {
        outcome: Outcome::Failed(why.to_owned()),
        before: 0,
        after: 0,
        cost_usd: None,
        called: false,
    }
}

/// `text` cut to about `max_tokens` (four characters each).
fn cap(text: &str, max_tokens: u64) -> String {
    let limit = usize::try_from(max_tokens.saturating_mul(4)).unwrap_or(usize::MAX);
    // Room for the summary turn's own header line.
    let limit = limit.saturating_sub(48);
    text.trim().chars().take(limit).collect()
}

/// Whether record `index` is in a `percent` sample (deterministic).
#[must_use]
pub fn sampled(index: usize, percent: f64) -> bool {
    if percent >= 100.0 {
        return true;
    }
    let mut rng = crate::rng::Rng::derived(0, &index.to_string());
    rng.unit() * 100.0 < percent
}

/// `compress-stats.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    /// Records read.
    pub records: usize,
    /// Records in the sample.
    pub sampled: usize,
    /// Copied unchanged.
    pub unchanged: usize,
    /// Compressed under the target.
    pub compressed: usize,
    /// Written to the over-limit file.
    pub over_limit: usize,
    /// Not records, or the summary failed.
    pub failed: usize,
    /// Tokens before.
    pub tokens_before: u64,
    /// Tokens after.
    pub tokens_after: u64,
    /// Summary calls.
    pub summary_calls: usize,
    /// Their priced spend.
    pub cost_usd: f64,
    /// Calls without a price.
    pub unpriced_calls: usize,
}

/// Compresses `records`, at most `parallel` at once; outcomes in order.
pub async fn compress_all(
    records: Vec<Value>,
    settings: Settings,
    summarizer: &dyn Summarizer,
    parallel: usize,
) -> Vec<Done> {
    futures_util::stream::iter(
        records
            .into_iter()
            .map(|r| compress(r, settings, summarizer)),
    )
    .buffered(parallel.max(1))
    .collect()
    .await
}

impl Stats {
    /// Counts `done`.
    pub fn add(&mut self, done: &Done) {
        self.tokens_before += done.before;
        self.tokens_after += done.after;
        if done.called {
            self.summary_calls += 1;
            match done.cost_usd {
                Some(c) => self.cost_usd += c,
                None => self.unpriced_calls += 1,
            }
        }
        match done.outcome {
            Outcome::Unchanged(_) => self.unchanged += 1,
            Outcome::Compressed(_) => self.compressed += 1,
            Outcome::OverLimit(_) => self.over_limit += 1,
            Outcome::Failed(_) => self.failed += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Fake(AtomicUsize);

    impl Summarizer for Fake {
        fn summarize<'a>(
            &'a self,
            turns: &'a [Turn],
            _: u64,
        ) -> BoxFuture<'a, Result<(String, Option<f64>), String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(
                async move { Ok((format!("{} turns about ledgers", turns.len()), Some(0.001))) },
            )
        }
    }

    fn record(n: usize, chars: usize) -> Value {
        let roles = ["human", "gpt", "tool", "gpt"];
        let mut turns = vec![Turn {
            from: "system".into(),
            value: "s".repeat(chars),
        }];
        for i in 1..n {
            turns.push(Turn {
                from: roles[(i - 1) % 4].into(),
                value: format!("{i:03}{}", "x".repeat(chars - 3)),
            });
        }
        serde_json::json!({ "conversations": turns, "id": "r1" })
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// Scenario: Head and tail preserved.
    #[test]
    fn head_and_tail_survive_and_one_summary_replaces_the_middle() {
        // 40 turns of 750 tokens: 30,000 tokens.
        let input = record(40, 3000);
        assert_eq!(
            total(&serde_json::from_value::<Vec<Turn>>(input["conversations"].clone()).unwrap()),
            30_000
        );
        let fake = Fake(AtomicUsize::new(0));
        let done = block_on(compress(input.clone(), Settings::default(), &fake));
        let Outcome::Compressed(out) = &done.outcome else {
            panic!("{done:?}")
        };
        let before: Vec<Turn> = serde_json::from_value(input["conversations"].clone()).unwrap();
        let after: Vec<Turn> = serde_json::from_value(out["conversations"].clone()).unwrap();
        assert_eq!(after[..4], before[..4], "system, human, gpt, tool kept");
        assert_eq!(
            after[after.len() - 4..],
            before[before.len() - 4..],
            "last 4 kept"
        );
        let summaries = after
            .iter()
            .filter(|t| t.value.starts_with("[Summary of"))
            .count();
        assert_eq!(summaries, 1);
        assert_eq!(after[4].from, "human");
        assert!(done.after <= TARGET_MAX_TOKENS, "{}", done.after);
        assert_eq!(out["id"], "r1", "other fields kept");
        assert_eq!(fake.0.load(Ordering::SeqCst), 1);
    }

    /// Scenario: Short trajectory untouched.
    #[test]
    fn a_short_record_is_copied_without_a_model_call() {
        let input = record(8, 4000);
        let fake = Fake(AtomicUsize::new(0));
        let done = block_on(compress(input.clone(), Settings::default(), &fake));
        assert_eq!(done.outcome, Outcome::Unchanged(input));
        assert_eq!(fake.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn samples_are_deterministic() {
        let a: Vec<bool> = (0..100).map(|i| sampled(i, 30.0)).collect();
        let b: Vec<bool> = (0..100).map(|i| sampled(i, 30.0)).collect();
        assert_eq!(a, b);
        assert!(a.iter().filter(|x| **x).count() > 10);
    }
}
