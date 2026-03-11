#![forbid(unsafe_code)]

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

#[cfg(feature = "otel")]
use opentelemetry::trace::Span as _;
#[cfg(feature = "otel")]
use opentelemetry_otlp::WithExportConfig;

pub const TELEMETRY_MODE_ENV: &str = "MARS_OTEL_MODE";
pub const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

static TELEMETRY_ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryMode {
    Off,
    Required,
}

impl TelemetryMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "off" => Some(Self::Off),
            "required" => Some(Self::Required),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("invalid telemetry mode '{value}': expected 'off' or 'required'")]
    InvalidMode { value: String },
    #[error("telemetry mode 'required' is not available: binary is built without 'otel' feature")]
    RequiredUnavailable,
    #[error("telemetry mode 'required' requires {0} to be set")]
    MissingRequiredEnv(&'static str),
    #[cfg(feature = "otel")]
    #[error("failed to build OTLP trace exporter: {0}")]
    TraceExporter(String),
    #[cfg(feature = "otel")]
    #[error("failed to build OTLP metrics exporter: {0}")]
    MetricsExporter(String),
}

#[derive(Debug, Clone, Copy)]
pub struct ServiceIdentity {
    pub service_name: &'static str,
    pub service_version: &'static str,
    pub component: &'static str,
}

impl ServiceIdentity {
    #[must_use]
    pub const fn new(
        service_name: &'static str,
        service_version: &'static str,
        component: &'static str,
    ) -> Self {
        Self {
            service_name,
            service_version,
            component,
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "otel"), allow(dead_code))]
pub struct Attribute {
    key: &'static str,
    value: AttributeValue,
}

impl Attribute {
    #[must_use]
    pub fn string(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: AttributeValue::String(value.into()),
        }
    }

    #[must_use]
    pub const fn bool(key: &'static str, value: bool) -> Self {
        Self {
            key,
            value: AttributeValue::Bool(value),
        }
    }

    #[must_use]
    pub const fn u64(key: &'static str, value: u64) -> Self {
        Self {
            key,
            value: AttributeValue::U64(value),
        }
    }

    #[must_use]
    pub const fn i64(key: &'static str, value: i64) -> Self {
        Self {
            key,
            value: AttributeValue::I64(value),
        }
    }

    #[must_use]
    pub const fn f64(key: &'static str, value: f64) -> Self {
        Self {
            key,
            value: AttributeValue::F64(value),
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "otel"), allow(dead_code))]
enum AttributeValue {
    String(String),
    Bool(bool),
    U64(u64),
    I64(i64),
    F64(f64),
}

#[derive(Debug)]
pub struct TelemetryRuntime {
    enabled: bool,
    #[cfg(feature = "otel")]
    tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    #[cfg(feature = "otel")]
    meter_provider: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
}

