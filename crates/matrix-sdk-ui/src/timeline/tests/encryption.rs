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
    collections::{BTreeMap, BTreeSet, HashMap},
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
    MemoryStore, QueueWedgeError, RoomInfo, RoomMemberships, StateChanges, StateStore, StoreError,
    assert_next_matches_with_timeout, async_trait,
    crypto::{OlmMachine, decrypt_room_key_export, types::events::UtdCause},
    deserialized_responses::{
        AlgorithmInfo, DecryptedRoomEvent, DisplayName, EncryptionInfo, RawAnySyncOrStrippedState,
        VerificationLevel, VerificationState,
    },
    room::ThreadSubscription,
    store::{
        ChildTransactionId, DependentQueuedRequest, DependentQueuedRequestKind, QueuedRequest,
        QueuedRequestKind, RoomLoadSettings, SentRequestKey, StoreConfig,
    },
    test_utils::test_client_builder,
};
use matrix_sdk_base::{
    MinimalRoomMemberEvent, StateStoreDataKey, StateStoreDataValue,
    deserialized_responses::{TimelineEvent, UnableToDecryptReason},
};
use matrix_sdk_test::{ALICE, BOB, async_test};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedTransactionId,
    OwnedUserId, RoomId, TransactionId, UserId, assign, event_id,
    events::{
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, GlobalAccountDataEventType,
        RoomAccountDataEventType, StateEventType,
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::encrypted::{
            EncryptedEventScheme, MegolmV1AesSha2ContentInit, Relation, Replacement,
            RoomEncryptedEventContent,
        },
    },
    owned_device_id, room_id,
    serde::Raw,
    user_id,
};
use serde_json::{json, value::to_raw_value};
use stream_assert::{assert_next_matches, assert_pending};
use tokio::{spawn, time::sleep};

