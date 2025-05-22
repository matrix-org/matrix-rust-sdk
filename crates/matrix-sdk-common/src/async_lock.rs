#[cfg(target_arch = "wasm32")]
use std::sync::{
    RwLock as StdRwLock, RwLockReadGuard as StdRwLockReadGuard,
    RwLockWriteGuard as StdRwLockWriteGuard,
};
#[cfg(target_arch = "wasm32")]
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::sync::{
    RwLock as AsyncRwLock, RwLockReadGuard as AsyncRwLockReadGuard,
    RwLockWriteGuard as AsyncRwLockWriteGuard,
};

/// A platform-independent `AsyncRwLock`.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct AsyncRwLock<T> {
    inner: Arc<StdRwLock<T>>,
}

#[cfg(target_arch = "wasm32")]
impl<T: Default> Default for AsyncRwLock<T> {
    fn default() -> Self {
        Self { inner: Arc::new(StdRwLock::new(T::default())) }
    }
}

#[cfg(target_arch = "wasm32")]
impl<T> AsyncRwLock<T> {
    /// Create a new `AsyncRwLock`.
    pub fn new(value: T) -> Self {
        Self { inner: Arc::new(StdRwLock::new(value)) }
    }

    /// Acquire a read lock asynchronously.
    pub async fn read(&self) -> AsyncRwLockReadGuard<'_, T> {
        AsyncRwLockReadFuture { lock: &self.inner, waker: None }.await
    }

    /// Acquire a write lock asynchronously.
    pub async fn write(&self) -> AsyncRwLockWriteGuard<'_, T> {
        AsyncRwLockWriteFuture { lock: &self.inner, waker: None }.await
    }
}

#[cfg(target_arch = "wasm32")]
/// A future for acquiring a read lock.
struct AsyncRwLockReadFuture<'a, T> {
    lock: &'a StdRwLock<T>,
    waker: Option<Waker>,
}

#[cfg(target_arch = "wasm32")]
impl<'a, T> Future for AsyncRwLockReadFuture<'a, T> {
    type Output = AsyncRwLockReadGuard<'a, T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Store the waker so we can wake the task if needed
        self.waker = Some(cx.waker().clone());

        // Try to acquire the read lock - this is a non-blocking operation in wasm
        match self.lock.try_read() {
            Ok(guard) => Poll::Ready(AsyncRwLockReadGuard { guard }),
            Err(_) => {
                // In a true async environment we would register a waker to be notified
                // when the lock becomes available, but in wasm we'll just yield to
                // the executor and try again next time.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
/// A future for acquiring a write lock.
struct AsyncRwLockWriteFuture<'a, T> {
    lock: &'a StdRwLock<T>,
    waker: Option<Waker>,
}

#[cfg(target_arch = "wasm32")]
impl<'a, T> Future for AsyncRwLockWriteFuture<'a, T> {
    type Output = AsyncRwLockWriteGuard<'a, T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Store the waker so we can wake the task if needed
        self.waker = Some(cx.waker().clone());

        // Try to acquire the write lock - this is a non-blocking operation in wasm
        match self.lock.try_write() {
            Ok(guard) => Poll::Ready(AsyncRwLockWriteGuard { guard }),
            Err(_) => {
                // In a true async environment we would register a waker to be notified
                // when the lock becomes available, but in wasm we'll just yield to
                // the executor and try again next time.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
/// A read guard for `AsyncRwLock`.
pub struct AsyncRwLockReadGuard<'a, T> {
    guard: StdRwLockReadGuard<'a, T>,
}

#[cfg(target_arch = "wasm32")]
impl<T> std::ops::Deref for AsyncRwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
/// A write guard for `AsyncRwLock`.
pub struct AsyncRwLockWriteGuard<'a, T> {
    guard: StdRwLockWriteGuard<'a, T>,
}

#[cfg(target_arch = "wasm32")]
impl<T> std::ops::Deref for AsyncRwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

#[cfg(target_arch = "wasm32")]
impl<T> std::ops::DerefMut for AsyncRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}
