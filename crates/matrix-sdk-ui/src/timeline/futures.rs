use std::{
    fs,
    future::{Future, IntoFuture},
    marker,
    path::Path,
    pin::Pin,
};

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk::{attachment::AttachmentConfig, TransmissionProgress};
use mime::Mime;
use ruma::{
    events::{
        room::message::{
            AddMentions, ForwardThread, OriginalRoomMessageEvent, RoomMessageEventContent,
        },
        AnyMessageLikeEventContent,
    },
    OwnedTransactionId, TransactionId,
};
use tracing::{error, info_span, Instrument};

use super::{
    Error, EventTimelineItem, LocalMessage, Timeline, TimelineItemContent, UnsupportedReplyItem,
};

pub struct Send<'a> {
    timeline: &'a Timeline,
    content: AnyMessageLikeEventContent,
    txn_id: Option<OwnedTransactionId>,
}

impl<'a> Send<'a> {
    pub(crate) fn new(timeline: &'a Timeline, content: AnyMessageLikeEventContent) -> Self {
        Self { timeline, content, txn_id: None }
    }

    /// Set a custom transaction ID.
    ///
    /// The transaction ID is a locally-unique ID describing a message
    /// transaction with the homeserver. Unless you're doing something special,
    /// you can pass in `None` which will create a suitable one for you
    /// automatically.
    ///
    /// * On the sending side, this field is used for re-trying earlier failed
    ///   transactions. Subsequent messages *must never* re-use an earlier
    ///   transaction ID.
    /// * On the receiving side, the field is used for recognizing our own
    ///   messages when they arrive down the sync: the server includes the ID in
    ///   the [`MessageLikeUnsigned`][ruma::events::MessageLikeUnsigned] field
    ///   `transaction_id` of the corresponding
    ///   [`SyncMessageLikeEvent`][ruma::events::SyncMessageLikeEvent], but only
    ///   for the *sending* device. Other devices will not see it.
    pub fn with_transaction_id(mut self, txn_id: &TransactionId) -> Self {
        self.txn_id = Some(txn_id.to_owned());
        self
    }
}

impl<'a> IntoFuture for Send<'a> {
    type Output = ();
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + marker::Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { timeline, content, txn_id } = self;
        let span = info_span!(
            "send",
            room_id = ?timeline.room().room_id(),
            txn_id = txn_id.as_ref().map(debug),
        );

        let txn_id = txn_id.unwrap_or_else(TransactionId::new);
        let fut = async move {
            timeline.inner.handle_local_event(txn_id.clone(), content.clone()).await;
            if timeline.msg_sender.send(LocalMessage { content, txn_id }).await.is_err() {
                error!("Internal error: timeline message receiver is closed");
            }
        };

        Box::pin(fut.instrument(span))
    }
}

pub struct SendReply<'a> {
    timeline: &'a Timeline,
    content: RoomMessageEventContent,
    reply_item: &'a EventTimelineItem,
    forward_thread: ForwardThread,
    add_mentions: AddMentions,
    txn_id: Option<OwnedTransactionId>,
}

impl<'a> SendReply<'a> {
    pub(crate) fn new(
        timeline: &'a Timeline,
        content: RoomMessageEventContent,
        reply_item: &'a EventTimelineItem,
        forward_thread: ForwardThread,
        add_mentions: AddMentions,
    ) -> Self {
        Self { timeline, content, reply_item, forward_thread, add_mentions, txn_id: None }
    }

    /// Set a custom transaction ID.
    ///
    /// The transaction ID is a locally-unique ID describing a message
    /// transaction with the homeserver. Unless you're doing something special,
    /// you can pass in `None` which will create a suitable one for you
    /// automatically.
    ///
    /// * On the sending side, this field is used for re-trying earlier failed
    ///   transactions. Subsequent messages *must never* re-use an earlier
    ///   transaction ID.
    /// * On the receiving side, the field is used for recognizing our own
    ///   messages when they arrive down the sync: the server includes the ID in
    ///   the [`MessageLikeUnsigned`][ruma::events::MessageLikeUnsigned] field
    ///   `transaction_id` of the corresponding
    ///   [`SyncMessageLikeEvent`][ruma::events::SyncMessageLikeEvent], but only
    ///   for the *sending* device. Other devices will not see it.
    pub fn with_transaction_id(mut self, txn_id: &TransactionId) -> Self {
        self.txn_id = Some(txn_id.to_owned());
        self
    }
}

impl<'a> IntoFuture for SendReply<'a> {
    type Output = Result<(), UnsupportedReplyItem>;
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + marker::Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { timeline, content, reply_item, forward_thread, add_mentions, txn_id } = self;
        let span = info_span!("send_reply", ?forward_thread, ?add_mentions).entered();

        let fut = 'make_fut: {
            // Error returns here must be in sync with
            // `EventTimelineItem::can_be_replied_to`
            let Some(event_id) = reply_item.event_id() else {
                break 'make_fut Err(UnsupportedReplyItem::MISSING_EVENT_ID);
            };

            let content = match reply_item.content() {
                TimelineItemContent::Message(msg) => {
                    let event = OriginalRoomMessageEvent {
                        event_id: event_id.to_owned(),
                        sender: reply_item.sender().to_owned(),
                        origin_server_ts: reply_item.timestamp(),
                        room_id: timeline.room().room_id().to_owned(),
                        content: msg.to_content(),
                        unsigned: Default::default(),
                    };
                    content.make_reply_to(&event, forward_thread, add_mentions)
                }
                _ => {
                    let Some(raw_event) = reply_item.latest_json() else {
                        break 'make_fut Err(UnsupportedReplyItem::MISSING_JSON);
                    };

                    content.make_reply_to_raw(
                        raw_event,
                        event_id.to_owned(),
                        timeline.room().room_id(),
                        forward_thread,
                        add_mentions,
                    )
                }
            };

            let mut send_fut = timeline.send(content.into());
            if let Some(txn_id) = txn_id {
                send_fut = send_fut.with_transaction_id(&txn_id);
            }

            Ok(send_fut)
        };

        Box::pin(
            async move {
                match fut {
                    Ok(fut) => {
                        fut.await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            .instrument(span.exit()),
        )
    }
}

pub struct SendAttachment<'a> {
    timeline: &'a Timeline,
    url: String,
    mime_type: Mime,
    config: AttachmentConfig,
    pub(crate) send_progress: SharedObservable<TransmissionProgress>,
}

impl<'a> SendAttachment<'a> {
    pub(crate) fn new(
        timeline: &'a Timeline,
        url: String,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> Self {
        Self { timeline, url, mime_type, config, send_progress: Default::default() }
    }

    /// Get a subscriber to observe the progress of sending the request
    /// body.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn subscribe_to_send_progress(&self) -> Subscriber<TransmissionProgress> {
        self.send_progress.subscribe()
    }
}

impl<'a> IntoFuture for SendAttachment<'a> {
    type Output = Result<(), Error>;
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + marker::Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { timeline, url, mime_type, config, send_progress } = self;
        Box::pin(async move {
            let body = Path::new(&url)
                .file_name()
                .ok_or(Error::InvalidAttachmentFileName)?
                .to_str()
                .expect("path was created from UTF-8 string, hence filename part is UTF-8 too");
            let data = fs::read(&url).map_err(|_| Error::InvalidAttachmentData)?;

            timeline
                .room()
                .send_attachment(body, &mime_type, data, config)
                .with_send_progress_observable(send_progress)
                .await
                .map_err(|_| Error::FailedSendingAttachment)?;

            Ok(())
        })
    }
}
