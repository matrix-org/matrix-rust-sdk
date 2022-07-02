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
//!
//! ### How it works
//!
//! The `register_event_handler` method registers event handlers of different
//! signatures by actually storing boxed closures that all have the same
//! signature of `async (EventHandlerData) -> ()` where `EventHandlerData` is a
//! private type that contains all of the data an event handler *might* need.
//!
//! The stored closure takes care of deserializing the event which the
//! `EventHandlerData` contains as a (borrowed) [`serde_json::value::RawValue`],
//! extracting the context arguments from other fields of `EventHandlerData` and
//! calling / `.await`ing the event handler if the previous steps succeeded.
//! It also logs any errors from the above chain of function calls.
//!
//! For more details, see the [`EventHandler`] trait.

#[cfg(any(feature = "anyhow", feature = "eyre"))]
use std::any::TypeId;
use std::{borrow::Cow, fmt, future::Future, ops::Deref};

use matrix_sdk_base::deserialized_responses::{EncryptionInfo, SyncRoomEvent};
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
    MessageLike,
    OriginalMessageLike,
    RedactedMessageLike,
    State,
    OriginalState,
    RedactedState,
    StrippedState,
    InitialState,
    ToDevice,
    Presence,
}

impl EventKind {
    fn message_like_redacted(redacted: bool) -> Self {
        if redacted {
            Self::RedactedMessageLike
        } else {
            Self::OriginalMessageLike
        }
    }

    fn state_redacted(redacted: bool) -> Self {
        if redacted {
            Self::RedactedState
        } else {
            Self::OriginalState
        }
    }
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
/// * Their return type has to be one of: `()`, `Result<(), impl Display + Debug
///   + 'static>` (if you are using `anyhow::Result` or `eyre::Result` you can
///   additionally enable the `anyhow` / `eyre` feature to get the verbose
///   `Debug` output printed on error)
///
/// ### How it works
///
/// This trait is basically a very constrained version of `Fn`: It requires at
/// least one argument, which is represented as its own generic parameter `Ev`
/// with the remaining parameter types being represented by the second generic
/// parameter `Ctx`; they have to be stuffed into one generic parameter as a
/// tuple because Rust doesn't have variadic generics.
///
/// `Ev` and `Ctx` are generic parameters rather than associated types because
/// the argument list is a generic parameter for the `Fn` traits too, so a
/// single type could implement `Fn` multiple times with different argument
/// lists¹. Luckily, when calling [`Client::register_event_handler`] with a
/// closure argument the trait solver takes into account that only a single one
/// of the implementations applies (even though this could theoretically change
/// through a dependency upgrade) and uses that rather than raising an ambiguity
/// error. This is the same trick used by web frameworks like actix-web and
/// axum.
///
/// ¹ the only thing stopping such types from existing in stable Rust is that
/// all manual implementations of the `Fn` traits require a Nightly feature
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
    pub encryption_info: Option<&'a EncryptionInfo>,
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

/// This event handler context argument is only applicable to room-specific
/// events.
///
/// Trying to use it in the event handler for another event, for example a
/// global account data or presence event, will result in the event handler
/// being skipped and an error getting logged.
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

impl EventHandlerContext for Option<EncryptionInfo> {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        Some(data.encryption_info.cloned())
    }
}

/// A custom value registered with
/// [`.register_event_handler_context`][Client::register_event_handler_context].
#[derive(Debug)]
pub struct Ctx<T>(pub T);

impl<T: Clone + Send + Sync + 'static> EventHandlerContext for Ctx<T> {
    fn from_data(data: &EventHandlerData<'_>) -> Option<Self> {
        data.client.event_handler_context::<T>().map(Ctx)
    }
}

impl<T> Deref for Ctx<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
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

impl<E: fmt::Debug + fmt::Display + 'static> EventHandlerResult for Result<(), E> {
    fn print_error(&self, event_type: &str) {
        match self {
            #[cfg(feature = "anyhow")]
            Err(e) if TypeId::of::<E>() == TypeId::of::<anyhow::Error>() => {
                tracing::error!("Event handler for `{}` failed: {:?}", event_type, e);
            }
            #[cfg(feature = "eyre")]
            Err(e) if TypeId::of::<E>() == TypeId::of::<eyre::Report>() => {
                tracing::error!("Event handler for `{}` failed: {:?}", event_type, e);
            }
            Err(e) => {
                tracing::error!("Event handler for `{}` failed: {}", event_type, e);
            }
            Ok(_) => {}
        }
    }
}

#[derive(Deserialize)]
struct UnsignedDetails {
    redacted_because: Option<serde::de::IgnoredAny>,
}

/// Event handling internals.
impl Client {
    pub(crate) async fn handle_sync_events<T>(
        &self,
        kind: EventKind,
        room: &Option<room::Room>,
        events: &[Raw<T>],
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct ExtractType<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
        }

