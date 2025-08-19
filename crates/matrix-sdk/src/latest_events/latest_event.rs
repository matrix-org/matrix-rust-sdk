// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::iter::once;

use eyeball::{AsyncLock, SharedObservable, Subscriber};
use matrix_sdk_base::event_cache::Event;
use ruma::{
    events::{
        call::{invite::CallInviteEventContent, notify::CallNotifyEventContent},
        poll::unstable_start::UnstablePollStartEventContent,
        relation::RelationType,
        room::{
            member::{MembershipState, RoomMemberEventContent},
            message::{MessageType, RoomMessageEventContent},
            power_levels::RoomPowerLevels,
        },
        sticker::StickerEventContent,
        AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncStateEvent,
        AnySyncTimelineEvent, SyncStateEvent,
    },
    EventId, OwnedEventId, OwnedRoomId, OwnedTransactionId, RoomId, TransactionId, UserId,
};
use tracing::error;

use crate::{event_cache::RoomEventCache, room::WeakRoom, send_queue::RoomSendQueueUpdate};

/// The latest event of a room or a thread.
///
/// Use [`LatestEvent::subscribe`] to get a stream of updates.
#[derive(Debug)]
pub(super) struct LatestEvent {
    /// The room owning this latest event.
    _room_id: OwnedRoomId,

    /// The thread (if any) owning this latest event.
    _thread_id: Option<OwnedEventId>,

    /// A buffer of the current [`LatestEventValue`] computed for local events
    /// seen by the send queue. See [`LatestEventValuesForLocalEvents`] to learn
    /// more.
    buffer_of_values_for_local_events: LatestEventValuesForLocalEvents,

    /// The latest event value.
    current_value: SharedObservable<LatestEventValue, AsyncLock>,
}

impl LatestEvent {
    pub(super) async fn new(
        room_id: &RoomId,
        thread_id: Option<&EventId>,
        room_event_cache: &RoomEventCache,
        weak_room: &WeakRoom,
    ) -> Self {
        Self {
            _room_id: room_id.to_owned(),
            _thread_id: thread_id.map(ToOwned::to_owned),
            buffer_of_values_for_local_events: LatestEventValuesForLocalEvents::new(),
            current_value: SharedObservable::new_async(
                LatestEventValue::new_remote(room_event_cache, weak_room).await,
            ),
        }
    }

    /// Return a [`Subscriber`] to new values.
    pub async fn subscribe(&self) -> Subscriber<LatestEventValue, AsyncLock> {
        self.current_value.subscribe().await
    }

    /// Update the inner latest event value, based on the event cache
    /// (specifically with a [`RoomEventCache`]).
    pub async fn update_with_event_cache(
        &mut self,
        room_event_cache: &RoomEventCache,
        power_levels: &Option<(&UserId, RoomPowerLevels)>,
    ) {
        let new_value =
            LatestEventValue::new_remote_with_power_levels(room_event_cache, power_levels).await;

        self.update(new_value).await;
    }

    /// Update the inner latest event value, based on the send queue
    /// (specifically with a [`RoomSendQueueUpdate`]).
    pub async fn update_with_send_queue(
        &mut self,
        send_queue_update: &RoomSendQueueUpdate,
        room_event_cache: &RoomEventCache,
        power_levels: &Option<(&UserId, RoomPowerLevels)>,
    ) {
        let new_value = LatestEventValue::new_local(
            send_queue_update,
            &mut self.buffer_of_values_for_local_events,
            room_event_cache,
            power_levels,
        )
        .await;

        self.update(new_value).await;
    }

    /// Update [`Self::current_value`] if and only if the `new_value` is not
    /// [`LatestEventValue::None`].
    async fn update(&mut self, new_value: LatestEventValue) {
        if let LatestEventValue::None = new_value {
            // Do not update to a `None` value.
        } else {
            self.current_value.set(new_value).await;
        }
    }
}

/// A latest event value!
#[derive(Debug, Default, Clone)]
pub enum LatestEventValue {
    /// No value has been computed yet, or no candidate value was found.
    #[default]
    None,

    /// The latest event represents a remote event.
    Remote(LatestEventContent),

    /// The latest event represents a local event that is sending.
    LocalIsSending(LatestEventContent),

    /// The latest event represents a local event that is wedged, either because
    /// a previous local event, or this local event cannot be sent.
    LocalIsWedged(LatestEventContent),
}

impl LatestEventValue {
    /// Create a new [`LatestEventValue::Remote`].
    async fn new_remote(room_event_cache: &RoomEventCache, weak_room: &WeakRoom) -> Self {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        let room = weak_room.get();
        let power_levels = match &room {
            Some(room) => {
                let power_levels = room.power_levels().await.ok();

                Some(room.own_user_id()).zip(power_levels)
            }

            None => None,
        };

        Self::new_remote_with_power_levels(room_event_cache, &power_levels).await
    }

    /// Create a new [`LatestEventValue::Remote`] based on existing power
    /// levels.
    async fn new_remote_with_power_levels(
        room_event_cache: &RoomEventCache,
        power_levels: &Option<(&UserId, RoomPowerLevels)>,
    ) -> Self {
        room_event_cache
            .rfind_map_event_in_memory_by(|event| find_and_map_timeline_event(event, power_levels))
            .await
            .map(Self::Remote)
            .unwrap_or_default()
    }

