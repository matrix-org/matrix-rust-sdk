#![cfg(not(target_arch = "wasm32"))]

use std::{io::Cursor, iter};

use assert_matches::assert_matches;
use futures_signals::signal_vec::VecDiff;
use futures_util::StreamExt;
use matrix_sdk_base::crypto::{decrypt_room_key_export, OlmMachine};
use matrix_sdk_test::async_test;
use ruma::{
    assign,
    events::room::encrypted::{
        EncryptedEventScheme, MegolmV1AesSha2ContentInit, Relation, Replacement,
        RoomEncryptedEventContent,
    },
    room_id, user_id,
};

use super::{TestTimeline, BOB};
use crate::room::timeline::{EncryptedMessage, TimelineItemContent};

#[async_test]
async fn retry_message_decryption() {
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

    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    timeline
        .handle_live_message_event(
            &BOB,
            RoomEncryptedEventContent::new(
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
            ),
        )
        .await;

    assert_eq!(timeline.inner.items().len(), 2);

    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event = item.as_event().unwrap();
    let session_id = assert_matches!(
        event.content(),
        TimelineItemContent::UnableToDecrypt(
            EncryptedMessage::MegolmV1AesSha2 { session_id, .. },
        ) => session_id
    );
    assert_eq!(session_id, SESSION_ID);

    let own_user_id = user_id!("@example:morheus.localhost");
    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();

    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;
    olm_machine.import_room_keys(exported_keys, false, |_, _| {}).await.unwrap();

    timeline
        .inner
        .retry_event_decryption(
            room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost"),
            &olm_machine,
            Some(iter::once(SESSION_ID).collect()),
        )
        .await;

    assert_eq!(timeline.inner.items().len(), 2);

    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 1, value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_matches!(&event.encryption_info, Some(_));
    let text = assert_matches!(&event.content, TimelineItemContent::Message(msg) => msg.body());
    assert_eq!(text, "It's a secret to everybody");
}

#[async_test]
async fn retry_edit_decryption() {
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

    const SESSION2_ID: &str = "HSRlM67FgLYl0J0l1luflfGwpnFcLKHnNoRqUuIhQ5Q";
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

    let timeline = TestTimeline::new();

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
    timeline.handle_live_message_event(&BOB, RoomEncryptedEventContent::new(encrypted, None)).await;

    let event_id = timeline.inner.items()[1].as_event().unwrap().event_id().unwrap().to_owned();

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
        .handle_live_message_event(
            &BOB,
            assign!(RoomEncryptedEventContent::new(encrypted, None), {
                relates_to: Some(Relation::Replacement(Replacement::new(event_id))),
            }),
        )
        .await;

    let mut keys = decrypt_room_key_export(Cursor::new(SESSION1_KEY), "1234").unwrap();
    keys.extend(decrypt_room_key_export(Cursor::new(SESSION2_KEY), "1234").unwrap());

    let own_user_id = user_id!("@example:morheus.localhost");
    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;
    olm_machine.import_room_keys(keys, false, |_, _| {}).await.unwrap();

    timeline
        .inner
        .retry_event_decryption(
            room_id!("!bdsREiCPHyZAPkpXer:morpheus.localhost"),
            &olm_machine,
            None,
        )
        .await;

    let items = timeline.inner.items();
    assert_eq!(items.len(), 2);

    let item = items[1].as_event().unwrap().as_remote().unwrap();

    assert_matches!(&item.encryption_info, Some(_));
    let msg = assert_matches!(&item.content, TimelineItemContent::Message(msg) => msg);
    assert!(msg.is_edited());
    assert_eq!(msg.body(), "This is Error");
}
