mod safe_encode;

#[cfg(target_arch = "wasm32")]
mod state_store;

#[cfg(target_arch = "wasm32")]
#[cfg(feature = "encryption")]
mod cryptostore;

#[cfg(target_arch = "wasm32")]
#[cfg(feature = "encryption")]
pub use cryptostore::IndexeddbStore as CryptoStore;
#[cfg(target_arch = "wasm32")]
pub use state_store::IndexeddbStore as StateStore;
