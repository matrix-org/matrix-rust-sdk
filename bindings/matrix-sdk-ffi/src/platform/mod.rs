use std::sync::OnceLock;
#[cfg(feature = "sentry")]
use std::sync::{atomic::AtomicBool, Arc};

#[cfg(feature = "sentry")]
use tracing::warn;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
#[cfg(feature = "sentry")]
use tracing_core::Level;
use tracing_core::Subscriber;
use tracing_subscriber::{
    field::RecordFields,
    fmt::{
        self,
        format::{DefaultFields, Writer},
        time::FormatTime,
        FormatEvent, FormatFields, FormattedFields,
    },
    layer::{Layered, SubscriberExt as _},
    registry::LookupSpan,
    reload::{self, Handle},
    util::SubscriberInitExt as _,
    EnvFilter, Layer, Registry,
};

#[cfg(feature = "sentry")]
use crate::tracing::BRIDGE_SPAN_NAME;
use crate::{error::ClientError, tracing::LogLevel};

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

                write!(writer, "{}", span.name())?;

                if let Some(fields) = &span.extensions().get::<FormattedFields<N>>() {
                    if !fields.is_empty() {
                        write!(writer, "{{{fields}}}")?;
                    }
                }
            }
        }

        writeln!(writer)
    }
}

// Another fields formatter is necessary because of this bug
// https://github.com/tokio-rs/tracing/issues/1372. Using a new
// formatter for the fields forces to record them in different span
// extensions, and thus remove the duplicated fields in the span.
#[derive(Default)]
struct FieldsFormatterForFiles(DefaultFields);

impl<'writer> FormatFields<'writer> for FieldsFormatterForFiles {
    fn format_fields<R: RecordFields>(
        &self,
        writer: Writer<'writer>,
        fields: R,
    ) -> std::fmt::Result {
        self.0.format_fields(writer, fields)
    }
}

type ReloadHandle = Handle<
    tracing_subscriber::fmt::Layer<
        Layered<EnvFilter, Registry>,
        FieldsFormatterForFiles,
        EventFormatter,
        RollingFileAppender,
    >,
    Layered<EnvFilter, Registry>,
>;

fn text_layers(
    config: TracingConfiguration,
) -> (impl Layer<Layered<EnvFilter, Registry>>, Option<ReloadHandle>) {
    let (file_layer, reload_handle) = config
        .write_to_files
        .map(|c| {
            let layer = make_file_layer(c);
            reload::Layer::new(layer)
        })
        .unzip();

    let layers = Layer::and_then(
        file_layer,
        config.write_to_stdout_or_system.then(|| {
            // Another fields formatter is necessary because of this bug
            // https://github.com/tokio-rs/tracing/issues/1372. Using a new
            // formatter for the fields forces to record them in different span
            // extensions, and thus remove the duplicated fields in the span.
            #[derive(Default)]
            struct FieldsFormatterFormStdoutOrSystem(DefaultFields);

            impl<'writer> FormatFields<'writer> for FieldsFormatterFormStdoutOrSystem {
                fn format_fields<R: RecordFields>(
                    &self,
                    writer: Writer<'writer>,
                    fields: R,
                ) -> std::fmt::Result {
                    self.0.format_fields(writer, fields)
                }
            }

            #[cfg(not(target_os = "android"))]
            return fmt::layer()
                .fmt_fields(FieldsFormatterFormStdoutOrSystem::default())
                .event_format(EventFormatter::new())
                // See comment above.
                .with_ansi(false)
                .with_writer(std::io::stderr);

            #[cfg(target_os = "android")]
            return fmt::layer()
                .fmt_fields(FieldsFormatterFormStdoutOrSystem::default())
                .event_format(EventFormatter::for_logcat())
                // See comment above.
                .with_ansi(false)
                .with_writer(paranoid_android::AndroidLogMakeWriter::new(
                    "org.matrix.rust.sdk".to_owned(),
                ));
        }),
    );

    (layers, reload_handle)
}