use super::TestTimeline;
use crate::{
    timeline::{
        EncryptedMessage, MsgLikeContent, MsgLikeKind, TimelineDetails, TimelineItemContent,
        tests::{TestDecryptor, TestRoomDataProvider, TestTimelineBuilder},
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

    let hook = Arc::new(DummyUtdHook::default());
    let client = test_client_builder(None).build().await.unwrap();
    let utd_hook = Arc::new(UtdHookManager::new(hook.clone(), client));

    let own_user_id = user_id!("@example:morheus.localhost");
    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;

    let timeline = TestTimelineBuilder::new()
        .unable_to_decrypt_hook(utd_hook.clone())
        .provider(TestRoomDataProvider::default().with_decryptor(TestDecryptor::new(
            room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost"),
            &olm_machine,
        )))
        .build();

    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline
        .handle_live_event(
            f.event(RoomEncryptedEventContent::new(
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
            .sender(&BOB)
            .into_utd_sync_timeline_event(),
        )
        .await;

    assert_eq!(timeline.controller.items().await.len(), 2);

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
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

    olm_machine.store().import_exported_room_keys(exported_keys, |_, _| {}).await.unwrap();

    timeline
        .controller
        .retry_event_decryption_test(Some(iter::once(SESSION_ID.to_owned()).collect()))
        .await;

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

    let hook = Arc::new(DummyUtdHook::default());
    let client = test_client_builder(None).build().await.unwrap();
    let utd_hook = Arc::new(
        UtdHookManager::new(hook.clone(), client).with_max_delay(Duration::from_millis(500)),
    );

    let own_user_id = user_id!("@example:morheus.localhost");
    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;

    let timeline = TestTimelineBuilder::new()
        .unable_to_decrypt_hook(utd_hook.clone())
        .provider(TestRoomDataProvider::default().with_decryptor(TestDecryptor::new(
            room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost"),
            &olm_machine,
        )))
        .build();

    let f = &timeline.factory;
    timeline
        .handle_live_event(
            f.event(RoomEncryptedEventContent::new(
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
            .sender(&BOB)
            .into_utd_sync_timeline_event(),
        )
        .await;

    sleep(Duration::from_millis(200)).await;

    // Simulate a retry decryption.
    // Due to the regression this was marking the event as successfully decrypted on
    // retry
    timeline
        .controller
        .retry_event_decryption_test(Some(iter::once(SESSION_ID.to_owned()).collect()))
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

    let own_user_id = user_id!("@example:morheus.localhost");
    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_decryptor(TestDecryptor::new(
            room_id!("!bdsREiCPHyZAPkpXer:morpheus.localhost"),
            &olm_machine,
        )))
        .build();

    let f = &timeline.factory;
    let mut stream = timeline.subscribe_events().await;

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
    timeline
        .handle_live_event(
            f.event(RoomEncryptedEventContent::new(encrypted, None))
                .sender(&BOB)
                .into_utd_sync_timeline_event(),
        )
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
    timeline
        .handle_live_event(
            f.event(assign!(RoomEncryptedEventContent::new(encrypted, None), {
                relates_to: Some(Relation::Replacement(Replacement::new(event_id))),
            }))
            .sender(&BOB)
            .into_utd_sync_timeline_event(),
        )
        .await;

    assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { .. });

    // When we provide the keys for them and request redecryption
    let mut keys = decrypt_room_key_export(Cursor::new(SESSION1_KEY), "1234").unwrap();
    keys.extend(decrypt_room_key_export(Cursor::new(SESSION2_KEY), "1234").unwrap());

    olm_machine.store().import_exported_room_keys(keys, |_, _| {}).await.unwrap();

    timeline.controller.retry_event_decryption_test(None).await;

    // Then first, the first item gets decrypted on its own
    assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, .. });

    // And second, they get resolved into a single event after the edit is decrypted
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

    let olm_machine = OlmMachine::new(user_id!("@jptest:matrix.org"), DEVICE_ID.into()).await;

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_decryptor(TestDecryptor::new(
            room_id!("!wFnAUSQbxMcfIMgvNX:flipdot.org"),
            &olm_machine,
        )))
        .build();

    let f = &timeline.factory;
    let mut stream = timeline.subscribe().await;

    // Given the timeline contains an event and an edit of that event, and another
    // event, all UTD.

    timeline
        .handle_live_event(
            f.event(encrypted_message(
                "AwgDEoABQsTrPTYDh22PTmfODR9EucX3qLl3buDcahHPjKJA8QIM+wW0s+e08Zi7/JbLdnZL1VL\
                 jO47HcRhxDTyHZPXPg8wd1l0Qb3irjnCnS7LFAc98+ko18CFJUGNeRZZwzGiorKK5VLMv0WQZI8\
                 mBZdKIaqDTUBFvcvbn2gQaWtUipQdJQRKyv2h0AWveVkv75lp5hRb7jolCi08oMX8cM+V3Zzyi7\
                 mlPAzZjDz0PaRbQwfbMTTHkcL7TZybBi4vLX4f5ZR2Iiysc7gw",
            ))
            .sender(&BOB)
            .into_utd_sync_timeline_event(),
        )
        .await;

    let event_id =
        assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { value } => value)
            .as_event()
            .unwrap()
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
    timeline
        .handle_live_event(
            f.event(assign!(msg2, { relates_to: Some(Relation::Replacement(Replacement::new(event_id))) }))
                .sender(&BOB)
                .into_utd_sync_timeline_event(),
        )
        .await;

    timeline
        .handle_live_event(
            f.event(encrypted_message(
                "AwgFEoABUAwzBLYStHEa1RaZtojePQ6sue9terXNMFufeLKci/UcpOpZC9o3lDxp9rxlNjk4Ii+\
                 fkOeSClib/qxt+wLszeQZVa04bRr6byK1dOhlptvAPjUCcEsaHyMMR1AnjT2vmFlJRGviwN6cvQ\
                 2r/fEvAW/9QB+N6fX4g9729bt5ftXRqa5QI7NA351RNUveRHxVvx+2x0WJArQjYGRk7tMS2rUto\
                 IYt2ZY17nE1UJjN7M87STnCF9c9qy4aGNqIpeVIht6XbtgD7gQ",
            ))
            .sender(&BOB)
            .into_utd_sync_timeline_event(),
        )
        .await;

    // Sanity: these events were added to the timeline
    assert_next_matches_with_timeout!(stream, VectorDiff::PushFront { .. });
    assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { .. });
    assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { .. });

    let keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "testing").unwrap();
    olm_machine.store().import_exported_room_keys(keys, |_, _| {}).await.unwrap();

    timeline
        .controller
        .retry_event_decryption_test(Some(iter::once(SESSION_ID.to_owned()).collect()))
        .await;

    // Then first, the original item got decrypted
    assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 1, value } => value);

    // And second, the edit was decrypted, resulting in us replacing the
    // original+edit with one item
    let edited_item =
        assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 1, value } => value);
    assert_next_matches_with_timeout!(stream, VectorDiff::Remove { index: 2 });

    let edited_event = edited_item.as_event().unwrap();
    assert!(edited_event.latest_edit_json().is_some());
    assert_eq!(edited_event.content().as_message().unwrap().body(), "edited");

    // And third, the last item was decrypted
    let normal_item =
        assert_next_matches_with_timeout!(stream, VectorDiff::Set { index:2, value } => value);

    assert_eq!(
        normal_item.as_event().unwrap().content().as_message().unwrap().body(),
        "Another message"
    );

    // (There are no more items)
    assert_pending!(stream);
}

