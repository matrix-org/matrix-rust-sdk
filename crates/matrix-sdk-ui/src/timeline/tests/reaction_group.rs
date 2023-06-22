// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use ruma::{server_name, EventId, UserId};

use crate::timeline::{
    tests::{ALICE, BOB},
    ReactionGroup,
};

/// The Matrix spec does not allow duplicate annotations to be created but it
/// is still possible for duplicates to be received over federation. And in
/// that case, clients are expected to treat duplicates as a single annotation.
#[test]
fn senders_are_deduplicated() {
    let group = {
        let mut group = ReactionGroup::default();
        insert(&mut group, &ALICE, 3);
        insert(&mut group, &BOB, 2);
        group
    };

    assert_eq!(group.senders().collect::<Vec<_>>(), vec![&ALICE.to_owned(), &BOB.to_owned()]);
}

fn insert(group: &mut ReactionGroup, sender: &UserId, count: u64) {
    for _ in 0..count {
        let event_id = EventId::new(server_name!("dummy.server"));
        group.0.insert((None, Some(event_id)), sender.to_owned());
    }
}
