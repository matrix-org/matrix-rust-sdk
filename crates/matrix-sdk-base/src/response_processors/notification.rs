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

use std::collections::BTreeMap;

use ruma::{
    OwnedRoomId,
    push::{Action, PushConditionRoomCtx, Ruleset},
    serde::Raw,
};

use crate::{
    deserialized_responses::RawAnySyncOrStrippedTimelineEvent, store::BaseStateStore, sync,
};

/// A classical set of data used by some processors dealing with notifications
/// and push rules.
pub struct Notification<'a> {
    pub push_rules: &'a Ruleset,
    pub notifications: &'a mut BTreeMap<OwnedRoomId, Vec<sync::Notification>>,
    pub state_store: &'a BaseStateStore,
}

impl<'a> Notification<'a> {
    pub fn new(
        push_rules: &'a Ruleset,
        notifications: &'a mut BTreeMap<OwnedRoomId, Vec<sync::Notification>>,
        state_store: &'a BaseStateStore,
    ) -> Self {
        Self { push_rules, notifications, state_store }
    }

    fn push_notification(
        &mut self,
        room_id: OwnedRoomId,
        actions: Vec<Action>,
        event: RawAnySyncOrStrippedTimelineEvent,
    ) {
        self.notifications.entry(room_id).or_default().push(sync::Notification { actions, event });
    }

    /// Push a new [`sync::Notification`] in [`Self::notifications`] from
    /// `event` if and only if `predicate` returns `true` for at least one of
    /// the [`Action`]s associated to this event and this
    /// `push_condition_room_ctx`. (based on [`Self::push_rules`]).
    ///
    /// This method returns the fetched [`Action`]s.
    pub async fn push_notification_from_event_if<E, P>(
        &mut self,
        push_condition_room_ctx: &PushConditionRoomCtx,
        event: &Raw<E>,
        predicate: P,
    ) -> &[Action]
    where
        Raw<E>: Into<RawAnySyncOrStrippedTimelineEvent>,
        P: Fn(&Action) -> bool,
    {
        let actions = self.push_rules.get_actions(event, push_condition_room_ctx).await;

        if actions.iter().any(predicate) {
            self.push_notification(
                push_condition_room_ctx.room_id.clone(),
                actions.to_owned(),
                event.clone().into(),
            );
        }

        actions
    }
}
