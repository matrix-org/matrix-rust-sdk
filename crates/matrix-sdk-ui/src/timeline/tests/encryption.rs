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

#![cfg(not(target_family = "wasm"))]

use std::{
    io::Cursor,
    iter,
    sync::{Arc, Mutex},
    time::Duration,
};

use as_variant::as_variant;
use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::{
    self, Client, assert_next_matches_with_timeout, assert_next_with_timeout,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::{
    crypto::{decrypt_room_key_export, types::events::UtdCause},
    deserialized_responses::{TimelineEvent, UnableToDecryptReason},
};
use matrix_sdk_test::{ALICE, BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    RoomId, UserId, assign, event_id,
    events::room::encrypted::{
        EncryptedEventScheme, MegolmV1AesSha2ContentInit, Relation, Replacement,
        RoomEncryptedEventContent,
    },
    room_id,
    serde::Raw,
    user_id,
};
use serde_json::{json, value::to_raw_value};
use stream_assert::{assert_next_matches, assert_pending};
use tokio::time::sleep;

use super::TestTimeline;
use crate::{
    timeline::{
        EncryptedMessage, MsgLikeContent, MsgLikeKind, RoomExt, TimelineDetails,
        TimelineItemContent,
    },
    unable_to_decrypt_hook::{UnableToDecryptHook, UnableToDecryptInfo, UtdHookManager},
};

#[derive(Debug, Default)]
struct DummyUtdHook {
    utds: Mutex<Vec<UnableToDecryptInfo>>,
}

impl UnableToDecryptHook for DummyUtdHook {
    fn on_utd(&self, info: UnableToDecryptInfo) {
        self.utds.lock().unwrap().push(info);
    }
}

pub(super) async fn get_client(
    room_id: &RoomId,
    user_id: Option<&UserId>,
) -> (Client, MatrixMockServer, EventFactory) {
    let server = MatrixMockServer::new().await;
    let client = if let Some(user_id) = user_id {
        server
            .client_builder()
            .logged_in_with_token("1234".into(), user_id.into(), "SomeDeviceId".into())
            .build()
            .await
    } else {
        server.client_builder().logged_in_with_oauth().build().await
    };
    client.event_cache().subscribe().unwrap();

    let event_factory = EventFactory::new().room(room_id);
    let member_event = event_factory.member(client.user_id().unwrap()).display_name("Alice");
    server.sync_room(&client, JoinedRoomBuilder::new(room_id).add_state_event(member_event)).await;

    (client, server, event_factory)
}

#[async_test]
async fn test_retry_message_decryption() {
    const SESSION_ID: &str = "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        ASKcWoiAVUM97482UAi83Avce62hSLce7i5JhsqoF6xeAAAACqt2Cg3nyJPRWTTMXxXH7TXnkfdlmBXbQtq5\
        bpHo3LRijcq2Gc6TXilESCmJN14pIsfKRJrWjZ0squ/XsoTFytuVLWwkNaW3QF6obeg2IoVtJXLMPdw3b2vO\
        vgwGY3OMP0XafH13j1vcb6YLzvgLkZQLnYvd47hv3yK/9GmKS9tokuaQ7dCVYckYcIOS09EDTs70YdxUd5WG\
        rQynATCLFP1p/NAGv70r9MK7Cy/mNpjD0r4qC7UEDIoi1kOWzHgnLo19wtvwsb8Fg8ATxcs3Wmtj8hIUYpDx\
        ia4sM10zbytUuaPUAfCDf42IyxdmOnGe1CueXhgI71y+RW0s0argNqUt7jB70JT0o9CyX6UBGRaqLk2MPY9T\
        hUu5J8X3UgIa6rcbWigzohzWm9rdbEHFrSWqjpfQYMaAKQQgETrjSy4XTrp2RhC2oNqG/hylI4ab+F4X6fpH\
        DYP1NqNMP5g36xNu7LhDnrUB5qsPjYOmWORxGLfudpF3oLYCSlr3DgHqEIB6HjQblLZ3KQuPBse3zxyROTnS\
        AhdPH4a/z1wioFtKNVph3hecsiKEdqnz4Y2coSIdhz58mJ9JWNQoFAENE5CSsoEZAGvafYZVpW4C75YY2zq1\
        wIeiFi1dT43/jLAUGkslsi1VvnyfUu8qO404RxYO3XHoGLMFoFLOO+lZ+VGci2Vz10AhxJhEBHxRKxw4k2uB\
        HztoSJUr/2Y\n\
        -----END MEGOLM SESSION DATA-----";

    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let (client, server, event_factory) = get_client(room_id, None).await;

    let hook = Arc::new(DummyUtdHook::default());
    let hook_manager = UtdHookManager::new(hook.clone(), client.clone());

    let room = client.get_room(room_id).unwrap();
    let timeline = room
        .timeline_builder()
        .with_unable_to_decrypt_hook(hook_manager.into())
        .build()
        .await
        .unwrap();
    let (_, mut stream) = timeline.subscribe_filter_map(Some).await;

    let event = event_factory
        .event(RoomEncryptedEventContent::new(
            EncryptedEventScheme::MegolmV1AesSha2(
                MegolmV1AesSha2ContentInit {
                    ciphertext: "\
                            AwgAEtABPRMavuZMDJrPo6pGQP4qVmpcuapuXtzKXJyi3YpEsjSWdzuRKIgJzD4P\
                            cSqJM1A8kzxecTQNJsC5q22+KSFEPxPnI4ltpm7GFowSoPSW9+bFdnlfUzEP1jPq\
                            YevHAsMJp2fRKkzQQbPordrUk1gNqEpGl4BYFeRqKl9GPdKFwy45huvQCLNNueql\
                            CFZVoYMuhxrfyMiJJAVNTofkr2um2mKjDTlajHtr39pTG8k0eOjSXkLOSdZvNOMz\
                            hGhSaFNeERSA2G2YbeknOvU7MvjiO0AKuxaAe1CaVhAI14FCgzrJ8g0y5nly+n7x\
                            QzL2G2Dn8EoXM5Iqj8W99iokQoVsSrUEnaQ1WnSIfewvDDt4LCaD/w7PGETMCQ"
                        .to_owned(),
                    sender_key: "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA".to_owned(),
                    device_id: "NLAZCWIOCO".into(),
                    session_id: SESSION_ID.into(),
                }
                .into(),
            ),
            None,
        ))
        .sender(&BOB);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
        })
        .await;

    let item = assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { value } => value);
    let event = item.as_event().unwrap();
    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::UnableToDecrypt(EncryptedMessage::MegolmV1AesSha2 {
                session_id,
                ..
            }),
            ..
        }) = event.content()
    );
    assert_eq!(session_id, SESSION_ID);

    assert_next_matches!(stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });

    {
        let utds = hook.utds.lock().unwrap();
        assert_eq!(utds.len(), 1);
        assert_eq!(utds[0].event_id, event.event_id().unwrap());
        assert!(utds[0].time_to_decrypt.is_none());
    }

    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();

    client
        .olm_machine_for_testing()
        .await
        .as_ref()
        .unwrap()
        .store()
        .import_exported_room_keys(exported_keys, |_, _| {})
        .await
        .unwrap();

    assert_eq!(timeline.controller.items().await.len(), 2);

    let item = assert_next_matches_with_timeout!(
        stream,
        VectorDiff::Set { index: 1, value } => value
    );
    let event = item.as_event().unwrap();
    assert_matches!(event.encryption_info(), Some(_));
    assert_let!(Some(message) = event.content().as_message());
    assert_eq!(message.body(), "It's a secret to everybody");
    assert!(!event.is_highlighted());

    // The message should not be re-reported as a late decryption.
    {
        let utds = hook.utds.lock().unwrap();
        assert_eq!(utds.len(), 1);

        // The previous UTD report is still there.
        assert_eq!(utds[0].event_id, event.event_id().unwrap());
        assert!(utds[0].time_to_decrypt.is_none());
    }
}

