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

use std::{
    iter::once,
    ops::{ControlFlow, Deref},
};

pub use matrix_sdk_base::latest_event::{LatestEventValue, LocalLatestEventValue};
use matrix_sdk_base::{deserialized_responses::TimelineEvent, store::SerializableEventContent};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, TransactionId, UserId,
    events::{
        AnyMessageLikeEventContent, AnySyncStateEvent, AnySyncTimelineEvent, SyncStateEvent,
        relation::Replacement,
        room::{
            member::MembershipState,
            message::{MessageType, Relation, RoomMessageEventContent},
            power_levels::RoomPowerLevels,
            redaction::RoomRedactionEventContent,
        },
    },
};
use tracing::error;

use crate::{event_cache::RoomEventCache, send_queue::RoomSendQueueUpdate};

/// A builder of [`LatestEventValue`]s.
pub(super) struct Builder;

impl Builder {
    /// Create a new [`LatestEventValue::Remote`].
    pub async fn new_remote(
        room_event_cache: &RoomEventCache,
        current_value_event_id: Option<OwnedEventId>,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) -> Option<LatestEventValue> {
        // If we are computing a value from the Event Cache, it's because we have
        // received an update from the Event Cache. This update falls in two categories:
        // either an event has been added or updated, or the room has been emptied (via
        // `EventCache::clear_all_rooms` for example).
        //
        // We consider the room has been emptied by default. If we are able to scan at
        // least one in-memory event, we consider the room has not been emptied.
        let mut room_has_been_emptied = true;
        let mut current_value_must_be_erased = false;

        if let Ok(Some(event)) = room_event_cache
            .rfind_map_event_in_memory_by(|event, previous_event| {
                // At least one event lives in-memory: we consider the room is not empty.
                room_has_been_emptied = false;

                match filter_timeline_event(
                    event,
                    previous_event,
                    current_value_event_id.as_ref(),
                    own_user_id,
                    power_levels,
                ) {
                    // Let's continue, event is not suitable.
                    ControlFlow::Continue(FilterContinue {
                        current_value_must_be_erased: erased,
                    }) => {
                        current_value_must_be_erased = erased;

                        None
                    }

                    // Stop! We found a suitable event!
                    ControlFlow::Break(()) => Some(event.clone()),
                }
            })
            .await
        {
            Some(LatestEventValue::Remote(event))
        } else {
            // When the room has been emptied, we must erase any previous value.
            if room_has_been_emptied {
                current_value_must_be_erased = true;
            }

            current_value_must_be_erased.then(LatestEventValue::default)
        }
    }

    /// Create a new [`LatestEventValue::LocalIsSending`] or
    /// [`LatestEventValue::LocalCannotBeSent`].
    pub async fn new_local(
        send_queue_update: &RoomSendQueueUpdate,
        buffer_of_values_for_local_events: &mut BufferOfValuesForLocalEvents,
        room_event_cache: &RoomEventCache,
        current_value_event_id: Option<OwnedEventId>,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) -> Option<LatestEventValue> {
        use crate::send_queue::{LocalEcho, LocalEchoContent};

        match send_queue_update {
            // A new local event is being sent.
            //
            // Let's create the `LatestEventValue` and push it in the buffer of values.
            RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id,
                content: local_echo_content,
            }) => match local_echo_content {
                LocalEchoContent::Event { serialized_event: serialized_event_content, .. } => {
                    Some(match serialized_event_content.deserialize() {
                        Ok(content) => {
                            if filter_any_message_like_event_content(
                                content,
                                None,
                                current_value_event_id.as_ref(),
                            )
                            .is_break()
                            {
                                let local_value = LocalLatestEventValue {
                                    timestamp: MilliSecondsSinceUnixEpoch::now(),
                                    content: serialized_event_content.clone(),
                                };

                                // If a local previous `LatestEventValue` exists and has been marked
                                // as “cannot be sent”, it means the new `LatestEventValue` must
                                // also be marked as “cannot be sent”.
                                let value =
                                    if let Some((_, LatestEventValue::LocalCannotBeSent(_))) =
                                        buffer_of_values_for_local_events.last()
                                    {
                                        LatestEventValue::LocalCannotBeSent(local_value)
                                    } else {
                                        LatestEventValue::LocalIsSending(local_value)
                                    };

                                buffer_of_values_for_local_events
                                    .push(transaction_id.to_owned(), value.clone());

                                value
                            } else {
                                return None;
                            }
                        }

                        Err(error) => {
                            error!(
                                ?error,
                                "Failed to deserialize an event from `RoomSendQueueUpdate::NewLocalEvent`"
                            );

                            return None;
                        }
                    })
                }

                LocalEchoContent::React { .. } => None,
            },

