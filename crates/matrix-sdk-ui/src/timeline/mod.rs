// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! A high-level view into a room's contents.
//!
//! See [`Timeline`] for details.

use std::{fs, path::PathBuf, sync::Arc};

use algorithms::rfind_event_by_item_id;
use event_item::TimelineItemHandle;
use eyeball_im::VectorDiff;
#[cfg(feature = "unstable-msc4274")]
use futures::SendGallery;
use futures_core::Stream;
use imbl::Vector;
use matrix_sdk::{
    Result,
    attachment::{AttachmentInfo, Thumbnail},
    deserialized_responses::TimelineEvent,
    event_cache::{EventCacheDropHandles, RoomEventCache},
    executor::JoinHandle,
    room::{
        Receipts, Room,
        edit::EditedContent,
        reply::{EnforceThread, Reply},
    },
    send_queue::{RoomSendQueueError, SendHandle},
};
use mime::Mime;
use pinned_events_loader::PinnedEventsRoom;
use ruma::{
    EventId, OwnedEventId, OwnedTransactionId, UserId,
    api::client::receipt::create_receipt::v3::ReceiptType,
    events::{
        AnyMessageLikeEventContent, AnySyncTimelineEvent, Mentions,
        poll::unstable_start::{NewUnstablePollStartEventContent, UnstablePollStartEventContent},
        receipt::{Receipt, ReceiptThread},
        relation::Thread,
        room::{
            message::{
                Relation, RelationWithoutReplacement, ReplyWithinThread,
                RoomMessageEventContentWithoutRelation, TextMessageEventContent,
            },
            pinned_events::RoomPinnedEventsEventContent,
        },
    },
    room_version_rules::RoomVersionRules,
};
use subscriber::TimelineWithDropHandle;
use thiserror::Error;
use tracing::{instrument, trace, warn};

use self::{
    algorithms::rfind_event_by_id, controller::TimelineController, futures::SendAttachment,
};
use crate::timeline::controller::CryptoDropHandles;

mod algorithms;
mod builder;
mod controller;
mod date_dividers;
mod error;
pub mod event_filter;
mod event_handler;
mod event_item;
pub mod futures;
mod item;
mod latest_event;
mod pagination;
mod pinned_events_loader;
mod subscriber;
mod tasks;
#[cfg(test)]
mod tests;
mod traits;
mod virtual_item;

pub use self::{
    builder::TimelineBuilder,
    controller::default_event_filter,
    error::*,
    event_filter::{TimelineEventCondition, TimelineEventFilter},
    event_item::{
        AnyOtherFullStateEventContent, EmbeddedEvent, EncryptedMessage, EventItemOrigin,
        EventSendState, EventTimelineItem, InReplyToDetails, MediaUploadProgress,
        MemberProfileChange, MembershipChange, Message, MsgLikeContent, MsgLikeKind,
        OtherMessageLike, OtherState, PollResult, PollState, Profile, ReactionInfo, ReactionStatus,
        ReactionsByKeyBySender, RoomMembershipChange, RoomPinnedEventsChange, Sticker,
        ThreadSummary, TimelineDetails, TimelineEventItemId, TimelineEventShieldState,
        TimelineEventShieldStateCode, TimelineItemContent,
    },
    item::{TimelineItem, TimelineItemKind, TimelineUniqueId},
    latest_event::{LatestEventValue, LatestEventValueLocalState},
    traits::RoomExt,
    virtual_item::VirtualTimelineItem,
};

/// A high-level view into a regular¹ room's contents.
///
/// ¹ This type is meant to be used in the context of rooms without a
/// `room_type`, that is rooms that are primarily used to exchange text
/// messages.
#[derive(Debug)]
pub struct Timeline {
    /// Cloneable, inner fields of the `Timeline`, shared with some background
    /// tasks.
    controller: TimelineController,

    /// The event cache specialized for this room's view.
    event_cache: RoomEventCache,

    /// References to long-running tasks held by the timeline.
    drop_handle: Arc<TimelineDropHandle>,
}

/// What should the timeline focus on?
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineFocus {
    /// Focus on live events, i.e. receive events from sync and append them in
    /// real-time.
    Live {
        /// Whether to hide in-thread replies from the live timeline.
        ///
        /// This should be set to true when the client can create
        /// [`Self::Thread`]-focused timelines from the thread roots themselves.
        hide_threaded_events: bool,
    },

    /// Focus on a specific event, e.g. after clicking a permalink.
    Event {
        target: OwnedEventId,
        num_context_events: u16,
        /// How to handle threaded events.
        thread_mode: TimelineEventFocusThreadMode,
    },

    /// Focus on a specific thread
    Thread { root_event_id: OwnedEventId },

    /// Only show pinned events.
    PinnedEvents { max_events_to_load: u16, max_concurrent_requests: u16 },
}

