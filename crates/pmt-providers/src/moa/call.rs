//! The MoA call procedure (spec: mixture-of-agents, MoA call procedure;
//! Reference failures are non-fatal; Strict-alternation recovery; MoA
//! traces; MoA billing and accounting): references run concurrently on
//! the tool-free view when the cadence says so, each failure becoming a
//! one-line note; the aggregator then gets the session's own request with
//! the guidance appended, and its stream is the call's. Every slot's call
//! is its own ledger row (`moa.reference` / `moa.aggregator`, the preset
//! as `purpose_detail`); only an aggregator failure fails the call, which
//! the caller then hands to the session's fallback chain.

use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::schema::{MoaPreset, MoaSlot};
use pmt_core::error::Error;

use super::guidance::{self, Reference, Shape};
use super::privacy::Filter;
use super::trace::{self, Record, Slot, Summary};
use super::{AGGREGATOR_PURPOSE, REFERENCE_PURPOSE, view};
use crate::caller::{ModelCaller, fit_reasoning};
use crate::classify::{ErrorClass, ProviderError};
use crate::cost::CallContext;
use crate::event::StreamEvent;
use crate::fallback::Walk;
use crate::request::Request;
use crate::resolve::{Route, Trace};
use crate::transport::{ModelStream, OnEnd, Tap};

/// The references' contribution to one call.
struct Fanned {
    /// What the aggregator reads.
    guidance: String,
    /// Their trace entries (empty when the turn's guidance was reused).
    references: Vec<Slot>,
    /// The blocks users see.
    advice: Vec<String>,
}

/// What one preset call needs besides the caller.
struct Call<'a> {
    walk: &'a Walk<'a>,
    preset_name: &'a str,
    preset: &'a MoaPreset,
    request: &'a Request,
    ctx: Option<&'a CallContext>,
}

impl Call<'_> {
    /// The context of a slot's ledger row.
    fn slot_ctx(&self, purpose: &str) -> Option<CallContext> {
        self.ctx.map(|ctx| CallContext {
            purpose: purpose.to_owned(),
            purpose_detail: Some(self.preset_name.to_owned()),
            ..ctx.clone()
        })
    }

    /// The route of `slot`, resolved with its own credential.
    async fn resolve(&self, slot: &MoaSlot) -> Result<Route, Error> {
        self.walk
            .resolver
            .resolve_target(&super::slot_target(slot), &mut Trace::default())
            .await
    }

    /// `request` as `slot` on `route` gets it, effort fitted to its model.
    fn slot_request(
        &self,
        request: &Request,
        slot: &MoaSlot,
        route: &Route,
        temperature: Option<f64>,
    ) -> Request {
        let sent = view::for_slot(request, slot, route, self.request.reasoning, temperature);
        fit_reasoning(&sent, route, self.walk.catalog).unwrap_or(sent)
    }

    /// The context window classification hooks are told for `route`.
    fn window(&self, route: &Route) -> u64 {
        let (meta, _) =
            self.walk
                .catalog
                .metadata_at(&route.provider, &route.model, Some(&route.base_url));
        meta.context_window
    }
}

impl ModelCaller {
    /// Sends `request` on MoA preset `route`; returns the aggregator's
    /// stream and the advisor blocks to show (when references ran).
    ///
    /// # Errors
    ///
    /// The aggregator's failure (its route not resolving included).
    pub(crate) async fn send_moa(
        &self,
        route: &Route,
        request: &Request,
        ctx: Option<&CallContext>,
        walk: &Walk<'_>,
    ) -> Result<(ModelStream, Vec<String>), ProviderError> {
        let config = walk.resolver.config;
        let preset = config.moa.preset(&route.model).ok_or_else(|| {
            ProviderError::refused(
                ErrorClass::Config,
                &super::unknown(&route.model, &config.moa),
            )
        })?;
        let call = Call {
            walk,
            preset_name: &route.model,
            preset: &preset,
            request,
            ctx,
        };
        let aggregator = call
            .resolve(&preset.aggregator)
            .await
            .map_err(|e| refusal(&e))?;
        if !preset.enabled {
            let sent = call.slot_request(
                request,
                &preset.aggregator,
                &aggregator,
                preset.aggregator_temperature,
            );
            let stream = self
                .send_sized(&aggregator, &sent, ctx, Some(call.window(&aggregator)))
                .await?;
            return Ok((stream, Vec::new()));
        }
        self.mixture_call(&call, &aggregator).await
    }