            // A local event has been cancelled before being sent.
            //
            // Remove the calculated `LatestEventValue` from the buffer of values, and return the
            // last `LatestEventValue` or calculate a new one.
            RoomSendQueueUpdate::CancelledLocalEvent { transaction_id } => {
                let or = if let Some(position) =
                    buffer_of_values_for_local_events.position(transaction_id)
                {
                    buffer_of_values_for_local_events.remove(position);

                    // We have cancelled a local value. If there is no more local values, and if
                    // there is no candidate for the Event Cache, we must generate some value, here
                    // `LatestEventValue::None`, to erase any existing value.
                    Some(LatestEventValue::None)
                } else {
                    None
                };

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    current_value_event_id,
                    own_user_id,
                    power_levels,
                )
                .await
                .or(or)
            }

            // A local event has successfully been sent!
            //
            // Mark all “cannot be sent” values as “is sending” after the one matching
            // `transaction_id`. Indeed, if an event has been sent, it means the send queue is
            // working, so if any value has been marked as “cannot be sent”, it must be marked as
            // “is sending”. Then, remove the calculated `LatestEventValue` from the buffer of
            // values. Finally, return the last `LatestEventValue` or calculate a new
            // one.
            RoomSendQueueUpdate::SentEvent { transaction_id, event_id } => {
                if let Some(position) =
                    buffer_of_values_for_local_events.mark_is_sending_after(transaction_id)
                {
                    let (_, value) = buffer_of_values_for_local_events.remove(position);

                    // The sent event is the last of the buffer. Let's compute a proper “detached”
                    // one if and only if the sent one was the last local one: not from the buffer,
                    // not from the Event Cache!.
                    if buffer_of_values_for_local_events.last().is_none() {
                        match value {
                            LatestEventValue::LocalIsSending(local_value)
                            | LatestEventValue::LocalCannotBeSent(local_value)
                            // Technically impossible, but it's not harmful to handle this that way.
                            | LatestEventValue::LocalHasBeenSent { value: local_value, .. } => {
                                return Some(LatestEventValue::LocalHasBeenSent { event_id: event_id.clone(), value: local_value });
                            }
                            LatestEventValue::Remote(_) | LatestEventValue::None => unreachable!("Impossible to get a remote `LatestEventValue`"),
                        }
                    }
                }

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    current_value_event_id,
                    own_user_id,
                    power_levels,
                )
                .await
            }

            // A local event has been replaced by another one.
            //
            // Replace the latest event value matching `transaction_id` in the buffer if it exists
            // (note: it should!), and return the last `LatestEventValue` or calculate a new one.
            RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id,
                new_content: new_serialized_event_content,
            } => {
                if let Some(position) = buffer_of_values_for_local_events.position(transaction_id) {
                    match new_serialized_event_content.deserialize() {
                        Ok(content) => {
                            if filter_any_message_like_event_content(
                                content,
                                None,
                                current_value_event_id.as_ref(),
                            )
                            .is_break()
                            {
                                buffer_of_values_for_local_events.replace_content(
                                    position,
                                    new_serialized_event_content.clone(),
                                );
                            } else {
                                buffer_of_values_for_local_events.remove(position);
                            }
                        }

                        Err(error) => {
                            error!(
                                ?error,
                                "Failed to deserialize an event from `RoomSendQueueUpdate::ReplacedLocalEvent`"
                            );

                            return None;
                        }
                    }
                }

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    current_value_event_id,
                    own_user_id,
                    power_levels,
                )
                .await
            }

            // An error has occurred.
            //
            // Mark the latest event value matching `transaction_id`, and all its following values,
            // as “cannot be sent” if the error isn't recoverable, and as "sending" if the error
            // was recoverable.
            RoomSendQueueUpdate::SendError { transaction_id, is_recoverable, .. } => {
                if *is_recoverable {
                    // If the room send queue error is recoverable, the send queue may retry to send
                    // it in a short while, so the event should still be considered
                    // sending. Leave it to that, at this point.
                    buffer_of_values_for_local_events.mark_is_sending_from(transaction_id);
                } else {
                    // If the error isn't recoverable, mark as a true "cannot be sent".
                    buffer_of_values_for_local_events.mark_cannot_be_sent_from(transaction_id);
                }

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    current_value_event_id,
                    own_user_id,
                    power_levels,
                )
                .await
            }

            // A local event has been unwedged and sending is being retried.
            //
            // Mark the latest event value matching `transaction_id`, and all its following values,
            // as “is sending”.
            RoomSendQueueUpdate::RetryEvent { transaction_id } => {
                buffer_of_values_for_local_events.mark_is_sending_from(transaction_id);

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    current_value_event_id,
                    own_user_id,
                    power_levels,
                )
                .await
            }

            // A media upload has made progress.
            //
            // Nothing to do here.
            RoomSendQueueUpdate::MediaUpload { .. } => None,
        }
    }

    /// Get the last [`LatestEventValue`] from the local latest event values if
    /// any, or create a new [`LatestEventValue`] from the remote events.
    ///
    /// If the buffer of latest event values is not empty, let's return the last
    /// one. Otherwise, it means we no longer have any local event: let's
    /// fallback on remote event!
    async fn new_local_or_remote(
        buffer_of_values_for_local_events: &mut BufferOfValuesForLocalEvents,
        room_event_cache: &RoomEventCache,
        current_value_event_id: Option<OwnedEventId>,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) -> Option<LatestEventValue> {
        if let Some((_, value)) = buffer_of_values_for_local_events.last() {
            Some(value.clone())
        } else {
            Self::new_remote(room_event_cache, current_value_event_id, own_user_id, power_levels)
                .await
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
/// all the following local events must also be marked as “cannot be sent”. And
/// vice versa, when the send queue is able to send an event again, all the
/// following local events must be marked as “is sending”.
///
/// This type isolates a couple of methods designed to manage these specific
/// behaviours.
#[derive(Debug)]
pub(super) struct BufferOfValuesForLocalEvents {
    buffer: Vec<(OwnedTransactionId, LatestEventValue)>,
}

impl BufferOfValuesForLocalEvents {
    /// Create a new [`BufferOfValuesForLocalEvents`].
    pub fn new() -> Self {
        Self { buffer: Vec::with_capacity(2) }
    }

    /// Check the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Get the last [`LatestEventValue`].
    fn last(&self) -> Option<&(OwnedTransactionId, LatestEventValue)> {
        self.buffer.last()
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
    /// [`LatestEventValue::LocalCannotBeSent`].
    fn push(&mut self, transaction_id: OwnedTransactionId, value: LatestEventValue) {
        assert!(value.is_local(), "`value` must be a local `LatestEventValue`");

        self.buffer.push((transaction_id, value));
    }

    /// Replace the content of the [`LatestEventValue`] at position `position`.
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - `position` is strictly greater than buffer's length,
    /// - the [`LatestEventValue`] is not of kind
    ///   [`LatestEventValue::LocalIsSending`] or
    ///   [`LatestEventValue::LocalCannotBeSent`].
    fn replace_content(&mut self, position: usize, new_content: SerializableEventContent) {
        let (_, value) = self.buffer.get_mut(position).expect("`position` must be valid");

        match value {
            LatestEventValue::LocalIsSending(LocalLatestEventValue { content, .. })
            | LatestEventValue::LocalCannotBeSent(LocalLatestEventValue { content, .. }) => {
                *content = new_content;
            }

            _ => panic!("`value` must be either `LocalIsSending` or `LocalCannotBeSent`"),
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
    /// following values, as “cannot be sent”.
    fn mark_cannot_be_sent_from(&mut self, transaction_id: &TransactionId) {
        let mut values = self.buffer.iter_mut();

        if let Some(first_value_to_wedge) = values
            .by_ref()
            .find(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over the found value and the following ones.
            for (_, value_to_wedge) in once(first_value_to_wedge).chain(values) {
                if let LatestEventValue::LocalIsSending(content) = value_to_wedge {
                    *value_to_wedge = LatestEventValue::LocalCannotBeSent(content.clone());
                }
            }
        }
    }

    /// Mark the `LatestEventValue` matching `transaction_id`, and all the
    /// following values, as “is sending”.
    fn mark_is_sending_from(&mut self, transaction_id: &TransactionId) {
        let mut values = self.buffer.iter_mut();

        if let Some(first_value_to_unwedge) = values
            .by_ref()
            .find(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over the found value and the following ones.
            for (_, value_to_unwedge) in once(first_value_to_unwedge).chain(values) {
                if let LatestEventValue::LocalCannotBeSent(content) = value_to_unwedge {
                    *value_to_unwedge = LatestEventValue::LocalIsSending(content.clone());
                }
            }
        }
    }

    /// Mark all the following values after the `LatestEventValue` matching
    /// `transaction_id` as “is sending”.
    ///
    /// Note that contrary to [`Self::mark_is_sending_from`], the
    /// `LatestEventValue` is untouched. However, its position is returned
    /// (if any).
    fn mark_is_sending_after(&mut self, transaction_id: &TransactionId) -> Option<usize> {
        let mut values = self.buffer.iter_mut();

        if let Some(position) = values
            .by_ref()
            .position(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over all values after the found one.
            for (_, value_to_unwedge) in values {
                if let LatestEventValue::LocalCannotBeSent(content) = value_to_unwedge {
                    *value_to_unwedge = LatestEventValue::LocalIsSending(content.clone());
                }
            }

            Some(position)
        } else {
            None
        }
    }
}

/// The [`ControlFlow::Continue`] value used by the filters.
#[derive(Debug)]
struct FilterContinue {
    /// Whether the current [`LatestEventValue`] must be erased or not.
    current_value_must_be_erased: bool,
}

/// Build the [`ControlFlow::Break`] for the filters.
fn filter_break() -> ControlFlow<(), FilterContinue> {
    ControlFlow::Break(())
}

/// Build the [`ControlFlow::Continue`] for the filters.
fn filter_continue() -> ControlFlow<(), FilterContinue> {
    ControlFlow::Continue(FilterContinue { current_value_must_be_erased: false })
}

/// Build the [`ControlFlow::Continue`] with erasing, for the filters.
fn filter_continue_with_erasing() -> ControlFlow<(), FilterContinue> {
    ControlFlow::Continue(FilterContinue { current_value_must_be_erased: true })
}

/// Filter a [`TimelineEvent`].
///
/// Be careful:
///
/// - `event` is the current event in the collection of events that is scanned.
/// - `previous_event` is the event sitting next to `event` in this collection,
///   it's the event that comes before `event` (`previous_event` is older than
///   `event`).
/// - `current_value_event_id` is the event ID of the current
///   [`LatestEventValue`].
fn filter_timeline_event(
    event: &TimelineEvent,
    previous_event: Option<&TimelineEvent>,
    current_value_event_id: Option<&OwnedEventId>,
    own_user_id: &UserId,
    power_levels: Option<&RoomPowerLevels>,
) -> ControlFlow<(), FilterContinue> {
    // Cast the event into an `AnySyncTimelineEvent`. If deserializing fails, we
    // ignore the event.
    let event = match event.raw().deserialize() {
        Ok(event) => event,
        Err(error) => {
            error!(
                ?error,
                "Failed to deserialize the event when looking for a suitable latest event"
            );

            return filter_continue();
        }
    };

    match event {
        AnySyncTimelineEvent::MessageLike(message_like_event) => {
            match message_like_event.original_content() {
                Some(any_message_like_event_content) => filter_any_message_like_event_content(
                    any_message_like_event_content,
                    previous_event,
                    current_value_event_id,
                ),

                // The event has been redacted.
                None => filter_continue(),
            }
        }

        AnySyncTimelineEvent::State(state) => {
            filter_any_sync_state_event(state, own_user_id, power_levels)
        }
    }
}

fn filter_any_message_like_event_content(
    event: AnyMessageLikeEventContent,
    previous_event: Option<&TimelineEvent>,
    current_value_event_id: Option<&OwnedEventId>,
) -> ControlFlow<(), FilterContinue> {
    match event {
        // `m.room.message`
        AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent {
            msgtype,
            relates_to,
            ..
        }) => {
            // Don't show incoming verification requests.
            if let MessageType::VerificationRequest(_) = msgtype {
                return filter_continue();
            }

            // Not all relations are accepted. Let's filter them.
            match relates_to {
                Some(Relation::Replacement(Replacement { event_id, .. })) => {
                    // If the edit relates to the immediate previous event, this is an acceptable
                    // latest event candidate, otherwise let's ignore it.
                    if Some(event_id) == previous_event.and_then(|event| event.event_id()) {
                        filter_break()
                    } else {
                        filter_continue()
                    }
                }

                _ => filter_break(),
            }
        }

        // `org.matrix.msc3381.poll.start`
        // `m.call.invite`
        // `m.rtc.notification`
        // `m.sticker`
        AnyMessageLikeEventContent::UnstablePollStart(_)
        | AnyMessageLikeEventContent::CallInvite(_)
        | AnyMessageLikeEventContent::RtcNotification(_)
        | AnyMessageLikeEventContent::Sticker(_) => filter_break(),

        // `m.room.redaction`
        AnyMessageLikeEventContent::RoomRedaction(RoomRedactionEventContent {
            redacts, ..
        }) => {
            // A redaction is not suitable.
            //
            // However, this redaction targets the current `LatestEventValue`! It means the
            // current value must be erased.
            if redacts.as_ref() == current_value_event_id {
                filter_continue_with_erasing()
            } else {
                filter_continue()
            }
        }

        // `m.room.encrypted`
        AnyMessageLikeEventContent::RoomEncrypted(_) => {
            // **explicitly** not suitable.
            filter_continue()
        }

        // Everything else is considered not suitable.
        _ => filter_continue(),
    }
}

fn filter_any_sync_state_event(
    event: AnySyncStateEvent,
    own_user_id: &UserId,
    power_levels: Option<&RoomPowerLevels>,
) -> ControlFlow<(), FilterContinue> {
    match event {
        AnySyncStateEvent::RoomMember(member) => {
            match member.membership() {
                MembershipState::Knock => {
                    let can_accept_or_decline_knocks = match power_levels {
                        Some(room_power_levels) => {
                            room_power_levels.user_can_invite(own_user_id)
                                || room_power_levels
                                    .user_can_kick_user(own_user_id, member.state_key())
                        }
                        None => false,
                    };

                    // The current user can act on the knock changes, so they should be
                    // displayed
                    if can_accept_or_decline_knocks {
                        // We can only decide whether the user can accept or decline knocks if the
                        // event isn't redacted.
                        return if matches!(member, SyncStateEvent::Original(_)) {
                            filter_break()
                        } else {
                            filter_continue()
                        };
                    }

                    filter_continue()
                }

                // A member is joining or is being invited…
                MembershipState::Join | MembershipState::Invite => {
                    // This member _is_ the current user, not someone else! This is a valid state
                    // event:
                    //
                    // - the user is joining a room: we want a `LatestEventValue` to get a first
                    //   value!
                    // - the user is being invited: we want a `LatestEventValue` to represent the
                    //   invitation!
                    match member {
                        // We can only decide whether the user is joining or is being invited if the
                        // event isn't redacted.
                        SyncStateEvent::Original(state) => {
                            if state.state_key.deref() == own_user_id {
                                filter_break()
                            } else {
                                filter_continue()
                            }
                        }

                        _ => filter_continue(),
                    }
                }

                _ => filter_continue(),
            }
        }

        _ => filter_continue(),
    }
}

#[cfg(test)]
mod filter_tests {
    use std::ops::Not;

    use assert_matches::assert_matches;
    use matrix_sdk_test::event_factory::EventFactory;
    use ruma::{
        event_id,
        events::{room::message::RoomMessageEventContent, rtc::notification::NotificationType},
        owned_user_id, user_id,
    };

    use super::{ControlFlow, FilterContinue, filter_timeline_event};

    macro_rules! assert_latest_event_content {
        ( event | $event_factory:ident | $event_builder:block
          is a candidate ) => {
            assert_latest_event_content!(@_ | $event_factory | $event_builder, ControlFlow::Break(_));
        };

        ( event | $event_factory:ident | $event_builder:block
          is not a candidate ) => {
            assert_latest_event_content!(@_ | $event_factory | $event_builder, ControlFlow::Continue(_));
        };

        ( @_ | $event_factory:ident | $event_builder:block, $expect:pat) => {
            let user_id = user_id!("@mnt_io:matrix.org");
            let event_factory = EventFactory::new().sender(user_id);
            let event = {
                let $event_factory = event_factory;
                $event_builder
            };

            assert_matches!(
                filter_timeline_event(&event, None, None, user_id!("@mnt_io:matrix.org"), None),
                $expect
            );
        };
    }

    #[test]
    fn test_room_message() {
        assert_latest_event_content!(
            event | event_factory | { event_factory.text_msg("hello").into_event() }
            is a candidate
        );
    }

    #[test]
    fn test_room_message_replacement() {
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id);
        let event = event_factory
            .text_msg("bonjour")
            .edit(event_id!("$ev0"), RoomMessageEventContent::text_plain("hello").into())
            .into_event();

        // Without a previous event.
        //
        // This is an edge case where either the event cache has been emptied and only
        // the edit is received via the sync for example, or either the previous event
        // is part of another chunk that is not loaded in memory yet. In this case,
        // let's not consider the event as a `LatestEventValue` candidate.
        {
            let previous_event = None;

            assert!(
                filter_timeline_event(&event, previous_event, None, user_id, None).is_continue()
            );
        }

        // With a previous event, but not the one being replaced.
        {
            let previous_event =
                Some(event_factory.text_msg("no!").event_id(event_id!("$ev1")).into_event());

            assert!(
                filter_timeline_event(&event, previous_event.as_ref(), None, user_id, None)
                    .is_continue()
            );
        }

        // With a previous event, and that's the one being replaced!
        {
            let previous_event =
                Some(event_factory.text_msg("hello").event_id(event_id!("$ev0")).into_event());

            assert!(
                filter_timeline_event(&event, previous_event.as_ref(), None, user_id, None)
                    .is_break()
            );
        }
    }

    #[test]
    fn test_redaction() {
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id);
        let event_id = event_id!("$ev0");
        let event = event_factory.redaction(event_id).into_event();

        // `current_value_event_id` is `None`, it cannot be used to decide whether the
        // current value must be erased.
        {
            let current_value_event_id = None;

            assert_matches!(
                filter_timeline_event(&event, None, current_value_event_id, user_id, None),
                ControlFlow::Continue(FilterContinue { current_value_must_be_erased }) => {
                    assert!(current_value_must_be_erased.not());
                }
            );
        }

        // `current_value_event_id` is `Some(_)`, but the redaction event doesn't target
        // this event ID.
        {
            let current_value_event_id = Some(event_id!("$ev1").to_owned());

            assert_matches!(
                filter_timeline_event(&event, None, current_value_event_id.as_ref(), user_id, None),
                ControlFlow::Continue(FilterContinue { current_value_must_be_erased }) => {
                    assert!(current_value_must_be_erased.not());
                }
            );
        }

        // `current_value_event_id` is `Some(_)`, and the redaction event does target
        // this event ID: great, the current value must be erased!
        {
            let current_value_event_id = Some(event_id.to_owned());

            assert_matches!(
                filter_timeline_event(&event, None, current_value_event_id.as_ref(), user_id, None),
                ControlFlow::Continue(FilterContinue { current_value_must_be_erased }) => {
                    assert!(current_value_must_be_erased);
                }
            );
        }
    }

    #[test]
    fn test_redacted() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .redacted(
                        user_id!("@mnt_io:matrix.org"),
                        ruma::events::room::message::RedactedRoomMessageEventContent::new(),
                    )
                    .event_id(event_id!("$ev0"))
                    .into_event()
            }
            is not a candidate
        );
    }

    #[test]
    fn test_poll() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .poll_start("the people need to know", "comté > gruyère", vec!["yes", "oui"])
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_call_invite() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .call_invite(
                        ruma::OwnedVoipId::from("vvooiipp".to_owned()),
                        ruma::UInt::from(1234u32),
                        ruma::events::call::SessionDescription::new(
                            "type".to_owned(),
                            "sdp".to_owned(),
                        ),
                        ruma::VoipVersionId::V1,
                    )
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_rtc_notification() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                     .rtc_notification(
                        NotificationType::Ring,
                    )
                    .mentions(vec![owned_user_id!("@alice:server.name")])
                    .relates_to_membership_state_event(ruma::OwnedEventId::try_from("$abc:server.name").unwrap())
                    .lifetime(60)
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_sticker() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .sticker(
                        "wink wink",
                        ruma::events::room::ImageInfo::new(),
                        ruma::OwnedMxcUri::from("mxc://foo/bar"),
                    )
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_encrypted_room_message() {
        assert_latest_event_content!(
            event | event_factory | {
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
            is not a candidate
        );
    }

    #[test]
    fn test_reaction() {
        // Take a random message-like event.
        assert_latest_event_content!(
            event | event_factory | { event_factory.reaction(event_id!("$ev0"), "+1").into_event() }
            is not a candidate
        );
    }

    #[test]
    fn test_state_event() {
        assert_latest_event_content!(
            event | event_factory | { event_factory.room_topic("new room topic").into_event() }
            is not a candidate
        );
    }

    #[test]
    fn test_knocked_state_event_without_power_levels() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .member(user_id!("@other_mnt_io:server.name"))
                    .membership(ruma::events::room::member::MembershipState::Knock)
                    .into_event()
            }
            is not a candidate
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
        room_power_levels.users.insert(user_id.to_owned(), 5.into());
        room_power_levels.users.insert(other_user_id.to_owned(), 4.into());

        // Cannot accept. Cannot decline.
        {
            room_power_levels.invite = 10.into();
            room_power_levels.kick = 10.into();
            assert!(
                filter_timeline_event(&event, None, None, user_id, Some(&room_power_levels))
                    .is_continue(),
                "cannot accept, cannot decline",
            );
        }

        // Can accept. Cannot decline.
        {
            room_power_levels.invite = 0.into();
            room_power_levels.kick = 10.into();
            assert!(
                filter_timeline_event(&event, None, None, user_id, Some(&room_power_levels))
                    .is_break(),
                "can accept, cannot decline",
            );
        }

        // Cannot accept. Can decline.
        {
            room_power_levels.invite = 10.into();
            room_power_levels.kick = 0.into();
            assert!(
                filter_timeline_event(&event, None, None, user_id, Some(&room_power_levels))
                    .is_break(),
                "cannot accept, can decline",
            );
        }

        // Can accept. Can decline.
        {
            room_power_levels.invite = 0.into();
            room_power_levels.kick = 0.into();
            assert!(
                filter_timeline_event(&event, None, None, user_id, Some(&room_power_levels))
                    .is_break(),
                "can accept, can decline",
            );
        }

        // Cannot accept. Can decline. But with an other user ID with at least the same
        // levels, i.e. the current user cannot kick another user with the same
        // or higher levels.
        {
            room_power_levels.users.insert(user_id.to_owned(), 5.into());
            room_power_levels.users.insert(other_user_id.to_owned(), 5.into());

            room_power_levels.invite = 10.into();
            room_power_levels.kick = 0.into();

            assert!(
                filter_timeline_event(&event, None, None, user_id, Some(&room_power_levels))
                    .is_continue(),
                "cannot accept, can decline, at least same user levels",
            );
        }
    }

    #[test]
    fn test_join_state_event() {
        use ruma::events::room::member::MembershipState;

        // The current user is joining the room.
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .member(user_id!("@mnt_io:matrix.org"))
                    .membership(MembershipState::Join)
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_join_state_event_for_someone_else() {
        use ruma::events::room::member::MembershipState;

        // The current user sees a join but for someone else.
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .member(user_id!("@other_mnt_io:server.name"))
                    .membership(MembershipState::Join)
                    .into_event()
            }
            is not a candidate
        );
    }

    #[test]
    fn test_invite_state_event() {
        use ruma::events::room::member::MembershipState;

        // The current user is receiving an invite.
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .member(user_id!("@mnt_io:matrix.org"))
                    .membership(MembershipState::Invite)
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_invite_state_event_for_someone_else() {
        use ruma::events::room::member::MembershipState;

        // The current user sees an invite but for someone else.
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .member(user_id!("@other_mnt_io:server.name"))
                    .membership(MembershipState::Invite)
                    .into_event()
            }
            is not a candidate
        );
    }

    #[test]
    fn test_room_message_verification_request() {
        use ruma::{OwnedDeviceId, events::room::message};

        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .event(RoomMessageEventContent::new(message::MessageType::VerificationRequest(
                        message::KeyVerificationRequestEventContent::new(
                            "body".to_owned(),
                            vec![],
                            OwnedDeviceId::from("device_id"),
                            user_id!("@user:server.name").to_owned(),
                        ),
                    )))
                    .into_event()
            }
            is not a candidate
        );
    }
}

