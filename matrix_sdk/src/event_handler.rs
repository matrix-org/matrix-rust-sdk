// Copyright 2021 Jonas Platte
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

//! Types and traits related for event handlers. For usage, see
//! [`Client::register_event_handler`].

use std::{borrow::Cow, future::Future, ops::Deref};

use matrix_sdk_base::deserialized_responses::SyncRoomEvent;
use ruma::{events::AnySyncStateEvent, serde::Raw};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;

use crate::{room, Client};

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventKind {
    GlobalAccountData,
    RoomAccountData,
    EphemeralRoomData,
    Message { redacted: bool },
    State { redacted: bool },
    StrippedState { redacted: bool },
    InitialState,
    ToDevice,
    Presence,
}

/// A statically-known event kind/type that can be retrieved from an event sync.
pub trait SyncEvent {
    #[doc(hidden)]
    const ID: (EventKind, &'static str);
}

/// Interface for event handlers.
///
/// This trait is an abstraction for a certain kind of functions / closures,
/// specifically:
///
/// * They must have at least one argument, which is the event itself, a type
///   that implements [`SyncEvent`]. Any additional arguments need to implement
///   the [`EventHandlerContext`] trait.
/// * Their return type has to be one of: `()`, `Result<(), impl
///   std::error::Error>` or `anyhow::Result<()>` (requires the `anyhow` Cargo
///   feature to be enabled)
pub trait EventHandler<Ev, Ctx>: Clone + Send + Sync + 'static {
    /// The future returned by `handle_event`.
    #[doc(hidden)]
    type Future: Future + Send + 'static;

    /// The event type being handled, for example a message event of type
    /// `m.room.message`.
    #[doc(hidden)]
    const ID: (EventKind, &'static str);

    /// Create a future for handling the given event.
    ///
    /// `data` provides additional data about the event, for example the room it
    /// appeared in.
    ///
    /// Returns `None` if one of the context extractors failed.
    #[doc(hidden)]
    fn handle_event(&self, ev: Ev, data: EventHandlerData<'_>) -> Option<Self::Future>;
}

#[doc(hidden)]
#[derive(Debug)]
pub struct EventHandlerData<'a> {
    pub client: Client,
    pub room: Option<room::Room>,
    pub raw: &'a RawJsonValue,
}

/// Context for an event handler.
///
/// This trait defines the set of types that may be used as additional arguments
/// in event handler functions after the event itself.
pub trait EventHandlerContext: Sized {
    #[doc(hidden)]
    fn from_data(_: &EventHandlerData<'_>) -> Option<Self>;
}

impl EventHandlerContext for Client {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        Some(data.client.clone())
    }
}

impl EventHandlerContext for room::Room {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        data.room.clone()
    }
}

/// The raw JSON form of an event.
///
/// Used as a context argument for event handlers (see
/// [`Client::register_event_handler`]).
// FIXME: This could be made to not own the raw JSON value with some changes to
//        the traits above, but only with GATs.
#[derive(Clone, Debug)]
pub struct RawEvent(pub Box<RawJsonValue>);

impl Deref for RawEvent {
    type Target = RawJsonValue;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl EventHandlerContext for RawEvent {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        Some(Self(data.raw.to_owned()))
    }
}

/// Return types supported for event handlers implement this trait.
///
/// It is not meant to be implemented outside of matrix-sdk.
pub trait EventHandlerResult: Sized {
    #[doc(hidden)]
    fn print_error(&self, event_type: &str);
}

impl EventHandlerResult for () {
    fn print_error(&self, _event_type: &str) {}
}

impl<E: std::error::Error> EventHandlerResult for Result<(), E> {
    fn print_error(&self, event_type: &str) {
        if let Err(e) = self {
            tracing::error!("Event handler for `{}` failed: {}", event_type, e);
        }
    }
}

#[cfg(feature = "anyhow")]
impl EventHandlerResult for anyhow::Result<()> {
    fn print_error(&self, event_type: &str) {
        if let Err(e) = self {
            tracing::error!("Event handler for `{}` failed: {:?}", event_type, e);
        }
    }
}

#[derive(Deserialize)]
struct UnsignedDetails {
    redacted_because: Option<serde::de::IgnoredAny>,
}

impl Client {
    pub(crate) async fn handle_sync_events<T>(
        &self,
        kind: EventKind,
        room: &Option<room::Room>,
        events: &[Raw<T>],
    ) -> serde_json::Result<()> {
        self.handle_sync_events_wrapped(kind, room, events, |x| x).await
    }

    pub(crate) async fn handle_sync_state_events(
        &self,
        room: &Option<room::Room>,
        state_events: &[Raw<AnySyncStateEvent>],
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct StateEventDetails<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
            unsigned: Option<UnsignedDetails>,
        }

