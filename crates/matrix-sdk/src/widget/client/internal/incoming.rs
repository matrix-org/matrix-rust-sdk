// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Facilities to handle incoming requests from the widget.

use std::sync::{Arc, Mutex};

use super::{
    super::{actions::ReadEventCommand, negotiate_permissions, ClientApiSettings},
    messages::{
        from_widget::{
            ReadEventBody, ReadEventResponseBody, SendEventBody, SendEventResponseBody,
            SupportedApiVersionsResponse,
        },
        Empty, OpenIdResponse, OpenIdState,
    },
    outgoing::{CommandProxy, ReadMatrixEvent, RequestOpenId, SendMatrixEvent, UpdateOpenId},
};
use crate::widget::{
    filter::{MatrixEventContent, MatrixEventFilterInput},
    Permissions,
};

/// Everything that we need in order to process an incoming request.
pub struct HandlerContext {
    pub permissions: Arc<Mutex<Option<Permissions>>>,
    pub settings: ClientApiSettings,
    pub proxy: Arc<CommandProxy>,
}

impl HandlerContext {
    pub fn new(
        permissions: Arc<Mutex<Option<Permissions>>>,
        settings: ClientApiSettings,
        proxy: Arc<CommandProxy>,
    ) -> Self {
        Self { permissions, settings, proxy }
    }

    fn permissions(&self) -> Option<Permissions> {
        self.permissions.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
pub trait RequestHandler {
    type Request: Sized;
    type Response: Sized;

    async fn handle(
        ctx: HandlerContext,
        request_id: String,
        request: Self::Request,
    ) -> Result<Self::Response, String>;
}

pub struct GetSupportedApiVersionsRequest;

#[async_trait::async_trait]
impl RequestHandler for GetSupportedApiVersionsRequest {
    type Request = Empty;
    type Response = SupportedApiVersionsResponse;

    async fn handle(
        _: HandlerContext,
        _: String,
        _: Self::Request,
    ) -> Result<Self::Response, String> {
        Ok(SupportedApiVersionsResponse::new())
    }
}

pub struct ContentLoadedRequest;

#[async_trait::async_trait]
impl RequestHandler for ContentLoadedRequest {
    type Request = Empty;
    type Response = Empty;

    async fn handle(
        ctx: HandlerContext,
        _: String,
        _: Self::Request,
    ) -> Result<Self::Response, String> {
        let (response, negotiate) = match (ctx.settings.init_on_content_load, ctx.permissions()) {
            (true, None) => (Ok(Empty {}), true),
            (true, Some(..)) => (Err("Already loaded".into()), false),
            _ => (Ok(Empty {}), false),
        };

        if negotiate {
            // TODO: Actually we need to spawn this *after* sending a reply.
            tokio::spawn(async move {
                if let Ok(negotiated) = negotiate_permissions(ctx.proxy).await {
                    ctx.permissions.lock().unwrap().replace(negotiated);
                }
            });
        }

        response
    }
}

pub struct GetOpenIdRequest;

#[async_trait::async_trait]
impl RequestHandler for GetOpenIdRequest {
    type Request = Empty;
    type Response = OpenIdResponse;

    async fn handle(
        ctx: HandlerContext,
        id: String,
        _: Self::Request,
    ) -> Result<Self::Response, String> {
        // TODO: Actually we need to spawn this *after* sending a reply.
        tokio::spawn(async move {
            let response = match ctx.proxy.send(RequestOpenId).await {
                Ok(openid) => OpenIdResponse::Allowed(OpenIdState::new(id, openid)),
                Err(_) => OpenIdResponse::Blocked,
            };

            if let Err(err) = ctx.proxy.send(UpdateOpenId(response)).await {
                tracing::error!("Widget rejected OpenID response: {}", err);
            }
        });

        Ok(OpenIdResponse::Pending)
    }
}

pub struct ReadEventRequest;

#[async_trait::async_trait]
impl RequestHandler for ReadEventRequest {
    type Request = ReadEventBody;
    type Response = ReadEventResponseBody;

    async fn handle(
        ctx: HandlerContext,
        _: String,
        req: Self::Request,
    ) -> Result<Self::Response, String> {
        let filters = ctx.permissions().map(|p| p.read).unwrap_or_default();

        let input = req.clone().into();
        if !filters.iter().any(|f| f.matches(&input)) {
            return Err("Not allowed".into());
        }

        let cmd = ReadEventCommand { event_type: req.event_type, limit: req.limit.unwrap_or(50) };
        let events = ctx.proxy.send(ReadMatrixEvent(cmd)).await.map_err(|err| err.to_string())?;

        Ok(events
            .into_iter()
            .filter(|raw| {
                raw.deserialize_as()
                    .ok()
                    .map(|de_helper| filters.iter().any(|f| f.matches(&de_helper)))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>()
            .into())
    }
}

pub struct SendEventRequest;

#[async_trait::async_trait]
impl RequestHandler for SendEventRequest {
    type Request = SendEventBody;
    type Response = SendEventResponseBody;

    async fn handle(
        ctx: HandlerContext,
        _: String,
        req: Self::Request,
    ) -> Result<Self::Response, String> {
        let filters = ctx.permissions().map(|p| p.send).unwrap_or_default();

        let input = req.clone().into();
        if !filters.iter().any(|f| f.matches(&input)) {
            return Err("Not allowed".into());
        }

        let id = ctx.proxy.send(SendMatrixEvent(req)).await.map_err(|err| err.to_string())?;
        Ok(SendEventResponseBody {
            event_id: id.to_string(),
            room_id: ctx.settings.room_id.to_string(),
        })
    }
}

impl From<ReadEventBody> for MatrixEventFilterInput {
    fn from(body: ReadEventBody) -> Self {
        Self {
            event_type: body.event_type,
            state_key: body.state_key,
            content: MatrixEventContent::default(),
        }
    }
}

impl From<SendEventBody> for MatrixEventFilterInput {
    fn from(body: SendEventBody) -> Self {
        Self {
            event_type: body.event_type,
            state_key: body.state_key,
            content: serde_json::from_value(body.content).unwrap_or_default(),
        }
    }
}