fn make_file_layer(
    file_configuration: TracingFileConfiguration,
) -> tracing_subscriber::fmt::Layer<
    Layered<EnvFilter, Registry, Registry>,
    FieldsFormatterForFiles,
    EventFormatter,
    RollingFileAppender,
> {
    let mut builder = RollingFileAppender::builder()
        .rotation(Rotation::HOURLY)
        .filename_prefix(&file_configuration.file_prefix);

    if let Some(max_files) = file_configuration.max_files {
        builder = builder.max_log_files(max_files as usize)
    }
    if let Some(file_suffix) = file_configuration.file_suffix {
        builder = builder.filename_suffix(file_suffix)
    }

    let writer =
        builder.build(&file_configuration.path).expect("Failed to create a rolling file appender.");

    fmt::layer()
        .fmt_fields(FieldsFormatterForFiles::default())
        .event_format(EventFormatter::new())
        // EventFormatter doesn't support ANSI colors anyways, but the
        // default field formatter does, which is unhelpful for iOS +
        // Android logs, but enabled by default.
        .with_ansi(false)
        .with_writer(writer)
}

/// Configuration to save logs to (rotated) log-files.
#[derive(uniffi::Record)]
pub struct TracingFileConfiguration {
    /// Base location for all the log files.
    path: String,

    /// Prefix for the log files' names.
    file_prefix: String,

    /// Optional suffix for the log file's names.
    file_suffix: Option<String>,

    /// Maximum number of rotated files.
    ///
    /// If not set, there's no max limit, i.e. the number of log files is
    /// unlimited.
    max_files: Option<u64>,
}

#[derive(PartialEq, PartialOrd)]
enum LogTarget {
    // External crates.
    Hyper,

    // FFI modules.
    MatrixSdkFfi,

    // SDK base modules.
    MatrixSdkBaseEventCache,
    MatrixSdkBaseSlidingSync,
    MatrixSdkBaseStoreAmbiguityMap,
    MatrixSdkBaseResponseProcessors,

    // SDK common modules.
    MatrixSdkCommonCrossProcessLock,
    MatrixSdkCommonDeserializedResponses,

    // SDK modules.
    MatrixSdk,
    MatrixSdkClient,
    MatrixSdkCrypto,
    MatrixSdkCryptoAccount,
    MatrixSdkEventCache,
    MatrixSdkEventCacheStore,
    MatrixSdkHttpClient,
    MatrixSdkLatestEvents,
    MatrixSdkOidc,
    MatrixSdkSendQueue,
    MatrixSdkSlidingSync,

    // SDK UI modules.
    MatrixSdkUiTimeline,
    MatrixSdkUiNotificationClient,
}

impl LogTarget {
    fn as_str(&self) -> &'static str {
        match self {
            LogTarget::Hyper => "hyper",
            LogTarget::MatrixSdkFfi => "matrix_sdk_ffi",
            LogTarget::MatrixSdkBaseEventCache => "matrix_sdk_base::event_cache",
            LogTarget::MatrixSdkBaseSlidingSync => "matrix_sdk_base::sliding_sync",
            LogTarget::MatrixSdkBaseStoreAmbiguityMap => "matrix_sdk_base::store::ambiguity_map",
            LogTarget::MatrixSdkBaseResponseProcessors => "matrix_sdk_base::response_processors",
            LogTarget::MatrixSdkCommonCrossProcessLock => "matrix_sdk_common::cross_process_lock",
            LogTarget::MatrixSdkCommonDeserializedResponses => {
                "matrix_sdk_common::deserialized_responses"
            }
            LogTarget::MatrixSdk => "matrix_sdk",
            LogTarget::MatrixSdkClient => "matrix_sdk::client",
            LogTarget::MatrixSdkCrypto => "matrix_sdk_crypto",
            LogTarget::MatrixSdkCryptoAccount => "matrix_sdk_crypto::olm::account",
            LogTarget::MatrixSdkOidc => "matrix_sdk::oidc",
            LogTarget::MatrixSdkHttpClient => "matrix_sdk::http_client",
            LogTarget::MatrixSdkSlidingSync => "matrix_sdk::sliding_sync",
            LogTarget::MatrixSdkEventCache => "matrix_sdk::event_cache",
            LogTarget::MatrixSdkLatestEvents => "matrix_sdk::latest_events",
            LogTarget::MatrixSdkSendQueue => "matrix_sdk::send_queue",
            LogTarget::MatrixSdkEventCacheStore => "matrix_sdk_sqlite::event_cache_store",
            LogTarget::MatrixSdkUiTimeline => "matrix_sdk_ui::timeline",
            LogTarget::MatrixSdkUiNotificationClient => "matrix_sdk_ui::notification_client",
        }
    }
}

