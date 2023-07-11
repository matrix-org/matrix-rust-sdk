// Copyright 2021 Jonas Platte
// Copyright 2022 Famedly GmbH
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
//! [`Client::add_event_handler`].
//!
//! ### How it works
//!
//! The `add_event_handler` method registers event handlers of different
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
use std::{
    borrow::Cow,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        RwLock,
    },
};

use anymap2::any::CloneAnySendSync;
use futures_util::stream::{FuturesUnordered, StreamExt};
use matrix_sdk_base::{
    deserialized_responses::{EncryptionInfo, SyncTimelineEvent},
    SendOutsideWasm, SyncOutsideWasm,
};
use ruma::{events::AnySyncStateEvent, push::Action, serde::Raw, OwnedRoomId};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::value::RawValue as RawJsonValue;
use tracing::{debug, error, field::debug, instrument, warn};

use self::maps::EventHandlerMaps;
use crate::{room, Client};

mod context;
mod maps;
mod static_events;

pub use self::context::{Ctx, EventHandlerContext, RawEvent};

#[cfg(not(target_arch = "wasm32"))]
type EventHandlerFut = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_arch = "wasm32")]
type EventHandlerFut = Pin<Box<dyn Future<Output = ()>>>;

#[cfg(not(target_arch = "wasm32"))]
type EventHandlerFn = dyn Fn(EventHandlerData<'_>) -> EventHandlerFut + Send + Sync;
#[cfg(target_arch = "wasm32")]
type EventHandlerFn = dyn Fn(EventHandlerData<'_>) -> EventHandlerFut;

type AnyMap = anymap2::Map<dyn CloneAnySendSync + Send + Sync>;

#[derive(Default)]
pub(crate) struct EventHandlerStore {
    handlers: RwLock<EventHandlerMaps>,
    context: RwLock<AnyMap>,
    counter: AtomicU64,
}

impl EventHandlerStore {
    pub fn add_handler(&self, handle: EventHandlerHandle, handler_fn: Box<EventHandlerFn>) {
        self.handlers.write().unwrap().add(handle, handler_fn);
    }

    pub fn add_context<T>(&self, ctx: T)
    where
        T: Clone + Send + Sync + 'static,
    {
        self.context.write().unwrap().insert(ctx);
    }

    pub fn remove(&self, handle: EventHandlerHandle) {
        self.handlers.write().unwrap().remove(handle);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.handlers.read().unwrap().len()
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HandlerKind {
    GlobalAccountData,
    RoomAccountData,
    EphemeralRoomData,
    Timeline,
    MessageLike,
    OriginalMessageLike,
    RedactedMessageLike,
    State,
    OriginalState,
    RedactedState,
    StrippedState,
    ToDevice,
    Presence,
}

impl HandlerKind {
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
    const KIND: HandlerKind;
    #[doc(hidden)]
    const TYPE: Option<&'static str>;
}

pub(crate) struct EventHandlerWrapper {
    handler_fn: Box<EventHandlerFn>,
    pub handler_id: u64,
}

/// Handle to remove a registered event handler by passing it to
/// [`Client::remove_event_handler`].
#[derive(Clone, Debug)]
pub struct EventHandlerHandle {
    pub(crate) ev_kind: HandlerKind,
    pub(crate) ev_type: Option<&'static str>,
    pub(crate) room_id: Option<OwnedRoomId>,
    pub(crate) handler_id: u64,
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
/// lists¹. Luckily, when calling [`Client::add_event_handler`] with a
/// closure argument the trait solver takes into account that only a single one
/// of the implementations applies (even though this could theoretically change
/// through a dependency upgrade) and uses that rather than raising an ambiguity
/// error. This is the same trick used by web frameworks like actix-web and
/// axum.
///
/// ¹ the only thing stopping such types from existing in stable Rust is that
/// all manual implementations of the `Fn` traits require a Nightly feature
pub trait EventHandler<Ev, Ctx>: SendOutsideWasm + SyncOutsideWasm + 'static {
    /// The future returned by `handle_event`.
    #[doc(hidden)]
    type Future: EventHandlerFuture;

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
pub trait EventHandlerFuture:
    Future<Output = <Self as EventHandlerFuture>::Output> + SendOutsideWasm + 'static
{
    type Output: EventHandlerResult;
}

impl<T> EventHandlerFuture for T
where
    T: Future + SendOutsideWasm + 'static,
    <T as Future>::Output: EventHandlerResult,
{
    type Output = <T as Future>::Output;
}

#[doc(hidden)]
#[derive(Debug)]
pub struct EventHandlerData<'a> {
    client: Client,
    room: Option<room::Room>,
    raw: &'a RawJsonValue,
    encryption_info: Option<&'a EncryptionInfo>,
    push_actions: &'a [Action],
    handle: EventHandlerHandle,
}

/// Return types supported for event handlers implement this trait.
///
/// It is not meant to be implemented outside of matrix-sdk.
pub trait EventHandlerResult: Sized {
    #[doc(hidden)]
    fn print_error(&self, event_type: Option<&str>);
}

impl EventHandlerResult for () {
    fn print_error(&self, _event_type: Option<&str>) {}
}

impl<E: fmt::Debug + fmt::Display + 'static> EventHandlerResult for Result<(), E> {
    fn print_error(&self, event_type: Option<&str>) {
        let msg_fragment = match event_type {
            Some(event_type) => format!(" for `{event_type}`"),
            None => "".to_owned(),
        };

        match self {
            #[cfg(feature = "anyhow")]
            Err(e) if TypeId::of::<E>() == TypeId::of::<anyhow::Error>() => {
                error!("Event handler{msg_fragment} failed: {e:?}");
            }
            #[cfg(feature = "eyre")]
            Err(e) if TypeId::of::<E>() == TypeId::of::<eyre::Report>() => {
                error!("Event handler{msg_fragment} failed: {e:?}");
            }
            Err(e) => {
                error!("Event handler{msg_fragment} failed: {e}");
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
    pub(crate) fn add_event_handler_impl<Ev, Ctx, H>(
        &self,
        handler: H,
        room_id: Option<OwnedRoomId>,
    ) -> EventHandlerHandle
    where
        Ev: SyncEvent + DeserializeOwned + Send + 'static,
        H: EventHandler<Ev, Ctx>,
    {
        let handler_fn: Box<EventHandlerFn> = Box::new(move |data| {
            let maybe_fut =
                serde_json::from_str(data.raw.get()).map(|ev| handler.handle_event(ev, data));

            Box::pin(async move {
                match maybe_fut {
                    Ok(Some(fut)) => {
                        fut.await.print_error(Ev::TYPE);
                    }
                    Ok(None) => {
                        error!(
                            event_type = Ev::TYPE, event_kind = ?Ev::KIND,
                            "Event handler has an invalid context argument",
                        );
                    }
                    Err(e) => {
                        warn!(
                            event_type = Ev::TYPE, event_kind = ?Ev::KIND,
                            "Failed to deserialize event, skipping event handler.\n
                             Deserialization error: {e}",
                        );
                    }
                }
            })
        });

        let handler_id = self.inner.event_handlers.counter.fetch_add(1, SeqCst);
        let handle =
            EventHandlerHandle { ev_kind: Ev::KIND, ev_type: Ev::TYPE, room_id, handler_id };

        self.inner.event_handlers.add_handler(handle.clone(), handler_fn);

        handle
    }

    pub(crate) async fn handle_sync_events<T>(
        &self,
        kind: HandlerKind,
        room: Option<&room::Room>,
        events: &[Raw<T>],
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct ExtractType<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
        }

        for raw_event in events {
            let event_type = raw_event.deserialize_as::<ExtractType<'_>>()?.event_type;
            self.call_event_handlers(room, raw_event.json(), kind, &event_type, None, &[]).await;
        }

        Ok(())
    }

    pub(crate) async fn handle_sync_state_events(
        &self,
        room: Option<&room::Room>,
        state_events: &[Raw<AnySyncStateEvent>],
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct StateEventDetails<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
            unsigned: Option<UnsignedDetails>,
        }

        // Event handlers for possibly-redacted state events
        self.handle_sync_events(HandlerKind::State, room, state_events).await?;

        // Event handlers specifically for redacted OR unredacted state events
        for raw_event in state_events {
            let StateEventDetails { event_type, unsigned } = raw_event.deserialize_as()?;
            let redacted = unsigned.and_then(|u| u.redacted_because).is_some();
            let handler_kind = HandlerKind::state_redacted(redacted);

            self.call_event_handlers(room, raw_event.json(), handler_kind, &event_type, None, &[])
                .await;
        }

        Ok(())
    }

    pub(crate) async fn handle_sync_timeline_events(
        &self,
        room: Option<&room::Room>,
        timeline_events: &[SyncTimelineEvent],
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct TimelineEventDetails<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
            state_key: Option<serde::de::IgnoredAny>,
            unsigned: Option<UnsignedDetails>,
        }

        for item in timeline_events {
            let TimelineEventDetails { event_type, state_key, unsigned } =
                item.event.deserialize_as()?;

            let redacted = unsigned.and_then(|u| u.redacted_because).is_some();
            let (handler_kind_g, handler_kind_r) = match state_key {
                Some(_) => (HandlerKind::State, HandlerKind::state_redacted(redacted)),
                None => (HandlerKind::MessageLike, HandlerKind::message_like_redacted(redacted)),
            };

            let raw_event = item.event.json();
            let encryption_info = item.encryption_info.as_ref();
            let push_actions = &item.push_actions;

            // Event handlers for possibly-redacted timeline events
            self.call_event_handlers(
                room,
                raw_event,
                handler_kind_g,
                &event_type,
                encryption_info,
                push_actions,
            )
            .await;

            // Event handlers specifically for redacted OR unredacted timeline events
            self.call_event_handlers(
                room,
                raw_event,
                handler_kind_r,
                &event_type,
                encryption_info,
                push_actions,
            )
            .await;

            // Event handlers for `AnySyncTimelineEvent`
            let kind = HandlerKind::Timeline;
            self.call_event_handlers(
                room,
                raw_event,
                kind,
                &event_type,
                encryption_info,
                push_actions,
            )
            .await;
        }

        Ok(())
    }

    #[instrument(skip_all, fields(?event_kind, ?event_type, room_id))]
    async fn call_event_handlers(
        &self,
        room: Option<&room::Room>,
        raw: &RawJsonValue,
        event_kind: HandlerKind,
        event_type: &str,
        encryption_info: Option<&EncryptionInfo>,
        push_actions: &[Action],
    ) {
        let room_id = room.map(|r| r.room_id());
        if let Some(room_id) = room_id {
            tracing::Span::current().record("room_id", debug(room_id));
        }

        // Construct event handler futures
        let mut futures: FuturesUnordered<_> = self
            .inner
            .event_handlers
            .handlers
            .read()
            .unwrap()
            .get_handlers(event_kind, event_type, room_id)
            .map(|(handle, handler_fn)| {
                let data = EventHandlerData {
                    client: self.clone(),
                    room: room.cloned(),
                    raw,
                    encryption_info,
                    push_actions,
                    handle,
                };

                (handler_fn)(data)
            })
            .collect();

        if !futures.is_empty() {
            debug!(amount = futures.len(), "Calling event handlers");

            // Run the event handler futures with the `self.event_handlers.handlers`
            // lock no longer being held.
            while let Some(()) = futures.next().await {}
        }
    }
}

/// A guard type that removes an event handler when it drops (goes out of
/// scope).
///
/// Created with [`Client::event_handler_drop_guard`].
#[derive(Debug)]
pub struct EventHandlerDropGuard {
    handle: EventHandlerHandle,
    client: Client,
}

impl EventHandlerDropGuard {
    pub(crate) fn new(handle: EventHandlerHandle, client: Client) -> Self {
        Self { handle, client }
    }
}

impl Drop for EventHandlerDropGuard {
    fn drop(&mut self) {
        self.client.remove_event_handler(self.handle.clone());
    }
}

macro_rules! impl_event_handler {
    ($($ty:ident),* $(,)?) => {
        impl<Ev, Fun, Fut, $($ty),*> EventHandler<Ev, ($($ty,)*)> for Fun
        where
            Ev: SyncEvent,
            Fun: Fn(Ev, $($ty),*) -> Fut + SendOutsideWasm + SyncOutsideWasm + 'static,
            Fut: EventHandlerFuture,
            $($ty: EventHandlerContext),*
        {
            type Future = Fut;

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

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{
        async_test, test_json::DEFAULT_SYNC_ROOM_ID, InvitedRoomBuilder, JoinedRoomBuilder,
    };
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
    use std::{
        future,
        sync::{
            atomic::{AtomicU8, Ordering::SeqCst},
            Arc,
        },
    };

    use matrix_sdk_test::{
        EphemeralTestEvent, StateTestEvent, StrippedStateTestEvent, SyncResponseBuilder,
        TimelineTestEvent,
    };
    use ruma::{
        events::{
            room::{
                member::{OriginalSyncRoomMemberEvent, StrippedRoomMemberEvent},
                name::OriginalSyncRoomNameEvent,
                power_levels::OriginalSyncRoomPowerLevelsEvent,
            },
            typing::SyncTypingEvent,
            AnySyncStateEvent,
        },
        room_id,
        serde::Raw,
    };
    use serde_json::json;

    use crate::{
        event_handler::Ctx,
        room::Room,
        test_utils::{logged_in_client, no_retry_test_client},
        Client,
    };

    #[async_test]
    async fn add_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let member_count = Arc::new(AtomicU8::new(0));
        let typing_count = Arc::new(AtomicU8::new(0));
        let power_levels_count = Arc::new(AtomicU8::new(0));
        let invited_member_count = Arc::new(AtomicU8::new(0));

        client.add_event_handler({
            let member_count = member_count.clone();
            move |_ev: OriginalSyncRoomMemberEvent, _room: Room| {
                member_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });
        client.add_event_handler({
            let typing_count = typing_count.clone();
            move |_ev: SyncTypingEvent| {
                typing_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });
        client.add_event_handler({
            let power_levels_count = power_levels_count.clone();
            move |_ev: OriginalSyncRoomPowerLevelsEvent, _client: Client, _room: Room| {
                power_levels_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });
        client.add_event_handler({
            let invited_member_count = invited_member_count.clone();
            move |_ev: StrippedRoomMemberEvent| {
                invited_member_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });

        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default()
                    .add_timeline_event(TimelineTestEvent::Member)
                    .add_ephemeral_event(EphemeralTestEvent::Typing)
                    .add_state_event(StateTestEvent::PowerLevels),
            )
            .add_invited_room(
                InvitedRoomBuilder::new(room_id!("!test_invited:example.org")).add_state_event(
                    StrippedStateTestEvent::Custom(json!({
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
                    })),
                ),
            )
            .build_sync_response();
        client.process_sync(response).await?;

        assert_eq!(member_count.load(SeqCst), 1);
        assert_eq!(typing_count.load(SeqCst), 1);
        assert_eq!(power_levels_count.load(SeqCst), 1);
        assert_eq!(invited_member_count.load(SeqCst), 1);

        Ok(())
    }

    #[async_test]
    async fn add_room_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let room_id_a = room_id!("!foo:example.org");
        let room_id_b = room_id!("!bar:matrix.org");

        let member_count = Arc::new(AtomicU8::new(0));
        let power_levels_count = Arc::new(AtomicU8::new(0));

        // Room event handlers for member events in both rooms
        client.add_room_event_handler(room_id_a, {
            let member_count = member_count.clone();
            move |_ev: OriginalSyncRoomMemberEvent, _room: Room| {
                member_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });
        client.add_room_event_handler(room_id_b, {
            let member_count = member_count.clone();
            move |_ev: OriginalSyncRoomMemberEvent, _room: Room| {
                member_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });

        // Power levels event handlers for member events in room A
        client.add_room_event_handler(room_id_a, {
            let power_levels_count = power_levels_count.clone();
            move |_ev: OriginalSyncRoomPowerLevelsEvent, _client: Client, _room: Room| {
                power_levels_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });

        // Room name event handler for room name events in room B
        client.add_room_event_handler(room_id_b, move |_ev: OriginalSyncRoomNameEvent| async {
            unreachable!("No room event in room B")
        });

        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::new(room_id_a)
                    .add_timeline_event(TimelineTestEvent::Member)
                    .add_state_event(StateTestEvent::PowerLevels)
                    .add_state_event(StateTestEvent::RoomName),
            )
            .add_joined_room(
                JoinedRoomBuilder::new(room_id_b)
                    .add_timeline_event(TimelineTestEvent::Member)
                    .add_state_event(StateTestEvent::PowerLevels),
            )
            .build_sync_response();
        client.process_sync(response).await?;

        assert_eq!(member_count.load(SeqCst), 2);
        assert_eq!(power_levels_count.load(SeqCst), 1);

        Ok(())
    }

    #[async_test]
    async fn remove_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let member_count = Arc::new(AtomicU8::new(0));

        client.add_event_handler({
            let member_count = member_count.clone();
            move |_ev: OriginalSyncRoomMemberEvent| {
                member_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });

        let handle_a = client.add_event_handler(move |_ev: OriginalSyncRoomMemberEvent| async {
            panic!("handler should have been removed");
        });
        let handle_b = client.add_room_event_handler(
            #[allow(unknown_lints, clippy::explicit_auto_deref)] // lint is buggy
            *DEFAULT_SYNC_ROOM_ID,
            move |_ev: OriginalSyncRoomMemberEvent| async {
                panic!("handler should have been removed");
            },
        );

        client.add_event_handler({
            let member_count = member_count.clone();
            move |_ev: OriginalSyncRoomMemberEvent| {
                member_count.fetch_add(1, SeqCst);
                future::ready(())
            }
        });

        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_timeline_event(TimelineTestEvent::Member),
            )
            .build_sync_response();

        client.remove_event_handler(handle_a);
        client.remove_event_handler(handle_b);

        client.process_sync(response).await?;

        assert_eq!(member_count.load(SeqCst), 2);

        Ok(())
    }

    #[async_test]
    async fn event_handler_drop_guard() {
        let client = no_retry_test_client(None).await;

        let handle = client.add_event_handler(|_ev: OriginalSyncRoomMemberEvent| async {});
        assert_eq!(client.inner.event_handlers.len(), 1);

        {
            let _guard = client.event_handler_drop_guard(handle);
            assert_eq!(client.inner.event_handlers.len(), 1);
            // guard dropped here
        }

        assert_eq!(client.inner.event_handlers.len(), 0);
    }

    #[async_test]
    async fn use_client_in_handler() {
        // This used to not work because we were requiring `Send` of event
        // handler futures even on WASM, where practically all futures that do
        // I/O aren't.
        let client = no_retry_test_client(None).await;

        client.add_event_handler(|_ev: OriginalSyncRoomMemberEvent, client: Client| async move {
            // All of Client's async methods that do network requests (and
            // possibly some that don't) are `!Send` on wasm. We obviously want
            // to be able to use them in event handlers.
            let _caps = client.get_capabilities().await?;
            anyhow::Ok(())
        });
    }

    #[async_test]
    async fn raw_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;
        let counter = Arc::new(AtomicU8::new(0));
        client.add_event_handler_context(counter.clone());
        client.add_event_handler(
            |_ev: Raw<OriginalSyncRoomMemberEvent>, counter: Ctx<Arc<AtomicU8>>| async move {
                counter.fetch_add(1, SeqCst);
            },
        );

        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_timeline_event(TimelineTestEvent::Member),
            )
            .build_sync_response();
        client.process_sync(response).await?;

        assert_eq!(counter.load(SeqCst), 1);
        Ok(())
    }

    #[async_test]
    async fn enum_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;
        let counter = Arc::new(AtomicU8::new(0));
        client.add_event_handler_context(counter.clone());
        client.add_event_handler(
            |_ev: AnySyncStateEvent, counter: Ctx<Arc<AtomicU8>>| async move {
                counter.fetch_add(1, SeqCst);
            },
        );

        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_timeline_event(TimelineTestEvent::Member),
            )
            .build_sync_response();
        client.process_sync(response).await?;

        assert_eq!(counter.load(SeqCst), 1);
        Ok(())
    }
}
