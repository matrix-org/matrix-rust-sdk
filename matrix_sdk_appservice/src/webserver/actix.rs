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

use std::pin::Pin;

pub use actix_web::Scope;
use actix_web::{
    dev::Payload,
    error::PayloadError,
    get, put,
    web::{self, BytesMut, Data},
    App, FromRequest, HttpRequest, HttpResponse, HttpServer,
};
use futures::Future;
use futures_util::TryStreamExt;
use ruma::api::appservice as api;

use crate::{error::Error, AppService};

pub async fn run_server(
    appservice: AppService,
    host: impl Into<String>,
    port: impl Into<u16>,
) -> Result<(), Error> {
    HttpServer::new(move || App::new().configure(appservice.actix_configure()))
        .bind((host.into(), port.into()))?
        .run()
        .await?;

    Ok(())
}

pub fn configure(config: &mut actix_web::web::ServiceConfig) {
    // also handles legacy routes
    config.service(push_transactions).service(query_user_id).service(query_room_alias).service(
        web::scope("/_matrix/app/v1")
            .service(push_transactions)
            .service(query_user_id)
            .service(query_room_alias),
    );
}

#[tracing::instrument]
#[put("/transactions/{txn_id}")]
async fn push_transactions(
    request: IncomingRequest<api::event::push_events::v1::IncomingRequest>,
    appservice: Data<AppService>,
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
    appservice: Data<AppService>,
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
    appservice: Data<AppService>,
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

impl<T: ruma::api::IncomingRequest> FromRequest for IncomingRequest<T> {
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;
    type Config = ();

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let request = request.to_owned();
        let payload = payload.take();

        Box::pin(async move {
            let mut builder =
                http::request::Builder::new().method(request.method()).uri(request.uri());

            let headers = builder.headers_mut().ok_or(Error::UnknownHttpRequestBuilder)?;
            for (key, value) in request.headers().iter() {
                headers.append(key, value.to_owned());
            }

            let bytes = payload
                .try_fold(BytesMut::new(), |mut body, chunk| async move {
                    body.extend_from_slice(&chunk);
                    Ok::<_, PayloadError>(body)
                })
                .await?
                .into();

            let access_token = match request.uri().query() {
                Some(query) => {
                    let query: Vec<(String, String)> = ruma::serde::urlencoded::from_str(query)?;
                    query.into_iter().find(|(key, _)| key == "access_token").map(|(_, value)| value)
                }
                None => None,
            };

            let access_token = match access_token {
                Some(access_token) => access_token,
                None => return Err(Error::MissingAccessToken),
            };

            let request = builder.body(bytes)?;
            let request = crate::transform_legacy_route(request)?;

            Ok(IncomingRequest {
                access_token,
                incoming: ruma::api::IncomingRequest::try_from_http_request(request)?,
            })
        })
    }
}
