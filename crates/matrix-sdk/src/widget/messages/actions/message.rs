use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Kind<Req, Resp> {
    Response(Response<Req, Resp>),
    Request(Request<Req>),
}

impl<T> Kind<Empty, T> {
    pub fn empty() -> Self {
        Kind::Request(Request::empty())
    }
}

impl<Req, Resp> Kind<Req, Resp> {
    pub fn request(content: Req) -> Self {
        Kind::Request(Request::new(content))
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Request<T> {
    #[serde(rename = "data")]
    pub content: T,
}

impl<T> Request<T> {
    pub fn new(content: T) -> Self {
        Self { content }
    }
}

impl Request<Empty> {
    pub const fn empty() -> Self {
        Self { content: Empty {} }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Response<Req, Resp> {
    #[serde(flatten)]
    pub request: Request<Req>,
    pub response: ResponseBody<Resp>,
}

impl<Req, Resp: Clone> Response<Req, Resp> {
    pub fn response(&self) -> Result<Resp, String> {
        match &self.response {
            ResponseBody::Success(resp) => Ok(resp.clone()),
            ResponseBody::Failure(err) => Err(err.as_ref().to_owned()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum ResponseBody<T> {
    Success(T),
    Failure(ErrorBody),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ErrorBody {
    pub error: ErrorMessage,
}

impl ErrorBody {
    pub fn new(message: impl AsRef<str>) -> Self {
        Self { error: ErrorMessage { message: message.as_ref().to_owned() } }
    }
}

impl AsRef<str> for ErrorBody {
    fn as_ref(&self) -> &str {
        &self.error.message
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ErrorMessage {
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Empty {}

impl<T> Request<T> {
    pub fn map<R>(self, response: Result<R, String>) -> Kind<T, R> {
        Kind::Response(Response {
            request: self,
            response: match response {
                Ok(response) => ResponseBody::Success(response),
                Err(error) => ResponseBody::Failure(ErrorBody::new(error)),
            },
        })
    }
}