const DEFAULT_TARGET_LOG_LEVELS: &[(LogTarget, LogLevel)] = &[
    (LogTarget::Hyper, LogLevel::Warn),
    (LogTarget::MatrixSdkFfi, LogLevel::Info),
    (LogTarget::MatrixSdk, LogLevel::Info),
    (LogTarget::MatrixSdkClient, LogLevel::Trace),
    (LogTarget::MatrixSdkCrypto, LogLevel::Debug),
    (LogTarget::MatrixSdkCryptoAccount, LogLevel::Trace),
    (LogTarget::MatrixSdkOidc, LogLevel::Trace),
    (LogTarget::MatrixSdkHttpClient, LogLevel::Debug),
    (LogTarget::MatrixSdkSlidingSync, LogLevel::Info),
    (LogTarget::MatrixSdkBaseSlidingSync, LogLevel::Info),
    (LogTarget::MatrixSdkUiTimeline, LogLevel::Info),
    (LogTarget::MatrixSdkSendQueue, LogLevel::Info),
    (LogTarget::MatrixSdkEventCache, LogLevel::Info),
    (LogTarget::MatrixSdkLatestEvents, LogLevel::Info),
    (LogTarget::MatrixSdkBaseEventCache, LogLevel::Info),
    (LogTarget::MatrixSdkEventCacheStore, LogLevel::Info),
    (LogTarget::MatrixSdkCommonCrossProcessLock, LogLevel::Warn),
    (LogTarget::MatrixSdkCommonDeserializedResponses, LogLevel::Warn),
    (LogTarget::MatrixSdkBaseStoreAmbiguityMap, LogLevel::Warn),
    (LogTarget::MatrixSdkUiNotificationClient, LogLevel::Info),
    (LogTarget::MatrixSdkBaseResponseProcessors, LogLevel::Debug),
];

const IMMUTABLE_LOG_TARGETS: &[LogTarget] = &[
    LogTarget::Hyper,                           // Too verbose
    LogTarget::MatrixSdk,                       // Too generic
    LogTarget::MatrixSdkFfi,                    // Too verbose
    LogTarget::MatrixSdkCommonCrossProcessLock, // Too verbose
    LogTarget::MatrixSdkBaseStoreAmbiguityMap,  // Too verbose
];

/// A log pack can be used to set the trace log level for a group of multiple
/// log targets at once, for debugging purposes.
#[derive(uniffi::Enum)]
pub enum TraceLogPacks {
    /// Enables all the logs relevant to the event cache.
    EventCache,
    /// Enables all the logs relevant to the send queue.
    SendQueue,
    /// Enables all the logs relevant to the timeline.
    Timeline,
    /// Enables all the logs relevant to the notification client.
    NotificationClient,
    /// Enables all the logs relevant to sync profiling.
    SyncProfiling,
    /// Enables all the logs relevant to the latest events.
    LatestEvents,
}