/// Options for controlling the behaviour of [`TimelineFocus::Event`]
/// for threaded events.
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineEventFocusThreadMode {
    /// Force the timeline into threaded mode. When the focused event is part of
    /// a thread, the timeline will be focused on that thread's root. Otherwise,
    /// the timeline will treat the target event itself as the thread root.
    /// Threaded events will never be hidden.
    ForceThread,
    /// Automatically determine if the target event is
    /// part of a thread or not. If the event is part of a thread, the timeline
    /// will be filtered to on-thread events.
    Automatic {
        /// When the target event is not part of a thread, whether to
        /// hide in-thread replies from the live timeline. Has no effect
        /// when the target event is part of a thread.
        ///
        /// This should be set to true when the client can create
        /// [`TimelineFocus::Thread`]-focused timelines from the thread roots
        /// themselves and doesn't use the [`Self::ForceThread`] mode.
        hide_threaded_events: bool,
    },
}

impl TimelineFocus {
    pub(super) fn debug_string(&self) -> String {
        match self {
            TimelineFocus::Live { .. } => "live".to_owned(),
            TimelineFocus::Event { target, .. } => format!("permalink:{target}"),
            TimelineFocus::Thread { root_event_id, .. } => format!("thread:{root_event_id}"),
            TimelineFocus::PinnedEvents { .. } => "pinned-events".to_owned(),
        }
    }
}

/// Changes how dividers get inserted, either in between each day or in between
/// each month
#[derive(Debug, Clone)]
pub enum DateDividerMode {
    Daily,
    Monthly,
}

/// Configuration for sending an attachment.
///
/// Like [`matrix_sdk::attachment::AttachmentConfig`], but instead of the
/// `reply` field, there's only a `in_reply_to` event id; it's the timeline
/// deciding to fill the rest of the reply parameters.
#[derive(Debug, Default)]
pub struct AttachmentConfig {
    pub txn_id: Option<OwnedTransactionId>,
    pub info: Option<AttachmentInfo>,
    pub thumbnail: Option<Thumbnail>,
    pub caption: Option<TextMessageEventContent>,
    pub mentions: Option<Mentions>,
    pub in_reply_to: Option<OwnedEventId>,
}

impl Timeline {
    /// Returns the room for this timeline.
    pub fn room(&self) -> &Room {
        self.controller.room()
    }

    /// Clear all timeline items.
    pub async fn clear(&self) {
        self.controller.clear().await;
    }

