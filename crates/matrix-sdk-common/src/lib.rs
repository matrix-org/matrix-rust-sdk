#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations)]

pub use instant;

pub mod deserialized_responses;
pub mod executor;
pub mod timeout;

/// Alias for `Send` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(not(target_arch = "wasm32"))]
pub trait SendOutsideWasm: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send> SendOutsideWasm for T {}

/// Alias for `Send` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(target_arch = "wasm32")]
pub trait SendOutsideWasm {}
#[cfg(target_arch = "wasm32")]
impl<T> SendOutsideWasm for T {}

/// Alias for `Sync` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(not(target_arch = "wasm32"))]
pub trait SyncOutsideWasm: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync> SyncOutsideWasm for T {}

/// Alias for `Sync` on non-wasm, empty trait (implemented by everything) on
/// wasm.
#[cfg(target_arch = "wasm32")]
pub trait SyncOutsideWasm {}
#[cfg(target_arch = "wasm32")]
impl<T> SyncOutsideWasm for T {}

/// Super trait that is used for our store traits, this trait will differ if
/// it's used on WASM. WASM targets will not require `Send` and `Sync` to have
/// implemented, while other targets will.
pub trait AsyncTraitDeps: std::fmt::Debug + SendOutsideWasm + SyncOutsideWasm {}
impl<T: std::fmt::Debug + SendOutsideWasm + SyncOutsideWasm> AsyncTraitDeps for T {}