#[async_test]
async fn test_retry_message_decryption_highlighted() {
    const SESSION_ID: &str = "C25PoE+4MlNidQD0YU5ibZqHawV0zZ/up7R8vYJBYTY";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        AUBvCG7VHqpYOpNJoIVxsTS1Qyu83w6xFDw67qDe1edSAAAACrnzwQzFMw//BB9iNKTviUfGPEKD9XlL9f8N\
        svGCe971WnKLqWJjtrc42UfyDXH0fz4HXeCN1b104GlzWVFp0r+9RuQpPsP3IZ1DxWPm/xsotr3N4BY3pdgK\
        wpbCq3oD9bQ0jcYqajrWfmEagSInobo9jd6CPyj6kz7mU/SXwva+aoYB8fVJptdYbIXQbvD8t9vS5SC6ZGlP\
        CpcJBscXIq79HpWgDjnfvUNZiITlazFcgPB8zI78MwISm4FX/4KAwxjWf0eGNwKPiTP8fjXpxKurgnMQEET/\
        nVb/r4yIO1Z8rM6vmzoTcQvUc5pXmAGhcLGWN6Q06D3hBuWw0etCKRW5bqcMRit5wmawvBV6j+QNKSPKy7xQ\
        zQhzx9TFfgGZ7rRsl9EPxn0FB/EJNHOkbqYqOmKix9jbh820jRG9i4vD+x+U6iXGpRPyb2S8w+1f9n3uH3yI\
        0XWypoX/eEh7cJv9YChq4Wst4UkP2l6ztP8H/dWXfDYHddkMMKnveeb3sjWRjJep7Ih3W5PyMmxfge85DryB\
        Sgvx6TKvtiC4zOKp1VStbXNgrpipWixhXP2F8BkDmJJvDYO1idWU2NbDJZY6AkKockUscnovpmV1yhovm83Y\
        sAZRyV3W2MlFpA5qAgdXWlBA4WZ/jus/Mey0dqFZtvDS6fC1S4cx5p6hXBwADLRjIiqq2dpn49+aUwqPMn/b\
        FM8H2PpVkKgrA+tx8LNQD+FWDfp6MyhmEJEvk9r5vU9LtTXtZl4toYvNY0UHUBbZj2xF9U9Z9A\n\
        -----END MEGOLM SESSION DATA-----";

    let own_user_id = user_id!("@example:matrix.org");
    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_decryptor(TestDecryptor::new(
            room_id!("!rYtFvMGENJleNQVJzb:matrix.org"),
            &olm_machine,
        )))
        .build();

    let f = &timeline.factory;
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_event(
            f.event(RoomEncryptedEventContent::new(
                EncryptedEventScheme::MegolmV1AesSha2(
                    MegolmV1AesSha2ContentInit {
                        ciphertext: "\
                            AwgAEpABNOd7Rxpc/98gaaOanApQ/h40uNyYE/aiFd8PKeQPH65bwuxBy/glodmteryH\
                            4t5d0cKSPjb+996yK90+A8YUevQKBuC+/+4iRF2CSqMNvArdOCnFHJdZBuCyRP6W82DZ\
                            sR1w5X/tKGs/A9egJdxomLCzMRZarayTXUlgMT8Kj7E9zKOgyLEZGki6Y9IPybfrU3+S\
                            b4VbF7RKY395/lIZFiLvJ5hUT+Ao1k13opeTE9GHtdOK0GzQPVFLnN61pRa3K/vV9Otk\
                            D0QbVS/4mE3C29+yIC1lEkwA"
                            .to_owned(),
                        sender_key: "peI8cfSKqZvTOAfY0Od2e7doDpJ1cxdBsOhSceTLU3E".to_owned(),
                        device_id: "KDCTEHOVSS".into(),
                        session_id: SESSION_ID.into(),
                    }
                    .into(),
                ),
                None,
            ))
            .sender(&BOB)
            .into_utd_sync_timeline_event(),
        )
        .await;

    assert_eq!(timeline.controller.items().await.len(), 2);

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
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

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();

    olm_machine.store().import_exported_room_keys(exported_keys, |_, _| {}).await.unwrap();

    timeline
        .controller
        .retry_event_decryption_test(Some(iter::once(SESSION_ID.to_owned()).collect()))
        .await;

    assert_eq!(timeline.controller.items().await.len(), 2);

    let item =
        assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 1, value } => value);
    let event = item.as_event().unwrap();
    assert_matches!(event.encryption_info(), Some(_));
    assert_let!(Some(message) = event.content().as_message());
    assert_eq!(message.body(), "A secret to everybody but Alice");
    assert!(event.is_highlighted());
}