#[cfg(test)]
mod buffer_of_values_for_local_event_tests {
    use assert_matches::assert_matches;
    use ruma::{
        OwnedTransactionId,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        owned_event_id,
        serde::Raw,
    };
    use serde_json::json;

    use super::{
        super::{super::local_room_message, RemoteLatestEventValue},
        BufferOfValuesForLocalEvents, LatestEventValue, LocalLatestEventValue,
    };

    fn remote_room_message(body: &str) -> RemoteLatestEventValue {
        RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain(body),
                    "type": "m.room.message",
                    "event_id": "$ev0",
                    "origin_server_ts": 42,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn test_last() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        assert!(buffer.last().is_none());

        let transaction_id = OwnedTransactionId::from("txnid");
        buffer.push(
            transaction_id.clone(),
            LatestEventValue::LocalIsSending(local_room_message("tome")),
        );

        assert_matches!(
            buffer.last(),
            Some((expected_transaction_id, LatestEventValue::LocalIsSending(_))) => {
                assert_eq!(expected_transaction_id, &transaction_id);
            }
        );
    }

    #[test]
    fn test_position() {
        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid");

        assert!(buffer.position(&transaction_id).is_none());

        buffer.push(
            transaction_id.clone(),
            LatestEventValue::LocalIsSending(local_room_message("raclette")),
        );
        buffer.push(
            OwnedTransactionId::from("othertxnid"),
            LatestEventValue::LocalIsSending(local_room_message("tome")),
        );

        assert_eq!(buffer.position(&transaction_id), Some(0));
    }

