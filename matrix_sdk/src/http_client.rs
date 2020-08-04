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
use reqwest::{header::AUTHORIZATION, Client, Response};
use tracing::trace;
use url::Url;

use matrix_sdk_common::locks::RwLock;

use crate::{api::r0::uiaa::UiaaResponse, Endpoint, Error, Result, Session};

#[derive(Clone, Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: Client,
    pub(crate) homeserver: Arc<Url>,
}

impl HttpClient {
    async fn send_request(
        &self,
        requires_auth: bool,
        method: HttpMethod,
        request: http::Request<Vec<u8>>,
        session: Arc<RwLock<Option<Session>>>,
    ) -> Result<Response> {
        let url = request.uri();
        let path_and_query = url.path_and_query().unwrap();
        let mut url = (&*self.homeserver).clone();

        url.set_path(path_and_query.path());
        url.set_query(path_and_query.query());

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

        let request_builder = if requires_auth {
            if let Some(session) = session.read().await.as_ref() {
                let header_value = format!("Bearer {}", &session.access_token);
                request_builder.header(AUTHORIZATION, header_value)
            } else {
                return Err(Error::AuthenticationRequired);
            }
        } else {
            request_builder
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

    pub async fn send<Request: Endpoint<ResponseError = crate::api::Error> + std::fmt::Debug>(
        &self,
        request: Request,
        session: Arc<RwLock<Option<Session>>>,
    ) -> Result<Request::Response> {
        let request: http::Request<Vec<u8>> = request.try_into()?;
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

        Ok(<Request::Response>::try_from(response)?)
    }

    pub async fn send_uiaa<Request: Endpoint<ResponseError = UiaaResponse> + std::fmt::Debug>(
        &self,
        request: Request,
        session: Arc<RwLock<Option<Session>>>,
    ) -> Result<Request::Response> {
        let request: http::Request<Vec<u8>> = request.try_into()?;
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

        let uiaa: Result<_> = <Request::Response>::try_from(response).map_err(Into::into);

        Ok(uiaa?)
    }
}