impl TraceLogPacks {
    // Note: all the log targets returned here must be part of
    // `DEFAULT_TARGET_LOG_LEVELS`.
    fn targets(&self) -> &[LogTarget] {
        match self {
            TraceLogPacks::EventCache => &[
                LogTarget::MatrixSdkEventCache,
                LogTarget::MatrixSdkBaseEventCache,
                LogTarget::MatrixSdkEventCacheStore,
                LogTarget::MatrixSdkCommonCrossProcessLock,
                LogTarget::MatrixSdkCommonDeserializedResponses,
            ],
            TraceLogPacks::SendQueue => &[LogTarget::MatrixSdkSendQueue],
            TraceLogPacks::Timeline => {
                &[LogTarget::MatrixSdkUiTimeline, LogTarget::MatrixSdkCommonDeserializedResponses]
            }
            TraceLogPacks::NotificationClient => &[LogTarget::MatrixSdkUiNotificationClient],
            TraceLogPacks::SyncProfiling => &[
                LogTarget::MatrixSdkSlidingSync,
                LogTarget::MatrixSdkBaseSlidingSync,
                LogTarget::MatrixSdkBaseResponseProcessors,
                LogTarget::MatrixSdkCrypto,
                LogTarget::MatrixSdkCommonCrossProcessLock,
                LogTarget::MatrixSdkCommonDeserializedResponses,
            ],
            TraceLogPacks::LatestEvents => &[
                LogTarget::MatrixSdkLatestEvents,
                LogTarget::MatrixSdkSendQueue,
                LogTarget::MatrixSdkEventCache,
            ],
        }
    }
}

#[cfg(feature = "sentry")]
struct SentryLoggingCtx {
    /// The Sentry client guard, which keeps the Sentry context alive.
    _guard: sentry::ClientInitGuard,

    /// Whether the Sentry layer is enabled or not, at a global level.
    enabled: Arc<AtomicBool>,
}

struct LoggingCtx {
    reload_handle: Option<ReloadHandle>,
    #[cfg(feature = "sentry")]
    sentry: Option<SentryLoggingCtx>,
}

static LOGGING: OnceLock<LoggingCtx> = OnceLock::new();

#[derive(uniffi::Record)]
pub struct TracingConfiguration {
    /// The desired log level.
    log_level: LogLevel,

    /// All the log packs, that will be set to `TRACE` when they're enabled.
    trace_log_packs: Vec<TraceLogPacks>,

    /// Additional targets that the FFI client would like to use.
    ///
    /// This can include, for instance, the target names for created
    /// [`crate::tracing::Span`]. These targets will use the global log level by
    /// default.
    extra_targets: Vec<String>,

    /// Whether to log to stdout, or in the logcat on Android.
    write_to_stdout_or_system: bool,

    /// If set, configures rotated log files where to write additional logs.
    write_to_files: Option<TracingFileConfiguration>,

    /// If set, the Sentry DSN to use for error reporting.
    #[cfg(feature = "sentry")]
    sentry_dsn: Option<String>,
}

