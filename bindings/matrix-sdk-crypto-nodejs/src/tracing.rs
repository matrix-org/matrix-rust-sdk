use napi_derive::*;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Subscribe to tracing events, i.e. turn on logs.
#[napi]
pub fn init_tracing() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_env("MATRIX_LOG"))
        .init();
}
