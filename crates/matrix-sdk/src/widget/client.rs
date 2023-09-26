//! Internal client widget API implementation.

use std::{borrow::Cow, ops::Deref};

use ruma::{
    events::{AnySyncTimelineEvent, TimelineEventType},
    serde::Raw,
};
use serde_json::Value as JsonValue;

use super::Permissions;

/// State machine that handles the client widget API interractions.
pub struct ClientApi;

impl ClientApi {
    /// Creates a new instance of a client widget API state machine.
    pub fn new() -> Self {
        Self
    }

    /// Processes an incoming event (an incoming raw message from a widget,
    /// or a data produced as a result of a previously sent `Action`).
    /// Produceses a list of actions that the client must perform.
    pub fn process(&mut self, _event: Event) -> Vec<Action> {
        // TODO: Process the event.
        Vec::new()
    }
}

/// Incoming event that the client API must process.
pub enum Event {
    /// An incoming raw message from the widget.
    MessageFromWidget(String),
    /// Client acquired permissions from the user.
    /// A response to an `Action::AcquirePermissions` command.
    PermissionsAcquired(CommandResult<Permissions>),
    /// Client got OpenId token for a given request ID.
    /// A response to an `Action::GetOpenId` command.
    OpenIdReceived(CommandResult<String>),
    /// Client read some matrix event(s).
    /// A response to an `Action::ReadMatrixEvent` or `Action::Subscribe`
    /// commands.
    MatrixEventRead(CommandResult<Vec<Raw<AnySyncTimelineEvent>>>),
    /// Client sent some matrix event. The response contains the event ID.
    /// A response to an `Action::SendMatrixEvent` command.
    MatrixEventSent(CommandResult<String>),
}

/// Action (a command) that client (driver) must perform.
#[allow(dead_code)] // TODO: Remove once all actions are implemented.
pub enum Action {
    /// Send a raw message to the widget.
    SendToWidget(String),
    /// Acquire permissions from the user given the set of desired permissions.
    /// Must eventually be answered with `Event::PermissionsAcquired`.
    AcquirePermissions(Command<Permissions>),
    /// Get OpenId token for a given request ID.
    GetOpenId(Command<String>),
    /// Read matrix event(s) that corresponds to the given description.
    ReadMatrixEvent(Command<ReadEventCommand>),
    // Send matrix event that corresponds to the given description.
    SendMatrixEvent(Command<SendEventCommand>),
    /// Subscribe to the events in the *current* room, i.e. a room which this
    /// widget is instantiated with. The client is aware of the room.
    Subscribe,
    /// Unsuscribe from the events in the *current* room. Symmetrical to
    /// `Subscribe`.
    Unsubscribe,
}

/// Command to read matrix event(s).
pub struct ReadEventCommand {
    /// Read event(s) of a given type.
    pub event_type: TimelineEventType,
    /// Limits for the Matrix request.
    pub limit: usize,
}

/// Command to send matrix event.
pub struct SendEventCommand {
    /// type of an event.
    pub event_type: TimelineEventType,
    /// State key of an event (if it's a state event).
    pub state_key: Option<String>,
    /// Raw content of an event.
    pub content: JsonValue,
}

/// Command that is sent from the client widget API state machine to the
/// client (driver) that must be performed. Once the command is executed,
/// the client will typically generate an `Event` with the result of it.
pub struct Command<T> {
    /// Certain commands are typically answered with certain event once the
    /// command is performed. The api state machine will "tag" each command
    /// with some "cookie" (in this case just an ID), so that once the
    /// result of the execution of this command is received, it could be
    /// matched.
    id: String,
    // Data associated with this command.
    data: T,
}

impl<T> Command<T> {
    /// Consumes the command and produces a command result with given data.
    pub fn result<R>(self, result: Result<R, Cow<'static, str>>) -> CommandResult<R> {
        CommandResult { id: self.id, result }
    }
}

impl<T> Deref for Command<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

/// The result of the execution of a command. Note that this type can only be
/// constructed within this module, i.e. it can only be constructed as a result
/// of a command that has been sent from this module, which means that the
/// client (driver) won't be able to send "invalid" commands, because they could
/// only be generated from a `Command` instance.
#[allow(dead_code)] // TODO: Remove once results are used.
pub struct CommandResult<T> {
    /// ID of the command that was executed. See `Command::id` for more details.
    id: String,
    /// Result of the execution of the command.
    result: Result<T, Cow<'static, str>>,
}
