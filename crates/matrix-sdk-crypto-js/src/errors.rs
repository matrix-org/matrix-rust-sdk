#[cfg(feature = "js")]
pub type Error = wasm_bindgen::JsError;

#[cfg(feature = "nodejs")]
#[derive(Debug)]
pub struct Error(napi::Error);

#[cfg(feature = "nodejs")]
impl<E> From<E> for Error
where
    E: std::error::Error,
{
    fn from(error: E) -> Self {
        Self(napi::Error::from_reason(error.to_string()))
    }
}