    #[test]
    #[should_panic]
    fn test_push_none() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        buffer.push(OwnedTransactionId::from("txnid"), LatestEventValue::None);
    }

    #[test]
    #[should_panic]
    fn test_push_remote() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::Remote(remote_room_message("tome")),
        );
    }

    #[test]
    fn test_push_local() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid0"),
            LatestEventValue::LocalIsSending(local_room_message("tome")),
        );
        buffer.push(
            OwnedTransactionId::from("txnid1"),
            LatestEventValue::LocalCannotBeSent(local_room_message("raclette")),
        );
        buffer.push(
            OwnedTransactionId::from("txnid1"),
            LatestEventValue::LocalHasBeenSent {
                event_id: owned_event_id!("$ev0"),
                value: local_room_message("raclette"),
            },
        );

        // no panic.
    }

    #[test]
    #[should_panic]
    fn test_replace_content_on_remote() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::Remote(remote_room_message("gruyère")),
        );

        let LocalLatestEventValue { content: new_content, .. } = local_room_message("comté");

        buffer.replace_content(0, new_content);
    }

    #[test]
    #[should_panic]
    fn test_replace_content_on_local_has_been_sent() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::LocalHasBeenSent {
                event_id: owned_event_id!("$ev0"),
                value: local_room_message("gruyère"),
            },
        );

        let LocalLatestEventValue { content: new_content, .. } = local_room_message("comté");

        buffer.replace_content(0, new_content);
    }

    #[test]
    fn test_replace_content_on_local_is_sending() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        let transaction_id = OwnedTransactionId::from("txnid0");
        buffer.push(
            transaction_id.clone(),
            LatestEventValue::LocalIsSending(local_room_message("gruyère")),
        );

        let LocalLatestEventValue { content: new_content, .. } = local_room_message("comté");

        buffer.replace_content(0, new_content);

        assert_matches!(
            buffer.last(),
            Some((expected_transaction_id, LatestEventValue::LocalIsSending(local_event))) => {
                assert_eq!(expected_transaction_id, &transaction_id);
                assert_matches!(
                    local_event.content.deserialize().unwrap(),
                    AnyMessageLikeEventContent::RoomMessage(content) => {
                        assert_eq!(content.body(), "comté");
                    }
                );
            }
        );
    }

    #[test]
    fn test_replace_content_on_local_cannot_be_sent() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        let transaction_id = OwnedTransactionId::from("txnid0");
        buffer.push(
            transaction_id.clone(),
            LatestEventValue::LocalCannotBeSent(local_room_message("gruyère")),
        );

        let LocalLatestEventValue { content: new_content, .. } = local_room_message("comté");

        buffer.replace_content(0, new_content);

        assert_matches!(
            buffer.last(),
            Some((expected_transaction_id, LatestEventValue::LocalCannotBeSent(local_event))) => {
                assert_eq!(expected_transaction_id, &transaction_id);
                assert_matches!(
                    local_event.content.deserialize().unwrap(),
                    AnyMessageLikeEventContent::RoomMessage(content) => {
                        assert_eq!(content.body(), "comté");
                    }
                );
            }
        );
    }

    #[test]
    fn test_remove() {
        let mut buffer = BufferOfValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::LocalIsSending(local_room_message("gryuère")),
        );

        assert!(buffer.last().is_some());

        buffer.remove(0);

        assert!(buffer.last().is_none());
    }

    #[test]
    fn test_mark_cannot_be_sent_from() {
        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(
            transaction_id_0,
            LatestEventValue::LocalIsSending(local_room_message("gruyère")),
        );
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalIsSending(local_room_message("brigand")),
        );
        buffer.push(
            transaction_id_2,
            LatestEventValue::LocalIsSending(local_room_message("raclette")),
        );

        buffer.mark_cannot_be_sent_from(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalCannotBeSent(_));
    }

    #[test]
    fn test_mark_is_sending_from() {
        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(
            transaction_id_0,
            LatestEventValue::LocalCannotBeSent(local_room_message("gruyère")),
        );
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalCannotBeSent(local_room_message("brigand")),
        );
        buffer.push(
            transaction_id_2,
            LatestEventValue::LocalCannotBeSent(local_room_message("raclette")),
        );

        buffer.mark_is_sending_from(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalIsSending(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalIsSending(_));
    }

    #[test]
    fn test_mark_is_sending_after() {
        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(
            transaction_id_0,
            LatestEventValue::LocalCannotBeSent(local_room_message("gruyère")),
        );
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalCannotBeSent(local_room_message("brigand")),
        );
        buffer.push(
            transaction_id_2,
            LatestEventValue::LocalCannotBeSent(local_room_message("raclette")),
        );

        buffer.mark_is_sending_after(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalIsSending(_));
    }
}