impl TelemetryRuntime {
    pub fn init(identity: ServiceIdentity) -> Result<Self, TelemetryError> {
        let mode = telemetry_mode_from_env()?;

        #[cfg(not(feature = "otel"))]
        {
            let _ = identity;
            if mode == TelemetryMode::Required {
                return Err(TelemetryError::RequiredUnavailable);
            }

            TELEMETRY_ENABLED.store(false, Ordering::Relaxed);
            return Ok(Self { enabled: false });
        }

        #[cfg(feature = "otel")]
        {
            if mode == TelemetryMode::Off {
                TELEMETRY_ENABLED.store(false, Ordering::Relaxed);
                return Ok(Self {
                    enabled: false,
                    tracer_provider: None,
                    meter_provider: None,
                });
            }

            let endpoint = env::var(OTEL_ENDPOINT_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or(TelemetryError::MissingRequiredEnv(OTEL_ENDPOINT_ENV))?;

            use opentelemetry::KeyValue;

            let resource = opentelemetry_sdk::Resource::builder_empty()
                .with_attributes([
                    KeyValue::new("service.name", identity.service_name),
                    KeyValue::new("service.version", identity.service_version),
                    KeyValue::new("service.instance.id", uuid::Uuid::new_v4().to_string()),
                    KeyValue::new("mars.component", identity.component),
                    KeyValue::new("os.type", std::env::consts::OS),
                    KeyValue::new("host.arch", std::env::consts::ARCH),
                ])
                .build();

            let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint.clone())
                .build()
                .map_err(|error| TelemetryError::TraceExporter(error.to_string()))?;

            let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(trace_exporter)
                .build();
            opentelemetry::global::set_tracer_provider(tracer_provider.clone());

            let metrics_exporter = opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .build()
                .map_err(|error| TelemetryError::MetricsExporter(error.to_string()))?;

            let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
                .with_resource(resource)
                .with_periodic_exporter(metrics_exporter)
                .build();
            opentelemetry::global::set_meter_provider(meter_provider.clone());

            TELEMETRY_ENABLED.store(true, Ordering::Relaxed);
            Ok(Self {
                enabled: true,
                tracer_provider: Some(tracer_provider),
                meter_provider: Some(meter_provider),
            })
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn tracer(&self, scope: &'static str) -> TelemetryTracer {
        if !self.enabled {
            return TelemetryTracer::disabled();
        }
        TelemetryTracer::enabled(scope)
    }

    #[must_use]
    pub fn meter(&self, scope: &'static str) -> TelemetryMeter {
        if !self.enabled {
            return TelemetryMeter::disabled();
        }
        TelemetryMeter::enabled(scope)
    }
}

impl Drop for TelemetryRuntime {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        {
            if let Some(meter_provider) = self.meter_provider.take() {
                let _ = meter_provider.shutdown();
            }
            if let Some(tracer_provider) = self.tracer_provider.take() {
                let _ = tracer_provider.shutdown();
            }
        }
        TELEMETRY_ENABLED.store(false, Ordering::Relaxed);
    }
}

#[must_use]
pub fn telemetry_enabled() -> bool {
    TELEMETRY_ENABLED.load(Ordering::Relaxed)
}

#[must_use]
pub fn global_tracer(scope: &'static str) -> TelemetryTracer {
    if telemetry_enabled() {
        TelemetryTracer::enabled(scope)
    } else {
        TelemetryTracer::disabled()
    }
}

#[must_use]
pub fn global_meter(scope: &'static str) -> TelemetryMeter {
    if telemetry_enabled() {
        TelemetryMeter::enabled(scope)
    } else {
        TelemetryMeter::disabled()
    }
}

pub fn telemetry_mode_from_env() -> Result<TelemetryMode, TelemetryError> {
    match env::var(TELEMETRY_MODE_ENV) {
        Ok(raw) => {
            TelemetryMode::parse(raw.trim()).ok_or(TelemetryError::InvalidMode { value: raw })
        }
        Err(env::VarError::NotPresent) => Ok(TelemetryMode::Off),
        Err(env::VarError::NotUnicode(_)) => Err(TelemetryError::InvalidMode {
            value: "<non-unicode>".to_string(),
        }),
    }
}

#[derive(Debug, Clone)]
pub struct TelemetryTracer {
    #[cfg(feature = "otel")]
    scope: Option<&'static str>,
}

impl TelemetryTracer {
    fn disabled() -> Self {
        Self {
            #[cfg(feature = "otel")]
            scope: None,
        }
    }

    fn enabled(_scope: &'static str) -> Self {
        Self {
            #[cfg(feature = "otel")]
            scope: Some(_scope),
        }
    }

    #[must_use]
    pub fn start_span(&self, name: &'static str, attrs: &[Attribute]) -> SpanGuard {
        #[cfg(feature = "otel")]
        {
            if let Some(scope) = self.scope {
                use opentelemetry::trace::{TraceContextExt, Tracer};

                let tracer = opentelemetry::global::tracer(scope);
                let mut span = tracer.start(name);
                span.set_attributes(to_key_values(attrs));
                let cx = opentelemetry::Context::current_with_span(span);
                return SpanGuard { context: Some(cx) };
            }
        }

        let _ = (name, attrs);
        SpanGuard::disabled()
    }

