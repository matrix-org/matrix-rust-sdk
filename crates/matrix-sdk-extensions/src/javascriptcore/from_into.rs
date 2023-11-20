use javascriptcore::{JSException, JSValue};

pub trait TryIntoRust<T> {
    type Error;

    fn try_into_rust(&self) -> Result<T, Self::Error>;
}

impl TryIntoRust<String> for &JSValue {
    type Error = JSException;

    fn try_into_rust(&self) -> Result<String, Self::Error> {
        Ok(self.as_string()?.to_string())
    }
}