// There has been a regression when the `retry_event_decryption` function
// changed from failing with an Error to instead return a new type of timeline
// event in UTD. The regression caused the timeline to consider any
// re-decryption attempt as successful.
#[async_test]
async fn test_false_positive_late_decryption_regression() {
    const SESSION_ID: &str = "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU";
    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let (client, server, event_factory) = get_client(room_id, None).await;

    let hook = Arc::new(DummyUtdHook::default());
    let utd_hook = Arc::new(
        UtdHookManager::new(hook.clone(), client.clone())
            .with_max_delay(Duration::from_millis(500)),
    );

    let room = client.get_room(room_id).unwrap();
    let timeline = room
        .timeline_builder()
        .with_unable_to_decrypt_hook(utd_hook.clone())
        .build()
        .await
        .unwrap();

    let event = event_factory
        .event(RoomEncryptedEventContent::new(
            EncryptedEventScheme::MegolmV1AesSha2(
                MegolmV1AesSha2ContentInit {
                    ciphertext: "\
                            AwgAEtABPRMavuZMDJrPo6pGQP4qVmpcuapuXtzKXJyi3YpEsjSWdzuRKIgJzD4P\
                            cSqJM1A8kzxecTQNJsC5q22+KSFEPxPnI4ltpm7GFowSoPSW9+bFdnlfUzEP1jPq\
                            YevHAsMJp2fRKkzQQbPordrUk1gNqEpGl4BYFeRqKl9GPdKFwy45huvQCLNNueql\
                            CFZVoYMuhxrfyMiJJAVNTofkr2um2mKjDTlajHtr39pTG8k0eOjSXkLOSdZvNOMz\
                            hGhSaFNeERSA2G2YbeknOvU7MvjiO0AKuxaAe1CaVhAI14FCgzrJ8g0y5nly+n7x\
                            QzL2G2Dn8EoXM5Iqj8W99iokQoVsSrUEnaQ1WnSIfewvDDt4LCaD/w7PGETMCQ"
                        .to_owned(),
                    sender_key: "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA".to_owned(),
                    device_id: "NLAZCWIOCO".into(),
                    session_id: SESSION_ID.into(),
                }
                .into(),
            ),
            None,
        ))
        .sender(&BOB);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
        })
        .await;

    sleep(Duration::from_millis(200)).await;

    // Simulate a retry decryption.
    // Due to the regression this was marking the event as successfully decrypted on
    // retry
    timeline
        .controller
        .retry_event_decryption(Some(iter::once(SESSION_ID.to_owned()).collect()))
        .await;
    assert_eq!(timeline.controller.items().await.len(), 2);

    // Wait past the max delay for utd late decryption detection
    sleep(Duration::from_secs(2)).await;

    {
        let utds = hook.utds.lock().unwrap();
        assert_eq!(utds.len(), 1);
        // This is the main thing we're testing: if this wasn't identified as a definite
        // UTD, this would be `Some(..)`.
        assert!(utds[0].time_to_decrypt.is_none());
    }
}

