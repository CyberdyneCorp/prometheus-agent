//! Cost and latency aware routing (spec: model-providers, Cost and latency
//! aware routing policy).
//!
//! At session start a non-`pinned` `routing.policy` picks the session's
//! route among the candidates (`[[routing.candidates]]`, else the
//! requested route plus the fallback chain). A candidate is dropped when
//! it has no usable credential, its health score is below
//! `fallback.min_health` or its model cannot call tools; the policy then
//! ranks the rest by estimated cost per turn, median time to first token,
//! or a weighted sum of both normalised over the eligible candidates. The
//! decision is chosen once: the session keeps it for every turn (only a
//! fallback or `/model` changes it later).

use pmt_config::schema::RoutingPolicy;
use pmt_obs::trajectory::payload::{RejectedRoute, RouteInputs, RouteSelected};
use pmt_obs::usage::Prices as _;

use crate::capability::{self, Needs};
use crate::fallback::RouteHealth;
use crate::metadata::Catalog;
use crate::resolve::{Resolver, Route, Target, Trace};

/// Input tokens of the typical turn a cost estimate prices.
pub const TURN_INPUT_TOKENS: f64 = 8_000.0;
/// Output tokens of that turn.
pub const TURN_OUTPUT_TOKENS: f64 = 1_000.0;

/// What a policy weighs about one eligible candidate.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Inputs {
    /// Estimated cost of a typical turn in USD (`None`: unpriced).
    pub cost_usd: Option<f64>,
    /// Median time to first token (`None`: never measured).
    pub ttft_p50_ms: Option<u64>,
    /// Health score in [0, 1].
    pub health: f64,
}

/// One candidate as a policy sees it: its label and its inputs, or why it
/// was dropped before ranking (`credentials`, `health`, `capability`).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// `provider/model`.
    pub label: String,
    /// The inputs of an eligible candidate, else the reason it was dropped.
    pub verdict: Result<Inputs, &'static str>,
}

/// The weight of cost (the rest is latency) a policy ranks by: `cheapest`
/// weighs only cost, `fastest` only latency, `balanced` both;
/// `None` for `pinned`, which does not rank.
#[must_use]
pub fn cost_weight(policy: RoutingPolicy, balanced: f64) -> Option<f64> {
    match policy {
        RoutingPolicy::Pinned => None,
        RoutingPolicy::Cheapest => Some(1.0),
        RoutingPolicy::Fastest => Some(0.0),
        RoutingPolicy::Balanced => Some(balanced.clamp(0.0, 1.0)),
    }
}

/// The index of the winning candidate under `weight` (see
/// [`cost_weight`]): the lowest `weight * cost + (1 - weight) * latency`,
/// each normalised to [0, 1] over the eligible candidates (an unknown
/// value counts as the worst); a tie goes to the earlier candidate.
/// `None` when no candidate is eligible.
#[must_use]
pub fn rank(weight: f64, candidates: &[Candidate]) -> Option<usize> {
    let eligible: Vec<(usize, &Inputs)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.verdict.as_ref().ok().map(|inputs| (i, inputs)))
        .collect();
    #[allow(clippy::cast_precision_loss)] // milliseconds far below 2^52
    let latency = |i: &Inputs| i.ttft_p50_ms.map(|ms| ms as f64);
    let costs = Normaliser::over(eligible.iter().map(|(_, i)| i.cost_usd));
    let latencies = Normaliser::over(eligible.iter().map(|(_, i)| latency(i)));
    let score = |inputs: &Inputs| {
        weight * costs.of(inputs.cost_usd) + (1.0 - weight) * latencies.of(latency(inputs))
    };
    let mut best: Option<(usize, f64)> = None;
    for (index, inputs) in eligible {
        let value = score(inputs);
        if best.is_none_or(|(_, b)| value < b) {
            best = Some((index, value));
        }
    }
    best.map(|(index, _)| index)
}

/// Min-max normalisation over the known values of one input.
struct Normaliser {
    range: Option<(f64, f64)>,
}

impl Normaliser {
    fn over(values: impl Iterator<Item = Option<f64>>) -> Self {
        let range = values.flatten().fold(None, |range: Option<(f64, f64)>, v| {
            Some(range.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))))
        });
        Self { range }
    }

    /// 0 for the best known value, 1 for the worst or an unknown one.
    fn of(&self, value: Option<f64>) -> f64 {
        match (value, self.range) {
            (Some(v), Some((lo, hi))) if hi > lo => (v - lo) / (hi - lo),
            (Some(_), Some(_)) => 0.0,
            _ => 1.0,
        }
    }
}

