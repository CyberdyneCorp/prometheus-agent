//! The metrics catalog as OTLP metrics: counters as cumulative monotonic
//! sums, histograms as cumulative explicit-bucket histograms, gauges as
//! gauges.

use super::proto::{self, metric::Data};
use crate::metrics::{BUCKETS_MS, Kind, Metric, Point, Snapshot, Value};

/// Every metric of `snapshot` that has data points.
#[must_use]
pub fn metrics(snapshot: &Snapshot) -> Vec<proto::Metric> {
    snapshot
        .metrics
        .iter()
        .filter(|m| !m.points.is_empty())
        .map(|m| metric(m, snapshot.start_unix_nano, snapshot.time_unix_nano))
        .collect()
}

fn metric(m: &Metric, start: u64, time: u64) -> proto::Metric {
    let numbers = || {
        m.points
            .iter()
            .filter_map(|p| match p.value {
                Value::Number(v) => Some(proto::NumberDataPoint {
                    start_time_unix_nano: start,
                    time_unix_nano: time,
                    as_double: v,
                    attributes: attributes(p),
                }),
                Value::Distribution(_) => None,
            })
            .collect()
    };
    let data = match m.kind {
        Kind::Counter => Data::Sum(proto::Sum {
            data_points: numbers(),
            aggregation_temporality: proto::CUMULATIVE,
            is_monotonic: true,
        }),
        Kind::Gauge => Data::Gauge(proto::Gauge {
            data_points: numbers(),
        }),
        Kind::Histogram => Data::Histogram(proto::Histogram {
            data_points: m
                .points
                .iter()
                .filter_map(|p| match &p.value {
                    Value::Distribution(d) => Some(proto::HistogramDataPoint {
                        start_time_unix_nano: start,
                        time_unix_nano: time,
                        count: d.count,
                        sum: Some(d.sum),
                        bucket_counts: d.buckets.clone(),
                        explicit_bounds: BUCKETS_MS.to_vec(),
                        attributes: attributes(p),
                    }),
                    Value::Number(_) => None,
                })
                .collect(),
            aggregation_temporality: proto::CUMULATIVE,
        }),
    };
    proto::Metric {
        name: m.name.to_owned(),
        description: m.description.to_owned(),
        unit: m.unit.to_owned(),
        data: Some(data),
    }
}

fn attributes(point: &Point) -> Vec<proto::KeyValue> {
    point
        .attributes
        .iter()
        .map(|(k, v)| proto::kv(k, v))
        .collect()
}