    /// Create a new [`LatestEventValue::LocalIsSending`] or
    /// [`LatestEventValue::LocalIsWedged`].
    async fn new_local(
        send_queue_update: &RoomSendQueueUpdate,
        buffer_of_values_for_local_events: &mut LatestEventValuesForLocalEvents,
        room_event_cache: &RoomEventCache,
        power_levels: &Option<(&UserId, RoomPowerLevels)>,
    ) -> Self {
        use crate::send_queue::{LocalEcho, LocalEchoContent};

        match send_queue_update {
            // A new local event is being sent.
            //
            // Let's create the `LatestEventValue` and push it in the buffer of values.
            RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id,
                content: local_echo_content,
            }) => match local_echo_content {
                LocalEchoContent::Event { serialized_event: content, .. } => {
                    match content.deserialize() {
                        Ok(content) => {
                            if let Some(content) = extract_content_from_any_message_like(content) {
                                let value = Self::LocalIsSending(content);

                                buffer_of_values_for_local_events
                                    .push(transaction_id.to_owned(), value.clone());

                                value
                            } else {
                                Self::None
                            }
                        }

                        Err(error) => {
                            error!(?error, "Failed to deserialize an event from `RoomSendQueueUpdate::NewLocalEvent`");

                            Self::None
                        }
                    }
                }

                LocalEchoContent::React { .. } => Self::None,
            },

            // A local event has been cancelled before being sent.
            //
            // Remove the calculated `LatestEventValue` from the buffer of values, and return the
            // last `LatestEventValue` or calculate a new one.
            RoomSendQueueUpdate::CancelledLocalEvent { transaction_id } => {
                if let Some(position) = buffer_of_values_for_local_events.position(transaction_id) {
                    buffer_of_values_for_local_events.remove(position);
                }

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    power_levels,
                )
                .await
            }

            // A local event has successfully been sent!
            //
            // Unwedge all wedged values after the one matching `transaction_id`. Indeed, if
            // an event has been sent, it means the send queue is working, so if any value has been
            // marked as wedged, it must be marked as unwedged. Then, remove the calculated
            // `LatestEventValue` from the buffer of values. Finally, return the last
            // `LatestEventValue` or calculate a new one.
            RoomSendQueueUpdate::SentEvent { transaction_id, .. } => {
                let position =
                    buffer_of_values_for_local_events.mark_unwedged_after(transaction_id);

                if let Some(position) = position {
                    buffer_of_values_for_local_events.remove(position);
                }

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    power_levels,
                )
                .await
            }

            // A local event has been replaced by another one.
            //
            // Replace the latest event value matching `transaction_id` in the buffer if it exists
            // (note: it should!), and return the last `LatestEventValue` or calculate a new one.
            RoomSendQueueUpdate::ReplacedLocalEvent { transaction_id, new_content: content } => {
                if let Some(position) = buffer_of_values_for_local_events.position(transaction_id) {
                    match content.deserialize() {
                        Ok(content) => {
                            if let Some(content) = extract_content_from_any_message_like(content) {
                                buffer_of_values_for_local_events
                                    .replace_content(position, content);
                            } else {
                                buffer_of_values_for_local_events.remove(position);
                            }
                        }

                        Err(error) => {
                            error!(?error, "Failed to deserialize an event from `RoomSendQueueUpdate::ReplacedLocalEvent`");

                            return Self::None;
                        }
                    }
                }

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    power_levels,
                )
                .await
            }

            // An error has occurred.
            //
            // Mark the latest event value matching `transaction_id`, and all its following values,
            // as wedged.
            RoomSendQueueUpdate::SendError { transaction_id, .. } => {
                buffer_of_values_for_local_events.mark_wedged_from(transaction_id);

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    power_levels,
                )
                .await
            }

            // A local event has been unwedged and sending is being retried.
            //
            // Mark the latest event value matching `transaction_id`, and all its following values,
            // as unwedged.
            RoomSendQueueUpdate::RetryEvent { transaction_id } => {
                buffer_of_values_for_local_events.mark_unwedged_from(transaction_id);

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    power_levels,
                )
                .await
            }

            // A media upload has made progress.
            //
            // Nothing to do here.
            RoomSendQueueUpdate::MediaUpload { .. } => Self::None,
        }
    }

    /// Get the last [`LatestEventValue`] from the local latest event values if
    /// any, or create a new [`LatestEventValue`] from the remote events.
    ///
    /// If the buffer of latest event values is not empty, let's return the last
    /// one. Otherwise, it means we no longer have any local event: let's
    /// fallback on remote event!
    async fn new_local_or_remote(
        buffer_of_values_for_local_events: &mut LatestEventValuesForLocalEvents,
        room_event_cache: &RoomEventCache,
        power_levels: &Option<(&UserId, RoomPowerLevels)>,
    ) -> Self {
        if let Some(value) = buffer_of_values_for_local_events.last() {
            value.clone()
        } else {
            Self::new_remote_with_power_levels(room_event_cache, power_levels).await
        }
    }
}