    /// Retry decryption of previously un-decryptable events given a list of
    /// session IDs whose keys have been imported.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{Client, config::SyncSettings, ruma::room_id};
    /// # use matrix_sdk_ui::Timeline;
    /// # async {
    /// # let mut client: Client = todo!();
    /// # let room_id = ruma::room_id!("!example:example.org");
    /// # let timeline: Timeline = todo!();
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// let result =
    ///     client.encryption().import_room_keys(path, "secret-passphrase").await?;
    ///
    /// // Given a timeline for a specific room_id
    /// if let Some(keys_for_users) = result.keys.get(room_id) {
    ///     let session_ids = keys_for_users.values().flatten();
    ///     timeline.retry_decryption(session_ids).await;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn retry_decryption<S: Into<String>>(
        &self,
        session_ids: impl IntoIterator<Item = S>,
    ) {
        self.controller
            .retry_event_decryption(Some(session_ids.into_iter().map(Into::into).collect()))
            .await;
    }

    #[tracing::instrument(skip(self))]
    async fn retry_decryption_for_all_events(&self) {
        self.controller.retry_event_decryption(None).await;
    }

    /// Get the current timeline item for the given event ID, if any.
    ///
    /// Will return a remote event, *or* a local echo that has been sent but not
    /// yet replaced by a remote echo.
    ///
    /// It's preferable to store the timeline items in the model for your UI, if
    /// possible, instead of just storing IDs and coming back to the timeline
    /// object to look up items.
    pub async fn item_by_event_id(&self, event_id: &EventId) -> Option<EventTimelineItem> {
        let items = self.controller.items().await;
        let (_, item) = rfind_event_by_id(&items, event_id)?;
        Some(item.to_owned())
    }

    /// Get the latest of the timeline's remote event ids.
    pub async fn latest_event_id(&self) -> Option<OwnedEventId> {
        self.controller.latest_event_id().await
    }

    /// Get the current timeline items, along with a stream of updates of
    /// timeline items.
    ///
    /// The stream produces `Vec<VectorDiff<_>>`, which means multiple updates
    /// at once. There are no delays, it consumes as many updates as possible
    /// and batches them.
    pub async fn subscribe(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, impl Stream<Item = Vec<VectorDiff<Arc<TimelineItem>>>> + use<>)
    {
        let (items, stream) = self.controller.subscribe().await;
        let stream = TimelineWithDropHandle::new(stream, self.drop_handle.clone());
        (items, stream)
    }

    /// Send a message to the room, and add it to the timeline as a local echo.
    ///
    /// For simplicity, this method doesn't currently allow custom message
    /// types.
    ///
    /// If the encryption feature is enabled, this method will transparently
    /// encrypt the room message if the room is encrypted.
    ///
    /// If sending the message fails, the local echo item will change its
    /// `send_state` to [`EventSendState::SendingFailed`].
    ///
    /// This will do the right thing in the presence of threads:
    /// - if this timeline is not focused on a thread, then it will send the
    ///   event as is.
    /// - if this is a threaded timeline, and the event to send is a room
    ///   message without a relationship, it will automatically mark it as a
    ///   thread reply with the correct reply fallback, and send it.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the message event.
    #[instrument(skip(self, content), fields(room_id = ?self.room().room_id()))]
    pub async fn send(&self, mut content: AnyMessageLikeEventContent) -> Result<SendHandle, Error> {
        // If this is a room event we're sending in a threaded timeline, we add the
        // thread relation ourselves.
        if content.relation().is_none()
            && let Some(reply) = self.infer_reply(None).await
        {
            match &mut content {
                AnyMessageLikeEventContent::RoomMessage(room_msg_content) => {
                    content = self
                        .room()
                        .make_reply_event(
                            // Note: this `.into()` gets rid of the relation, but we've checked
                            // previously that the `relates_to` field wasn't
                            // set.
                            room_msg_content.clone().into(),
                            reply,
                        )
                        .await?
                        .into();
                }

                AnyMessageLikeEventContent::UnstablePollStart(
                    UnstablePollStartEventContent::New(poll),
                ) => {
                    if let Some(thread_root) = self.controller.thread_root() {
                        poll.relates_to = Some(RelationWithoutReplacement::Thread(Thread::plain(
                            thread_root,
                            reply.event_id,
                        )));
                    }
                }

                AnyMessageLikeEventContent::Sticker(sticker) => {
                    if let Some(thread_root) = self.controller.thread_root() {
                        sticker.relates_to =
                            Some(Relation::Thread(Thread::plain(thread_root, reply.event_id)));
                    }
                }

                _ => {}
            }
        }

        Ok(self.room().send_queue().send(content).await?)
    }

    /// Send a reply to the given event.
    ///
    /// Currently it only supports events with an event ID and JSON being
    /// available (which can be removed by local redactions). This is subject to
    /// change. Use [`EventTimelineItem::can_be_replied_to`] to decide whether
    /// to render a reply button.
    ///
    /// The sender will be added to the mentions of the reply if
    /// and only if the event has not been written by the sender.
    ///
    /// This will do the right thing in the presence of threads:
    /// - if this timeline is not focused on a thread, then it will forward the
    ///   thread relationship of the replied-to event, if present.
    /// - if this is a threaded timeline, it will mark the reply as an in-thread
    ///   reply.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the reply.
    ///
    /// * `in_reply_to` - The ID of the event to reply to.
    #[instrument(skip(self, content))]
    pub async fn send_reply(
        &self,
        content: RoomMessageEventContentWithoutRelation,
        in_reply_to: OwnedEventId,
    ) -> Result<(), Error> {
        let reply = self
            .infer_reply(Some(in_reply_to))
            .await
            .expect("the reply will always be set because we provided a replied-to event id");
        let content = self.room().make_reply_event(content, reply).await?;
        self.send(content.into()).await?;
        Ok(())
    }

    /// Given a message or media to send, and an optional `in_reply_to` event,
    /// automatically fills the [`Reply`] information based on the current
    /// timeline focus.
    pub(crate) async fn infer_reply(&self, in_reply_to: Option<OwnedEventId>) -> Option<Reply> {
        // If there's a replied-to event id, the reply is pretty straightforward, and we
        // should only infer the `EnforceThread` based on the current focus.
        if let Some(in_reply_to) = in_reply_to {
            let enforce_thread = if self.controller.is_threaded() {
                EnforceThread::Threaded(ReplyWithinThread::Yes)
            } else {
                EnforceThread::MaybeThreaded
            };
            return Some(Reply { event_id: in_reply_to, enforce_thread });
        }

        let thread_root = self.controller.thread_root()?;

        // The latest event id is used for the reply-to fallback, for clients which
        // don't handle threads. It should be correctly set to the latest
        // event in the thread, which the timeline instance might or might
        // not know about; in this case, we do a best effort of filling it, and resort
        // to using the thread root if we don't know about any event.
        //
        // Note: we could trigger a back-pagination if the timeline is empty, and wait
        // for the results, if the timeline is too often empty.

        let latest_event_id = self
            .controller
            .items()
            .await
            .iter()
            .rev()
            .find_map(|item| {
                if let TimelineItemKind::Event(event) = item.kind() {
                    event.event_id().map(ToOwned::to_owned)
                } else {
                    None
                }
            })
            .unwrap_or(thread_root);

        Some(Reply {
            event_id: latest_event_id,
            enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
        })
    }

    /// Edit an event given its [`TimelineEventItemId`] and some new content.
    ///
    /// Only supports events for which [`EventTimelineItem::is_editable()`]
    /// returns `true`.
    #[instrument(skip(self, new_content))]
    pub async fn edit(
        &self,
        item_id: &TimelineEventItemId,
        new_content: EditedContent,
    ) -> Result<(), Error> {
        let items = self.items().await;
        let Some((_pos, item)) = rfind_event_by_item_id(&items, item_id) else {
            return Err(Error::EventNotInTimeline(item_id.clone()));
        };

        match item.handle() {
            TimelineItemHandle::Remote(event_id) => {
                let content = self
                    .room()
                    .make_edit_event(event_id, new_content)
                    .await
                    .map_err(EditError::RoomError)?;
                self.send(content).await?;
                Ok(())
            }

            TimelineItemHandle::Local(handle) => {
                // Relations are filled by the editing code itself.
                let new_content: AnyMessageLikeEventContent = match new_content {
                    EditedContent::RoomMessage(message) => {
                        if item.content.is_message() {
                            AnyMessageLikeEventContent::RoomMessage(message.into())
                        } else {
                            return Err(EditError::ContentMismatch {
                                original: item.content.debug_string().to_owned(),
                                new: "a message".to_owned(),
                            }
                            .into());
                        }
                    }

                    EditedContent::PollStart { new_content, .. } => {
                        if item.content.is_poll() {
                            AnyMessageLikeEventContent::UnstablePollStart(
                                UnstablePollStartEventContent::New(
                                    NewUnstablePollStartEventContent::new(new_content),
                                ),
                            )
                        } else {
                            return Err(EditError::ContentMismatch {
                                original: item.content.debug_string().to_owned(),
                                new: "a poll".to_owned(),
                            }
                            .into());
                        }
                    }

                    EditedContent::MediaCaption { caption, formatted_caption, mentions } => {
                        if handle
                            .edit_media_caption(caption, formatted_caption, mentions)
                            .await
                            .map_err(RoomSendQueueError::StorageError)?
                        {
                            return Ok(());
                        }
                        return Err(EditError::InvalidLocalEchoState.into());
                    }
                };

                if !handle.edit(new_content).await.map_err(RoomSendQueueError::StorageError)? {
                    return Err(EditError::InvalidLocalEchoState.into());
                }

                Ok(())
            }
        }
    }

    /// Toggle a reaction on an event.
    ///
    /// Adds or redacts a reaction based on the state of the reaction at the
    /// time it is called.
    ///
    /// When redacting a previous reaction, the redaction reason is not set.
    ///
    /// Ensures that only one reaction is sent at a time to avoid race
    /// conditions and spamming the homeserver with requests.
    ///
    /// Returns `true` if the reaction was added, `false` if it was removed.
    pub async fn toggle_reaction(
        &self,
        item_id: &TimelineEventItemId,
        reaction_key: &str,
    ) -> Result<bool, Error> {
        self.controller.toggle_reaction_local(item_id, reaction_key).await
    }

    /// Sends an attachment to the room.
    ///
    /// It does not currently support local echoes.
    ///
    /// If the encryption feature is enabled, this method will transparently
    /// encrypt the room message if the room is encrypted.
    ///
    /// The attachment and its optional thumbnail are stored in the media cache
    /// and can be retrieved at any time, by calling
    /// [`Media::get_media_content()`] with the `MediaSource` that can be found
    /// in the corresponding `TimelineEventItem`, and using a
    /// `MediaFormat::File`.
    ///
    /// # Arguments
    ///
    /// * `source` - The source of the attachment to send.
    ///
    /// * `mime_type` - The attachment's mime type.
    ///
    /// * `config` - An attachment configuration object containing details about
    ///   the attachment like a thumbnail, its size, duration etc.
    ///
    /// [`Media::get_media_content()`]: matrix_sdk::Media::get_media_content
    #[instrument(skip_all)]
    pub fn send_attachment(
        &self,
        source: impl Into<AttachmentSource>,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> SendAttachment<'_> {
        SendAttachment::new(self, source.into(), mime_type, config)
    }

    /// Sends a media gallery to the room.
    ///
    /// If the encryption feature is enabled, this method will transparently
    /// encrypt the room message if the room is encrypted.
    ///
    /// The attachments and their optional thumbnails are stored in the media
    /// cache and can be retrieved at any time, by calling
    /// [`Media::get_media_content()`] with the `MediaSource` that can be found
    /// in the corresponding `TimelineEventItem`, and using a
    /// `MediaFormat::File`.
    ///
    /// # Arguments
    /// * `gallery` - A configuration object containing details about the
    ///   gallery like files, thumbnails, etc.
    ///
    /// [`Media::get_media_content()`]: matrix_sdk::Media::get_media_content
    #[cfg(feature = "unstable-msc4274")]
    #[instrument(skip_all)]
    pub fn send_gallery(&self, gallery: GalleryConfig) -> SendGallery<'_> {
        SendGallery::new(self, gallery)
    }

    /// Redact an event given its [`TimelineEventItemId`] and an optional
    /// reason.
    pub async fn redact(
        &self,
        item_id: &TimelineEventItemId,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        let items = self.items().await;
        let Some((_pos, event)) = rfind_event_by_item_id(&items, item_id) else {
            return Err(RedactError::ItemNotFound(item_id.clone()).into());
        };

        match event.handle() {
            TimelineItemHandle::Remote(event_id) => {
                self.room().redact(event_id, reason, None).await.map_err(RedactError::HttpError)?;
            }
            TimelineItemHandle::Local(handle) => {
                if !handle.abort().await.map_err(RoomSendQueueError::StorageError)? {
                    return Err(RedactError::InvalidLocalEchoState.into());
                }
            }
        }

        Ok(())
    }

    /// Fetch unavailable details about the event with the given ID.
    ///
    /// This method only works for IDs of remote [`EventTimelineItem`]s,
    /// to prevent losing details when a local echo is replaced by its
    /// remote echo.
    ///
    /// This method tries to make all the requests it can. If an error is
    /// encountered for a given request, it is forwarded with the
    /// [`TimelineDetails::Error`] variant.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The event ID of the event to fetch details for.
    ///
    /// # Errors
    ///
    /// Returns an error if the identifier doesn't match any event with a remote
    /// echo in the timeline, or if the event is removed from the timeline
    /// before all requests are handled.
    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub async fn fetch_details_for_event(&self, event_id: &EventId) -> Result<(), Error> {
        self.controller.fetch_in_reply_to_details(event_id).await
    }

    /// Fetch all member events for the room this timeline is displaying.
    ///
    /// If the full member list is not known, sender profiles are currently
    /// likely not going to be available. This will be fixed in the future.
    ///
    /// If fetching the members fails, any affected timeline items will have
    /// the `sender_profile` set to [`TimelineDetails::Error`].
    #[instrument(skip_all)]
    pub async fn fetch_members(&self) {
        self.controller.set_sender_profiles_pending().await;
        match self.room().sync_members().await {
            Ok(_) => {
                self.controller.update_missing_sender_profiles().await;
            }
            Err(e) => {
                self.controller.set_sender_profiles_error(Arc::new(e)).await;
            }
        }
    }

    /// Get the latest read receipt for the given user.
    ///
    /// Contrary to [`Room::load_user_receipt()`] that only keeps track of read
    /// receipts received from the homeserver, this keeps also track of implicit
    /// read receipts in this timeline, i.e. when a room member sends an event.
    #[instrument(skip(self))]
    pub async fn latest_user_read_receipt(
        &self,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.controller.latest_user_read_receipt(user_id).await
    }

    /// Get the ID of the timeline event with the latest read receipt for the
    /// given user.
    ///
    /// In contrary to [`Self::latest_user_read_receipt()`], this allows to know
    /// the position of the read receipt in the timeline even if the event it
    /// applies to is not visible in the timeline, unless the event is unknown
    /// by this timeline.
    #[instrument(skip(self))]
    pub async fn latest_user_read_receipt_timeline_event_id(
        &self,
        user_id: &UserId,
    ) -> Option<OwnedEventId> {
        self.controller.latest_user_read_receipt_timeline_event_id(user_id).await
    }

    /// Subscribe to changes in the read receipts of our own user.
    pub async fn subscribe_own_user_read_receipts_changed(&self) -> impl Stream<Item = ()> + use<> {
        self.controller.subscribe_own_user_read_receipts_changed().await
    }

    /// Send the given receipt.
    ///
    /// This uses [`Room::send_single_receipt`] internally, but checks
    /// first if the receipt points to an event in this timeline that is more
    /// recent than the current ones, to avoid unnecessary requests.
    ///
    /// If an unthreaded receipt is sent, this will also unset the unread flag
    /// of the room if necessary.
    ///
    /// The thread of the receipt is determined by the timeline instance's
    /// focus mode and `hide_threaded_events` flag.
    ///
    /// Returns a boolean indicating if it sent the receipt or not.
    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub async fn send_single_receipt(
        &self,
        receipt_type: ReceiptType,
        event_id: OwnedEventId,
    ) -> Result<bool> {
        let thread = self.controller.infer_thread_for_read_receipt(&receipt_type);

        if !self.controller.should_send_receipt(&receipt_type, &thread, &event_id).await {
            trace!(
                "not sending receipt, because we already cover the event with a previous receipt"
            );

            if thread == ReceiptThread::Unthreaded {
                // Unset the read marker.
                self.room().set_unread_flag(false).await?;
            }

            return Ok(false);
        }

        trace!("sending receipt");
        self.room().send_single_receipt(receipt_type, thread, event_id).await?;
        Ok(true)
    }

    /// Send the given receipts.
    ///
    /// This uses [`Room::send_multiple_receipts`] internally, but
    /// checks first if the receipts point to events in this timeline that
    /// are more recent than the current ones, to avoid unnecessary
    /// requests.
    ///
    /// This also unsets the unread marker of the room if necessary.
    #[instrument(skip(self))]
    pub async fn send_multiple_receipts(&self, mut receipts: Receipts) -> Result<()> {
        if let Some(fully_read) = &receipts.fully_read
            && !self
                .controller
                .should_send_receipt(
                    &ReceiptType::FullyRead,
                    &ReceiptThread::Unthreaded,
                    fully_read,
                )
                .await
        {
            receipts.fully_read = None;
        }

        if let Some(read_receipt) = &receipts.public_read_receipt
            && !self
                .controller
                .should_send_receipt(&ReceiptType::Read, &ReceiptThread::Unthreaded, read_receipt)
                .await
        {
            receipts.public_read_receipt = None;
        }

        if let Some(private_read_receipt) = &receipts.private_read_receipt
            && !self
                .controller
                .should_send_receipt(
                    &ReceiptType::ReadPrivate,
                    &ReceiptThread::Unthreaded,
                    private_read_receipt,
                )
                .await
        {
            receipts.private_read_receipt = None;
        }

        let room = self.room();

        if !receipts.is_empty() {
            room.send_multiple_receipts(receipts).await?;
        } else {
            room.set_unread_flag(false).await?;
        }

        Ok(())
    }

    /// Mark the timeline as read by attempting to send a read receipt on the
    /// latest visible event.
    ///
    /// The latest visible event is determined from the timeline's focus kind
    /// and whether or not it hides threaded events. If no latest event can
    /// be determined and the timeline is live, the room's unread marker is
    /// unset instead.
    ///
    /// # Arguments
    ///
    /// * `receipt_type` - The type of receipt to send. When using
    ///   [`ReceiptType::FullyRead`], an unthreaded receipt will be sent. This
    ///   works even if the latest event belongs to a thread, as a threaded
    ///   reply also belongs to the unthreaded timeline. Otherwise the
    ///   [`ReceiptThread`] will be determined based on the timeline's focus
    ///   kind.
    ///
    /// # Returns
    ///
    /// A boolean indicating if the receipt was sent or not.
    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub async fn mark_as_read(&self, receipt_type: ReceiptType) -> Result<bool> {
        if let Some(event_id) = self.controller.latest_event_id().await {
            self.send_single_receipt(receipt_type, event_id).await
        } else {
            trace!("can't mark room as read because there's no latest event id");

            // For live timelines, unset the read marker in this case.
            if self.controller.is_live() {
                self.room().set_unread_flag(false).await?;
            }

            Ok(false)
        }
    }

    /// Adds a new pinned event by sending an updated `m.room.pinned_events`
    /// event containing the new event id.
    ///
    /// This method will first try to get the pinned events from the current
    /// room's state and if it fails to do so it'll try to load them from the
    /// homeserver.
    ///
    /// Returns `true` if we pinned the event, `false` if the event was already
    /// pinned.
    pub async fn pin_event(&self, event_id: &EventId) -> Result<bool> {
        let mut pinned_event_ids = if let Some(event_ids) = self.room().pinned_event_ids() {
            event_ids
        } else {
            self.room().load_pinned_events().await?.unwrap_or_default()
        };
        let event_id = event_id.to_owned();
        if pinned_event_ids.contains(&event_id) {
            Ok(false)
        } else {
            pinned_event_ids.push(event_id);
            let content = RoomPinnedEventsEventContent::new(pinned_event_ids);
            self.room().send_state_event(content).await?;
            Ok(true)
        }
    }

    /// Removes a pinned event by sending an updated `m.room.pinned_events`
    /// event without the event id we want to remove.
    ///
    /// This method will first try to get the pinned events from the current
    /// room's state and if it fails to do so it'll try to load them from the
    /// homeserver.
    ///
    /// Returns `true` if we unpinned the event, `false` if the event wasn't
    /// pinned before.
    pub async fn unpin_event(&self, event_id: &EventId) -> Result<bool> {
        let mut pinned_event_ids = if let Some(event_ids) = self.room().pinned_event_ids() {
            event_ids
        } else {
            self.room().load_pinned_events().await?.unwrap_or_default()
        };
        let event_id = event_id.to_owned();
        if let Some(idx) = pinned_event_ids.iter().position(|e| *e == *event_id) {
            pinned_event_ids.remove(idx);
            let content = RoomPinnedEventsEventContent::new(pinned_event_ids);
            self.room().send_state_event(content).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Create a [`EmbeddedEvent`] from an arbitrary event, be it in the
    /// timeline or not.
    ///
    /// Can be `None` if the event cannot be represented as a standalone item,
    /// because it's an aggregation.
    pub async fn make_replied_to(
        &self,
        event: TimelineEvent,
    ) -> Result<Option<EmbeddedEvent>, Error> {
        self.controller.make_replied_to(event).await
    }

    /// Returns whether this timeline is focused on a thread (be it live, or
    /// from a permalink to a threaded event).
    pub fn is_threaded(&self) -> bool {
        self.controller.is_threaded()
    }
}

/// Test helpers, likely not very useful in production.
#[doc(hidden)]
impl Timeline {
    /// Get the current list of timeline items.
    pub async fn items(&self) -> Vector<Arc<TimelineItem>> {
        self.controller.items().await
    }

    pub async fn subscribe_filter_map<U: Clone>(
        &self,
        f: impl Fn(Arc<TimelineItem>) -> Option<U>,
    ) -> (Vector<U>, impl Stream<Item = VectorDiff<U>>) {
        let (items, stream) = self.controller.subscribe_filter_map(f).await;
        let stream = TimelineWithDropHandle::new(stream, self.drop_handle.clone());
        (items, stream)
    }
}

#[derive(Debug)]
struct TimelineDropHandle {
    room_update_join_handle: JoinHandle<()>,
    pinned_events_join_handle: Option<JoinHandle<()>>,
    thread_update_join_handle: Option<JoinHandle<()>>,
    local_echo_listener_handle: JoinHandle<()>,
    _event_cache_drop_handle: Arc<EventCacheDropHandles>,
    _crypto_drop_handles: CryptoDropHandles,
}

impl Drop for TimelineDropHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.pinned_events_join_handle.take() {
            handle.abort();
        }

        if let Some(handle) = self.thread_update_join_handle.take() {
            handle.abort();
        }

        self.local_echo_listener_handle.abort();
        self.room_update_join_handle.abort();
    }
}