#[async_test]
async fn test_retry_edit_decryption() {
    const SESSION1_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        AXou7bY+PWm0GrxTioyoKTkxAgfrQ5lGIla62WoBMrqWAAAACgXidLIt0gaK5NT3mGigzFAPjh/M0ibXjSvo\
        P9haNoJN2839XPCqHpErqje9x25Vy830vQXu9OpwT/QNgVXoffK6rXvIMvom6V2ElopBSVVHqgJdfqRrlGKH\
        okfW6AE+ApVPk31BclxuUuxCy+Ph9sWBTW3MA64YGog5Ddp2PAz2Vk/iZ9Dcmtf5CDLbhIRsWiLuSEvO56ok\
        8/ZxCsiuI4SXx+hikBs+krMTIHn74NL5ffpIlnPSOVtbiY49wE1SRwVgdeJUO9qjHpQX3fZKldBBC01l0BuB\
        WK+W/f/LlOPgLr9Eac/u66fCK6Y81ziJOyn3l1wQuu3MQvuuJfwOqcljl47/yg6SaoTYhZ3ytHXkkBtYx0E6\
        h+J3dgXvW6r0prqci/0gljDQR7KtWEUhXb0BwPK7ojRZWBIzF9T/5uKOio/hBZJ7MQHXt8S2HGOB+gKuzrG8\
        azLt5EB48zgeciNlvQ5zh+AltVEErbyENhCAOxEMoO2sTjK1WZ58ZZmti8uaEZ2mJOCciAp6QiFFDnx2FiPv\
        5DN4g22qr4A2Z4rFZNgum4VosoDA8hBvqr+G9TN5ZxVyi4IPOlqv7ycf6WGOLB6022HmZMX74KHlimDtiYlv\
        G6q7EyfpmeT5rKs51f83rQNkRzcNXKlK83YwIBxCdv9EQXZ4WATAvRqeVF8/m2qpv58zIHjLmq7irckNDmPF\
        W8aUFGxYXuU\n\
        -----END MEGOLM SESSION DATA-----";

    const SESSION2_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        AbMgil4w2zS9PcZ25f+vdcBdv0/YVaOg52K49DwCmMUkAAAAChEzP9tvnK3jd0NA+BjFfm0zzHYOiu5EyRK/\
        F+2mFmC5vYzSiT6Zcx3dn23cU+BpmkCH/HxFli1TMZ29jLZt/ri6FgwRZtkNqmcRDnPi18xnY1GTDFYtdZEZ\
        8Fv4L29JVOWLgEIGRdH1ct8HAqxxgSCAEcuVY7ns8xjGWKrX6gs2yanF9vUbdMyRHzBqgytzwnXl+sg5TvQS\
        a5Hh8D0eGewv0gWzUVh4PIhpwTxbEJ97k6Dklq2UneJiBo4kmna4uCRz3khq69k0kajIEiqT6eZtwIz0lDDT\
        V+MQz7YUKkFI6Th88VL9/eehcnuYQgefEEbHeb3zvoA6LSJGpvJEPcHaVNpFgnxNlQaDowtb5XMGZfI/YU4O\
        exTiEdtbYSjGnwDEuVUXtFfHCElvrBhvO3MAiXrk1QbZRNzyNUvU+1+ZmPc0IBsDHJiCN/15MKuEWF9kKqt+\
        9FsFoRnKbXwUfDk9azdOtzymiel6xiD7kr5RTEmyxBIbTQukqZSSyTzKcTxiWQyK7HL0vxztf7Vdy7o1qtKo\
        9Q48eyIc4fc3HwcSLz6CqRlJENsuhqdPcovE4TeIrv72/WBFLot+gGFltrhdXeaNdzLo+xTSdIjXRpnPtNob\
        dld8OyD3F7GpNdtMXoNhpQNfeOWca0eKUkL/gJw5T7kNkTwso2t1gfcIezEge1UpigAQxUgVDRLTdZZ+C1mM\
        rHCyB4ElRjU\n\
        -----END MEGOLM SESSION DATA-----";

    let room_id = room_id!("!bdsREiCPHyZAPkpXer:morpheus.localhost");
    let (client, server, event_factory) = get_client(room_id, None).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (initial, mut stream) = timeline.subscribe_filter_map(|e| e.as_event().cloned()).await;
    assert!(initial.is_empty(), "Initially we don't have any events in the timeline");

    // Given there are 2 UTD events in the timeline: one message and one edit of
    // that message
    let encrypted = EncryptedEventScheme::MegolmV1AesSha2(
        MegolmV1AesSha2ContentInit {
            ciphertext: "\
                AwgAEpABqOCAaP6NqXquQcEsrGCVInjRTLHmVH8exqYO0b5Aulhgzqrt6oWVUZCpSRBCnlmvnc96\
                n/wpjlALt6vYUcNr2lMkXpuKuYaQhHx5c4in2OJCkPzGmbpXRRdw6WC25uzzKr5Vi5Fa8B5o1C5E\
                DGgNsJg8jC+cVZbcbVCFisQcLATG8UBDuZUGn3WtVFzw0aHzgxGEc+t4C8J9aWwqwokaEF7fRjTK\
                ma5GZJZKR9KfdmeHR2TsnlnLPiPh5F12hqwd5XaOMQemS2j4pENfxpBlYIy5Wk3FQN0G"
                .to_owned(),
            sender_key: "sKSGv2uD9zUncgL6GiLedvuky3fjVcEz9qVKZkpzN14".to_owned(),
            device_id: "PNQBRWYIJL".into(),
            session_id: "gI3QWFyqg55EDS8d0omSJwDw8ZWBNEGUw8JxoZlzJgU".into(),
        }
        .into(),
    );
    let event = event_factory.event(RoomEncryptedEventContent::new(encrypted, None)).sender(&BOB);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
        })
        .await;

    let event_id =
        assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { value } => value)
            .event_id()
            .unwrap()
            .to_owned();

    let encrypted = EncryptedEventScheme::MegolmV1AesSha2(
        MegolmV1AesSha2ContentInit {
            ciphertext: "\
                AwgAEtABWuWeRLintqVP5ez5kki8sDsX7zSq++9AJo9lELGTDjNKzbF8sowUgg0DaGoP\
                dgWyBmuUxT2bMggwM0fAevtu4XcFtWUx1c/sj1vhekrng9snmXpz4a30N8jhQ7N4WoIg\
                /G5wsPKtOITjUHeon7EKjTPFU7xoYXmxbjDL/9R4hGQdRqogs1hj0ZnWRxNCvr3ahq24\
                E0j8WyBrQXOb2PIHVNfV/9eW8AB744UQXn8FJpmQO8c0Us3YorXtIFrwAtvI3FknD7Lj\
                eeYFpR9oeyZKuzo2Wzp7eiEZt0Lm+xb7Lfp9yY52RhAO7JLlCM4oPff2yXHpUmcjdGsi\
                9Zc9Z92hiILkZoKOSGccYQoLjYlfL8rVsIVvl4tDDQ"
                .to_owned(),
            sender_key: "sKSGv2uD9zUncgL6GiLedvuky3fjVcEz9qVKZkpzN14".to_owned(),
            device_id: "PNQBRWYIJL".into(),
            session_id: "HSRlM67FgLYl0J0l1luflfGwpnFcLKHnNoRqUuIhQ5Q".into(),
        }
        .into(),
    );

    let event = event_factory
        .event(RoomEncryptedEventContent::new(
            encrypted,
            Some(Relation::Replacement(Replacement::new(event_id))),
        ))
        .sender(&BOB);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
        })
        .await;

    assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, .. });
    assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { .. });

    // When we provide the keys for them a redecryption will trigger.
    let mut keys = decrypt_room_key_export(Cursor::new(SESSION1_KEY), "1234").unwrap();
    keys.extend(decrypt_room_key_export(Cursor::new(SESSION2_KEY), "1234").unwrap());

    client
        .olm_machine_for_testing()
        .await
        .as_ref()
        .unwrap()
        .store()
        .import_exported_room_keys(keys, |_, _| {})
        .await
        .unwrap();

    // Then first, the first item gets decrypted on its own
    assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, .. });

    // And second, they get resolved into a single event after the edit is decrypted
    assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, value } => value);
    let item =
        assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, value } => value);

    assert_next_matches_with_timeout!(stream, VectorDiff::Remove { index: 1 });

    assert_matches!(item.encryption_info(), Some(_));
    assert_matches!(item.latest_edit_json(), Some(_));
    assert_let!(Some(msg) = item.content().as_message());
    assert!(msg.is_edited());
    assert_eq!(msg.body(), "This is Error");

    // (There are no more items)
    assert_pending!(stream);
}

