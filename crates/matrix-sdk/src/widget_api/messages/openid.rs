use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum State {
    #[serde(rename = "allowed")]
    Allowed(Response),
    #[serde(rename = "blocked")]
    Blocked,
    #[serde(rename = "request")]
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    #[serde(rename = "original_request_id")]
    id: String,
    #[serde(rename = "access_token")]
    token: String,
    #[serde(rename = "expires_in")]
    expires_in_seconds: usize,
    #[serde(rename = "matrix_server_name")]
    server: String,
    #[serde(rename = "token_type")]
    kind: String,
}
