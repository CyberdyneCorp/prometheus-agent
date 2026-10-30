//! The `tracing` layer that writes [`Record`]s, and its installation.
//!
//! The level filter is `PMT_LOG`, else `RUST_LOG` (directives such as
//! `info,pmt_store=debug`), else `logging.level` of the launch profile,
//! which [`LogHandle::set_level`] applies once the profile's configuration
//! is loaded. Spans are always enabled so that their context fields reach
//! the records of any level.

use std::collections::HashMap;
use std::fmt;
use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};

use pmt_config::schema::LogLevel;
use pmt_core::secret::Redactor;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record as SpanRecord};
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

use super::{CONTEXT_FIELDS, DEFAULT_COMPONENT, KEEP, MAX_BYTES, Record, Rotating, route};
use crate::trajectory::timestamp;

/// How the process logs.
#[derive(Debug, Clone, Default)]
pub struct LogSetup {
    /// The Prometheus home; without one, records are only shown on stderr.
    pub home: Option<PathBuf>,
    /// `PMT_LOG` or `RUST_LOG` (the launch environment's `log_filter()`).
    pub env_filter: Option<String>,
    /// Secret values of the launch environment.
    pub redactor: Redactor,
    /// Print WARN and above to stderr in human form (an interactive
    /// terminal).
    pub stderr: bool,
}

/// Adjusts a running logger.
#[derive(Debug, Clone)]
pub struct LogHandle {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    home: Option<PathBuf>,
    /// Set from the environment: then `set_level` changes nothing.
    directives: Option<Targets>,
    level: RwLock<LevelFilter>,
    redactor: Redactor,
    profile_redactors: RwLock<HashMap<String, Redactor>>,
    stderr: bool,
    files: Mutex<Rotating>,
}

impl LogHandle {
    /// Applies `logging.level` unless `PMT_LOG`/`RUST_LOG` set the filter.
    pub fn set_level(&self, level: LogLevel) {
        if self.shared.directives.is_none() {
            *self
                .shared
                .level
                .write()
                .unwrap_or_else(PoisonError::into_inner) = level_filter(level);
        }
    }

    /// Records of `profile` are also redacted with `redactor` (the
    /// profile's secret store values).
    pub fn register_profile(&self, profile: &str, redactor: Redactor) {
        self.shared
            .profile_redactors
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(profile.to_owned(), redactor);
    }
}

static HANDLE: OnceLock<LogHandle> = OnceLock::new();

/// Installs the logger as the process-wide subscriber (once; later calls
/// return the first handle).
pub fn install(setup: LogSetup) -> &'static LogHandle {
    HANDLE.get_or_init(|| {
        let (layer, handle) = layer(setup);
        // Fails only when a subscriber is already set (tests): keep it.
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer));
        handle
    })
}

/// The installed logger, if any.
#[must_use]
pub fn handle() -> Option<&'static LogHandle> {
    HANDLE.get()
}

/// The layer and its handle, for a subscriber the caller builds (tests use
/// `tracing::subscriber::with_default`).
#[must_use]
pub fn layer(setup: LogSetup) -> (JsonLayer, LogHandle) {
    let directives = setup.env_filter.as_deref().and_then(|text| {
        let parsed = Targets::from_str(text).ok();
        if parsed.is_none() {
            eprintln!("warning: ignoring invalid log filter '{text}'");
        }
        parsed
    });
    let shared = Arc::new(Shared {
        home: setup.home,
        directives,
        level: RwLock::new(level_filter(LogLevel::default())),
        redactor: setup.redactor,
        profile_redactors: RwLock::default(),
        stderr: setup.stderr && std::io::stderr().is_terminal(),
        files: Mutex::new(Rotating::new(MAX_BYTES, KEEP)),
    });
    (
        JsonLayer {
            shared: Arc::clone(&shared),
        },
        LogHandle { shared },
    )
}

fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

/// Writes every enabled event as a JSON record.
#[derive(Debug)]
pub struct JsonLayer {
    shared: Arc<Shared>,
}

impl JsonLayer {
    /// The same layer with other rotation limits (tests).
    #[must_use]
    pub fn with_rotation(self, max_bytes: u64, keep: usize) -> Self {
        *self
            .shared
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Rotating::new(max_bytes, keep);
        self
    }