    /// References (per the cadence), guidance, aggregator, trace.
    async fn mixture_call(
        &self,
        call: &Call<'_>,
        aggregator: &Route,
    ) -> Result<(ModelStream, Vec<String>), ProviderError> {
        let mixture = self.mixture();
        let filter = Filter::new(
            call.walk.resolver.config.moa.privacy_filter,
            mixture.secrets(),
        );
        let step = mixture.begin(call.ctx, mixture.fanout(call.preset_name, call.preset));
        let fanned = match step.reuse.clone() {
            Some(guidance) => Fanned {
                guidance,
                references: Vec::new(),
                advice: Vec::new(),
            },
            None => self.fan_out(call, &filter).await,
        };
        let sent = call.slot_request(
            call.request,
            &call.preset.aggregator,
            aggregator,
            call.preset.aggregator_temperature,
        );
        let traced = self.record(call, step.iteration, fanned.references, aggregator, &sent);
        let result = self
            .aggregate(call, aggregator, &sent, &fanned.guidance)
            .await;
        match (result, traced) {
            (Ok(mut stream), Some((record, file))) => {
                trace_on_end(&mut stream, record, file, &filter);
                Ok((stream, fanned.advice))
            }
            (Err(error), Some((mut record, file))) => {
                record.aggregator.error = Some(filter.trace(&error.message));
                trace::append(&file, &record);
                Err(error)
            }
            (result, None) => result.map(|stream| (stream, fanned.advice)),
        }
    }

    /// Runs the references and keeps their guidance for the turn.
    async fn fan_out(&self, call: &Call<'_>, filter: &Filter<'_>) -> Fanned {
        let (outcomes, slots): (Vec<Reference>, Vec<Slot>) =
            self.references(call).await.into_iter().unzip();
        let guidance = guidance::text(&outcomes, |t| filter.guidance(t));
        self.mixture().keep(call.ctx, &guidance);
        Fanned {
            guidance,
            advice: outcomes.iter().map(|r| advice(r, filter)).collect(),
            references: slots.into_iter().map(|s| traced(s, filter)).collect(),
        }
    }

    /// The trace record of this call and its file, when traces are on.
    fn record(
        &self,
        call: &Call<'_>,
        iteration: u32,
        references: Vec<Slot>,
        aggregator: &Route,
        sent: &Request,
    ) -> Option<(Record, std::path::PathBuf)> {
        let ctx = call.ctx?;
        let session = ctx.session_id.to_string();
        let config = &call.walk.resolver.config.moa;
        let file = trace::file(config, self.mixture().profile_dir.as_deref(), &session)?;
        let mut slot = Slot::new(&aggregator.provider, &aggregator.model);
        slot.request = Some(Summary::of(
            sent,
            aggregator,
            call.preset.aggregator_temperature,
        ));
        let record = Record {
            at: jiff::Timestamp::now().to_string(),
            session_id: session,
            turn_id: ctx.turn_id.clone(),
            preset: call.preset_name.to_owned(),
            iteration,
            references,
            aggregator: slot,
        };
        Some((record, file))
    }

    /// Every reference of the preset, concurrently.
    async fn references(&self, call: &Call<'_>) -> Vec<(Reference, Slot)> {
        let view = view::reference_view(call.request);
        let ctx = call.slot_ctx(REFERENCE_PURPOSE);
        let runs = call
            .preset
            .reference_models
            .iter()
            .map(|slot| self.reference(call, slot, &view, ctx.as_ref()));
        futures_util::future::join_all(runs).await
    }

