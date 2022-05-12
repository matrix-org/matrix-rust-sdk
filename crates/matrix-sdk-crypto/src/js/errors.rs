use std::error::Error;

use wasm_bindgen::JsValue;

pub fn any_error_to_jsvalue<E>(error: E) -> JsValue
where
    E: Error,
{
    error.to_string().into()
}
