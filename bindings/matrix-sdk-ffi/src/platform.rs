#[cfg(target_os = "android")]
pub use android::*;
#[cfg(target_os = "ios")]
pub use ios::*;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub use other::*;

#[cfg(target_os = "android")]
mod android {
    use android_logger::{Config, FilterBuilder};
    use tracing::log::Level;

    #[uniffi::export]
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
    #[uniffi::export]
    fn setup_tracing(configuration: String) {
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

    #[uniffi::export]
    fn setup_tracing(configuration: String) {
        tracing_subscriber::registry()
            .with(EnvFilter::new(configuration))
            .with(fmt::layer().with_ansi(false).with_writer(io::stderr))
            .init();
    }
}