#[async_test]
async fn test_retry_fetching_encryption_info() {
    const SESSION_ID: &str = "C25PoE+4MlNidQD0YU5ibZqHawV0zZ/up7R8vYJBYTY";
    let sender = user_id!("@sender:s.co");
    let room_id = room_id!("!room:s.co");

    let own_user_id = user_id!("@me:s.co");
    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;

    // Given when I ask the room for new encryption info for any session, it will
    // say "verified"
    let verified_encryption_info = make_encryption_info(SESSION_ID, VerificationState::Verified);

    let provider = TestRoomDataProvider::default()
        .with_encryption_info(SESSION_ID, verified_encryption_info)
        .with_decryptor(TestDecryptor::new(room_id, &olm_machine));

    let timeline = TestTimelineBuilder::new().provider(provider).build();

    let f = &timeline.factory;
    let mut stream = timeline.subscribe_events().await;

    // But right now the timeline contains 2 events whose info says "unverified"
    // One is linked to SESSION_ID, the other is linked to some other session.
    let timeline_event_this_session = TimelineEvent::from_decrypted(
        DecryptedRoomEvent {
            event: f.text_msg("foo").sender(sender).room(room_id).into_raw(),
            encryption_info: make_encryption_info(
                SESSION_ID,
                VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            ),
            unsigned_encryption_info: None,
        },
        None,
    );
    let timeline_event_other_session = TimelineEvent::from_decrypted(
        DecryptedRoomEvent {
            event: f.text_msg("foo").sender(sender).room(room_id).into_raw(),
            encryption_info: make_encryption_info(
                "other_session_id",
                VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            ),
            unsigned_encryption_info: None,
        },
        None,
    );
    timeline.handle_live_event(timeline_event_this_session).await;
    timeline.handle_live_event(timeline_event_other_session).await;

    // Sanity: the events come through as unverified
    assert_eq!(timeline.controller.items().await.len(), 3);
    {
        let event = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
        let fetched_encryption_info = event.as_remote().unwrap().encryption_info.as_ref().unwrap();
        assert_matches!(
            fetched_encryption_info.verification_state,
            VerificationState::Unverified(_)
        );
    }
    {
        let event = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
        let fetched_encryption_info = event.as_remote().unwrap().encryption_info.as_ref().unwrap();
        assert_matches!(
            fetched_encryption_info.verification_state,
            VerificationState::Unverified(_)
        );
    }

    // When we retry the session with ID SESSION_ID
    timeline
        .controller
        .retry_event_decryption_test(Some(iter::once(SESSION_ID.to_owned()).collect()))
        .await;

    // Then the event in that session has been updated to be verified
    let event =
        assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 0, value } => value);

    let fetched_encryption_info = event.as_remote().unwrap().encryption_info.as_ref().unwrap();
    assert_matches!(fetched_encryption_info.verification_state, VerificationState::Verified);

    assert_eq!(timeline.controller.items().await.len(), 3);

    // But the other one is unchanged because it was for a different session - no
    // other updates are waiting
    assert_pending!(stream);
}

