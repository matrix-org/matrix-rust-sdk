// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{collections::BTreeMap, convert::TryInto};

use olm_rs::sas::OlmSas;
use ruma::{
    events::{
        key::verification::{
            cancel::CancelCode,
            mac::{KeyVerificationMacEventContent, ToDeviceKeyVerificationMacEventContent},
            Relation,
        },
        AnyMessageEventContent, AnyToDeviceEventContent,
    },
    serde::Base64,
    DeviceKeyAlgorithm, DeviceKeyId, UserId,
};
use sha2::{Digest, Sha256};
use tracing::{trace, warn};

use super::{FlowId, OutgoingContent};
use crate::{
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    verification::event_enums::{MacContent, StartContent},
    Emoji, ReadOnlyAccount, ReadOnlyOwnUserIdentity,
};

#[derive(Clone, Debug)]
pub struct SasIds {
    pub account: ReadOnlyAccount,
    pub own_identity: Option<ReadOnlyOwnUserIdentity>,
    pub other_device: ReadOnlyDevice,
    pub other_identity: Option<ReadOnlyUserIdentities>,
}

/// Calculate the commitment for a accept event from the public key and the
/// start event.
///
/// # Arguments
///
/// * `public_key` - Our own ephemeral public key that is used for the
/// interactive verification.
///
/// * `content` - The `m.key.verification.start` event content that started the
/// interactive verification process.
pub fn calculate_commitment(public_key: &Base64, content: &StartContent<'_>) -> Base64 {
    let content = content.canonical_json();
    let content_string = content.to_string();

    Base64::new(
        Sha256::new()
            .chain_update(public_key.encode())
            .chain_update(&content_string)
            .finalize()
            .as_slice()
            .to_owned(),
    )
}

