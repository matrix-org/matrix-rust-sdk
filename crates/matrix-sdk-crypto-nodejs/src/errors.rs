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

impl Into<napi::Error> for Error {
    fn into(self) -> napi::Error {
        self.0
    }
}