        self.handle_sync_events_wrapped_with(
            room,
            events,
            |ev| (ev, None),
            |raw| Ok((kind, raw.deserialize_as::<ExtractType<'_>>()?.event_type)),
        )
        .await
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

        // Event handlers for possibly-redacted state events
        self.handle_sync_events(EventKind::State, room, state_events).await?;

        // Event handlers specifically for redacted OR unredacted state events
        self.handle_sync_events_wrapped_with(
            room,
            state_events,
            |ev| (ev, None),
            |raw| {
                let StateEventDetails { event_type, unsigned } = raw.deserialize_as()?;
                let redacted = unsigned.and_then(|u| u.redacted_because).is_some();
                Ok((EventKind::state_redacted(redacted), event_type))
            },
        )
        .await?;

        Ok(())
    }

    pub(crate) async fn handle_sync_timeline_events(
        &self,
        room: &Option<room::Room>,
        timeline_events: &[SyncRoomEvent],
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct TimelineEventDetails<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
            state_key: Option<serde::de::IgnoredAny>,
            unsigned: Option<UnsignedDetails>,
        }

        // Event handlers for possibly-redacted timeline events
        self.handle_sync_events_wrapped_with(
            room,
            timeline_events,
            |e| (&e.event, e.encryption_info.as_ref()),
            |raw| {
                let TimelineEventDetails { event_type, state_key, .. } = raw.deserialize_as()?;

                let kind = match state_key {
                    Some(_) => EventKind::State,
                    None => EventKind::MessageLike,
                };

                Ok((kind, event_type))
            },
        )
        .await?;

        // Event handlers specifically for redacted OR unredacted timeline events
        self.handle_sync_events_wrapped_with(
            room,
            timeline_events,
            |e| (&e.event, e.encryption_info.as_ref()),
            |raw| {
                let TimelineEventDetails { event_type, state_key, unsigned } =
                    raw.deserialize_as()?;

                let redacted = unsigned.and_then(|u| u.redacted_because).is_some();
                let kind = match state_key {
                    Some(_) => EventKind::state_redacted(redacted),
                    None => EventKind::message_like_redacted(redacted),
                };

                Ok((kind, event_type))
            },
        )
        .await?;

        Ok(())
    }

