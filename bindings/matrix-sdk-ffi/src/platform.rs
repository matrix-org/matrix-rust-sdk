#[cfg(target_os = "android")]
use android as platform_impl;
#[cfg(target_os = "ios")]
use ios as platform_impl;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
use other as platform_impl;

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

    #[cfg(feature = "jaeger-rageshakes")]
    pub fn setup_tracing_with_jaeger(configuration: String, client_name: String, path: String) {
        use crate::RUNTIME;

        let runtime = RUNTIME.handle().to_owned();

        let tracer = crate::rageshake::create_rageshake_tracer(path.into(), &client_name, runtime);
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);

        tracing_subscriber::registry()
            .with(EnvFilter::new(configuration))
            .with(fmt::layer().with_ansi(true).with_writer(io::stderr))
            .with(layer)
            .init();
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

/// Enable tracing where plaintext logs get output to STDOUT and traces in the
/// Jaeger JSON format[1].
///
/// The `client_name` is used to prefix the Jaeger JSON files. `configuration`
/// is the usual RUST_LOG trace configuration string.
///
///
/// The logger will output the Jaeger JSON files into the given `path`,
/// periodically those files will be collected and put into a compressed
/// archive. The source JSON files will be cleaned up, but the compressed
/// archives will be left. The caller needs to ensure that unneeded archive
/// files get cleaned up.
///
/// [1]: https://www.jaegertracing.io/docs/1.23/apis/
#[uniffi::export]
#[cfg(all(target_os = "ios", feature = "jaeger-rageshakes"))]
pub fn setup_tracing_with_jaeger(configuration: String, client_name: String, path: String) {
    platform_impl::setup_tracing_with_jaeger(configuration, client_name, path)
}
