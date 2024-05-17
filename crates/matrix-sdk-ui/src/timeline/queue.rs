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
    Room,
};
use matrix_sdk_base::RoomState;
use ruma::{events::AnyMessageLikeEventContent, OwnedEventId, OwnedTransactionId};
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
    timeline: TimelineInner,
    room: Room,
    mut msg_receiver: Receiver<LocalMessage>,
) {
    let mut queue = VecDeque::new();
    let mut send_task: SendMessageTask = SendMessageTask::Idle;
    let mut recv_fut: Either<_, Pending<Option<LocalMessage>>> =
        Either::Left(Box::pin(msg_receiver.recv()));

    loop {
        select! {
            send_result = &mut send_task => {
                trace!("SendMessageTask finished");

                send_task.reset();

                handle_send_result(
                    send_result,
                    &mut send_task,
                    &mut queue,
                    &timeline,
                ).await;
            }

            recv_res = &mut recv_fut => {
                recv_fut = if let Some(msg) = recv_res {
                    trace!("Got a LocalMessage");

                    send_or_queue_msg(
                        msg,
                        room.clone(),
                        &mut send_task,
                        &mut queue,
                        &timeline,
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

async fn send_or_queue_msg(
    msg: LocalMessage,
    room: Room,
    send_task: &mut SendMessageTask,
    queue: &mut VecDeque<LocalMessage>,
    timeline: &TimelineInner,
) {
    // Only send the message immediately if there aren't other messages to be sent
    // first, and we're not currently sending a message.
    if !queue.is_empty() || !send_task.is_idle() {
        queue.push_back(msg);
        return;
    }

    if room.state() != RoomState::Joined {
        info!("Refusing to send message, room is not joined");
        timeline
            .update_event_send_state(
                &msg.txn_id,
                EventSendState::SendingFailed {
                    // FIXME: Probably not exactly right
                    error: Arc::new(matrix_sdk::Error::InconsistentState),
                },
            )
            .await;
        return;
    }

    send_task.start(room, msg);
}

async fn handle_send_result(
    send_result: SendMessageResult,
    send_task: &mut SendMessageTask,
    queue: &mut VecDeque<LocalMessage>,
    timeline: &TimelineInner,
) {
    match send_result {
        SendMessageResult::Success { event_id, txn_id } => {
            timeline.update_event_send_state(&txn_id, EventSendState::Sent { event_id }).await;

            // Event was successfully sent, move on to the next queued event.
            if let Some(msg) = queue.pop_front() {
                send_task.start(timeline.room().clone(), msg);
            }
        }

        SendMessageResult::SendingFailed { send_error, txn_id } => {
            timeline
                .update_event_send_state(
                    &txn_id,
                    EventSendState::SendingFailed { error: Arc::new(send_error) },
                )
                .await;

            // Clear the queue and wait for the user to explicitly retry (which will
            // re-append the to-be-sent events in the queue).
            queue.clear();
        }

        SendMessageResult::TaskError { join_error, txn_id } => {
            error!("Message-sending task failed: {join_error}");

            timeline
                .update_event_send_state(
                    &txn_id,
                    EventSendState::SendingFailed {
                        // FIXME: Probably not exactly right
                        error: Arc::new(matrix_sdk::Error::InconsistentState),
                    },
                )
                .await;

            // See above comment in the `SendingFailed` arm.
            queue.clear();
        }
    }
}

/// Result of [`SendMessageTask`].
enum SendMessageResult {
    /// The message was sent successfully.
    Success {
        /// The event id returned by the server.
        event_id: OwnedEventId,
        /// The transaction ID of the message that was being sent by the task.
        txn_id: OwnedTransactionId,
    },

    /// Sending failed.
    SendingFailed {
        /// The reason of the sending failure.
        send_error: matrix_sdk::Error,
        /// The transaction ID of the message that was being sent by the task.
        txn_id: OwnedTransactionId,
    },

    /// The [`SendMessageTask`] failed, likely due to a panic.
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
        task: JoinHandle<SendMessageResult>,
    },
}

impl SendMessageTask {
    #[must_use]
    fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Spawns a task sending the message to the room, and updating the timeline
    /// once the result has been processed.
    fn start(&mut self, room: Room, msg: LocalMessage) {
        debug!("Spawning message-sending task");

        let txn_id = msg.txn_id.clone();

        let task = spawn(async move {
            match room.send(msg.content).with_transaction_id(&msg.txn_id).await {
                Ok(response) => {
                    SendMessageResult::Success { event_id: response.event_id, txn_id: msg.txn_id }
                }
                Err(error) => {
                    SendMessageResult::SendingFailed { send_error: error, txn_id: msg.txn_id }
                }
            }
        });

        *self = Self::Running { txn_id, task };
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

            SendMessageTask::Running { txn_id, task } => Pin::new(task).poll(cx).map(|result| {
                let txn_id = mem::replace(txn_id, OwnedTransactionId::from(""));
                if txn_id.as_str().is_empty() {
                    warn!("SendMessageTask polled after returning Poll::Ready!");
                }
                result.unwrap_or_else(|error| SendMessageResult::TaskError {
                    join_error: error,
                    txn_id,
                })
            }),
        }
    }
}
