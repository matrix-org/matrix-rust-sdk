use std::{collections::BTreeMap, convert::TryInto};

use olm_rs::sas::OlmSas;

use matrix_sdk_common::{
    api::r0::{
        keys::{AlgorithmAndDeviceId, KeyAlgorithm},
        to_device::{send_event_to_device::Request as ToDeviceRequest, DeviceIdOrAllDevices},
    },
    events::{
        key::verification::{cancel::CancelCode, mac::MacEventContent},
        AnyToDeviceEventContent, EventType, ToDeviceEvent,
    },
    identifiers::{DeviceId, UserId},
    uuid::Uuid,
};

use crate::{Account, Device};

#[derive(Clone, Debug)]
pub struct SasIds {
    pub account: Account,
    pub other_device: Device,
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
fn emoji_from_index(index: u8) -> (&'static str, &'static str) {
    match index {
        0 => ("🐶", "Dog"),
        1 => ("🐱", "Cat"),
        2 => ("🦁", "Lion"),
        3 => ("🐎", "Horse"),
        4 => ("🦄", "Unicorn"),
        5 => ("🐷", "Pig"),
        6 => ("🐘", "Elephant"),
        7 => ("🐰", "Rabbit"),
        8 => ("🐼", "Panda"),
        9 => ("🐓", "Rooster"),
        10 => ("🐧", "Penguin"),
        11 => ("🐢", "Turtle"),
        12 => ("🐟", "Fish"),
        13 => ("🐙", "Octopus"),
        14 => ("🦋", "Butterfly"),
        15 => ("🌷", "Flower"),
        16 => ("🌳", "Tree"),
        17 => ("🌵", "Cactus"),
        18 => ("🍄", "Mushroom"),
        19 => ("🌏", "Globe"),
        20 => ("🌙", "Moon"),
        21 => ("☁️", "Cloud"),
        22 => ("🔥", "Fire"),
        23 => ("🍌", "Banana"),
        24 => ("🍎", "Apple"),
        25 => ("🍓", "Strawberry"),
        26 => ("🌽", "Corn"),
        27 => ("🍕", "Pizza"),
        28 => ("🎂", "Cake"),
        29 => ("❤️", "Heart"),
        30 => ("😀", "Smiley"),
        31 => ("🤖", "Robot"),
        32 => ("🎩", "Hat"),
        33 => ("👓", "Glasses"),
        34 => ("🔧", "Spanner"),
        35 => ("🎅", "Santa"),
        36 => ("👍", "Thumbs up"),
        37 => ("☂️", "Umbrella"),
        38 => ("⌛", "Hourglass"),
        39 => ("⏰", "Clock"),
        40 => ("🎁", "Gift"),
        41 => ("💡", "Light Bulb"),
        42 => ("📕", "Book"),
        43 => ("✏️", "Pencil"),
        44 => ("📎", "Paperclip"),
        45 => ("✂️", "Scissors"),
        46 => ("🔒", "Lock"),
        47 => ("🔑", "Key"),
        48 => ("🔨", "Hammer"),
        49 => ("☎️", "Telephone"),
        50 => ("🏁", "Flag"),
        51 => ("🚂", "Train"),
        52 => ("🚲", "Bicycle"),
        53 => ("✈️", "Airplane"),
        54 => ("🚀", "Rocket"),
        55 => ("🏆", "Trophy"),
        56 => ("⚽", "Ball"),
        57 => ("🎸", "Guitar"),
        58 => ("🎺", "Trumpet"),
        59 => ("🔔", "Bell"),
        60 => ("⚓", "Anchor"),
        61 => ("🎧", "Headphones"),
        62 => ("📁", "Folder"),
        63 => ("📌", "Pin"),
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
    event: &ToDeviceEvent<MacEventContent>,
) -> Result<(Vec<Device>, Vec<String>), CancelCode> {
    let mut verified_devices = Vec::new();

    let info = extra_mac_info_receive(&ids, flow_id);

    let mut keys = event.content.mac.keys().cloned().collect::<Vec<String>>();
    keys.sort();
    let keys = sas
        .calculate_mac(&keys.join(","), &format!("{}KEY_IDS", &info))
        .expect("Can't calculate SAS MAC");

    if keys != event.content.keys {
        return Err(CancelCode::KeyMismatch);
    }

    for (key_id, key_mac) in &event.content.mac {
        let split: Vec<&str> = key_id.splitn(2, ':').collect();

        if split.len() != 2 {
            continue;
        }

        let algorithm: KeyAlgorithm = if let Ok(a) = split[0].try_into() {
            a
        } else {
            continue;
        };
        let id = split[1];

        let device_key_id = AlgorithmAndDeviceId(algorithm, id.into());

        if let Some(key) = ids.other_device.keys().get(&device_key_id) {
            if key_mac
                == &sas
                    .calculate_mac(key, &format!("{}{}", info, key_id))
                    .expect("Can't calculate SAS MAC")
            {
                verified_devices.push(ids.other_device.clone());
            } else {
                return Err(CancelCode::KeyMismatch);
            }
        }
        // TODO add an else branch for the master key here
    }

    Ok((verified_devices, vec![]))
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
/// * `we_started` - Flag signaling if the SAS process was started on our side.
///
/// # Panics
///
/// This will panic if the public key of the other side wasn't set.
pub fn get_mac_content(sas: &OlmSas, ids: &SasIds, flow_id: &str) -> MacEventContent {
    let mut mac: BTreeMap<String, String> = BTreeMap::new();

    let key_id = AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, ids.account.device_id().into());
    let key = ids.account.identity_keys().ed25519();
    let info = extra_mac_info_send(ids, flow_id);

    mac.insert(
        key_id.to_string(),
        sas.calculate_mac(key, &format!("{}{}", info, key_id))
            .expect("Can't calculate SAS MAC"),
    );

    // TODO Add the cross signing master key here if we trust/have it.

    let mut keys = mac.keys().cloned().collect::<Vec<String>>();
    keys.sort();
    let keys = sas
        .calculate_mac(&keys.join(","), &format!("{}KEY_IDS", &info))
        .expect("Can't calculate SAS MAC");

    MacEventContent {
        transaction_id: flow_id.to_owned(),
        keys,
        mac,
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
fn extra_info_sas(ids: &SasIds, flow_id: &str, we_started: bool) -> String {
    let (first_user, first_device, second_user, second_device) = if we_started {
        (
            ids.account.user_id(),
            ids.account.device_id(),
            ids.other_device.user_id(),
            ids.other_device.device_id(),
        )
    } else {
        (
            ids.other_device.user_id(),
            ids.other_device.device_id(),
            ids.account.user_id(),
            ids.account.device_id(),
        )
    };

    format!(
        "MATRIX_KEY_VERIFICATION_SAS{first_user}{first_device}\
        {second_user}{second_device}{transaction_id}",
        first_user = first_user,
        first_device = first_device,
        second_user = second_user,
        second_device = second_device,
        transaction_id = flow_id,
    )
}

/// Get the emoji version of the short authentication string.
///
/// Returns a vector of tuples where the first element is the emoji and the
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
    flow_id: &str,
    we_started: bool,
) -> Vec<(&'static str, &'static str)> {
    let bytes = sas
        .generate_bytes(&extra_info_sas(&ids, &flow_id, we_started), 6)
        .expect("Can't generate bytes");

    bytes_to_emoji(bytes)
}

fn bytes_to_emoji_index(bytes: Vec<u8>) -> Vec<u8> {
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
    vec![
        ((num >> 42) & 63) as u8,
        ((num >> 36) & 63) as u8,
        ((num >> 30) & 63) as u8,
        ((num >> 24) & 63) as u8,
        ((num >> 18) & 63) as u8,
        ((num >> 12) & 63) as u8,
        ((num >> 6) & 63) as u8,
    ]
}

fn bytes_to_emoji(bytes: Vec<u8>) -> Vec<(&'static str, &'static str)> {
    let numbers = bytes_to_emoji_index(bytes);

    // Convert the 6 bit number into a emoji/description tuple.
    numbers.into_iter().map(emoji_from_index).collect()
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
pub fn get_decimal(sas: &OlmSas, ids: &SasIds, flow_id: &str, we_started: bool) -> (u16, u16, u16) {
    let bytes = sas
        .generate_bytes(&extra_info_sas(&ids, &flow_id, we_started), 5)
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

pub fn content_to_request(
    recipient: &UserId,
    recipient_device: &DeviceId,
    content: AnyToDeviceEventContent,
) -> ToDeviceRequest {
    let mut messages = BTreeMap::new();
    let mut user_messages = BTreeMap::new();

    user_messages.insert(
        DeviceIdOrAllDevices::DeviceId(recipient_device.into()),
        serde_json::value::to_raw_value(&content).expect("Can't serialize to-device content"),
    );
    messages.insert(recipient.clone(), user_messages);

    let event_type = match content {
        AnyToDeviceEventContent::KeyVerificationAccept(_) => EventType::KeyVerificationAccept,
        AnyToDeviceEventContent::KeyVerificationStart(_) => EventType::KeyVerificationStart,
        AnyToDeviceEventContent::KeyVerificationKey(_) => EventType::KeyVerificationKey,
        AnyToDeviceEventContent::KeyVerificationMac(_) => EventType::KeyVerificationMac,
        AnyToDeviceEventContent::KeyVerificationCancel(_) => EventType::KeyVerificationCancel,
        _ => unreachable!(),
    };

    ToDeviceRequest {
        txn_id: Uuid::new_v4().to_string(),
        event_type,
        messages,
    }
}

#[cfg(test)]
mod test {
    use proptest::prelude::*;

    use super::{bytes_to_decimal, bytes_to_emoji, bytes_to_emoji_index, emoji_from_index};

    #[test]
    fn test_emoji_generation() {
        let bytes = vec![0, 0, 0, 0, 0, 0];
        let index: Vec<(&'static str, &'static str)> = vec![0, 0, 0, 0, 0, 0, 0]
            .into_iter()
            .map(emoji_from_index)
            .collect();
        assert_eq!(bytes_to_emoji(bytes), index);

        let bytes = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

        let index: Vec<(&'static str, &'static str)> = vec![63, 63, 63, 63, 63, 63, 63]
            .into_iter()
            .map(emoji_from_index)
            .collect();
        assert_eq!(bytes_to_emoji(bytes), index);
    }

    #[test]
    fn test_decimal_generation() {
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

            prop_assert!(first <= 9191 && first >= 1000);
            prop_assert!(second <= 9191 && second >= 1000);
            prop_assert!(third <= 9191 && third >= 1000);
        }
    }
}