    fn event_enabled(&self, metadata: &Metadata<'_>) -> bool {
        match &self.shared.directives {
            Some(targets) => targets.would_enable(metadata.target(), metadata.level()),
            None => {
                *metadata.level()
                    <= *self
                        .shared
                        .level
                        .read()
                        .unwrap_or_else(PoisonError::into_inner)
            }
        }
    }

    fn redactor_for(&self, profile: Option<&str>) -> Redactor {
        let profiles = self
            .shared
            .profile_redactors
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match profile.and_then(|p| profiles.get(p)) {
            Some(extra) => self.shared.redactor.merged(extra),
            None => self.shared.redactor.clone(),
        }
    }

    fn emit(&self, record: &Record, level: Level) {
        if self.shared.stderr && level <= Level::WARN {
            let label = if level == Level::ERROR {
                "error"
            } else {
                "warning"
            };
            let _ = writeln!(std::io::stderr(), "{label}: {}", record.msg);
        }
        let Some(home) = &self.shared.home else {
            return;
        };
        let path = route(home, &record.component, record.profile.as_deref());
        let Ok(mut line) = serde_json::to_vec(record) else {
            return;
        };
        line.push(b'\n');
        // A log write that fails has nowhere to be reported.
        let _ = self
            .shared
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(&path, &line);
    }
}

/// The context fields a span sets.
#[derive(Debug, Default, Clone)]
struct SpanContext(HashMap<&'static str, String>);

impl Visit for SpanContext {
    fn record_str(&mut self, field: &Field, value: &str) {
        if CONTEXT_FIELDS.contains(&field.name()) {
            self.0.insert(field.name(), value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if CONTEXT_FIELDS.contains(&field.name()) {
            self.0.insert(field.name(), format!("{value:?}"));
        }
    }
}

/// An event's message and other fields.
#[derive(Debug, Default)]
struct EventFields {
    context: SpanContext,
    msg: String,
    fields: Map<String, Value>,
}

impl EventFields {
    fn put(&mut self, field: &Field, value: Value) {
        match (field.name(), value) {
            ("message", Value::String(text)) => self.msg = text,
            (name, Value::String(text)) if CONTEXT_FIELDS.contains(&name) => {
                self.context.0.insert(field.name(), text);
            }
            (name, value) => {
                self.fields.insert(name.to_owned(), value);
            }
        }
    }
}

impl Visit for EventFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::String(value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, Value::String(format!("{value:?}")));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }
}

impl<S> Layer<S> for JsonLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        // The level can change at run time (`set_level`).
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        metadata.is_span() || self.event_enabled(metadata)
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut context = SpanContext::default();
        attrs.record(&mut context);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(context);
        }
    }

    fn on_record(&self, id: &Id, values: &SpanRecord<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(context) = extensions.get_mut::<SpanContext>() {
                values.record(context);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut context = SpanContext::default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(fields) = span.extensions().get::<SpanContext>() {
                    context.0.extend(fields.0.clone());
                }
            }
        }
        let mut fields = EventFields::default();
        event.record(&mut fields);
        context.0.extend(fields.context.0.drain());
        let record = build(&context, fields, *event.metadata().level());
        let redacted = redact(
            record,
            &self.redactor_for(context.0.get("profile").map(String::as_str)),
        );
        self.emit(&redacted, *event.metadata().level());
    }
}

fn build(context: &SpanContext, fields: EventFields, level: Level) -> Record {
    let get = |key: &str| context.0.get(key).cloned();
    Record {
        ts: timestamp(jiff::Timestamp::now()),
        level: level.as_str().to_lowercase(),
        component: get("component").unwrap_or_else(|| DEFAULT_COMPONENT.to_owned()),
        profile: get("profile"),
        session_id: get("session_id"),
        turn_id: get("turn_id"),
        trace_id: get("trace_id"),
        msg: fields.msg,
        fields: fields.fields,
    }
}

fn redact(mut record: Record, redactor: &Redactor) -> Record {
    record.msg = redactor.redact_all(&record.msg);
    for value in record.fields.values_mut() {
        if let Value::String(text) = value {
            *text = redactor.redact_all(text);
        }
    }
    record
}
