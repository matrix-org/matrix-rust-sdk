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
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
    EnvFilter, Layer,
};

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

#[derive(uniffi::Record)]
pub struct TracingConfiguration {
    /// A filter line following the [RUST_LOG format].
    ///
    /// [RUST_LOG format]: https://rust-lang-nursery.github.io/rust-cookbook/development_tools/debugging/config_log.html
    filter: String,

    /// Whether to log to stdout, or in the logcat on Android.
    write_to_stdout_or_system: bool,

    /// If set, configures rotated log files where to write additional logs.
    write_to_files: Option<TracingFileConfiguration>,
}

#[uniffi::export]
pub fn setup_tracing(config: TracingConfiguration) {
    log_panics();

    tracing_subscriber::registry()
        .with(EnvFilter::new(&config.filter))
        .with(text_layers(config))
        .init();
}