#[async_test]
async fn test_retry_edit_and_more() {
    const DEVICE_ID: &str = "MTEGRRVPEN";
    const SENDER_KEY: &str = "NFPM2+ucU3n3sEdbDdwwv48Bsj4AiQ185lGuRFjy+gs";
    const SESSION_ID: &str = "SMNh04luorH5E8J3b4XYuOBFp8dldO5njacq0OFO70o";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        AXT1CtOfPgmZRXEk4st3ZwIGShWtZ6iDW0+fwku7AIonAAAACr31UJxAbryf6bH3eF5y+WrOipWmZ6G/59A3\
        kuCwntIOrdIC5ShTRWo0qmcWHav2TaFBCx7kWFUs1ryFZjzksCB7sRnVhfXsDUgGGKgj0MOESlPH9Px+IOcV\
        B6Dr9rjj2STtapCknlit9FMrOcfQhsV5q+ymZwm1C32Zc3UTEtyxfpXiIVyru4Xsrzti61fDIiWFj7Mie4Wn\
        7YQ8SQ1Q9CZUnOCzflP4Yw+5cXHwMRDcz7/kIPzczCYILLp89G//Uh8QN25tN+oCPhBmTxMxoHhabEwkZ/rK\
        D1T+jXDK/dClfXqDXxjjAhQpcUI0soWeAGEq8nMEE5J2D/42AOpKVYqfq2GPiGoPQk3suy4GtDJQlXZaFuz/\
        l4fmHwB1CJCxMUlgpRJ4PhRHAfJn9zfiskM19/dj/G9foGt8KQBRnnbxDVM4eYuoMJZn7SaQfXFmybBTY+Z/\
        bYGg9FUKn/LyjYc8jqbyXCnddzCHB+YENwEOP3WQQrZccyvjuTv5oB/TqK4yS90phIvkLlqEyJXKxxPnzAvV\
        CArjU7naYXMeVieMqcntbeaXutLftLUIF7KUUCPu357sTKjaAp8z98YfPZBctrHRrx7Oo2t6Wtph0A5N/NwA\
        dSN2ceRzRzkoupc4FCxvH6o6PmmtD9DfxtZsk+HA+8NQhgFpvm/VYalikckW+wGFxB4nn1nVViS4GN5n8fc/\
        Ug\n\
        -----END MEGOLM SESSION DATA-----";

    fn encrypted_message(ciphertext: &str) -> RoomEncryptedEventContent {
        RoomEncryptedEventContent::new(
            EncryptedEventScheme::MegolmV1AesSha2(
                MegolmV1AesSha2ContentInit {
                    ciphertext: ciphertext.into(),
                    sender_key: SENDER_KEY.into(),
                    device_id: DEVICE_ID.into(),
                    session_id: SESSION_ID.into(),
                }
                .into(),
            ),
            None,
        )
    }

    let room_id = room_id!("!wFnAUSQbxMcfIMgvNX:flipdot.org");
    let (client, server, event_factory) = get_client(room_id, None).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe_filter_map(|e| e.as_event().cloned()).await;

    // Given the timeline contains an event and an edit of that event, and another
    // event, all UTD.

    let event = event_factory
        .event(encrypted_message(
            "AwgDEoABQsTrPTYDh22PTmfODR9EucX3qLl3buDcahHPjKJA8QIM+wW0s+e08Zi7/JbLdnZL1VL\
                 jO47HcRhxDTyHZPXPg8wd1l0Qb3irjnCnS7LFAc98+ko18CFJUGNeRZZwzGiorKK5VLMv0WQZI8\
                 mBZdKIaqDTUBFvcvbn2gQaWtUipQdJQRKyv2h0AWveVkv75lp5hRb7jolCi08oMX8cM+V3Zzyi7\
                 mlPAzZjDz0PaRbQwfbMTTHkcL7TZybBi4vLX4f5ZR2Iiysc7gw",
        ))
        .sender(&BOB)
        .event_id(event_id!("$event1:termina.org.uk"));

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
        })
        .await;

    let event_id =
        assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { value } => value)
            .event_id()
            .unwrap()
            .to_owned();

    let msg2 = encrypted_message(
        "AwgEErABt7svMEHDYJTjCQEHypR21l34f9IZLNyFaAbI+EiCIN7C8X5iKmkzuYSmGUodyGKbFRYrW9l5dLj\
         35xIRli3SZ6duZpmBI7D4pBGPj2T2Jkc/I9kd/I4EhpvV2emDTioB7jwUfFoATfdA0z/6ciTmU73PStKHZM\
         +WYNxCWZERsCQBtiINzC80FymwLjh4nBhnyW0nlMihGGasakn+3wKQUY0HkVoFM8TXQlCXl1RM2oxL9nn0C\
         dRu2LPArXc5K/1GBSyfluSrdQuA9DciLwVHJB9NwvbZ/7flIkaOC7ahahmk2ws+QeSz8MmHt+9QityK3ZUB\
         4uEzsQ0",
    );

    let event = event_factory
        .event(assign!(msg2, {relates_to: Some(Relation::Replacement(Replacement::new(event_id)))}))
        .sender(&BOB)
        .event_id(event_id!("$event2:termina.org.uk"));
    let second_event = event_factory
        .event(encrypted_message(
            "AwgFEoABUAwzBLYStHEa1RaZtojePQ6sue9terXNMFufeLKci/UcpOpZC9o3lDxp9rxlNjk4Ii+\
                 fkOeSClib/qxt+wLszeQZVa04bRr6byK1dOhlptvAPjUCcEsaHyMMR1AnjT2vmFlJRGviwN6cvQ\
                 2r/fEvAW/9QB+N6fX4g9729bt5ftXRqa5QI7NA351RNUveRHxVvx+2x0WJArQjYGRk7tMS2rUto\
                 IYt2ZY17nE1UJjN7M87STnCF9c9qy4aGNqIpeVIht6XbtgD7gQ",
        ))
        .sender(&BOB)
        .event_id(event_id!("$event3:termina.org.uk"));
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(event)
                    .add_timeline_event(second_event),
            );
        })
        .await;

    // Sanity: these events were added to the timeline
    assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, .. });
    assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { .. });
    assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 1, .. });
    assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { .. });

    // We have 4 items, 3 UTDs and a date divider.
    let items = timeline.items().await;
    assert_eq!(items.len(), 4);
    assert!(items[0].is_date_divider());

    let keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "testing").unwrap();
    client
        .olm_machine_for_testing()
        .await
        .as_ref()
        .unwrap()
        .store()
        .import_exported_room_keys(keys, |_, _| {})
        .await
        .unwrap();

    // Then first, the original item got decrypted
    assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, value } => value);

    // And second, the edit was decrypted, resulting in us replacing the
    // original+edit with one item
    let edited_event =
        assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_next_matches_with_timeout!(stream, VectorDiff::Remove { index: 1 });

    assert!(edited_event.latest_edit_json().is_some());
    assert_eq!(edited_event.content().as_message().unwrap().body(), "edited");

    // And third, the last item was decrypted
    let normal_item =
        assert_next_matches_with_timeout!(stream, VectorDiff::Set { index:1, value } => value);

    assert_eq!(normal_item.content().as_message().unwrap().body(), "Another message");

    // (There are no more items)
    assert_pending!(stream);

    let items = timeline.items().await;

    // We're left with 3 items, the zeroth one is the date divider and the next two
    // are the messages.
    assert!(items[0].is_date_divider());
    assert_eq!(items.len(), 3);
}

