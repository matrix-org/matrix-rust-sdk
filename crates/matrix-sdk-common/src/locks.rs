#[cfg(target_arch = "wasm32")]
pub use async_lock::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
