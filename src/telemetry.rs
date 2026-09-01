//! This crate's own copy of `skilj-demo/src/bin/server.rs`'s reference
//! telemetry wiring - see that file's own doc comment for the full
//! reasoning, repeated only in summary here. `skilj-core`/`skilj-rest`/
//! `skilj` only ever emit `tracing` spans/events and record measurements
//! through `opentelemetry::global::meter(...)` - none of them install a
//! subscriber or a `MeterProvider`. [`init`] is where a real consuming
//! app actually does that: always a console `fmt` layer, and - only
//! when `OTEL_EXPORTER_OTLP_ENDPOINT` is set - all three OpenTelemetry
//! signals over OTLP/HTTP: trace spans via `tracing-opentelemetry`'s
//! layer, every `tracing::info!`/`warn!`/etc. event also exported as a
//! correlated OTel log record via `opentelemetry-appender-tracing`'s
//! bridge, and the counters/histograms already recorded throughout
//! `skilj-core`/`skilj-rest`/`skilj` exported as OTel metrics - no new
//! call sites needed anywhere in this crate's own `helpdesk.rs` for any
//! of the three.
//!
//! Shared across all three binaries (`server`/`alerter`/`scheduler`),
//! each passing its own `service_name` - so they show up as three
//! distinct services in whatever OTLP backend is on the other end,
//! rather than one undifferentiated blob. `cargo run` works with no
//! collector present either way.

/// The three OTel SDK providers [`init`] builds when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set - held by each binary's `main`
/// for the process's lifetime purely so none is dropped early (dropping
/// any one tears down its own exporter). Three separate signals, three
/// separate providers/exporters/processors - OpenTelemetry doesn't
/// unify trace, log, and metric export the way it does for the
/// `tracing` layers consuming all three.
pub struct TelemetryProviders {
    tracer_provider: opentelemetry_sdk::trace::SdkTracerProvider,
    logger_provider: opentelemetry_sdk::logs::SdkLoggerProvider,
    meter_provider: opentelemetry_sdk::metrics::SdkMeterProvider,
}

impl TelemetryProviders {
    /// Flushes and tears down all three exporters. Without this, a
    /// `SIGINT`/`SIGTERM` (or just falling off the end of `main`) drops
    /// the process (and everything still sitting in each batch
    /// processor's buffer, unexported) instantly. Each provider's own
    /// `.shutdown()` blocks until its buffered data is flushed or its
    /// own internal timeout elapses; a failure here is logged, not
    /// propagated - there's nothing a binary's own exit path could
    /// usefully do about a flush that didn't fully succeed beyond
    /// saying so.
    pub fn shutdown(&self) {
        if let Err(e) = self.tracer_provider.shutdown() {
            tracing::warn!(error = %e, "failed to shut down the trace provider");
        }
        if let Err(e) = self.logger_provider.shutdown() {
            tracing::warn!(error = %e, "failed to shut down the log provider");
        }
        if let Err(e) = self.meter_provider.shutdown() {
            tracing::warn!(error = %e, "failed to shut down the meter provider");
        }
    }
}

/// Installs the process-wide `tracing` subscriber - always a console
/// `fmt` layer (`RUST_LOG`, defaulting to `info`), and, only when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set, two more layers stacked on top
/// (trace export, log export) plus a separate metrics export pipeline.
///
/// **Must be the very first thing a binary's `main` does** -
/// `opentelemetry::global::meter()`'s own doc comment warns that a
/// `Meter` obtained before the provider changes will never reflect a
/// later change, and every counter/histogram in `skilj-core`/
/// `skilj-rest`/`skilj` is a `LazyLock` that calls `global::meter()` the
/// first time it's actually used (the first command processed, the
/// first event appended, the first HTTP request) - always strictly
/// after this has already run, as long as this runs first.
///
/// `env_filter` is registered first, ahead of the `tracing_subscriber`
/// layers - in `tracing-subscriber`, a callsite's enabled/disabled
/// decision is global to the whole subscriber stack, not per-layer, so
/// this is what makes `RUST_LOG` gate the trace/log OTLP exporters too,
/// not just the console. It has no bearing on the separate metrics
/// pipeline, which isn't `tracing`-driven at all.
pub fn init(service_name: &str) -> Option<TelemetryProviders> {
    use tracing_subscriber::prelude::*;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer();

    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_err() {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .init();
        return None;
    }

    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.to_string())
        .build();

    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
        .expect(
            "OTEL_EXPORTER_OTLP_ENDPOINT is set - building the OTLP/HTTP span exporter \
             shouldn't fail this early (no network call happens yet)",
        );
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let tracer =
        opentelemetry::trace::TracerProvider::tracer(&tracer_provider, service_name.to_string());
    let otel_trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .build()
        .expect(
            "OTEL_EXPORTER_OTLP_ENDPOINT is set - building the OTLP/HTTP log exporter \
             shouldn't fail this early (no network call happens yet)",
        );
    let logger_provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(resource.clone())
        .build();
    let otel_log_layer =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(otel_trace_layer)
        .with(otel_log_layer)
        .init();

    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .build()
        .expect(
            "OTEL_EXPORTER_OTLP_ENDPOINT is set - building the OTLP/HTTP metric exporter \
             shouldn't fail this early (no network call happens yet)",
        );
    let mut metric_reader_builder =
        opentelemetry_sdk::metrics::PeriodicReader::builder(metric_exporter);
    if std::env::var("OTEL_METRIC_EXPORT_INTERVAL").is_err() {
        // The SDK's own default export interval is 60s
        // (opentelemetry_sdk::metrics::periodic_reader::DEFAULT_INTERVAL) -
        // verified against a real collector: with it left alone, a 20s
        // demo run exported zero metrics. Far too slow for this
        // project's whole point (a dashboard that's actually moving -
        // see the provisioned dashboard's own 5s `refresh`), so this
        // shortens it to 10s whenever the caller hasn't already set the
        // standard `OTEL_METRIC_EXPORT_INTERVAL` env var themselves
        // (milliseconds) - `PeriodicReaderBuilder::new` already reads
        // that env var internally; `.with_interval` below would
        // silently override it if called unconditionally.
        metric_reader_builder =
            metric_reader_builder.with_interval(std::time::Duration::from_secs(10));
    }
    let metric_reader = metric_reader_builder.build();
    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(metric_reader)
        .with_resource(resource)
        .build();
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    Some(TelemetryProviders {
        tracer_provider,
        logger_provider,
        meter_provider,
    })
}