#[async_test]
async fn test_retry_message_decryption_highlighted() {
    const SESSION_ID: &str = "iLmOdBBComUwueq8mKVU1Om5xXzfP3As0T5W6JnmzcU";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        AX24HyDDbSft4ogbfNZNOIfDW77PkIX/pFxHBgMMkU8FAAAAClcahP9R2+HkWpo8ME4+C7BKJlZAhqEZsvfjoqdQVo8\
        1vMJkdINNuG9jdl4DWd3GxpgiJLTmNqfZewG4Fca1RG6X67KNv7XFwreIn38+wjtqaPa6ODnx2C9ia0nyjKw88x1I4m\
        MYeRj8NgMvPmBFk5gQXlUeWw9b0LGUzUwn7JtRjvpygmbTJTerLXvAbBJo2CiFVjTlbG+4w2N++PoqtaWHNBmqqEJNF\
        c2EKnyqkOmHvNkMLAWAkEkbmqSOTjwBYq28PHqY3UTgGafRGkdX+mp0PsexVLEgzNL0SQFNAVaTlnr0WBxnWMGyS88/\
        n9BeI31YTUjb837ZDDKVgXvu0vybchM5MNActoyxzcOYQ/bqK9Cd7l6O3MvJ8iqgbbMkkKJDO1OY3RByaNmDHXRRQhL\
        vLIiPqjLqw6NLZYMTb9Qi5cGKnhehEWafKepSDTB29J6szAlzaWdX3m5abgOhi629IPmshKX5AXrfpGP6O6h3BeOpSb\
        UzcXmEuJJbyi4TbtbTmL4E9kJGWsvs7pobmmp6ndkR2xjHwWdZh9JDzjJvCF4Se61wkmq2tUUQaANch/ORWLxx/Sf3E\
        NFEAmaDU3PAWsce2AR1LweCTCtgqg2veJ/irKn816SZd8p9E4ujwBPtiwHYXVBaKmja2/BnFEKbU3vzBU8R5RIr4VEd\
        C1r72yL/zIx+P220q3gPvLEqglUsES5Hwo5+7y/CKLa5ZvLPby+DUyVJxn9lLdvCvoxcBWzKGMrwEmwnClWPxrUFqaP\
        R36ruSneNIpFsVqemSCVF2irnHWgtYHvH8EUOAbsK8KLMHiCg8HxlkLd5GL1lpBwwR+c\n\
        -----END MEGOLM SESSION DATA-----";

    let room_id = room_id!("!TWUdbkXUixrqPXtWbN:matrix.local");
    let user_id = user_id!("@willow:matrix.local");
    let (client, server, event_factory) = get_client(room_id, Some(user_id)).await;
    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe().await;

    let content = RoomEncryptedEventContent::new(
        EncryptedEventScheme::MegolmV1AesSha2(
            MegolmV1AesSha2ContentInit {
                ciphertext: "\
                            AwgBEuAC3YC8wNxHOlnuXuyoBwRhtbwE+sVm1CMRZylzapX4uHEB/xP8QFoH6yN5KzGi34h\
                            6QEb2b3Y+dwNHzHhuSHhtqGNOncJKT3KoPamXzlapmqpF3EjbfN07M9ZMuRNGC6EMF7cCRN\
                            yy7h4S9erzXs73uRZV9t0dMpk5FJ9/vHFBfEic4p26eQjltnk7CCJ0sMukAsLzkZOPFOdoP\
                            KLOAsvmPskcYCmtvNfMLIHG+e3YMDj/UzQ9mZl69cD8/r9dOdMiYzdhhullqIFgrXZq+e6J\
                            SMiTNLdEk7uu8OgAf/GhVmC2h9vaN3MIPfGcu8Z7Yf9RQ8mfGrxFxESLc+NhRaEuCjX5Tc7\
                            AzPamR41+JV5xHh7FsOFp7a1eLs9MTRHsq1Vfzv02ecQeiUdRtyIVH+IwKAkc336CLnItvy\
                            CQhXEqcFWKqWLg9+LpTeg0IrUhVhRpQiztqN44vH8xfWpCHDOwbFhJ6DV9NTWQUzDkJjZVI\
                            W6pEbevP8tyDbQtSDSfpdSHoEPor7WVV9rp9FFqznXZxH9G39dWxI7h40vKdNTiqKkfQNZc\
                            dwAyDw"
                    .to_owned(),
                sender_key: "RfaXABigv2vPj0TciwpZTBU0uwWg7iSHvRHA2V2NFiM".to_owned(),
                device_id: "KFWPUYHXZA".into(),
                session_id: SESSION_ID.into(),
            }
            .into(),
        ),
        None,
    );
    let event = event_factory.event(content).sender(&BOB);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
        })
        .await;

    assert_eq!(timeline.controller.items().await.len(), 2);

    let updates = assert_next_with_timeout!(stream);
    let item = assert_matches!(&updates[0], VectorDiff::PushBack { value } => value);
    let event = item.as_event().unwrap();
    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::UnableToDecrypt(EncryptedMessage::MegolmV1AesSha2 {
                session_id,
                ..
            }),
            ..
        }) = event.content()
    );
    assert_eq!(session_id, SESSION_ID);

    let date_divider = assert_matches!(&updates[1], VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();
    client
        .olm_machine_for_testing()
        .await
        .as_ref()
        .unwrap()
        .store()
        .import_exported_room_keys(exported_keys, |_, _| {})
        .await
        .unwrap();

    timeline.controller.retry_event_decryption(None).await;

    assert_eq!(timeline.controller.items().await.len(), 2);

    let updates = assert_next_with_timeout!(stream);
    let item = assert_matches!(&updates[0], VectorDiff::Set { index: 1, value } => value);
    let event = item.as_event().unwrap();
    assert_matches!(event.encryption_info(), Some(_));
    assert_let!(Some(message) = event.content().as_message());
    assert_eq!(message.body(), "A secret to everybody but Willow");
    assert!(event.is_highlighted());
}

