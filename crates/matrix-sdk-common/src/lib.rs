#![doc = include_str!("../README.md")]
#![deny(missing_debug_implementations)]

pub use async_trait::async_trait;
pub use instant;
pub use ruma;

pub mod deserialized_responses;
pub mod executor;
pub mod locks;
pub mod util;

/// Super trait that is used for our store traits, this trait will differ if
/// it's used on WASM. WASM targets will not require `Send` and `Sync` to have
/// implemented, while other targets will.
#[cfg(not(target_arch = "wasm32"))]
pub trait AsyncTraitDeps: std::fmt::Debug + Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: std::fmt::Debug + Send + Sync> AsyncTraitDeps for T {}

/// Super trait that is used for our store traits, this trait will differ if
/// it's used on WASM. WASM targets will not require `Send` and `Sync` to have
/// implemented, while other targets will.
#[cfg(target_arch = "wasm32")]
pub trait AsyncTraitDeps: std::fmt::Debug {}
#[cfg(target_arch = "wasm32")]
impl<T: std::fmt::Debug> AsyncTraitDeps for T {}
