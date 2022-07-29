use std::{fmt, time::Duration};

use futures_core::TryFuture;
#[cfg(not(target_arch = "wasm32"))]
use tokio::time::timeout;
#[cfg(target_arch = "wasm32")]
use wasm_timer::ext::TryFutureExt;

/// Error type notifying that a timeout has elapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElapsedError(());

impl fmt::Display for ElapsedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "time waiting for future has elapsed!")
    }
}

/// Wait for `future` to be completed.
///
/// If the given timeout has elapsed the method will stop waiting and return
/// an error.
pub async fn wait<T, F: TryFuture<Output = T>>(
    future: F,
    duration: Duration,
) -> Result<T, ElapsedError> {
    #[cfg(not(target_arch = "wasm32"))]
    return timeout(duration, future).await.map_err(|_| ElapsedError(()));

    #[cfg(target_arch = "wasm32")]
    return future.timeout(duration).await.map_err(|_| ElapsedError(()));
}