/// A buffer of the current [`LatestEventValue`] computed for local events
/// seen by the send queue. It is used by
/// [`LatestEvent::buffer_of_values_for_local_events`].
///
/// The system does only receive [`RoomSendQueueUpdate`]s. It's not designed to
/// iterate over local events in the send queue when a local event is changed
/// (cancelled, or updated for example). That's why we keep our own buffer here.
/// Imagine the system receives 4 [`RoomSendQueueUpdate`]:
///
/// 1. [`RoomSendQueueUpdate::NewLocalEvent`]: new local event,
/// 2. [`RoomSendQueueUpdate::NewLocalEvent`]: new local event,
/// 3. [`RoomSendQueueUpdate::ReplacedLocalEvent`]: replaced the first local
///    event,
/// 4. [`RoomSendQueueUpdate::CancelledLocalEvent`]: cancelled the second local
///    event.
///
/// `NewLocalEvent`s will trigger the computation of new
/// `LatestEventValue`s, but `CancelledLocalEvent` for example doesn't hold
/// any information to compute a new `LatestEventValue`, so we need to
/// remember the previous values, until the local events are sent and
/// removed from this buffer.
///
/// Another reason why we need a buffer is to handle wedged local event. Imagine
/// the system receives 3 [`RoomSendQueueUpdate`]:
///
/// 1. [`RoomSendQueueUpdate::NewLocalEvent`]: new local event,
/// 2. [`RoomSendQueueUpdate::NewLocalEvent`]: new local event,
/// 3. [`RoomSendQueueUpdate::SendError`]: the first local event has failed to
///    be sent.
///
/// Because a `SendError` is received (targeting the first `NewLocalEvent`), the
/// send queue is stopped. However, the `LatestEventValue` targets the second
/// `NewLocalEvent`. The system must consider that when a local event is wedged,
/// all the following local events must also be marked as wedged. And vice
/// versa, when the send queue is able to send an event again, all the following
/// local events must be marked as unwedged.
///
/// This type isolates a couple of methods designed to manage these specific
/// behaviours.
#[derive(Debug)]
struct LatestEventValuesForLocalEvents {
    buffer: Vec<(OwnedTransactionId, LatestEventValue)>,
}

impl LatestEventValuesForLocalEvents {
    /// Create a new [`LatestEventValuesForLocalEvents`].
    fn new() -> Self {
        Self { buffer: Vec::with_capacity(2) }
    }

    /// Get the last [`LatestEventValue`].
    fn last(&self) -> Option<&LatestEventValue> {
        self.buffer.last().map(|(_, value)| value)
    }

    /// Find the position of the [`LatestEventValue`] matching `transaction_id`.
    fn position(&self, transaction_id: &TransactionId) -> Option<usize> {
        self.buffer
            .iter()
            .position(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
    }

    /// Push a new [`LatestEventValue`].
    ///
    /// # Panics
    ///
    /// Panics if `value` is not of kind [`LatestEventValue::LocalIsSending`] or
    /// [`LatestEventValue::LocalIsWedged`].
    fn push(&mut self, transaction_id: OwnedTransactionId, value: LatestEventValue) {
        assert!(
            matches!(
                value,
                LatestEventValue::LocalIsSending(_) | LatestEventValue::LocalIsWedged(_)
            ),
            "`value` must be either `LocalIsSending` or `LocalIsWedged`"
        );

        self.buffer.push((transaction_id, value));
    }

    /// Replace the [`LatestEventContent`] of the [`LatestEventValue`] at
    /// position `position`.
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - `position` is strictly greater than buffer's length,
    /// - the [`LatestEventValue`] is not of kind
    ///   [`LatestEventValue::LocalIsSending`] or
    ///   [`LatestEventValue::LocalIsWedged`].
    fn replace_content(&mut self, position: usize, new_content: LatestEventContent) {
        let (_, value) = self.buffer.get_mut(position).expect("`position` must be valid");

        match value {
            LatestEventValue::LocalIsSending(content) => *content = new_content,
            LatestEventValue::LocalIsWedged(content) => *content = new_content,
            _ => panic!("`value` must be either `LocalIsSending` or `LocalIsWedged`"),
        }
    }

    /// Remove the [`LatestEventValue`] at position `position`.
    ///
    /// # Panics
    ///
    /// Panics if `position` is strictly greater than buffer's length.
    fn remove(&mut self, position: usize) -> (OwnedTransactionId, LatestEventValue) {
        self.buffer.remove(position)
    }

    /// Mark the `LatestEventValue` matching `transaction_id`, and all the
    /// following values, as wedged.
    fn mark_wedged_from(&mut self, transaction_id: &TransactionId) {
        let mut values = self.buffer.iter_mut();

        if let Some(first_value_to_wedge) = values
            .by_ref()
            .find(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over the found value and the following ones.
            for (_, value_to_wedge) in once(first_value_to_wedge).chain(values) {
                if let LatestEventValue::LocalIsSending(content) = value_to_wedge {
                    *value_to_wedge = LatestEventValue::LocalIsWedged(content.clone());
                }
            }
        }
    }

    /// Mark the `LatestEventValue` matching `transaction_id`, and all the
    /// following values, as unwedged.
    fn mark_unwedged_from(&mut self, transaction_id: &TransactionId) {
        let mut values = self.buffer.iter_mut();

        if let Some(first_value_to_unwedge) = values
            .by_ref()
            .find(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over the found value and the following ones.
            for (_, value_to_unwedge) in once(first_value_to_unwedge).chain(values) {
                if let LatestEventValue::LocalIsWedged(content) = value_to_unwedge {
                    *value_to_unwedge = LatestEventValue::LocalIsSending(content.clone());
                }
            }
        }
    }

    /// Mark all the following values after the `LatestEventValue` matching
    /// `transaction_id` as unwedged.
    ///
    /// Note that contrary to [`Self::unwedged_from`], the `LatestEventValue` is
    /// untouched. However, its position is returned (if any).
    fn mark_unwedged_after(&mut self, transaction_id: &TransactionId) -> Option<usize> {
        let mut values = self.buffer.iter_mut();

        if let Some(position) = values
            .by_ref()
            .position(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over all values after the found one.
            for (_, value_to_unwedge) in values {
                if let LatestEventValue::LocalIsWedged(content) = value_to_unwedge {
                    *value_to_unwedge = LatestEventValue::LocalIsSending(content.clone());
                }
            }

            Some(position)
        } else {
            None
        }
    }
}

/// A latest event value content!
#[derive(Debug, Clone)]
pub enum LatestEventContent {
    /// A `m.room.message` event.
    RoomMessage(RoomMessageEventContent),