    #[must_use]
    pub fn start_child_span(
        &self,
        parent: &SpanGuard,
        name: &'static str,
        attrs: &[Attribute],
    ) -> SpanGuard {
        #[cfg(feature = "otel")]
        {
            if let (Some(scope), Some(parent_context)) = (self.scope, parent.context.as_ref()) {
                use opentelemetry::trace::{TraceContextExt, Tracer};

                let tracer = opentelemetry::global::tracer(scope);
                let mut span = tracer.start_with_context(name, parent_context);
                span.set_attributes(to_key_values(attrs));
                let cx = opentelemetry::Context::current_with_span(span);
                return SpanGuard { context: Some(cx) };
            }
        }

        let _ = (parent, name, attrs);
        SpanGuard::disabled()
    }
}

#[derive(Debug)]
pub struct SpanGuard {
    #[cfg(feature = "otel")]
    context: Option<opentelemetry::Context>,
}

impl SpanGuard {
    fn disabled() -> Self {
        Self {
            #[cfg(feature = "otel")]
            context: None,
        }
    }

    pub fn set_attributes(&self, attrs: &[Attribute]) {
        #[cfg(feature = "otel")]
        {
            use opentelemetry::trace::TraceContextExt;

            if let Some(context) = self.context.as_ref() {
                context.span().set_attributes(to_key_values(attrs));
            }
        }

        let _ = attrs;
    }

    pub fn set_status_ok(&self) {
        #[cfg(feature = "otel")]
        {
            use opentelemetry::trace::{Status, TraceContextExt};

            if let Some(context) = self.context.as_ref() {
                context.span().set_status(Status::Ok);
            }
        }
    }

    pub fn set_status_error(&self, description: impl Into<String>) {
        #[cfg(feature = "otel")]
        {
            use opentelemetry::trace::{Status, TraceContextExt};

            if let Some(context) = self.context.as_ref() {
                context.span().set_status(Status::Error {
                    description: description.into().into(),
                });
            }
        }

        #[cfg(not(feature = "otel"))]
        {
            let _ = description.into();
        }
    }

    pub fn end(&mut self) {
        #[cfg(feature = "otel")]
        {
            use opentelemetry::trace::TraceContextExt;

            if let Some(context) = self.context.take() {
                context.span().end();
            }
        }
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        self.end();
    }
}

#[derive(Debug, Clone)]
pub struct TelemetryMeter {
    #[cfg(feature = "otel")]
    meter: Option<opentelemetry::metrics::Meter>,
}

impl TelemetryMeter {
    fn disabled() -> Self {
        Self {
            #[cfg(feature = "otel")]
            meter: None,
        }
    }

    fn enabled(_scope: &'static str) -> Self {
        Self {
            #[cfg(feature = "otel")]
            meter: Some(opentelemetry::global::meter(_scope)),
        }
    }

    #[must_use]
    pub fn u64_counter(
        &self,
        name: &'static str,
        description: &'static str,
        unit: &'static str,
    ) -> U64Counter {
        #[cfg(feature = "otel")]
        {
            if let Some(meter) = self.meter.as_ref() {
                let counter = meter
                    .u64_counter(name)
                    .with_description(description)
                    .with_unit(unit)
                    .build();
                return U64Counter {
                    counter: Some(counter),
                };
            }
        }

        let _ = (name, description, unit);
        U64Counter::disabled()
    }

    #[must_use]
    pub fn u64_histogram(
        &self,
        name: &'static str,
        description: &'static str,
        unit: &'static str,
    ) -> U64Histogram {
        #[cfg(feature = "otel")]
        {
            if let Some(meter) = self.meter.as_ref() {
                let histogram = meter
                    .u64_histogram(name)
                    .with_description(description)
                    .with_unit(unit)
                    .build();
                return U64Histogram {
                    histogram: Some(histogram),
                };
            }
        }

        let _ = (name, description, unit);
        U64Histogram::disabled()
    }

