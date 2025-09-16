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
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering::SeqCst},
    },
    task::{Context, Poll},
};

#[cfg(target_family = "wasm")]
use anymap2::any::CloneAny;
#[cfg(not(target_family = "wasm"))]
use anymap2::any::CloneAnySendSync;
use eyeball::{SharedObservable, Subscriber};
use futures_core::Stream;
use futures_util::stream::{FuturesUnordered, StreamExt};
use matrix_sdk_base::{
    SendOutsideWasm, SyncOutsideWasm,
    deserialized_responses::{EncryptionInfo, TimelineEvent},
    sync::State,
};
use matrix_sdk_common::deserialized_responses::ProcessedToDeviceEvent;
use pin_project_lite::pin_project;
use ruma::{OwnedRoomId, events::BooleanType, push::Action, serde::Raw};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::value::RawValue as RawJsonValue;
use tracing::{debug, error, field::debug, instrument, warn};

use self::maps::EventHandlerMaps;
use crate::{Client, Room};

mod context;
mod maps;
mod static_events;

pub use self::context::{Ctx, EventHandlerContext, RawEvent};

#[cfg(not(target_family = "wasm"))]
type EventHandlerFut = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_family = "wasm")]
type EventHandlerFut = Pin<Box<dyn Future<Output = ()>>>;

#[cfg(not(target_family = "wasm"))]
type EventHandlerFn = dyn Fn(EventHandlerData<'_>) -> EventHandlerFut + Send + Sync;
#[cfg(target_family = "wasm")]
type EventHandlerFn = dyn Fn(EventHandlerData<'_>) -> EventHandlerFut;

#[cfg(not(target_family = "wasm"))]
type AnyMap = anymap2::Map<dyn CloneAnySendSync + Send + Sync>;
#[cfg(target_family = "wasm")]
type AnyMap = anymap2::Map<dyn CloneAny>;

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
        if redacted { Self::RedactedMessageLike } else { Self::OriginalMessageLike }
    }

    fn state_redacted(redacted: bool) -> Self {
        if redacted { Self::RedactedState } else { Self::OriginalState }
    }
}

/// A statically-known event kind/type that can be retrieved from an event sync.
pub trait SyncEvent {
    #[doc(hidden)]
    const KIND: HandlerKind;
    #[doc(hidden)]
    const TYPE: Option<&'static str>;
    #[doc(hidden)]
    type IsPrefix: BooleanType;
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
    pub(crate) ev_type: Option<StaticEventTypePart>,
    pub(crate) room_id: Option<OwnedRoomId>,
    pub(crate) handler_id: u64,
}

