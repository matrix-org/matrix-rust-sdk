use std::collections::HashMap;

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

use crate::RUNTIME;

#[cfg(target_os = "android")]
mod android {
    use android_logger::{Config, FilterBuilder};
    use tracing::log::Level;

    pub fn setup_tracing(filter: String) {
        std::env::set_var("RUST_BACKTRACE", "1");

        log_panics::init();

        let log_config = Config::default()
            .with_min_level(Level::Trace)
            .with_tag("matrix-rust-sdk")
            .with_filter(FilterBuilder::new().parse(&filter).build());

        android_logger::init_once(log_config);
    }
}

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

#[cfg(target_os = "ios")]
mod ios {
    use std::io;

    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    pub fn setup_tracing(configuration: String) {
        tracing_subscriber::registry()
            .with(EnvFilter::new(configuration))
            .with(fmt::layer().with_ansi(false).with_writer(io::stderr))
            .init();
    }

    pub fn setup_otlp_tracing(
        configuration: String,
        user: String,
        password: String,
        otlp_endpoint: String,
    ) -> anyhow::Result<()> {
        let otlp_tracer =
            super::create_otlp_tracer(user, password, otlp_endpoint, "element-x-ios".to_owned())?;

        let otlp_layer = tracing_opentelemetry::layer().with_tracer(otlp_tracer);

        tracing_subscriber::registry()
            .with(EnvFilter::new(configuration))
            .with(fmt::layer().with_ansi(false).with_writer(io::stderr))
            .with(otlp_layer)
            .init();

        Ok(())
    }
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod other {
    use std::io;

    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    pub fn setup_tracing(configuration: String) {
        tracing_subscriber::registry()
            .with(EnvFilter::new(configuration))
            .with(fmt::layer().with_ansi(true).with_writer(io::stderr))
            .init();
    }
}

#[uniffi::export]
pub fn setup_tracing(filter: String) {
    platform_impl::setup_tracing(filter)
}

#[cfg(target_os = "ios")]
#[uniffi::export]
pub fn setup_otlp_tracing(filter: String, user: String, password: String, otlp_endpoint: String) {
    platform_impl::setup_otlp_tracing(filter, user, password, otlp_endpoint)
        .expect("Couldn't configure the OpenTelemetry tracer")
}