#[cfg(not(target_family = "wasm"))]
pub type TimelineEventFilterFn =
    dyn Fn(&AnySyncTimelineEvent, &RoomVersionRules) -> bool + Send + Sync;
#[cfg(target_family = "wasm")]
pub type TimelineEventFilterFn = dyn Fn(&AnySyncTimelineEvent, &RoomVersionRules) -> bool;

/// A source for sending an attachment.
///
/// The [`AttachmentSource::File`] variant can be constructed from any type that
/// implements `Into<PathBuf>`.
#[derive(Debug, Clone)]
pub enum AttachmentSource {
    /// The data of the attachment.
    Data {
        /// The bytes of the attachment.
        bytes: Vec<u8>,

        /// The filename of the attachment.
        filename: String,
    },

    /// An attachment loaded from a file.
    ///
    /// The bytes and the filename will be read from the file at the given path.
    File(PathBuf),
}

impl AttachmentSource {
    /// Try to convert this attachment source into a `(bytes, filename)` tuple.
    pub(crate) fn try_into_bytes_and_filename(self) -> Result<(Vec<u8>, String), Error> {
        match self {
            Self::Data { bytes, filename } => Ok((bytes, filename)),
            Self::File(path) => {
                let filename = path
                    .file_name()
                    .ok_or(Error::InvalidAttachmentFileName)?
                    .to_str()
                    .ok_or(Error::InvalidAttachmentFileName)?
                    .to_owned();
                let bytes = fs::read(&path).map_err(|_| Error::InvalidAttachmentData)?;
                Ok((bytes, filename))
            }
        }
    }
}

