use std::sync::{atomic::AtomicBool, Arc, OnceLock};

use tracing::warn;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_core::Subscriber;
use tracing_subscriber::{
    field::RecordFields,
    fmt::{
        self,
        format::{DefaultFields, Writer},
        time::FormatTime,
        FormatEvent, FormatFields, FormattedFields,
    },
    layer::SubscriberExt as _,
    registry::LookupSpan,
    util::SubscriberInitExt as _,
    Layer,
};

use crate::{error::ClientError, tracing::LogLevel};

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

    let file_layer = config.write_to_files.map(|c| {
        let mut builder = RollingFileAppender::builder()
            .rotation(Rotation::HOURLY)
            .filename_prefix(&c.file_prefix);

        if let Some(max_files) = c.max_files {
            builder = builder.max_log_files(max_files as usize)
        };
        if let Some(file_suffix) = c.file_suffix {
            builder = builder.filename_suffix(file_suffix)
        }

        let writer = builder.build(&c.path).expect("Failed to create a rolling file appender.");

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

        fmt::layer()
            .fmt_fields(FieldsFormatterForFiles::default())
            .event_format(EventFormatter::new())
            // EventFormatter doesn't support ANSI colors anyways, but the
            // default field formatter does, which is unhelpful for iOS +
            // Android logs, but enabled by default.
            .with_ansi(false)
            .with_writer(writer)
    });

    Layer::and_then(
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
    )
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

    // SDK common modules.
    MatrixSdkCommonStoreLocks,

    // SDK modules.
    MatrixSdk,
    MatrixSdkClient,
    MatrixSdkCrypto,
    MatrixSdkCryptoAccount,
    MatrixSdkEventCache,
    MatrixSdkEventCacheStore,
    MatrixSdkHttpClient,
    MatrixSdkOidc,
    MatrixSdkSendQueue,
    MatrixSdkSlidingSync,

    // SDK UI modules.
    MatrixSdkUiTimeline,
}

impl LogTarget {
    fn as_str(&self) -> &'static str {
        match self {
            LogTarget::Hyper => "hyper",
            LogTarget::MatrixSdkFfi => "matrix_sdk_ffi",
            LogTarget::MatrixSdkBaseEventCache => "matrix_sdk_base::event_cache",
            LogTarget::MatrixSdkBaseSlidingSync => "matrix_sdk_base::sliding_sync",
            LogTarget::MatrixSdkBaseStoreAmbiguityMap => "matrix_sdk_base::store::ambiguity_map",
            LogTarget::MatrixSdkCommonStoreLocks => "matrix_sdk_common::store_locks",
            LogTarget::MatrixSdk => "matrix_sdk",
            LogTarget::MatrixSdkClient => "matrix_sdk::client",
            LogTarget::MatrixSdkCrypto => "matrix_sdk_crypto",
            LogTarget::MatrixSdkCryptoAccount => "matrix_sdk_crypto::olm::account",
            LogTarget::MatrixSdkOidc => "matrix_sdk::oidc",
            LogTarget::MatrixSdkHttpClient => "matrix_sdk::http_client",
            LogTarget::MatrixSdkSlidingSync => "matrix_sdk::sliding_sync",
            LogTarget::MatrixSdkEventCache => "matrix_sdk::event_cache",
            LogTarget::MatrixSdkSendQueue => "matrix_sdk::send_queue",
            LogTarget::MatrixSdkEventCacheStore => "matrix_sdk_sqlite::event_cache_store",
            LogTarget::MatrixSdkUiTimeline => "matrix_sdk_ui::timeline",
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
    (LogTarget::MatrixSdkBaseEventCache, LogLevel::Info),
    (LogTarget::MatrixSdkEventCacheStore, LogLevel::Info),
    (LogTarget::MatrixSdkCommonStoreLocks, LogLevel::Warn),
    (LogTarget::MatrixSdkBaseStoreAmbiguityMap, LogLevel::Warn),
];

const IMMUTABLE_LOG_TARGETS: &[LogTarget] = &[
    LogTarget::Hyper,                          // Too verbose
    LogTarget::MatrixSdk,                      // Too generic
    LogTarget::MatrixSdkFfi,                   // Too verbose
    LogTarget::MatrixSdkCommonStoreLocks,      // Too verbose
    LogTarget::MatrixSdkBaseStoreAmbiguityMap, // Too verbose
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
            ],
            TraceLogPacks::SendQueue => &[LogTarget::MatrixSdkSendQueue],
            TraceLogPacks::Timeline => &[LogTarget::MatrixSdkUiTimeline],
        }
    }
}

