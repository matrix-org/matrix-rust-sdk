// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{convert::TryFrom, sync::Arc};

use http::{Method as HttpMethod, Response as HttpResponse};
use reqwest::{Client, Response};
use tracing::trace;
use url::Url;

use matrix_sdk_common::{locks::RwLock, FromHttpRequestError, FromHttpResponseError, Outgoing};

use crate::{Endpoint, Error, Result, Session};

#[derive(Clone, Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: Client,
    pub(crate) homeserver: Arc<Url>,
}

impl HttpClient {
    async fn send_request<Request>(
        &self,
        requires_auth: bool,
        method: HttpMethod,
        request: Request,
        session: Arc<RwLock<Option<Session>>>,
    ) -> Result<Response>
    where
        Request: Endpoint,
        <Request as Outgoing>::Incoming:
            TryFrom<http::Request<Vec<u8>>, Error = FromHttpRequestError>,
        <Request::Response as Outgoing>::Incoming: TryFrom<
            http::Response<Vec<u8>>,
            Error = FromHttpResponseError<<Request as Endpoint>::ResponseError>,
        >,
    {
        let request = {
            let read_guard;
            let access_token = if requires_auth {
                read_guard = session.read().await;

                if let Some(session) = read_guard.as_ref() {
                    Some(session.access_token.as_str())
                } else {
                    return Err(Error::AuthenticationRequired);
                }
            } else {
                None
            };

            request.try_into_http_request(&self.homeserver.to_string(), access_token)?
        };

        let url = &request.uri().to_string();

        let request_builder = match method {
            HttpMethod::GET => self.inner.get(url),
            HttpMethod::POST => {
                let body = request.body().clone();
                self.inner
                    .post(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            HttpMethod::PUT => {
                let body = request.body().clone();
                self.inner
                    .put(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            HttpMethod::DELETE => {
                let body = request.body().clone();
                self.inner
                    .delete(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            method => panic!("Unsupported method {}", method),
        };

        Ok(request_builder.send().await?)
    }

    async fn response_to_http_response(
        &self,
        mut response: Response,
    ) -> Result<http::Response<Vec<u8>>> {
        let status = response.status();
        let mut http_builder = HttpResponse::builder().status(status);
        let headers = http_builder.headers_mut().unwrap();

        for (k, v) in response.headers_mut().drain() {
            if let Some(key) = k {
                headers.insert(key, v);
            }
        }
        let body = response.bytes().await?.as_ref().to_owned();
        Ok(http_builder.body(body).unwrap())
    }

    pub async fn send<Request>(
        &self,
        request: Request,
        session: Arc<RwLock<Option<Session>>>,
    ) -> Result<<Request::Response as Outgoing>::Incoming>
    where
        Request: Endpoint,
        <Request as Outgoing>::Incoming:
            TryFrom<http::Request<Vec<u8>>, Error = FromHttpRequestError>,
        <Request::Response as Outgoing>::Incoming: TryFrom<
            http::Response<Vec<u8>>,
            Error = FromHttpResponseError<<Request as Endpoint>::ResponseError>,
        >,
        Error: From<FromHttpResponseError<<Request as Endpoint>::ResponseError>>,
    {
        let response = self
            .send_request(
                Request::METADATA.requires_authentication,
                Request::METADATA.method,
                request,
                session,
            )
            .await?;

        trace!("Got response: {:?}", response);

        let response = self.response_to_http_response(response).await?;

        Ok(<Request::Response as Outgoing>::Incoming::try_from(
            response,
        )?)
    }
}
