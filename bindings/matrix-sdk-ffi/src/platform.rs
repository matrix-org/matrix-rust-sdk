use std::collections::HashMap;
#[cfg(not(target_os = "android"))]
use std::io;

#[cfg(target_os = "android")]
use android as platform_impl;
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_core::future::BoxFuture;
#[cfg(target_os = "ios")]
use ios as platform_impl;
use opentelemetry::{
    sdk::{
        trace::{BatchMessage, TraceRuntime, Tracer},
        util::tokio_interval_stream,
        Resource,
    },
    KeyValue,
};
use opentelemetry_otlp::{Protocol, WithExportConfig};
#[cfg(not(any(target_os = "ios", target_os = "android")))]
use other as platform_impl;
use tokio::runtime::Handle;
#[cfg(not(target_os = "android"))]
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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

#[cfg(not(target_os = "android"))]
fn setup_tracing_helper(configuration: String, colors: bool) {
    tracing_subscriber::registry()
        .with(EnvFilter::new(configuration))
        .with(
            fmt::layer()
                .with_file(true)
                .with_line_number(true)
                .with_ansi(colors)
                .with_writer(io::stderr),
        )
        .init();
}

#[cfg(not(target_os = "android"))]
fn setup_otlp_tracing_helper(
    configuration: String,
    colors: bool,
    client_name: String,
    user: String,
    password: String,
    otlp_endpoint: String,
) -> anyhow::Result<()> {
    let otlp_tracer = create_otlp_tracer(user, password, otlp_endpoint, client_name)?;
    let otlp_layer = tracing_opentelemetry::layer().with_tracer(otlp_tracer);

    tracing_subscriber::registry()
        .with(EnvFilter::new(configuration))
        .with(
            fmt::layer()
                .with_file(true)
                .with_line_number(true)
                .with_ansi(colors)
                .with_writer(io::stderr),
        )
        .with(otlp_layer)
        .init();

    Ok(())
}

#[cfg(target_os = "android")]
mod android {
    use tracing_subscriber::{prelude::*, EnvFilter};

    fn log_panics() {
        std::env::set_var("RUST_BACKTRACE", "1");
        log_panics::init();
    }

    pub fn setup_tracing(configuration: String) {
        log_panics();

        tracing_subscriber::registry()
            .with(EnvFilter::new(configuration))
            .with(
                tracing_android::layer("org.matrix.rust.sdk")
                    .expect("Could not configure the Android tracing layer"),
            )
            .init();
    }

    pub fn setup_otlp_tracing(
        configuration: String,
        client_name: String,
        user: String,
        password: String,
        otlp_endpoint: String,
    ) -> anyhow::Result<()> {
        log_panics();

        let otlp_tracer = super::create_otlp_tracer(user, password, otlp_endpoint, client_name)?;
        let otlp_layer = tracing_opentelemetry::layer().with_tracer(otlp_tracer);

        tracing_subscriber::registry()
            .with(EnvFilter::new(configuration))
            .with(
                tracing_android::layer("org.matrix.rust.sdk")
                    .expect("Could not configure the Android tracing layer"),
            )
            .with(otlp_layer)
            .init();

        Ok(())
    }
}

#[cfg(target_os = "ios")]
mod ios {
    pub fn setup_tracing(configuration: String) {
        super::setup_tracing_helper(configuration, false);
    }

    pub fn setup_otlp_tracing(
        configuration: String,
        client_name: String,
        user: String,
        password: String,
        otlp_endpoint: String,
    ) -> anyhow::Result<()> {
        super::setup_otlp_tracing_helper(
            configuration,
            false,
            client_name,
            user,
            password,
            otlp_endpoint,
        )
    }
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod other {
    pub fn setup_tracing(configuration: String) {
        super::setup_tracing_helper(configuration, true);
    }

    pub fn setup_otlp_tracing(
        configuration: String,
        client_name: String,
        user: String,
        password: String,
        otlp_endpoint: String,
    ) -> anyhow::Result<()> {
        super::setup_otlp_tracing_helper(
            configuration,
            true,
            client_name,
            user,
            password,
            otlp_endpoint,
        )
    }
}

#[uniffi::export]
pub fn setup_tracing(filter: String) {
    platform_impl::setup_tracing(filter)
}

#[uniffi::export]
pub fn setup_otlp_tracing(
    filter: String,
    client_name: String,
    user: String,
    password: String,
    otlp_endpoint: String,
) {
    platform_impl::setup_otlp_tracing(filter, client_name, user, password, otlp_endpoint)
        .expect("Couldn't configure the OpenTelemetry tracer")
}
