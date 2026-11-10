//! The OTLP exporter (spec: observability, Opt-in OpenTelemetry export):
//! spans wait in a bounded queue of [`QUEUE_CAPACITY`] items that drops
//! the oldest on overflow; one worker thread with its own runtime drains
//! it in batches and posts a cumulative metrics snapshot every
//! [`METRICS_EVERY`]. Queuing is a push under a short lock, so a slow or
//! unreachable collector never blocks a turn; items of a failed export are
//! dropped and counted in `prometheus.otel.dropped` (no retry pile-up).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_config::schema::OtelProtocol;
use pmt_core::error::{Error, codes};
use prost::Message as _;
use tokio::sync::{Notify, oneshot};

use super::proto;
use crate::metrics::{self, global};

/// Items queued before the oldest is dropped.
pub const QUEUE_CAPACITY: usize = 4_096;
/// Spans sent per request at most.
const BATCH: usize = 512;
/// How long a span may wait for its batch.
const BATCH_DELAY: Duration = Duration::from_millis(500);
/// Time between two metrics exports.
pub const METRICS_EVERY: Duration = Duration::from_secs(15);
/// One export request's deadline.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// What the exporter sends where.
#[derive(Debug, Clone)]
pub struct ExportSettings {
    /// The collector's base URL.
    pub endpoint: String,
    /// `http/protobuf` or `grpc`.
    pub protocol: OtelProtocol,
    /// Request headers (from `observability.otel.header_secrets`).
    pub headers: Vec<(String, String)>,
    /// `observability.otel.capture_content`.
    pub capture_content: bool,
    /// `observability.otel.propagate_external`.
    pub propagate_external: bool,
    /// Proxy and CA settings of the launch profile.
    pub outbound: pmt_net::OutboundPolicy,
    /// `service.*` resource attributes (name, version, instance).
    pub resource: Vec<(String, String)>,
}

/// Counters of the export posture (`prometheus status --exports`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ExportStats {
    /// Items waiting.
    pub queued: u64,
    /// Items dropped since start.
    pub dropped: u64,
    /// Items exported since start.
    pub exported: u64,
    /// When the last export succeeded (RFC 3339).
    pub last_success: Option<String>,
    /// The last export failure.
    pub last_error: Option<String>,
}

#[derive(Debug)]
struct Inner {
    host: String,
    protocol: OtelProtocol,
    capture_content: bool,
    propagate_external: bool,
    queue: Mutex<VecDeque<proto::Span>>,
    wake: Notify,
    flushes: Mutex<Vec<oneshot::Sender<()>>>,
    dropped: AtomicU64,
    exported: AtomicU64,
    last: Mutex<(Option<String>, Option<String>)>,
}

/// A running OTLP exporter (cheap to clone).
#[derive(Debug, Clone)]
pub struct Exporter {
    inner: Arc<Inner>,
}

