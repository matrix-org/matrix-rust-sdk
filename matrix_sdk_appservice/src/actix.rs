// Copyright 2021 Famedly GmbH
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

use std::{
    convert::{TryFrom, TryInto},
    pin::Pin,
};

pub use actix_web::Scope;
use actix_web::{
    dev::Payload,
    error::PayloadError,
    get,
    http::PathAndQuery,
    put,
    web::{self, BytesMut, Data},
    App, FromRequest, HttpRequest, HttpResponse, HttpServer,
};
use futures::Future;
use futures_util::{TryFutureExt, TryStreamExt};
use matrix_sdk::api_appservice as api;

use crate::{error::Error, Appservice};

pub async fn run_server(
    appservice: Appservice,
    host: impl AsRef<str>,
    port: impl Into<u16>,
) -> Result<(), Error> {
    HttpServer::new(move || App::new().service(appservice.actix_service()))
        .bind((host.as_ref(), port.into()))?
        .run()
        .await?;

    Ok(())
}

pub fn get_scope() -> Scope {
    gen_scope("/"). // handle legacy routes
    service(gen_scope("/_matrix/app/v1"))
}

fn gen_scope(scope: &str) -> Scope {
    web::scope(scope).service(push_transactions).service(query_user_id).service(query_room_alias)
}

#[tracing::instrument]
#[put("/transactions/{txn_id}")]
async fn push_transactions(
    request: IncomingRequest<api::event::push_events::v1::IncomingRequest>,
    appservice: Data<Appservice>,
) -> Result<HttpResponse, Error> {
    if !appservice.compare_hs_token(request.access_token) {
        return Ok(HttpResponse::Unauthorized().finish());
    }

    appservice.get_cached_client(None)?.receive_transaction(request.incoming).await?;

    Ok(HttpResponse::Ok().json("{}"))
}

#[tracing::instrument]
#[get("/users/{user_id}")]
async fn query_user_id(
    request: IncomingRequest<api::query::query_user_id::v1::IncomingRequest>,
    appservice: Data<Appservice>,
) -> Result<HttpResponse, Error> {
    if !appservice.compare_hs_token(request.access_token) {
        return Ok(HttpResponse::Unauthorized().finish());
    }

    Ok(HttpResponse::Ok().json("{}"))
}

#[tracing::instrument]
#[get("/rooms/{room_alias}")]
async fn query_room_alias(
    request: IncomingRequest<api::query::query_room_alias::v1::IncomingRequest>,
    appservice: Data<Appservice>,
) -> Result<HttpResponse, Error> {
    if !appservice.compare_hs_token(request.access_token) {
        return Ok(HttpResponse::Unauthorized().finish());
    }

    Ok(HttpResponse::Ok().json("{}"))
}

#[derive(Debug)]
pub struct IncomingRequest<T> {
    access_token: String,
    incoming: T,
}

impl<T: matrix_sdk::IncomingRequest> FromRequest for IncomingRequest<T> {
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;
    type Config = ();

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let request = request.to_owned();
        let payload = payload.take();

        Box::pin(async move {
            let uri = request.uri().to_owned();

            let uri = if !uri.path().starts_with("/_matrix/app/v1") {
                // rename legacy routes
                let mut parts = uri.into_parts();
                let path_and_query = match parts.path_and_query {
                    Some(path_and_query) => format!("/_matrix/app/v1{}", path_and_query),
                    None => "/_matrix/app/v1".to_owned(),
                };
                parts.path_and_query =
                    Some(PathAndQuery::try_from(path_and_query).map_err(http::Error::from)?);
                parts.try_into().map_err(http::Error::from)?
            } else {
                uri
            };

            let mut builder = http::request::Builder::new().method(request.method()).uri(uri);

            let headers = builder.headers_mut().ok_or(Error::UnknownHttpRequestBuilder)?;
            for (key, value) in request.headers().iter() {
                headers.append(key, value.to_owned());
            }

            let bytes = payload
                .try_fold(BytesMut::new(), |mut body, chunk| async move {
                    body.extend_from_slice(&chunk);
                    Ok::<_, PayloadError>(body)
                })
                .and_then(|bytes| async move { Ok::<Vec<u8>, _>(bytes.into_iter().collect()) })
                .await?;

            let access_token = match request.uri().query() {
                Some(query) => {
                    let query: Vec<(String, String)> = matrix_sdk::urlencoded::from_str(query)?;
                    query.into_iter().find(|(key, _)| key == "access_token").map(|(_, value)| value)
                }
                None => None,
            };

            let access_token = match access_token {
                Some(access_token) => access_token,
                None => return Err(Error::MissingAccessToken),
            };

            let request = builder.body(bytes)?;

            Ok(IncomingRequest {
                access_token,
                incoming: matrix_sdk::IncomingRequest::try_from_http_request(request)?,
            })
        })
    }
}
