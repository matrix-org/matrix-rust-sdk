#[cfg(not(target_arch="wasm32"))]
compile_error!("indexeddb is only support on wasm32. You may want to add --target wasm32-unknown-unknown");

pub mod state_store;
pub mod cryptostore;