fn make_encryption_info(
    session_id: &str,
    verification_state: VerificationState,
) -> Arc<EncryptionInfo> {
    Arc::new(EncryptionInfo {
        sender: BOB.to_owned(),
        sender_device: Some(owned_device_id!("BOBDEVICE")),
        algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: Default::default(),
            sender_claimed_keys: Default::default(),
            session_id: Some(session_id.to_owned()),
        },
        verification_state,
    })
}

#[async_test]
async fn test_utd_cause_for_nonmember_event_is_found() {
    // Given a timline
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
    // Given a timline
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
    // Given a timline
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
    // Given a timline
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
async fn test_retry_decryption_updates_response() {
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

    let own_user_id = user_id!("@example:morheus.localhost");
    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_decryptor(TestDecryptor::new(
            room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost"),
            &olm_machine,
        )))
        .build();

    let mut stream = timeline.subscribe_events().await;

    let original_event_id = event_id!("$original");
    let f = &timeline.factory;
    timeline
        .handle_live_event(
            f.event(RoomEncryptedEventContent::new(
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
            .sender(&BOB)
            .into_utd_sync_timeline_event(),
        )
        .await;

    timeline
        .handle_live_event(f.text_msg("well said!").reply_to(original_event_id).sender(&ALICE))
        .await;

    // We receive the UTD.
    {
        let event = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
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

    // We receive the text response.
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

    olm_machine.store().import_exported_room_keys(exported_keys, |_, _| {}).await.unwrap();

    // Retry decrypting the UTD.
    timeline
        .controller
        .retry_event_decryption_test(Some(iter::once(SESSION_ID.to_owned()).collect()))
        .await;

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

/// See https://github.com/matrix-org/matrix-rust-sdk/issues/5474 - quoting poljar:
/// Since in our sliding sync world we have two syncs running, one for the
/// events and one for the to-device messages, I think we have a race:
///
/// 1. The event is received, we try to decrypt it but there's no key yet.
/// 2. The room key is received, we notify that a key has been received.
/// 3. The event is pushed to the timeline.
///
/// This test checks that the event is decrypted even in this case.
#[async_test]
async fn test_event_is_redecrypted_even_if_key_arrives_between_receiving_and_adding_to_timeline() {
    /// A state store that refuses to complete calls to `set_kv_data` until
    /// someone calls `stop_delaying`. Other than that, it works like
    /// MemoryStore.
    #[derive(Debug)]
    struct DelayingStore {
        memory_store: MemoryStore,
        delaying: Mutex<bool>,
    }

    impl DelayingStore {
        fn new() -> Self {
            Self { memory_store: MemoryStore::new(), delaying: Mutex::new(true) }
        }

        fn stop_delaying(&self) {
            *self.delaying.lock().unwrap() = false
        }
    }

    #[async_trait]
    impl StateStore for DelayingStore {
        type Error = StoreError;

        async fn get_kv_data(
            &self,
            key: StateStoreDataKey<'_>,
        ) -> matrix_sdk_base::store::Result<Option<StateStoreDataValue>> {
            self.memory_store.get_kv_data(key).await
        }

        async fn set_kv_data(
            &self,
            key: StateStoreDataKey<'_>,
            value: StateStoreDataValue,
        ) -> matrix_sdk_base::store::Result<()> {
            // This is the key behaviour of this store - we wait to set this value until
            // someone calls `stop_delaying`.
            //
            // We use `sleep` here for simplicity. The cool way would be to use a custom
            // waker or something like that.
            while *self.delaying.lock().unwrap() {
                sleep(Duration::from_millis(10)).await;
            }

            self.memory_store.set_kv_data(key, value).await
        }

        async fn remove_kv_data(
            &self,
            key: StateStoreDataKey<'_>,
        ) -> matrix_sdk_base::store::Result<()> {
            self.memory_store.remove_kv_data(key).await
        }

        async fn save_changes(&self, changes: &StateChanges) -> matrix_sdk_base::store::Result<()> {
            self.memory_store.save_changes(changes).await
        }

        async fn get_presence_event(
            &self,
            user_id: &UserId,
        ) -> matrix_sdk_base::store::Result<Option<Raw<PresenceEvent>>> {
            self.memory_store.get_presence_event(user_id).await
        }

        async fn get_presence_events(
            &self,
            user_ids: &[OwnedUserId],
        ) -> matrix_sdk_base::store::Result<Vec<Raw<PresenceEvent>>> {
            self.memory_store.get_presence_events(user_ids).await
        }

        async fn get_state_event(
            &self,
            room_id: &RoomId,
            event_type: StateEventType,
            state_key: &str,
        ) -> matrix_sdk_base::store::Result<Option<RawAnySyncOrStrippedState>> {
            self.memory_store.get_state_event(room_id, event_type, state_key).await
        }

        async fn get_state_events(
            &self,
            room_id: &RoomId,
            event_type: StateEventType,
        ) -> matrix_sdk_base::store::Result<Vec<RawAnySyncOrStrippedState>> {
            self.memory_store.get_state_events(room_id, event_type).await
        }

        async fn get_state_events_for_keys(
            &self,
            room_id: &RoomId,
            event_type: StateEventType,
            state_keys: &[&str],
        ) -> matrix_sdk_base::store::Result<Vec<RawAnySyncOrStrippedState>, Self::Error> {
            self.memory_store.get_state_events_for_keys(room_id, event_type, state_keys).await
        }

        async fn get_profile(
            &self,
            room_id: &RoomId,
            user_id: &UserId,
        ) -> matrix_sdk_base::store::Result<Option<MinimalRoomMemberEvent>> {
            self.memory_store.get_profile(room_id, user_id).await
        }

        async fn get_profiles<'a>(
            &self,
            room_id: &RoomId,
            user_ids: &'a [OwnedUserId],
        ) -> matrix_sdk_base::store::Result<BTreeMap<&'a UserId, MinimalRoomMemberEvent>> {
            self.memory_store.get_profiles(room_id, user_ids).await
        }

        async fn get_user_ids(
            &self,
            room_id: &RoomId,
            memberships: RoomMemberships,
        ) -> matrix_sdk_base::store::Result<Vec<OwnedUserId>> {
            self.memory_store.get_user_ids(room_id, memberships).await
        }

        async fn get_room_infos(
            &self,
            room_load_settings: &RoomLoadSettings,
        ) -> matrix_sdk_base::store::Result<Vec<RoomInfo>> {
            self.memory_store.get_room_infos(room_load_settings).await
        }

        async fn get_users_with_display_name(
            &self,
            room_id: &RoomId,
            display_name: &DisplayName,
        ) -> matrix_sdk_base::store::Result<BTreeSet<OwnedUserId>> {
            self.memory_store.get_users_with_display_name(room_id, display_name).await
        }

        async fn get_users_with_display_names<'a>(
            &self,
            room_id: &RoomId,
            display_names: &'a [DisplayName],
        ) -> matrix_sdk_base::store::Result<HashMap<&'a DisplayName, BTreeSet<OwnedUserId>>>
        {
            self.memory_store.get_users_with_display_names(room_id, display_names).await
        }

        async fn get_account_data_event(
            &self,
            event_type: GlobalAccountDataEventType,
        ) -> matrix_sdk_base::store::Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
            self.memory_store.get_account_data_event(event_type).await
        }

        async fn get_room_account_data_event(
            &self,
            room_id: &RoomId,
            event_type: RoomAccountDataEventType,
        ) -> matrix_sdk_base::store::Result<Option<Raw<AnyRoomAccountDataEvent>>> {
            self.memory_store.get_room_account_data_event(room_id, event_type).await
        }

        async fn get_user_room_receipt_event(
            &self,
            room_id: &RoomId,
            receipt_type: ReceiptType,
            thread: ReceiptThread,
            user_id: &UserId,
        ) -> matrix_sdk_base::store::Result<Option<(OwnedEventId, Receipt)>> {
            self.memory_store
                .get_user_room_receipt_event(room_id, receipt_type, thread, user_id)
                .await
        }

        async fn get_event_room_receipt_events(
            &self,
            room_id: &RoomId,
            receipt_type: ReceiptType,
            thread: ReceiptThread,
            event_id: &EventId,
        ) -> matrix_sdk_base::store::Result<Vec<(OwnedUserId, Receipt)>> {
            self.memory_store
                .get_event_room_receipt_events(room_id, receipt_type, thread, event_id)
                .await
        }

        async fn get_custom_value(
            &self,
            key: &[u8],
        ) -> matrix_sdk_base::store::Result<Option<Vec<u8>>> {
            self.memory_store.get_custom_value(key).await
        }

        async fn set_custom_value(
            &self,
            key: &[u8],
            value: Vec<u8>,
        ) -> matrix_sdk_base::store::Result<Option<Vec<u8>>> {
            self.memory_store.set_custom_value(key, value).await
        }

        async fn remove_custom_value(
            &self,
            key: &[u8],
        ) -> matrix_sdk_base::store::Result<Option<Vec<u8>>> {
            self.memory_store.remove_custom_value(key).await
        }

        async fn remove_room(&self, room_id: &RoomId) -> matrix_sdk_base::store::Result<()> {
            self.memory_store.remove_room(room_id).await
        }

        async fn save_send_queue_request(
            &self,
            room_id: &RoomId,
            transaction_id: OwnedTransactionId,
            created_at: MilliSecondsSinceUnixEpoch,
            kind: QueuedRequestKind,
            priority: usize,
        ) -> matrix_sdk_base::store::Result<(), Self::Error> {
            self.memory_store
                .save_send_queue_request(room_id, transaction_id, created_at, kind, priority)
                .await
        }

        async fn update_send_queue_request(
            &self,
            room_id: &RoomId,
            transaction_id: &TransactionId,
            kind: QueuedRequestKind,
        ) -> matrix_sdk_base::store::Result<bool, Self::Error> {
            self.memory_store.update_send_queue_request(room_id, transaction_id, kind).await
        }

        async fn remove_send_queue_request(
            &self,
            room_id: &RoomId,
            transaction_id: &TransactionId,
        ) -> matrix_sdk_base::store::Result<bool, Self::Error> {
            self.memory_store.remove_send_queue_request(room_id, transaction_id).await
        }

        async fn load_send_queue_requests(
            &self,
            room_id: &RoomId,
        ) -> matrix_sdk_base::store::Result<Vec<QueuedRequest>, Self::Error> {
            self.memory_store.load_send_queue_requests(room_id).await
        }

        async fn update_send_queue_request_status(
            &self,
            room_id: &RoomId,
            transaction_id: &TransactionId,
            error: Option<QueueWedgeError>,
        ) -> matrix_sdk_base::store::Result<(), Self::Error> {
            self.memory_store.update_send_queue_request_status(room_id, transaction_id, error).await
        }

        async fn load_rooms_with_unsent_requests(
            &self,
        ) -> matrix_sdk_base::store::Result<Vec<OwnedRoomId>, Self::Error> {
            self.memory_store.load_rooms_with_unsent_requests().await
        }

        async fn save_dependent_queued_request(
            &self,
            room_id: &RoomId,
            parent_txn_id: &TransactionId,
            own_txn_id: ChildTransactionId,
            created_at: MilliSecondsSinceUnixEpoch,
            content: DependentQueuedRequestKind,
        ) -> matrix_sdk_base::store::Result<(), Self::Error> {
            self.memory_store
                .save_dependent_queued_request(
                    room_id,
                    parent_txn_id,
                    own_txn_id,
                    created_at,
                    content,
                )
                .await
        }

        async fn mark_dependent_queued_requests_as_ready(
            &self,
            room_id: &RoomId,
            parent_txn_id: &TransactionId,
            sent_parent_key: SentRequestKey,
        ) -> matrix_sdk_base::store::Result<usize, Self::Error> {
            self.memory_store
                .mark_dependent_queued_requests_as_ready(room_id, parent_txn_id, sent_parent_key)
                .await
        }

        async fn update_dependent_queued_request(
            &self,
            room_id: &RoomId,
            own_transaction_id: &ChildTransactionId,
            new_content: DependentQueuedRequestKind,
        ) -> matrix_sdk_base::store::Result<bool, Self::Error> {
            self.memory_store
                .update_dependent_queued_request(room_id, own_transaction_id, new_content)
                .await
        }

        async fn remove_dependent_queued_request(
            &self,
            room: &RoomId,
            own_txn_id: &ChildTransactionId,
        ) -> matrix_sdk_base::store::Result<bool, Self::Error> {
            self.memory_store.remove_dependent_queued_request(room, own_txn_id).await
        }

        async fn load_dependent_queued_requests(
            &self,
            room: &RoomId,
        ) -> matrix_sdk_base::store::Result<Vec<DependentQueuedRequest>, Self::Error> {
            self.memory_store.load_dependent_queued_requests(room).await
        }

        async fn upsert_thread_subscription(
            &self,
            room: &RoomId,
            thread_id: &EventId,
            subscription: ThreadSubscription,
        ) -> matrix_sdk_base::store::Result<(), Self::Error> {
            self.memory_store.upsert_thread_subscription(room, thread_id, subscription).await
        }

        async fn load_thread_subscription(
            &self,
            room: &RoomId,
            thread_id: &EventId,
        ) -> matrix_sdk_base::store::Result<Option<ThreadSubscription>, Self::Error> {
            self.memory_store.load_thread_subscription(room, thread_id).await
        }

        async fn remove_thread_subscription(
            &self,
            room: &RoomId,
            thread_id: &EventId,
        ) -> matrix_sdk_base::store::Result<(), Self::Error> {
            self.memory_store.remove_thread_subscription(room, thread_id).await
        }
    }

    //// 1. The event is received, we try to decrypt it but there's no key yet.
    ////    Because we're using the DelayingStore, it gets stuck after being
    ////    received but before we add it to the timeline. It actually gets stuck
    ////    inside `UtdHookManager::report_utd` but that is not important to this
    ////    test, so long as it is stuck somewhere.
    let utd_hook = Arc::new(DummyUtdHook::default());
    let state_store = Arc::new(DelayingStore::new());
    let store_config = StoreConfig::new(
        "test_event_is_redecrypted_even_if_key_arrives_between_receiving_and_adding_to_timeline"
            .to_owned(),
    )
    .state_store(state_store.clone());

    let client = test_client_builder(None).store_config(store_config).build().await.unwrap();

    //// 2. The room key is received, we notify that a key has been received.
    //// 3. The event is pushed to the timeline.

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

    let own_user_id = user_id!("@example:morheus.localhost");
    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;

    let utd_hook_manager = Arc::new(UtdHookManager::new(utd_hook, client));

    let timeline = Arc::new(
        TestTimelineBuilder::new()
            .unable_to_decrypt_hook(utd_hook_manager)
            .provider(TestRoomDataProvider::default().with_decryptor(TestDecryptor::new(
                room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost"),
                &olm_machine,
            )))
            .build(),
    );

    let mut stream = timeline.subscribe_events().await;

    let original_event_id = event_id!("$original");

    let timeline_clone = timeline.clone();
    let handle_event_handle = spawn(async move {
        let f = &timeline_clone.factory;
        timeline_clone
            .handle_live_event(
                f.event(RoomEncryptedEventContent::new(
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
                .sender(&BOB)
                .into_utd_sync_timeline_event(),
            )
            .await
    });

    // The timeline has received the event and it is stuck in the DelayingStore, so
    // the event won't actually appear in the stream yet.
    assert_pending!(stream);

    // Import a room key backup.
    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();
    olm_machine.store().import_exported_room_keys(exported_keys, |_, _| {}).await.unwrap();

    // Now we have the key we need to decrypt the event.
    // Unblock the timeline from receiving the event.
    state_store.stop_delaying();
    handle_event_handle.await.expect("Failed to wait for the handle_event handle");

    // The event appears, and it is UTD as expected.
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

    // And this is what we are testing: we triggered a redecryption, so it later
    // appears as decrypted, even though the arrival of the key raced with
    // adding the event to the timeline.
    {
        let event = assert_next_matches_with_timeout!(
            stream,
            VectorDiff::Set { index: 0, value } => value
        );
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
