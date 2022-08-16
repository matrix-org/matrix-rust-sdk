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

use std::net::ToSocketAddrs;

use matrix_sdk::{
    bytes::Bytes,
    ruma::{
        self,
        api::{
            appservice::query::{
                query_room_alias::v1 as query_room, query_user_id::v1 as query_user,
            },
            IncomingRequest,
        },
    },
};
use serde::Serialize;
use warp::{filters::BoxedFilter, path::FullPath, Filter, Rejection, Reply};

use crate::{AppService, Error, Result};

pub async fn run_server(
    appservice: AppService,
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

pub fn warp_filter(appservice: AppService) -> BoxedFilter<(impl Reply,)> {
    // TODO: try to use a struct instead of needlessly cloning appservice multiple
    // times on every request
    warp::any()
        .and(filters::transactions(appservice.clone()))
        .or(filters::users(appservice.clone()))
        .or(filters::rooms(appservice))
        .recover(handle_rejection)
        .boxed()
}

mod filters {
    use super::*;

    pub fn users(appservice: AppService) -> BoxedFilter<(impl Reply,)> {
        warp::get()
            .and(
                warp::path!("_matrix" / "app" / "v1" / "users" / String)
                    // legacy route
                    .or(warp::path!("users" / String))
                    .unify(),
            )
            .and(warp::path::end())
            .and(common(appservice))
            .and_then(handlers::user)
            .boxed()
    }

    pub fn rooms(appservice: AppService) -> BoxedFilter<(impl Reply,)> {
        warp::get()
            .and(
                warp::path!("_matrix" / "app" / "v1" / "rooms" / String)
                    // legacy route
                    .or(warp::path!("rooms" / String))
                    .unify(),
            )
            .and(warp::path::end())
            .and(common(appservice))
            .and_then(handlers::room)
            .boxed()
    }

    pub fn transactions(appservice: AppService) -> BoxedFilter<(impl Reply,)> {
        warp::put()
            .and(
                warp::path!("_matrix" / "app" / "v1" / "transactions" / String)
                    // legacy route
                    .or(warp::path!("transactions" / String))
                    .unify(),
            )
            .and(warp::path::end())
            .and(common(appservice))
            .and_then(handlers::transaction)
            .boxed()
    }

    fn common(appservice: AppService) -> BoxedFilter<(AppService, http::Request<Bytes>)> {
        warp::any()
            .and(valid_access_token(appservice.registration().hs_token.clone()))
            .map(move || appservice.clone())
            .and(
                http_request().and_then(|request| async move {
                    Ok::<http::Request<Bytes>, Rejection>(request)
                }),
            )
            .boxed()
    }

    pub fn valid_access_token(token: String) -> BoxedFilter<()> {
        warp::any()
            .map(move || token.clone())
            .and(warp::query::raw())
            .and_then(|token: String, query: String| async move {
                let query: Vec<(String, String)> =
                    ruma::serde::urlencoded::from_str(&query).map_err(Error::from)?;

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
                    .path_and_query(format!("{}?{query}", path.as_str()))
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
    use percent_encoding::percent_decode_str;
    use serde::Serialize;

    use super::*;

    #[derive(Serialize)]
    struct EmptyObject {}

    pub async fn user(
        user_id: String,
        appservice: AppService,
        request: http::Request<Bytes>,
    ) -> Result<impl Reply, Rejection> {
        if let Some(user_exists) = appservice.event_handler.users.lock().await.as_mut() {
            let user_id = percent_decode_str(&user_id).decode_utf8().map_err(Error::from)?;
            let request = query_user::IncomingRequest::try_from_http_request(request, &[user_id])
                .map_err(Error::from)?;
            return if user_exists(appservice.clone(), request).await {
                Ok(warp::reply::json(&EmptyObject {}))
            } else {
                Err(warp::reject::not_found())
            };
        }
        Ok(warp::reply::json(&EmptyObject {}))
    }

    pub async fn room(
        room_id: String,
        appservice: AppService,
        request: http::Request<Bytes>,
    ) -> Result<impl Reply, Rejection> {
        if let Some(room_exists) = appservice.event_handler.rooms.lock().await.as_mut() {
            let room_id = percent_decode_str(&room_id).decode_utf8().map_err(Error::from)?;
            let request = query_room::IncomingRequest::try_from_http_request(request, &[room_id])
                .map_err(Error::from)?;
            return if room_exists(appservice.clone(), request).await {
                Ok(warp::reply::json(&EmptyObject {}))
            } else {
                Err(warp::reject::not_found())
            };
        }
        Ok(warp::reply::json(&EmptyObject {}))
    }

    pub async fn transaction(
        txn_id: String,
        appservice: AppService,
        request: http::Request<Bytes>,
    ) -> Result<impl Reply, Rejection> {
        let incoming_transaction: ruma::api::appservice::event::push_events::v1::IncomingRequest =
            ruma::api::IncomingRequest::try_from_http_request(request, &[txn_id])
                .map_err(Error::from)?;

        appservice.receive_transaction(incoming_transaction).await?;
        Ok(warp::reply::json(&EmptyObject {}))
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

pub async fn handle_rejection(err: Rejection) -> Result<impl Reply, Rejection> {
    if err.find::<Unauthorized>().is_some() || err.find::<warp::reject::InvalidQuery>().is_some() {
        let code = http::StatusCode::UNAUTHORIZED;
        let message = "UNAUTHORIZED";

        let json =
            warp::reply::json(&ErrorMessage { code: code.as_u16(), message: message.into() });
        Ok(warp::reply::with_status(json, code))
    } else {
        Err(err)
    }
}