/// Get a tuple of an emoji and a description of the emoji using a number.
///
/// This is taken directly from the [spec]
///
/// # Panics
///
/// The spec defines 64 unique emojis, this function panics if the index is
/// bigger than 63.
///
/// [spec]: https://matrix.org/docs/spec/client_server/latest#sas-method-emoji
fn emoji_from_index(index: u8) -> Emoji {
    match index {
        0 => Emoji { symbol: "🐶", description: "Dog" },
        1 => Emoji { symbol: "🐱", description: "Cat" },
        2 => Emoji { symbol: "🦁", description: "Lion" },
        3 => Emoji { symbol: "🐎", description: "Horse" },
        4 => Emoji { symbol: "🦄", description: "Unicorn" },
        5 => Emoji { symbol: "🐷", description: "Pig" },
        6 => Emoji { symbol: "🐘", description: "Elephant" },
        7 => Emoji { symbol: "🐰", description: "Rabbit" },
        8 => Emoji { symbol: "🐼", description: "Panda" },
        9 => Emoji { symbol: "🐓", description: "Rooster" },
        10 => Emoji { symbol: "🐧", description: "Penguin" },
        11 => Emoji { symbol: "🐢", description: "Turtle" },
        12 => Emoji { symbol: "🐟", description: "Fish" },
        13 => Emoji { symbol: "🐙", description: "Octopus" },
        14 => Emoji { symbol: "🦋", description: "Butterfly" },
        15 => Emoji { symbol: "🌷", description: "Flower" },
        16 => Emoji { symbol: "🌳", description: "Tree" },
        17 => Emoji { symbol: "🌵", description: "Cactus" },
        18 => Emoji { symbol: "🍄", description: "Mushroom" },
        19 => Emoji { symbol: "🌏", description: "Globe" },
        20 => Emoji { symbol: "🌙", description: "Moon" },
        21 => Emoji { symbol: "☁️", description: "Cloud" },
        22 => Emoji { symbol: "🔥", description: "Fire" },
        23 => Emoji { symbol: "🍌", description: "Banana" },
        24 => Emoji { symbol: "🍎", description: "Apple" },
        25 => Emoji { symbol: "🍓", description: "Strawberry" },
        26 => Emoji { symbol: "🌽", description: "Corn" },
        27 => Emoji { symbol: "🍕", description: "Pizza" },
        28 => Emoji { symbol: "🎂", description: "Cake" },
        29 => Emoji { symbol: "❤️", description: "Heart" },
        30 => Emoji { symbol: "😀", description: "Smiley" },
        31 => Emoji { symbol: "🤖", description: "Robot" },
        32 => Emoji { symbol: "🎩", description: "Hat" },
        33 => Emoji { symbol: "👓", description: "Glasses" },
        34 => Emoji { symbol: "🔧", description: "Spanner" },
        35 => Emoji { symbol: "🎅", description: "Santa" },
        36 => Emoji { symbol: "👍", description: "Thumbs up" },
        37 => Emoji { symbol: "☂️", description: "Umbrella" },
        38 => Emoji { symbol: "⌛", description: "Hourglass" },
        39 => Emoji { symbol: "⏰", description: "Clock" },
        40 => Emoji { symbol: "🎁", description: "Gift" },
        41 => Emoji { symbol: "💡", description: "Light Bulb" },
        42 => Emoji { symbol: "📕", description: "Book" },
        43 => Emoji { symbol: "✏️", description: "Pencil" },
        44 => Emoji { symbol: "📎", description: "Paperclip" },
        45 => Emoji { symbol: "✂️", description: "Scissors" },
        46 => Emoji { symbol: "🔒", description: "Lock" },
        47 => Emoji { symbol: "🔑", description: "Key" },
        48 => Emoji { symbol: "🔨", description: "Hammer" },
        49 => Emoji { symbol: "☎️", description: "Telephone" },
        50 => Emoji { symbol: "🏁", description: "Flag" },
        51 => Emoji { symbol: "🚂", description: "Train" },
        52 => Emoji { symbol: "🚲", description: "Bicycle" },
        53 => Emoji { symbol: "✈️", description: "Airplane" },
        54 => Emoji { symbol: "🚀", description: "Rocket" },
        55 => Emoji { symbol: "🏆", description: "Trophy" },
        56 => Emoji { symbol: "⚽", description: "Ball" },
        57 => Emoji { symbol: "🎸", description: "Guitar" },
        58 => Emoji { symbol: "🎺", description: "Trumpet" },
        59 => Emoji { symbol: "🔔", description: "Bell" },
        60 => Emoji { symbol: "⚓", description: "Anchor" },
        61 => Emoji { symbol: "🎧", description: "Headphones" },
        62 => Emoji { symbol: "📁", description: "Folder" },
        63 => Emoji { symbol: "📌", description: "Pin" },
        _ => panic!("Trying to fetch an emoji outside the allowed range"),
    }
}

/// Get the extra info that will be used when we check the MAC of a
/// m.key.verification.key event.
///
/// # Arguments
///
/// * `ids` - The ids that are used for this SAS authentication flow.
///
/// * `flow_id` - The unique id that identifies this SAS verification process.
fn extra_mac_info_receive(ids: &SasIds, flow_id: &str) -> String {
    format!(
        "MATRIX_KEY_VERIFICATION_MAC{first_user}{first_device}\
        {second_user}{second_device}{transaction_id}",
        first_user = ids.other_device.user_id(),
        first_device = ids.other_device.device_id(),
        second_user = ids.account.user_id(),
        second_device = ids.account.device_id(),
        transaction_id = flow_id,
    )
}

