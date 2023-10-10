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

use ruma::{
    api::client::account::request_openid_token::v3::Response as RumaOpenIdResponse,
    events::AnyTimelineEvent, serde::Raw, OwnedEventId,
};

use super::actions::CommandResult;
use crate::widget::Permissions;

/// Incoming event that the client API must process.
pub enum Event {
    /// An incoming raw message from the widget.
    MessageFromWidget(String),
    /// Matrix event received. This one is delivered as a result of client
    /// subscribing to the events (`Action::Subscribe` command).
    MatrixEventReceived(Raw<AnyTimelineEvent>),
    /// Client acquired permissions from the user.
    /// A response to an `Action::AcquirePermissions` command.
    PermissionsAcquired(CommandResult<Permissions>),
    /// Client got OpenId token for a given request ID.
    /// A response to an `Action::GetOpenId` command.
    OpenIdReceived(CommandResult<RumaOpenIdResponse>),
    /// Client read some matrix event(s).
    /// A response to an `Action::ReadMatrixEvent` commands.
    MatrixEventRead(CommandResult<Vec<Raw<AnyTimelineEvent>>>),
    /// Client sent some matrix event. The response contains the event ID.
    /// A response to an `Action::SendMatrixEvent` command.
    MatrixEventSent(CommandResult<OwnedEventId>),
}
