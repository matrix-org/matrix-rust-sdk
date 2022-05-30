/// Generic error wrapping `napi::Error`.
#[derive(Debug)]
pub struct Error(napi::Error);

impl<E> From<E> for Error
where
    E: std::error::Error,
{
    fn from(error: E) -> Self {
        Self(napi::Error::from_reason(error.to_string()))
    }
}

impl From<Error> for napi::Error {
    fn from(value: Error) -> Self {
        value.0
    }
}

pub fn into_err<E>(error: E) -> napi::Error
where
    E: std::error::Error,
{
    Error::from(error).into()
}
