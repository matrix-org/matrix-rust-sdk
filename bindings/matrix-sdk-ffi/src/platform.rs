use std::collections::HashMap;

use base64::{engine::general_purpose::STANDARD, Engine};
use futures_core::future::BoxFuture;
use opentelemetry::{
    sdk::{
        trace::{BatchMessage, TraceRuntime, Tracer},
        util::tokio_interval_stream,
        Resource,
    },
    KeyValue,
};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use tokio::runtime::Handle;
use tracing_core::Subscriber;
use tracing_subscriber::{fmt, prelude::*, registry::LookupSpan, EnvFilter, Layer};

use crate::RUNTIME;

#[derive(Clone, Debug)]
struct TracingRuntime {
    runtime: Handle,
}

impl opentelemetry::runtime::Runtime for TracingRuntime {
    type Interval = tokio_stream::wrappers::IntervalStream;
    type Delay = ::std::pin::Pin<Box<tokio::time::Sleep>>;

    fn interval(&self, duration: std::time::Duration) -> Self::Interval {
        let _guard = self.runtime.enter();
        tokio_interval_stream(duration)
    }

    fn spawn(&self, future: BoxFuture<'static, ()>) {
        #[allow(clippy::let_underscore_future)]
        let _ = self.runtime.spawn(future);
    }

    fn delay(&self, duration: std::time::Duration) -> Self::Delay {
        let _guard = self.runtime.enter();
        Box::pin(tokio::time::sleep(duration))
    }
}

impl TraceRuntime for TracingRuntime {
    type Receiver = tokio_stream::wrappers::ReceiverStream<BatchMessage>;
    type Sender = tokio::sync::mpsc::Sender<BatchMessage>;

    fn batch_message_channel(&self, capacity: usize) -> (Self::Sender, Self::Receiver) {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        (sender, tokio_stream::wrappers::ReceiverStream::new(receiver))
    }
}

pub fn create_otlp_tracer(
    user: String,
    password: String,
    otlp_endpoint: String,
    client_name: String,
) -> anyhow::Result<Tracer> {
    let runtime = RUNTIME.handle().to_owned();

    let auth = STANDARD.encode(format!("{user}:{password}"));
    let headers = HashMap::from([("Authorization".to_owned(), format!("Basic {auth}"))]);
    let http_client = matrix_sdk::reqwest::ClientBuilder::new().build()?;

    let exporter = opentelemetry_otlp::new_exporter()
        .http()
        .with_http_client(http_client)
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(otlp_endpoint)
        .with_headers(headers);

    let tracer_runtime = TracingRuntime { runtime: runtime.to_owned() };

    let _guard = runtime.enter();
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(
            opentelemetry::sdk::trace::config()
                .with_resource(Resource::new(vec![KeyValue::new("service.name", client_name)])),
        )
        .install_batch(tracer_runtime)?;

    Ok(tracer)
}

#[cfg(target_os = "android")]
pub fn log_panics() {
    std::env::set_var("RUST_BACKTRACE", "1");
    log_panics::init();
}

fn fmt_layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    #[cfg(not(target_os = "android"))]
    return fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_ansi(!cfg!(target_os = "ios"))
        .with_writer(std::io::stderr);

    #[cfg(target_os = "android")]
    return fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .with_level(false)
        .without_time()
        .with_writer(paranoid_android::AndroidLogMakeWriter::new(
            "org.matrix.rust.sdk".to_owned(),
        ));
}

#[uniffi::export]
pub fn setup_tracing(filter: String) {
    #[cfg(target_os = "android")]
    log_panics();

    tracing_subscriber::registry().with(EnvFilter::new(filter)).with(fmt_layer()).init();
}

#[uniffi::export]
pub fn setup_otlp_tracing(
    filter: String,
    client_name: String,
    user: String,
    password: String,
    otlp_endpoint: String,
) {
    #[cfg(target_os = "android")]
    log_panics();

    let otlp_tracer = create_otlp_tracer(user, password, otlp_endpoint, client_name)
        .expect("Couldn't configure the OpenTelemetry tracer");
    let otlp_layer = tracing_opentelemetry::layer().with_tracer(otlp_tracer);

    tracing_subscriber::registry()
        .with(EnvFilter::new(filter))
        .with(fmt_layer())
        .with(otlp_layer)
        .init();
}
