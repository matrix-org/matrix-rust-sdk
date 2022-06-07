use napi_derive::*;

/// Subscribe to tracing events, i.e. turn on logs.
#[napi]
pub fn init_tracing() {
    tracing_subscriber::fmt::init();
}