impl TracingConfiguration {
    /// Sets up the tracing configuration and return a [`Logger`] instance
    /// holding onto it.
    #[cfg_attr(not(feature = "sentry"), allow(unused_mut))]
    fn build(mut self) -> LoggingCtx {
        // Show full backtraces, if we run into panics.
        std::env::set_var("RUST_BACKTRACE", "1");

        // Log panics.
        log_panics::init();

        let env_filter = build_tracing_filter(&self);

        let logging_ctx;
        #[cfg(feature = "sentry")]
        {
            // Prepare the Sentry layer, if a DSN is provided.
            let (sentry_layer, sentry_logging_ctx) =
                if let Some(sentry_dsn) = self.sentry_dsn.take() {
                    // Initialize the Sentry client with the given options.
                    let sentry_guard = sentry::init((
                        sentry_dsn,
                        sentry::ClientOptions {
                            traces_sampler: Some(Arc::new(|ctx| {
                                // Make sure bridge spans are always uploaded
                                if ctx.name() == BRIDGE_SPAN_NAME {
                                    1.0
                                } else {
                                    0.0
                                }
                            })),
                            attach_stacktrace: true,
                            release: Some(env!("VERGEN_GIT_SHA").into()),
                            ..sentry::ClientOptions::default()
                        },
                    ));

                    let sentry_enabled = Arc::new(AtomicBool::new(true));

                    // Add a Sentry layer to the tracing subscriber.
                    //
                    // Pass custom event and span filters, which will ignore anything, if the Sentry
                    // support has been globally disabled, or if the statement doesn't include a
                    // `sentry` field set to `true`.
                    let sentry_layer = sentry_tracing::layer()
                        .event_filter({
                            let enabled = sentry_enabled.clone();

                            move |metadata| {
                                if enabled.load(std::sync::atomic::Ordering::SeqCst)
                                    && metadata.fields().field("sentry").is_some()
                                {
                                    sentry_tracing::default_event_filter(metadata)
                                } else {
                                    // Ignore the event.
                                    sentry_tracing::EventFilter::Ignore
                                }
                            }
                        })
                        .span_filter({
                            let enabled = sentry_enabled.clone();

                            move |metadata| {
                                if enabled.load(std::sync::atomic::Ordering::SeqCst) {
                                    matches!(
                                        metadata.level(),
                                        &Level::ERROR | &Level::WARN | &Level::INFO | &Level::DEBUG
                                    )
                                } else {
                                    // Ignore, if sentry is globally disabled.
                                    false
                                }
                            }
                        });

                    (
                        Some(sentry_layer),
                        Some(SentryLoggingCtx { _guard: sentry_guard, enabled: sentry_enabled }),
                    )
                } else {
                    (None, None)
                };
            let (text_layers, reload_handle) = crate::platform::text_layers(self);

            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new(&env_filter))
                .with(text_layers)
                .with(sentry_layer)
                .init();
            logging_ctx = LoggingCtx { reload_handle, sentry: sentry_logging_ctx };
        }
        #[cfg(not(feature = "sentry"))]
        {
            let (text_layers, reload_handle) = crate::platform::text_layers(self);
            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new(&env_filter))
                .with(text_layers)
                .init();
            logging_ctx = LoggingCtx { reload_handle };
        }

        // Log the log levels 🧠.
        tracing::info!(env_filter, "Logging has been set up");

        logging_ctx
    }
}

fn build_tracing_filter(config: &TracingConfiguration) -> String {
    // We are intentionally not setting a global log level because we don't want to
    // risk third party crates logging sensitive information.
    // As such we need to make sure that panics will be properly logged.
    // On 2025-01-08, `log_panics` uses the `panic` target, at the error log level.
    let mut filters = vec!["panic=error".to_owned()];

    let global_level = config.log_level;

    DEFAULT_TARGET_LOG_LEVELS.iter().for_each(|(target, default_level)| {
        let level = if IMMUTABLE_LOG_TARGETS.contains(target) {
            // If the target is immutable, keep the log level.
            *default_level
        } else if config.trace_log_packs.iter().any(|pack| pack.targets().contains(target)) {
            // If a log pack includes that target, set the associated log level to TRACE.
            LogLevel::Trace
        } else if *default_level > global_level {
            // If the default level is more verbose than the global level, keep the default.
            *default_level
        } else {
            // Otherwise, use the global level.
            global_level
        };

        filters.push(format!("{}={}", target.as_str(), level.as_str()));
    });

    // Finally append the extra targets requested by the client.
    for target in &config.extra_targets {
        filters.push(format!("{}={}", target, config.log_level.as_str()));
    }

    filters.join(",")
}

/// Sets up logs and the tokio runtime for the current application.
///
/// If `use_lightweight_tokio_runtime` is set to true, this will set up a
/// lightweight tokio runtime, for processes that have memory limitations (like
/// the NSE process on iOS). Otherwise, this can remain false, in which case a
/// multithreaded tokio runtime will be set up.
#[matrix_sdk_ffi_macros::export]
pub fn init_platform(
    config: TracingConfiguration,
    use_lightweight_tokio_runtime: bool,
) -> Result<(), ClientError> {
    #[cfg(all(feature = "js", target_family = "wasm"))]
    {
        console_error_panic_hook::set_once();
    }
    #[cfg(not(target_family = "wasm"))]
    {
        LOGGING.set(config.build()).map_err(|_| ClientError::Generic {
            msg: "logger already initialized".to_owned(),
            details: None,
        })?;

        if use_lightweight_tokio_runtime {
            setup_lightweight_tokio_runtime();
        } else {
            setup_multithreaded_tokio_runtime();
        }
    }

    Ok(())
}

