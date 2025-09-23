// Copyright 2025 Kévin Commaille
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! HTTP client and helpers for making OAuth 2.0 requests.

use matrix_sdk_base::BoxFuture;
use oauth2::{
    AsyncHttpClient, ErrorResponse, HttpClientError, HttpRequest, HttpResponse, RequestTokenError,
};

/// An HTTP client for making OAuth 2.0 requests.
#[derive(Debug, Clone)]
pub(super) struct OAuthHttpClient {
    pub(super) inner: reqwest::Client,
    /// Rewrite HTTPS requests to use HTTP instead.
    ///
    /// This is a workaround to bypass some checks that require an HTTPS URL,
    /// but we can only mock HTTP URLs.
    #[cfg(test)]
    pub(super) insecure_rewrite_https_to_http: bool,
}

impl<'c> AsyncHttpClient<'c> for OAuthHttpClient {
    type Error = HttpClientError<reqwest::Error>;

    type Future = BoxFuture<'c, Result<HttpResponse, Self::Error>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            #[cfg(test)]
            let request = if self.insecure_rewrite_https_to_http
                && request.uri().scheme().is_some_and(|scheme| *scheme == http::uri::Scheme::HTTPS)
            {
                let mut request = request;

                let mut uri_parts = request.uri().clone().into_parts();
                uri_parts.scheme = Some(http::uri::Scheme::HTTP);
                *request.uri_mut() = http::uri::Uri::from_parts(uri_parts)
                    .expect("reconstructing URI from parts should work");

                request
            } else {
                request
            };

            let response = self.inner.call(request).await?;

            Ok(response)
        })
    }
}

/// Check the status code of the given HTTP response to identify errors.
pub(super) fn check_http_response_status_code<T: ErrorResponse + 'static>(
    http_response: &HttpResponse,
) -> Result<(), RequestTokenError<HttpClientError<reqwest::Error>, T>> {
    if http_response.status().as_u16() < 400 {
        return Ok(());
    }

    let reason = http_response.body().as_slice();
    let error = if reason.is_empty() {
        RequestTokenError::Other("server returned an empty error response".to_owned())
    } else {
        match serde_json::from_slice(reason) {
            Ok(error) => RequestTokenError::ServerResponse(error),
            Err(error) => RequestTokenError::Other(error.to_string()),
        }
    };

    Err(error)
}

/// Check that the server returned a response with a JSON `Content-Type`.
pub(super) fn check_http_response_json_content_type<T: ErrorResponse + 'static>(
    http_response: &HttpResponse,
) -> Result<(), RequestTokenError<HttpClientError<reqwest::Error>, T>> {
    let Some(content_type) = http_response.headers().get(http::header::CONTENT_TYPE) else {
        return Ok(());
    };

    if content_type
        .to_str()
        // Check only the beginning of the content type, because there might be extra
        // parameters, like a charset.
        .is_ok_and(|ct| ct.to_lowercase().starts_with(mime::APPLICATION_JSON.essence_str()))
    {
        Ok(())
    } else {
        Err(RequestTokenError::Other(format!(
            "unexpected response Content-Type: {content_type:?}, should be `{}`",
            mime::APPLICATION_JSON
        )))
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_matches;
    use oauth2::{RequestTokenError, basic::BasicErrorResponse};

    use super::{check_http_response_json_content_type, check_http_response_status_code};

    #[test]
    fn test_check_http_response_status_code() {
        // OK
        let response = http::Response::builder().status(200).body(Vec::<u8>::new()).unwrap();
        assert_matches!(check_http_response_status_code::<BasicErrorResponse>(&response), Ok(()));

        // Error without body.
        let response = http::Response::builder().status(404).body(Vec::<u8>::new()).unwrap();
        assert_matches!(
            check_http_response_status_code::<BasicErrorResponse>(&response),
            Err(RequestTokenError::Other(_))
        );

        // Error with invalid body.
        let response =
            http::Response::builder().status(404).body(b"invalid error format".to_vec()).unwrap();
        assert_matches!(
            check_http_response_status_code::<BasicErrorResponse>(&response),
            Err(RequestTokenError::Other(_))
        );

        // Error with valid body.
        let response = http::Response::builder()
            .status(404)
            .body(br#"{"error": "invalid_request"}"#.to_vec())
            .unwrap();
        assert_matches!(
            check_http_response_status_code::<BasicErrorResponse>(&response),
            Err(RequestTokenError::ServerResponse(_))
        );
    }

    #[test]
    fn test_check_http_response_json_content_type() {
        // Valid content type.
        let response = http::Response::builder()
            .status(200)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(b"{}".to_vec())
            .unwrap();
        assert_matches!(
            check_http_response_json_content_type::<BasicErrorResponse>(&response),
            Ok(())
        );

        // Valid content type with charset.
        let response = http::Response::builder()
            .status(200)
            .header(http::header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(b"{}".to_vec())
            .unwrap();
        assert_matches!(
            check_http_response_json_content_type::<BasicErrorResponse>(&response),
            Ok(())
        );

        // Without content type.
        let response = http::Response::builder().status(200).body(b"{}".to_vec()).unwrap();
        assert_matches!(
            check_http_response_json_content_type::<BasicErrorResponse>(&response),
            Ok(())
        );

        // Wrong content type.
        let response = http::Response::builder()
            .status(200)
            .header(http::header::CONTENT_TYPE, "text/html")
            .body(b"<html><body><h1>HTML!</h1></body></html>".to_vec())
            .unwrap();
        assert_matches!(
            check_http_response_json_content_type::<BasicErrorResponse>(&response),
            Err(RequestTokenError::Other(_))
        );
    }
}
