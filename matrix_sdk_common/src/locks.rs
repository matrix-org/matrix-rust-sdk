// could switch to futures-lock completely at some point, blocker:
// https://github.com/asomers/futures-locks/issues/34
// https://www.reddit.com/r/rust/comments/f4zldz/i_audited_3_different_implementation_of_async/

#[cfg(target_arch = "wasm32")]
pub use futures_locks::Mutex;
#[cfg(target_arch = "wasm32")]
pub use futures_locks::RwLock;

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::sync::RwLock;
