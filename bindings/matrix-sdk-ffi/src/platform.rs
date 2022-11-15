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