/// Get the content for a m.key.verification.mac event.
///
/// Returns a tuple that contains the list of verified devices and the list of
/// verified master keys.
///
/// # Arguments
///
/// * `sas` - The Olm SAS object that can be used to MACs
///
/// * `ids` - The ids that are used for this SAS authentication flow.
///
/// * `flow_id` - The unique id that identifies this SAS verification process.
///
/// * `event` - The m.key.verification.mac event that was sent to us by
/// the other side.
pub fn receive_mac_event(
    sas: &OlmSas,
    ids: &SasIds,
    flow_id: &str,
    sender: &UserId,
    content: &MacContent<'_>,
) -> Result<(Vec<ReadOnlyDevice>, Vec<ReadOnlyUserIdentities>), CancelCode> {
    let mut verified_devices = Vec::new();
    let mut verified_identities = Vec::new();

    let info = extra_mac_info_receive(ids, flow_id);

    trace!(
        "Received a key.verification.mac event from {} {}",
        sender,
        ids.other_device.device_id()
    );

    let mut keys = content.mac().keys().map(|k| k.as_str()).collect::<Vec<_>>();
    keys.sort_unstable();

    let keys = Base64::parse(
        sas.calculate_mac(&keys.join(","), &format!("{}KEY_IDS", &info))
            .expect("Can't calculate SAS MAC"),
    )
    .expect("Can't base64-decode SAS MAC");

    if keys != *content.keys() {
        return Err(CancelCode::KeyMismatch);
    }

    for (key_id, key_mac) in content.mac() {
        trace!(
            "Checking MAC for the key id {} from {} {}",
            key_id,
            sender,
            ids.other_device.device_id()
        );

        let key_id: Box<DeviceKeyId> = match key_id.as_str().try_into() {
            Ok(id) => id,
            Err(_) => continue,
        };

        if let Some(key) = ids.other_device.keys().get(&key_id) {
            let calculated_mac = Base64::parse(
                sas.calculate_mac(key, &format!("{}{}", info, key_id))
                    .expect("Can't calculate SAS MAC"),
            )
            .expect("Can't base64-decode SAS MAC");

            if *key_mac == calculated_mac {
                trace!("Successfully verified the device key {} from {}", key_id, sender);
                verified_devices.push(ids.other_device.clone());
            } else {
                return Err(CancelCode::KeyMismatch);
            }
        } else if let Some(identity) = &ids.other_identity {
            if let Some(key) = identity.master_key().get_key(&key_id) {
                // TODO we should check that the master key signs the device,
                // this way we know the master key also trusts the device
                let calculated_mac = Base64::parse(
                    sas.calculate_mac(key, &format!("{}{}", info, key_id))
                        .expect("Can't calculate SAS MAC"),
                )
                .expect("Can't base64-decode SAS MAC");

                if *key_mac == calculated_mac {
                    trace!("Successfully verified the master key {} from {}", key_id, sender);
                    verified_identities.push(identity.clone())
                } else {
                    return Err(CancelCode::KeyMismatch);
                }
            }
        } else {
            warn!(
                "Key ID {} in MAC event from {} {} doesn't belong to any device \
                or user identity",
                key_id,
                sender,
                ids.other_device.device_id()
            );
        }
    }

    Ok((verified_devices, verified_identities))
}

/// Get the extra info that will be used when we generate a MAC and need to send
/// it out
///
/// # Arguments
///
/// * `ids` - The ids that are used for this SAS authentication flow.
///
/// * `flow_id` - The unique id that identifies this SAS verification process.
fn extra_mac_info_send(ids: &SasIds, flow_id: &str) -> String {
    format!(
        "MATRIX_KEY_VERIFICATION_MAC{first_user}{first_device}\
        {second_user}{second_device}{transaction_id}",
        first_user = ids.account.user_id(),
        first_device = ids.account.device_id(),
        second_user = ids.other_device.user_id(),
        second_device = ids.other_device.device_id(),
        transaction_id = flow_id,
    )
}