impl<P> From<P> for AttachmentSource
where
    P: Into<PathBuf>,
{
    fn from(value: P) -> Self {
        Self::File(value.into())
    }
}

/// Configuration for sending a gallery.
///
/// This duplicates [`matrix_sdk::attachment::GalleryConfig`] but uses an
/// `AttachmentSource` so that we can delay loading the actual data until we're
/// inside the SendGallery future. This allows [`Timeline::send_gallery`] to
/// return early without blocking the caller.
#[cfg(feature = "unstable-msc4274")]
#[derive(Debug, Default)]
pub struct GalleryConfig {
    pub(crate) txn_id: Option<OwnedTransactionId>,
    pub(crate) items: Vec<GalleryItemInfo>,
    pub(crate) caption: Option<TextMessageEventContent>,
    pub(crate) mentions: Option<Mentions>,
    pub(crate) in_reply_to: Option<OwnedEventId>,
}

#[cfg(feature = "unstable-msc4274")]
impl GalleryConfig {
    /// Create a new empty `GalleryConfig`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the transaction ID to send.
    ///
    /// # Arguments
    ///
    /// * `txn_id` - A unique ID that can be attached to a `MessageEvent` held
    ///   in its unsigned field as `transaction_id`. If not given, one is
    ///   created for the message.
    #[must_use]
    pub fn txn_id(mut self, txn_id: OwnedTransactionId) -> Self {
        self.txn_id = Some(txn_id);
        self
    }