impl Exporter {
    /// Starts the worker thread.
    ///
    /// # Errors
    ///
    /// `net.client_init` / `net.extra_ca` when the HTTP client cannot be
    /// built; `config.invalid` for an endpoint that is not a URL.
    pub fn start(settings: ExportSettings) -> Result<Self, Error> {
        let url = reqwest::Url::parse(&settings.endpoint).map_err(|e| {
            Error::new(
                &codes::CONFIG_INVALID,
                format!("observability.otel.endpoint {:?}: {e}", settings.endpoint),
            )
        })?;
        let mut builder = pmt_net::client_builder(&settings.outbound)?.timeout(EXPORT_TIMEOUT);
        if settings.protocol == OtelProtocol::Grpc {
            builder = builder.http2_prior_knowledge();
        }
        let client = builder.build().map_err(|e| {
            Error::new(
                &codes::NET_CLIENT_INIT,
                format!("cannot build the OTLP client: {e}"),
            )
        })?;
        let exporter = Self {
            inner: Arc::new(Inner {
                host: url.host_str().unwrap_or_default().to_owned(),
                protocol: settings.protocol,
                capture_content: settings.capture_content,
                propagate_external: settings.propagate_external,
                queue: Mutex::default(),
                wake: Notify::new(),
                flushes: Mutex::default(),
                dropped: AtomicU64::new(0),
                exported: AtomicU64::new(0),
                last: Mutex::default(),
            }),
        };
        let worker = Worker {
            inner: Arc::clone(&exporter.inner),
            client,
            settings,
        };
        std::thread::Builder::new()
            .name("otlp-export".to_owned())
            .spawn(move || worker.run())
            .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, format!("otlp export thread: {e}")))?;
        Ok(exporter)
    }

    /// Queues a finished span; the oldest queued item is dropped when the
    /// queue is full.
    pub fn push_span(&self, span: proto::Span) {
        let mut queue = self
            .inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if queue.len() >= QUEUE_CAPACITY {
            queue.pop_front();
            self.inner.drop_items(1);
        }
        queue.push_back(span);
        drop(queue);
        self.inner.wake.notify_one();
    }

    /// Sends everything queued and a metrics snapshot now, waiting at most
    /// `within`.
    pub fn flush_blocking(&self, within: Duration) {
        let (tx, rx) = oneshot::channel();
        self.inner
            .flushes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tx);
        self.inner.wake.notify_one();
        let deadline = std::time::Instant::now() + within;
        let mut rx = rx;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => return,
            }
        }
    }

    /// `capture_content` is on.
    #[must_use]
    pub fn captures_content(&self) -> bool {
        self.inner.capture_content
    }

    /// `propagate_external` is on.
    #[must_use]
    pub fn propagates_external(&self) -> bool {
        self.inner.propagate_external
    }

    /// The collector's host (never its credentials).
    #[must_use]
    pub fn host(&self) -> &str {
        &self.inner.host
    }

    /// The wire protocol.
    #[must_use]
    pub fn protocol(&self) -> OtelProtocol {
        self.inner.protocol
    }

    /// Queue and export counters.
    #[must_use]
    pub fn stats(&self) -> ExportStats {
        let (last_success, last_error) = self
            .inner
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        ExportStats {
            queued: self
                .inner
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len() as u64,
            dropped: self.inner.dropped.load(Ordering::Relaxed),
            exported: self.inner.exported.load(Ordering::Relaxed),
            last_success,
            last_error,
        }
    }
}

impl Inner {
    fn drop_items(&self, count: u64) {
        self.dropped.fetch_add(count, Ordering::Relaxed);
        #[allow(clippy::cast_precision_loss)] // counts far below 2^52
        global().add(metrics::OTEL_DROPPED, &[], count as f64);
    }

    fn take_batch(&self) -> Vec<proto::Span> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let n = queue.len().min(BATCH);
        queue.drain(..n).collect()
    }

    fn outcome(&self, result: Result<(), String>, items: u64) {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let health = crate::health::global();
        match result {
            Ok(()) => {
                self.exported.fetch_add(items, Ordering::Relaxed);
                last.0 = Some(crate::trajectory::timestamp(jiff::Timestamp::now()));
                drop(last);
                health.report(None, "otel", crate::health::State::Ok, &self.host);
            }
            Err(why) => {
                drop(last);
                self.drop_items(items);
                tracing::debug!("otlp export failed: {why}");
                health.report(None, "otel", crate::health::State::Degraded, &why);
                self.last.lock().unwrap_or_else(PoisonError::into_inner).1 = Some(why);
            }
        }
    }
}

struct Worker {
    inner: Arc<Inner>,
    client: reqwest::Client,
    settings: ExportSettings,
}