    /// One reference: its answer, or the one-line reason it has none.
    async fn reference(
        &self,
        call: &Call<'_>,
        slot: &MoaSlot,
        view: &Request,
        ctx: Option<&CallContext>,
    ) -> (Reference, Slot) {
        let label = slot.label();
        let mut entry = Slot::new(&slot.provider, &slot.model);
        let failed = |entry: Slot, why: String| {
            let entry = Slot {
                error: Some(why.clone()),
                ..entry
            };
            (
                Reference {
                    label: label.clone(),
                    outcome: Err(why),
                },
                entry,
            )
        };
        let route = match call.resolve(slot).await {
            Ok(route) => route,
            Err(err) => return failed(entry, err.message().to_owned()),
        };
        let temperature = call.preset.reference_temperature;
        let sent = call.slot_request(view, slot, &route, temperature);
        entry.request = Some(Summary::of(&sent, &route, temperature));
        let mut stream = match self
            .send_sized(&route, &sent, ctx, Some(call.window(&route)))
            .await
        {
            Ok(stream) => stream,
            Err(err) => return failed(entry, err.to_error(&route.provider).message().to_owned()),
        };
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::TextDelta { text: delta } = event {
                text.push_str(&delta);
            }
        }
        entry.usage = stream.usage();
        if let Some(failure) = stream.failure() {
            let why = failure.to_error(&route.provider).message().to_owned();
            return failed(entry, why);
        }
        entry.output = Some(text.clone());
        (
            Reference {
                label,
                outcome: Ok(text),
            },
            entry,
        )
    }

    /// The aggregator call with `guidance` after the stable prefix; a
    /// destination that refuses consecutive user messages is retried
    /// once merged and remembered as merge-required for the session.
    async fn aggregate(
        &self,
        call: &Call<'_>,
        aggregator: &Route,
        request: &Request,
        guidance: &str,
    ) -> Result<ModelStream, ProviderError> {
        let ctx = call.slot_ctx(AGGREGATOR_PURPOSE);
        let window = Some(call.window(aggregator));
        let mixture = self.mixture();
        let shape = Shape::of(aggregator, mixture.merge_required(call.ctx, aggregator));
        let sent = guidance::inject(request, guidance, shape);
        match self
            .send_sized(aggregator, &sent, ctx.as_ref(), window)
            .await
        {
            Err(error) if shape != Shape::Merged && guidance::alternation_refused(&error) => {
                tracing::info!(
                    destination = %aggregator.base_url,
                    model = %aggregator.model,
                    "the aggregator refuses consecutive user messages; merging the MoA guidance"
                );
                mixture.require_merge(call.ctx, aggregator);
                let merged = guidance::inject(request, guidance, Shape::Merged);
                self.send_sized(aggregator, &merged, ctx.as_ref(), window)
                    .await
            }
            other => other,
        }
    }
}

/// An aggregator route that did not resolve, as a refused call.
fn refusal(error: &Error) -> ProviderError {
    let class = if error.code().kind == pmt_core::error::ErrorKind::Auth {
        ErrorClass::Auth
    } else {
        ErrorClass::Config
    };
    ProviderError::refused(class, error)
}

/// The block users see for one reference.
fn advice(reference: &Reference, filter: &Filter<'_>) -> String {
    match &reference.outcome {
        Ok(text) => format!(
            "advisor {}:\n{}",
            reference.label,
            filter.display(text.trim())
        ),
        Err(why) => format!("advisor {}: unavailable: {why}", reference.label),
    }
}

/// A reference's trace entry after the filter.
fn traced(slot: Slot, filter: &Filter<'_>) -> Slot {
    Slot {
        output: slot.output.map(|t| filter.trace(&t)),
        error: slot.error.map(|e| filter.trace(&e)),
        ..slot
    }
}

/// Writes `record` with the aggregator's output once `stream` ends.
fn trace_on_end(
    stream: &mut ModelStream,
    record: Record,
    file: std::path::PathBuf,
    filter: &Filter<'_>,
) {
    let seen = Arc::new(Mutex::new((String::new(), Vec::<String>::new())));
    let tap = Arc::clone(&seen);
    stream.tap(Tap(Box::new(move |event| {
        let mut seen = tap.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            StreamEvent::TextDelta { text } => seen.0.push_str(text),
            StreamEvent::ToolCallEnd { name, .. } => seen.1.push(name.clone()),
            _ => {}
        }
    })));
    let owned = filter.to_owned_filter();
    stream.on_end(OnEnd::new(move |failure, usage| {
        let filter = owned.filter();
        let (text, tools) =
            std::mem::take(&mut *seen.lock().unwrap_or_else(PoisonError::into_inner));
        let mut record = record;
        record.aggregator.output = Some(filter.trace(&text));
        record.aggregator.tool_calls = tools;
        record.aggregator.usage = usage;
        record.aggregator.error = failure.map(|f| filter.trace(&f.message));
        trace::append(&file, &record);
    }));
}