/// Get the content for a m.key.verification.mac event.
///
/// # Arguments
///
/// * `sas` - The Olm SAS object that can be used to generate the MAC
///
/// * `ids` - The ids that are used for this SAS authentication flow.
///
/// * `flow_id` - The unique id that identifies this SAS verification process.
///
/// # Panics
///
/// This will panic if the public key of the other side wasn't set.
pub fn get_mac_content(sas: &OlmSas, ids: &SasIds, flow_id: &FlowId) -> OutgoingContent {
    let mut mac: BTreeMap<String, Base64> = BTreeMap::new();

    let key_id = DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, ids.account.device_id());
    let key = ids.account.identity_keys().ed25519();
    let info = extra_mac_info_send(ids, flow_id.as_str());

    mac.insert(
        key_id.to_string(),
        Base64::parse(
            sas.calculate_mac(key, &format!("{}{}", info, key_id))
                .expect("Can't calculate SAS MAC"),
        )
        .expect("Can't base64-decode SAS MAC"),
    );

    if let Some(own_identity) = &ids.own_identity {
        if own_identity.is_verified() {
            if let Some(key) = own_identity.master_key().get_first_key() {
                let key_id = format!("{}:{}", DeviceKeyAlgorithm::Ed25519, &key);

                let calculated_mac = Base64::parse(
                    sas.calculate_mac(key, &format!("{}{}", info, &key_id))
                        .expect("Can't calculate SAS Master key MAC"),
                )
                .expect("Can't base64-decode SAS Master key MAC");

                mac.insert(key_id, calculated_mac);
            }
        }
    }

    // TODO Add the cross signing master key here if we trust/have it.

    let mut keys: Vec<_> = mac.keys().map(|s| s.as_str()).collect();
    keys.sort_unstable();

    let keys = Base64::parse(
        sas.calculate_mac(&keys.join(","), &format!("{}KEY_IDS", &info))
            .expect("Can't calculate SAS MAC"),
    )
    .expect("Can't base64-decode SAS MAC");

    match flow_id {
        FlowId::ToDevice(s) => AnyToDeviceEventContent::KeyVerificationMac(
            ToDeviceKeyVerificationMacEventContent::new(s.clone(), mac, keys),
        )
        .into(),
        FlowId::InRoom(r, e) => (
            r.clone(),
            AnyMessageEventContent::KeyVerificationMac(KeyVerificationMacEventContent::new(
                mac,
                keys,
                Relation::new(e.clone()),
            )),
        )
            .into(),
    }
}

/// Get the extra info that will be used when we generate bytes for the short
/// auth string.
///
/// # Arguments
///
/// * `ids` - The ids that are used for this SAS authentication flow.
///
/// * `flow_id` - The unique id that identifies this SAS verification process.
///
/// * `we_started` - Flag signaling if the SAS process was started on our side.
fn extra_info_sas(
    ids: &SasIds,
    own_pubkey: &str,
    their_pubkey: &str,
    flow_id: &str,
    we_started: bool,
) -> String {
    let our_info = format!("{}|{}|{}", ids.account.user_id(), ids.account.device_id(), own_pubkey);
    let their_info =
        format!("{}|{}|{}", ids.other_device.user_id(), ids.other_device.device_id(), their_pubkey);

    let (first_info, second_info) =
        if we_started { (our_info, their_info) } else { (their_info, our_info) };

    let info = format!(
        "MATRIX_KEY_VERIFICATION_SAS|{first_info}|{second_info}|{flow_id}",
        first_info = first_info,
        second_info = second_info,
        flow_id = flow_id,
    );

    trace!("Generated a SAS extra info: {}", info);

    info
}

/// Get the emoji version of the short authentication string.
///
/// Returns seven tuples where the first element is the emoji and the
/// second element the English description of the emoji.
///
/// # Arguments
///
/// * `sas` - The Olm SAS object that can be used to generate bytes using the
/// shared secret.
///
/// * `ids` - The ids that are used for this SAS authentication flow.
///
/// * `flow_id` - The unique id that identifies this SAS verification process.
///
/// * `we_started` - Flag signaling if the SAS process was started on our side.
///
/// # Panics
///
/// This will panic if the public key of the other side wasn't set.
pub fn get_emoji(
    sas: &OlmSas,
    ids: &SasIds,
    their_pubkey: &str,
    flow_id: &str,
    we_started: bool,
) -> [Emoji; 7] {
    let bytes = sas
        .generate_bytes(
            &extra_info_sas(ids, &sas.public_key(), their_pubkey, flow_id, we_started),
            6,
        )
        .expect("Can't generate bytes");

    bytes_to_emoji(bytes)
}