impl Worker {
    fn run(self) {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            tracing::error!("otlp export disabled: cannot start its runtime");
            return;
        };
        runtime.block_on(self.serve());
    }

    async fn serve(self) {
        let mut next_metrics = tokio::time::Instant::now() + METRICS_EVERY;
        loop {
            tokio::select! {
                () = self.inner.wake.notified() => tokio::time::sleep(Duration::from_millis(20)).await,
                () = tokio::time::sleep(BATCH_DELAY) => {}
            }
            let flushes: Vec<_> = std::mem::take(
                &mut *self
                    .inner
                    .flushes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            );
            loop {
                let batch = self.inner.take_batch();
                if batch.is_empty() {
                    break;
                }
                let items = batch.len() as u64;
                let result = self.send_spans(batch).await;
                self.inner.outcome(result, items);
            }
            if !flushes.is_empty() || tokio::time::Instant::now() >= next_metrics {
                next_metrics = tokio::time::Instant::now() + METRICS_EVERY;
                let result = self.send_metrics().await;
                self.inner.outcome(result, 0);
            }
            for flush in flushes {
                let _ = flush.send(());
            }
        }
    }

    fn resource(&self) -> proto::Resource {
        proto::Resource {
            attributes: self
                .settings
                .resource
                .iter()
                .map(|(k, v)| proto::kv(k, v))
                .collect(),
        }
    }

    fn scope() -> proto::InstrumentationScope {
        proto::InstrumentationScope {
            name: "prometheus".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    async fn send_spans(&self, spans: Vec<proto::Span>) -> Result<(), String> {
        let request = proto::ExportTraceServiceRequest {
            resource_spans: vec![proto::ResourceSpans {
                resource: Some(self.resource()),
                scope_spans: vec![proto::ScopeSpans {
                    scope: Some(Self::scope()),
                    spans,
                }],
            }],
        };
        self.post(Signal::Traces, request.encode_to_vec()).await
    }

    async fn send_metrics(&self) -> Result<(), String> {
        let request = proto::ExportMetricsServiceRequest {
            resource_metrics: vec![proto::ResourceMetrics {
                resource: Some(self.resource()),
                scope_metrics: vec![proto::ScopeMetrics {
                    scope: Some(Self::scope()),
                    metrics: super::metrics_proto::metrics(&global().snapshot()),
                }],
            }],
        };
        self.post(Signal::Metrics, request.encode_to_vec()).await
    }

    async fn post(&self, signal: Signal, body: Vec<u8>) -> Result<(), String> {
        let base = self.settings.endpoint.trim_end_matches('/');
        let mut request = match self.settings.protocol {
            OtelProtocol::HttpProtobuf => self
                .client
                .post(format!("{base}{}", signal.http_path()))
                .header("content-type", "application/x-protobuf")
                .body(body),
            OtelProtocol::Grpc => {
                let mut framed = Vec::with_capacity(body.len() + 5);
                framed.push(0);
                framed.extend_from_slice(
                    &u32::try_from(body.len()).unwrap_or(u32::MAX).to_be_bytes(),
                );
                framed.extend_from_slice(&body);
                self.client
                    .post(format!("{base}{}", signal.grpc_path()))
                    .header("content-type", "application/grpc")
                    .header("te", "trailers")
                    .body(framed)
            }
        };
        for (name, value) in &self.settings.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        let grpc_status = response
            .headers()
            .get("grpc-status")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let _ = response.bytes().await;
        match grpc_status.as_deref() {
            _ if !status.is_success() => Err(format!("HTTP {}", status.as_u16())),
            Some(code) if code != "0" => Err(format!("gRPC status {code}")),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Signal {
    Traces,
    Metrics,
}

impl Signal {
    const fn http_path(self) -> &'static str {
        match self {
            Self::Traces => "/v1/traces",
            Self::Metrics => "/v1/metrics",
        }
    }

    const fn grpc_path(self) -> &'static str {
        match self {
            Self::Traces => "/opentelemetry.proto.collector.trace.v1.TraceService/Export",
            Self::Metrics => "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export",
        }
    }
}
