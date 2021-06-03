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

use std::{net::ToSocketAddrs, result::Result as StdResult};

use futures::TryFutureExt;
use matrix_sdk::Bytes;
use serde::Serialize;
use warp::{filters::BoxedFilter, path::FullPath, Filter, Rejection, Reply};

use crate::{Appservice, Error, Result};

pub async fn run_server(
    appservice: Appservice,
    host: impl Into<String>,
    port: impl Into<u16>,
) -> Result<()> {
    let routes = warp_filter(appservice);

    let mut addr = format!("{}:{}", host.into(), port.into()).to_socket_addrs()?;
    if let Some(addr) = addr.next() {
        warp::serve(routes).run(addr).await;
        Ok(())
    } else {
        Err(Error::HostPortToSocketAddrs)
    }
}

pub fn warp_filter(appservice: Appservice) -> BoxedFilter<(impl Reply,)> {
    warp::any()
        .and(filters::valid_access_token(appservice.registration().hs_token.clone()))
        .and(filters::transactions(appservice))
        .or(filters::users())
        .or(filters::rooms())
        .recover(handle_rejection)
        .boxed()
}

mod filters {
    use super::*;

    pub fn users() -> BoxedFilter<(impl Reply,)> {
        warp::get()
            .and(
                warp::path!("_matrix" / "app" / "v1" / "users" / String)
                    // legacy route
                    .or(warp::path!("users" / String))
                    .unify(),
            )
            .and_then(handlers::user)
            .boxed()
    }

    pub fn rooms() -> BoxedFilter<(impl Reply,)> {
        warp::get()
            .and(
                warp::path!("_matrix" / "app" / "v1" / "rooms" / String)
                    // legacy route
                    .or(warp::path!("rooms" / String))
                    .unify(),
            )
            .and_then(handlers::room)
            .boxed()
    }

    pub fn transactions(appservice: Appservice) -> BoxedFilter<(impl Reply,)> {
        warp::put()
            .and(
                warp::path!("_matrix" / "app" / "v1" / "transactions" / String)
                    // legacy route
                    .or(warp::path!("transactions" / String))
                    .unify(),
            )
            .and(with_appservice(appservice))
            .and(http_request().and_then(|request| async move {
                let request = crate::transform_legacy_route(request).map_err(Error::from)?;
                Ok::<http::Request<Bytes>, Rejection>(request)
            }))
            .and_then(handlers::transaction)
            .boxed()
    }

    pub fn with_appservice(
        appservice: Appservice,
    ) -> impl Filter<Extract = (Appservice,), Error = std::convert::Infallible> + Clone {
        warp::any().map(move || appservice.clone())
    }

    pub fn valid_access_token(token: String) -> BoxedFilter<()> {
        warp::any()
            .map(move || token.clone())
            .and(warp::query::raw())
            .and_then(|token: String, query: String| async move {
                let query: Vec<(String, String)> =
                    matrix_sdk::urlencoded::from_str(&query).map_err(Error::from)?;

                if query.into_iter().any(|(key, value)| key == "access_token" && value == token) {
                    Ok::<(), Rejection>(())
                } else {
                    Err(warp::reject::custom(Unauthorized))
                }
            })
            .untuple_one()
            .boxed()
    }

    pub fn http_request() -> impl Filter<Extract = (http::Request<Bytes>,), Error = Rejection> + Copy
    {
        // TODO: extract `hyper::Request` instead
        // blocked by https://github.com/seanmonstar/warp/issues/139
        warp::any()
            .and(warp::method())
            .and(warp::filters::path::full())
            .and(warp::filters::query::raw())
            .and(warp::header::headers_cloned())
            .and(warp::body::bytes())
            .and_then(|method, path: FullPath, query, headers, bytes| async move {
                let uri = http::uri::Builder::new()
                    .path_and_query(format!("{}?{}", path.as_str(), query))
                    .build()
                    .map_err(Error::from)?;

                let mut request = http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(bytes)
                    .map_err(Error::from)?;

                *request.headers_mut() = headers;

                Ok::<http::Request<Bytes>, Rejection>(request)
            })
    }
}

mod handlers {
    use super::*;

    pub async fn user(_: String) -> StdResult<impl warp::Reply, Rejection> {
        Ok(warp::reply::json(&String::from("{}")))
    }

    pub async fn room(_: String) -> StdResult<impl warp::Reply, Rejection> {
        Ok(warp::reply::json(&String::from("{}")))
    }

    pub async fn transaction(
        _: String,
        appservice: Appservice,
        request: http::Request<Bytes>,
    ) -> StdResult<impl warp::Reply, Rejection> {
        let incoming_transaction: matrix_sdk::api_appservice::event::push_events::v1::IncomingRequest =
            matrix_sdk::IncomingRequest::try_from_http_request(request).map_err(Error::from)?;

        let client = appservice.get_cached_client(None)?;
        client.receive_transaction(incoming_transaction).map_err(Error::from).await?;

        Ok(warp::reply::json(&String::from("{}")))
    }
}

#[derive(Debug)]
struct Unauthorized;

impl warp::reject::Reject for Unauthorized {}

#[derive(Serialize)]
struct ErrorMessage {
    code: u16,
    message: String,
}

pub async fn handle_rejection(
    err: Rejection,
) -> std::result::Result<impl Reply, std::convert::Infallible> {
    let mut code = http::StatusCode::INTERNAL_SERVER_ERROR;
    let mut message = "INTERNAL_SERVER_ERROR";

    if err.find::<Unauthorized>().is_some() || err.find::<warp::reject::InvalidQuery>().is_some() {
        code = http::StatusCode::UNAUTHORIZED;
        message = "UNAUTHORIZED";
    }

    let json = warp::reply::json(&ErrorMessage { code: code.as_u16(), message: message.into() });

    Ok(warp::reply::with_status(json, code))
}
