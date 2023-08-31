use std::{collections::HashMap, fmt::Debug};

use base64::{engine::general_purpose::STANDARD, Engine};
use futures_core::future::BoxFuture;
use opentelemetry::{
    sdk::{runtime::RuntimeChannel, trace::Tracer, util::tokio_interval_stream, Resource},
    KeyValue,
};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use tokio::runtime::Handle;
use tracing_core::Subscriber;
use tracing_subscriber::{
    fmt::{self, time::FormatTime, FormatEvent, FormatFields, FormattedFields},
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
    EnvFilter, Layer,
};

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

impl<T: Debug + Send> RuntimeChannel<T> for TracingRuntime {
    type Receiver = tokio_stream::wrappers::ReceiverStream<T>;
    type Sender = tokio::sync::mpsc::Sender<T>;

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

fn text_layers<S>(config: TracingConfiguration) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    // Adjusted version of tracing_subscriber::fmt::Format
    struct EventFormatter {
        display_timestamp: bool,
        display_level: bool,
    }

    impl EventFormatter {
        fn new() -> Self {
            Self { display_timestamp: true, display_level: true }
        }

        #[cfg(target_os = "android")]
        fn for_logcat() -> Self {
            // Level and time are already captured by logcat separately
            Self { display_timestamp: false, display_level: false }
        }

        fn format_timestamp(&self, writer: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
            if fmt::time::SystemTime.format_time(writer).is_err() {
                writer.write_str("<unknown time>")?;
            }
            Ok(())
        }

        fn write_filename(
            &self,
            writer: &mut fmt::format::Writer<'_>,
            filename: &str,
        ) -> std::fmt::Result {
            const CRATES_IO_PATH_MATCHER: &str = ".cargo/registry/src/index.crates.io";
            let crates_io_filename = filename
                .split_once(CRATES_IO_PATH_MATCHER)
                .and_then(|(_, rest)| rest.split_once('/').map(|(_, rest)| rest));

            if let Some(filename) = crates_io_filename {
                writer.write_str("<crates.io>/")?;
                writer.write_str(filename)
            } else {
                writer.write_str(filename)
            }
        }
    }

    impl<S, N> FormatEvent<S, N> for EventFormatter
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
        N: for<'a> FormatFields<'a> + 'static,
    {
        fn format_event(
            &self,
            ctx: &fmt::FmtContext<'_, S, N>,
            mut writer: fmt::format::Writer<'_>,
            event: &tracing_core::Event<'_>,
        ) -> std::fmt::Result {
            let meta = event.metadata();

            if self.display_timestamp {
                self.format_timestamp(&mut writer)?;
                writer.write_char(' ')?;
            }

            if self.display_level {
                // For info and warn, add a padding space to the left
                write!(writer, "{:>5} ", meta.level())?;
            }

            write!(writer, "{}: ", meta.target())?;

            ctx.format_fields(writer.by_ref(), event)?;

            if let Some(filename) = meta.file() {
                writer.write_str(" | ")?;
                self.write_filename(&mut writer, filename)?;
                if let Some(line_number) = meta.line() {
                    write!(writer, ":{line_number}")?;
                }
            }

            if let Some(scope) = ctx.event_scope() {
                writer.write_str(" | spans: ")?;
                let mut first = true;

                for span in scope.from_root() {
                    if !first {
                        writer.write_str(" > ")?;
                    }
                    first = false;
                    write!(writer, "{}", span.metadata().name())?;

                    let ext = span.extensions();
                    if let Some(fields) = &ext.get::<FormattedFields<N>>() {
                        if !fields.is_empty() {
                            write!(writer, "{{{fields}}}")?;
                        }
                    }
                }
            }

            writeln!(writer)
        }
    }

    let file_layer = config.write_to_files.map(|c| {
        fmt::layer()
            .event_format(EventFormatter::new())
            // EventFormatter doesn't support ANSI colors anyways, but the
            // default field formatter does, which is unhelpful for iOS +
            // Android logs, but enabled by default.
            .with_ansi(false)
            .with_writer(tracing_appender::rolling::hourly(c.path, c.file_prefix))
    });

    Layer::and_then(
        file_layer,
        config.write_to_stdout_or_system.then(|| {
            #[cfg(not(target_os = "android"))]
            return fmt::layer()
                .event_format(EventFormatter::new())
                // See comment above.
                .with_ansi(false)
                .with_writer(std::io::stderr);

            #[cfg(target_os = "android")]
            return fmt::layer()
                .event_format(EventFormatter::for_logcat())
                // See comment above.
                .with_ansi(false)
                .with_writer(paranoid_android::AndroidLogMakeWriter::new(
                    "org.matrix.rust.sdk".to_owned(),
                ));
        }),
    )
}

#[derive(uniffi::Record)]
pub struct TracingFileConfiguration {
    path: String,
    file_prefix: String,
}

#[derive(uniffi::Record)]
pub struct TracingConfiguration {
    filter: String,
    /// Controls whether to print to stdout or, equivalent, the system logs on
    /// Android.
    write_to_stdout_or_system: bool,
    write_to_files: Option<TracingFileConfiguration>,
}

#[uniffi::export]
pub fn setup_tracing(config: TracingConfiguration) {
    #[cfg(target_os = "android")]
    log_panics();

    tracing_subscriber::registry()
        .with(EnvFilter::new(&config.filter))
        .with(text_layers(config))
        .init();
}

#[derive(uniffi::Record)]
pub struct OtlpTracingConfiguration {
    client_name: String,
    user: String,
    password: String,
    otlp_endpoint: String,
    filter: String,
    /// Controls whether to print to stdout or, equivalent, the system logs on
    /// Android.
    write_to_stdout_or_system: bool,
    write_to_files: Option<TracingFileConfiguration>,
}

#[uniffi::export]
pub fn setup_otlp_tracing(config: OtlpTracingConfiguration) {
    #[cfg(target_os = "android")]
    log_panics();

    let otlp_tracer =
        create_otlp_tracer(config.user, config.password, config.otlp_endpoint, config.client_name)
            .expect("Couldn't configure the OpenTelemetry tracer");
    let otlp_layer = tracing_opentelemetry::layer().with_tracer(otlp_tracer);

    tracing_subscriber::registry()
        .with(EnvFilter::new(&config.filter))
        .with(text_layers(TracingConfiguration {
            filter: config.filter,
            write_to_stdout_or_system: config.write_to_stdout_or_system,
            write_to_files: config.write_to_files,
        }))
        .with(otlp_layer)
        .init();
}