/// Set the global enablement level for the Sentry layer (after the logs have
/// been set up).
#[matrix_sdk_ffi_macros::export]
#[cfg(feature = "sentry")]
pub fn enable_sentry_logging(enabled: bool) {
    if let Some(ctx) = LOGGING.get() {
        if let Some(sentry_ctx) = &ctx.sentry {
            sentry_ctx.enabled.store(enabled, std::sync::atomic::Ordering::SeqCst);
        } else {
            warn!("Sentry logging is not enabled");
        }
    } else {
        // Can't use log statements here, since logging hasn't been enabled yet 🧠
        eprintln!("Logging hasn't been enabled yet");
    };
}

/// Updates the tracing subscriber with a new file writer based on the provided
/// configuration.
///
/// This method will throw if `init_platform` hasn't been called, or if it was
/// called with `write_to_files` set to `None`.
#[matrix_sdk_ffi_macros::export]
pub fn reload_tracing_file_writer(
    configuration: TracingFileConfiguration,
) -> Result<(), ClientError> {
    let Some(logging_context) = LOGGING.get() else {
        return Err(ClientError::Generic {
            msg: "Logging hasn't been initialized yet".to_owned(),
            details: None,
        });
    };

    let Some(reload_handle) = logging_context.reload_handle.as_ref() else {
        return Err(ClientError::Generic {
            msg: "Logging wasn't initialized with a file config".to_owned(),
            details: None,
        });
    };

    let layer = make_file_layer(configuration);
    reload_handle.reload(layer).map_err(|error| ClientError::Generic {
        msg: format!("Failed to reload file config: {error}"),
        details: None,
    })
}

#[cfg(not(target_family = "wasm"))]
fn setup_multithreaded_tokio_runtime() {
    async_compat::set_runtime_builder(Box::new(|| {
        eprintln!("spawning a multithreaded tokio runtime");

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        builder
    }));
}

#[cfg(not(target_family = "wasm"))]
fn setup_lightweight_tokio_runtime() {
    async_compat::set_runtime_builder(Box::new(|| {
        eprintln!("spawning a lightweight tokio runtime");

        // Get the number of available cores through the system, if possible.
        let num_available_cores =
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);

        // The number of worker threads will be either that or 4, whichever is smaller.
        let num_worker_threads = num_available_cores.min(4);

        // Chosen by a fair dice roll.
        let num_blocking_threads = 2;

        // 1 MiB of memory per worker thread. Should be enough for everyone™.
        let max_memory_bytes = 1024 * 1024;

        let mut builder = tokio::runtime::Builder::new_multi_thread();

        builder
            .enable_all()
            .worker_threads(num_worker_threads)
            .thread_stack_size(max_memory_bytes)
            .max_blocking_threads(num_blocking_threads);

        builder
    }));
}

#[cfg(test)]
mod tests {
    use similar_asserts::assert_eq;

    use super::build_tracing_filter;
    use crate::platform::TraceLogPacks;

    #[test]
    fn test_default_tracing_filter() {
        let config = super::TracingConfiguration {
            log_level: super::LogLevel::Error,
            trace_log_packs: Vec::new(),
            extra_targets: vec!["super_duper_app".to_owned()],
            write_to_stdout_or_system: true,
            write_to_files: None,
            #[cfg(feature = "sentry")]
            sentry_dsn: None,
        };

        let filter = build_tracing_filter(&config);

        assert_eq!(
            filter,
            r#"panic=error,
            hyper=warn,
            matrix_sdk_ffi=info,
            matrix_sdk=info,
            matrix_sdk::client=trace,
            matrix_sdk_crypto=debug,
            matrix_sdk_crypto::olm::account=trace,
            matrix_sdk::oidc=trace,
            matrix_sdk::http_client=debug,
            matrix_sdk::sliding_sync=info,
            matrix_sdk_base::sliding_sync=info,
            matrix_sdk_ui::timeline=info,
            matrix_sdk::send_queue=info,
            matrix_sdk::event_cache=info,
            matrix_sdk::latest_events=info,
            matrix_sdk_base::event_cache=info,
            matrix_sdk_sqlite::event_cache_store=info,
            matrix_sdk_common::cross_process_lock=warn,
            matrix_sdk_common::deserialized_responses=warn,
            matrix_sdk_base::store::ambiguity_map=warn,
            matrix_sdk_ui::notification_client=info,
            matrix_sdk_base::response_processors=debug,
            super_duper_app=error"#
                .split('\n')
                .map(|s| s.trim())
                .collect::<Vec<_>>()
                .join("")
        );
    }

