use std::{future::Future, panic::AssertUnwindSafe, sync::Arc};

use futures_util::FutureExt;
use tokio::task::JoinHandle;
use tracing::error;

use crate::RUNTIME;

pub(crate) fn unwrap_or_clone_arc<T: Clone>(arc: Arc<T>) -> T {
    Arc::try_unwrap(arc).unwrap_or_else(|x| (*x).clone())
}

pub fn spawn_future<F>(future: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    // tokio::spawn doesn't require unwind safety either, everything we do seems
    // to be unwind-unsafe but if one of these tasks panics we usually have
    // bigger issues than inconsistent state (note: that is the worst that can
    // happen by asserting unwind safety, no unsafe code is involved here)
    let future = AssertUnwindSafe(future);
    RUNTIME.spawn(async move {
        if let Err(e) = future.catch_unwind().await {
            if let Some(s) = e.downcast_ref::<&'static str>() {
                error!("Spawned future panicked: {s}");
            } else if let Some(s) = e.downcast_ref::<String>() {
                error!("Spawned future panicked: {s}");
            } else {
                error!("Spawned future panicked with non-string payload");
            }
        }
    })
}
