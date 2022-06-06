use napi_derive::*;

#[napi]
pub fn init_tracing() {
    tracing_subscriber::fmt::init();
}