/// What routing consults about candidates.
pub struct Context<'a> {
    /// Resolves candidates (a failure means no usable credential).
    pub resolver: &'a Resolver<'a>,
    /// Health scores and latency.
    pub health: &'a dyn RouteHealth,
    /// Model capabilities and prices.
    pub catalog: &'a Catalog,
    /// `fallback.min_health`.
    pub min_health: f64,
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("min_health", &self.min_health)
            .finish_non_exhaustive()
    }
}

/// A session's chosen route and the `provider.route_selected` event that
/// records it.
#[derive(Debug, Clone)]
pub struct Selection {
    /// The chosen candidate.
    pub target: Target,
    /// Its resolved route.
    pub route: Route,
    /// The decision with its inputs.
    pub event: RouteSelected,
}

/// Chooses a session's route among `candidates` under `policy` (`None`
/// for `pinned`, and when no candidate is eligible: the requested route
/// then stands, failing or not as it would without a policy).
pub async fn select(
    policy: RoutingPolicy,
    balanced_weight: f64,
    candidates: &[Target],
    ctx: &Context<'_>,
) -> Option<Selection> {
    let weight = cost_weight(policy, balanced_weight)?;
    let mut seen = std::collections::HashSet::new();
    let mut assessed: Vec<(Target, Option<Route>, Candidate)> = Vec::new();
    for target in candidates {
        let (route, candidate) = assess(target, ctx).await;
        if let Some(route) = &route
            && !seen.insert(route.identity())
        {
            continue;
        }
        assessed.push((target.clone(), route, candidate));
    }
    let views: Vec<Candidate> = assessed.iter().map(|(_, _, c)| c.clone()).collect();
    let winner = rank(weight, &views)?;
    let event = event(policy, winner, &views);
    let (target, route, _) = assessed.swap_remove(winner);
    Some(Selection {
        target,
        route: route?,
        event,
    })
}

/// Resolves and checks one candidate.
async fn assess(target: &Target, ctx: &Context<'_>) -> (Option<Route>, Candidate) {
    let fallback_label = format!(
        "{}/{}",
        target.provider,
        target.model.as_deref().unwrap_or("default")
    );
    let Ok(route) = ctx
        .resolver
        .resolve_target(target, &mut Trace::default())
        .await
    else {
        let candidate = Candidate {
            label: fallback_label,
            verdict: Err("credentials"),
        };
        return (None, candidate);
    };
    let label = route.label();
    // A MoA preset stands or falls with its aggregator.
    let acting = route.acting();
    let health = ctx.health.score(&acting.provider, &acting.model);
    let meta = ctx
        .catalog
        .metadata_at(&acting.provider, &acting.model, Some(&acting.base_url))
        .0;
    let tools = Needs {
        tools: true,
        ..Needs::default()
    };
    let verdict = if health < ctx.min_health {
        Err("health")
    } else if capability::check(&label, &meta, &tools).is_err() {
        Err("capability")
    } else {
        Ok(Inputs {
            cost_usd: turn_cost(ctx.catalog, acting),
            ttft_p50_ms: ctx.health.ttft_p50_ms(&acting.provider, &acting.model),
            health,
        })
    };
    (Some(route), Candidate { label, verdict })
}

/// The estimated cost of a typical turn on `route`: a local server costs
/// nothing, an unpriced route is unknown.
fn turn_cost(catalog: &Catalog, route: &Route) -> Option<f64> {
    if route.local {
        return Some(0.0);
    }
    let price = catalog.price(&route.provider, &route.model)?;
    Some((price.input * TURN_INPUT_TOKENS + price.output * TURN_OUTPUT_TOKENS) / 1_000_000.0)
}

