#[cfg(not(target_arch="wasm32"))]
compile_error!("indexeddb is only support on wasm32. You may want to add --target wasm32-unknown-unknown");

mod state_store;

#[cfg(feature = "encryption")]
mod cryptostore;

#[cfg(feature = "encryption")]
pub use cryptostore::IndexeddbStore as CryptoStore;

pub use state_store::IndexeddbStore as StateStore;