/// Get the index of the emoji of the short authentication string.
///
/// Returns seven u8 numbers in the range from 0 to 63 inclusive, those numbers
/// can be converted to a unique emoji defined by the spec using the
/// [emoji_from_index](#method.emoji_from_index) method.
///
/// # Arguments
///
/// * `sas` - The Olm SAS object that can be used to generate bytes using the
/// shared secret.
///
/// * `ids` - The ids that are used for this SAS authentication flow.
///
/// * `flow_id` - The unique id that identifies this SAS verification process.
///
/// * `we_started` - Flag signaling if the SAS process was started on our side.
///
/// # Panics
///
/// This will panic if the public key of the other side wasn't set.
pub fn get_emoji_index(
    sas: &OlmSas,
    ids: &SasIds,
    their_pubkey: &str,
    flow_id: &str,
    we_started: bool,
) -> [u8; 7] {
    let bytes = sas
        .generate_bytes(
            &extra_info_sas(ids, &sas.public_key(), their_pubkey, flow_id, we_started),
            6,
        )
        .expect("Can't generate bytes");

    bytes_to_emoji_index(bytes)
}

fn bytes_to_emoji_index(bytes: Vec<u8>) -> [u8; 7] {
    let bytes: Vec<u64> = bytes.iter().map(|b| *b as u64).collect();
    // Join the 6 bytes into one 64 bit unsigned int. This u64 will contain 48
    // bits from our 6 bytes.
    let mut num: u64 = bytes[0] << 40;
    num += bytes[1] << 32;
    num += bytes[2] << 24;
    num += bytes[3] << 16;
    num += bytes[4] << 8;
    num += bytes[5];

    // Take the top 42 bits of our 48 bits from the u64 and convert each 6 bits
    // into a 6 bit number.
    [
        ((num >> 42) & 63) as u8,
        ((num >> 36) & 63) as u8,
        ((num >> 30) & 63) as u8,
        ((num >> 24) & 63) as u8,
        ((num >> 18) & 63) as u8,
        ((num >> 12) & 63) as u8,
        ((num >> 6) & 63) as u8,
    ]
}

fn bytes_to_emoji(bytes: Vec<u8>) -> [Emoji; 7] {
    let numbers = bytes_to_emoji_index(bytes);

    // Convert the 6 bit number into a emoji/description tuple.
    [
        emoji_from_index(numbers[0]),
        emoji_from_index(numbers[1]),
        emoji_from_index(numbers[2]),
        emoji_from_index(numbers[3]),
        emoji_from_index(numbers[4]),
        emoji_from_index(numbers[5]),
        emoji_from_index(numbers[6]),
    ]
}

/// Get the decimal version of the short authentication string.
///
/// Returns a tuple containing three 4 digit integer numbers that represent
/// the short auth string.
///
/// # Arguments
///
/// * `sas` - The Olm SAS object that can be used to generate bytes using the
/// shared secret.
///
/// * `ids` - The ids that are used for this SAS authentication flow.
///
/// * `flow_id` - The unique id that identifies this SAS verification process.
///
/// * `we_started` - Flag signaling if the SAS process was started on our side.
///
/// # Panics
///
/// This will panic if the public key of the other side wasn't set.
pub fn get_decimal(
    sas: &OlmSas,
    ids: &SasIds,
    their_pubkey: &str,
    flow_id: &str,
    we_started: bool,
) -> (u16, u16, u16) {
    let bytes = sas
        .generate_bytes(
            &extra_info_sas(ids, &sas.public_key(), their_pubkey, flow_id, we_started),
            5,
        )
        .expect("Can't generate bytes");

    bytes_to_decimal(bytes)
}