#[cfg(all(not(target_family = "wasm"), test))]
mod builder_tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        RoomState,
        deserialized_responses::TimelineEventKind,
        linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
        store::{ChildTransactionId, SerializableEventContent},
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        EventId, MilliSecondsSinceUnixEpoch, OwnedRoomId, OwnedTransactionId, event_id,
        events::{
            AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            SyncMessageLikeEvent, reaction::ReactionEventContent, relation::Annotation,
            room::message::RoomMessageEventContent,
        },
        room_id,
        serde::Raw,
        user_id,
    };
    use serde_json::json;

    use super::{
        super::RemoteLatestEventValue, BufferOfValuesForLocalEvents, Builder, LatestEventValue,
        RoomEventCache, RoomSendQueueUpdate,
    };
    use crate::{
        Client, Error,
        send_queue::{
            AbstractProgress, LocalEcho, LocalEchoContent, RoomSendQueue, SendHandle,
            SendReactionHandle,
        },
        test_utils::mocks::MatrixMockServer,
    };

    macro_rules! assert_remote_value_matches_room_message_with_body {
        ( $latest_event_value:expr => with body = $body:expr ) => {{
            let latest_event_value = $latest_event_value;

            assert_matches!(
                latest_event_value,
                Some(LatestEventValue::Remote(RemoteLatestEventValue { kind: TimelineEventKind::PlainText { event: ref event }, .. })) => {
                    assert_matches!(
                        event.deserialize().unwrap(),
                        AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomMessage(
                                SyncMessageLikeEvent::Original(message_content)
                            )
                        ) => {
                            assert_eq!(message_content.content.body(), $body);
                        }
                    );

                    latest_event_value.unwrap()
                }
            )
        }};
    }

    macro_rules! assert_local_value_matches_room_message_with_body {
        ( $latest_event_value:expr, $pattern:path => with body = $body:expr ) => {{
            let latest_event_value = $latest_event_value;

            assert_matches!(
                latest_event_value,
                Some( $pattern (ref local_event)) => {
                    assert_matches!(
                        local_event.content.deserialize().unwrap(),
                        AnyMessageLikeEventContent::RoomMessage(message_content) => {
                            assert_eq!(message_content.body(), $body);
                        }
                    );

                    latest_event_value.unwrap()
                }
            )
        }};

        ( $latest_event_value:expr, $pattern:path {
            $local_value:ident with body = $body:expr
            $( , $field:ident => $more:block )*
        } ) => {{
            let latest_event_value = $latest_event_value;

            assert_matches!(
                latest_event_value,
                Some( $pattern { ref $local_value, $( ref $field, )* .. }) => {
                    assert_matches!(
                        $local_value .content.deserialize().unwrap(),
                        AnyMessageLikeEventContent::RoomMessage(message_content) => {
                            assert_eq!(message_content.body(), $body);

                            $({
                                let $field = $field;
                                $more
                            })*
                        }
                    );

                    latest_event_value.unwrap()
                }
            )
        }};
    }

    fn remote_room_message(event_id: &EventId, body: &str) -> RemoteLatestEventValue {
        RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain(body),
                    "type": "m.room.message",
                    "event_id": event_id,
                    "origin_server_ts": 42,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        )
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
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
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

        assert_remote_value_matches_room_message_with_body!(
            // We get `event_id_1` because `event_id_2` isn't a candidate,
            // and `event_id_0` hasn't been read yet (because events are read
            // backwards).
            Builder::new_remote(&room_event_cache, None, user_id, None).await => with body = "world"
        );
    }

    #[async_test]
    async fn test_remote_without_a_candidate() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let event_factory = EventFactory::new().sender(user_id).room(room_id);

        let room = client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // Insert an non-suitable candidate.
        client
            .event_cache_store()
            .lock()
            .await
            .expect("Could not acquire the event cache lock")
            .as_clean()
            .expect("Could not acquire a clean event cache lock")
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
                        items: vec![event_factory.room_topic("new room topic").into()],
                    },
                ],
            )
            .await
            .unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        // Check initial state.
        let current_value = {
            let value = room.latest_event();

            assert_matches!(value, LatestEventValue::None);

            value
        };

        // Compute a new remote value: not able to find a relevant candidate.
        //
        // No candidate is found, so it's just `None` here.
        assert_matches!(
            Builder::new_remote(&room_event_cache, current_value.event_id(), user_id, None,).await,
            None
        );
    }

    #[async_test]
    async fn test_remote_with_a_candidate() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let event_factory = EventFactory::new().sender(user_id).room(room_id);

        let room = client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // Insert a suitable candidate.
        client
            .event_cache_store()
            .lock()
            .await
            .expect("Could not acquire the event cache lock")
            .as_clean()
            .expect("Could not acquire a clean event cache lock")
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
                            event_factory.text_msg("hello").event_id(event_id!("$ev0")).into(),
                        ],
                    },
                ],
            )
            .await
            .unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        // Check initial state.
        let current_value = {
            let value = room.latest_event();

            assert_matches!(value, LatestEventValue::None);

            value
        };

        // Compute a new remote value: will be able to find a relevant
        // candidate.
        //
        // A candidate is found, so it's a `Some(LatestEventValue::Remote)`
        // that is returned! Let's check the event.
        assert_remote_value_matches_room_message_with_body!(
            Builder::new_remote(&room_event_cache, current_value.event_id(), user_id, None).await => with body = "hello"
        );
    }

    #[async_test]
    async fn test_remote_without_a_candidate_but_with_an_existing_latest_event_value() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let event_factory = EventFactory::new().sender(user_id).room(room_id);

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // Insert a non-suitable candidate.
        client
            .event_cache_store()
            .lock()
            .await
            .expect("Could not acquire the event cache lock")
            .as_clean()
            .expect("Could not acquire a clean event cache lock")
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![event_factory.room_topic("new room topic").into()],
                    },
                ],
            )
            .await
            .unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        // Initial state.
        let current_value =
            LatestEventValue::Remote(remote_room_message(event_id!("$ev0"), "hello"));

        // Compute a new remote value: with no candidate.
        //
        // No candidate is found, so it's just a `None` here.
        assert_matches!(
            Builder::new_remote(&room_event_cache, current_value.event_id(), user_id, None).await,
            None
        );
    }

    #[async_test]
    async fn test_remote_without_a_candidate_but_with_an_erasable_existing_latest_event_value() {
        let room_id = room_id!("!r0");
        let event_id = event_id!("$ev0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let event_factory = EventFactory::new().sender(user_id).room(room_id);

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // Insert a non-suitable candidate.
        client
            .event_cache_store()
            .lock()
            .await
            .expect("Could not acquire the event cache lock")
            .as_clean()
            .expect("Could not acquire a clean event cache lock")
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![event_factory.redaction(event_id).into()],
                    },
                ],
            )
            .await
            .unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        // Initial state.
        let current_value = LatestEventValue::Remote(remote_room_message(event_id, "hello"));

        // Compute a new remote value: with no candidate, but it's a `m.room.redaction`
        // that erases `current_value`!
        //
        // No candidate is found, so it SHOULD BE a `None`, but since we found a
        // `m.room.redaction` targeting our `current_value`, it MUST BE a
        // `Some(LatestEventValue::None)` to erase it.
        assert_matches!(
            Builder::new_remote(&room_event_cache, current_value.event_id(), user_id, None).await,
            Some(LatestEventValue::None)
        );
    }

    #[async_test]
    async fn test_remote_with_a_candidate_and_an_erasable_existing_latest_event_value() {
        let room_id = room_id!("!r0");
        let event_id = event_id!("$ev0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let event_factory = EventFactory::new().sender(user_id).room(room_id);

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // Insert a non-suitable candidate.
        client
            .event_cache_store()
            .lock()
            .await
            .expect("Could not acquire the event cache lock")
            .as_clean()
            .expect("Could not acquire a clean event cache lock")
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![
                            event_factory.redaction(event_id).into(),
                            event_factory.text_msg("world").into(),
                        ],
                    },
                ],
            )
            .await
            .unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        // Initial state.
        let current_value = LatestEventValue::Remote(remote_room_message(event_id, "hello"));

        // Compute a new remote value: with a candidate, and a `m.room.redaction`
        // that erases `current_value`!
        //
        // A candidate is found, so it MUST BE a
        // `Some(LatestEventValue::Remote(_))`, whatever the `current_value`
        // is.
        assert_remote_value_matches_room_message_with_body!(
            Builder::new_remote(&room_event_cache, current_value.event_id(), user_id, None).await => with body = "world"
        );
    }

    #[async_test]
    async fn test_remote_when_room_has_been_emptied() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let event_factory = EventFactory::new().sender(user_id).room(room_id);

        let room = client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // Insert a suitable candidate.
        client
            .event_cache_store()
            .lock()
            .await
            .expect("Could not acquire the event cache lock")
            .as_clean()
            .expect("Could not acquire a clean event cache lock")
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
                            event_factory.text_msg("hello").event_id(event_id!("$ev0")).into(),
                        ],
                    },
                ],
            )
            .await
            .unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        // Check initial state.
        let current_value = {
            let value = room.latest_event();

            assert_matches!(value, LatestEventValue::None);

            value
        };

        // Compute a new remote value: will be able to find a relevant
        // candidate.
        let current_value = assert_remote_value_matches_room_message_with_body!(
            Builder::new_remote(&room_event_cache, current_value.event_id(), user_id, None).await => with body = "hello"
        );

        // Now, let's clear all rooms.
        event_cache.clear_all_rooms().await.unwrap();

        // The latest event has been erased!
        assert_matches!(
            Builder::new_remote(&room_event_cache, current_value.event_id(), user_id, None).await,
            Some(LatestEventValue::None)
        );
    }

    async fn local_prelude() -> (Client, OwnedRoomId, RoomSendQueue, RoomEventCache) {
        let room_id = room_id!("!r0").to_owned();

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        client.base_client().get_or_create_room(&room_id, RoomState::Joined);
        let room = client.get_room(&room_id).unwrap();
        let user_id = client.user_id().unwrap();
        let event_factory = EventFactory::new().sender(user_id).room(&room_id);

        // Insert a non-suitable candidate.
        client
            .event_cache_store()
            .lock()
            .await
            .expect("Could not acquire the event cache lock")
            .as_clean()
            .expect("Could not acquire a clean event cache lock")
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(&room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![event_factory.room_topic("new room topic").into()],
                    },
                ],
            )
            .await
            .unwrap();

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
    async fn test_local_new_local_event_with_content_of_kind_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();

        // Receiving one `NewLocalEvent`.
        let previous_value = {
            let transaction_id = OwnedTransactionId::from("txnid0");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, None, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            )
        };

        // Receiving another `NewLocalEvent`, ensuring it's pushed back in the buffer.
        {
            let transaction_id = OwnedTransactionId::from("txnid1");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "B");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );
        }

        assert_eq!(buffer.buffer.len(), 2);
    }

    #[async_test]
    async fn test_local_new_local_event_with_content_of_kind_event_when_previous_local_event_cannot_be_sent()
     {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();

        // Receiving one `NewLocalEvent`.
        let (transaction_id_0, previous_value) = {
            let transaction_id = OwnedTransactionId::from("txnid0");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, None, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            (transaction_id, value)
        };

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` must change to indicate it “cannot be sent”, because the
        // error is unrecoverable.
        let previous_value = {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("oopsy".to_owned().into())),
                is_recoverable: false,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's marked as “cannot be sent”.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalCannotBeSent => with body = "A"
            );

            assert_eq!(buffer.buffer.len(), 1);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));

            value
        };

        // Receiving another `NewLocalEvent`, ensuring it's pushed back in the buffer,
        // and as a `LocalCannotBeSent` because the previous value is itself
        // `LocalCannotBeSent`.
        {
            let transaction_id = OwnedTransactionId::from("txnid1");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "B");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalCannotBeSent => with body = "B"
            );
        }

        assert_eq!(buffer.buffer.len(), 2);
        assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));
    }

    #[async_test]
    async fn test_local_new_local_event_with_content_of_kind_react() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();

        // Receiving one `NewLocalEvent` with content of kind event.
        let (transaction_id_0, previous_value) = {
            let transaction_id = OwnedTransactionId::from("txnid0");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, None, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            (transaction_id, value)
        };

        // Receiving one `NewLocalEvent` with content of kind react! This time, it is
        // ignored.
        {
            let transaction_id = OwnedTransactionId::from("txnid1");
            let content = LocalEchoContent::React {
                key: "<< 1".to_owned(),
                send_handle: SendReactionHandle::new(
                    room_send_queue.clone(),
                    ChildTransactionId::new(),
                ),
                applies_to: transaction_id_0,
            };

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the previous local event.
            assert_matches!(
                Builder::new_local(
                    &update,
                    &mut buffer,
                    &room_event_cache,
                    previous_value.event_id(),
                    user_id,
                    None
                )
                .await,
                None
            )
        }

        assert_eq!(buffer.buffer.len(), 1);
    }

    #[async_test]
    async fn test_local_cancelled_local_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        // Receiving three `NewLocalEvent`s.
        let previous_value = {
            let mut value = None;

            for (transaction_id, body) in
                [(&transaction_id_0, "A"), (&transaction_id_1, "B"), (&transaction_id_2, "C")]
            {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                value = Some(assert_local_value_matches_room_message_with_body!(
                    Builder::new_local(&update, &mut buffer, &room_event_cache, value.and_then(|value: LatestEventValue| value.event_id()), user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                ));
            }

            assert_eq!(buffer.buffer.len(), 3);

            value.unwrap()
        };

        // Receiving a `CancelledLocalEvent` targeting the second event. The
        // `LatestEventValue` must not change.
        let previous_value = {
            let update = RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: transaction_id_1.clone(),
            };

            // The `LatestEventValue` hasn't changed, it still matches the latest local
            // event.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "C"
            );

            assert_eq!(buffer.buffer.len(), 2);

            value
        };

        // Receiving a `CancelledLocalEvent` targeting the second (so the last) event.
        // The `LatestEventValue` must point to the first local event.
        let previous_value = {
            let update = RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: transaction_id_2.clone(),
            };

            // The `LatestEventValue` has changed, it matches the previous (so the first)
            // local event.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            assert_eq!(buffer.buffer.len(), 1);

            value
        };

        // Receiving a `CancelledLocalEvent` targeting the first (so the last) event.
        // The `LatestEventValue` cannot be computed from the send queue and will
        // fallback to the event cache. The event cache is empty in this case, so we get
        // nothing.
        {
            let update =
                RoomSendQueueUpdate::CancelledLocalEvent { transaction_id: transaction_id_0 };

            // The `LatestEventValue` has changed, it's empty!
            assert_matches!(
                Builder::new_local(
                    &update,
                    &mut buffer,
                    &room_event_cache,
                    previous_value.event_id(),
                    user_id,
                    None
                )
                .await,
                Some(LatestEventValue::None)
            );

            assert!(buffer.buffer.is_empty());
        }
    }

    #[async_test]
    async fn test_local_sent_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        let previous_value = {
            let mut value = None;

            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                value = Some(assert_local_value_matches_room_message_with_body!(
                    Builder::new_local(&update, &mut buffer, &room_event_cache, value.and_then(|value: LatestEventValue| value.event_id()), user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                ));
            }

            assert_eq!(buffer.buffer.len(), 2);

            value.unwrap()
        };

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // must not change.
        let previous_value = {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_0.clone(),
                event_id: event_id!("$ev0").to_owned(),
            };

            // The `LatestEventValue` hasn't changed, it still matches the latest local
            // event.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 1);

            value
        };

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // hasn't changed, this is still this event, but the status has changed to
        // `LocalHasBeenSent`.
        {
            let expected_event_id = event_id!("$ev1");
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_1,
                event_id: expected_event_id.to_owned(),
            };

            // The `LatestEventValue` hasn't changed.
            assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalHasBeenSent {
                    value with body = "B",
                    event_id => {
                        assert_eq!(event_id, expected_event_id);
                    }
                }
            );

            assert!(buffer.buffer.is_empty());
        }
    }

    #[async_test]
    async fn test_local_replaced_local_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        let previous_value = {
            let mut value = None;

            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                value = Some(assert_local_value_matches_room_message_with_body!(
                    Builder::new_local(&update, &mut buffer, &room_event_cache, value.and_then(|value: LatestEventValue| value.event_id()), user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                ));
            }

            assert_eq!(buffer.buffer.len(), 2);

            value.unwrap()
        };

        // Receiving a `ReplacedLocalEvent` targeting the first event. The
        // `LatestEventValue` must not change.
        let previous_value = {
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
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);

            value
        };

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
            assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B."
            );

            assert_eq!(buffer.buffer.len(), 2);
        }
    }

    #[async_test]
    async fn test_local_replaced_local_event_by_a_non_suitable_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid0");

        // Receiving one `NewLocalEvent`.
        let previous_value = {
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, None, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            assert_eq!(buffer.buffer.len(), 1);

            value
        };

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
                Builder::new_local(
                    &update,
                    &mut buffer,
                    &room_event_cache,
                    previous_value.event_id(),
                    user_id,
                    None
                )
                .await,
                None
            );

            assert_eq!(buffer.buffer.len(), 0);
        }
    }

    #[async_test]
    async fn test_local_send_unrecoverable_error() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        let previous_value = {
            let mut value = None;

            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                value = Some(assert_local_value_matches_room_message_with_body!(
                    Builder::new_local(&update, &mut buffer, &room_event_cache, value.and_then(|value: LatestEventValue| value.event_id()), user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                ));
            }

            assert_eq!(buffer.buffer.len(), 2);

            value.unwrap()
        };

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` must change to indicate it “cannot be sent”, because the
        // error is unrecoverable.
        let previous_value = {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("oopsy".to_owned().into())),
                is_recoverable: false,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's marked as “cannot be sent”.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalCannotBeSent => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
            assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));

            value
        };

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // must change: since an event has been sent, the following events are now
        // “is sending”.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_0.clone(),
                event_id: event_id!("$ev0").to_owned(),
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's “is sending”.
            assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 1);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
        }
    }

    #[async_test]
    async fn test_local_send_recoverable_error() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        let previous_value = {
            let mut value = None;

            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                value = Some(assert_local_value_matches_room_message_with_body!(
                    Builder::new_local(&update, &mut buffer, &room_event_cache, value.and_then(|value: LatestEventValue| value.event_id()), user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                ));
            }

            assert_eq!(buffer.buffer.len(), 2);

            value.unwrap()
        };

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` doesn't change, because the sending error is recoverable
        // (and thus will be retried soon, or as soon as network comes back).
        let previous_value = {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("no more network".to_owned().into())),
                is_recoverable: true,
            };

            // The `LatestEventValue` still matches the latest local event and should still
            // be marked as a local being sent.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
            assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalIsSending(_));

            value
        };

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // must change: since an event has been sent, the following events are now
        // “is sending”.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_0.clone(),
                event_id: event_id!("$ev0").to_owned(),
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's “is sending”.
            assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 1);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
        }
    }

    #[async_test]
    async fn test_local_retry_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        let previous_value = {
            let mut value = None;

            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                value = Some(assert_local_value_matches_room_message_with_body!(
                    Builder::new_local(&update, &mut buffer, &room_event_cache, value.and_then(|value: LatestEventValue| value.event_id()), user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                ));
            }

            assert_eq!(buffer.buffer.len(), 2);

            value.unwrap()
        };

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` must change to indicate it “cannot be sent”, because the
        // error is unrecoverable.
        let previous_value = {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("oopsy".to_owned().into())),
                is_recoverable: false,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's marked as “cannot be sent”.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalCannotBeSent => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
            assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));

            value
        };

        // Receiving a `RetryEvent` targeting the first event. The `LatestEventValue`
        // must change: this local event and its following must be “is sending”.
        {
            let update =
                RoomSendQueueUpdate::RetryEvent { transaction_id: transaction_id_0.clone() };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's “is sending”.
            assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, previous_value.event_id(), user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
            assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalIsSending(_));
        }
    }

    #[async_test]
    async fn test_local_media_upload() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = BufferOfValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid");

        // Receiving a `NewLocalEvent`.
        let previous_value = {
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            let value = assert_local_value_matches_room_message_with_body!(
                Builder::new_local(&update, &mut buffer, &room_event_cache, None, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            assert_eq!(buffer.buffer.len(), 1);

            value
        };

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
                Builder::new_local(
                    &update,
                    &mut buffer,
                    &room_event_cache,
                    previous_value.event_id(),
                    user_id,
                    None
                )
                .await,
                None
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
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
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
                                event_factory.text_msg("hello").event_id(event_id_0).into(),
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

        let mut buffer = BufferOfValuesForLocalEvents::new();

        // We get a `Remote` because there is no `Local*` values!
        assert_remote_value_matches_room_message_with_body!(
            Builder::new_local(
                // An update that won't create a new `LatestEventValue`: it maps
                // to zero existing local value.
                &RoomSendQueueUpdate::SentEvent {
                    transaction_id: OwnedTransactionId::from("txnid"),
                    event_id: event_id_1.to_owned(),
                },
                &mut buffer,
                &room_event_cache,
                None,
                user_id,
                None,
            )
            .await
            => with body = "hello"
        );
    }
}