#[async_test]
async fn test_utd_cause_for_nonmember_event_is_found() {
    // Given a timeline
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // When we add an event with "membership: leave"
    timeline.handle_live_event(utd_event_with_unsigned(json!({ "membership": "leave" }))).await;

    // Then its UTD cause is membership
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event = item.as_event().unwrap();
    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::UnableToDecrypt(EncryptedMessage::MegolmV1AesSha2 { cause, .. }),
            ..
        }) = event.content()
    );
    assert_eq!(*cause, UtdCause::SentBeforeWeJoined);
}

#[async_test]
async fn test_utd_cause_for_nonmember_event_is_found_unstable_prefix() {
    // Given a timeline
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // When we add an event with "io.element.msc4115.membership: leave"
    timeline
        .handle_live_event(utd_event_with_unsigned(
            json!({ "io.element.msc4115.membership": "leave" }),
        ))
        .await;

    // Then its UTD cause is membership
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event = item.as_event().unwrap();
    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::UnableToDecrypt(EncryptedMessage::MegolmV1AesSha2 { cause, .. }),
            ..
        }) = event.content()
    );
    assert_eq!(*cause, UtdCause::SentBeforeWeJoined);
}

#[async_test]
async fn test_utd_cause_for_member_event_is_unknown() {
    // Given a timeline
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // When we add an event with "membership: join"
    timeline.handle_live_event(utd_event_with_unsigned(json!({ "membership": "join" }))).await;

    // Then its UTD cause is membership
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event = item.as_event().unwrap();
    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::UnableToDecrypt(EncryptedMessage::MegolmV1AesSha2 { cause, .. }),
            ..
        }) = event.content()
    );
    assert_eq!(*cause, UtdCause::Unknown);
}