/// The static part of an event type.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StaticEventTypePart {
    /// The full event type is static.
    Full(&'static str),
    /// Only the prefix of the event type is static.
    Prefix(&'static str),
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
pub trait EventHandler<Ev, Ctx>: Clone + SendOutsideWasm + SyncOutsideWasm + 'static {
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
    fn handle_event(self, ev: Ev, data: EventHandlerData<'_>) -> Option<Self::Future>;
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
    room: Option<Room>,
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
        Ev: SyncEvent + DeserializeOwned + SendOutsideWasm + 'static,
        H: EventHandler<Ev, Ctx>,
    {
        let handler_fn: Box<EventHandlerFn> = Box::new(move |data| {
            let maybe_fut = serde_json::from_str(data.raw.get())
                .map(|ev| handler.clone().handle_event(ev, data));

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
        let ev_type = Ev::TYPE.map(|ev_type| {
            if Ev::IsPrefix::as_bool() {
                StaticEventTypePart::Prefix(ev_type)
            } else {
                StaticEventTypePart::Full(ev_type)
            }
        });
        let handle = EventHandlerHandle { ev_kind: Ev::KIND, ev_type, room_id, handler_id };

        self.inner.event_handlers.add_handler(handle.clone(), handler_fn);

        handle
    }

    pub(crate) async fn handle_sync_events<T>(
        &self,
        kind: HandlerKind,
        room: Option<&Room>,
        events: &[Raw<T>],
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct ExtractType<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
        }

        for raw_event in events {
            let event_type = raw_event.deserialize_as_unchecked::<ExtractType<'_>>()?.event_type;
            self.call_event_handlers(room, raw_event.json(), kind, &event_type, None, &[]).await;
        }

        Ok(())
    }

    pub(crate) async fn handle_sync_to_device_events(
        &self,
        events: &[ProcessedToDeviceEvent],
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct ExtractType<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
        }

        for processed_to_device in events {
            let (raw_event, encryption_info) = match processed_to_device {
                ProcessedToDeviceEvent::Decrypted { raw, encryption_info } => {
                    (raw, Some(encryption_info))
                }
                other => (&other.to_raw(), None),
            };
            let event_type = raw_event.deserialize_as_unchecked::<ExtractType<'_>>()?.event_type;
            self.call_event_handlers(
                None,
                raw_event.json(),
                HandlerKind::ToDevice,
                &event_type,
                encryption_info,
                &[],
            )
            .await;
        }

        Ok(())
    }

    pub(crate) async fn handle_sync_state_events(
        &self,
        room: Option<&Room>,
        state: &State,
    ) -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct StateEventDetails<'a> {
            #[serde(borrow, rename = "type")]
            event_type: Cow<'a, str>,
            unsigned: Option<UnsignedDetails>,
        }

        let state_events = match state {
            State::Before(events) => events,
            State::After(events) => events,
        };

        // Event handlers for possibly-redacted state events
        self.handle_sync_events(HandlerKind::State, room, state_events).await?;

        // Event handlers specifically for redacted OR unredacted state events
        for raw_event in state_events {
            let StateEventDetails { event_type, unsigned } =
                raw_event.deserialize_as_unchecked()?;
            let redacted = unsigned.and_then(|u| u.redacted_because).is_some();
            let handler_kind = HandlerKind::state_redacted(redacted);

            self.call_event_handlers(room, raw_event.json(), handler_kind, &event_type, None, &[])
                .await;
        }

        Ok(())
    }

    pub(crate) async fn handle_sync_timeline_events(
        &self,
        room: Option<&Room>,
        timeline_events: &[TimelineEvent],
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
                item.raw().deserialize_as_unchecked()?;

            let redacted = unsigned.and_then(|u| u.redacted_because).is_some();
            let (handler_kind_g, handler_kind_r) = match state_key {
                Some(_) => (HandlerKind::State, HandlerKind::state_redacted(redacted)),
                None => (HandlerKind::MessageLike, HandlerKind::message_like_redacted(redacted)),
            };

            let raw_event = item.raw().json();
            let encryption_info = item.encryption_info().map(|i| &**i);
            let push_actions = item.push_actions().unwrap_or(&[]);

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
        room: Option<&Room>,
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
            Fun: FnOnce(Ev, $($ty),*) -> Fut + Clone + SendOutsideWasm + SyncOutsideWasm + 'static,
            Fut: EventHandlerFuture,
            $($ty: EventHandlerContext),*
        {
            type Future = Fut;

            fn handle_event(self, ev: Ev, _d: EventHandlerData<'_>) -> Option<Self::Future> {
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

/// An observer of events (may be tailored to a room).
///
/// Only the most recent value can be observed. Subscribers are notified when a
/// new value is sent, but there is no guarantee that they will see all values.
///
/// To create such observer, use [`Client::observe_events`] or
/// [`Client::observe_room_events`].
#[derive(Debug)]
pub struct ObservableEventHandler<T> {
    /// This type is actually nothing more than a thin glue layer between the
    /// [`EventHandler`] mechanism and the reactive programming types from
    /// [`eyeball`]. Here, we use a [`SharedObservable`] that is updated by the
    /// [`EventHandler`].
    shared_observable: SharedObservable<Option<T>>,

    /// This type owns the [`EventHandlerDropGuard`]. As soon as this type goes
    /// out of scope, the event handler is unregistered/removed.
    ///
    /// [`EventHandlerSubscriber`] holds a weak, non-owning reference, to this
    /// guard. It is useful to detect when to close the [`Stream`]: as soon as
    /// this type goes out of scope, the subscriber will close itself on poll.
    event_handler_guard: Arc<EventHandlerDropGuard>,
}

impl<T> ObservableEventHandler<T> {
    pub(crate) fn new(
        shared_observable: SharedObservable<Option<T>>,
        event_handler_guard: EventHandlerDropGuard,
    ) -> Self {
        Self { shared_observable, event_handler_guard: Arc::new(event_handler_guard) }
    }

    /// Subscribe to this observer.
    ///
    /// It returns an [`EventHandlerSubscriber`], which implements [`Stream`].
    /// See its documentation to learn more.
    pub fn subscribe(&self) -> EventHandlerSubscriber<T> {
        EventHandlerSubscriber::new(
            self.shared_observable.subscribe(),
            // The subscriber holds a weak non-owning reference to the event handler guard, so that
            // it can detect when this observer is dropped, and can close the subscriber's stream.
            Arc::downgrade(&self.event_handler_guard),
        )
    }
}

pin_project! {
    /// The subscriber of an [`ObservableEventHandler`].
    ///
    /// To create such subscriber, use [`ObservableEventHandler::subscribe`].
    ///
    /// This type implements [`Stream`], which means it is possible to poll the
    /// next value asynchronously. In other terms, polling this type will return
    /// the new event as soon as they are synced. See [`Client::observe_events`]
    /// to learn more.
    #[derive(Debug)]
    pub struct EventHandlerSubscriber<T> {
        // The `Subscriber` associated to the `SharedObservable` inside
        // `ObservableEventHandle`.
        //
        // Keep in mind all this API is just a thin glue layer between
        // `EventHandle` and `SharedObservable`, that's… maagiic!
        #[pin]
        subscriber: Subscriber<Option<T>>,

        // A weak non-owning reference to the event handler guard from
        // `ObservableEventHandler`. When this type is polled (via its `Stream`
        // implementation), it is possible to detect whether the observable has
        // been dropped by upgrading this weak reference, and close the `Stream`
        // if it needs to.
        event_handler_guard: Weak<EventHandlerDropGuard>,
    }
}

impl<T> EventHandlerSubscriber<T> {
    fn new(
        subscriber: Subscriber<Option<T>>,
        event_handler_handle: Weak<EventHandlerDropGuard>,
    ) -> Self {
        Self { subscriber, event_handler_guard: event_handler_handle }
    }
}

impl<T> Stream for EventHandlerSubscriber<T>
where
    T: Clone,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        let Some(_) = this.event_handler_guard.upgrade() else {
            // The `EventHandlerHandle` has been dropped via `EventHandlerDropGuard`. It
            // means the `ObservableEventHandler` has been dropped. It's time to
            // close this stream.
            return Poll::Ready(None);
        };

        // First off, the subscriber is of type `Subscriber<Option<T>>` because the
        // `SharedObservable` starts with a `None` value to indicate it has no yet
        // received any update. We want the `Stream` to return `T`, not `Option<T>`. We
        // then filter out all `None` value.
        //
        // Second, when a `None` value is met, we want to poll again (hence the `loop`).
        // At best, there is a new value to return. At worst, the subscriber will return
        // `Poll::Pending` and will register the wakers accordingly.

        loop {
            match this.subscriber.as_mut().poll_next(context) {
                // Stream has been closed somehow.
                Poll::Ready(None) => return Poll::Ready(None),

                // The initial value (of the `SharedObservable` behind `self.subscriber`) has been
                // polled. We want to filter it out.
                Poll::Ready(Some(None)) => {
                    // Loop over.
                    continue;
                }

                // We have a new value!
                Poll::Ready(Some(Some(value))) => return Poll::Ready(Some(value)),

                // Classical pending.
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{
        DEFAULT_TEST_ROOM_ID, InvitedRoomBuilder, JoinedRoomBuilder, async_test,
        event_factory::{EventFactory, PreviousMembership},
    };
    use serde::Serialize;
    use stream_assert::{assert_closed, assert_pending, assert_ready};
    #[cfg(target_family = "wasm")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
    use std::{
        future,
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering::SeqCst},
        },
    };

    use assert_matches2::assert_let;
    use matrix_sdk_common::{deserialized_responses::EncryptionInfo, locks::Mutex};
    use matrix_sdk_test::{StateTestEvent, StrippedStateTestEvent, SyncResponseBuilder};
    use once_cell::sync::Lazy;
    use ruma::{
        event_id,
        events::{
            AnySyncStateEvent, AnySyncTimelineEvent, AnyToDeviceEvent,
            macros::EventContent,
            room::{
                member::{MembershipState, OriginalSyncRoomMemberEvent, StrippedRoomMemberEvent},
                name::OriginalSyncRoomNameEvent,
                power_levels::OriginalSyncRoomPowerLevelsEvent,
            },
            secret_storage::key::SecretStorageKeyEvent,
            typing::SyncTypingEvent,
        },
        room_id,
        serde::Raw,
        user_id,
    };
    use serde_json::json;

    use crate::{
        Client, Room,
        event_handler::Ctx,
        test_utils::{logged_in_client, no_retry_test_client},
    };

    static MEMBER_EVENT: Lazy<Raw<AnySyncTimelineEvent>> = Lazy::new(|| {
        EventFactory::new()
            .member(user_id!("@example:localhost"))
            .membership(MembershipState::Join)
            .display_name("example")
            .event_id(event_id!("$151800140517rfvjc:localhost"))
            .previous(PreviousMembership::new(MembershipState::Invite).display_name("example"))
            .into()
    });

    #[async_test]
    async fn test_add_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let member_count = Arc::new(AtomicU8::new(0));
        let typing_count = Arc::new(AtomicU8::new(0));
        let power_levels_count = Arc::new(AtomicU8::new(0));
        let invited_member_count = Arc::new(AtomicU8::new(0));

        client.add_event_handler({
            let member_count = member_count.clone();
            move |_ev: OriginalSyncRoomMemberEvent, _room: Room| async move {
                member_count.fetch_add(1, SeqCst);
            }
        });
        client.add_event_handler({
            let typing_count = typing_count.clone();
            move |_ev: SyncTypingEvent| async move {
                typing_count.fetch_add(1, SeqCst);
            }
        });
        client.add_event_handler({
            let power_levels_count = power_levels_count.clone();
            move |_ev: OriginalSyncRoomPowerLevelsEvent, _client: Client, _room: Room| async move {
                power_levels_count.fetch_add(1, SeqCst);
            }
        });
        client.add_event_handler({
            let invited_member_count = invited_member_count.clone();
            move |_ev: StrippedRoomMemberEvent| async move {
                invited_member_count.fetch_add(1, SeqCst);
            }
        });

        let f = EventFactory::new();
        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default()
                    .add_timeline_event(MEMBER_EVENT.clone())
                    .add_typing(
                        f.typing(vec![user_id!("@alice:matrix.org"), user_id!("@bob:example.com")]),
                    )
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
    async fn test_add_to_device_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let captured_event: Arc<Mutex<Option<AnyToDeviceEvent>>> = Arc::new(Mutex::new(None));
        let captured_info: Arc<Mutex<Option<EncryptionInfo>>> = Arc::new(Mutex::new(None));

        client.add_event_handler({
            let captured = captured_event.clone();
            let captured_info = captured_info.clone();
            move |ev: AnyToDeviceEvent, encryption_info: Option<EncryptionInfo>| {
                let mut captured_lock = captured.lock();
                *captured_lock = Some(ev);
                let mut captured_info_lock = captured_info.lock();
                *captured_info_lock = encryption_info;
                future::ready(())
            }
        });

        let response = SyncResponseBuilder::default()
            .add_to_device_event(json!({
              "sender": "@alice:example.com",
              "type": "m.custom.to.device.type",
              "content": {
                "a": "test",
              }
            }))
            .build_sync_response();
        client.process_sync(response).await?;

        let captured = captured_event.lock().clone();
        assert_let!(Some(received_event) = captured);
        assert_eq!(received_event.event_type().to_string(), "m.custom.to.device.type");
        let info = captured_info.lock().clone();
        assert!(info.is_none());
        Ok(())
    }

    #[async_test]
    async fn test_add_room_event_handler() -> crate::Result<()> {
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
        client.add_room_event_handler(
            room_id_b,
            // lint is buggy: rustc wants the explicit conversion from ! to () here, but clippy
            // thinks it's useless.
            #[allow(clippy::unused_unit)]
            async move |_ev: OriginalSyncRoomNameEvent| -> () {
                unreachable!("No room event in room B")
            },
        );

        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::new(room_id_a)
                    .add_timeline_event(MEMBER_EVENT.clone())
                    .add_state_event(StateTestEvent::PowerLevels)
                    .add_state_event(StateTestEvent::RoomName),
            )
            .add_joined_room(
                JoinedRoomBuilder::new(room_id_b)
                    .add_timeline_event(MEMBER_EVENT.clone())
                    .add_state_event(StateTestEvent::PowerLevels),
            )
            .build_sync_response();
        client.process_sync(response).await?;

        assert_eq!(member_count.load(SeqCst), 2);
        assert_eq!(power_levels_count.load(SeqCst), 1);

        Ok(())
    }

    #[async_test]
    async fn test_add_event_handler_with_tuples() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        client.add_event_handler(
            |_ev: OriginalSyncRoomMemberEvent, (_room, _client): (Room, Client)| future::ready(()),
        );

        // If it compiles, it works. No need to assert anything.

        Ok(())
    }

    #[async_test]
    async fn test_remove_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let member_count = Arc::new(AtomicU8::new(0));

        client.add_event_handler({
            let member_count = member_count.clone();
            move |_ev: OriginalSyncRoomMemberEvent| async move {
                member_count.fetch_add(1, SeqCst);
            }
        });

        let handle_a = client.add_event_handler(
            // lint is buggy: rustc wants the explicit conversion from ! to () here, but clippy
            // thinks it's useless.
            #[allow(clippy::unused_unit)]
            async move |_ev: OriginalSyncRoomMemberEvent| -> () {
                panic!("handler should have been removed");
            },
        );
        let handle_b = client.add_room_event_handler(
            #[allow(unknown_lints, clippy::explicit_auto_deref)] // lint is buggy
            *DEFAULT_TEST_ROOM_ID,
            // lint is buggy: rustc wants the explicit conversion from ! to () here, but clippy
            // thinks it's useless.
            #[allow(clippy::unused_unit)]
            async move |_ev: OriginalSyncRoomMemberEvent| -> () {
                panic!("handler should have been removed");
            },
        );

        client.add_event_handler({
            let member_count = member_count.clone();
            move |_ev: OriginalSyncRoomMemberEvent| async move {
                member_count.fetch_add(1, SeqCst);
            }
        });

        let response = SyncResponseBuilder::default()
            .add_joined_room(JoinedRoomBuilder::default().add_timeline_event(MEMBER_EVENT.clone()))
            .build_sync_response();

        client.remove_event_handler(handle_a);
        client.remove_event_handler(handle_b);

        client.process_sync(response).await?;

        assert_eq!(member_count.load(SeqCst), 2);

        Ok(())
    }

    #[async_test]
    async fn test_event_handler_drop_guard() {
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
    async fn test_use_client_in_handler() {
        // This used to not work because we were requiring `Send` of event
        // handler futures even on WASM, where practically all futures that do
        // I/O aren't.
        let client = no_retry_test_client(None).await;

        client.add_event_handler(|_ev: OriginalSyncRoomMemberEvent, client: Client| async move {
            // All of Client's async methods that do network requests (and
            // possibly some that don't) are `!Send` on wasm. We obviously want
            // to be able to use them in event handlers.
            let _caps = client.get_capabilities().await.map_err(|e| anyhow::anyhow!("{}", e))?;
            anyhow::Ok(())
        });
    }

    #[async_test]
    async fn test_raw_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;
        let counter = Arc::new(AtomicU8::new(0));
        client.add_event_handler_context(counter.clone());
        client.add_event_handler(
            |_ev: Raw<OriginalSyncRoomMemberEvent>, counter: Ctx<Arc<AtomicU8>>| async move {
                counter.fetch_add(1, SeqCst);
            },
        );

        let response = SyncResponseBuilder::default()
            .add_joined_room(JoinedRoomBuilder::default().add_timeline_event(MEMBER_EVENT.clone()))
            .build_sync_response();
        client.process_sync(response).await?;

        assert_eq!(counter.load(SeqCst), 1);
        Ok(())
    }

    #[async_test]
    async fn test_enum_event_handler() -> crate::Result<()> {
        let client = logged_in_client(None).await;
        let counter = Arc::new(AtomicU8::new(0));
        client.add_event_handler_context(counter.clone());
        client.add_event_handler(
            |_ev: AnySyncStateEvent, counter: Ctx<Arc<AtomicU8>>| async move {
                counter.fetch_add(1, SeqCst);
            },
        );

        let response = SyncResponseBuilder::default()
            .add_joined_room(JoinedRoomBuilder::default().add_timeline_event(MEMBER_EVENT.clone()))
            .build_sync_response();
        client.process_sync(response).await?;

        assert_eq!(counter.load(SeqCst), 1);
        Ok(())
    }

    #[async_test]
    async fn test_observe_events() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let room_id_0 = room_id!("!r0.matrix.org");
        let room_id_1 = room_id!("!r1.matrix.org");

        let observable = client.observe_events::<OriginalSyncRoomNameEvent, Room>();

        let mut subscriber = observable.subscribe();

        assert_pending!(subscriber);

        let mut response_builder = SyncResponseBuilder::new();
        let response = response_builder
            .add_joined_room(JoinedRoomBuilder::new(room_id_0).add_state_event(
                StateTestEvent::Custom(json!({
                    "content": {
                        "name": "Name 0"
                    },
                    "event_id": "$ev0",
                    "origin_server_ts": 1,
                    "sender": "@mnt_io:matrix.org",
                    "state_key": "",
                    "type": "m.room.name",
                    "unsigned": {
                        "age": 1,
                    }
                })),
            ))
            .build_sync_response();
        client.process_sync(response).await?;

        let (room_name, room) = assert_ready!(subscriber);

        assert_eq!(room_name.event_id.as_str(), "$ev0");
        assert_eq!(room.room_id(), room_id_0);
        assert_eq!(room.name().unwrap(), "Name 0");

        assert_pending!(subscriber);

        let response = response_builder
            .add_joined_room(JoinedRoomBuilder::new(room_id_1).add_state_event(
                StateTestEvent::Custom(json!({
                    "content": {
                        "name": "Name 1"
                    },
                    "event_id": "$ev1",
                    "origin_server_ts": 2,
                    "sender": "@mnt_io:matrix.org",
                    "state_key": "",
                    "type": "m.room.name",
                    "unsigned": {
                        "age": 2,
                    }
                })),
            ))
            .build_sync_response();
        client.process_sync(response).await?;

        let (room_name, room) = assert_ready!(subscriber);

        assert_eq!(room_name.event_id.as_str(), "$ev1");
        assert_eq!(room.room_id(), room_id_1);
        assert_eq!(room.name().unwrap(), "Name 1");

        assert_pending!(subscriber);

        drop(observable);
        assert_closed!(subscriber);

        Ok(())
    }

    #[async_test]
    async fn test_observe_room_events() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let room_id = room_id!("!r0.matrix.org");

        let observable_for_room =
            client.observe_room_events::<OriginalSyncRoomNameEvent, (Room, Client)>(room_id);

        let mut subscriber_for_room = observable_for_room.subscribe();

        assert_pending!(subscriber_for_room);

        let mut response_builder = SyncResponseBuilder::new();
        let response = response_builder
            .add_joined_room(JoinedRoomBuilder::new(room_id).add_state_event(
                StateTestEvent::Custom(json!({
                    "content": {
                        "name": "Name 0"
                    },
                    "event_id": "$ev0",
                    "origin_server_ts": 1,
                    "sender": "@mnt_io:matrix.org",
                    "state_key": "",
                    "type": "m.room.name",
                    "unsigned": {
                        "age": 1,
                    }
                })),
            ))
            .build_sync_response();
        client.process_sync(response).await?;

        let (room_name, (room, _client)) = assert_ready!(subscriber_for_room);

        assert_eq!(room_name.event_id.as_str(), "$ev0");
        assert_eq!(room.name().unwrap(), "Name 0");

        assert_pending!(subscriber_for_room);

        let response = response_builder
            .add_joined_room(JoinedRoomBuilder::new(room_id).add_state_event(
                StateTestEvent::Custom(json!({
                    "content": {
                        "name": "Name 1"
                    },
                    "event_id": "$ev1",
                    "origin_server_ts": 2,
                    "sender": "@mnt_io:matrix.org",
                    "state_key": "",
                    "type": "m.room.name",
                    "unsigned": {
                        "age": 2,
                    }
                })),
            ))
            .build_sync_response();
        client.process_sync(response).await?;

        let (room_name, (room, _client)) = assert_ready!(subscriber_for_room);

        assert_eq!(room_name.event_id.as_str(), "$ev1");
        assert_eq!(room.name().unwrap(), "Name 1");

        assert_pending!(subscriber_for_room);

        drop(observable_for_room);
        assert_closed!(subscriber_for_room);

        Ok(())
    }

    #[async_test]
    async fn test_observe_several_room_events() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let room_id = room_id!("!r0.matrix.org");

        let observable_for_room =
            client.observe_room_events::<OriginalSyncRoomNameEvent, (Room, Client)>(room_id);

        let mut subscriber_for_room = observable_for_room.subscribe();

        assert_pending!(subscriber_for_room);

        let mut response_builder = SyncResponseBuilder::new();
        let response = response_builder
            .add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(StateTestEvent::Custom(json!({
                        "content": {
                            "name": "Name 0"
                        },
                        "event_id": "$ev0",
                        "origin_server_ts": 1,
                        "sender": "@mnt_io:matrix.org",
                        "state_key": "",
                        "type": "m.room.name",
                        "unsigned": {
                            "age": 1,
                        }
                    })))
                    .add_state_event(StateTestEvent::Custom(json!({
                        "content": {
                            "name": "Name 1"
                        },
                        "event_id": "$ev1",
                        "origin_server_ts": 2,
                        "sender": "@mnt_io:matrix.org",
                        "state_key": "",
                        "type": "m.room.name",
                        "unsigned": {
                            "age": 1,
                        }
                    })))
                    .add_state_event(StateTestEvent::Custom(json!({
                        "content": {
                            "name": "Name 2"
                        },
                        "event_id": "$ev2",
                        "origin_server_ts": 3,
                        "sender": "@mnt_io:matrix.org",
                        "state_key": "",
                        "type": "m.room.name",
                        "unsigned": {
                            "age": 1,
                        }
                    }))),
            )
            .build_sync_response();
        client.process_sync(response).await?;

        let (room_name, (room, _client)) = assert_ready!(subscriber_for_room);

        // Check we only get notified about the latest received event
        assert_eq!(room_name.event_id.as_str(), "$ev2");
        assert_eq!(room.name().unwrap(), "Name 2");

        assert_pending!(subscriber_for_room);

        drop(observable_for_room);
        assert_closed!(subscriber_for_room);

        Ok(())
    }

    #[async_test]
    async fn test_observe_events_with_type_prefix() -> crate::Result<()> {
        let client = logged_in_client(None).await;

        let observable = client.observe_events::<SecretStorageKeyEvent, ()>();

        let mut subscriber = observable.subscribe();

        assert_pending!(subscriber);

        let mut response_builder = SyncResponseBuilder::new();
        let response = response_builder
            .add_custom_global_account_data(json!({
                "content": {
                    "algorithm": "m.secret_storage.v1.aes-hmac-sha2",
                    "iv": "gH2iNpiETFhApvW6/FFEJQ",
                    "mac": "9Lw12m5SKDipNghdQXKjgpfdj1/K7HFI2brO+UWAGoM",
                    "passphrase": {
                        "algorithm": "m.pbkdf2",
                        "salt": "IuLnH7S85YtZmkkBJKwNUKxWF42g9O1H",
                        "iterations": 10,
                    },
                },
                "type": "m.secret_storage.key.foobar",
            }))
            .build_sync_response();
        client.process_sync(response).await?;

        let (secret_storage_key, ()) = assert_ready!(subscriber);

        assert_eq!(secret_storage_key.content.key_id, "foobar");

        assert_pending!(subscriber);

        drop(observable);
        assert_closed!(subscriber);

        Ok(())
    }

    #[async_test]
    async fn test_observe_room_events_with_type_prefix() -> crate::Result<()> {
        // To create an event handler for a room account data event type with prefix, we
        // need to create a custom event type, none exist in the Matrix specification
        // yet.
        #[derive(Debug, Clone, EventContent, Serialize)]
        #[ruma_event(type = "fake.event.*", kind = RoomAccountData)]
        struct AccountDataWithPrefixEventContent {
            #[ruma_event(type_fragment)]
            #[serde(skip)]
            key_id: String,
        }

        let room_id = room_id!("!r0.matrix.org");
        let client = logged_in_client(None).await;

        let observable = client.observe_room_events::<AccountDataWithPrefixEvent, Room>(room_id);

        let mut subscriber = observable.subscribe();

        assert_pending!(subscriber);

        let mut response_builder = SyncResponseBuilder::new();
        let response = response_builder
            .add_joined_room(
                JoinedRoomBuilder::new(room_id).add_account_data_bulk([Raw::new(&json!({
                    "content": {},
                    "type": "fake.event.foobar",
                }))
                .unwrap()
                .cast_unchecked()]),
            )
            .build_sync_response();
        client.process_sync(response).await?;

        let (secret_storage_key, _room) = assert_ready!(subscriber);

        assert_eq!(secret_storage_key.content.key_id, "foobar");

        assert_pending!(subscriber);

        drop(observable);
        assert_closed!(subscriber);

        Ok(())
    }
}