struct SentryLoggingCtx {
    /// The Sentry client guard, which keeps the Sentry context alive.
    _guard: sentry::ClientInitGuard,

    /// Whether the Sentry layer is enabled or not, at a global level.
    enabled: Arc<AtomicBool>,
}

struct LoggingCtx {
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
    sentry_dsn: Option<String>,
}

impl TracingConfiguration {
    /// Sets up the tracing configuration and return a [`Logger`] instance
    /// holding onto it.
    fn build(mut self) -> LoggingCtx {
        // Show full backtraces, if we run into panics.
        std::env::set_var("RUST_BACKTRACE", "1");

        // Log panics.
        log_panics::init();

        // Prepare the Sentry layer, if a DSN is provided.
        let (sentry_layer, sentry_logging_ctx) = if let Some(sentry_dsn) = self.sentry_dsn.take() {
            // Initialize the Sentry client with the given options.
            let sentry_guard = sentry::init((
                sentry_dsn,
                sentry::ClientOptions {
                    traces_sample_rate: 0.0,
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
                            sentry_tracing::default_span_filter(metadata)
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

        let env_filter = build_tracing_filter(&self);

        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(&env_filter))
            .with(crate::platform::text_layers(self))
            .with(sentry_layer)
            .init();

        // Log the log levels 🧠.
        tracing::info!(env_filter, "Logging has been set up");

        LoggingCtx { sentry: sentry_logging_ctx }
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
    LOGGING.set(config.build()).map_err(|_| ClientError::Generic {
        msg: "logger already initialized".to_owned(),
        details: None,
    })?;

    if use_lightweight_tokio_runtime {
        setup_lightweight_tokio_runtime();
    } else {
        setup_multithreaded_tokio_runtime();
    }

    Ok(())
}

/// Set the global enablement level for the Sentry layer (after the logs have
/// been set up).
#[matrix_sdk_ffi_macros::export]
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

fn setup_multithreaded_tokio_runtime() {
    async_compat::set_runtime_builder(Box::new(|| {
        eprintln!("spawning a multithreaded tokio runtime");

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        builder
    }));
}

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
            sentry_dsn: None,
        };

        let filter = build_tracing_filter(&config);

        assert_eq!(
            filter,
            "panic=error,\
            hyper=warn,\
            matrix_sdk_ffi=info,\
            matrix_sdk=info,\
            matrix_sdk::client=trace,\
            matrix_sdk_crypto=debug,\
            matrix_sdk_crypto::olm::account=trace,\
            matrix_sdk::oidc=trace,\
            matrix_sdk::http_client=debug,\
            matrix_sdk::sliding_sync=info,\
            matrix_sdk_base::sliding_sync=info,\
            matrix_sdk_ui::timeline=info,\
            matrix_sdk::send_queue=info,\
            matrix_sdk::event_cache=info,\
            matrix_sdk_base::event_cache=info,\
            matrix_sdk_sqlite::event_cache_store=info,\
            matrix_sdk_common::store_locks=warn,\
            matrix_sdk_base::store::ambiguity_map=warn,\
            super_duper_app=error"
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
            sentry_dsn: None,
        };

        let filter = build_tracing_filter(&config);

        assert_eq!(
            filter,
            "panic=error,\
            hyper=warn,\
            matrix_sdk_ffi=info,\
            matrix_sdk=info,\
            matrix_sdk::client=trace,\
            matrix_sdk_crypto=trace,\
            matrix_sdk_crypto::olm::account=trace,\
            matrix_sdk::oidc=trace,\
            matrix_sdk::http_client=trace,\
            matrix_sdk::sliding_sync=trace,\
            matrix_sdk_base::sliding_sync=trace,\
            matrix_sdk_ui::timeline=trace,\
            matrix_sdk::send_queue=trace,\
            matrix_sdk::event_cache=trace,\
            matrix_sdk_base::event_cache=trace,\
            matrix_sdk_sqlite::event_cache_store=trace,\
            matrix_sdk_common::store_locks=warn,\
            matrix_sdk_base::store::ambiguity_map=warn,\
            super_duper_app=trace,\
            some_other_span=trace"
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
            matrix_sdk_base::event_cache=trace,
            matrix_sdk_sqlite::event_cache_store=trace,
            matrix_sdk_common::store_locks=warn,
            matrix_sdk_base::store::ambiguity_map=warn,
            super_duper_app=info"#
                .split('\n')
                .map(|s| s.trim())
                .collect::<Vec<_>>()
                .join("")
        );
    }
}