    #[test]
    fn test_trace_tracing_filter() {
        let config = super::TracingConfiguration {
            log_level: super::LogLevel::Trace,
            trace_log_packs: Vec::new(),
            extra_targets: vec!["super_duper_app".to_owned(), "some_other_span".to_owned()],
            write_to_stdout_or_system: true,
            write_to_files: None,
            #[cfg(feature = "sentry")]
            sentry_dsn: None,
        };

        let filter = build_tracing_filter(&config);

        assert_eq!(
            filter,
            r#"panic=error,
            hyper=warn,
            matrix_sdk_ffi=info,
            matrix_sdk=info,
            matrix_sdk::client=trace,
            matrix_sdk_crypto=trace,
            matrix_sdk_crypto::olm::account=trace,
            matrix_sdk::oidc=trace,
            matrix_sdk::http_client=trace,
            matrix_sdk::sliding_sync=trace,
            matrix_sdk_base::sliding_sync=trace,
            matrix_sdk_ui::timeline=trace,
            matrix_sdk::send_queue=trace,
            matrix_sdk::event_cache=trace,
            matrix_sdk::latest_events=trace,
            matrix_sdk_base::event_cache=trace,
            matrix_sdk_sqlite::event_cache_store=trace,
            matrix_sdk_common::cross_process_lock=warn,
            matrix_sdk_common::deserialized_responses=trace,
            matrix_sdk_base::store::ambiguity_map=warn,
            matrix_sdk_ui::notification_client=trace,
            matrix_sdk_base::response_processors=trace,
            super_duper_app=trace,
            some_other_span=trace"#
                .split('\n')
                .map(|s| s.trim())
                .collect::<Vec<_>>()
                .join("")
        );
    }

    #[test]
    fn test_trace_log_packs() {
        let config = super::TracingConfiguration {
            log_level: super::LogLevel::Info,
            trace_log_packs: vec![TraceLogPacks::EventCache, TraceLogPacks::SendQueue],
            extra_targets: vec!["super_duper_app".to_owned()],
            write_to_stdout_or_system: true,
            write_to_files: None,
            #[cfg(feature = "sentry")]
            sentry_dsn: None,
        };

        let filter = build_tracing_filter(&config);

        assert_eq!(
            filter,
            r#"panic=error,
            hyper=warn,
            matrix_sdk_ffi=info,
            matrix_sdk=info,
            matrix_sdk::client=trace,
            matrix_sdk_crypto=debug,
            matrix_sdk_crypto::olm::account=trace,
            matrix_sdk::oidc=trace,
            matrix_sdk::http_client=debug,
            matrix_sdk::sliding_sync=info,
            matrix_sdk_base::sliding_sync=info,
            matrix_sdk_ui::timeline=info,
            matrix_sdk::send_queue=trace,
            matrix_sdk::event_cache=trace,
            matrix_sdk::latest_events=info,
            matrix_sdk_base::event_cache=trace,
            matrix_sdk_sqlite::event_cache_store=trace,
            matrix_sdk_common::cross_process_lock=warn,
            matrix_sdk_common::deserialized_responses=trace,
            matrix_sdk_base::store::ambiguity_map=warn,
            matrix_sdk_ui::notification_client=info,
            matrix_sdk_base::response_processors=debug,
            super_duper_app=info"#
                .split('\n')
                .map(|s| s.trim())
                .collect::<Vec<_>>()
                .join("")
        );
    }
}