    /// Adds a media item to the gallery.
    ///
    /// # Arguments
    ///
    /// * `item` - Information about the item to be added.
    #[must_use]
    pub fn add_item(mut self, item: GalleryItemInfo) -> Self {
        self.items.push(item);
        self
    }

    /// Set the optional caption.
    ///
    /// # Arguments
    ///
    /// * `caption` - The optional caption.
    pub fn caption(mut self, caption: Option<TextMessageEventContent>) -> Self {
        self.caption = caption;
        self
    }

    /// Set the mentions of the message.
    ///
    /// # Arguments
    ///
    /// * `mentions` - The mentions of the message.
    pub fn mentions(mut self, mentions: Option<Mentions>) -> Self {
        self.mentions = mentions;
        self
    }

    /// Set the reply information of the message.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The event ID to reply to.
    pub fn in_reply_to(mut self, event_id: Option<OwnedEventId>) -> Self {
        self.in_reply_to = event_id;
        self
    }

    /// Returns the number of media items in the gallery.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Checks whether the gallery contains any media items or not.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(feature = "unstable-msc4274")]
#[derive(Debug)]
/// Metadata for a gallery item
pub struct GalleryItemInfo {
    /// The attachment source.
    pub source: AttachmentSource,
    /// The mime type.
    pub content_type: Mime,
    /// The attachment info.
    pub attachment_info: AttachmentInfo,
    /// The caption.
    pub caption: Option<TextMessageEventContent>,
    /// The thumbnail.
    pub thumbnail: Option<Thumbnail>,
}

#[cfg(feature = "unstable-msc4274")]
impl TryFrom<GalleryItemInfo> for matrix_sdk::attachment::GalleryItemInfo {
    type Error = Error;

    fn try_from(value: GalleryItemInfo) -> Result<Self, Self::Error> {
        let (data, filename) = value.source.try_into_bytes_and_filename()?;
        Ok(matrix_sdk::attachment::GalleryItemInfo {
            filename,
            content_type: value.content_type,
            data,
            attachment_info: value.attachment_info,
            caption: value.caption,
            thumbnail: value.thumbnail,
        })
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
/// The level of read receipt tracking for the timeline.
pub enum TimelineReadReceiptTracking {
    /// Track read receipts for all events.
    AllEvents,
    /// Track read receipts only for message-like events.
    MessageLikeEvents,
    /// Disable read receipt tracking.
    Disabled,
}

impl TimelineReadReceiptTracking {
    /// Whether or not read receipt tracking is enabled.
    pub fn is_enabled(&self) -> bool {
        match self {
            TimelineReadReceiptTracking::AllEvents
            | TimelineReadReceiptTracking::MessageLikeEvents => true,
            TimelineReadReceiptTracking::Disabled => false,
        }
    }
}