fn bytes_to_decimal(bytes: Vec<u8>) -> (u16, u16, u16) {
    let bytes: Vec<u16> = bytes.into_iter().map(|b| b as u16).collect();

    // This bitwise operation is taken from the [spec]
    // [spec]: https://matrix.org/docs/spec/client_server/latest#sas-method-decimal
    let first = bytes[0] << 5 | bytes[1] >> 3;
    let second = (bytes[1] & 0x7) << 10 | bytes[2] << 2 | bytes[3] >> 6;
    let third = (bytes[3] & 0x3F) << 7 | bytes[4] >> 1;

    (first + 1000, second + 1000, third + 1000)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod test {
    use proptest::prelude::*;
    use ruma::{
        events::key::verification::start::ToDeviceKeyVerificationStartEventContent, serde::Base64,
    };
    use serde_json::json;

    use super::{
        bytes_to_decimal, bytes_to_emoji, bytes_to_emoji_index, calculate_commitment,
        emoji_from_index,
    };
    use crate::{verification::event_enums::StartContent, Emoji};

    #[test]
    fn commitment_calculation() {
        let commitment = Base64::parse("CCQmB4JCdB0FW21FdAnHj/Hu8+W9+Nb0vgwPEnZZQ4g").unwrap();

        let public_key = Base64::parse("Q/NmNFEUS1fS+YeEmiZkjjblKTitrKOAk7cPEumcMlg").unwrap();
        let content = json!({
            "from_device":"XOWLHHFSWM",
            "transaction_id":"bYxBsirjUJO9osar6ST4i2M2NjrYLA7l",
            "method":"m.sas.v1",
            "key_agreement_protocols":["curve25519-hkdf-sha256","curve25519"],
            "hashes":["sha256"],
            "message_authentication_codes":["hkdf-hmac-sha256","hmac-sha256"],
            "short_authentication_string":["decimal","emoji"]
        });

        let content: ToDeviceKeyVerificationStartEventContent =
            serde_json::from_value(content).unwrap();
        let content = StartContent::from(&content);
        let calculated_commitment = calculate_commitment(&public_key, &content);

        assert_eq!(commitment, calculated_commitment);
    }

    #[test]
    fn emoji_generation() {
        let bytes = vec![0, 0, 0, 0, 0, 0];
        let index: Vec<Emoji> =
            vec![0, 0, 0, 0, 0, 0, 0].into_iter().map(emoji_from_index).collect();
        assert_eq!(bytes_to_emoji(bytes), index.as_ref());

        let bytes = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

        let index: Vec<Emoji> =
            vec![63, 63, 63, 63, 63, 63, 63].into_iter().map(emoji_from_index).collect();
        assert_eq!(bytes_to_emoji(bytes), index.as_ref());
    }

    #[test]
    fn decimal_generation() {
        let bytes = vec![0, 0, 0, 0, 0];
        let result = bytes_to_decimal(bytes);

        assert_eq!(result, (1000, 1000, 1000));

        let bytes = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let result = bytes_to_decimal(bytes);
        assert_eq!(result, (9191, 9191, 9191));
    }

    proptest! {
        #[test]
        fn proptest_emoji(bytes in prop::array::uniform6(0u8..)) {
            let numbers = bytes_to_emoji_index(bytes.to_vec());

            for number in numbers {
                prop_assert!(number < 64);
            }
        }
    }

    proptest! {
        #[test]
        fn proptest_decimals(bytes in prop::array::uniform5(0u8..)) {
            let (first, second, third) = bytes_to_decimal(bytes.to_vec());

            prop_assert!((1000..=9191).contains(&first));
            prop_assert!((1000..=9191).contains(&second));
            prop_assert!((1000..=9191).contains(&third));
        }
    }
}
