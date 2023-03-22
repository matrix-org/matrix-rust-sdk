#![cfg(not(target_arch = "wasm32"))]

use std::{collections::BTreeSet, io::Cursor, iter};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
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
    let mut stream = timeline.subscribe().await;

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

    assert_eq!(timeline.inner.items().await.len(), 2);

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
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

    assert_eq!(timeline.inner.items().await.len(), 2);

    let item =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 1, value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_matches!(event.encryption_info(), Some(_));
    let text = assert_matches!(event.content(), TimelineItemContent::Message(msg) => msg.body());
    assert_eq!(text, "It's a secret to everybody");
    assert!(!event.is_highlighted());
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

    let event_id =
        timeline.inner.items().await[1].as_event().unwrap().event_id().unwrap().to_owned();

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

    let items = timeline.inner.items().await;
    assert_eq!(items.len(), 2);

    let item = items[1].as_event().unwrap().as_remote().unwrap();

    assert_matches!(item.encryption_info(), Some(_));
    let msg = assert_matches!(item.content(), TimelineItemContent::Message(msg) => msg);
    assert!(msg.is_edited());
    assert_eq!(msg.body(), "This is Error");
}

#[async_test]
async fn retry_edit_and_more() {
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

    let timeline = TestTimeline::new();

    timeline
        .handle_live_message_event(
            &BOB,
            encrypted_message(
                "AwgDEoABQsTrPTYDh22PTmfODR9EucX3qLl3buDcahHPjKJA8QIM+wW0s+e08Zi7/JbLdnZL1VL\
                 jO47HcRhxDTyHZPXPg8wd1l0Qb3irjnCnS7LFAc98+ko18CFJUGNeRZZwzGiorKK5VLMv0WQZI8\
                 mBZdKIaqDTUBFvcvbn2gQaWtUipQdJQRKyv2h0AWveVkv75lp5hRb7jolCi08oMX8cM+V3Zzyi7\
                 mlPAzZjDz0PaRbQwfbMTTHkcL7TZybBi4vLX4f5ZR2Iiysc7gw",
            ),
        )
        .await;

    let event_id =
        timeline.inner.items().await[1].as_event().unwrap().event_id().unwrap().to_owned();

    let msg2 = encrypted_message(
        "AwgEErABt7svMEHDYJTjCQEHypR21l34f9IZLNyFaAbI+EiCIN7C8X5iKmkzuYSmGUodyGKbFRYrW9l5dLj\
         35xIRli3SZ6duZpmBI7D4pBGPj2T2Jkc/I9kd/I4EhpvV2emDTioB7jwUfFoATfdA0z/6ciTmU73PStKHZM\
         +WYNxCWZERsCQBtiINzC80FymwLjh4nBhnyW0nlMihGGasakn+3wKQUY0HkVoFM8TXQlCXl1RM2oxL9nn0C\
         dRu2LPArXc5K/1GBSyfluSrdQuA9DciLwVHJB9NwvbZ/7flIkaOC7ahahmk2ws+QeSz8MmHt+9QityK3ZUB\
         4uEzsQ0",
    );
    timeline
        .handle_live_message_event(
            &BOB,
            assign!(msg2, { relates_to: Some(Relation::Replacement(Replacement::new(event_id))) }),
        )
        .await;

    timeline
        .handle_live_message_event(
            &BOB,
            encrypted_message(
                "AwgFEoABUAwzBLYStHEa1RaZtojePQ6sue9terXNMFufeLKci/UcpOpZC9o3lDxp9rxlNjk4Ii+\
                 fkOeSClib/qxt+wLszeQZVa04bRr6byK1dOhlptvAPjUCcEsaHyMMR1AnjT2vmFlJRGviwN6cvQ\
                 2r/fEvAW/9QB+N6fX4g9729bt5ftXRqa5QI7NA351RNUveRHxVvx+2x0WJArQjYGRk7tMS2rUto\
                 IYt2ZY17nE1UJjN7M87STnCF9c9qy4aGNqIpeVIht6XbtgD7gQ",
            ),
        )
        .await;

    assert_eq!(timeline.inner.items().await.len(), 4);

    let olm_machine = OlmMachine::new(user_id!("@jptest:matrix.org"), DEVICE_ID.into()).await;
    let keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "testing").unwrap();
    olm_machine.import_room_keys(keys, false, |_, _| {}).await.unwrap();

    timeline
        .inner
        .retry_event_decryption(
            room_id!("!wFnAUSQbxMcfIMgvNX:flipdot.org"),
            &olm_machine,
            Some(BTreeSet::from_iter([SESSION_ID])),
        )
        .await;

    let timeline_items = timeline.inner.items().await;
    assert_eq!(timeline_items.len(), 3);
    assert!(timeline_items[0].is_day_divider());
    assert_eq!(
        timeline_items[1].as_event().unwrap().content().as_message().unwrap().body(),
        "edited"
    );
    assert_eq!(
        timeline_items[2].as_event().unwrap().content().as_message().unwrap().body(),
        "Another message"
    );
}

#[async_test]
async fn retry_message_decryption_highlighted() {
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

    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(
            &BOB,
            RoomEncryptedEventContent::new(
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
            ),
        )
        .await;

    assert_eq!(timeline.inner.items().await.len(), 2);

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event = item.as_event().unwrap();
    let session_id = assert_matches!(
        event.content(),
        TimelineItemContent::UnableToDecrypt(
            EncryptedMessage::MegolmV1AesSha2 { session_id, .. },
        ) => session_id
    );
    assert_eq!(session_id, SESSION_ID);

    let own_user_id = user_id!("@example:matrix.org");
    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();

    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;
    olm_machine.import_room_keys(exported_keys, false, |_, _| {}).await.unwrap();

    timeline
        .inner
        .retry_event_decryption(
            room_id!("!rYtFvMGENJleNQVJzb:matrix.org"),
            &olm_machine,
            Some(iter::once(SESSION_ID).collect()),
        )
        .await;

    assert_eq!(timeline.inner.items().await.len(), 2);

    let item =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 1, value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_matches!(event.encryption_info(), Some(_));
    let text = assert_matches!(event.content(), TimelineItemContent::Message(msg) => msg.body());
    assert_eq!(text, "A secret to everybody but Alice");
    assert!(event.is_highlighted());
}