    #[must_use]
    pub fn f64_histogram(
        &self,
        name: &'static str,
        description: &'static str,
        unit: &'static str,
    ) -> F64Histogram {
        #[cfg(feature = "otel")]
        {
            if let Some(meter) = self.meter.as_ref() {
                let histogram = meter
                    .f64_histogram(name)
                    .with_description(description)
                    .with_unit(unit)
                    .build();
                return F64Histogram {
                    histogram: Some(histogram),
                };
            }
        }

        let _ = (name, description, unit);
        F64Histogram::disabled()
    }
}

#[derive(Debug, Clone)]
pub struct U64Counter {
    #[cfg(feature = "otel")]
    counter: Option<opentelemetry::metrics::Counter<u64>>,
}

impl U64Counter {
    fn disabled() -> Self {
        Self {
            #[cfg(feature = "otel")]
            counter: None,
        }
    }

    pub fn add(&self, value: u64, attrs: &[Attribute]) {
        #[cfg(feature = "otel")]
        {
            if let Some(counter) = self.counter.as_ref() {
                counter.add(value, &to_key_values(attrs));
                return;
            }
        }

        let _ = (value, attrs);
    }
}

#[derive(Debug, Clone)]
pub struct U64Histogram {
    #[cfg(feature = "otel")]
    histogram: Option<opentelemetry::metrics::Histogram<u64>>,
}

impl U64Histogram {
    fn disabled() -> Self {
        Self {
            #[cfg(feature = "otel")]
            histogram: None,
        }
    }

    pub fn record(&self, value: u64, attrs: &[Attribute]) {
        #[cfg(feature = "otel")]
        {
            if let Some(histogram) = self.histogram.as_ref() {
                histogram.record(value, &to_key_values(attrs));
                return;
            }
        }

        let _ = (value, attrs);
    }
}

#[derive(Debug, Clone)]
pub struct F64Histogram {
    #[cfg(feature = "otel")]
    histogram: Option<opentelemetry::metrics::Histogram<f64>>,
}

impl F64Histogram {
    fn disabled() -> Self {
        Self {
            #[cfg(feature = "otel")]
            histogram: None,
        }
    }

    pub fn record(&self, value: f64, attrs: &[Attribute]) {
        #[cfg(feature = "otel")]
        {
            if let Some(histogram) = self.histogram.as_ref() {
                histogram.record(value, &to_key_values(attrs));
                return;
            }
        }

        let _ = (value, attrs);
    }
}

#[cfg(feature = "otel")]
fn to_key_values(attrs: &[Attribute]) -> Vec<opentelemetry::KeyValue> {
    attrs
        .iter()
        .map(|attr| {
            let value = match &attr.value {
                AttributeValue::String(value) => opentelemetry::Value::from(value.clone()),
                AttributeValue::Bool(value) => opentelemetry::Value::from(*value),
                AttributeValue::U64(value) => opentelemetry::Value::from(*value as i64),
                AttributeValue::I64(value) => opentelemetry::Value::from(*value),
                AttributeValue::F64(value) => opentelemetry::Value::from(*value),
            };
            opentelemetry::KeyValue::new(attr.key, value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::TelemetryMode;

    #[test]
    fn parser_accepts_supported_modes() {
        assert_eq!(TelemetryMode::parse("off"), Some(TelemetryMode::Off));
        assert_eq!(
            TelemetryMode::parse("required"),
            Some(TelemetryMode::Required)
        );
    }

    #[test]
    fn parser_rejects_invalid_mode() {
        assert!(TelemetryMode::parse("auto").is_none());
        assert!(TelemetryMode::parse("OFF").is_none());
    }
}
