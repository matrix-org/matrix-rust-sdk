use std::{error::Error, fmt, time::Duration};

use futures_core::Future;
#[cfg(not(target_arch = "wasm32"))]
use tokio::time::timeout as tokio_timeout;
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

impl Error for ElapsedError {}

impl From<std::io::Error> for ElapsedError {
    fn from(io_error: std::io::Error) -> Self {
        io_error.into()
    }
}

/// Wait for `future` to be completed. `future` needs to return
/// a `Result`.
///
/// If the given timeout has elapsed the method will stop waiting and return
/// an error.
pub async fn timeout<F, T>(future: F, duration: Duration) -> Result<T, ElapsedError>
where
    F: Future<Output = T>,
{
    #[cfg(not(target_arch = "wasm32"))]
    return tokio_timeout(duration, future).await.map_err(|_| ElapsedError(()));

    #[cfg(target_arch = "wasm32")]
    {
        let try_future = async {
            let output = future.await;

            // Contrary to clippy's note, the qualification of the Result is necessary
            #[allow(unused_qualifications)]
            Result::<T, ElapsedError>::Ok(output)
        };

        return try_future.timeout(duration).await.map_err(|_| ElapsedError(()));
    }
}

// TODO: Enable tests for wasm32 and debug why `with_timeout` test fails
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) mod tests {
    use matrix_sdk_test::async_test;
    #[cfg(not(target_arch = "wasm32"))]
    use tokio::time::{sleep, Duration};
    #[cfg(target_arch = "wasm32")]
    use {std::time::Duration, wasm_timer::Delay};

    use super::timeout;

    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    #[async_test]
    async fn without_timeout() {
        let duration_future = Duration::from_millis(500);
        let duration_timeout = Duration::from_millis(800);

        #[cfg(not(target_arch = "wasm32"))]
        let fut = sleep(duration_future);
        #[cfg(target_arch = "wasm32")]
        let fut = Delay::new(duration_future);

        let res = timeout(fut, duration_timeout).await;

        assert!(res.is_ok());
    }

    #[async_test]
    async fn with_timeout() {
        let duration_future = Duration::from_millis(900);
        let duration_timeout = Duration::from_millis(800);

        #[cfg(not(target_arch = "wasm32"))]
        let fut = sleep(duration_future);
        #[cfg(target_arch = "wasm32")]
        let fut = Delay::new(duration_future);

        let res = timeout(fut, duration_timeout).await;

        assert!(res.is_err());
    }
}