    /// A `m.sticker` event.
    Sticker(StickerEventContent),

    /// An `org.matrix.msc3381.poll.start` event.
    Poll(UnstablePollStartEventContent),

    /// A `m.call.invite` event.
    CallInvite(CallInviteEventContent),

    /// A `m.call.notify` event.
    CallNotify(CallNotifyEventContent),

    /// A `m.room.member` event, more precisely a knock membership change that
    /// can be handled by the current user.
    KnockedStateEvent(RoomMemberEventContent),

    /// A redacted event.
    Redacted(AnySyncMessageLikeEvent),
}

fn find_and_map_timeline_event(
    event: &Event,
    power_levels: &Option<(&UserId, RoomPowerLevels)>,
) -> Option<LatestEventContent> {
    // Cast the event into an `AnySyncTimelineEvent`. If deserializing fails, we
    // ignore the event.
    let event = match event.raw().deserialize() {
        Ok(event) => event,
        Err(error) => {
            error!(
                ?error,
                "Failed to deserialize the event when looking for a suitable latest event"
            );

            return None;
        }
    };

    match event {
        AnySyncTimelineEvent::MessageLike(message_like_event) => {
            match message_like_event.original_content() {
                Some(any_message_like_event_content) => {
                    extract_content_from_any_message_like(any_message_like_event_content)
                }

                // The event has been redacted.
                None => Some(LatestEventContent::Redacted(message_like_event)),
            }
        }

        // We don't currently support most state events…
        AnySyncTimelineEvent::State(state) => {
            // … but we make an exception for knocked state events _if_ the current user
            // can either accept or decline them.
            if let AnySyncStateEvent::RoomMember(member) = state {
                if matches!(member.membership(), MembershipState::Knock) {
                    let can_accept_or_decline_knocks = match power_levels {
                        Some((own_user_id, room_power_levels)) => {
                            room_power_levels.user_can_invite(own_user_id)
                                || room_power_levels.user_can_kick(own_user_id)
                        }
                        _ => false,
                    };

                    // The current user can act on the knock changes, so they should be
                    // displayed
                    if can_accept_or_decline_knocks {
                        return Some(LatestEventContent::KnockedStateEvent(match member {
                            SyncStateEvent::Original(member) => member.content,
                            SyncStateEvent::Redacted(_) => {
                                // Cannot decide if the user can accept or decline knocks because
                                // the event has been redacted.
                                return None;
                            }
                        }));
                    }
                }
            }

            None
        }
    }
}

fn extract_content_from_any_message_like(
    event: AnyMessageLikeEventContent,
) -> Option<LatestEventContent> {
    match event {
        AnyMessageLikeEventContent::RoomMessage(message) => {
            // Don't show incoming verification requests.
            if let MessageType::VerificationRequest(_) = message.msgtype {
                return None;
            }

            // Check if this is a replacement for another message. If it is, ignore
            // it.
            let is_replacement = message.relates_to.as_ref().is_some_and(|relates_to| {
                if let Some(relation_type) = relates_to.rel_type() {
                    relation_type == RelationType::Replacement
                } else {
                    false
                }
            });

            if is_replacement {
                None
            } else {
                Some(LatestEventContent::RoomMessage(message))
            }
        }

        AnyMessageLikeEventContent::UnstablePollStart(poll) => Some(LatestEventContent::Poll(poll)),

        AnyMessageLikeEventContent::CallInvite(invite) => {
            Some(LatestEventContent::CallInvite(invite))
        }

        AnyMessageLikeEventContent::CallNotify(notify) => {
            Some(LatestEventContent::CallNotify(notify))
        }

        AnyMessageLikeEventContent::Sticker(sticker) => Some(LatestEventContent::Sticker(sticker)),

        // Encrypted events are not suitable.
        AnyMessageLikeEventContent::RoomEncrypted(_) => None,

        // Everything else is considered not suitable.
        _ => None,
    }
}

#[cfg(test)]
mod tests_latest_event_content {
    use assert_matches::assert_matches;
    use matrix_sdk_test::event_factory::EventFactory;
    use ruma::{event_id, user_id};

    use super::{find_and_map_timeline_event, LatestEventContent, RoomMessageEventContent};

    macro_rules! assert_latest_event_content {
        ( with | $event_factory:ident | $event_builder:block
          it produces $match:pat ) => {
            let user_id = user_id!("@mnt_io:matrix.org");
            let event_factory = EventFactory::new().sender(user_id);
            let event = {
                let $event_factory = event_factory;
                $event_builder
            };

            assert_matches!(find_and_map_timeline_event(&event, &None), $match);
        };
    }

