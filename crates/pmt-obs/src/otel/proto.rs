//! The subset of the OTLP protobuf messages (opentelemetry-proto v1:
//! `common`, `resource`, `trace`, `metrics` and the two collector export
//! requests) this export sends, declared with the `prost` derive. Field
//! numbers and types follow the upstream `.proto` files exactly; fields
//! the export never sets are left out (protobuf readers skip absent
//! fields), so these structs decode real collector payloads too.

#![allow(missing_docs)] // mirrors the upstream schema field by field

use prost::{Message, Oneof};

#[derive(Clone, PartialEq, Message)]
pub struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4")]
    pub value: Option<any_value::Value>,
}

pub mod any_value {
    use super::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        StringValue(String),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        #[prost(int64, tag = "3")]
        IntValue(i64),
        #[prost(double, tag = "4")]
        DoubleValue(f64),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<AnyValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InstrumentationScope {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Resource {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<KeyValue>,
}

/// `ExportTraceServiceRequest` (same shape as `TracesData`).
#[derive(Clone, PartialEq, Message)]
pub struct ExportTraceServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_spans: Vec<ResourceSpans>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ResourceSpans {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_spans: Vec<ScopeSpans>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ScopeSpans {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub spans: Vec<Span>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Span {
    #[prost(bytes = "vec", tag = "1")]
    pub trace_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub span_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub parent_span_id: Vec<u8>,
    #[prost(string, tag = "5")]
    pub name: String,
    /// `SpanKind`: 1 internal, 2 server, 3 client.
    #[prost(int32, tag = "6")]
    pub kind: i32,
    #[prost(fixed64, tag = "7")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "8")]
    pub end_time_unix_nano: u64,
    #[prost(message, repeated, tag = "9")]
    pub attributes: Vec<KeyValue>,
    #[prost(message, optional, tag = "15")]
    pub status: Option<Status>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Status {
    #[prost(string, tag = "2")]
    pub message: String,
    /// `StatusCode`: 0 unset, 1 ok, 2 error.
    #[prost(int32, tag = "3")]
    pub code: i32,
}

/// `ExportMetricsServiceRequest` (same shape as `MetricsData`).
#[derive(Clone, PartialEq, Message)]
pub struct ExportMetricsServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_metrics: Vec<ResourceMetrics>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ResourceMetrics {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_metrics: Vec<ScopeMetrics>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ScopeMetrics {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub metrics: Vec<Metric>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Metric {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(string, tag = "3")]
    pub unit: String,
    #[prost(oneof = "metric::Data", tags = "5, 7, 9")]
    pub data: Option<metric::Data>,
}

pub mod metric {
    use super::{Gauge, Histogram, Oneof, Sum};

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Data {
        #[prost(message, tag = "5")]
        Gauge(Gauge),
        #[prost(message, tag = "7")]
        Sum(Sum),
        #[prost(message, tag = "9")]
        Histogram(Histogram),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Gauge {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<NumberDataPoint>,
}

/// `AggregationTemporality` cumulative.
pub const CUMULATIVE: i32 = 2;

#[derive(Clone, PartialEq, Message)]
pub struct Sum {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<NumberDataPoint>,
    #[prost(int32, tag = "2")]
    pub aggregation_temporality: i32,
    #[prost(bool, tag = "3")]
    pub is_monotonic: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Histogram {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<HistogramDataPoint>,
    #[prost(int32, tag = "2")]
    pub aggregation_temporality: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct NumberDataPoint {
    #[prost(fixed64, tag = "2")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(double, tag = "4")]
    pub as_double: f64,
    #[prost(message, repeated, tag = "7")]
    pub attributes: Vec<KeyValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HistogramDataPoint {
    #[prost(fixed64, tag = "2")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(fixed64, tag = "4")]
    pub count: u64,
    #[prost(double, optional, tag = "5")]
    pub sum: Option<f64>,
    #[prost(fixed64, repeated, tag = "6")]
    pub bucket_counts: Vec<u64>,
    #[prost(double, repeated, tag = "7")]
    pub explicit_bounds: Vec<f64>,
    #[prost(message, repeated, tag = "9")]
    pub attributes: Vec<KeyValue>,
}

/// A string attribute.
#[must_use]
pub fn kv(key: &str, value: impl Into<String>) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.into())),
        }),
    }
}

/// An integer attribute.
#[must_use]
pub fn kv_int(key: &str, value: u64) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(
                i64::try_from(value).unwrap_or(i64::MAX),
            )),
        }),
    }
}

/// A floating-point attribute.
#[must_use]
pub fn kv_double(key: &str, value: f64) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::DoubleValue(value)),
        }),
    }
}

/// The string value of `key` among `attributes` (tests, the posture
/// report).
#[must_use]
pub fn string_attr<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attributes.iter().find(|a| a.key == key).and_then(|a| {
        match a.value.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(s) => Some(s.as_str()),
            _ => None,
        }
    })
}
