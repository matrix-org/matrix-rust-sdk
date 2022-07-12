use std::future::Future;

use js_sys::Promise;
use wasm_bindgen::{JsValue, UnwrapThrowExt};
use wasm_bindgen_futures::spawn_local;

pub(crate) fn future_to_promise<F, T>(future: F) -> Promise
where
    F: Future<Output = Result<T, anyhow::Error>> + 'static,
    T: Into<JsValue>,
{
    let mut future = Some(future);

    Promise::new(&mut |resolve, reject| {
        let future = future.take().unwrap_throw();

        spawn_local(async move {
            match future.await {
                Ok(value) => resolve.call1(&JsValue::UNDEFINED, &value.into()).unwrap_throw(),
                Err(value) => {
                    reject.call1(&JsValue::UNDEFINED, &value.to_string().into()).unwrap_throw()
                }
            };
        });
    })
}
