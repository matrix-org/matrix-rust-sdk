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

use std::collections::BTreeMap;

use ruma::{
    DeviceKeyAlgorithm, DeviceKeyId, OwnedDeviceKeyId, UserId,
    events::{
        AnyMessageLikeEventContent, AnyToDeviceEventContent,
        key::verification::{
            cancel::CancelCode,
            mac::{KeyVerificationMacEventContent, ToDeviceKeyVerificationMacEventContent},
        },
        relation::Reference,
    },
    serde::Base64,
};
use sha2::{Digest, Sha256};
use tracing::{trace, warn};
use vodozemac::{Curve25519PublicKey, sas::EstablishedSas};

use super::{FlowId, OutgoingContent, sas_state::SupportedMacMethod};
use crate::{
    Emoji, OwnUserIdentityData,
    identities::{DeviceData, UserIdentityData},
    olm::StaticAccountData,
    verification::event_enums::{MacContent, StartContent},
};

#[derive(Clone, Debug)]
pub struct SasIds {
    pub account: StaticAccountData,
    pub own_identity: Option<OwnUserIdentityData>,
    pub other_device: DeviceData,
    pub other_identity: Option<UserIdentityData>,
}

/// Calculate the commitment for a accept event from the public key and the
/// start event.
///
/// # Arguments
///
/// * `public_key` - Our own ephemeral public key that is used for the
///   interactive verification.
///
/// * `content` - The `m.key.verification.start` event content that started the
///   interactive verification process.
pub fn calculate_commitment(public_key: Curve25519PublicKey, content: &StartContent<'_>) -> Base64 {
    let content = content.canonical_json();
    let content_string = content.to_string();

    Base64::new(
        Sha256::new()
            .chain_update(public_key.to_base64())
            .chain_update(content_string)
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
    /*
    This list was generated from the data in the spec [1] with the following command:

    jq  -r '.[] |  "        " + (.number|tostring) + " => Emoji { symbol: \"" + .emoji + "\", description: \"" + .description + "\" },"' sas-emoji.json

    [1]: https://github.com/matrix-org/matrix-spec/blob/main/data-definitions/sas-emoji.json
    */
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
        36 => Emoji { symbol: "👍", description: "Thumbs Up" },
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
        53 => Emoji { symbol: "✈️", description: "Aeroplane" },
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
        second_user = ids.account.user_id,
        second_device = ids.account.device_id,
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
/// * `event` - The m.key.verification.mac event that was sent to us by the
///   other side.
pub fn receive_mac_event(
    sas: &EstablishedSas,
    ids: &SasIds,
    flow_id: &str,
    sender: &UserId,
    mac_method: SupportedMacMethod,
    content: &MacContent<'_>,
) -> Result<(Vec<DeviceData>, Vec<UserIdentityData>), CancelCode> {
    let mut verified_devices = Vec::new();
    let mut verified_identities = Vec::new();

    let info = extra_mac_info_receive(ids, flow_id);

    trace!(
        ?sender,
        device_id = ?ids.other_device.device_id(),
        "Received a key.verification.mac event"
    );

    let mut keys = content.mac().keys().map(|k| k.as_str()).collect::<Vec<_>>();
    keys.sort_unstable();
    mac_method.verify_mac(sas, &keys.join(","), &format!("{info}KEY_IDS"), content.keys())?;

    for (key_id, key_mac) in content.mac() {
        trace!(
            ?sender,
            device_id = ?ids.other_device.device_id(),
            key_id,
            "Checking a SAS MAC",
        );

        let key_id: OwnedDeviceKeyId = match key_id.as_str().try_into() {
            Ok(id) => id,
            Err(_) => continue,
        };

        if let Some(key) = ids.other_device.keys().get(&key_id) {
            mac_method.verify_mac(sas, &key.to_base64(), &format!("{info}{key_id}"), key_mac)?;
            trace!(?sender, ?key_id, "Successfully verified a device key");
            verified_devices.push(ids.other_device.clone());
        } else if let Some(identity) = &ids.other_identity {
            if let Some(key) = identity.master_key().get_key(&key_id) {
                // TODO: we should check that the master key signs the device,
                // this way we know the master key also trusts the device
                mac_method.verify_mac(
                    sas,
                    &key.to_base64(),
                    &format!("{info}{key_id}"),
                    key_mac,
                )?;
                trace!(?sender, ?key_id, "Successfully verified a master key");
                verified_identities.push(identity.clone())
            }
        } else {
            warn!(
                "Key ID {key_id} in MAC event from {sender} {} doesn't belong to any device \
                or user identity",
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
        first_user = ids.account.user_id,
        first_device = ids.account.device_id,
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
pub fn get_mac_content(
    sas: &EstablishedSas,
    ids: &SasIds,
    flow_id: &FlowId,
    mac_method: SupportedMacMethod,
) -> OutgoingContent {
    let mut mac: BTreeMap<String, Base64> = BTreeMap::new();

    let key_id = DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &ids.account.device_id);
    let key = ids.account.identity_keys.ed25519.to_base64();
    let info = extra_mac_info_send(ids, flow_id.as_str());

    mac.insert(key_id.to_string(), mac_method.calculate_mac(sas, &key, &format!("{info}{key_id}")));

    if let Some(own_identity) = &ids.own_identity
        && own_identity.is_verified()
        && let Some(key) = own_identity.master_key().get_first_key()
    {
        let key_id = format!("{}:{}", DeviceKeyAlgorithm::Ed25519, key.to_base64());

        let calculated_mac =
            mac_method.calculate_mac(sas, &key.to_base64(), &format!("{info}{key_id}"));

        mac.insert(key_id, calculated_mac);
    }

    let mut keys: Vec<_> = mac.keys().map(|s| s.as_str()).collect();
    keys.sort_unstable();

    let keys = mac_method.calculate_mac(sas, &keys.join(","), &format!("{info}KEY_IDS"));

    match flow_id {
        FlowId::ToDevice(s) => AnyToDeviceEventContent::KeyVerificationMac(
            ToDeviceKeyVerificationMacEventContent::new(s.clone(), mac, keys),
        )
        .into(),
        FlowId::InRoom(r, e) => {
            (
                r.clone(),
                AnyMessageLikeEventContent::KeyVerificationMac(
                    KeyVerificationMacEventContent::new(mac, keys, Reference::new(e.clone())),
                ),
            )
                .into()
        }
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
    own_pubkey: Curve25519PublicKey,
    their_pubkey: Curve25519PublicKey,
    flow_id: &str,
    we_started: bool,
) -> String {
    let our_info =
        format!("{}|{}|{}", ids.account.user_id, ids.account.device_id, own_pubkey.to_base64());
    let their_info = format!(
        "{}|{}|{}",
        ids.other_device.user_id(),
        ids.other_device.device_id(),
        their_pubkey.to_base64()
    );

    let (first_info, second_info) =
        if we_started { (our_info, their_info) } else { (their_info, our_info) };

    let info = format!("MATRIX_KEY_VERIFICATION_SAS|{first_info}|{second_info}|{flow_id}");

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
///   shared secret.
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
    sas: &EstablishedSas,
    ids: &SasIds,
    flow_id: &str,
    we_started: bool,
) -> [Emoji; 7] {
    let bytes = sas.bytes(&extra_info_sas(
        ids,
        sas.our_public_key(),
        sas.their_public_key(),
        flow_id,
        we_started,
    ));

    let indices = bytes.emoji_indices();

    [
        emoji_from_index(indices[0]),
        emoji_from_index(indices[1]),
        emoji_from_index(indices[2]),
        emoji_from_index(indices[3]),
        emoji_from_index(indices[4]),
        emoji_from_index(indices[5]),
        emoji_from_index(indices[6]),
    ]
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
///   shared secret.
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
    sas: &EstablishedSas,
    ids: &SasIds,
    flow_id: &str,
    we_started: bool,
) -> [u8; 7] {
    let bytes = sas.bytes(&extra_info_sas(
        ids,
        sas.our_public_key(),
        sas.their_public_key(),
        flow_id,
        we_started,
    ));

    bytes.emoji_indices()
}

/// Get the decimal version of the short authentication string.
///
/// Returns a tuple containing three 4 digit integer numbers that represent
/// the short auth string.
///
/// # Arguments
///
/// * `sas` - The Olm SAS object that can be used to generate bytes using the
///   shared secret.
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
    sas: &EstablishedSas,
    ids: &SasIds,
    flow_id: &str,
    we_started: bool,
) -> (u16, u16, u16) {
    let bytes = sas.bytes(&extra_info_sas(
        ids,
        sas.our_public_key(),
        sas.their_public_key(),
        flow_id,
        we_started,
    ));

    bytes.decimals()
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use ruma::{
        events::key::verification::start::ToDeviceKeyVerificationStartEventContent, serde::Base64,
    };
    use serde_json::json;
    use vodozemac::Curve25519PublicKey;

    use super::calculate_commitment;
    use crate::verification::event_enums::StartContent;

    #[test]
    fn commitment_calculation() {
        let commitment = Base64::parse("CCQmB4JCdB0FW21FdAnHj/Hu8+W9+Nb0vgwPEnZZQ4g").unwrap();

        let public_key =
            Curve25519PublicKey::from_base64("Q/NmNFEUS1fS+YeEmiZkjjblKTitrKOAk7cPEumcMlg")
                .unwrap();
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
        let calculated_commitment = calculate_commitment(public_key, &content);

        assert_eq!(commitment, calculated_commitment);
    }
}
