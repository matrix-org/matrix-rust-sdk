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

use std::{
    collections::VecDeque,
    future::{pending, Future, Pending},
    mem,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::future::Either;
use matrix_sdk::{
    executor::{spawn, JoinError, JoinHandle},
    room::{self, Room},
};
use ruma::{events::AnyMessageLikeEventContent, OwnedTransactionId};
use tokio::{select, sync::mpsc::Receiver};
use tracing::{debug, error, info, instrument, trace, warn};

use super::{inner::TimelineInner, EventSendState};

/// A locally-created message that is supposed to be sent.
pub(super) struct LocalMessage {
    /// The transaction ID.
    ///
    /// Used for finding the corresponding local echo in the timeline.
    pub txn_id: OwnedTransactionId,
    /// The message contents.
    pub content: AnyMessageLikeEventContent,
}

#[instrument(skip_all, fields(room_id = ?room.room_id()))]
pub(super) async fn send_queued_messages(
    timeline_inner: TimelineInner,
    room: room::Common,
    mut msg_receiver: Receiver<LocalMessage>,
) {
    let mut queue = VecDeque::new();
    let mut send_task: SendMessageTask = SendMessageTask::Idle;
    let mut recv_fut: Either<_, Pending<Option<LocalMessage>>> =
        Either::Left(Box::pin(msg_receiver.recv()));

    loop {
        select! {
            result = &mut send_task => {
                trace!("SendMessageTask finished");
                send_task.reset();
                handle_task_ready(
                    result,
                    &mut send_task,
                    &mut queue,
                    &timeline_inner,
                ).await;
            }
            recv_res = &mut recv_fut => {
                recv_fut = if let Some(msg) = recv_res {
                    trace!("Got a LocalMessage");
                    handle_message(
                        msg,
                        room.clone(),
                        &mut send_task,
                        &mut queue,
                        &timeline_inner,
                    ).await;

                    // appease the borrow checker
                    drop(recv_fut);

                    Either::Left(Box::pin(msg_receiver.recv()))
                } else {
                    info!("Message receiver closed");
                    Either::Right(pending())
                };
            }
        }

        if send_task.is_idle() && matches!(recv_fut, Either::Right(_)) {
            break;
        }
    }

    info!("Stopped");
}

async fn handle_message(
    msg: LocalMessage,
    room: room::Common,
    send_task: &mut SendMessageTask,
    queue: &mut VecDeque<LocalMessage>,
    timeline_inner: &TimelineInner,
) {
    if queue.is_empty() && send_task.is_idle() {
        match Room::from(room) {
            Room::Joined(room) => {
                send_task.start(room, timeline_inner.clone(), msg);
            }
            _ => {
                info!("Refusing to send message, room is not joined");
                timeline_inner
                    .update_event_send_state(
                        &msg.txn_id,
                        EventSendState::SendingFailed {
                            // FIXME: Probably not exactly right
                            error: Arc::new(matrix_sdk::Error::InconsistentState),
                        },
                    )
                    .await;
            }
        }
    } else {
        queue.push_back(msg);
    }
}

async fn handle_task_ready(
    result: SendMessageResult,
    send_task: &mut SendMessageTask,
    queue: &mut VecDeque<LocalMessage>,
    timeline_inner: &TimelineInner,
) {
    match result {
        SendMessageResult::Success { room } => {
            if let Some(msg) = queue.pop_front() {
                send_task.start(room, timeline_inner.clone(), msg);
            }
        }
        SendMessageResult::SendingFailed => {
            // Timeline items are marked as failed / cancelled in this case.
            // Clear the timeline and wait for the user to explicitly retry.
            queue.clear();
        }
        SendMessageResult::TaskError { join_error, txn_id } => {
            error!("Message-sending task failed: {join_error}");
            queue.clear();

            let send_state = EventSendState::SendingFailed {
                // FIXME: Probably not exactly right
                error: Arc::new(matrix_sdk::Error::InconsistentState),
            };
            timeline_inner.update_event_send_state(&txn_id, send_state).await;
        }
    }
}

/// Result of [`SendMessageTask`].
enum SendMessageResult {
    /// The message was sent successfully, and the local echo was updated to
    /// indicate this.
    Success {
        /// The joined room object, used to start sending of the next message
        /// in the queue, if it isn't empty.
        room: room::Common,
    },
    /// Sending failed, and the local echo was updated to indicate this.
    SendingFailed,
    /// The [`SendMessageTask`] failed, likely due to a panic.
    ///
    /// This means that the timeline item was likely not updated yet, which thus
    /// becomes the responsibility of the code observing this result.
    TaskError {
        /// The error with which the task failed.
        join_error: JoinError,
        /// The transaction ID of the message that was being sent by the task.
        txn_id: OwnedTransactionId,
    },
}

/// Future that tracks the process of an event-sending background task, if one
/// is currently active for a given message-sending queue.
enum SendMessageTask {
    /// No background task is active right now.
    Idle,
    /// A background task has been spawned, we're waiting for its result.
    Running {
        /// The transaction ID of the message that is being sent.
        txn_id: OwnedTransactionId,
        /// Handle to the task itself.
        join_handle: JoinHandle<Option<room::Common>>,
    },
}

impl SendMessageTask {
    #[must_use]
    fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    fn start(&mut self, room: room::Common, timeline_inner: TimelineInner, msg: LocalMessage) {
        debug!("Spawning message-sending task");
        let txn_id = msg.txn_id.clone();
        let join_handle = spawn(async move {
            let result = room.send(msg.content, Some(&msg.txn_id)).await;
            let (room, send_state) = match result {
                Ok(response) => (Some(room), EventSendState::Sent { event_id: response.event_id }),
                Err(error) => (None, EventSendState::SendingFailed { error: Arc::new(error) }),
            };

            timeline_inner.update_event_send_state(&msg.txn_id, send_state).await;
            room
        });
        *self = Self::Running { txn_id, join_handle };
    }

    fn reset(&mut self) {
        *self = Self::Idle;
    }
}

impl Future for SendMessageTask {
    type Output = SendMessageResult;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut *self {
            SendMessageTask::Idle => Poll::Pending,
            SendMessageTask::Running { txn_id, join_handle } => {
                Pin::new(join_handle).poll(cx).map(|result| {
                    let txn_id = mem::replace(txn_id, OwnedTransactionId::from(""));
                    if txn_id.as_str().is_empty() {
                        warn!("SendMessageTask polled after returning Poll::Ready!");
                    }

                    match result {
                        Ok(Some(room)) => SendMessageResult::Success { room },
                        Ok(None) => SendMessageResult::SendingFailed,
                        Err(join_error) => SendMessageResult::TaskError { join_error, txn_id },
                    }
                })
            }
        }
    }
}
