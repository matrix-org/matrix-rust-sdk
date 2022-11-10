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
    convert::Infallible,
    future::Future,
    net::ToSocketAddrs,
    pin::Pin,
    task::{self, Poll},
};

use axum::{
    async_trait,
    body::{Bytes, HttpBody},
    extract::{FromRequest, Path, RequestParts},
    middleware::{self, Next},
    response::{ErrorResponse, IntoResponse, Response},
    routing::{future::RouteFuture, get, put},
    BoxError, Extension, Json, Router,
};
use http::StatusCode;
use hyper::Body;
use matrix_sdk::ruma::{self, api::IncomingRequest};
use serde::{Deserialize, Serialize};
use tower::{make, Service, ServiceBuilder};

use crate::{AppService, Error, Result};

pub async fn run_server(
    appservice: AppService,
    host: impl Into<String>,
    port: impl Into<u16>,
) -> Result<()> {
    let router: AppServiceRouter = router(appservice);

    let mut addr = (host.into(), port.into()).to_socket_addrs()?;
    if let Some(addr) = addr.next() {
        hyper::Server::bind(&addr).serve(make::Shared::new(router)).await?;
        Ok(())
    } else {
        Err(Error::HostPortToSocketAddrs)
    }
}

pub fn router<B>(appservice: AppService) -> AppServiceRouter<B>
where
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    AppServiceRouter(
        Router::new()
            .route("/_matrix/app/v1/users/:user_id", get(handlers::user))
            .route("/_matrix/app/v1/rooms/:room_id", get(handlers::room))
            .route("/_matrix/app/v1/transactions/:txn_id", put(handlers::transaction))
            .route("/users/:user_id", get(handlers::user))
            .route("/rooms/:room_id", get(handlers::room))
            .route("/transactions/:txn_id", put(handlers::transaction))
            // FIXME: Use Route::with_state instead of an Extension layer in axum 0.6
            .layer(
                ServiceBuilder::new()
                    .layer(Extension(appservice))
                    .layer(middleware::from_fn(validate_access_token)),
            ),
    )
}

#[derive(Debug)]
pub struct AppServiceRouter<B = Body>(Router<B>);

impl<B> Clone for AppServiceRouter<B> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<B> Service<http::Request<B>> for AppServiceRouter<B>
where
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    // axum's Response type is part of the signature because axum::Router::nest
    // requires the inner service to have that exact response (body) type in
    // 0.5.x; this will be fixed in axum 0.6.0.
    type Response = Response;
    type Error = Infallible;
    type Future = AppServiceRouteFuture<B>;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        // When the AppServiceRouter is nested inside another axum Router under
        // a path that includes path parameters, those should not be received by
        // the Path extractor used inside the `handlers` module.
        req.extensions_mut().clear();

        AppServiceRouteFuture(self.0.call(req))
    }
}

pub struct AppServiceRouteFuture<B>(RouteFuture<B, Infallible>);

impl<B> Future for AppServiceRouteFuture<B>
where
    B: HttpBody,
{
    type Output = Result<Response, Infallible>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

pub struct MatrixRequest<T>(T);

#[async_trait]
impl<T, B> FromRequest<B> for MatrixRequest<T>
where
    T: IncomingRequest,
    B: HttpBody + Send,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    type Rejection = Response;

    async fn from_request(req: &mut RequestParts<B>) -> Result<Self, Self::Rejection> {
        let path_params =
            req.extract::<Path<Vec<String>>>().await.map_err(IntoResponse::into_response)?;
        let parts = req.extract::<http::request::Parts>().await.map_err(|e| match e {})?;
        let body = req.extract::<Bytes>().await.map_err(IntoResponse::into_response)?;

        let http_request = http::Request::from_parts(parts, body);

        let request = T::try_from_http_request(http_request, &path_params).map_err(|_e| {
            // TODO: JSON error response
            StatusCode::BAD_REQUEST.into_response()
        })?;

        Ok(Self(request))
    }
}

mod handlers {
    use axum::{response::IntoResponse, Extension, Json};
    use http::StatusCode;
    use ruma::api::appservice::{
        event::push_events,
        query::{query_room_alias, query_user_id},
    };
    use serde::Serialize;

    use super::{ErrorMessage, MatrixRequest};
    use crate::AppService;

    #[derive(Serialize)]
    struct EmptyObject {}

    pub async fn user(
        Extension(appservice): Extension<AppService>,
        MatrixRequest(request): MatrixRequest<query_user_id::v1::IncomingRequest>,
    ) -> impl IntoResponse {
        if let Some(user_exists) = appservice.event_handler.users.lock().await.as_mut() {
            if user_exists(appservice.clone(), request).await {
                Ok(Json(EmptyObject {}))
            } else {
                Err(StatusCode::NOT_FOUND)
            }
        } else {
            Ok(Json(EmptyObject {}))
        }
    }

    pub async fn room(
        Extension(appservice): Extension<AppService>,
        MatrixRequest(request): MatrixRequest<query_room_alias::v1::IncomingRequest>,
    ) -> impl IntoResponse {
        if let Some(room_exists) = appservice.event_handler.rooms.lock().await.as_mut() {
            if room_exists(appservice.clone(), request).await {
                Ok(Json(&EmptyObject {}))
            } else {
                Err(StatusCode::NOT_FOUND)
            }
        } else {
            Ok(Json(&EmptyObject {}))
        }
    }

    pub async fn transaction(
        appservice: Extension<AppService>,
        MatrixRequest(request): MatrixRequest<push_events::v1::IncomingRequest>,
    ) -> impl IntoResponse {
        match appservice.receive_transaction(request).await {
            Ok(_) => Ok(Json(&EmptyObject {})),
            Err(e) => {
                let status_code = StatusCode::INTERNAL_SERVER_ERROR;
                Err((
                    status_code,
                    Json(ErrorMessage { code: status_code.as_u16(), message: e.to_string() }),
                ))
            }
        }
    }
}

#[derive(Deserialize)]
struct QueryParameters {
    access_token: String,
}

#[derive(Serialize)]
struct ErrorMessage {
    code: u16,
    message: String,
}

async fn validate_access_token<B>(
    req: http::Request<B>,
    next: Next<B>,
) -> Result<Response, ErrorResponse> {
    let appservice =
        req.extensions().get::<AppService>().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let query_string = req.uri().query().unwrap_or("");
    match ruma::serde::urlencoded::from_str::<QueryParameters>(query_string) {
        Ok(query) if query.access_token == appservice.registration.hs_token => {
            Ok(next.run(req).await)
        }
        _ => {
            let status_code = StatusCode::UNAUTHORIZED;
            let message =
                ErrorMessage { code: status_code.as_u16(), message: "UNAUTHORIZED".into() };
            Err((status_code, Json(message)).into())
        }
    }
}