    #[test]
    fn test_room_message() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory.text_msg("hello").into_event()
            }
            it produces Some(LatestEventContent::RoomMessage(_))
        );
    }

    #[test]
    fn test_redacted() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .redacted(
                        user_id!("@mnt_io:matrix.org"),
                        ruma::events::room::message::RedactedRoomMessageEventContent::new()
                    )
                    .into_event()
            }
            it produces Some(LatestEventContent::Redacted(_))
        );
    }

    #[test]
    fn test_room_message_replacement() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .text_msg("bonjour")
                    .edit(
                        event_id!("$ev0"),
                        RoomMessageEventContent::text_plain("hello").into()
                    )
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_poll() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .poll_start(
                        "the people need to know",
                        "comté > gruyère",
                        vec!["yes", "oui"]
                    )
                    .into_event()
            }
            it produces Some(LatestEventContent::Poll(_))
        );
    }

    #[test]
    fn test_call_invite() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .call_invite(
                        ruma::OwnedVoipId::from("vvooiipp".to_owned()),
                        ruma::UInt::from(1234u32),
                        ruma::events::call::SessionDescription::new("type".to_owned(), "sdp".to_owned()),
                        ruma::VoipVersionId::V1,
                    )
                    .into_event()
            }
            it produces Some(LatestEventContent::CallInvite(_))
        );
    }

    #[test]
    fn test_call_notify() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .call_notify(
                        "call_id".to_owned(),
                        ruma::events::call::notify::ApplicationType::Call,
                        ruma::events::call::notify::NotifyType::Ring,
                        ruma::events::Mentions::new(),
                    )
                    .into_event()
            }
            it produces Some(LatestEventContent::CallNotify(_))
        );
    }

    #[test]
    fn test_sticker() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .sticker(
                        "wink wink",
                        ruma::events::room::ImageInfo::new(),
                        ruma::OwnedMxcUri::from("mxc://foo/bar")
                    )
                    .into_event()
            }
            it produces Some(LatestEventContent::Sticker(_))
        );
    }

    #[test]
    fn test_encrypted_room_message() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .event(ruma::events::room::encrypted::RoomEncryptedEventContent::new(
                        ruma::events::room::encrypted::EncryptedEventScheme::MegolmV1AesSha2(
                            ruma::events::room::encrypted::MegolmV1AesSha2ContentInit {
                                ciphertext: "cipher".to_owned(),
                                sender_key: "sender_key".to_owned(),
                                device_id: "device_id".into(),
                                session_id: "session_id".to_owned(),
                            }
                            .into(),
                        ),
                        None,
                    ))
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_reaction() {
        // Take a random message-like event.
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .reaction(event_id!("$ev0"), "+1")
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_state_event() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .room_topic("new room topic")
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_knocked_state_event_without_power_levels() {
        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .member(user_id!("@other_mnt_io:server.name"))
                    .membership(ruma::events::room::member::MembershipState::Knock)
                    .into_event()
            }
            it produces None
        );
    }

    #[test]
    fn test_knocked_state_event_with_power_levels() {
        use ruma::{
            events::room::{
                member::MembershipState,
                power_levels::{RoomPowerLevels, RoomPowerLevelsSource},
            },
            room_version_rules::AuthorizationRules,
        };

        let user_id = user_id!("@mnt_io:matrix.org");
        let other_user_id = user_id!("@other_mnt_io:server.name");
        let event_factory = EventFactory::new().sender(user_id);
        let event =
            event_factory.member(other_user_id).membership(MembershipState::Knock).into_event();

        let mut room_power_levels =
            RoomPowerLevels::new(RoomPowerLevelsSource::None, &AuthorizationRules::V1, []);
        room_power_levels.users_default = 5.into();

        // Cannot accept. Cannot decline.
        {
            let mut room_power_levels = room_power_levels.clone();
            room_power_levels.invite = 10.into();
            room_power_levels.kick = 10.into();
            assert_matches!(
                find_and_map_timeline_event(&event, &Some((user_id, room_power_levels))),
                None,
                "cannot accept, cannot decline",
            );
        }

        // Can accept. Cannot decline.
        {
            let mut room_power_levels = room_power_levels.clone();
            room_power_levels.invite = 0.into();
            room_power_levels.kick = 10.into();
            assert_matches!(
                find_and_map_timeline_event(&event, &Some((user_id, room_power_levels))),
                Some(LatestEventContent::KnockedStateEvent(_)),
                "can accept, cannot decline",
            );
        }

        // Cannot accept. Can decline.
        {
            let mut room_power_levels = room_power_levels.clone();
            room_power_levels.invite = 10.into();
            room_power_levels.kick = 0.into();
            assert_matches!(
                find_and_map_timeline_event(&event, &Some((user_id, room_power_levels))),
                Some(LatestEventContent::KnockedStateEvent(_)),
                "cannot accept, can decline",
            );
        }

        // Can accept. Can decline.
        {
            room_power_levels.invite = 0.into();
            room_power_levels.kick = 0.into();
            assert_matches!(
                find_and_map_timeline_event(&event, &Some((user_id, room_power_levels))),
                Some(LatestEventContent::KnockedStateEvent(_)),
                "can accept, can decline",
            );
        }
    }

    #[test]
    fn test_room_message_verification_request() {
        use ruma::{events::room::message, OwnedDeviceId};

        assert_latest_event_content!(
            with |event_factory| {
                event_factory
                    .event(
                        RoomMessageEventContent::new(
                            message::MessageType::VerificationRequest(
                                message::KeyVerificationRequestEventContent::new(
                                    "body".to_owned(),
                                    vec![],
                                    OwnedDeviceId::from("device_id"),
                                    user_id!("@user:server.name").to_owned(),
                                )
                            )
                        )
                    )
                    .into_event()
            }
            it produces None
        );
    }
}

#[cfg(test)]
mod tests_latest_event_values_for_local_events {
    use assert_matches::assert_matches;
    use ruma::OwnedTransactionId;

    use super::{
        LatestEventContent, LatestEventValue, LatestEventValuesForLocalEvents,
        RoomMessageEventContent,
    };

    fn room_message(body: &str) -> LatestEventContent {
        LatestEventContent::RoomMessage(RoomMessageEventContent::text_plain(body))
    }

