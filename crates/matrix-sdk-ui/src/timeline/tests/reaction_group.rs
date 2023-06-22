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

use ruma::{server_name, user_id, EventId, OwnedEventId, OwnedTransactionId, UserId};

use crate::timeline::{
    tests::{ALICE, BOB},
    ReactionGroup,
};

#[test]
fn by_sender() {
    let alice = ALICE.to_owned();
    let bob = BOB.to_owned();

    let reaction_1 = new_reaction();
    let reaction_2 = new_reaction();

    let mut reaction_group = ReactionGroup::default();
    reaction_group.0.insert(reaction_1.clone(), alice.clone());
    reaction_group.0.insert(reaction_2, bob);

    let alice_reactions = reaction_group.by_sender(&alice).collect::<Vec<_>>();

    let reaction = *alice_reactions.get(0).unwrap();
    assert_eq!(*reaction.1.unwrap(), reaction_1.1.unwrap());
}

#[test]
fn by_sender_with_empty_group() {
    let reaction_group = ReactionGroup::default();

    let reactions = reaction_group.by_sender(&ALICE).collect::<Vec<_>>();

    assert!(reactions.is_empty());
}

#[test]
fn by_sender_with_multiple_users() {
    let alice = ALICE.to_owned();
    let bob = BOB.to_owned();
    let carol = user_id!("@carol:other.server");

    let reaction_1 = new_reaction();
    let reaction_2 = new_reaction();
    let reaction_3 = new_reaction();

    let mut reaction_group = ReactionGroup::default();
    reaction_group.0.insert(reaction_1, alice.clone());
    reaction_group.0.insert(reaction_2, alice.clone());
    reaction_group.0.insert(reaction_3, bob.clone());

    let alice_reactions = reaction_group.by_sender(&alice).collect::<Vec<_>>();
    let bob_reactions = reaction_group.by_sender(&bob).collect::<Vec<_>>();
    let carol_reactions = reaction_group.by_sender(carol).collect::<Vec<_>>();

    assert_eq!(alice_reactions.len(), 2);
    assert_eq!(bob_reactions.len(), 1);
    assert!(carol_reactions.is_empty());
}

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
        group.0.insert(new_reaction(), sender.to_owned());
    }
}

fn new_reaction() -> (Option<OwnedTransactionId>, Option<OwnedEventId>) {
    let event_id = EventId::new(server_name!("example.org"));
    (None, Some(event_id))
}