#[async_test]
async fn test_utd_cause_for_missing_membership_is_unknown() {
    // Given a timeline
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // When we add an event with no membership in unsigned
    timeline.handle_live_event(utd_event_with_unsigned(json!({}))).await;

    // Then its UTD cause is membership
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event = item.as_event().unwrap();
    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::UnableToDecrypt(EncryptedMessage::MegolmV1AesSha2 { cause, .. }),
            ..
        }) = event.content()
    );
    assert_eq!(*cause, UtdCause::Unknown);
}

#[async_test]
async fn test_retry_decryption_updates_reply() {
    const SESSION_ID: &str = "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        ASKcWoiAVUM97482UAi83Avce62hSLce7i5JhsqoF6xeAAAACqt2Cg3nyJPRWTTMXxXH7TXnkfdlmBXbQtq5\
        bpHo3LRijcq2Gc6TXilESCmJN14pIsfKRJrWjZ0squ/XsoTFytuVLWwkNaW3QF6obeg2IoVtJXLMPdw3b2vO\
        vgwGY3OMP0XafH13j1vcb6YLzvgLkZQLnYvd47hv3yK/9GmKS9tokuaQ7dCVYckYcIOS09EDTs70YdxUd5WG\
        rQynATCLFP1p/NAGv70r9MK7Cy/mNpjD0r4qC7UEDIoi1kOWzHgnLo19wtvwsb8Fg8ATxcs3Wmtj8hIUYpDx\
        ia4sM10zbytUuaPUAfCDf42IyxdmOnGe1CueXhgI71y+RW0s0argNqUt7jB70JT0o9CyX6UBGRaqLk2MPY9T\
        hUu5J8X3UgIa6rcbWigzohzWm9rdbEHFrSWqjpfQYMaAKQQgETrjSy4XTrp2RhC2oNqG/hylI4ab+F4X6fpH\
        DYP1NqNMP5g36xNu7LhDnrUB5qsPjYOmWORxGLfudpF3oLYCSlr3DgHqEIB6HjQblLZ3KQuPBse3zxyROTnS\
        AhdPH4a/z1wioFtKNVph3hecsiKEdqnz4Y2coSIdhz58mJ9JWNQoFAENE5CSsoEZAGvafYZVpW4C75YY2zq1\
        wIeiFi1dT43/jLAUGkslsi1VvnyfUu8qO404RxYO3XHoGLMFoFLOO+lZ+VGci2Vz10AhxJhEBHxRKxw4k2uB\
        HztoSJUr/2Y\n\
        -----END MEGOLM SESSION DATA-----";

    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let (client, server, event_factory) = get_client(room_id, None).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut stream) = timeline.subscribe_filter_map(|e| e.as_event().cloned()).await;

    let original_event_id = event_id!("$original");

    let event = event_factory
        .event(RoomEncryptedEventContent::new(
            EncryptedEventScheme::MegolmV1AesSha2(
                MegolmV1AesSha2ContentInit {
                    ciphertext: "\
                            AwgAEtABPRMavuZMDJrPo6pGQP4qVmpcuapuXtzKXJyi3YpEsjSWdzuRKIgJzD4P\
                            cSqJM1A8kzxecTQNJsC5q22+KSFEPxPnI4ltpm7GFowSoPSW9+bFdnlfUzEP1jPq\
                            YevHAsMJp2fRKkzQQbPordrUk1gNqEpGl4BYFeRqKl9GPdKFwy45huvQCLNNueql\
                            CFZVoYMuhxrfyMiJJAVNTofkr2um2mKjDTlajHtr39pTG8k0eOjSXkLOSdZvNOMz\
                            hGhSaFNeERSA2G2YbeknOvU7MvjiO0AKuxaAe1CaVhAI14FCgzrJ8g0y5nly+n7x\
                            QzL2G2Dn8EoXM5Iqj8W99iokQoVsSrUEnaQ1WnSIfewvDDt4LCaD/w7PGETMCQ"
                        .to_owned(),
                    sender_key: "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA".to_owned(),
                    device_id: "NLAZCWIOCO".into(),
                    session_id: SESSION_ID.into(),
                }
                .into(),
            ),
            None,
        ))
        .event_id(original_event_id)
        .sender(&BOB);

    let reply = event_factory.text_msg("well said!").reply_to(original_event_id).sender(&ALICE);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id).add_timeline_event(event).add_timeline_event(reply),
            );
        })
        .await;

    // We receive the UTD.
    {
        let event =
            assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { value } => value);

        assert_let!(
            TimelineItemContent::MsgLike(MsgLikeContent {
                kind: MsgLikeKind::UnableToDecrypt(EncryptedMessage::MegolmV1AesSha2 {
                    session_id,
                    ..
                }),
                ..
            }) = event.content()
        );
        assert_eq!(session_id, SESSION_ID);
    }

    // We receive the reply.
    {
        let event = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
        let msglike = event.content().as_msglike().unwrap();
        let msg = msglike.as_message().unwrap();
        assert_eq!(msg.body(), "well said!");

        let reply_details = msglike.in_reply_to.clone().unwrap();
        assert_eq!(reply_details.event_id, original_event_id);

        let replied_to = as_variant!(&reply_details.event, TimelineDetails::Ready).unwrap();
        assert!(replied_to.content.is_unable_to_decrypt());
    }

    // Import a room key backup.
    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();

    client
        .olm_machine_for_testing()
        .await
        .as_ref()
        .unwrap()
        .store()
        .import_exported_room_keys(exported_keys, |_, _| {})
        .await
        .unwrap();

    // The response is updated.
    {
        let event = assert_next_matches_with_timeout!(
            stream,
            VectorDiff::Set { index: 1, value } => value
        );

        let msglike = event.content().as_msglike().unwrap();
        let msg = msglike.as_message().unwrap();
        assert_eq!(msg.body(), "well said!");

        let reply_details = msglike.in_reply_to.clone().unwrap();
        assert_eq!(reply_details.event_id, original_event_id);

        let replied_to = as_variant!(&reply_details.event, TimelineDetails::Ready).unwrap();
        assert_eq!(replied_to.content.as_message().unwrap().body(), "It's a secret to everybody");
    }

    // The event itself is decrypted.
    {
        let event = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
        assert_matches!(event.encryption_info(), Some(_));
        assert_let!(Some(message) = event.content().as_message());
        assert_eq!(message.body(), "It's a secret to everybody");
        assert!(!event.is_highlighted());
    }
}

fn utd_event_with_unsigned(unsigned: serde_json::Value) -> TimelineEvent {
    let raw = Raw::from_json(
        to_raw_value(&json!({
            "event_id": "$myevent",
            "sender": "@u:s",
            "origin_server_ts": 3,
            "type": "m.room.encrypted",
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "NOT_REAL_CIPHERTEXT",
                "sender_key": "SENDER_KEY",
                "device_id": "DEVICE_ID",
                "session_id":  "SESSION_ID",
            },
            "unsigned": unsigned

        }))
        .unwrap(),
    );

    TimelineEvent::from_utd(
        raw,
        matrix_sdk::deserialized_responses::UnableToDecryptInfo {
            session_id: Some("SESSION_ID".into()),
            reason: UnableToDecryptReason::MissingMegolmSession { withheld_code: None },
        },
    )
}