    #[test]
    fn test_last() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        assert!(buffer.last().is_none());

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::LocalIsSending(room_message("tome")),
        );

        assert_matches!(
            buffer.last(),
            Some(LatestEventValue::LocalIsSending(LatestEventContent::RoomMessage(_)))
        );
    }

    #[test]
    fn test_position() {
        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid");

        assert!(buffer.position(&transaction_id).is_none());

        buffer.push(
            transaction_id.clone(),
            LatestEventValue::LocalIsSending(room_message("raclette")),
        );
        buffer.push(
            OwnedTransactionId::from("othertxnid"),
            LatestEventValue::LocalIsSending(room_message("tome")),
        );

        assert_eq!(buffer.position(&transaction_id), Some(0));
    }

    #[test]
    #[should_panic]
    fn test_push_none() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(OwnedTransactionId::from("txnid"), LatestEventValue::None);
    }

    #[test]
    #[should_panic]
    fn test_push_remote() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::Remote(room_message("tome")),
        );
    }

    #[test]
    fn test_push_local() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid0"),
            LatestEventValue::LocalIsSending(room_message("tome")),
        );
        buffer.push(
            OwnedTransactionId::from("txnid1"),
            LatestEventValue::LocalIsWedged(room_message("raclette")),
        );

        // no panic.
    }

    #[test]
    fn test_replace_content() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid0"),
            LatestEventValue::LocalIsSending(room_message("gruyère")),
        );

        buffer.replace_content(0, room_message("comté"));

        assert_matches!(
            buffer.last(),
            Some(LatestEventValue::LocalIsSending(LatestEventContent::RoomMessage(content))) => {
                assert_eq!(content.body(), "comté");
            }
        );
    }

    #[test]
    fn test_remove() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::LocalIsSending(room_message("gryuère")),
        );

        assert!(buffer.last().is_some());

        buffer.remove(0);

        assert!(buffer.last().is_none());
    }

    #[test]
    fn test_wedged_from() {
        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(transaction_id_0, LatestEventValue::LocalIsSending(room_message("gruyère")));
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalIsSending(room_message("brigand")),
        );
        buffer.push(transaction_id_2, LatestEventValue::LocalIsSending(room_message("raclette")));

        buffer.mark_wedged_from(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalIsWedged(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalIsWedged(_));
    }

    #[test]
    fn test_unwedged_from() {
        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(transaction_id_0, LatestEventValue::LocalIsWedged(room_message("gruyère")));
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalIsWedged(room_message("brigand")),
        );
        buffer.push(transaction_id_2, LatestEventValue::LocalIsWedged(room_message("raclette")));

        buffer.mark_unwedged_from(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalIsWedged(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalIsSending(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalIsSending(_));
    }

    #[test]
    fn test_unwedged_after() {
        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(transaction_id_0, LatestEventValue::LocalIsWedged(room_message("gruyère")));
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalIsWedged(room_message("brigand")),
        );
        buffer.push(transaction_id_2, LatestEventValue::LocalIsWedged(room_message("raclette")));

        buffer.mark_unwedged_after(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalIsWedged(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalIsWedged(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalIsSending(_));
    }
}

#[cfg(all(not(target_family = "wasm"), test))]
mod tests_latest_event_value_non_wasm {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
        store::SerializableEventContent,
        RoomState,
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        event_id,
        events::{
            reaction::ReactionEventContent, relation::Annotation,
            room::message::RoomMessageEventContent, AnyMessageLikeEventContent,
        },
        room_id, user_id, MilliSecondsSinceUnixEpoch, OwnedRoomId, OwnedTransactionId,
    };

    use super::{
        LatestEvent, LatestEventContent, LatestEventValue, LatestEventValuesForLocalEvents,
        RoomEventCache, RoomSendQueueUpdate,
    };
    use crate::{
        client::WeakClient,
        room::WeakRoom,
        send_queue::{AbstractProgress, LocalEcho, LocalEchoContent, RoomSendQueue, SendHandle},
        test_utils::mocks::MatrixMockServer,
        Client, Error,
    };

    #[async_test]
    async fn test_update_ignores_none_value() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let weak_client = WeakClient::from_client(&client);

        // Create the room.
        client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let weak_room = WeakRoom::new(weak_client, room_id.to_owned());

        // Get a `RoomEventCache`.
        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        let mut latest_event = LatestEvent::new(room_id, None, &room_event_cache, &weak_room).await;

        // First off, check the default value is `None`!
        assert_matches!(latest_event.current_value.get().await, LatestEventValue::None);

        // Second, set a new value.
        latest_event
            .update(LatestEventValue::LocalIsSending(LatestEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("foo"),
            )))
            .await;

        assert_matches!(
            latest_event.current_value.get().await,
            LatestEventValue::LocalIsSending(_)
        );

        // Finally, set a new `None` value. It must be ignored.
        latest_event.update(LatestEventValue::None).await;

        assert_matches!(
            latest_event.current_value.get().await,
            LatestEventValue::LocalIsSending(_)
        );
    }

    #[async_test]
    async fn test_remote_is_scanning_event_backwards_from_event_cache() {
        let room_id = room_id!("!r0");
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(room_id);
        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");
        let event_id_2 = event_id!("$ev2");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Prelude.
        {
            // Create the room.
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            // Initialise the event cache store.
            client
                .event_cache_store()
                .lock()
                .await
                .unwrap()
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![
                                // a latest event candidate
                                event_factory.text_msg("hello").event_id(event_id_0).into(),
                                // a latest event candidate
                                event_factory.text_msg("world").event_id(event_id_1).into(),
                                // not a latest event candidate
                                event_factory
                                    .room_topic("new room topic")
                                    .event_id(event_id_2)
                                    .into(),
                            ],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();
        let weak_room = WeakRoom::new(WeakClient::from_client(&client), room_id.to_owned());

        assert_matches!(
            LatestEventValue::new_remote(&room_event_cache, &weak_room).await,
            LatestEventValue::Remote(LatestEventContent::RoomMessage(message_content)) => {
                // We get `event_id_1` because `event_id_2` isn't a candidate,
                // and `event_id_0` hasn't been read yet (because events are
                // read backwards).
                assert_eq!(message_content.body(), "world");
            }
        );
    }

    async fn local_prelude() -> (Client, OwnedRoomId, RoomSendQueue, RoomEventCache) {
        let room_id = room_id!("!r0").to_owned();

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        client.base_client().get_or_create_room(&room_id, RoomState::Joined);
        let room = client.get_room(&room_id).unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(&room_id).await.unwrap();

        let send_queue = client.send_queue();
        let room_send_queue = send_queue.for_room(room);

        (client, room_id, room_send_queue, room_event_cache)
    }

    fn new_local_echo_content(
        room_send_queue: &RoomSendQueue,
        transaction_id: &OwnedTransactionId,
        body: &str,
    ) -> LocalEchoContent {
        LocalEchoContent::Event {
            serialized_event: SerializableEventContent::new(
                &AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent::text_plain(body)),
            )
            .unwrap(),
            send_handle: SendHandle::new(
                room_send_queue.clone(),
                transaction_id.clone(),
                MilliSecondsSinceUnixEpoch::now(),
            ),
            send_error: None,
        }
    }

    #[async_test]
    async fn test_local_new_local_event() {
        let (_client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;

        let mut buffer = LatestEventValuesForLocalEvents::new();

        // Receiving one `NewLocalEvent`.
        {
            let transaction_id = OwnedTransactionId::from("txnid0");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the new local event.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(LatestEventContent::RoomMessage(message_content)) => {
                    assert_eq!(message_content.body(), "A");
                }
            );
        }

        // Receiving another `NewLocalEvent`, ensuring it's pushed back in the buffer.
        {
            let transaction_id = OwnedTransactionId::from("txnid1");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "B");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the new local event.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );
        }

        assert_eq!(buffer.buffer.len(), 2);
        assert_matches!(
            &buffer.buffer[0].1,
            LatestEventValue::LocalIsSending(
                LatestEventContent::RoomMessage(message_content)
            ) => {
                assert_eq!(message_content.body(), "A");
            }
        );
        assert_matches!(
            &buffer.buffer[1].1,
            LatestEventValue::LocalIsSending(
                LatestEventContent::RoomMessage(message_content)
            ) => {
                assert_eq!(message_content.body(), "B");
            }
        );
    }

    #[async_test]
    async fn test_local_cancelled_local_event() {
        let (_client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        // Receiving three `NewLocalEvent`s.
        {
            for (transaction_id, body) in
                [(&transaction_id_0, "A"), (&transaction_id_1, "B"), (&transaction_id_2, "C")]
            {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_matches!(
                    LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                    LatestEventValue::LocalIsSending(
                        LatestEventContent::RoomMessage(message_content)
                    ) => {
                        assert_eq!(message_content.body(), body);
                    }
                );
            }

            assert_eq!(buffer.buffer.len(), 3);
        }

        // Receiving a `CancelledLocalEvent` targeting the second event. The
        // `LatestEventValue` must not change.
        {
            let update = RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: transaction_id_1.clone(),
            };

            // The `LatestEventValue` hasn't changed, it still matches the latest local
            // event.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "C");
                }
            );

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `CancelledLocalEvent` targeting the second (so the last) event.
        // The `LatestEventValue` must point to the first local event.
        {
            let update = RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: transaction_id_2.clone(),
            };

            // The `LatestEventValue` has changed, it matches the previous (so the first)
            // local event.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "A");
                }
            );

            assert_eq!(buffer.buffer.len(), 1);
        }

        // Receiving a `CancelledLocalEvent` targeting the first (so the last) event.
        // The `LatestEventValue` cannot be computed from the send queue and will
        // fallback to the event cache. The event cache is empty in this case, so we get
        // nothing.
        {
            let update =
                RoomSendQueueUpdate::CancelledLocalEvent { transaction_id: transaction_id_0 };

            // The `LatestEventValue` has changed, it's empty!
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::None
            );

            assert!(buffer.buffer.is_empty());
        }
    }

    #[async_test]
    async fn test_local_sent_event() {
        let (_client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        {
            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_matches!(
                    LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                    LatestEventValue::LocalIsSending(
                        LatestEventContent::RoomMessage(message_content)
                    ) => {
                        assert_eq!(message_content.body(), body);
                    }
                );
            }

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // must not change.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_0.clone(),
                event_id: event_id!("$ev0").to_owned(),
            };

            // The `LatestEventValue` hasn't changed, it still matches the latest local
            // event.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );

            assert_eq!(buffer.buffer.len(), 1);
        }

        // Receiving a `SentEvent` targeting the first event. The `LaetstEvent` cannot
        // be computed from the send queue and will fallback to the event cache.
        // The event cache is empty in this case, so we get nothing.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_1,
                event_id: event_id!("$ev1").to_owned(),
            };

            // The `LatestEventValue` has changed, it's empty!
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::None
            );

            assert!(buffer.buffer.is_empty());
        }
    }

    #[async_test]
    async fn test_local_replaced_local_event() {
        let (_client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        {
            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_matches!(
                    LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                    LatestEventValue::LocalIsSending(
                        LatestEventContent::RoomMessage(message_content)
                    ) => {
                        assert_eq!(message_content.body(), body);
                    }
                );
            }

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `ReplacedLocalEvent` targeting the first event. The
        // `LatestEventValue` must not change.
        {
            let transaction_id = &transaction_id_0;
            let LocalEchoContent::Event { serialized_event: new_content, .. } =
                new_local_echo_content(&room_send_queue, transaction_id, "A.")
            else {
                panic!("oopsy");
            };

            let update = RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: transaction_id.clone(),
                new_content,
            };

            // The `LatestEventValue` hasn't changed, it still matches the latest local
            // event.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `ReplacedLocalEvent` targeting the second (so the last) event.
        // The `LatestEventValue` is changing.
        {
            let transaction_id = &transaction_id_1;
            let LocalEchoContent::Event { serialized_event: new_content, .. } =
                new_local_echo_content(&room_send_queue, transaction_id, "B.")
            else {
                panic!("oopsy");
            };

            let update = RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: transaction_id.clone(),
                new_content,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but with its new content.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B.");
                }
            );

            assert_eq!(buffer.buffer.len(), 2);
        }
    }

    #[async_test]
    async fn test_local_replaced_local_event_by_a_non_suitable_event() {
        let (_client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid0");

        // Receiving one `NewLocalEvent`.
        {
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "A");
                }
            );

            assert_eq!(buffer.buffer.len(), 1);
        }

        // Receiving a `ReplacedLocalEvent` targeting the first event. Sadly, the new
        // event cannot be mapped to a `LatestEventValue`! The first event is removed
        // from the buffer, and the `LatestEventValue` becomes `None` because there is
        // no other alternative.
        {
            let new_content = SerializableEventContent::new(&AnyMessageLikeEventContent::Reaction(
                ReactionEventContent::new(Annotation::new(
                    event_id!("$ev0").to_owned(),
                    "+1".to_owned(),
                )),
            ))
            .unwrap();

            let update = RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: transaction_id.clone(),
                new_content,
            };

            // The `LatestEventValue` has changed!
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::None
            );

            assert_eq!(buffer.buffer.len(), 0);
        }
    }

    #[async_test]
    async fn test_local_send_error() {
        let (_client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        {
            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_matches!(
                    LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                    LatestEventValue::LocalIsSending(
                        LatestEventContent::RoomMessage(message_content)
                    ) => {
                        assert_eq!(message_content.body(), body);
                    }
                );
            }

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` must change to indicate it's wedged.
        {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("oopsy".to_owned().into())),
                is_recoverable: true,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's marked as wedged.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsWedged(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(
                &buffer.buffer[0].1,
                LatestEventValue::LocalIsWedged(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "A");
                }
            );
            assert_matches!(
                &buffer.buffer[1].1,
                LatestEventValue::LocalIsWedged(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );
        }

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // must change: since an event has been sent, the following events are now
        // unwedged.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_0.clone(),
                event_id: event_id!("$ev0").to_owned(),
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's unwedged.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );

            assert_eq!(buffer.buffer.len(), 1);
            assert_matches!(
                &buffer.buffer[0].1,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );
        }
    }

    #[async_test]
    async fn test_local_retry_event() {
        let (_client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        {
            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_matches!(
                    LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                    LatestEventValue::LocalIsSending(
                        LatestEventContent::RoomMessage(message_content)
                    ) => {
                        assert_eq!(message_content.body(), body);
                    }
                );
            }

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` must change to indicate it's wedged.
        {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("oopsy".to_owned().into())),
                is_recoverable: true,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's marked as wedged.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsWedged(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(
                &buffer.buffer[0].1,
                LatestEventValue::LocalIsWedged(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "A");
                }
            );
            assert_matches!(
                &buffer.buffer[1].1,
                LatestEventValue::LocalIsWedged(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );
        }

        // Receiving a `RetryEvent` targeting the first event. The `LatestEventValue`
        // must change: this local event and its following must be unwedged.
        {
            let update =
                RoomSendQueueUpdate::RetryEvent { transaction_id: transaction_id_0.clone() };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's unwedged.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(
                &buffer.buffer[0].1,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "A");
                }
            );
            assert_matches!(
                &buffer.buffer[1].1,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "B");
                }
            );
        }
    }

    #[async_test]
    async fn test_local_media_upload() {
        let (_client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid");

        // Receiving a `NewLocalEvent`.
        {
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::LocalIsSending(
                    LatestEventContent::RoomMessage(message_content)
                ) => {
                    assert_eq!(message_content.body(), "A");
                }
            );

            assert_eq!(buffer.buffer.len(), 1);
        }

        // Receiving a `MediaUpload` targeting the first event. The
        // `LatestEventValue` must not change as `MediaUpload` are ignored.
        {
            let update = RoomSendQueueUpdate::MediaUpload {
                related_to: transaction_id,
                file: None,
                index: 0,
                progress: AbstractProgress { current: 0, total: 0 },
            };

            // The `LatestEventValue` has changed somehow, it tells no new
            // `LatestEventValue` is computed.
            assert_matches!(
                LatestEventValue::new_local(&update, &mut buffer, &room_event_cache, &None).await,
                LatestEventValue::None
            );

            assert_eq!(buffer.buffer.len(), 1);
        }
    }

    #[async_test]
    async fn test_local_fallbacks_to_remote_when_empty() {
        let room_id = room_id!("!r0");
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(room_id);
        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Prelude.
        {
            // Create the room.
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            // Initialise the event cache store.
            client
                .event_cache_store()
                .lock()
                .await
                .unwrap()
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![event_factory
                                .text_msg("hello")
                                .event_id(event_id_0)
                                .into()],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();

        assert_matches!(
            LatestEventValue::new_local(
                // An update that won't be create a new `LatestEventValue`.
                &RoomSendQueueUpdate::SentEvent {
                    transaction_id: OwnedTransactionId::from("txnid"),
                    event_id: event_id_1.to_owned(),
                },
                &mut buffer,
                &room_event_cache,
                &None,
            )
            .await,
            // We get a `Remote` because there is no `Local*` values!
            LatestEventValue::Remote(LatestEventContent::RoomMessage(message_content)) => {
                assert_eq!(message_content.body(), "hello");
            }
        );
    }
}
