//! The Prometheus text exposition format (version 0.0.4) of a
//! [`Snapshot`], served at the daemon's `GET /metrics` (spec:
//! observability, Local metrics scrape endpoint). Dots become underscores,
//! counters get `_total`, histograms `_bucket`/`_sum`/`_count`.

use std::fmt::Write as _;

use super::{BUCKETS_MS, Kind, Point, Snapshot, Value};

/// The `Content-Type` of the rendering.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Renders every metric of `snapshot`.
#[must_use]
pub fn render(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    for metric in &snapshot.metrics {
        let base = metric.name.replace('.', "_");
        let (name, kind) = match metric.kind {
            Kind::Counter => (format!("{base}_total"), "counter"),
            Kind::Histogram => (base.clone(), "histogram"),
            Kind::Gauge => (base.clone(), "gauge"),
        };
        let _ = writeln!(out, "# HELP {name} {}", metric.description);
        let _ = writeln!(out, "# TYPE {name} {kind}");
        for point in &metric.points {
            match &point.value {
                Value::Number(v) => {
                    let _ = writeln!(out, "{name}{} {}", labels(point, None), number(*v));
                }
                Value::Distribution(d) => {
                    let mut cumulative = 0;
                    for (i, count) in d.buckets.iter().enumerate() {
                        cumulative += count;
                        let le = BUCKETS_MS
                            .get(i)
                            .map_or_else(|| "+Inf".to_owned(), |b| number(*b));
                        let _ = writeln!(
                            out,
                            "{base}_bucket{} {cumulative}",
                            labels(point, Some(&le))
                        );
                    }
                    let _ = writeln!(out, "{base}_sum{} {}", labels(point, None), number(d.sum));
                    let _ = writeln!(out, "{base}_count{} {}", labels(point, None), d.count);
                }
            }
        }
    }
    out
}

fn labels(point: &Point, le: Option<&str>) -> String {
    let mut pairs: Vec<String> = point
        .attributes
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    if let Some(le) = le {
        pairs.push(format!("le=\"{le}\""));
    }
    if pairs.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", pairs.join(","))
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ERRORS, MODEL_LATENCY_MS, Registry};
    use super::render;

    #[test]
    fn counters_and_histograms_render_in_exposition_format() {
        let registry = Registry::default();
        registry.add(ERRORS, &[("kind", "auth")], 1.0);
        registry.observe(MODEL_LATENCY_MS, &[("provider", "mock")], 30.0);
        let text = render(&registry.snapshot());
        assert!(
            text.contains("# TYPE prometheus_errors_total counter"),
            "{text}"
        );
        assert!(
            text.contains("prometheus_errors_total{kind=\"auth\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("prometheus_model_latency_ms_bucket{provider=\"mock\",le=\"50\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("prometheus_model_latency_ms_bucket{provider=\"mock\",le=\"+Inf\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("prometheus_model_latency_ms_count{provider=\"mock\"} 1"),
            "{text}"
        );
    }
}