    async fn handle_sync_events_wrapped_with<'a, T: 'a, U: 'a>(
        &self,
        room: &Option<room::Room>,
        list: &'a [U],
        get_event_details: impl Fn(&'a U) -> (&'a Raw<T>, Option<&'a EncryptionInfo>),
        get_id: impl Fn(&Raw<T>) -> serde_json::Result<(EventKind, Cow<'_, str>)>,
    ) -> serde_json::Result<()> {
        for x in list {
            let (raw_event, encryption_info) = get_event_details(x);
            let (ev_kind, ev_type) = get_id(raw_event)?;
            let event_handler_id = (ev_kind, &*ev_type);

            // Construct event handler futures
            let futures: Vec<_> = self
                .event_handlers()
                .await
                .get(&event_handler_id)
                .into_iter()
                .flatten()
                .map(|handler| {
                    let data = EventHandlerData {
                        client: self.clone(),
                        room: room.clone(),
                        raw: raw_event.json(),
                        encryption_info,
                    };
                    (handler)(data)
                })
                .collect();

            // Run the event handler futures with the `self.event_handlers` lock
            // no longer being held, in order.
            for fut in futures {
                fut.await;
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
        EphemeralRoomEventContent, GlobalAccountDataEventContent, MessageLikeEventContent,
        RedactContent, RedactedEventContent, RoomAccountDataEventContent, StateEventContent,
        StaticEventContent, ToDeviceEventContent,
    };

    use super::{EventKind, SyncEvent};

    impl<C> SyncEvent for events::GlobalAccountDataEvent<C>
    where
        C: StaticEventContent + GlobalAccountDataEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::GlobalAccountData, C::TYPE);
    }

    impl<C> SyncEvent for events::RoomAccountDataEvent<C>
    where
        C: StaticEventContent + RoomAccountDataEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::RoomAccountData, C::TYPE);
    }

    impl<C> SyncEvent for events::SyncEphemeralRoomEvent<C>
    where
        C: StaticEventContent + EphemeralRoomEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::EphemeralRoomData, C::TYPE);
    }

    impl<C> SyncEvent for events::SyncMessageLikeEvent<C>
    where
        C: StaticEventContent + MessageLikeEventContent + RedactContent,
        C::Redacted: MessageLikeEventContent + RedactedEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::MessageLike, C::TYPE);
    }

    impl<C> SyncEvent for events::OriginalSyncMessageLikeEvent<C>
    where
        C: StaticEventContent + MessageLikeEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::OriginalMessageLike, C::TYPE);
    }

    impl<C> SyncEvent for events::RedactedSyncMessageLikeEvent<C>
    where
        C: StaticEventContent + MessageLikeEventContent + RedactedEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::RedactedMessageLike, C::TYPE);
    }

    impl SyncEvent for events::room::redaction::SyncRoomRedactionEvent {
        const ID: (EventKind, &'static str) =
            (EventKind::MessageLike, events::room::redaction::RoomRedactionEventContent::TYPE);
    }

    impl SyncEvent for events::room::redaction::OriginalSyncRoomRedactionEvent {
        const ID: (EventKind, &'static str) = (
            EventKind::OriginalMessageLike,
            events::room::redaction::RoomRedactionEventContent::TYPE,
        );
    }

    impl SyncEvent for events::room::redaction::RedactedSyncRoomRedactionEvent {
        const ID: (EventKind, &'static str) = (
            EventKind::RedactedMessageLike,
            events::room::redaction::RoomRedactionEventContent::TYPE,
        );
    }

    impl<C> SyncEvent for events::SyncStateEvent<C>
    where
        C: StaticEventContent + StateEventContent + RedactContent,
        C::Redacted: StateEventContent + RedactedEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::State, C::TYPE);
    }

    impl<C> SyncEvent for events::OriginalSyncStateEvent<C>
    where
        C: StaticEventContent + StateEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::OriginalState, C::TYPE);
    }

    impl<C> SyncEvent for events::RedactedSyncStateEvent<C>
    where
        C: StaticEventContent + StateEventContent + RedactedEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::RedactedState, C::TYPE);
    }

    impl<C> SyncEvent for events::StrippedStateEvent<C>
    where
        C: StaticEventContent + StateEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::StrippedState, C::TYPE);
    }

    impl<C> SyncEvent for events::InitialStateEvent<C>
    where
        C: StaticEventContent + StateEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::InitialState, C::TYPE);
    }

    impl<C> SyncEvent for events::ToDeviceEvent<C>
    where
        C: StaticEventContent + ToDeviceEventContent,
    {
        const ID: (EventKind, &'static str) = (EventKind::ToDevice, C::TYPE);
    }

    impl SyncEvent for PresenceEvent {
        const ID: (EventKind, &'static str) = (EventKind::Presence, PresenceEventContent::TYPE);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use matrix_sdk_test::async_test;
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
    use std::{future, sync::Arc};

    use matrix_sdk_test::{EventBuilder, EventsJson};
    use ruma::{
        events::room::member::{OriginalSyncRoomMemberEvent, StrippedRoomMemberEvent},
        room_id,
    };
    use serde_json::json;

    use crate::{room, Client};

    #[async_test]
    async fn event_handler() -> crate::Result<()> {
        use std::sync::atomic::{AtomicU8, Ordering::SeqCst};

        let client = crate::client::tests::logged_in_client(None).await;

        let member_count = Arc::new(AtomicU8::new(0));
        let typing_count = Arc::new(AtomicU8::new(0));
        let power_levels_count = Arc::new(AtomicU8::new(0));
        let invited_member_count = Arc::new(AtomicU8::new(0));

        client
            .register_event_handler({
                let member_count = member_count.clone();
                move |_ev: OriginalSyncRoomMemberEvent, _room: room::Room| {
                    member_count.fetch_add(1, SeqCst);
                    future::ready(())
                }
            })
            .await
            .register_event_handler({
                let typing_count = typing_count.clone();
                move |_ev: OriginalSyncRoomMemberEvent| {
                    typing_count.fetch_add(1, SeqCst);
                    future::ready(())
                }
            })
            .await
            .register_event_handler({
                let power_levels_count = power_levels_count.clone();
                move |_ev: OriginalSyncRoomMemberEvent, _client: Client, _room: room::Room| {
                    power_levels_count.fetch_add(1, SeqCst);
                    future::ready(())
                }
            })
            .await
            .register_event_handler({
                let invited_member_count = invited_member_count.clone();
                move |_ev: StrippedRoomMemberEvent| {
                    invited_member_count.fetch_add(1, SeqCst);
                    future::ready(())
                }
            })
            .await;

        let response = EventBuilder::default()
            .add_room_event(EventsJson::Member)
            .add_ephemeral(EventsJson::Typing)
            .add_state_event(EventsJson::PowerLevels)
            .add_custom_invited_event(
                room_id!("!test_invited:example.org"),
                json!({
                    "content": {
                        "avatar_url": "mxc://example.org/SEsfnsuifSDFSSEF",
                        "displayname": "Alice",
                        "membership": "invite",
                    },
                    "event_id": "$143273582443PhrSn:example.org",
                    "origin_server_ts": 1432735824653u64,
                    "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
                    "sender": "@example:example.org",
                    "state_key": "@alice:example.org",
                    "type": "m.room.member",
                    "unsigned": {
                        "age": 1234,
                        "invite_room_state": [
                            {
                                "content": {
                                    "name": "Example Room"
                                },
                                "sender": "@bob:example.org",
                                "state_key": "",
                                "type": "m.room.name"
                            },
                            {
                                "content": {
                                    "join_rule": "invite"
                                },
                                "sender": "@bob:example.org",
                                "state_key": "",
                                "type": "m.room.join_rules"
                            }
                        ]
                    }
                }),
            )
            .build_sync_response();
        client.process_sync(response).await?;

        assert_eq!(member_count.load(SeqCst), 1);
        assert_eq!(typing_count.load(SeqCst), 1);
        assert_eq!(power_levels_count.load(SeqCst), 1);
        assert_eq!(invited_member_count.load(SeqCst), 1);

        Ok(())
    }
}