/// The `provider.route_selected` payload: every candidate's inputs, the
/// winner, and every other candidate with why it lost (its drop reason,
/// or what the policy ranks by).
fn event(policy: RoutingPolicy, winner: usize, candidates: &[Candidate]) -> RouteSelected {
    let ranked_out = match policy {
        RoutingPolicy::Cheapest => "cost",
        RoutingPolicy::Fastest => "latency",
        RoutingPolicy::Balanced | RoutingPolicy::Pinned => "score",
    };
    let rejected = candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != winner)
        .map(|(_, c)| RejectedRoute {
            route: c.label.clone(),
            reason: c.verdict.err().unwrap_or(ranked_out).to_owned(),
        })
        .collect();
    let inputs = candidates
        .iter()
        .filter_map(|c| {
            c.verdict.as_ref().ok().map(|i| RouteInputs {
                route: c.label.clone(),
                cost_usd: i.cost_usd,
                ttft_p50_ms: i.ttft_p50_ms,
                health: i.health,
            })
        })
        .collect();
    RouteSelected {
        policy: policy.as_str().to_owned(),
        selected: candidates[winner].label.clone(),
        rejected,
        candidates: inputs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible(label: &str, cost: Option<f64>, ttft: Option<u64>) -> Candidate {
        Candidate {
            label: label.to_owned(),
            verdict: Ok(Inputs {
                cost_usd: cost,
                ttft_p50_ms: ttft,
                health: 1.0,
            }),
        }
    }

    fn dropped(label: &str, reason: &'static str) -> Candidate {
        Candidate {
            label: label.to_owned(),
            verdict: Err(reason),
        }
    }

    /// The routing table: policy, candidates, expected winner.
    #[test]
    fn each_policy_picks_its_candidate() {
        let table: Vec<(RoutingPolicy, Vec<Candidate>, Option<&str>)> = vec![
            (
                RoutingPolicy::Cheapest,
                vec![
                    dropped("a/cheap", "health"),
                    eligible("b/mid", Some(0.02), Some(900)),
                    eligible("c/dear", Some(0.09), Some(300)),
                ],
                Some("b/mid"),
            ),
            (
                RoutingPolicy::Fastest,
                vec![
                    eligible("b/mid", Some(0.02), Some(900)),
                    eligible("c/dear", Some(0.09), Some(300)),
                ],
                Some("c/dear"),
            ),
            // Balanced at 0.5: b is (0 + 1) / 2, c (1 + 0) / 2, d
            // (0.25 + 0.5) / 2 wins.
            (
                RoutingPolicy::Balanced,
                vec![
                    eligible("b/mid", Some(0.02), Some(900)),
                    eligible("c/dear", Some(0.10), Some(300)),
                    eligible("d/even", Some(0.04), Some(600)),
                ],
                Some("d/even"),
            ),
            // Unknown values rank last; ties keep the earlier candidate.
            (
                RoutingPolicy::Cheapest,
                vec![
                    eligible("x/unpriced", None, Some(100)),
                    eligible("y/local", Some(0.0), None),
                    eligible("z/local", Some(0.0), None),
                ],
                Some("y/local"),
            ),
            (
                RoutingPolicy::Fastest,
                vec![
                    eligible("y/untried", Some(0.0), None),
                    eligible("x/measured", None, Some(4_000)),
                ],
                Some("x/measured"),
            ),
            (
                RoutingPolicy::Cheapest,
                vec![dropped("a/x", "credentials"), dropped("b/y", "capability")],
                None,
            ),
        ];
        for (policy, candidates, want) in table {
            let weight = cost_weight(policy, 0.5).unwrap();
            let got = rank(weight, &candidates).map(|i| candidates[i].label.as_str());
            assert_eq!(got, want, "{policy:?} over {candidates:?}");
        }
    }

    #[test]
    fn pinned_never_ranks_and_weights_are_clamped() {
        assert_eq!(cost_weight(RoutingPolicy::Pinned, 0.5), None);
        assert_eq!(cost_weight(RoutingPolicy::Balanced, 7.0), Some(1.0));
    }

    #[test]
    fn the_event_names_every_loser_with_its_reason_and_every_input() {
        let candidates = vec![
            dropped("a/cheap", "health"),
            eligible("b/mid", Some(0.02), Some(900)),
            eligible("c/dear", Some(0.09), Some(300)),
        ];
        let event = event(RoutingPolicy::Cheapest, 1, &candidates);
        assert_eq!(event.policy, "cheapest");
        assert_eq!(event.selected, "b/mid");
        let rejected: Vec<(&str, &str)> = event
            .rejected
            .iter()
            .map(|r| (r.route.as_str(), r.reason.as_str()))
            .collect();
        assert_eq!(rejected, [("a/cheap", "health"), ("c/dear", "cost")]);
        assert_eq!(event.candidates.len(), 2);
        assert_eq!(event.candidates[1].ttft_p50_ms, Some(300));
    }
}
