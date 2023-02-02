#![cfg(not(target_arch = "wasm32"))]

use std::{io::Cursor, iter};

use assert_matches::assert_matches;
use futures_signals::signal_vec::VecDiff;
use futures_util::StreamExt;
use matrix_sdk_base::crypto::{decrypt_room_key_export, OlmMachine};
use matrix_sdk_test::async_test;
use ruma::{
    events::room::encrypted::{
        EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
    },
    room_id, user_id,
};

use super::{TestTimeline, BOB};
use crate::room::timeline::{EncryptedMessage, TimelineItemContent};

#[async_test]
async fn unable_to_decrypt() {
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