        self.handle_sync_events_wrapped_with(room, state_events, std::convert::identity, |raw| {
            let StateEventDetails { event_type, unsigned } = raw.deserialize_as()?;
            let redacted = unsigned.and_then(|u| u.redacted_because).is_some();
            Ok((EventKind::State { redacted }, event_type))
        })
        .await
    }

    pub(crate) async fn handle_sync_timeline_events(
        &self,
        room: &Option<room::Room>,
        timeline_events: &[SyncRoomEvent],
    ) -> serde_json::Result<()> {
        // FIXME: add EncryptionInfo to context
        #[derive(Deserialize)]
        struct TimelineEventDetails<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
            state_key: Option<serde::de::IgnoredAny>,
            unsigned: Option<UnsignedDetails>,
        }

        self.handle_sync_events_wrapped_with(
            room,
            timeline_events,
            |e| &e.event,
            |raw| {
                let TimelineEventDetails { event_type, state_key, unsigned } =
                    raw.deserialize_as()?;

                let redacted = unsigned.and_then(|u| u.redacted_because).is_some();
                let kind = match state_key {
                    Some(_) => EventKind::State { redacted },
                    None => EventKind::Message { redacted },
                };

                Ok((kind, event_type))
            },
        )
        .await
    }

    async fn handle_sync_events_wrapped<'a, T: 'a, U: 'a>(
        &self,
        kind: EventKind,
        room: &Option<room::Room>,
        events: &'a [U],
        get_event: impl Fn(&'a U) -> &'a Raw<T>,
    ) -> Result<(), serde_json::Error> {
        #[derive(Deserialize)]
        struct ExtractType<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
        }

        self.handle_sync_events_wrapped_with(room, events, get_event, |raw| {
            Ok((kind, raw.deserialize_as::<ExtractType>()?.event_type))
        })
        .await
    }

    async fn handle_sync_events_wrapped_with<'a, T: 'a, U: 'a>(
        &self,
        room: &Option<room::Room>,
        list: &'a [U],
        get_event: impl Fn(&'a U) -> &'a Raw<T>,
        get_id: impl Fn(&Raw<T>) -> serde_json::Result<(EventKind, Cow<'_, str>)>,
    ) -> serde_json::Result<()> {
        for x in list {
            let event = get_event(x);
            let (ev_kind, ev_type) = get_id(event)?;
            let event_handler_id = (ev_kind, &*ev_type);

            if let Some(handlers) = self.event_handlers.read().await.get(&event_handler_id) {
                for handler in &*handlers {
                    let data = EventHandlerData {
                        client: self.clone(),
                        room: room.clone(),
                        raw: event.json(),
                    };
                    (handler)(data).await;
                }
            }
        }

        Ok(())
    }
}

macro_rules! impl_event_handler {
    ($($ty:ident),* $(,)?) => {
        impl<Ev, Fun, Fut, $($ty),*> EventHandler<Ev, ($($ty,)*)> for Fun
        where
            Ev: SyncEvent,
            Fun: Fn(Ev, $($ty),*) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future + Send + 'static,
            Fut::Output: EventHandlerResult,
            $($ty: EventHandlerContext),*
        {
            type Future = Fut;
            const ID: (EventKind, &'static str) = Ev::ID;

            fn handle_event(&self, ev: Ev, _d: EventHandlerData<'_>) -> Option<Self::Future> {
                Some((self)(ev, $($ty::from_data(&_d)?),*))
            }
        }
    };
}

impl_event_handler!();
impl_event_handler!(A);
impl_event_handler!(A, B);
impl_event_handler!(A, B, C);
impl_event_handler!(A, B, C, D);
impl_event_handler!(A, B, C, D, E);
impl_event_handler!(A, B, C, D, E, F);
impl_event_handler!(A, B, C, D, E, F, G);
impl_event_handler!(A, B, C, D, E, F, G, H);

mod static_events {
    use ruma::events::{
        self,
        presence::{PresenceEvent, PresenceEventContent},
        StaticEventContent,
    };

    use super::{EventKind, SyncEvent};

    impl<C> SyncEvent for events::GlobalAccountDataEvent<C>
    where
        C: StaticEventContent + events::GlobalAccountDataEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::GlobalAccountData, C::TYPE);
    }

    impl<C> SyncEvent for events::RoomAccountDataEvent<C>
    where
        C: StaticEventContent + events::RoomAccountDataEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::RoomAccountData, C::TYPE);
    }

    impl<C> SyncEvent for events::SyncEphemeralRoomEvent<C>
    where
        C: StaticEventContent + events::EphemeralRoomEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::EphemeralRoomData, C::TYPE);
    }

    impl<C> SyncEvent for events::SyncMessageEvent<C>
    where
        C: StaticEventContent + events::MessageEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::Message { redacted: false }, C::TYPE);
    }

    impl<C> SyncEvent for events::SyncStateEvent<C>
    where
        C: StaticEventContent + events::StateEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::State { redacted: false }, C::TYPE);
    }

    impl<C> SyncEvent for events::StrippedStateEvent<C>
    where
        C: StaticEventContent + events::StateEventContent,
    {
        const ID: (EventKind, &'static str) =
            (EventKind::StrippedState { redacted: false }, C::TYPE);
    }

    impl<C> SyncEvent for events::InitialStateEvent<C>
    where
        C: StaticEventContent + events::StateEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::InitialState, C::TYPE);
    }

    impl<C> SyncEvent for events::ToDeviceEvent<C>
    where
        C: StaticEventContent + events::ToDeviceEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::ToDevice, C::TYPE);
    }

    impl SyncEvent for PresenceEvent {
        const ID: (EventKind, &'static str) = (EventKind::Presence, PresenceEventContent::TYPE);
    }

    impl<C> SyncEvent for events::RedactedSyncMessageEvent<C>
    where
        C: StaticEventContent + events::RedactedMessageEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::Message { redacted: true }, C::TYPE);
    }

    impl<C> SyncEvent for events::RedactedSyncStateEvent<C>
    where
        C: StaticEventContent + events::RedactedStateEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::State { redacted: true }, C::TYPE);
    }

    impl<C> SyncEvent for events::RedactedStrippedStateEvent<C>
    where
        C: StaticEventContent + events::RedactedStateEventContent,
    {
        const ID: (EventKind, &'static str) =
            (EventKind::StrippedState { redacted: true }, C::TYPE);
    }
}
